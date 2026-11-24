//! Read-only operator health and the runner's loopback watchdog.
use super::{QUEUE, RUN_LOCK, STATE_FILE};
use crate::{cloud_transport, mailbox_startup};
use futures_util::future::join_all;
use radio999k::{
    Queue,
    events::callback_transport::validate_public_addresses,
    grok_wake, mailbox_catalog as catalog,
    private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file},
};
use reqwest::{Client, ClientBuilder, Response, Url};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{self, Read},
    net::{IpAddr, SocketAddr},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const LOCAL_BUDGET: Duration = Duration::from_secs(2);
const PUBLIC_BUDGET: Duration = Duration::from_secs(4);
const DELIVERY_STATE_LIMIT: u64 = 8192;
// Some custom paths occur once in the legacy endpoint and once in the route map.
const RUNTIME_STATE_LIMIT: u64 = 2 * MAX_PRIVATE_CONFIG_BYTES + DELIVERY_STATE_LIMIT;
pub(super) const WATCH_INTERVAL: Duration = Duration::from_secs(15);
const MAX_WATCH_MISSES: u8 = 3;

fn state_file(path: &Path, limit: u64) -> Option<Value> {
    let mut bytes = Vec::new();
    open_private_file(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > limit {
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
        .user_agent("999K-Health")
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

// The profile handshake comes from the spawned child. The same child must
// own the listener on both sides of the protocol probe.
pub(super) async fn probe_owned_loopback(port: u16, pid: Option<u32>) -> Result<(), &'static str> {
    #[cfg(target_os = "macos")]
    {
        let pid = pid.filter(|pid| *pid > 0).ok_or("hub_listener_unowned")?;
        for probe in [true, false] {
            if !crate::lifecycle_service::owns_loopback(port, pid, LOCAL_BUDGET)
                .await
                .unwrap_or(false)
            {
                return Err("hub_listener_unowned");
            }
            if probe {
                probe_loopback(port).await?;
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (port, pid);
        Err("hub_listener_unowned")
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

fn public_origin(state: &Value) -> Option<String> {
    match (state["state"].as_str()?, state["tunnel"].as_str()?) {
        ("connected", "connected") => cloud_transport::handoff_origin(state),
        ("local_ready", "externally_managed") if state["public_origin_source"] == "configured" => {
            cloud_transport::public_origin(state["public_origin"].as_str()?).ok()
        }
        _ => None,
    }
}

fn public_endpoint(origin: &str, profile: &catalog::HttpProfile, raw: &Value) -> Option<Url> {
    let raw = raw.as_str()?;
    let path = raw.strip_prefix(origin)?;
    if !mailbox_startup::valid_route(profile, path) {
        return None;
    }
    Url::parse(raw).ok()
}

async fn public_routes<F, Fut>(
    state: &Value,
    skip: Option<&'static str>,
    budget: Duration,
    probe: F,
) -> Vec<Value>
where
    F: Fn(Url) -> Fut,
    Fut: std::future::Future<Output = Result<(), &'static str>>,
{
    let origin = public_origin(state);
    let paths = state["health_public_paths"].as_object();
    let mut targets = Vec::new();
    for profile in catalog::http_profiles() {
        if profile.adapter == catalog::Adapter::Receiver {
            continue;
        }
        if let Some(paths) = paths {
            if let Some(raw) = paths.get(profile.id) {
                let url = origin.as_ref().and_then(|origin| {
                    let path = raw.as_str()?;
                    mailbox_startup::valid_route(profile, path)
                        .then(|| Url::parse(&format!("{origin}{path}")).ok())
                        .flatten()
                });
                targets.push((profile.id, url));
            }
        } else if let Some(key) = profile.endpoint_key {
            let raw = &state[key];
            if !raw.is_null() || state[profile.enabled_key] == true {
                let url = origin
                    .as_deref()
                    .and_then(|origin| public_endpoint(origin, profile, raw));
                targets.push((profile.id, url));
            }
        }
    }
    let generic = paths.map_or_else(
        || (state["generic_enabled"] == true).then(|| json!("/mcp")),
        |paths| paths.get("generic").cloned(),
    );
    if let Some(path) = generic {
        let url = origin
            .filter(|_| path == "/mcp")
            .and_then(|origin| Url::parse(&format!("{origin}/mcp")).ok());
        targets.push(("generic", url));
    }
    let mut seen = HashSet::new();
    let duplicate_paths: HashSet<_> = targets
        .iter()
        .filter_map(|(_, url)| {
            let path = url.as_ref()?.path();
            (!seen.insert(path)).then_some(path)
        })
        .map(str::to_owned)
        .collect();
    let probe = &probe;
    let duplicate_paths = &duplicate_paths;
    join_all(targets.into_iter().map(|(profile, url)| async move {
        let mut route = json!({
            "profile": profile,
            "public_reachability": "not_checked",
            "authenticated_mcp": "not_checked",
            "authentication_reason": "credentialed_probe_not_supported",
        });
        if let Some(reason) = skip {
            route["reason"] = json!(reason);
        } else if let Some(url) = url {
            if duplicate_paths.contains(url.path()) {
                route["reason"] = json!("duplicate_public_route");
                return route;
            }
            let result = tokio::time::timeout(budget, probe(url))
                .await
                .unwrap_or(Err("public_probe_timeout"));
            match result {
                Ok(()) => route["public_reachability"] = json!("auth_stage_reached"),
                Err(reason) => {
                    route["public_reachability"] = json!("failed");
                    route["reason"] = json!(reason);
                }
            }
        } else {
            route["reason"] = json!("public_endpoint_unavailable");
        }
        route
    }))
    .await
}

async fn public_auth_stage(url: Url) -> Result<(), &'static str> {
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
    let response = request(&client, url).await.map_err(public_request_error)?;
    if expected_error(response, 401, "unauthorized")
        .await
        .map_err(public_request_error)?
    {
        Ok(())
    } else {
        Err("public_auth_stage_not_confirmed")
    }
}

fn public_request_error(error: reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "public_probe_timeout"
    } else {
        "public_unreachable"
    }
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

fn current_route_catalog(state: &Value) -> bool {
    let Some(paths) = state["health_public_paths"].as_object() else {
        return false;
    };
    if !paths.keys().all(|id| {
        id == "generic"
            || catalog::http_by_id(id)
                .is_some_and(|entry| entry.adapter != catalog::Adapter::Receiver)
    }) {
        return false;
    }
    if state["generic_enabled"]
        .as_bool()
        .is_some_and(|enabled| enabled != paths.get("generic").is_some_and(|path| path == "/mcp"))
    {
        return false;
    }
    let origin = public_origin(state);
    catalog::http_profiles().iter().all(|profile| {
        let Some(key) = profile.endpoint_key else {
            return true;
        };
        let legacy = &state[key];
        legacy.is_null()
            || origin
                .as_deref()
                .and_then(|origin| public_endpoint(origin, profile, legacy))
                .is_some_and(|url| paths.get(profile.id).is_some_and(|path| path == url.path()))
    })
}

async fn tunnel_health(
    state: &Value,
    active: bool,
    locally_reachable: bool,
    probe_public: bool,
) -> Value {
    let mode = match state["tunnel_mode"].as_str() {
        Some("local_only") => {
            return json!({"state":"skipped","public_reachability":"skipped","routes":[]});
        }
        Some("private_companions") => {
            return json!({"state":"private_companions","public_reachability":"skipped","routes":[]});
        }
        Some(mode @ ("ssh" | "external")) => mode,
        _ => {
            return json!({"state":"unknown","public_reachability":"unknown","reason":"configuration_unavailable","routes":[]});
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
    let skip = if !probe_public {
        Some("public_probe_not_requested")
    } else if !locally_reachable {
        Some("local_hub_not_ready")
    } else {
        None
    };
    let catalog_current = current_route_catalog(state);
    let routes = public_routes(state, skip, PUBLIC_BUDGET, public_auth_stage).await;
    let mut report = json!({
        "state": state_name,
        "mode": mode,
        "public_reachability": "unknown",
        "route_catalog": if catalog_current { "current" } else { "partial" },
    });
    if let Some(reason) = skip {
        report["reason"] = json!(reason);
    } else if let Some(failed) = routes
        .iter()
        .find(|route| route["public_reachability"] == "failed")
    {
        report["public_reachability"] = json!("failed");
        report["reason"] = failed["reason"].clone();
    } else if !catalog_current {
        report["reason"] = json!("public_route_catalog_incomplete");
    } else if routes.is_empty() {
        report["reason"] = json!("no_configured_public_mcp_endpoint");
    } else if routes
        .iter()
        .all(|route| route["public_reachability"] == "auth_stage_reached")
    {
        report["public_reachability"] = json!("auth_stage_reached");
    } else {
        report["reason"] = json!("public_routes_not_checked");
    }
    report["routes"] = json!(routes);
    report
}

pub(super) async fn report(home: &Path, probe_public: bool) -> (Value, bool) {
    let active = runner_active(home);
    let state = state_file(&home.join(STATE_FILE), RUNTIME_STATE_LIMIT).unwrap_or(Value::Null);
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
        if state["tunnel_mode"] == "private_companions" {
            let pid = state["host_pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok());
            probe_owned_loopback(port, pid).await
        } else {
            probe_loopback(port).await
        }
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
        state_file(&home.join(grok_wake::STATUS_FILE), DELIVERY_STATE_LIMIT).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let mut status = json!({
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
    if state["tunnel_mode"] == "private_companions" {
        status["companions"] = if local_ready {
            state["companions"].clone()
        } else {
            Value::Null
        };
    }
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
        let profile = catalog::http_by_id("grok-bot").unwrap();
        let origin = public_origin(&state).unwrap();
        assert_eq!(
            public_endpoint(&origin, profile, &state["grok_endpoint"])
                .unwrap()
                .path(),
            "/team-grok/mcp"
        );
        for rejected in [
            "https://fixture.example.other.test/team-grok/mcp",
            "https://fixture.example@other.test/team-grok/mcp",
            "https://fixture.example//other.test/team-grok/mcp",
            "https://fixture.example/team-grok/mcp?secret=fixture",
            "https://fixture.example/receiver",
            "https://fixture.example/muse/mcp",
        ] {
            state["grok_endpoint"] = json!(rejected);
            assert!(public_endpoint(&origin, profile, &state["grok_endpoint"]).is_none());
        }
        state["public_origin_source"] = Value::Null;
        state["grok_endpoint"] = json!("https://fixture.example/team-grok/mcp");
        assert!(public_origin(&state).is_none());
    }

    #[tokio::test]
    async fn public_health_reports_each_route_and_does_not_leak_config() {
        let state = json!({
            "state": "local_ready",
            "tunnel": "externally_managed",
            "public_origin_source": "configured",
            "public_origin": "https://private-fixture.example",
            "health_public_paths": {
                "muse": "/muse/mcp",
                "dot": "/dot/mcp",
                "grok-bot": "/grok/mcp",
                "codex-cloud": "/codex/mcp?secret=hidden-fixture",
                "instinct": "/test-instinct/mcp",
                "cue": "/cue/mcp",
                "generic": "/mcp",
                "codex-local": "/receiver",
            },
        });
        let routes = public_routes(&state, None, Duration::from_millis(20), |url| async move {
            match url.path() {
                "/muse/mcp" => Ok(()),
                "/dot/mcp" => Err("public_auth_stage_not_confirmed"),
                "/grok/mcp" => std::future::pending().await,
                "/test-instinct/mcp" | "/cue/mcp" | "/mcp" => Ok(()),
                _ => panic!("an invalid endpoint must not be probed"),
            }
        })
        .await;
        let lookup = |id: &str| routes.iter().find(|v| v["profile"] == id).unwrap();
        assert_eq!(routes.len(), 7);
        assert_eq!(
            lookup("instinct")["public_reachability"],
            "auth_stage_reached"
        );
        assert_eq!(lookup("cue")["public_reachability"], "auth_stage_reached");
        assert_eq!(
            lookup("generic")["public_reachability"],
            "auth_stage_reached"
        );
        assert_eq!(lookup("muse")["public_reachability"], "auth_stage_reached");
        assert_eq!(lookup("dot")["public_reachability"], "failed");
        assert_eq!(lookup("dot")["reason"], "public_auth_stage_not_confirmed");
        assert_eq!(lookup("grok-bot")["reason"], "public_probe_timeout");
        assert_eq!(lookup("codex-cloud")["public_reachability"], "not_checked");
        assert!(
            routes
                .iter()
                .all(|v| v["authenticated_mcp"] == "not_checked")
        );
        let output = json!(routes).to_string();
        assert!(!output.contains("private-fixture"));
        assert!(!output.contains("hidden-fixture"));
    }

    #[tokio::test]
    async fn duplicate_route_paths_cannot_credit_multiple_profiles() {
        let state = json!({
            "state": "local_ready",
            "tunnel": "externally_managed",
            "public_origin_source": "configured",
            "public_origin": "https://fixture.example",
            "health_public_paths": {
                "cue": "/duplicate/mcp",
                "instinct": "/duplicate/mcp",
                "muse": "/muse/mcp",
            },
        });
        let routes = public_routes(&state, None, PUBLIC_BUDGET, |url| async move {
            assert_eq!(url.path(), "/muse/mcp");
            Ok(())
        })
        .await;
        for route in routes {
            if route["profile"] == "muse" {
                assert_eq!(route["public_reachability"], "auth_stage_reached");
            } else {
                assert_eq!(route["public_reachability"], "not_checked");
                assert_eq!(route["reason"], "duplicate_public_route");
            }
        }
    }

    #[test]
    fn route_state_budget_does_not_expand_the_delivery_state_budget() {
        use std::io::Write;
        let path = format!("/a{}/mcp", "a".repeat(5000));
        let bytes = json!({
            "public_origin": "https://fixture.example",
            "muse_endpoint": format!("https://fixture.example{path}"),
            "health_public_paths": {"muse": path}
        })
        .to_string();
        assert!(bytes.len() as u64 > DELIVERY_STATE_LIMIT);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes.as_bytes()).unwrap();
        assert!(state_file(file.path(), RUNTIME_STATE_LIMIT).is_some());
        assert!(state_file(file.path(), DELIVERY_STATE_LIMIT).is_none());
        file.write_all(&vec![b' '; RUNTIME_STATE_LIMIT as usize])
            .unwrap();
        assert!(state_file(file.path(), RUNTIME_STATE_LIMIT).is_none());
    }

    #[test]
    fn stale_catalog_cannot_omit_routes_present_in_legacy_evidence() {
        let mut state = json!({
            "state": "local_ready",
            "tunnel": "externally_managed",
            "public_origin_source": "configured",
            "public_origin": "https://fixture.example",
            "health_public_paths": {"muse":"/muse/mcp"},
        });
        assert!(current_route_catalog(&state));
        state["dot_endpoint"] = json!("https://fixture.example/dot/mcp");
        assert!(!current_route_catalog(&state));
        state["health_public_paths"]["dot"] = json!("/custom-dot/mcp");
        assert!(!current_route_catalog(&state));
        state["health_public_paths"]["dot"] = json!("/dot/mcp");
        assert!(current_route_catalog(&state));
        state["generic_enabled"] = json!(true);
        assert!(!current_route_catalog(&state));
        state["health_public_paths"]["generic"] = json!("/mcp");
        assert!(current_route_catalog(&state));
    }

    #[tokio::test]
    async fn public_routes_are_not_contacted_unless_requested() {
        let state = json!({
            "state": "local_ready",
            "tunnel": "externally_managed",
            "tunnel_mode": "external",
            "public_origin_source": "configured",
            "public_origin": "https://private-fixture.example",
            "muse_endpoint": "https://private-fixture.example/muse/mcp",
        });
        let report = tunnel_health(&state, true, true, false).await;
        assert_eq!(report["public_reachability"], "unknown");
        assert_eq!(report["route_catalog"], "partial");
        assert_eq!(report["routes"][0]["reason"], "public_probe_not_requested");
        assert_eq!(report["routes"][0]["authenticated_mcp"], "not_checked");
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
