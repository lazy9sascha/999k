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
}

impl Step {
    fn result(method: &'static str, path: String, result: serde_json::Value) -> Self {
        Self {
            method,
            path,
            status: 200,
            result,
            payload: None,
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
                "/muse/mcp",
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
                "/muse/mcp",
                &mut state,
                |_| Ok(())
            )
            .await,
        Err(ProvisionError::Http(503))
    ));
    assert_eq!(state.dns_record, ResourceState::Attempted);

    for _ in 0..2 {
        let result = provider
            .provision(installation(), HOST, 54843, "/muse/mcp", &mut state, |_| {
                Ok(())
            })
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
                "/muse/mcp",
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
                "/muse/mcp",
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
                "/muse/mcp",
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
                "/muse/mcp",
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
                .provision(installation(), host, 54843, route, &mut resources, |_| Ok(
                    ()
                ))
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
            "/muse/mcp",
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
