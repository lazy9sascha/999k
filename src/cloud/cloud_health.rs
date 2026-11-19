//! Read-only operator health and the runner's loopback watchdog.
use super::{QUEUE, RUN_LOCK, STATE_FILE};
use crate::{cloud_transport, mailbox_startup};
use pigeonmq::{
    Queue, events::callback_transport::validate_public_addresses, grok_wake,
    mailbox_catalog as catalog, private_file::open_private_file,
};
use reqwest::{Client, ClientBuilder, Response, Url};
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    net::{IpAddr, SocketAddr},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const LOCAL_BUDGET: Duration = Duration::from_secs(2);
const PUBLIC_BUDGET: Duration = Duration::from_secs(4);
pub(super) const WATCH_INTERVAL: Duration = Duration::from_secs(15);
const MAX_WATCH_MISSES: u8 = 3;

fn state_file(path: &Path) -> Option<Value> {
    let mut bytes = Vec::new();
    open_private_file(path)
        .ok()?
        .take(8193)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 8192 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn runner_active(home: &Path) -> bool {
    let Ok(lock) = open_private_file(&home.join(RUN_LOCK)) else {
        return false;
    };
    // The runner owns an exclusive lock. Checking it never creates the file.
    // SAFETY: lock owns a valid descriptor throughout the call.
    let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    result != 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock
}

fn client() -> ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent("PigeonMQ-Health")
}

async fn request(client: &Client, url: Url) -> Result<Response, reqwest::Error> {
    // Even on a real MCP route this fails at authentication before any tool can run.
    client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .body("{}")
        .send()
        .await
}

async fn expected_error(
    mut response: Response,
    status: u16,
    code: &str,
) -> Result<bool, reqwest::Error> {
    if response.status().as_u16() != status {
        return Ok(false);
    }
    let mut body = Vec::new();
    while let Some(bytes) = response.chunk().await? {
        if bytes.len() > 256 - body.len() {
            return Ok(false);
        }
        body.extend_from_slice(&bytes);
    }
    Ok(serde_json::from_slice::<Value>(&body)
        .is_ok_and(|value| value == json!({"error":{"code":code}})))
}

async fn probe_loopback(port: u16) -> Result<(), &'static str> {
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).expect("numeric loopback port");
    let client = client()
        .http1_only()
        .timeout(LOCAL_BUDGET)
        .build()
        .map_err(|_| "hub_client_unavailable")?;
    let response = request(&client, url).await.map_err(|_| "hub_unreachable")?;
    if response.status().as_u16() == 503 {
        return Err("hub_busy");
    }
    if expected_error(response, 404, "route_not_found")
        .await
        .map_err(|_| "hub_unreachable")?
    {
        Ok(())
    } else {
        Err("unexpected_hub_response")
    }
}

#[derive(Default)]
pub(super) struct Watchdog {
    misses: u8,
    recovered: bool,
}

impl Watchdog {
    fn missed(&mut self, result: Result<(), &'static str>) -> bool {
        if result == Ok(()) {
            self.recovered = false;
        }
        self.misses = if result == Err("hub_unreachable") {
            self.misses.saturating_add(1)
        } else {
            0
        };
        if self.misses < MAX_WATCH_MISSES || self.recovered {
            return false;
        }
        self.recovered = true;
        self.misses = 0;
        true
    }

    pub(super) async fn wait_for_failure(&mut self, port: u16, interval: Duration) {
        loop {
            tokio::time::sleep(interval).await;
            if self.missed(probe_loopback(port).await) {
                return;
            }
        }
    }
}

fn public_endpoint(state: &Value) -> Option<Url> {
    let origin = match (state["state"].as_str()?, state["tunnel"].as_str()?) {
        ("connected", "connected") => cloud_transport::handoff_origin(state)?,
        ("local_ready", "externally_managed") if state["public_origin_source"] == "configured" => {
            cloud_transport::public_origin(state["public_origin"].as_str()?).ok()?
        }
        _ => return None,
    };
    catalog::http_profiles().iter().find_map(|profile| {
        let raw = state[profile.endpoint_key?].as_str()?;
        let path = raw.strip_prefix(&origin)?;
        if !mailbox_startup::valid_route(profile, path) {
            return None;
        }
        Url::parse(raw).ok()
    })
}

async fn public_auth_stage(url: Url) -> Result<(), &'static str> {
    tokio::time::timeout(PUBLIC_BUDGET, async {
        let host = url.host().ok_or("public_origin_unavailable")?;
        let addresses: Vec<IpAddr> = match host {
            url::Host::Domain(name) => tokio::net::lookup_host((name, 443))
                .await
                .map_err(|_| "public_dns_unavailable")?
                .take(33)
                .map(|address| address.ip())
                .collect(),
            url::Host::Ipv4(address) => vec![address.into()],
            url::Host::Ipv6(address) => vec![address.into()],
        };
        validate_public_addresses(&addresses).map_err(|_| "public_address_unavailable")?;
        let mut builder = client().https_only(true).timeout(PUBLIC_BUDGET);
        if let url::Host::Domain(name) = host {
            builder = builder.resolve(name, SocketAddr::new(addresses[0], 443));
        }
        let client = builder.build().map_err(|_| "public_probe_unavailable")?;
        let response = request(&client, url)
            .await
            .map_err(|_| "public_unreachable")?;
        if expected_error(response, 401, "unauthorized")
            .await
            .map_err(|_| "public_unreachable")?
        {
            Ok(())
        } else {
            Err("public_auth_stage_not_confirmed")
        }
    })
    .await
    .map_err(|_| "public_probe_timeout")?
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs())
}

fn grok_delivery(state: &Value, evidence: &Value, active: bool, now: u64) -> Value {
    match state["delivery_enabled"]["grok_wake"].as_bool() {
        Some(false) => return json!({"state":"skipped"}),
        None => return json!({"state":"unknown","reason":"configuration_unavailable"}),
        Some(true) => {}
    }
    if !active
        || state["host_pid"].as_u64().is_none()
        || evidence["worker_pid"] != state["host_pid"]
    {
        return json!({"state":"unknown","reason":"worker_status_unavailable"});
    }
    let worker = match evidence["state"].as_str() {
        Some(value @ ("running" | "failed" | "auth_blocked" | "rate_limited")) => value,
        _ => return json!({"state":"unknown","reason":"worker_status_unavailable"}),
    };
    let observed_at = evidence["updated"].as_u64().filter(|t| *t > 0 && *t <= now);
    let status = evidence["detail"]["status"]
        .as_u64()
        .filter(|n| (100..=599).contains(n));
    let last = match (worker, evidence["detail"]["outcome"].as_str(), status) {
        ("running", Some("accepted"), Some(200..=299)) => Some("accepted"),
        ("running", Some("failed"), _) => Some("failed"),
        ("running", Some("rejected"), Some(_)) => Some("rejected"),
        ("auth_blocked", _, Some(401 | 403)) => Some("rejected"),
        ("rate_limited", _, Some(429)) => Some("rate_limited"),
        _ => None,
    };
    let observation = last.zip(observed_at).map(|(outcome, at)| {
        json!({"outcome":outcome,"http_status":status,"observed_at":at,"age_seconds":now-at})
    });
    let (state, reason) = match worker {
        "failed" | "auth_blocked" => ("degraded", Some(worker)),
        "rate_limited"
            if evidence["detail"]["until"]
                .as_u64()
                .is_some_and(|t| t > now) =>
        {
            ("degraded", Some(worker))
        }
        _ if matches!(last, Some("failed" | "rejected")) && observation.is_some() => {
            ("degraded", last)
        }
        _ if observation.is_none() => ("unknown", Some("no_delivery_evidence")),
        _ => ("observed", None),
    };
    // An observation is a timestamped fact, not proof of current health or of agent processing.
    json!({"state":state,"worker":worker,"reason":reason,"last_attempt":observation})
}

fn events_delivery(state: &Value, key: &str) -> Value {
    match state["delivery_enabled"][key].as_bool() {
        Some(false) => json!({"state":"skipped"}),
        Some(true) => json!({"state":"unknown","reason":"delivery_recency_not_recorded"}),
        None => json!({"state":"unknown","reason":"configuration_unavailable"}),
    }
}

async fn tunnel_health(
    state: &Value,
    active: bool,
    locally_reachable: bool,
    probe_public: bool,
) -> Value {
    let mode = match state["tunnel_mode"].as_str() {
        Some("local_only") => return json!({"state":"skipped","public_reachability":"skipped"}),
        Some(mode @ ("ssh" | "external")) => mode,
        _ => {
            return json!({"state":"unknown","public_reachability":"unknown","reason":"configuration_unavailable"});
        }
    };
    let state_name = if mode == "external" {
        "externally_managed"
    } else if !active {
        "unknown"
    } else {
        state["tunnel"]
            .as_str()
            .filter(|value| {
                matches!(
                    *value,
                    "connected" | "starting" | "retrying" | "failed" | "stopped"
                )
            })
            .unwrap_or("unknown")
    };
    let mut report = json!({"state":state_name,"mode":mode,"public_reachability":"unknown"});
    if probe_public && locally_reachable {
        match public_endpoint(state) {
            Some(endpoint) => match public_auth_stage(endpoint).await {
                Ok(()) => report["public_reachability"] = json!("auth_stage_reached"),
                Err(reason) => {
                    report["public_reachability"] = json!("failed");
                    report["reason"] = json!(reason);
                }
            },
            None => report["reason"] = json!("no_configured_public_mcp_endpoint"),
        }
    } else if probe_public {
        report["reason"] = json!("local_hub_not_ready");
    } else {
        report["reason"] = json!("public_probe_not_requested");
    }
    report
}

pub(super) async fn report(home: &Path, probe_public: bool) -> (Value, bool) {
    let active = runner_active(home);
    let state = state_file(&home.join(STATE_FILE)).unwrap_or(Value::Null);
    let hub = if !active {
        Err("runner_inactive")
    } else if state.is_null() {
        Err("state_unavailable")
    } else if state["local_hub"] != "running" {
        Err("hub_not_ready")
    } else if let Some(port) = state["local_port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
    {
        probe_loopback(port).await
    } else {
        Err("state_unavailable")
    };
    // Do not block the runner while SQLite holds or waits for a read lock.
    let queue_home = home.to_owned();
    let backlog = tokio::task::spawn_blocking(move || {
        super::private_home(&queue_home).ok()?;
        Queue::inspect_never_claimed_backlog(&queue_home.join(QUEUE)).ok()
    })
    .await
    .ok()
    .flatten();
    let reason = hub
        .err()
        .or(backlog.is_none().then_some("queue_unavailable"));
    let local_ready = reason.is_none();
    let evidence = if active && state["delivery_enabled"]["grok_wake"] == true {
        state_file(&home.join(grok_wake::STATUS_FILE)).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let status = json!({
        "local_ready":local_ready,
        "reason":reason,
        "remote_verified":false,
        "hub":{"state":hub.map_or_else(|reason| reason, |()| "ready")},
        "queue":{
            "state":if backlog.is_some() {"readable"} else {"unavailable"},
            "never_claimed_backlog":backlog,
        },
        "tunnel":tunnel_health(&state, active, hub.is_ok(), probe_public).await,
        "delivery":{
            "grok_wake":grok_delivery(&state, &evidence, active, unix_now()),
            "dot_events":events_delivery(&state, "dot_events"),
            "codex_events":events_delivery(&state, "codex_events"),
        },
    });
    (status, local_ready)
}

pub(crate) fn health(home: &Path, probe_public: bool) -> i32 {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        println!(
            "{}",
            json!({"local_ready":false,"reason":"health_runtime_unavailable","remote_verified":false})
        );
        return 1;
    };
    let (status, ready) = runtime.block_on(report(home, probe_public));
    // A timed-out OS resolver worker must not delay returning the report.
    runtime.shutdown_background();
    println!("{status}");
    i32::from(!ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_restarts_once_until_a_confirmed_response() {
        let mut watchdog = Watchdog::default();
        let miss = Err("hub_unreachable");
        assert!(!watchdog.missed(miss));
        assert!(!watchdog.missed(miss));
        assert!(!watchdog.missed(Err("hub_busy")));
        assert!(!watchdog.missed(miss));
        assert!(!watchdog.missed(miss));
        assert!(watchdog.missed(miss));
        for _ in 0..9 {
            assert!(!watchdog.missed(miss));
        }
        assert!(!watchdog.missed(Ok(())));
        assert!(!watchdog.missed(miss));
        assert!(!watchdog.missed(miss));
        assert!(watchdog.missed(miss));
    }

    #[tokio::test]
    async fn auth_probe_sends_no_credentials_and_requires_the_expected_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                bytes.push(stream.read_u8().await.unwrap());
                if bytes.ends_with(b"\r\n\r\n{}") {
                    break;
                }
                assert!(bytes.len() < 4096);
            }
            let wire = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
            assert!(wire.starts_with("post /fixture/mcp http/1.1"));
            assert!(!wire.contains("authorization:"));
            assert!(!wire.contains("cookie:"));
            let body = r#"{"error":{"code":"unauthorized"}}"#;
            stream.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        };
        let probe = async {
            let client = client().timeout(LOCAL_BUDGET).build().unwrap();
            let url = Url::parse(&format!("http://{address}/fixture/mcp")).unwrap();
            assert!(
                expected_error(request(&client, url).await.unwrap(), 401, "unauthorized")
                    .await
                    .unwrap()
            );
        };
        tokio::time::timeout(LOCAL_BUDGET, async { tokio::join!(upstream, probe) })
            .await
            .unwrap();
    }

    #[test]
    fn probe_url_uses_an_authoritative_origin_and_registered_route() {
        let mut state = json!({
            "state":"local_ready","tunnel":"externally_managed","public_origin_source":"configured",
            "public_origin":"https://fixture.example","grok_endpoint":"https://fixture.example/team-grok/mcp",
        });
        assert_eq!(public_endpoint(&state).unwrap().path(), "/team-grok/mcp");
        for rejected in [
            "https://fixture.example.other.test/team-grok/mcp",
            "https://fixture.example@other.test/team-grok/mcp",
            "https://fixture.example//other.test/team-grok/mcp",
            "https://fixture.example/team-grok/mcp?secret=fixture",
            "https://fixture.example/receiver",
            "https://fixture.example/muse/mcp",
        ] {
            state["grok_endpoint"] = json!(rejected);
            assert!(public_endpoint(&state).is_none());
        }
        state["public_origin_source"] = Value::Null;
        state["grok_endpoint"] = json!("https://fixture.example/team-grok/mcp");
        assert!(public_endpoint(&state).is_none());
    }

    #[tokio::test]
    async fn configured_tunnel_failure_and_explicitly_disabled_tunnel_are_distinct() {
        let mut state = json!({"tunnel_mode":"ssh","tunnel":"failed"});
        let report = tunnel_health(&state, true, false, true).await;
        assert_eq!(report["state"], "failed");
        assert_eq!(report["public_reachability"], "unknown");
        state["tunnel_mode"] = json!("local_only");
        let report = tunnel_health(&state, true, false, true).await;
        assert_eq!(report["state"], "skipped");
        assert_eq!(report["public_reachability"], "skipped");
    }

    #[test]
    fn delivery_evidence_separates_skipped_unknown_blocked_and_real_acceptance() {
        let mut state = json!({"delivery_enabled":{"grok_wake":false},"host_pid":9});
        let mut evidence = json!({"state":"running","worker_pid":9,"updated":12});
        assert_eq!(
            grok_delivery(&state, &evidence, true, 13)["state"],
            "skipped"
        );
        state["delivery_enabled"]["grok_wake"] = json!(true);
        assert_eq!(
            grok_delivery(&state, &evidence, false, 13)["state"],
            "unknown"
        );
        assert_eq!(
            grok_delivery(&state, &evidence, true, 13)["reason"],
            "no_delivery_evidence"
        );
        evidence["detail"] = json!({"outcome":"accepted","status":204,"private":"fixture"});
        let report = grok_delivery(&state, &evidence, true, 28);
        assert_eq!(
            report["last_attempt"],
            json!({"outcome":"accepted","http_status":204,"observed_at":12,"age_seconds":16})
        );
        assert_eq!(report["state"], "observed");
        assert!(!report.to_string().contains("fixture"));
        evidence["worker_pid"] = json!(2);
        assert!(grok_delivery(&state, &evidence, true, 28)["last_attempt"].is_null());
        evidence["worker_pid"] = json!(9);
        evidence["state"] = json!("auth_blocked");
        evidence["detail"] = json!({"status":403});
        assert_eq!(
            grok_delivery(&state, &evidence, true, 28)["state"],
            "degraded"
        );
        assert_eq!(
            grok_delivery(&state, &evidence, true, 28)["last_attempt"]["outcome"],
            "rejected"
        );
    }
}
