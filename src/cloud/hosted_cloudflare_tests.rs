use super::*;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const ACCOUNT: &str = "11111111111111111111111111111111";
const ZONE: &str = "22222222222222222222222222222222";
const ZONE_NAME: &str = "example.test";
const ID: &str = "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb";
const DNS_ID: &str = "33333333333333333333333333333333";
const HOST: &str = "pilot.example.test";
const INSTALLATION: &str = "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa";
const API: &str = "synthetic-test-management-token";
const CONNECTOR: &str = "synthetic-test-connector-token";

fn installation() -> Uuid {
    Uuid::parse_str(INSTALLATION).unwrap()
}

fn tunnel() -> serde_json::Value {
    json!({"id":ID, "name":format!("pigeonmq-{INSTALLATION}"),
           "account_tag":ACCOUNT,"config_src":"cloudflare"})
}

fn dns(comment: &str) -> serde_json::Value {
    json!({"id":DNS_ID,"type":"CNAME","name":HOST,
           "content":format!("{ID}.cfargotunnel.com"),"proxied":true,"comment":comment})
}

fn marker() -> String {
    format!("pigeonmq installation {INSTALLATION}")
}

struct Step {
    method: &'static str,
    path: String,
    status: u16,
    result: serde_json::Value,
    payload: Option<serde_json::Value>,
    drop_response: bool,
}

impl Step {
    fn result(method: &'static str, path: String, result: serde_json::Value) -> Self {
        Self {
            method,
            path,
            status: 200,
            result,
            payload: None,
            drop_response: false,
        }
    }

    fn body(mut self, payload: serde_json::Value) -> Self {
        self.payload = Some(payload);
        self
    }

    fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

fn dns_path() -> String {
    format!("/client/v4/zones/{ZONE}/dns_records")
}

fn tunnels_path() -> String {
    format!("/client/v4/accounts/{ACCOUNT}/cfd_tunnel")
}

fn lookup_dns(result: serde_json::Value) -> Step {
    Step::result(
        "GET",
        format!("{}?name.exact={HOST}&page=1&per_page=100", dns_path()),
        result,
    )
}

fn lookup_tunnel(result: serde_json::Value) -> Step {
    Step::result(
        "GET",
        format!(
            "{}?name=pigeonmq-{INSTALLATION}&is_deleted=false&page=1&per_page=100",
            tunnels_path(),
        ),
        result,
    )
}

fn create_tunnel() -> Step {
    Step::result("POST", tunnels_path(), tunnel())
        .body(json!({"name":format!("pigeonmq-{INSTALLATION}"),"config_src":"cloudflare"}))
}

fn configure() -> Step {
    Step::result(
        "PUT",
        format!("{}/{ID}/configurations", tunnels_path()),
        json!({}),
    )
    .body(json!({"config":{"ingress":[
        {"hostname":HOST,"path":"^/muse/mcp$","service":"http://127.0.0.1:54843",
         "originRequest":{"httpHostHeader":"127.0.0.1:54843"}},
        {"service":"http_status:404"}
    ]}}))
}

fn create_dns() -> Step {
    Step::result("POST", dns_path(), dns(&marker())).body(json!({
        "type":"CNAME","name":HOST,"content":format!("{ID}.cfargotunnel.com"),
        "proxied":true,"ttl":1,"comment":marker()
    }))
}

fn get_token() -> Step {
    Step::result(
        "GET",
        format!("{}/{ID}/token", tunnels_path()),
        json!(CONNECTOR),
    )
}

async fn exchange(mut incoming: TcpStream, step: Step) {
    let mut data = Vec::new();
    let (header_end, body_len) = loop {
        if let Some(index) = data.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = std::str::from_utf8(&data[..index]).unwrap();
            let mut lines = head.lines();
            let path = format!("{} {} HTTP/1.1", step.method, step.path);
            assert_eq!(lines.next(), Some(path.as_str()));
            let headers = lines
                .map(|line| line.split_once(':').unwrap())
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(
                headers.get("authorization"),
                Some(&format!("Bearer {API}").as_str())
            );
            let body_len = headers
                .get("content-length")
                .map_or(0, |len| len.parse().unwrap());
            break (index + 4, body_len);
        }
        assert!(data.len() < 8192);
        assert!(incoming.read_buf(&mut data).await.unwrap() > 0);
    };
    while data.len() < header_end + body_len {
        assert!(incoming.read_buf(&mut data).await.unwrap() > 0);
    }
    if let Some(expected) = step.payload {
        let actual: serde_json::Value =
            serde_json::from_slice(&data[header_end..header_end + body_len]).unwrap();
        assert_eq!(actual, expected);
    } else {
        assert_eq!(body_len, 0);
    }
    if step.drop_response {
        return;
    }
    let mut response = json!({"success":step.status==200,"result":step.result});
    if let Some(records) = response["result"].as_array() {
        response["result_info"] =
            json!({"page":1,"per_page":100,"count":records.len(),"total_count":2000});
    }
    let response = response.to_string();
    let bytes = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        step.status,
        response.len(),
    );
    incoming.write_all(bytes.as_bytes()).await.unwrap();
}

async fn fixture(steps: Vec<Step>) -> (CloudflareProvisioner, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut provider = CloudflareProvisioner::new(ACCOUNT, ZONE, ZONE_NAME, API).unwrap();
    provider.api = Url::parse(&format!(
        "http://{}/client/v4/",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        for step in steps {
            tokio::time::timeout(Duration::from_secs(3), async {
                let (incoming, _) = listener.accept().await.unwrap();
                exchange(incoming, step).await;
            })
            .await
            .expect("did not receive the expected Cloudflare request");
        }
    });
    (provider, server)
}

#[tokio::test]
async fn reconciles_definite_and_uncertain_dns_outcomes_without_duplicate_creates() {
    let (provider, server) = fixture(vec![
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
        create_tunnel(),
        configure(),
        create_dns().status(429),
        lookup_dns(json!([])),
        lookup_tunnel(json!([tunnel()])),
        configure(),
        create_dns().status(503),
        lookup_dns(json!([dns(&marker())])),
        lookup_tunnel(json!([tunnel()])),
        configure(),
        get_token(),
        lookup_dns(json!([dns(&marker())])),
        lookup_tunnel(json!([tunnel()])),
        configure(),
        get_token(),
    ])
    .await;

    let mut state = CloudflareResources::default();
    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Http(429))
    ));
    assert_eq!(state.tunnel, ResourceState::Known(ID.to_owned()));
    assert_eq!(state.dns_record, ResourceState::New);

    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Http(503))
    ));
    assert_eq!(state.dns_record, ResourceState::Attempted);

    for _ in 0..2 {
        let result = provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(state.dns_record, ResourceState::Known(DNS_ID.to_owned()));
        assert_eq!(result.origin, format!("https://{HOST}"));
        assert_eq!(result.into_connector_token(), CONNECTOR);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn refuses_dns_owned_by_a_different_installation_before_any_write() {
    let (provider, server) = fixture(vec![lookup_dns(json!([dns("another installation")]))]).await;
    let mut state = CloudflareResources::default();
    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Conflict)
    ));
    assert_eq!(state, CloudflareResources::default());
    server.await.unwrap();
}

#[tokio::test]
async fn keeps_an_unknown_tunnel_create_pending_when_the_list_is_empty() {
    let (provider, server) = fixture(vec![
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
        create_tunnel().status(503),
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
    ])
    .await;
    let mut state = CloudflareResources::default();
    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Http(503))
    ));
    assert_eq!(state.tunnel, ResourceState::Attempted);
    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Uncertain)
    ));
    assert_eq!(state.tunnel, ResourceState::Attempted);
    server.await.unwrap();
}

#[tokio::test]
async fn never_creates_a_tunnel_without_saving_the_attempt() {
    let (provider, server) = fixture(vec![lookup_dns(json!([])), lookup_tunnel(json!([]))]).await;
    let mut state = CloudflareResources::default();
    assert!(matches!(
        provider
            .provision(
                installation(),
                HOST,
                54843,
                &["/muse/mcp"],
                &mut state,
                |next| {
                    assert_eq!(next.tunnel, ResourceState::Attempted);
                    Err(io::Error::other("fixture save failure"))
                }
            )
            .await,
        Err(ProvisionError::Persist)
    ));
    assert_eq!(state, CloudflareResources::default());
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_addresses_or_non_mcp_paths_fail_before_provider_calls() {
    let (provider, server) = fixture(vec![]).await;
    for (host, route) in [
        ("one.two.example.test", "/muse/mcp"),
        ("pilot.example.net", "/muse/mcp"),
        (HOST, "/receiver"),
        (HOST, "/receiver/mcp"),
        (HOST, "/admin/mcp"),
        (HOST, "/muse/mcp.*"),
    ] {
        let mut resources = CloudflareResources::default();
        assert!(matches!(
            provider
                .provision(
                    installation(),
                    host,
                    54843,
                    &[route],
                    &mut resources,
                    |_| Ok(())
                )
                .await,
            Err(ProvisionError::Settings)
        ));
        assert_eq!(resources, CloudflareResources::default());
    }
    server.await.unwrap();
}

#[tokio::test]
async fn completes_fresh_provisioning_and_persists_resource_ids_before_token_fetch() {
    let (provider, server) = fixture(vec![
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
        create_tunnel(),
        configure(),
        create_dns(),
        get_token(),
    ])
    .await;

    let mut state = CloudflareResources::default();
    let mut persisted = Vec::new();
    let result = provider
        .provision(
            installation(),
            HOST,
            54843,
            &["/muse/mcp"],
            &mut state,
            |next| {
                persisted.push(next.clone());
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(persisted.len(), 4);
    assert_eq!(persisted[0].tunnel, ResourceState::Attempted);
    assert_eq!(persisted[1].tunnel, ResourceState::Known(ID.to_owned()));
    assert_eq!(persisted[2].dns_record, ResourceState::Attempted);
    assert_eq!(
        persisted[3].dns_record,
        ResourceState::Known(DNS_ID.to_owned())
    );
    assert_eq!(state, persisted[3]);
    assert_eq!(result.origin, format!("https://{HOST}"));
    assert_eq!(result.into_connector_token(), CONNECTOR);
    server.await.unwrap();
}

#[path = "hosted_cloudflare_lifecycle_tests.rs"]
mod lifecycle_tests;

#[tokio::test]
async fn publishes_explicit_paths_on_one_tunnel_and_keeps_default_denial() {
    let route = |path| {
        json!({"hostname":HOST,"path":path,"service":"http://127.0.0.1:54843",
               "originRequest":{"httpHostHeader":"127.0.0.1:54843"}})
    };
    let (provider, server) = fixture(vec![
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
        create_tunnel(),
        configure().body(json!({"config":{"ingress":[
            route("^/dot/mcp$"),
            route(r"^/\.well-known/oauth-protected-resource/dot/mcp$"),
            route("^/codex/mcp$"),
            route(r"^/\.well-known/oauth-protected-resource/codex/mcp$"),
            {"service":"http_status:404"}
        ]}})),
        create_dns(),
        get_token(),
    ])
    .await;

    let result = provider
        .provision(
            installation(),
            HOST,
            54843,
            &["/dot/mcp", "/codex/mcp"],
            &mut CloudflareResources::default(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(result.origin, format!("https://{HOST}"));
    server.await.unwrap();
}

#[tokio::test]
async fn unix_ingress_uses_only_opaque_socket_and_deny_default() {
    use crate::hosted_origin::UnixOrigin;
    let origin = UnixOrigin::parse("0123456789abcdef0123456789abcdef").unwrap();
    let configure = Step::result(
        "PUT",
        format!("{}/{ID}/configurations", tunnels_path()),
        json!({}),
    )
    .body(json!({"config":{"ingress":[
        {"hostname":HOST,"path":"^/dot/mcp$",
         "service":"unix:/private/tmp/pmq-0123456789abcdef0123456789abcdef/hub.sock",
         "originRequest":{"httpHostHeader":"127.0.0.1:54843"}},
        {"hostname":HOST,"path":r"^/\.well-known/oauth-protected-resource/dot/mcp$",
         "service":"unix:/private/tmp/pmq-0123456789abcdef0123456789abcdef/hub.sock",
         "originRequest":{"httpHostHeader":"127.0.0.1:54843"}},
        {"service":"http_status:404"}
    ]}}));
    let (provider, server) = fixture(vec![
        lookup_dns(json!([])),
        lookup_tunnel(json!([])),
        create_tunnel(),
        configure,
        create_dns(),
        get_token(),
    ])
    .await;
    let mut resources = CloudflareResources::default();
    provider
        .provision_at(
            installation(),
            HOST,
            54843,
            Some(&origin),
            &["/dot/mcp"],
            &mut resources,
            |_| Ok(()),
        )
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn operator_reopens_sqlite_and_reconciles_a_lost_tunnel_create_response() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use http::StatusCode;

    let (home, operator) = crate::tests::fixture();
    let proof = URL_SAFE_NO_PAD.encode([7; 32]);
    let (status, initial) = crate::tests::call(
        &operator,
        Method::POST,
        "/v1/enrollments",
        &proof,
        Some(json!({"request_id": Uuid::new_v4().to_string(), "user_code": "A1B2C3D4"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = Uuid::parse_str(initial["installation_id"].as_str().unwrap()).unwrap();
    let now = crate::epoch_seconds().unwrap();
    let account = Uuid::from_u128(1);
    operator
        .record(|registry| {
            let attempt = registry.start_browser(id, "A1B2C3D4", now)?;
            registry.take_browser(attempt.id, now)?;
            registry.approve(
                id,
                account,
                &std::collections::HashSet::from([account]),
                now,
            )
        })
        .unwrap();

    let hostname = operator.hostname(id);
    // Use the existing protocol fixtures with the server-issued installation and operator scope.
    let translate = |step: Step| {
        let replace = |input: &str| {
            input
                .replace(ACCOUNT, &operator.account_id)
                .replace(ZONE, &operator.zone_id)
                .replace(INSTALLATION, &id.to_string())
                .replace(HOST, &hostname)
        };
        let replace_json =
            |value: serde_json::Value| serde_json::from_str(&replace(&value.to_string())).unwrap();
        Step {
            path: replace(&step.path),
            result: replace_json(step.result),
            payload: step.payload.map(replace_json),
            ..step
        }
    };
    let mut lost_tunnel = create_tunnel();
    lost_tunnel.drop_response = true;
    let (provider, server) = fixture(
        vec![
            lookup_dns(json!([])),
            lookup_tunnel(json!([])),
            lost_tunnel,
            lookup_dns(json!([])),
            lookup_tunnel(json!([tunnel()])),
            configure(),
            create_dns(),
            get_token(),
            lookup_dns(json!([dns(&marker())])),
            lookup_tunnel(json!([tunnel()])),
            configure(),
            get_token(),
        ]
        .into_iter()
        .map(translate)
        .collect(),
    )
    .await;
    let api = provider.api;
    // Use the same synthetic management token that the provider fixture expects.
    radio999k::private_file::replace_private(&home.path().join("cloudflare.key"), API.as_bytes())
        .unwrap();
    let mut operator = crate::Operator::from_home(home.path()).unwrap();
    operator.cloudflare.api = api.clone();
    let path = format!("/v1/enrollments/{id}/finish");
    let (status, _) = crate::tests::call(&operator, Method::POST, &path, &proof, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    for recovered in [false, true] {
        drop(operator);
        operator = crate::Operator::from_home(home.path()).unwrap();
        operator.cloudflare.api = api.clone();
        let record = operator
            .record(|registry| registry.get(id, &proof, crate::epoch_seconds().unwrap()))
            .unwrap();
        let checkpoint = operator
            .bound_resources(record.resources.as_deref())
            .unwrap();
        let expected = if recovered {
            CloudflareResources {
                tunnel: ResourceState::Known(ID.into()),
                dns_record: ResourceState::Known(DNS_ID.into()),
            }
        } else {
            CloudflareResources {
                tunnel: ResourceState::Attempted,
                dns_record: ResourceState::New,
            }
        };
        assert_eq!(checkpoint, expected);

        let (status, result) =
            crate::tests::call(&operator, Method::POST, &path, &proof, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["public_url"], initial["public_url"]);
        assert_eq!(result["connector_token"], CONNECTOR);
    }
    server.await.unwrap();
}
