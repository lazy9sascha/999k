//! Selected peer transport setup. A route probe cannot prove a peer replied.
use crate::{cloud_credentials, hosted_enrollment, stdio_mcp};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path, time::Duration};
use url::Url;

const INVENTORY: &str = "orca-ingress.json";
const VERSION: &str = "2025-03-26";

pub(crate) struct Options<'a> {
    pub(crate) provider: Option<Url>,
    pub(crate) bearer_file: Option<&'a Path>,
}

fn endpoint(raw: &str, route: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let origin = url.origin().ascii_serialization();
    (raw == format!("{origin}{route}") && crate::cloud_transport::public_origin(&origin).is_ok())
        .then_some(url)
}

fn server_names(peer: &str) -> (&'static str, &'static str) {
    match peer {
        "muse" => ("999k-muse", "pigeonmq-muse"),
        "grok-bot" => ("999k-grok-bot", "pigeonmq-cloud-grok-bot"),
        "cue" => ("999k-cue", "pigeonmq-cue"),
        "instinct" => ("999k-instinct", "pigeonmq-instinct"),
        _ => ("", ""),
    }
}

fn credential(home: &Path, peer: &str, path: Option<&Path>) -> Option<String> {
    if let Some(path) = path {
        return stdio_mcp::bearer(path).ok();
    }
    if let Ok(path) = super::resolve_bearer_file(home, peer) {
        return stdio_mcp::bearer(&path).ok();
    }
    let profile = radio999k::mailbox_catalog::http_by_id(peer)?;
    let app = cloud_credentials::App::for_profile(profile)?;
    let bytes = cloud_credentials::read_keychain_bearer(home, app).ok()?;
    String::from_utf8(bytes).ok()
}

fn probe(endpoint: &Url, peer: &str, bearer: &str) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .user_agent(concat!("p-999k/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    else {
        return false;
    };
    let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"p-999k-setup","version":env!("CARGO_PKG_VERSION")}
    }});
    let initialize = || {
        client
            .post(endpoint.clone())
            .header("Accept", "application/json, text/event-stream")
            .json(&request)
    };
    if !initialize()
        .send()
        .is_ok_and(|response| matches!(response.status().as_u16(), 401 | 403))
    {
        return false;
    }
    let Some(reply) = initialize()
        .bearer_auth(bearer)
        .send()
        .ok()
        .and_then(mcp_response)
    else {
        return false;
    };
    let names = server_names(peer);
    if reply["id"] != 1
        || reply["result"]["protocolVersion"] != VERSION
        || !reply["result"]["serverInfo"]["name"]
            .as_str()
            .is_some_and(|name| name == names.0 || name == names.1)
    {
        return false;
    }
    client
        .post(endpoint.clone())
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Protocol-Version", VERSION)
        .bearer_auth(bearer)
        .json(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/list"}))
        .send()
        .ok()
        .and_then(mcp_response)
        .is_some_and(|reply| {
            reply["id"] == 2
                && reply["result"]["tools"].as_array().is_some_and(|tools| {
                    ["p-999k_read", "p-999k_reply"].iter().all(|expected| {
                        tools
                            .iter()
                            .filter_map(|tool| tool["name"].as_str())
                            .map(radio999k::mailbox_tools::canonical_tool_name)
                            .any(|name| name == *expected)
                    })
                })
        })
}

fn mcp_response(response: reqwest::blocking::Response) -> Option<Value> {
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    // A built-in route's tool catalog can be larger than its handshake.
    const MAX_BYTES: u64 = 64 * 1024;
    let mut body = Vec::new();
    response.take(MAX_BYTES + 1).read_to_end(&mut body).ok()?;
    if body.len() as u64 > MAX_BYTES {
        return None;
    }
    let reply = radio999k::mcp_request::strict_json(&body).ok()?;
    (reply["jsonrpc"] == "2.0" && reply.get("error").is_none()).then_some(reply)
}

fn peer_endpoint(
    home: &Path,
    peer: &str,
    route: &str,
    provider: Option<&Url>,
) -> Result<Url, String> {
    let inventory = super::read_private_json::<BTreeMap<String, String>>(&home.join(INVENTORY))
        .map_err(|_| "Check the owner-private orca-ingress.json file")?;
    if let Some(raw) = inventory.as_ref().and_then(|entries| entries.get(peer)) {
        let url = endpoint(raw, route)
            .ok_or("The configured HTTPS endpoint does not match the selected peer route")?;
        if provider.is_some() {
            println!(
                "Using the existing HTTPS endpoint for {peer} from orca-ingress.json. Skipping hosted registration."
            );
        }
        return Ok(url);
    }
    if hosted_enrollment::has_selectable_reservation(home)? {
        if let Some(provider) = provider {
            let origin = hosted_enrollment::reserve_selected_peer(home, provider, route)?;
            return endpoint(&format!("{}{route}", origin.trim_end_matches('/')), route)
                .ok_or_else(|| "The approved hosted address is invalid".into());
        }
        if let Some((_, scope, origin)) = hosted_enrollment::staged_unix(home)?
            && scope.mcp_paths.iter().any(|path| path == route)
        {
            return endpoint(&format!("{}{route}", origin.trim_end_matches('/')), route)
                .ok_or_else(|| "The approved hosted address is invalid".into());
        }
        return Err("Select this peer in Orca setup to attach it to the reserved address.".into());
    }
    let origin = if let Some(provider) = provider {
        hosted_enrollment::reserve_selected_peer(home, provider, route)?
    } else if let Some((_, scope, approved_origin)) = hosted_enrollment::staged_unix(home)?
        && scope.mcp_paths.iter().any(|path| path == route)
    {
        approved_origin
    } else {
        return Err("For operator-hosted setup, repeat the selected-peer command with --orca-reserve-address. For your own HTTPS ingress, add the peer’s exact MCP endpoint to your private orca-ingress.json.".into());
    };
    endpoint(&format!("{}{route}", origin.trim_end_matches('/')), route)
        .ok_or_else(|| "The approved hosted address is invalid".into())
}

pub(crate) fn verified_pairing_origin(
    home: &Path,
    peer: &str,
    bearer_file: &Path,
) -> std::io::Result<String> {
    use std::io::Error;
    let (route, pair) = crate::pairing::route(peer).ok_or_else(|| {
        Error::other("Pairing is available for muse, grok-bot, cue, and instinct")
    })?;
    let url = peer_endpoint(home, peer, route, None).map_err(Error::other)?;
    let bearer = stdio_mcp::bearer(bearer_file)
        .map_err(|_| Error::other("A private bearer for the selected peer is required"))?;
    stdio_mcp::verify_pairing_bearer(home, route, &bearer)?;
    if !probe(&url, peer, &bearer) {
        return Err(Error::other(
            "The selected peer's HTTPS ingress did not pass the authenticated MCP check",
        ));
    }
    if !pairing_route_available(url.origin().ascii_serialization().as_str(), pair) {
        return Err(Error::other(
            "The selected peer's pairing route is not published on the HTTPS ingress",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

fn pairing_route_available(origin: &str, pair: &str) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .user_agent(concat!("p-999k/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    else {
        return false;
    };
    let Ok(response) = client
        .post(format!("{origin}{pair}"))
        .json(&json!({}))
        .send()
    else {
        return false;
    };
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return false;
    }
    let mut body = Vec::new();
    const MAX_BYTES: u64 = 4096;
    response.take(MAX_BYTES + 1).read_to_end(&mut body).is_ok()
        && body.len() as u64 <= MAX_BYTES
        && radio999k::mcp_request::strict_json(&body)
            .is_ok_and(|value| value["error"]["code"] == "pairing_denied")
}

pub(crate) fn dot_oauth_endpoint(home: &Path, route: &str) -> Result<Option<Url>, String> {
    let inventory = super::read_private_json::<BTreeMap<String, String>>(&home.join(INVENTORY))
        .map_err(|_| "Check the owner-private orca-ingress.json file")?;
    inventory
        .as_ref()
        .and_then(|entries| entries.get("dot"))
        .map(|raw| {
            endpoint(raw, route).ok_or_else(|| {
                "The configured Dot endpoint must be the exact HTTPS MCP path.".into()
            })
        })
        .transpose()
}

fn dot_oauth_discovery(endpoint: &Url, issuer: &str) -> bool {
    let check = || -> Option<bool> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("p-999k/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .ok()?;
        let response = client.post(endpoint.clone())
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"p-999k-setup","version":env!("CARGO_PKG_VERSION")}
            }}))
            .send().ok()?;
        if response.status().as_u16() != 401 {
            return Some(false);
        }
        let metadata = [
            "/.well-known/oauth-protected-resource",
            "/.well-known/cloudflare-access-protected-resource",
        ]
        .into_iter()
        .filter_map(|prefix| endpoint.join(&format!("{prefix}{}", endpoint.path())).ok())
        .find(|url| {
            let challenge = format!("resource_metadata=\"{url}\"");
            response
                .headers()
                .get_all(reqwest::header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|header| header.to_str().ok())
                .any(|value| {
                    value.split_once(' ').is_some_and(|(scheme, parameters)| {
                        scheme.eq_ignore_ascii_case("bearer")
                            && parameters.split(',').any(|p| p.trim() == challenge)
                    })
                })
        })?;
        let response = client.get(metadata).send().ok()?;
        if !response.status().is_success() {
            return Some(false);
        }
        const LIMIT: u64 = 16 * 1024;
        let mut body = Vec::new();
        response.take(LIMIT + 1).read_to_end(&mut body).ok()?;
        if body.len() as u64 > LIMIT {
            return Some(false);
        }
        let document = radio999k::mcp_request::strict_json(&body).ok()?;
        Some(
            document["resource"].as_str() == Some(endpoint.as_str())
                && document["authorization_servers"] == json!([issuer]),
        )
    };
    check().unwrap_or(false)
}

fn prepare_dot(
    home: &Path,
    route: &str,
    discover: impl FnOnce(&Url, &str) -> bool,
) -> Result<Option<&'static str>, String> {
    let Some(url) = dot_oauth_endpoint(home, route)? else {
        return Ok(None);
    };
    let config = super::read_private_json::<Value>(&home.join("cloudflare-access.json"))
        .map_err(|_| "Check the owner-private cloudflare-access.json file.")?
        .ok_or("Configure the dedicated Cloudflare Access OAuth binding for Dot.")?;
    let issuer = config["issuer"]
        .as_str()
        .filter(|_| config["dot"].is_object())
        .ok_or("Configure the dedicated Cloudflare Access OAuth binding for Dot.")?;
    if !discover(&url, issuer) {
        return Err("The configured Dot route did not expose matching OAuth discovery. Check the Cloudflare Access app, issuer, and published path.".into());
    }
    Ok(Some(concat!(
        "Remote transport for dot: OAuth discovery ready.\n",
        "Cloudflare Access handles sign-in and records login attempts. ",
        "999K checks your email and account identifier in the signed assertion during each request.\n",
        "Approval gives ChatGPT access to its 999K mailbox. The message queue stays on your Mac.\n",
        "Read the OAuth data notice at https://github.com/lazy9sascha/p-999k/blob/main/docs/configuration.md#oauth-sign-in-and-message-data\n",
        "Add the configured Dot URL in ChatGPT and approve the connection. ",
        "Verify authenticated tools and a linked reply in the original project."
    )))
}

pub(super) fn prepare(home: &Path, peer: &str, options: Options<'_>) {
    let Some(profile) = radio999k::mailbox_catalog::http_by_id(peer) else {
        return;
    };
    let route = profile.route;
    let waiting = |reason: &str| {
        println!("Remote transport for {peer}: waiting. {reason}");
    };
    if peer == "dot" {
        match prepare_dot(home, route, dot_oauth_discovery) {
            Ok(Some(instructions)) => println!("{instructions}"),
            Ok(None) => waiting(
                "For direct OAuth, configure the protected Dot endpoint in owner-private orca-ingress.json. If you already use a private Secure MCP Tunnel, keep its existing connection.",
            ),
            Err(reason) => waiting(&reason),
        }
        return;
    }
    let saved_provider = match hosted_enrollment::saved_route_provider(home, route) {
        Ok(provider) => provider,
        Err(reason) => {
            waiting(&reason);
            return;
        }
    };
    let provider = options.provider.as_ref().or(saved_provider.as_ref());
    let url = match peer_endpoint(home, peer, route, provider) {
        Ok(url) => url,
        Err(reason) => {
            waiting(&reason);
            return;
        }
    };
    if provider.is_none()
        && hosted_enrollment::staged_unix(home).is_ok_and(|staged| {
            staged.is_some_and(|(_, scope, origin)| {
                scope.mcp_paths.iter().any(|path| path == route)
                    && endpoint(&format!("{}{route}", origin.trim_end_matches('/')), route).as_ref()
                        == Some(&url)
            })
        })
        && let Err(err) = crate::lifecycle::schedule_hosted_unix(home)
    {
        waiting(&format!("The hosted credential is still staged: {err}"));
        return;
    }
    let Some(bearer) = credential(home, peer, options.bearer_file) else {
        waiting(
            "The peer's private credential is unavailable. Pass --orca-remote-bearer-file with its owner-private bearer file.",
        );
        return;
    };
    if !probe(&url, peer, &bearer) {
        waiting(
            "The selected route did not prove authenticated MCP access. Review the ingress and matching peer credential, then repeat setup.",
        );
        return;
    }
    println!("Remote transport for {peer}: authenticated MCP route ready.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_uses_only_a_staged_peer_route_without_new_enrollment() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lookup = |peer, route| peer_endpoint(home.path(), peer, route, None);
        assert!(lookup("cue", "/cue/mcp").is_err());
        assert!(!home.path().join("hosted-enrollment.json").exists());
        hosted_enrollment::stage_unix_fixture(
            home.path(),
            uuid::Uuid::new_v4(),
            54843,
            &["/cue/mcp"],
        );
        assert_eq!(
            lookup("cue", "/cue/mcp").unwrap().as_str(),
            "https://u-fixture.example.test/cue/mcp"
        );
        assert!(lookup("muse", "/muse/mcp").is_err());
    }

    #[test]
    fn configured_peer_endpoint_can_replace_an_address_first_reservation() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        hosted_enrollment::stage_unix_fixture(&home, uuid::Uuid::new_v4(), 54843, &[]);
        let enrollment = home.join("hosted-enrollment.json");
        let before = std::fs::read(&enrollment).unwrap();
        let inventory = home.join(INVENTORY);
        radio999k::private_file::replace_private(
            &inventory,
            br#"{"muse":"https://radio.example.test/muse/mcp"}"#,
        )
        .unwrap();
        let provider = Url::parse("https://registration.example.test").unwrap();
        for hosted in [None, Some(&provider)] {
            assert_eq!(
                peer_endpoint(&home, "muse", "/muse/mcp", hosted)
                    .unwrap()
                    .as_str(),
                "https://radio.example.test/muse/mcp"
            );
        }
        assert_eq!(std::fs::read(&enrollment).unwrap(), before);
        assert!(peer_endpoint(&home, "cue", "/cue/mcp", None).is_err());
        radio999k::private_file::replace_private(
            &inventory,
            br#"{"muse":"https://radio.example.test/cue/mcp"}"#,
        )
        .unwrap();
        assert!(peer_endpoint(&home, "muse", "/muse/mcp", Some(&provider)).is_err());
    }

    #[test]
    fn probe_requires_anonymous_denial_and_authenticated_selected_tools() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
        };

        for first_status in [401, 200] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let local_url = Url::parse(&format!(
                "http://{}/muse/mcp",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let server = std::thread::spawn(move || {
                let exchanges = if first_status == 401 { 3 } else { 1 };
                for index in 0..exchanges {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert_eq!(line, "POST /muse/mcp HTTP/1.1\r\n");
                    let mut auth = None;
                    let mut bytes = 0;
                    loop {
                        line.clear();
                        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                        let (key, value) = line.split_once(':').unwrap();
                        if key.eq_ignore_ascii_case("authorization") {
                            auth = Some(value.trim().to_owned());
                        }
                        if key.eq_ignore_ascii_case("content-length") {
                            bytes = value.trim().parse().unwrap();
                        }
                    }
                    assert_eq!(auth.as_deref(), (index > 0).then_some("Bearer fixture"));
                    let mut body = vec![0; bytes];
                    reader.read_exact(&mut body).unwrap();
                    let request = radio999k::mcp_request::strict_json(&body).unwrap();
                    let (status, body) = match index {
                        0 => {
                            assert_eq!(request["method"], "initialize");
                            (first_status, json!({}))
                        }
                        1 => {
                            assert_eq!(request["method"], "initialize");
                            (
                                200,
                                json!({"jsonrpc":"2.0","id":1,"result":{
                                    "protocolVersion":VERSION, "serverInfo":{"name":"999k-muse"}
                                }}),
                            )
                        }
                        _ => {
                            assert_eq!(request["method"], "tools/list");
                            (
                                200,
                                json!({"jsonrpc":"2.0","id":2,"result":{"tools":[
                                    {"name":"pigeonmq_read"},{"name":"p-999k_reply"}
                                ]}}),
                            )
                        }
                    };
                    let body = body.to_string();
                    write!(stream, "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            });
            // Production callers validate HTTPS and the exact route before this point.
            assert_eq!(probe(&local_url, "muse", "fixture"), first_status == 401);
            server.join().unwrap();
        }
    }

    #[test]
    fn dot_uses_explicit_https_ingress_without_changing_the_reservation() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        hosted_enrollment::stage_unix_fixture(&home, uuid::Uuid::new_v4(), 54843, &["/cue/mcp"]);
        assert!(dot_oauth_endpoint(&home, "/dot/mcp").unwrap().is_none());
        assert!(
            prepare_dot(&home, "/dot/mcp", |_, _| panic!("No direct OAuth endpoint"))
                .unwrap()
                .is_none()
        );
        let inventory = home.join(INVENTORY);
        radio999k::private_file::replace_private(
            &inventory,
            br#"{"dot":"https://fixture.example/dot/mcp"}"#,
        )
        .unwrap();
        assert_eq!(
            dot_oauth_endpoint(&home, "/dot/mcp")
                .unwrap()
                .unwrap()
                .as_str(),
            "https://fixture.example/dot/mcp"
        );
        assert!(
            prepare_dot(&home, "/dot/mcp", |_, _| panic!("No Access binding"))
                .unwrap_err()
                .contains("Access OAuth binding")
        );
        radio999k::private_file::replace_private(
            &home.join("cloudflare-access.json"),
            br#"{"issuer":"https://fixture.cloudflareaccess.com","dot":{}}"#,
        )
        .unwrap();
        assert!(prepare_dot(&home, "/dot/mcp", |_, _| false).is_err());
        let instructions = prepare_dot(&home, "/dot/mcp", |url, issuer| {
            assert_eq!(url.as_str(), "https://fixture.example/dot/mcp");
            assert_eq!(issuer, "https://fixture.cloudflareaccess.com");
            true
        })
        .unwrap()
        .unwrap();
        assert!(instructions.starts_with("Remote transport for dot: OAuth discovery ready."));
        let (notice, _) = instructions.split_once("approve the connection").unwrap();
        assert!(notice.contains("Cloudflare Access handles sign-in"));
        assert!(notice.contains("email and account identifier"));
        assert!(notice.contains("The message queue stays on your Mac"));
        assert!(notice.contains("#oauth-sign-in-and-message-data"));
        radio999k::private_file::replace_private(
            &inventory,
            br#"{"dot":"https://fixture.example/codex/mcp"}"#,
        )
        .unwrap();
        assert!(dot_oauth_endpoint(&home, "/dot/mcp").is_err());
    }

    #[test]
    fn dot_discovery_requires_the_selected_resource_and_issuer() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
        };
        let issuer = "https://fixture.cloudflareaccess.com";
        for (status, metadata_path, resource_path, response_issuer, ready) in [
            (
                401,
                "/.well-known/oauth-protected-resource/dot/mcp",
                "/dot/mcp",
                issuer,
                true,
            ),
            (
                401,
                "/.well-known/cloudflare-access-protected-resource/dot/mcp",
                "/dot/mcp",
                issuer,
                true,
            ),
            (
                200,
                "/.well-known/oauth-protected-resource/dot/mcp",
                "/dot/mcp",
                issuer,
                false,
            ),
            (
                401,
                "/.well-known/oauth-protected-resource/codex/mcp",
                "/dot/mcp",
                issuer,
                false,
            ),
            (
                401,
                "/.well-known/oauth-protected-resource/dot/mcp",
                "/codex/mcp",
                issuer,
                false,
            ),
            (
                401,
                "/.well-known/oauth-protected-resource/dot/mcp",
                "/dot/mcp",
                "https://other.cloudflareaccess.com",
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let url = Url::parse(&format!("{origin}/dot/mcp")).unwrap();
            let server = std::thread::spawn(move || {
                let mut exchanges = vec![(
                    "POST /dot/mcp HTTP/1.1".to_owned(),
                    format!(
                        "HTTP/1.1 {status} Response\r\nWWW-Authenticate: Bearer resource_metadata=\"{origin}{metadata_path}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                )];
                if status == 401 && metadata_path.ends_with("/dot/mcp") {
                    let body = json!({"resource":format!("{origin}{resource_path}"),"authorization_servers":[response_issuer]});
                    exchanges.push((
                        format!("GET {metadata_path} HTTP/1.1"),
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.to_string().len()),
                    ));
                }
                for (request_line, response) in exchanges {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert_eq!(line.trim_end(), request_line);
                    let mut length = 0;
                    loop {
                        line.clear();
                        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                        let (name, value) = line.split_once(':').unwrap();
                        assert!(!name.eq_ignore_ascii_case("authorization"));
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    stream.write_all(response.as_bytes()).unwrap();
                }
            });
            assert_eq!(dot_oauth_discovery(&url, issuer), ready);
            server.join().unwrap();
        }
    }

    #[test]
    fn owner_ingress_must_match_the_selected_https_path_exactly() {
        let route = "/muse/mcp";
        assert!(endpoint("https://fixture.example/muse/mcp", route).is_some());
        for url in [
            "http://fixture.example/muse/mcp",
            "https://fixture.example",
            "https://fixture.example/grok/mcp",
            "https://fixture.example/muse/mcp/",
            "https://fixture.example/muse/mcp?x=1",
            "https://user@fixture.example/muse/mcp",
            "https://fixture.example:444/muse/mcp",
            "https://fixture.example/muse/../muse/mcp",
        ] {
            assert!(endpoint(url, route).is_none(), "{url}");
        }
    }
}
#[cfg(test)]
mod pairing_route_tests {
    use super::pairing_route_available;

    #[test]
    fn accepts_structured_denial_only() {
        use std::{io::Read, io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            let body = r#"{"error":{"code":"pairing_denied"}}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert!(pairing_route_available(&origin, "/muse/pair"));
        server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            let body = "not found";
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        assert!(!pairing_route_available(&origin, "/muse/pair"));
        server.join().unwrap();
    }
}
