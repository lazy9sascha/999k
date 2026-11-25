//! Selected peer transport setup. A route probe cannot prove a peer replied.
use crate::{cloud_credentials, hosted_connector, hosted_enrollment, stdio_mcp};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path, time::Duration};
use url::Url;

const INVENTORY: &str = "orca-ingress.json";
const VERSION: &str = "2025-03-26";

pub(crate) struct Options<'a> {
    pub(crate) reserve_address: bool,
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

pub(crate) fn verified_pairing_origin(
    home: &Path,
    peer: &str,
    bearer_file: &Path,
) -> std::io::Result<String> {
    use std::io::Error;
    let (route, _) = crate::pairing::route(peer)
        .ok_or_else(|| Error::other("Pairing is available for muse, grok-bot, and cue"))?;
    let inventory = super::read_private_json::<BTreeMap<String, String>>(&home.join(INVENTORY))
        .map_err(|_| Error::other("Check the owner-private orca-ingress.json file"))?
        .ok_or_else(|| Error::other("Configure the selected peer's HTTPS ingress in the owner-private orca-ingress.json file"))?;
    let url = inventory.get(peer)
        .and_then(|raw| endpoint(raw, route))
        .ok_or_else(|| Error::other("Add the selected peer's exact HTTPS MCP endpoint to the owner-private orca-ingress.json file"))?;
    let bearer = stdio_mcp::bearer(bearer_file)
        .map_err(|_| Error::other("A private bearer for the selected peer is required"))?;
    stdio_mcp::verify_pairing_bearer(home, route, &bearer)?;
    if !probe(&url, peer, &bearer) {
        return Err(Error::other(
            "The selected peer's HTTPS ingress did not pass the authenticated MCP check",
        ));
    }
    Ok(url.origin().ascii_serialization())
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
        waiting(
            "Connect Dot through its private Secure MCP Tunnel. See the Dot tunnel setup in the configuration guide.",
        );
        return;
    }
    let path = home.join(INVENTORY);
    let configured = match super::read_private_json::<BTreeMap<String, String>>(&path) {
        Ok(map) => map.and_then(|value| value.get(peer).cloned()),
        Err(_) => {
            waiting("Check the owner-private orca-ingress.json file before continuing.");
            return;
        }
    };
    let configured = if let Some(raw) = configured {
        Some(raw)
    } else if options.reserve_address {
        match hosted_enrollment::reserve_selected_peer(home, route) {
            Ok(origin) => Some(format!("{}{route}", origin.trim_end_matches('/'))),
            Err(err) => {
                waiting(&err);
                return;
            }
        }
    } else if let Ok(Some((_, scope))) = hosted_enrollment::staged_unix(home)
        && scope.mcp_paths.iter().any(|path| path == route)
    {
        hosted_connector::load_connection(home)
            .ok()
            .flatten()
            .map(|(origin, _)| format!("{}{route}", origin.trim_end_matches('/')))
    } else {
        None
    };
    let Some(raw) = configured else {
        waiting(
            "Configure an approved HTTPS ingress for the selected MCP route and add it to your private orca-ingress.json. For an operator-run registration provider configured for only this route, set RADIO999K_REGISTRATION_URL and repeat with --orca-reserve-address.",
        );
        return;
    };
    let Some(url) = endpoint(&raw, route) else {
        waiting("The configured HTTPS endpoint does not match the selected peer route.");
        return;
    };
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
