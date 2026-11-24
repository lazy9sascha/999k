//! Loopback RPC transport and authenticated ordinary-message ingress.
use crate::mailbox_startup::Binding;
use crate::mcp_sdk;
use catalog::Adapter;

use crate::auth::{self, Operation, Principal};
use radio999k::{
    DeadLetterInspection, Envelope, Error, Outcome, Queue, mailbox_catalog as catalog,
    mailbox_generation,
};
use radio999k::{
    mailbox_tools::registered_mailbox_tools,
    mcp_request::{
        accepted_cancellation, check_streamable_mailbox_body, registered_mailbox_fields,
        registered_send,
    },
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    net::{Ipv4Addr, TcpListener},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
mod access;
mod cloudflare_access;
mod transport;
#[cfg(test)]
use std::io::Read;
#[cfg(test)]
use std::net::TcpStream;
use transport::{
    Fault, Input, MAX_HEADERS, ProfileHandler, ProfileListener, ProfileStream, WireResponse,
    serve_profile_listeners,
};
const MAX_HTTP_CONNECTION_WORKERS: usize = 8;
const MAX_BODY: usize = 16384;
// JSON may encode one body byte as six ASCII bytes (\uXXXX).
const MAX_MUSE_ENVELOPE: usize = catalog::default_body_limit(MAX_HEADERS);

#[path = "adapters/grok_bot/mod.rs"]
mod grok_bot;
pub(crate) use grok_bot::validate_grok_setup_policy;
pub use grok_bot::{check_grok, prepare_grok};
use radio999k::inbox_page;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    sender: String,
    recipient: String,
    body: String,
    correlation_id: Option<String>,
    in_reply_to: Option<String>,
    idempotency_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiveBody {
    recipient: String,
    #[serde(default = "default_rpc_lease_seconds")]
    lease_seconds: u64,
}
fn default_rpc_lease_seconds() -> u64 {
    // Match the receive-request CLI default in main.rs.
    30
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondBody {
    recipient: String,
    id: String,
    receipt: String,
    result: Outcome,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusBody {
    requester: String,
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DlqBody {
    id: Option<String>,
    continuation: Option<String>,
}

const GENERIC_ROUTES: &[(&str, usize)] = &[
    ("/mcp", MAX_MUSE_ENVELOPE),
    ("/rpc/request", MAX_BODY),
    ("/rpc/receive", MAX_BODY),
    ("/rpc/respond", MAX_BODY),
    ("/rpc/status", MAX_BODY),
    ("/messages/send", MAX_BODY),
    ("/messages/dlq", MAX_BODY),
];

#[cfg(test)]
fn route_body_limit(path: &str) -> Result<usize, Fault> {
    catalog::http_by_route(path)
        .map(|profile| profile.body_limit(MAX_HEADERS))
        .or_else(|| {
            GENERIC_ROUTES
                .iter()
                .find(|(route, _)| *route == path)
                .map(|(_, limit)| *limit)
        })
        .ok_or((404, "route_not_found"))
}
#[cfg(test)]
fn parse_head(bytes: &[u8], authority: &str) -> Result<Input, Fault> {
    transport::parse_head(bytes, authority, route_body_limit)
}
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, Fault> {
    serde_json::from_slice(body).map_err(|_| (400, "invalid_json_or_fields"))
}
fn mapped(error: Error) -> Fault {
    match error {
        Error::Invalid(_) => (400, "invalid_input"),
        Error::NotFound => (404, "not_found"),
        Error::Conflict => (409, "conflict"),
        Error::StaleReceipt => (409, "stale_receipt"),
        Error::Database(_) => (503, "storage_unavailable"),
        Error::Json(_) | Error::Schema(_) => (500, "internal_error"),
    }
}
// Keep the same hidden/not-found response as each adapter's existing boundary.
pub(crate) fn terminal_confirmation_fault(error: Error, mapped: fn(Error) -> Fault) -> Fault {
    match error {
        Error::Invalid("terminal_answer_required") => (400, "terminal_answer_required"),
        Error::Invalid("existing_claim_requires_reconciliation") => {
            (409, "existing_claim_requires_reconciliation")
        }
        Error::Invalid("invalid_terminal_relationship") => mapped(Error::NotFound),
        other => mapped(other),
    }
}

// Deferred adapters have their own exact-read body and field limits. Confirm
// must not expose an ID that exact read would conceal.
pub(crate) fn terminal_observation_fault(error: Fault) -> Fault {
    match error.1 {
        "message_not_available" | "invalid_identifier" | "invalid_body" => (404, "not_found"),
        _ => error,
    }
}

fn mcp_tools() -> Value {
    json!([
        {"name":"radio999k_send","description":"Enqueue an ordinary message. Requires authenticated send authority; does not run an agent or acknowledge a message. Single recipient only: central registration-backed subset preflight is unavailable in this generic profile.","inputSchema":{"type":"object","properties":{"sender":{"type":"string"},"recipient":{"type":"string"},"body":{"type":"string"},"correlation_id":{"type":"string"},"in_reply_to":{"type":"string"},"idempotency_key":{"type":"string"}},"required":["sender","recipient","body"],"additionalProperties":false}},
        {"name":"radio999k_request","description":"Enqueue an RPC request. Returns message ID as request ID.","inputSchema":{"type":"object","properties":{"sender":{"type":"string"},"recipient":{"type":"string"},"body":{"type":"string"},"correlation_id":{"type":"string"},"in_reply_to":{"type":"string"},"idempotency_key":{"type":"string"}},"required":["sender","recipient","body"],"additionalProperties":false}},
        {"name":"radio999k_receive","description":"Claim one RPC request for a recipient with a lease.","inputSchema":{"type":"object","properties":{"recipient":{"type":"string"},"lease_seconds":{"type":"integer"}},"required":["recipient"],"additionalProperties":false}},
        {"name":"radio999k_respond","description":"Store a result and atomically end the claim.","inputSchema":{"type":"object","properties":{"recipient":{"type":"string"},"id":{"type":"string"},"receipt":{"type":"string"},"result":{"type":"object"}},"required":["recipient","id","receipt","result"],"additionalProperties":false}},
        {"name":"radio999k_status","description":"Check request state and result.","inputSchema":{"type":"object","properties":{"requester":{"type":"string"},"id":{"type":"string"}},"required":["requester","id"],"additionalProperties":false}}
    ])
}

fn call_mcp_tool(
    queue: &mut Queue,
    principal: &Principal,
    name: &str,
    args: Value,
) -> Result<Value, Fault> {
    Ok(match name {
        "radio999k_send" => {
            let b: RequestBody =
                serde_json::from_value(args).map_err(|_| (400, "invalid_params"))?;
            send_message(queue, b, principal)?
        }
        "radio999k_request" => {
            let b: RequestBody =
                serde_json::from_value(args).map_err(|_| (400, "invalid_params"))?;
            principal.operation(Operation::Request)?;
            principal.identity(&b.sender)?;
            principal.target(&b.recipient)?;
            serde_json::to_value(
                queue
                    .request(Envelope {
                        sender: b.sender,
                        recipient: b.recipient,
                        body: b.body,
                        correlation_id: b.correlation_id,
                        in_reply_to: b.in_reply_to,
                        idempotency_key: b.idempotency_key,
                    })
                    .map_err(mapped)?,
            )
            .map_err(|_| (500, "internal_error"))?
        }
        "radio999k_receive" => {
            let b: ReceiveBody =
                serde_json::from_value(args).map_err(|_| (400, "invalid_params"))?;
            principal.operation(Operation::Receive)?;
            principal.mailbox(&b.recipient)?;
            serde_json::to_value(
                queue
                    .receive_request(&b.recipient, Duration::from_secs(b.lease_seconds))
                    .map_err(mapped)?,
            )
            .map_err(|_| (500, "internal_error"))?
        }
        "radio999k_respond" => {
            let b: RespondBody =
                serde_json::from_value(args).map_err(|_| (400, "invalid_params"))?;
            principal.operation(Operation::Respond)?;
            principal.mailbox(&b.recipient)?;
            serde_json::to_value(
                queue
                    .respond(&b.recipient, &b.id, &b.receipt, b.result)
                    .map_err(mapped)?,
            )
            .map_err(|_| (500, "internal_error"))?
        }
        "radio999k_status" => {
            let b: StatusBody =
                serde_json::from_value(args).map_err(|_| (400, "invalid_params"))?;
            principal.operation(Operation::Status)?;
            principal.identity(&b.requester)?;
            serde_json::to_value(queue.status(&b.requester, &b.id).map_err(mapped)?)
                .map_err(|_| (500, "internal_error"))?
        }
        _ => return Err((404, "unknown_tool")),
    })
}

// Both wire formats use the same authority check before the first queue write.
fn send_message(
    queue: &mut Queue,
    body: RequestBody,
    principal: &Principal,
) -> Result<Value, Fault> {
    principal.operation(Operation::Send)?;
    principal.identity(&body.sender)?;
    principal.target(&body.recipient)?;
    serde_json::to_value(
        queue
            .send(Envelope {
                sender: body.sender,
                recipient: body.recipient,
                body: body.body,
                correlation_id: body.correlation_id,
                in_reply_to: body.in_reply_to,
                idempotency_key: body.idempotency_key,
            })
            .map_err(mapped)?,
    )
    .map_err(|_| (500, "internal_error"))
}

fn dispatch(
    queue: &mut Queue,
    path: &str,
    body: &[u8],
    principal: &Principal,
) -> Result<Value, Fault> {
    let result = match path {
        "/messages/send" => return send_message(queue, decode(body)?, principal),
        "/rpc/request" => {
            let b: RequestBody = decode(body)?;
            principal.operation(Operation::Request)?;
            principal.identity(&b.sender)?;
            principal.target(&b.recipient)?;
            serde_json::to_value(
                queue
                    .request(Envelope {
                        sender: b.sender,
                        recipient: b.recipient,
                        body: b.body,
                        correlation_id: b.correlation_id,
                        in_reply_to: b.in_reply_to,
                        idempotency_key: b.idempotency_key,
                    })
                    .map_err(mapped)?,
            )
        }
        "/rpc/receive" => {
            let b: ReceiveBody = decode(body)?;
            principal.operation(Operation::Receive)?;
            principal.mailbox(&b.recipient)?;
            serde_json::to_value(
                queue
                    .receive_request(&b.recipient, Duration::from_secs(b.lease_seconds))
                    .map_err(mapped)?,
            )
        }
        "/rpc/respond" => {
            let b: RespondBody = decode(body)?;
            principal.operation(Operation::Respond)?;
            principal.mailbox(&b.recipient)?;
            serde_json::to_value(
                queue
                    .respond(&b.recipient, &b.id, &b.receipt, b.result)
                    .map_err(mapped)?,
            )
        }
        "/rpc/status" => {
            let b: StatusBody = decode(body)?;
            principal.operation(Operation::Status)?;
            principal.identity(&b.requester)?;
            serde_json::to_value(queue.status(&b.requester, &b.id).map_err(mapped)?)
        }
        _ => return Err((404, "route_not_found")),
    };
    result.map_err(|_| (500, "internal_error"))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_dispatch_cannot_cross_a_mailbox_generation() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("queue.sqlite3");
        drop(Queue::open(&db).unwrap());
        for descriptor in catalog::http_profiles()
            .iter()
            .filter(|descriptor| descriptor.adapter != Adapter::Receiver)
        {
            let mailbox = descriptor.id;
            let mut binding = Binding::bundled(
                descriptor,
                temp.path().join(descriptor.policy_file),
                temp.path(),
            );
            let generation = BoundGeneration::load(&db, &binding).unwrap().unwrap();
            let request = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
                binding.route
            );
            let input = parse_head(request.as_bytes(), "127.0.0.1:1234").unwrap();
            let mut routes = HubRoutes::default();
            routes
                .mount_bound(
                    &binding.route,
                    descriptor.body_limit(MAX_HEADERS),
                    RouteHandler::Disabled,
                    Some(generation.clone()),
                )
                .unwrap();
            let state = HubState {
                db: db.clone(),
                routes,
                deadline: None,
            };
            let dispatch =
                || dispatch_hub_request(&state, "127.0.0.1:1234", &input, b"{}", &|| false, None);
            assert!(matches!(
                dispatch(),
                WireResponse::Json((Err((404, "profile_disabled")), 200))
            ));
            let guard = generation.enter(&db).unwrap().unwrap();
            assert!(mailbox_generation::retire(&db, mailbox).is_err());
            drop(guard);
            mailbox_generation::retire(&db, mailbox).unwrap();
            assert!(matches!(
                dispatch(),
                WireResponse::Json((Err((403, "mailbox_generation_retired")), 200))
            ));
            mailbox_generation::reactivate(&db, mailbox).unwrap();
            assert!(matches!(
                dispatch(),
                WireResponse::Json((Err((403, "mailbox_generation_retired")), 200))
            ));
            assert!(BoundGeneration::load(&db, &binding).is_err());
            binding.generation = mailbox_generation::active_storage(&db, mailbox).unwrap();
            let replacement = BoundGeneration::load(&db, &binding).unwrap().unwrap();
            assert!(replacement.enter(&db).unwrap().is_some());
        }
    }

    #[test]
    fn legacy_queue_binding_stays_valid_without_a_generation_lock() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("queue.sqlite3");
        drop(Queue::open(&db).unwrap());
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("DROP TABLE pigeonmq_mailbox_generations", [])
            .unwrap();
        let binding = Binding::bundled(
            catalog::by_adapter(Adapter::Dot),
            temp.path().join("dot-policy.json"),
            temp.path(),
        );
        let generation = BoundGeneration::load(&db, &binding).unwrap().unwrap();
        assert!(generation.enter(&db).unwrap().is_none());
        assert!(!mailbox_generation::lock_path(&db).exists());
    }

    #[test]
    fn dormant_managed_listener_serves_no_routes_and_preserves_queue() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = home.path().join("queue.sqlite3");
        drop(Queue::open(&db).unwrap());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(&db).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(
            serve_hub_on(
                db.clone(),
                None,
                Vec::new(),
                Vec::new(),
                None,
                (addr.port(), Some(1), HubMode::Explicit),
                false,
                Some(listener.try_clone().unwrap()),
                None,
            )
            .unwrap_err()
            .to_string(),
            "explicit policy, port and lifetime limit required"
        );

        let queue = db.clone();
        let serving = std::thread::spawn(move || {
            serve_hub_on(
                queue,
                None,
                Vec::new(),
                Vec::new(),
                Some("managed"),
                (addr.port(), Some(1), HubMode::Managed),
                false,
                Some(listener),
                None,
            )
        });
        for route in ["/dot/mcp", "/mcp", "/receiver", "/rpc/request"] {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            write!(
                stream,
                "POST {route} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
            ).unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            assert_eq!(reply.split_whitespace().nth(1), Some("404"), "{reply}");
            assert!(reply.contains("route_not_found"), "{reply}");
        }
        serving.join().unwrap().unwrap();
        assert_eq!(fs::read(&db).unwrap(), before);
        assert!(!mailbox_generation::lock_path(&db).exists());
    }

    #[test]
    fn catalog_routes_stay_disabled_without_configured_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let mut routes = HubRoutes::default();
        // Even generic auth must not provide fallback access to a disabled profile.
        routes
            .mount_generic(temp.path().join("absent-policy.json"))
            .unwrap();
        for definition in catalog::http_profiles() {
            routes
                .mount(
                    definition.route,
                    definition.body_limit(MAX_HEADERS),
                    RouteHandler::Disabled,
                )
                .unwrap();
        }
        let state = HubState {
            db: temp.path().join("absent.db"),
            routes,
            deadline: None,
        };
        for definition in catalog::http_profiles() {
            let head = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
                definition.route
            );
            let input = parse_head(head.as_bytes(), "127.0.0.1:1234").unwrap();
            assert!(matches!(
                dispatch_hub_request(&state, "127.0.0.1:1234", &input, b"{}", &|| false, None),
                transport::WireResponse::Json((Err((404, "profile_disabled")), 200))
            ));
        }
    }

    #[test]
    fn generic_dlq_http_is_opt_in_scoped_and_read_only() {
        use sha2::{Digest, Sha256};
        use std::{fs, os::unix::fs::PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = temp.path().canonicalize().unwrap().join("queue.db");
        let mut queue = Queue::open(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let envelope = |recipient: &str, in_reply_to: Option<String>| Envelope {
            sender: "synthetic-sender".into(),
            recipient: recipient.into(),
            body: "private synthetic body".into(),
            correlation_id: None,
            in_reply_to,
            idempotency_key: None,
        };
        let first = queue.send(envelope("first", None)).unwrap().message.id;
        let second = queue.send(envelope("second", None)).unwrap().message.id;
        let hidden = queue.send(envelope("hidden", None)).unwrap().message.id;
        let pending = queue.send(envelope("first", None)).unwrap().message.id;
        let terminal = queue
            .send(envelope("first", Some(first.clone())))
            .unwrap()
            .message
            .id;
        let mut visible = vec![first.clone(), second.clone()];
        for i in 0..50 {
            let recipient = if i % 2 == 0 { "first" } else { "second" };
            visible.push(queue.send(envelope(recipient, None)).unwrap().message.id);
        }
        drop(queue);
        let conn = rusqlite::Connection::open(&db).unwrap();
        for id in visible.iter().chain([&hidden, &terminal]) {
            conn.execute(
                "UPDATE messages SET attempts=5,dead_lettered_at_ms=123 WHERE id=?1",
                [id],
            )
            .unwrap();
        }
        drop(conn);

        let policy = temp.path().join("generic.json");
        let principals: Vec<_> = [
            ("inspect-token", "inspect_dlq", vec!["first", "second"]),
            ("read-token", "read", vec!["first"]),
            ("empty-token", "inspect_dlq", vec![]),
        ]
        .into_iter()
        .map(|(token, op, mailboxes)| {
            json!({
                "id": token, "token_sha256": Sha256::digest(token)
                    .iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                "operations": [op], "mailboxes": mailboxes, "send_to": [], "revoked": false,
            })
        })
        .collect();
        fs::write(&policy, json!({"principals": principals}).to_string()).unwrap();
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
        let mut state = HubState {
            db: db.clone(),
            routes: HubRoutes::default(),
            deadline: None,
        };
        let call = |state: &HubState, token: &str, body: Value| -> Result<Value, Fault> {
            let authority = "127.0.0.1:1234";
            let body = body.to_string();
            let head = format!(
                "POST /messages/dlq HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let input = transport::parse_head(head.as_bytes(), authority, |p| {
                state.routes.get(p).map(|route| route.body_limit)
            })?;
            match dispatch_hub_request(state, authority, &input, body.as_bytes(), &|| false, None) {
                transport::WireResponse::Json((result, 200)) => result.map(Option::unwrap),
                _ => panic!("expected generic JSON response"),
            }
        };
        assert_eq!(
            call(&state, "inspect-token", json!({})),
            Err((404, "route_not_found"))
        );
        state.routes.mount_generic(policy).unwrap();
        assert_eq!(
            call(&state, "invalid-token", json!({})),
            Err((401, "unauthorized"))
        );
        assert_eq!(
            call(&state, "read-token", json!({})),
            Err((403, "forbidden"))
        );
        assert_eq!(
            call(&state, "empty-token", json!({})).unwrap(),
            json!({"entries":[],"next_continuation":null})
        );
        let before = fs::read(&db).unwrap();
        let entry =
            |id| json!({"id":id,"state":"dead_lettered","attempts":5,"dead_lettered_at_ms":123});
        let first_page = call(&state, "inspect-token", json!({})).unwrap();
        let cursor = first_page["next_continuation"].as_str().unwrap();
        assert_eq!(
            first_page,
            json!({"entries":visible[..50].iter().map(entry).collect::<Vec<_>>(),
                   "next_continuation":visible[49]})
        );
        assert_eq!(
            call(&state, "inspect-token", json!({"continuation":cursor})).unwrap(),
            json!({"entries":visible[50..].iter().map(entry).collect::<Vec<_>>(),
                   "next_continuation":null})
        );
        for cursor in [hidden.as_str(), pending.as_str(), "absent"] {
            assert_eq!(
                call(&state, "inspect-token", json!({"continuation":cursor})),
                Err((400, "invalid_continuation"))
            );
        }
        assert_eq!(
            call(&state, "empty-token", json!({"continuation":cursor})),
            Err((400, "invalid_continuation"))
        );
        assert_eq!(
            call(
                &state,
                "inspect-token",
                json!({"id":second,"continuation":cursor})
            ),
            Err((400, "invalid_json_or_fields"))
        );
        assert_eq!(
            call(&state, "inspect-token", json!({"id": second})).unwrap(),
            entry(&second)
        );
        for id in [
            hidden.as_str(),
            pending.as_str(),
            terminal.as_str(),
            "absent",
        ] {
            assert_eq!(
                call(&state, "inspect-token", json!({"id":id})).unwrap(),
                json!({"state":"not_dead_lettered_or_missing"})
            );
        }
        assert_eq!(
            call(&state, "inspect-token", json!({"recipient":"hidden"})),
            Err((400, "invalid_json_or_fields"))
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn catalog_routes_enforce_exact_body_limits() {
        // Pin the current wire limits independently of the route lookup and the caps it uses.
        for (route, limit) in [
            ("/muse/mcp", 106_496),
            ("/grok/mcp", 106_496),
            ("/dot/mcp", 32_768),
            ("/codex/mcp", 106_496),
            ("/receiver", 106_496),
        ] {
            let head = |length| {
                format!(
                    "POST {route} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
                )
            };
            assert!(
                parse_head(head(limit).as_bytes(), "127.0.0.1:1234").is_ok(),
                "{route}"
            );
            assert!(
                matches!(
                    parse_head(head(limit + 1).as_bytes(), "127.0.0.1:1234"),
                    Err((413, "body_too_large"))
                ),
                "{route}"
            );
        }
    }

    fn head(extra: &str) -> Vec<u8> {
        format!("POST /rpc/status HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Type: application/json\r\nContent-Length: 2\r\n{extra}\r\n").into_bytes()
    }
    #[test]
    fn rejects_browser_duplicate_framing_and_bad_host() {
        for extra in [
            "Origin: null\r\n",
            "Sec-Fetch-Site: same-origin\r\n",
            "Host: attacker.invalid\r\n",
            "Transfer-Encoding: chunked\r\n",
            "Content-Length: 2\r\n",
        ] {
            assert!(parse_head(&head(extra), "127.0.0.1:1234").is_err());
        }
        assert!(parse_head(&head(""), "127.0.0.1:1234").is_ok());
        assert!(parse_head(&head(""), "localhost:1234").is_err());
    }
    #[test]
    fn grok_and_muse_framing_accept_escaped_16_kib_text() {
        assert_eq!(radio999k::grok_mailbox::MAX_TEXT, 16384);
        assert_eq!(radio999k::muse_mailbox::MAX_REQUEST_BYTES, 16384);
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"radio999k_send","arguments":{{"recipient":"muse","body":"{}","idempotency_key":"framing-fixture"}}}}}}"#,
            "\\u0061".repeat(radio999k::grok_mailbox::MAX_TEXT)
        );
        let decoded: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            decoded["params"]["arguments"]["body"]
                .as_str()
                .unwrap()
                .len(),
            radio999k::grok_mailbox::MAX_TEXT
        );
        for path in ["/grok/mcp", "/muse/mcp"] {
            let request = |length| {
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
                )
            };
            assert!(parse_head(request(body.len()).as_bytes(), "127.0.0.1:1234").is_ok());
            assert_eq!(
                parse_head(
                    request(6 * 16384 + MAX_HEADERS + 1).as_bytes(),
                    "127.0.0.1:1234"
                )
                .err(),
                Some((413, "body_too_large"))
            );
        }
    }
    #[test]
    fn missing_oauth_metadata_preserves_host_and_origin_checks() {
        for path in [
            "/.well-known/oauth-protected-resource/codex/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
        ] {
            let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n");
            assert_eq!(
                parse_head(request.as_bytes(), "127.0.0.1:1234").err(),
                Some((404, "route_not_found"))
            );
            assert_eq!(
                parse_head(request.as_bytes(), "attacker.invalid").err(),
                Some((403, "loopback_host_required"))
            );
            let browser = request.replace("\r\n\r\n", "\r\nOrigin: null\r\n\r\n");
            assert_eq!(
                parse_head(browser.as_bytes(), "127.0.0.1:1234").err(),
                Some((403, "browser_requests_forbidden"))
            );
        }
        assert_eq!(
            parse_head(
                b"GET /codex/mcp HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n",
                "127.0.0.1:1234"
            )
            .err(),
            Some((405, "post_required"))
        );
    }
    struct GenericFixture {
        _dir: tempfile::TempDir,
        state: HubState,
    }

    impl GenericFixture {
        fn new() -> Self {
            use sha2::{Digest, Sha256};
            use std::{fs, os::unix::fs::PermissionsExt};
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let db = dir.path().join("generic.sqlite3");
            drop(Queue::open(&db).unwrap());
            fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
            let policy = dir.path().join("generic.json");
            let principals: Vec<_> = [
                (
                    "sender",
                    vec!["send", "request", "status"],
                    vec!["worker"],
                    vec![],
                ),
                ("worker", vec!["receive", "respond"], vec![], vec!["worker"]),
                ("observer", vec!["read"], vec![], vec!["worker"]),
            ]
            .into_iter()
            .map(|(id, operations, send_to, mailboxes)| {
                let token_sha256 = Sha256::digest(format!("{id}-fixture"))
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                json!({"id":id,"token_sha256":token_sha256,"operations":operations,
                    "send_to":send_to,"mailboxes":mailboxes,"revoked":false})
            })
            .collect();
            fs::write(&policy, json!({"principals":principals}).to_string()).unwrap();
            fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
            let mut routes = HubRoutes::default();
            routes.mount_generic(policy).unwrap();
            Self {
                _dir: dir,
                state: HubState {
                    db,
                    routes,
                    deadline: None,
                },
            }
        }

        fn post(&self, path: &str, token: &str, headers: &str, body: &str) -> (u16, Vec<u8>) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let authority = address.to_string();
            thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let (stream, _) = listener.accept().unwrap();
                    handle_hub_connection(stream, &authority, &self.state);
                });
                let mut stream = TcpStream::connect(address).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                write!(
                    stream, "POST {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n{body}",
                    body.len(),
                ).unwrap();
                let mut wire = Vec::new();
                stream.read_to_end(&mut wire).unwrap();
                worker.join().unwrap();
                let offset = wire.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
                let status = std::str::from_utf8(&wire[..offset])
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse()
                    .unwrap();
                (status, wire.split_off(offset + 4))
            })
        }

        fn rpc(&self, token: &str, method: &str, params: Value) -> (u16, Value) {
            let body = json!({"jsonrpc":"2.0","id":"generic-test","method":method,"params":params});
            let (status, reply) = self.post("/mcp", token, ACCEPT_MCP, &body.to_string());
            (
                status,
                serde_json::from_slice(&reply).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&reply).into_owned())
                }),
            )
        }

        fn http(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
            let (status, reply) = self.post(path, token, "", &body.to_string());
            (status, serde_json::from_slice(&reply).unwrap())
        }

        fn tool(&self, token: &str, name: &str, arguments: Value) -> (u16, Value) {
            self.rpc(
                token,
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            )
        }
    }
    const ACCEPT_MCP: &str = "Accept: application/json, text/event-stream\r\n";

    fn unpack((status, response): (u16, Value)) -> Value {
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], "generic-test");
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["content"][0]["type"], "text");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn generic_mcp_uses_streamable_http_without_an_alias_or_fallback() {
        let f = GenericFixture::new();
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        for (path, token, headers, status) in [
            ("/mcp", "wrong", "", 401),
            ("/mcp", "sender-fixture", "", 406),
            (
                "/mcp",
                "sender-fixture",
                "Accept: application/json\r\n",
                406,
            ),
            (
                "/mcp",
                "sender-fixture",
                "Origin: http://localhost\r\n",
                403,
            ),
            ("/mcp", "sender-fixture", ACCEPT_MCP, 200),
            ("/mailbox/mcp", "sender-fixture", ACCEPT_MCP, 404),
        ] {
            assert_eq!(f.post(path, token, headers, ping).0, status);
        }

        let (status, reply) = f.rpc("sender-fixture", "initialize", json!({
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
        }));
        assert_eq!(status, 200);
        assert_eq!(reply["id"], "generic-test");
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "999k");
        assert!(reply["result"]["capabilities"].get("events").is_none());

        let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(
            f.post("/mcp", "sender-fixture", ACCEPT_MCP, initialized),
            (202, Vec::new())
        );
        let (status, listed) = f.rpc("sender-fixture", "tools/list", json!({}));
        assert_eq!(status, 200);
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "radio999k_request"));
        let find = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
        let schema = &find("radio999k_send")["inputSchema"];
        assert_eq!(schema["required"], json!(["sender", "recipient", "body"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            find("radio999k_receive")["inputSchema"]["required"],
            json!(["recipient"])
        );
        for method in ["resources/list", "events/list"] {
            let (status, response) = f.rpc("sender-fixture", method, json!({}));
            assert_eq!(status, 200);
            assert_eq!(response["error"]["code"], -32601);
        }

        assert_eq!(
            f.post(
                "/mcp",
                "sender-fixture",
                ACCEPT_MCP,
                r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"tools/list"}"#
            )
            .0,
            400
        );
        for arguments in [json!(["sender", "worker"]), json!(false), json!("bad")] {
            let (status, response) = f.tool("sender-fixture", "radio999k_request", arguments);
            assert!(
                matches!(status, 400 | 415) || response.get("error").is_some(),
                "invalid arguments accepted: {status} {response}"
            );
        }
        let (status, unknown) = f.tool("sender-fixture", "not_a_tool", json!({}));
        assert_eq!(status, 200);
        assert_eq!(unknown["error"]["code"], -32602);
        let (status, invalid) = f.tool(
            "worker-fixture",
            "radio999k_receive",
            json!({"lease_seconds":30}),
        );
        assert_eq!(status, 200);
        assert_eq!(invalid["error"]["code"], -32602);
    }

    #[test]
    fn generic_mcp_rpc_preserves_identity_operation_mailbox_and_receipt_guards() {
        let f = GenericFixture::new();
        let request = json!({"sender":"sender","recipient":"worker","body":"synthetic RPC"});
        let forbidden = (403, json!({"error":{"code":"forbidden"}}));
        assert_eq!(
            f.tool("observer-fixture", "radio999k_request", request.clone()),
            forbidden
        );
        for args in [
            json!({"sender":"observer","recipient":"worker","body":"spoof"}),
            json!({"sender":"sender","recipient":"outside","body":"outside target"}),
        ] {
            assert_eq!(
                f.tool("sender-fixture", "radio999k_request", args),
                forbidden
            );
        }
        let queued = unpack(f.tool("sender-fixture", "radio999k_request", request));
        let id = &queued["message"]["id"];
        assert_eq!(
            f.tool(
                "worker-fixture",
                "radio999k_receive",
                json!({"recipient":"outside"})
            ),
            forbidden
        );
        let receive = unpack(f.tool(
            "worker-fixture",
            "radio999k_receive",
            json!({"recipient":"worker"}),
        ));
        assert_eq!(&receive["message"]["id"], id);
        let answer = json!({"outcome":"success","value":{"ok":true}});
        assert_eq!(
            f.tool(
                "worker-fixture",
                "radio999k_respond",
                json!({"recipient":"worker","id":id,"receipt":"wrong","result":answer})
            ),
            (409, json!({"error":{"code":"stale_receipt"}})),
        );
        let completed = unpack(f.tool(
            "worker-fixture",
            "radio999k_respond",
            json!({"recipient":"worker","id":id,"receipt":receive["receipt"],"result":answer}),
        ));
        assert_eq!(completed, answer);
        assert_eq!(
            f.tool(
                "sender-fixture",
                "radio999k_status",
                json!({"requester":"worker","id":id})
            ),
            forbidden
        );
        let status = unpack(f.tool(
            "sender-fixture",
            "radio999k_status",
            json!({"requester":"sender","id":id}),
        ));
        assert_eq!(status["state"], "answered");
    }

    #[test]
    fn generic_raw_http_and_mcp_keep_their_operations_and_authority_separate() {
        let f = GenericFixture::new();
        let send = json!({"sender":"sender","recipient":"worker","body":"synthetic send"});
        assert_eq!(
            f.http("/messages/send", "sender-fixture", send.clone()).0,
            200
        );
        assert_eq!(f.http("/messages/send", "observer-fixture", send).0, 403);
        assert_eq!(
            f.tool("sender-fixture", "radio999k_send", json!({"sender":"sender","recipient":"worker,observer","body":"synthetic","idempotency_key":"subset"})),
            (503, json!({"error":{"code":"recipient_preflight_unavailable"}})),
        );
        let rows: i64 = rusqlite::Connection::open(&f.state.db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);

        for invalid in [
            json!(0),
            Value::Null,
            json!(u64::MAX),
            json!(-1),
            json!(1.5),
            json!("30"),
        ] {
            let args = json!({"recipient":"worker","lease_seconds":invalid});
            assert_eq!(
                f.http("/rpc/receive", "worker-fixture", args.clone()).0,
                400
            );
            let (status, response) = f.tool("worker-fixture", "radio999k_receive", args);
            assert!(status == 400 || response["error"]["code"] == -32602);
        }
        for (mcp, args, seconds) in [
            (true, json!({"recipient":"worker"}), 30),
            (false, json!({"recipient":"worker"}), 30),
            (true, json!({"recipient":"worker","lease_seconds":75}), 75),
            (false, json!({"recipient":"worker","lease_seconds":75}), 75),
        ] {
            let (status, queued) = f.http(
                "/rpc/request",
                "sender-fixture",
                json!({"sender":"sender","recipient":"worker","body":"synthetic RPC"}),
            );
            assert_eq!(status, 200);
            let now = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64
            };
            let before = now();
            let delivery = if mcp {
                unpack(f.tool("worker-fixture", "radio999k_receive", args))
            } else {
                let (status, result) = f.http("/rpc/receive", "worker-fixture", args);
                assert_eq!(status, 200);
                result
            };
            let after = now();
            assert_eq!(delivery["message"]["id"], queued["message"]["id"]);
            let until = delivery["lease_until_ms"].as_i64().unwrap();
            assert!((before + seconds * 1000..=after + seconds * 1000).contains(&until));
        }
        assert_eq!(
            f.http(
                "/rpc/receive",
                "worker-fixture",
                json!({"recipient":"worker"})
            ),
            (200, Value::Null)
        );
    }

    #[test]
    fn request_fields_and_error_mapping_are_safe() {
        assert!(decode::<StatusBody>(br#"{"requester":"a","id":"x","identity":"admin"}"#).is_err());
        assert_eq!(
            mapped(Error::Invalid("private data")),
            (400, "invalid_input")
        );
        assert_eq!(mapped(Error::StaleReceipt), (409, "stale_receipt"));
    }
}

// Muse is a separate authenticated profile of the existing loopback transport.
// This path never creates a queue, emits Events, starts a model, or uses dot ACLs.
use radio999k::muse_mailbox::{self, MailboxError, MuseScope};
use rmcp::{
    ErrorData,
    model::{ErrorCode, RequestId},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
struct MuseState {
    db: PathBuf,
    policy: PathBuf,
    deadline: Option<Instant>,
    waiting: AtomicBool,
    active: Mutex<HashMap<RequestId, Arc<AtomicBool>>>,
}
struct ActiveMuseRequest<'a> {
    state: &'a MuseState,
    key: RequestId,
    cancelled: Arc<AtomicBool>,
}
impl Drop for ActiveMuseRequest<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.state.active.lock() {
            active.remove(&self.key);
        }
    }
}
struct MuseWaitGuard<'a>(&'a AtomicBool);
impl Drop for MuseWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl MuseState {
    fn begin(&self, key: RequestId) -> Result<ActiveMuseRequest<'_>, Fault> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| (503, "request_state_unavailable"))?;
        if active.contains_key(&key) || active.len() >= MAX_HTTP_CONNECTION_WORKERS {
            return Err((409, "request_already_active"));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        active.insert(key.clone(), cancelled.clone());
        Ok(ActiveMuseRequest {
            state: self,
            key,
            cancelled,
        })
    }
    fn check_open(&self, active: &ActiveMuseRequest<'_>, deadline: Instant) -> Result<(), Fault> {
        if active.cancelled.load(Ordering::Acquire)
            || Instant::now() >= deadline
            || self.deadline.is_some_and(|end| Instant::now() >= end)
        {
            return Err((499, "request_cancelled"));
        }
        Ok(())
    }
    fn scope(
        &self,
        authorization: &[Vec<u8>],
        operation: auth::MuseOperation,
        active: &ActiveMuseRequest<'_>,
        deadline: Instant,
    ) -> Result<MuseScope, Fault> {
        self.check_open(active, deadline)?;
        auth::authenticate_muse(&self.policy, authorization)?.scope(operation)
    }
    fn send_checkpoint(
        &self,
        authorization: &[Vec<u8>],
        active: &ActiveMuseRequest<'_>,
        deadline: Instant,
        recipient: &str,
        thread_peers: Option<&[String]>,
    ) -> Result<(), Fault> {
        self.check_open(active, deadline)?;
        let principal = auth::authenticate_muse(&self.policy, authorization)?;
        let scope = principal.scope(auth::MuseOperation::Send)?;
        auth::check_registered_send(&scope.allowed_senders, recipient, thread_peers, || {
            self.check_open(active, deadline)?;
            principal.require_operation(auth::MuseOperation::Read)
        })
    }
    fn confirm_scope(
        &self,
        authorization: &[Vec<u8>],
        active: &ActiveMuseRequest<'_>,
        deadline: Instant,
    ) -> Result<MuseScope, Fault> {
        // Reload at each checkpoint, not once per request or per individual grant.
        self.check_open(active, deadline)?;
        let principal = auth::authenticate_muse(&self.policy, authorization)?;
        let scope = principal.scope(auth::MuseOperation::Reply)?;
        self.check_open(active, deadline)?;
        principal.require_operation(auth::MuseOperation::Read)?;
        Ok(scope)
    }
}

fn muse_fault(error: MailboxError) -> Fault {
    let status = match error.reason {
        "forbidden" => 403,
        "unauthorized" | "credential_expired" => 401,
        "request_cancelled" => 499,
        "message_not_available" => 404,
        "wait_busy"
        | "reply_conflict"
        | "reconciliation_required"
        | "existing_claim_requires_reconciliation" => 409,
        "storage_unavailable" | "auth_unavailable" => 503,
        _ => 400,
    };
    (status, error.reason)
}

#[cfg(test)]
#[path = "inbox_page_host_tests.rs"]
mod inbox_page_host_tests;

fn dispatch_muse(
    state: &MuseState,
    authority: &str,
    input: &Input,
    body: &[u8],
    (max_body, deadline): (usize, Instant),
) -> Result<http::Response<Vec<u8>>, Fault> {
    auth::authenticate_muse(&state.policy, &input.authorization)?;
    if Instant::now() >= deadline {
        return Err((499, "request_cancelled"));
    }
    check_streamable_mailbox_body(body)?;
    let request = mcp_sdk::streamable_request(&input.path, &input.sdk_headers, body)?;
    let mut active = None;
    let response = mcp_sdk::dispatch_streamable(
        "999k-muse",
        registered_mailbox_tools("Muse", "muse"),
        request,
        authority,
        (max_body, deadline),
        |id, name, arguments| {
            if active.is_none() {
                active = Some(state.begin(id.clone())?);
            }
            call_muse_tool(
                state,
                &input.authorization,
                active.as_ref().ok_or((500, "internal_error"))?,
                deadline,
                name,
                &arguments,
            )
        },
        |_, _| {
            Ok(Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "method_not_found",
                None,
            )))
        },
    )?;
    if let Some(id) = accepted_cancellation(response.status(), body)? {
        let active = state
            .active
            .lock()
            .map_err(|_| (503, "request_state_unavailable"))?;
        if let Some(cancelled) = active.get(&id) {
            cancelled.store(true, Ordering::Release);
        }
        // A completion that already committed is not undone.
    }
    Ok(response)
}

fn call_muse_tool(
    state: &MuseState,
    authorization: &[Vec<u8>],
    active: &ActiveMuseRequest<'_>,
    deadline: Instant,
    name: &str,
    arguments: &Value,
) -> Result<Value, Fault> {
    Ok(match name {
        "radio999k_inbox" => inbox_page::list_muse(&state.db, "muse", arguments, deadline, || {
            state
                .scope(authorization, auth::MuseOperation::Read, active, deadline)
                .map(|scope| scope.allowed_senders)
        })?,
        "radio999k_recipients" => {
            registered_mailbox_fields(arguments, &[], &[])?;
            let scope = state.scope(authorization, auth::MuseOperation::Send, active, deadline)?;
            json!({"recipients":scope.allowed_senders,"connection_status":"unknown"})
        }
        "radio999k_confirm_terminal" => {
            let args = registered_mailbox_fields(arguments, &["message_id"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            if message_id.trim().is_empty() || message_id.len() > 128 {
                return Err((400, "invalid_fields"));
            }
            let confirm_scope = || state.confirm_scope(authorization, active, deadline);
            let visible = |sender: &str| {
                if confirm_scope()?
                    .allowed_senders
                    .iter()
                    .any(|peer| peer == sender)
                {
                    Ok(())
                } else {
                    Err((404, "not_found"))
                }
            };
            let observed = muse_mailbox::read(&state.db, message_id, &confirm_scope()?)
                .map_err(|error| terminal_observation_fault(muse_fault(error)))?;
            if observed.in_reply_to.is_none() {
                visible(&observed.sender)?;
                return Err((400, "terminal_answer_required"));
            }
            let mut queue = Queue::open_existing(&state.db).map_err(mapped)?;
            let mut fault = None;
            let result = queue.confirm_terminal_checked("muse", message_id, |sender| {
                visible(sender).map_err(|error| {
                    fault = Some(error);
                    Error::Invalid("authorization_changed")
                })
            });
            let duplicate = result.map_err(|error| {
                fault.unwrap_or_else(|| terminal_confirmation_fault(error, mapped))
            })?;
            json!({"state":"acknowledged","message_id":message_id,"duplicate":duplicate})
        }
        radio999k::mailbox_tools::READ_THREAD => radio999k::conversation::read_thread_tool(
            &state.db,
            "muse",
            muse_mailbox::MAX_REQUEST_BYTES as i64,
            arguments,
            || {
                state
                    .scope(authorization, auth::MuseOperation::Read, active, deadline)
                    .map(|s| s.allowed_senders)
            },
        )?,
        "radio999k_read" => {
            let args = registered_mailbox_fields(arguments, &["message_id"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            let scope = state.scope(authorization, auth::MuseOperation::Read, active, deadline)?;
            let observed = muse_mailbox::read(&state.db, message_id, &scope).map_err(muse_fault)?;
            let current =
                state.scope(authorization, auth::MuseOperation::Read, active, deadline)?;
            if !current.allowed_senders.contains(&observed.sender) {
                return Err((403, "forbidden"));
            }
            serde_json::to_value(observed).map_err(|_| (500, "serialization_failed"))?
        }
        "radio999k_wait" => {
            let args = registered_mailbox_fields(arguments, &["timeout_ms"], &[])?;
            let timeout = args["timeout_ms"]
                .as_u64()
                .filter(|value| *value <= 10000)
                .ok_or((400, "invalid_timeout"))?;
            state.scope(authorization, auth::MuseOperation::Wait, active, deadline)?;
            if state
                .waiting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err((409, "wait_busy"));
            }
            let _waiting = MuseWaitGuard(&state.waiting);
            let until = deadline.min(Instant::now() + Duration::from_millis(timeout));
            // Internal raw-page position only. It is never accepted from
            // clients, persisted, returned as a cursor, or treated as ACK.
            // Requests take priority so a repeated terminal answer cannot
            // hide a pending request. Terminal observation still does not ACK.
            let mut after_seq = 0;
            let mut terminal = false;
            loop {
                let scope =
                    state.scope(authorization, auth::MuseOperation::Wait, active, deadline)?;
                if timeout != 0 && Instant::now() >= until {
                    break json!({"state":"timeout","message":null});
                }
                let page = muse_mailbox::scan_page(&state.db, &scope, after_seq, terminal)
                    .map_err(muse_fault)?;
                let current =
                    state.scope(authorization, auth::MuseOperation::Wait, active, deadline)?;
                if timeout != 0 && Instant::now() >= until {
                    break json!({"state":"timeout","message":null});
                }
                if let Some(observed) = page.message {
                    if !current.allowed_senders.contains(&observed.sender) {
                        return Err((403, "forbidden"));
                    }
                    break json!({"state":"message","message":observed});
                }
                if page.scanned == muse_mailbox::SCAN_PAGE_SIZE {
                    if timeout == 0 {
                        // Do not misreport an incomplete zero-wait scan as
                        // an empty mailbox. Use a bounded wait or read by ID.
                        return Err((503, "observation_budget_exhausted"));
                    }
                    after_seq = page.next_seq;
                    continue;
                }
                if !terminal {
                    terminal = true;
                    after_seq = 0;
                    continue;
                }
                let now = Instant::now();
                if timeout == 0 || now >= until {
                    break json!({"state":"timeout","message":null});
                }
                after_seq = 0;
                terminal = false;
                thread::sleep((until - now).min(Duration::from_millis(100)));
            }
        }
        "radio999k_reply" => {
            let args = registered_mailbox_fields(arguments, &["message_id", "body"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            let body = args["body"].as_str().ok_or((400, "invalid_fields"))?;
            let result = muse_mailbox::reply(&state.db, message_id, body, || {
                state
                    .scope(authorization, auth::MuseOperation::Reply, active, deadline)
                    .map_err(|(_, reason)| MailboxError { reason })
            })
            .map_err(muse_fault)?;
            serde_json::to_value(result).map_err(|_| (500, "serialization_failed"))?
        }
        "radio999k_send" => send_registered(
            &state.db,
            "muse",
            muse_mailbox::MAX_REQUEST_BYTES as i64,
            arguments,
            || {
                state
                    .scope(authorization, auth::MuseOperation::Read, active, deadline)
                    .map(|scope| scope.allowed_senders)
            },
            |recipient, peers| {
                state.send_checkpoint(authorization, active, deadline, recipient, peers)
            },
        )?,
        _ => return Err((404, "unknown_tool")),
    })
}

fn send_registered(
    db: &std::path::Path,
    mailbox: &str,
    request_bytes: i64,
    arguments: &Value,
    read_peers: impl FnOnce() -> Result<Vec<String>, Fault>,
    mut send_checkpoint: impl FnMut(&str, Option<&[String]>) -> Result<(), Fault>,
) -> Result<Value, Fault> {
    let (envelope, parent) = registered_send(mailbox, arguments)?;
    let recipient = envelope.recipient.clone();
    let peers = parent.map(|_| read_peers()).transpose()?;
    let mut fault = None;
    // The queue calls this before locking, after locking and before committing.
    // Never carry a policy or registry snapshot across those boundaries.
    let authorize = || {
        send_checkpoint(&recipient, peers.as_deref()).map_err(|error| {
            fault = Some(error);
            Error::Invalid("forbidden")
        })
    };
    let mut queue = Queue::open_existing(db).map_err(mapped)?;
    let result = match (parent, peers.as_deref()) {
        (Some(parent), Some(peers)) => queue.continue_checked(
            envelope,
            parent,
            radio999k::conversation::ThreadScope {
                mailbox,
                peers,
                request_bytes,
            },
            authorize,
        ),
        _ => queue.send_checked(envelope, authorize),
    };
    let result = result.map_err(|error| fault.unwrap_or_else(|| mapped(error)))?;
    serde_json::to_value(result).map_err(|_| (500, "internal_error"))
}

/// Read-only readiness. A policy file is configuration, not permission to create credentials.
pub fn check_muse(db: &std::path::Path, policy: &std::path::Path) -> io::Result<bool> {
    auth::require_private_file(db).map_err(|_| io::Error::other("private database required"))?;
    auth::validate_muse(policy)
        .map_err(|(_, reason)| auth::local_policy_error(reason, "invalid Muse policy"))?;
    muse_mailbox::is_prepared(db).map_err(|_| io::Error::other("Muse storage unavailable"))
}

/// Explicit approved installation. Never called implicitly by check or serve.
pub fn prepare_muse(db: &std::path::Path, policy: &std::path::Path) -> io::Result<()> {
    auth::require_private_file(db).map_err(|_| io::Error::other("private database required"))?;
    auth::local_muse_scope(policy, auth::MuseOperation::Reply)
        .map_err(|(_, reason)| auth::local_policy_error(reason, "Muse preparation forbidden"))?;
    muse_mailbox::prepare(db).map_err(|_| io::Error::other("Muse preparation failed"))
}

/// One listener/origin, fixed paths and separate authentication profiles.
/// No token-based fallback, routing headers, new queue, credential or tunnel setup.
struct ReceiverProfile {
    policy: PathBuf,
    registration: PathBuf,
    identity: (u64, u64),
}
impl ReceiverProfile {
    fn new(db: &std::path::Path, policy: PathBuf, registration: PathBuf) -> Result<Self, Fault> {
        use std::os::unix::fs::MetadataExt;
        auth::validate_private(&policy)?;
        auth::require_private_file(&registration)?;
        let metadata = radio999k::private_file::open_private_file(db)?
            .metadata()
            .map_err(|_| (503, "storage_unavailable"))?;
        Ok(Self {
            policy,
            registration,
            identity: (metadata.dev(), metadata.ino()),
        })
    }
    fn check_queue(&self, db: &std::path::Path) -> Result<(), Fault> {
        use std::os::unix::fs::MetadataExt;
        let metadata = radio999k::private_file::open_private_file(db)?
            .metadata()
            .map_err(|_| (503, "storage_unavailable"))?;
        if self.identity != (metadata.dev(), metadata.ino()) {
            return Err((503, "queue_identity_changed"));
        }
        Ok(())
    }
}
enum RouteHandler {
    Disabled,
    Generic(Arc<PathBuf>),
    Muse(Arc<MuseState>),
    Dot(radio999k::dot_profile::DotHost),
    Grok(grok_bot::GrokState),
    Mailbox(
        crate::codex_mcp::Profile,
        Option<Arc<crate::codex_events::Runtime>>,
    ),
    Receiver(ReceiverProfile),
}

#[derive(Clone)]
struct BoundGeneration {
    mailbox: &'static str,
    storage: String,
}

impl BoundGeneration {
    fn load(db: &Path, binding: &Binding) -> io::Result<Option<Self>> {
        if binding.descriptor.adapter == Adapter::Receiver {
            if binding.generation.is_some() {
                return Err(io::Error::other(
                    "receiver does not support managed mailbox generations",
                ));
            }
            return Ok(None);
        }
        if binding
            .generation
            .as_deref()
            .is_some_and(|storage| !mailbox_generation::valid_storage(storage))
        {
            return Err(io::Error::other("invalid mailbox generation"));
        }
        let mailbox = binding.descriptor.id;
        let generation = Self {
            mailbox,
            storage: binding.generation.clone().unwrap_or_else(|| mailbox.into()),
        };
        match generation.check(db) {
            Ok(()) => Ok(Some(generation)),
            Err((_, reason)) => Err(io::Error::other(reason)),
        }
    }

    fn check(&self, db: &Path) -> Result<(), Fault> {
        let active = mailbox_generation::active_storage(db, self.mailbox)
            .map_err(|_| (503, "mailbox_generation_unavailable"))?;
        if active.as_deref() != Some(self.storage.as_str()) {
            return Err((403, "mailbox_generation_retired"));
        }
        Ok(())
    }

    fn enter(&self, db: &Path) -> Result<Option<std::fs::File>, Fault> {
        let lock = mailbox_generation::lock_dispatch(db)
            .map_err(|_| (503, "mailbox_generation_unavailable"))?;
        self.check(db)?;
        Ok(lock)
    }
}

struct HubRoute {
    body_limit: usize,
    handler: RouteHandler,
    generation: Option<BoundGeneration>,
    access: Option<access::MailboxAccess>,
}

#[derive(Default)]
struct HubRoutes(HashMap<String, HubRoute>);

impl HubRoutes {
    fn mount(&mut self, path: &str, body_limit: usize, handler: RouteHandler) -> io::Result<()> {
        self.mount_bound(path, body_limit, handler, None)
    }

    fn mount_bound(
        &mut self,
        path: &str,
        body_limit: usize,
        handler: RouteHandler,
        generation: Option<BoundGeneration>,
    ) -> io::Result<()> {
        use std::collections::hash_map::Entry;
        let Entry::Vacant(route) = self.0.entry(path.into()) else {
            return Err(io::Error::other("duplicate hub route"));
        };
        route.insert(HubRoute {
            body_limit,
            handler,
            generation,
            access: None,
        });
        Ok(())
    }

    fn mount_generic(&mut self, policy: PathBuf) -> io::Result<()> {
        let policy = Arc::new(policy);
        for &(path, limit) in GENERIC_ROUTES {
            self.mount(path, limit, RouteHandler::Generic(policy.clone()))?;
        }
        Ok(())
    }

    fn get(&self, path: &str) -> Result<&HubRoute, Fault> {
        self.0.get(path).ok_or((404, "route_not_found"))
    }
}

struct HubState {
    db: PathBuf,
    routes: HubRoutes,
    deadline: Option<Instant>,
}

fn dispatch_generic_hub(
    state: &HubState,
    policy: &std::path::Path,
    input: &Input,
    body: &[u8],
    authority: &str,
    limits: (usize, Instant),
) -> Result<WireResponse, Fault> {
    let principal = auth::authenticate_private(policy, &input.authorization)?;
    if input.path == "/messages/dlq" {
        principal.operation(Operation::InspectDlq)?;
        let request: DlqBody = decode(body)?;
        let result = match (request.id.as_deref(), request.continuation.as_deref()) {
            (Some(_), Some(_)) => return Err((400, "invalid_json_or_fields")),
            (Some(id), None) => {
                Queue::inspect_mailbox_dead_letter(&state.db, principal.mailboxes(), id).map(
                    |entry| match entry {
                        Some(entry) => json!(entry),
                        None => json!(DeadLetterInspection::NotDeadLetteredOrMissing),
                    },
                )
            }
            (None, continuation) => {
                Queue::list_mailbox_dead_letters(&state.db, principal.mailboxes(), continuation)
                    .map(|page| json!(page))
            }
        };
        return result
            .map(|value| (Ok(Some(value)), 200).into())
            .map_err(|error| match error {
                Error::Invalid(
                    "names, identifiers and keys must not be blank" | "message ID too long",
                ) => (400, "invalid_input"),
                Error::Invalid("invalid_continuation") => (400, "invalid_continuation"),
                _ => (503, "storage_unavailable"),
            });
    }
    // Generic policy snapshots retain their existing in-flight revocation semantics.
    // They never authorize a Muse deferred operation or a different profile.
    auth::require_private_file(&state.db)?;
    let mut queue = Queue::open_existing(&state.db).map_err(mapped)?;
    if state.deadline.is_some_and(|end| Instant::now() >= end) {
        return Err((499, "request_cancelled"));
    }
    if input.path == "/mcp" {
        check_streamable_mailbox_body(body)?;
        let request = mcp_sdk::streamable_request(&input.path, &input.sdk_headers, body)?;
        return mcp_sdk::dispatch_generic_streamable(
            mcp_tools(),
            request,
            authority,
            limits,
            |name, args| call_mcp_tool(&mut queue, &principal, name, args),
        )
        .map(WireResponse::Sdk);
    }
    dispatch(&mut queue, &input.path, body, &principal).map(|value| (Ok(Some(value)), 200).into())
}
fn dispatch_receiver(
    state: &HubState,
    profile: &ReceiverProfile,
    input: &Input,
    body: &[u8],
    context: &radio999k::mcp_request::Context<'_>,
) -> Result<Value, Fault> {
    use radio999k::receiver_api::{Action, Request};
    let principal = auth::authenticate_private(&profile.policy, &input.authorization)?;
    let request: Request = decode(body)?;
    let operation = if matches!(&request.action, Action::Complete { .. }) {
        Operation::Reply
    } else {
        Operation::Receive
    };
    let denied = std::cell::Cell::new(None);
    let recipient = request.recipient.clone();
    let authorize = || {
        let checked = (|| {
            if context.cancelled() {
                return Err((499, "request_cancelled"));
            }
            let latest = auth::authenticate_private(&profile.policy, &input.authorization)?;
            if latest.id != principal.id {
                return Err((403, "forbidden"));
            }
            latest.operation(operation)?;
            latest.mailbox(&recipient)?;
            profile.check_queue(&state.db)
        })();
        checked.map_err(|fault| {
            denied.set(Some(fault));
            Error::Invalid("receiver_authorization_failed")
        })
    };
    authorize().map_err(|_| denied.get().unwrap_or((403, "forbidden")))?;
    radio999k::receiver_api::execute(
        &state.db,
        &profile.registration,
        &principal.id,
        request,
        authorize,
    )
    .map_err(|error| {
        denied.get().unwrap_or_else(|| match error {
            Error::Invalid(
                reason @ ("sender_revoked_or_mailbox_mismatch"
                | "recipient_revoked_or_mailbox_mismatch"
                | "sender_not_registered"
                | "recipient_not_registered"
                | "runtime_owner_mismatch"),
            ) => (403, reason),
            Error::Invalid("auth_unavailable") => (503, "auth_unavailable"),
            error => mapped(error),
        })
    })
}
fn dispatch_hub_request(
    state: &HubState,
    authority: &str,
    input: &Input,
    body: &[u8],
    peer_closed: &(dyn Fn() -> bool + Sync),
    hosted: Option<uuid::Uuid>,
) -> transport::WireResponse {
    let route = match state.routes.get(&input.path) {
        Ok(route) => route,
        Err(error) => return (Err(error), 200).into(),
    };
    // Only the managed hosted Unix listener provides an installation ID.
    // The shared guard also fences switching opt-in mode and rotating the grant.
    let mut hosted_guard = None;
    let mut scoped_cue = None;
    if let Some(installation) = hosted.filter(|_| input.path == "/cue/mcp") {
        let Some(home) = state.db.parent() else {
            return (Err((401, "unauthorized")), 200).into();
        };
        let scope = crate::hosted_mailbox_bearer::Scope::new(home, installation);
        let Ok(guard) = scope.lock(false) else {
            return (Err((401, "unauthorized")), 200).into();
        };
        hosted_guard = Some(guard);
        match crate::lifecycle::hosted_cue_mode(home, installation) {
            Ok(true) => scoped_cue = Some(scope),
            Ok(false) => {}
            Err(_) => return (Err((401, "unauthorized")), 200).into(),
        }
    }
    let _hosted_guard = hosted_guard;
    let projected;
    let authorization = match &route.access {
        Some(access) if !input.access_assertions.is_empty() => {
            projected = match access.authorization(&input.access_assertions) {
                Ok(header) => [header],
                Err(fault) => return (Err(fault), 200).into(),
            };
            &projected[..]
        }
        _ => &input.authorization,
    };
    let now = Instant::now();
    // Dot and Codex Events apply the request deadline inside their wire handlers.
    let handles_deadline = match &route.handler {
        RouteHandler::Dot(_) => true,
        RouteHandler::Mailbox(profile, _) => profile
            .definition()
            .is_ok_and(|definition| definition.events == catalog::EventsCapability::Codex),
        _ => false,
    };
    if !handles_deadline && state.deadline.is_some_and(|end| now >= end) {
        return (Err((499, "request_cancelled")), 200).into();
    }
    let deadline = state
        .deadline
        .unwrap_or(now + Duration::from_secs(15))
        .min(now + Duration::from_secs(15));
    let context = radio999k::mcp_request::Context::new(deadline, peer_closed);
    // Retire and activate take the exclusive side of this lock. The current
    // binding remains stable for the request, including deferred waits.
    let _generation = match &route.generation {
        Some(generation) => match generation.enter(&state.db) {
            Ok(lock) => lock,
            Err(fault) => return (Err(fault), 200).into(),
        },
        None => None,
    };
    let result = match &route.handler {
        RouteHandler::Disabled => Err((404, "profile_disabled")),
        RouteHandler::Generic(policy) => {
            return dispatch_generic_hub(
                state,
                policy,
                input,
                body,
                authority,
                (route.body_limit, deadline),
            )
            .unwrap_or_else(|fault| (Err(fault), 200).into());
        }
        RouteHandler::Receiver(receiver) => {
            dispatch_receiver(state, receiver, input, body, &context).map(Some)
        }
        RouteHandler::Muse(muse) => {
            return match dispatch_muse(muse, authority, input, body, (route.body_limit, deadline)) {
                Ok(response) => transport::WireResponse::Sdk(response),
                Err(error) => (Err(error), 200).into(),
            };
        }
        RouteHandler::Grok(grok) => {
            return match grok_bot::dispatch_grok(grok, authority, input, body, route.body_limit) {
                Ok(response) => transport::WireResponse::Sdk(response),
                Err(error) => (Err(error), 200).into(),
            };
        }
        RouteHandler::Dot(host) => {
            return match dispatch_dot(host, authority, input, authorization, body, &context) {
                Ok(response) => transport::WireResponse::Sdk(response),
                Err(error) => (Err(error), 200).into(),
            };
        }
        RouteHandler::Mailbox(profile, events) => {
            let scoped_profile;
            let (profile, events) = if let Some(scope) = scoped_cue {
                scoped_profile = match profile.hosted_cue(scope) {
                    Ok(value) => value,
                    Err(fault) => return (Err(fault), 200).into(),
                };
                (&scoped_profile, None)
            } else {
                (profile, events.as_deref())
            };
            return match profile.dispatch_streamable_http(
                (authority, &input.path),
                &input.sdk_headers,
                authorization,
                body,
                events,
                (route.body_limit, &context),
            ) {
                Ok(response) => transport::WireResponse::Sdk(response),
                Err(error) => (Err(error), 200).into(),
            };
        }
    };
    (result, 200).into()
}

fn dispatch_dot(
    host: &radio999k::dot_profile::DotHost,
    authority: &str,
    input: &Input,
    authorization: &[Vec<u8>],
    body: &[u8],
    context: &radio999k::mcp_request::Context<'_>,
) -> Result<http::Response<Vec<u8>>, Fault> {
    let bearer = match authorization {
        [value] => std::str::from_utf8(value)
            .ok()
            .and_then(|v| v.strip_prefix("Bearer ")),
        _ => None,
    }
    .ok_or((401, "unauthorized"))?;
    host.dispatch_streamable_http(
        (authority, &input.path),
        &input.sdk_headers,
        bearer,
        body,
        context,
    )
}

fn handle_hub_connection(stream: impl Into<ProfileStream>, authority: &str, state: &HubState) {
    handle_hub_routes(stream.into(), authority, state, None, None, false)
}

fn handle_hub_routes(
    stream: ProfileStream,
    authority: &str,
    state: &HubState,
    allowed: Option<&[String]>,
    hosted: Option<uuid::Uuid>,
    legacy_tool_listing: bool,
) {
    transport::handle_connection(
        stream,
        authority,
        |path| {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|route| route == path)) {
                return Err((404, "route_not_found"));
            }
            state.routes.get(path).map(|route| route.body_limit)
        },
        |input, body, peer_closed| {
            let mut response =
                dispatch_hub_request(state, authority, input, body, peer_closed, hosted);
            if legacy_tool_listing
                && let transport::WireResponse::Sdk(sdk) = &mut response
                && sdk.status().is_success()
                && let Ok(mut value) = serde_json::from_slice(sdk.body())
                && radio999k::mailbox_tools::translate_tool_list(
                    &mut value,
                    radio999k::mailbox_tools::legacy_tool_name,
                )
                && let Ok(encoded) = serde_json::to_vec(&value)
            {
                *sdk.body_mut() = encoded;
            }
            response
        },
    );
}

enum HubMode {
    Explicit,
    Managed,
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep the existing hub entry point and explicit socket configuration"
)]
pub fn serve_hub(
    db: PathBuf,
    generic_policy: Option<PathBuf>,
    bindings: Vec<Binding>,
    local_profiles: Vec<crate::codex_mcp::Profile>,
    manifest_fingerprint: Option<&str>,
    port: u16,
    seconds: Option<u64>,
    local_stdio: bool,
) -> io::Result<()> {
    serve_hub_on(
        db,
        generic_policy,
        bindings,
        local_profiles,
        manifest_fingerprint,
        (port, seconds, HubMode::Explicit),
        local_stdio,
        None,
        None,
    )
}

/// The managed hub keeps its loopback listener after the last profile is removed.
pub(crate) fn serve_managed_hub(
    db: PathBuf,
    bindings: Vec<Binding>,
    port: u16,
    local_stdio: bool,
) -> io::Result<()> {
    serve_hub_on(
        db,
        None,
        bindings,
        Vec::new(),
        Some("managed"),
        (port, None, HubMode::Managed),
        local_stdio,
        None,
        None,
    )
}

#[cfg(any(target_os = "macos", test))]
// NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
pub(crate) fn serve_dot_on(
    listener: TcpListener,
    db: PathBuf,
    dot_policy: PathBuf,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        return Err(io::Error::other("Cloud Dot listener must use loopback"));
    }
    let home = db
        .parent()
        .ok_or_else(|| io::Error::other("queue parent required"))?;
    let dot = Binding::bundled(catalog::by_adapter(Adapter::Dot), dot_policy, home);
    serve_hub_on(
        db,
        None,
        vec![dot],
        Vec::new(),
        None,
        (addr.port(), None, HubMode::Explicit),
        false,
        Some(listener),
        None,
    )
}

pub(crate) fn hub_queue_lock_path(db: &Path) -> PathBuf {
    let mut lock = db.as_os_str().to_owned();
    lock.push(".hub.lock");
    PathBuf::from(lock)
}

// Keep the sibling file. Unlinking a live lock lets another hub lock a new file.
pub(crate) fn lock_hub_queue(db: &Path) -> io::Result<(File, PathBuf)> {
    use radio999k::private_file::open_private_file;
    let private_db = |path: &Path| {
        open_private_file(path).map_err(|_| io::Error::other("private database required"))
    };
    let before = private_db(db)?.metadata()?;
    let db = db.canonicalize()?;
    let lock = hub_queue_lock_path(&db);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(io::Error::other("hub_queue_lock_unavailable")),
    }
    let owner =
        open_private_file(&lock).map_err(|_| io::Error::other("hub_queue_lock_unavailable"))?;
    // SAFETY: The descriptor holds the exclusive lock until the last owner drops it.
    if unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(error.kind(), "hub_queue_already_owned")
        } else {
            io::Error::other("hub_queue_lock_unavailable")
        });
    }
    let current = private_db(&db)?.metadata()?;
    if (current.dev(), current.ino()) != (before.dev(), before.ino()) {
        return Err(io::Error::other("queue_identity_changed"));
    }
    Ok((owner, db))
}

/// Hosted transport uses the existing managed hub, queue lock, and route state.
struct HostedSocket<'a> {
    installation: uuid::Uuid,
    scope: &'a crate::hosted_registration::OriginDescriptor,
    #[cfg(test)]
    fixture_root: Option<&'a Path>,
}

impl HostedSocket<'_> {
    fn bind(&self) -> io::Result<crate::hosted_origin_local::OriginListener> {
        let origin = self.scope.origin().map_err(io::Error::other)?;
        #[cfg(test)]
        if let Some(root) = self.fixture_root {
            return crate::hosted_origin_local::bind_in(root, &origin, self.installation);
        }
        crate::hosted_origin_local::bind_system(&origin, self.installation)
    }
}

#[allow(dead_code)] // No exposed activation path until isolated launchd acceptance.
pub(crate) fn serve_managed_hosted_hub(
    db: PathBuf,
    bindings: Vec<Binding>,
    port: u16,
    installation: uuid::Uuid,
    scope: &crate::hosted_registration::OriginDescriptor,
    local_stdio: bool,
) -> io::Result<()> {
    serve_hub_on(
        db,
        None,
        bindings,
        Vec::new(),
        Some("managed"),
        (port, None, HubMode::Managed),
        local_stdio,
        None,
        Some(HostedSocket {
            installation,
            scope,
            #[cfg(test)]
            fixture_root: None,
        }),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "Both startup modes pass their explicit configuration to this shared listener"
)]
fn serve_hub_on(
    db: PathBuf,
    generic_policy: Option<PathBuf>,
    bindings: Vec<Binding>,
    local_profiles: Vec<crate::codex_mcp::Profile>,
    manifest_fingerprint: Option<&str>,
    (port, seconds, mode): (u16, Option<u64>, HubMode),
    local_stdio: bool,
    prebound: Option<TcpListener>,
    hosted: Option<HostedSocket<'_>>,
) -> io::Result<()> {
    let managed = matches!(mode, HubMode::Managed);
    if port == 0
        || seconds.is_some_and(|value| !(1..=3600).contains(&value))
        || (!managed
            && generic_policy.is_none()
            && bindings.is_empty()
            && local_profiles.is_empty())
    {
        return Err(io::Error::other(
            "explicit policy, port and lifetime limit required",
        ));
    }
    let local_mailbox_enabled = !local_profiles.is_empty();
    if local_mailbox_enabled
        && (seconds.is_none()
            || manifest_fingerprint.is_some()
            || generic_policy.is_some()
            || !bindings.is_empty()
            || prebound.is_some())
    {
        return Err(io::Error::other(
            "local mailboxes require a separate bounded loopback hub",
        ));
    }
    if manifest_fingerprint.is_some() && generic_policy.is_some() {
        return Err(io::Error::other(
            "a profile manifest cannot mount the generic API",
        ));
    }
    let (queue_owner, db) = lock_hub_queue(&db)?;
    let queue_owner = Arc::new(queue_owner);
    let prepared = muse_mailbox::is_prepared(&db)
        .map_err(|_| io::Error::other("existing queue unavailable"))?;
    let deadline = seconds.map(|value| Instant::now() + Duration::from_secs(value));
    let generic_enabled = generic_policy.is_some();
    let mut routes = HubRoutes::default();
    if let Some(policy) = generic_policy {
        auth::validate_private(&policy).map_err(|_| io::Error::other("invalid generic policy"))?;
        routes.mount_generic(policy)?;
    }
    let mut enabled = serde_json::Map::new();
    for descriptor in catalog::http_profiles() {
        let mounted = bindings
            .iter()
            .any(|binding| binding.descriptor == descriptor);
        enabled.insert(descriptor.enabled_key.into(), json!(mounted));
        if !mounted && !managed && manifest_fingerprint.is_none() && !local_mailbox_enabled {
            routes.mount(
                descriptor.route,
                descriptor.body_limit(MAX_HEADERS),
                RouteHandler::Disabled,
            )?;
        }
    }
    let mut dot_events = None;
    let mut codex_events = None;
    let mut grok_wake_binding = None;
    for binding in bindings {
        let descriptor = binding.descriptor;
        let generation = BoundGeneration::load(&db, &binding)?;
        let handler = match descriptor.adapter {
            Adapter::Muse => {
                auth::validate_muse(&binding.policy).map_err(|(_, reason)| {
                    auth::local_policy_error(reason, "invalid Muse policy")
                })?;
                if !prepared {
                    return Err(io::Error::other("Muse storage unprepared"));
                }
                RouteHandler::Muse(Arc::new(MuseState {
                    db: db.clone(),
                    policy: binding.policy,
                    deadline,
                    waiting: AtomicBool::new(false),
                    active: Mutex::new(HashMap::new()),
                }))
            }
            Adapter::Dot => {
                dot_events = radio999k::dot_profile::check_with_generation(
                    &db,
                    &binding.policy,
                    false,
                    binding.generation.as_deref(),
                )?
                .map(Arc::new);
                RouteHandler::Dot(radio999k::dot_profile::DotBackend::host_with_events(
                    db.clone(),
                    binding.policy,
                    dot_events.clone(),
                ))
            }
            Adapter::Grok => {
                if !check_grok(&db, &binding.policy)? {
                    return Err(io::Error::other("Grok storage unprepared"));
                }
                grok_wake_binding = Some((
                    binding.policy.clone(),
                    binding.wake_files_dir,
                    generation.clone(),
                ));
                RouteHandler::Grok(grok_bot::GrokState::new(
                    db.clone(),
                    binding.policy,
                    deadline,
                ))
            }
            Adapter::SharedMailbox => {
                let profile = binding
                    .mailbox(&db)
                    .ok_or_else(|| io::Error::other("missing mailbox binding"))?;
                profile
                    .validate()
                    .map_err(|(_, reason)| io::Error::other(reason))?;
                let events = if descriptor
                    .mailbox
                    .as_ref()
                    .is_some_and(|profile| profile.events == catalog::EventsCapability::Codex)
                {
                    let events = crate::codex_events::Runtime::new_with_generation(
                        &profile,
                        binding.generation.as_deref(),
                    )
                    .map_err(|(_, reason)| io::Error::other(reason))?
                    .map(Arc::new);
                    if let Some(events) = &events {
                        events
                            .check()
                            .map_err(|error| io::Error::other(error.reason))?;
                    }
                    codex_events = events.clone();
                    events
                } else {
                    None
                };
                RouteHandler::Mailbox(profile, events)
            }
            Adapter::Receiver => {
                let registration = binding
                    .registration
                    .ok_or_else(|| io::Error::other("missing receiver registration"))?;
                let receiver = ReceiverProfile::new(&db, binding.policy, registration)
                    .map_err(|(_, reason)| io::Error::other(reason))?;
                RouteHandler::Receiver(receiver)
            }
        };
        routes.mount_bound(
            &binding.route,
            descriptor.body_limit(MAX_HEADERS),
            handler,
            generation,
        )?;
    }
    for profile in local_profiles {
        let path = profile
            .route()
            .map_err(|(_, reason)| io::Error::other(reason))?
            .to_owned();
        routes.mount(
            &path,
            MAX_MUSE_ENVELOPE,
            RouteHandler::Mailbox(profile, None),
        )?;
    }
    if !local_mailbox_enabled && prebound.is_none() {
        access::mount(&db, &mut routes)?;
    }
    let hosted_listener = if let Some(socket) = &hosted {
        if !managed
            || socket.scope.hub_port != port
            || socket
                .scope
                .mcp_paths
                .iter()
                .any(|path| routes.get(path).is_err())
        {
            return Err(io::Error::other(
                "hosted Unix origin does not match the managed hub",
            ));
        }
        // The existing queue lock has already been acquired. Hold socket custody
        // until this hub and its accepted connection workers have stopped.
        match socket.bind() {
            Ok(listener) => Some(listener),
            Err(_) => {
                tracing::warn!("hosted Unix origin unavailable; local hub stays active");
                None
            }
        }
    } else {
        None
    };
    let stdio_routes: Vec<String> = routes
        .0
        .iter()
        .filter(|(path, route)| {
            crate::stdio_mcp::named_route(path) && !matches!(route.handler, RouteHandler::Disabled)
        })
        .map(|(path, _)| path.clone())
        .collect();
    let stdio_listener = if local_stdio {
        let home = db
            .parent()
            .ok_or_else(|| io::Error::other("queue parent required"))?;
        match crate::stdio_mcp::bind(home, &stdio_routes) {
            Ok(listener) => Some(listener),
            Err(_) if managed => {
                tracing::warn!(
                    "managed stdio socket unavailable; other hub transports stay active"
                );
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let listener = match prebound {
        Some(listener) => listener,
        None => TcpListener::bind((Ipv4Addr::LOCALHOST, port))?,
    };
    // Best-effort wake on its own thread. It never blocks a commit.
    let grok_wake = if let Some((policy, files_dir, generation)) = grok_wake_binding {
        use radio999k::grok_wake::WakeConfig;
        let config = match files_dir {
            Some(dir) => Some(WakeConfig::from_files_dir(&db, &dir)),
            None => WakeConfig::discover_with_keychain(
                &db,
                crate::cloud_credentials::read_grok_wake_keychain,
            ),
        };
        let queue = db.clone();
        config
            .map(|config| {
                let owner = queue_owner.clone();
                radio999k::grok_wake::spawn(config, deadline, move |id| {
                    // The wake thread can outlive the listener.
                    let _keep_queue_owner = &owner;
                    generation.as_ref()?.check(&queue).ok()?;
                    let scope = grok_bot::observation_scope(&policy).ok()?;
                    let (sender, conversation_id) =
                        radio999k::grok_mailbox::wake_candidate(&queue, id, &scope).ok()??;
                    let latest = grok_bot::observation_scope(&policy).ok()?;
                    latest
                        .allowed_senders
                        .contains(&sender)
                        .then_some(conversation_id)
                })
            })
            .transpose()?
            .is_some()
    } else {
        false
    };
    let state = Arc::new(HubState {
        db,
        routes,
        deadline,
    });
    let public_state = state.clone();
    let mut listeners: Vec<(ProfileListener, Arc<ProfileHandler>)> = vec![(
        ProfileListener::Tcp(listener),
        Arc::new(move |stream, authority| {
            handle_hub_connection(stream, authority, &public_state);
        }),
    )];
    if let (Some(holder), Some(socket)) = (&hosted_listener, &hosted) {
        let allowed = socket.scope.mcp_paths.clone();
        let installation = socket.installation;
        let hosted_state = state.clone();
        listeners.push((
            ProfileListener::Unix(holder.listener.try_clone()?, format!("127.0.0.1:{port}")),
            Arc::new(move |stream, authority| {
                handle_hub_routes(
                    stream,
                    authority,
                    &hosted_state,
                    Some(&allowed),
                    Some(installation),
                    false,
                );
            }),
        ));
    }
    if let Some(holder) = &stdio_listener {
        let stdio_state = state.clone();
        listeners.push((
            ProfileListener::Unix(
                holder.listener.try_clone()?,
                crate::stdio_mcp::AUTHORITY.into(),
            ),
            Arc::new(move |stream, authority| {
                handle_hub_routes(
                    stream,
                    authority,
                    &stdio_state,
                    Some(&stdio_routes),
                    None,
                    true,
                );
            }),
        ));
    }
    let dot_events_enabled = dot_events.is_some();
    let _dot_events_worker = dot_events
        .map(|events| events.start(deadline))
        .transpose()
        .map_err(|e| io::Error::other(e.reason))?;
    let codex_events_enabled = codex_events.is_some();
    let _events_worker = codex_events
        .map(|events| events.start(deadline))
        .transpose()
        .map_err(|error| io::Error::other(error.reason))?;
    let mut ready = json!({
        "transport":"hub-local-http",
        "url":format!("http://127.0.0.1:{port}"),
        "generic_enabled":generic_enabled,
        "local_mailbox_enabled":local_mailbox_enabled,
        "local_mailbox_port":local_mailbox_enabled.then_some(port),
        "dot_events_enabled":dot_events_enabled,
        "codex_events_enabled":codex_events_enabled,
        "grok_wake_enabled":grok_wake,
        "runtime_verified":false,"remote_verified":false,
    });
    ready
        .as_object_mut()
        .expect("readiness object")
        .extend(enabled);
    if local_stdio {
        ready["local_stdio_socket"] = json!(stdio_listener.is_some());
    }
    if let Some(fingerprint) = manifest_fingerprint {
        ready["profiles_fingerprint"] = json!(fingerprint);
    }
    println!("{ready}");
    io::stdout().flush()?;
    serve_profile_listeners(listeners, deadline, MAX_HTTP_CONNECTION_WORKERS)
}

#[cfg(test)]
mod dot_wire {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn private_json(path: &Path, value: Value) {
        fs::write(path, value.to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    const TOKEN: &str = "synthetic-dot-wire-only";

    fn fixture() -> (tempfile::TempDir, Queue, Arc<HubState>) {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = temp.path().join("queue.sqlite3");
        let queue = Queue::open(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let registration = temp.path().join("pigeonmq-cloud-dot-registration.json");
        private_json(
            &registration,
            json!({
                "dot":{"mailbox":"dot","revoked":false},
                "test-peer":{"mailbox":"test-peer","revoked":false}
            }),
        );
        let policy = temp.path().join("pigeonmq-cloud-dot-policy.json");
        let digest: String = Sha256::digest(TOKEN)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        private_json(
            &policy,
            json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot","enabled":true,
            "registration":registration,"token_sha256":digest}),
        );
        radio999k::dot_profile::check(&db, &policy, true).unwrap();
        let mut routes = HubRoutes::default();
        let descriptor = catalog::by_adapter(catalog::Adapter::Dot);
        routes
            .mount(
                descriptor.route,
                descriptor.body_limit(MAX_HEADERS),
                RouteHandler::Dot(radio999k::dot_profile::DotBackend::host(db.clone(), policy)),
            )
            .unwrap();
        let state = Arc::new(HubState {
            db: db.clone(),
            routes,
            deadline: None,
        });
        (temp, queue, state)
    }

    fn post(
        state: &Arc<HubState>,
        token: &str,
        id: Value,
        name: &str,
        args: Value,
    ) -> (u16, Value) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = state.clone();
        let worker = std::thread::spawn(move || {
            handle_hub_connection(listener.accept().unwrap().0, &addr.to_string(), &state);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let body = json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":args}})
        .to_string();
        write!(client, "POST /dot/mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {token}\r\nMcp-Method: tools/call\r\nMcp-Name: {name}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    fn success((status, response): (u16, Value)) -> Value {
        assert_eq!(status, 200, "{response}");
        assert!(response.get("error").is_none(), "{response}");
        let result = &response["result"];
        if let Some(value) = result.get("structuredContent") {
            value.clone()
        } else {
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
        }
    }

    #[test]
    fn hosted_cue_bearer_rotation_isolated_from_local_stdio() {
        use std::os::unix::net::UnixStream;
        let home = tempfile::Builder::new()
            .prefix("hc")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = home.path().join("queue.sqlite3");
        drop(Queue::open(&db).unwrap());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let local = "test-local-cue";
        let policy = home.path().join("pigeonmq-cloud-cue-policy.json");
        let registration = home.path().join("pigeonmq-cloud-registration.json");
        let digest: String = Sha256::digest(local)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        private_json(
            &policy,
            json!({"principals":[{
                "id":"cue","token_sha256":digest,"operations":["read","reply"],
                "send_to":[],"mailboxes":["cue"],"revoked":false
            }]}),
        );
        private_json(
            &registration,
            json!({"cue":{"mailbox":"cue","revoked":false}}),
        );
        let root = tempfile::Builder::new()
            .prefix("hp")
            .tempdir_in("/tmp")
            .unwrap();
        let origin = crate::hosted_origin_local::new_origin().unwrap();
        let socket = root.path().join(origin.directory_name()).join("hub.sock");
        let stdio = crate::stdio_mcp::socket_path(home.path());
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let installation = uuid::Uuid::new_v4();
        let life = home.path().join(".pigeonmq-lifecycle");
        fs::create_dir(&life).unwrap();
        fs::set_permissions(&life, fs::Permissions::from_mode(0o700)).unwrap();
        let state = life.join("profiles.json");
        let baseline =
            json!({"port":port,"registration":registration,"relay_command":null,"profiles":{}});
        private_json(&state, baseline.clone());
        let descriptor: crate::hosted_registration::OriginDescriptor =
            serde_json::from_value(json!({
                "kind":"unix_v1","nonce":origin.nonce(),"hub_port":port,"mcp_paths":["/cue/mcp"],
            }))
            .unwrap();
        let cue = Binding::bundled(
            catalog::http_by_id("cue").unwrap(),
            policy.clone(),
            home.path(),
        );
        let root_path = root.path().to_owned();
        let worker = std::thread::spawn(move || {
            serve_hub_on(
                db,
                None,
                vec![cue],
                Vec::new(),
                Some("managed"),
                (port, Some(4), HubMode::Managed),
                true,
                None,
                Some(HostedSocket {
                    installation,
                    scope: &descriptor,
                    fixture_root: Some(&root_path),
                }),
            )
        });
        let ready = Instant::now() + Duration::from_secs(2);
        while !(socket.exists() && stdio.exists()) && Instant::now() < ready {
            std::thread::sleep(Duration::from_millis(10));
        }
        let post = |socket: &Path, host: &str, route: &str, bearer: &str| -> u16 {
            let mut client = UnixStream::connect(socket).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
            write!(client,"POST {route} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {bearer}\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            response.split_whitespace().nth(1).unwrap().parse().unwrap()
        };
        let authority = format!("127.0.0.1:{port}");
        let hosted = |token| post(&socket, &authority, "/cue/mcp", token);
        let local_post = |token| post(&stdio, crate::stdio_mcp::AUTHORITY, "/cue/mcp", token);
        assert_eq!(hosted(local), 200); // Legacy installations have not enabled C22.
        assert_eq!(local_post(local), 200);
        let scope = crate::hosted_mailbox_bearer::Scope::new(home.path(), installation);
        let transfer = tempfile::tempdir().unwrap();
        fs::set_permissions(transfer.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let first = transfer.path().join("remote-first");
        let second = transfer.path().join("remote-second");
        {
            let _write = scope.lock(true).unwrap();
            scope
                .rotate_locked(60, &first, || {
                    let mut state_data = baseline.clone();
                    state_data["finite_cue_installation"] = json!(installation.to_string());
                    private_json(&state, state_data);
                    Ok(())
                })
                .unwrap();
        }
        let older = fs::read_to_string(&first).unwrap();
        assert_eq!(hosted(local), 401);
        assert_eq!(hosted(&older), 200);
        assert_eq!(local_post(&older), 401);
        assert_eq!(post(&socket, &authority, "/instinct/mcp", &older), 404);
        {
            let _write = scope.lock(true).unwrap();
            scope.rotate_locked(60, &second, || Ok(())).unwrap();
        }
        let newer = fs::read_to_string(&second).unwrap();
        assert_eq!(hosted(&older), 401);
        assert_eq!(hosted(&newer), 200);
        {
            let _write = scope.lock(true).unwrap();
            scope.revoke_locked().unwrap();
        }
        assert_eq!(hosted(&newer), 401);
        assert_eq!(hosted(local), 401);
        fs::remove_file(home.path().join("hosted-cue-bearer.json")).unwrap();
        assert_eq!(hosted(local), 401);
        assert_eq!(local_post(local), 200);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn hosted_unix_uses_same_queue_and_auth_without_targeting_tcp() {
        use std::os::unix::net::UnixStream;
        let (home, _, state) = fixture();
        let root = tempfile::Builder::new()
            .prefix("po")
            .tempdir_in("/tmp")
            .unwrap();
        let root_path = root.path().to_owned();
        let origin = crate::hosted_origin_local::new_origin().unwrap();
        let socket = root_path.join(origin.directory_name()).join("hub.sock");
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let installation = uuid::Uuid::new_v4();
        let scope: crate::hosted_registration::OriginDescriptor = serde_json::from_value(json!({
            "kind":"unix_v1", "nonce":origin.nonce(), "hub_port":port, "mcp_paths":["/dot/mcp"],
        }))
        .unwrap();
        let dot = Binding::bundled(
            catalog::by_adapter(catalog::Adapter::Dot),
            home.path().join("pigeonmq-cloud-dot-policy.json"),
            home.path(),
        );
        let database = state.db.clone();
        let worker = std::thread::spawn(move || {
            serve_hub_on(
                database,
                None,
                vec![dot],
                Vec::new(),
                Some("managed"),
                (port, Some(2), HubMode::Managed),
                false,
                None,
                Some(HostedSocket {
                    installation,
                    scope: &scope,
                    fixture_root: Some(&root_path),
                }),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let authority = format!("127.0.0.1:{port}");
        let post = |route: &str, host: &str, bearer: &str| {
            let mut client = UnixStream::connect(&socket).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"radio999k_recipients","arguments":{}}}"#;
            write!(
                client,
                "POST {route} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {bearer}\r\nMcp-Method: tools/call\r\nMcp-Name: radio999k_recipients\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            let (header, body) = response.split_once("\r\n\r\n").unwrap();
            (
                header
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u16>()
                    .unwrap(),
                serde_json::from_str::<Value>(body).unwrap(),
            )
        };
        assert_eq!(
            post("/dot/mcp", &authority, "fixture-connector-token").0,
            401
        );
        assert_eq!(post("/dot/mcp", "wrong.example.test", TOKEN).0, 403);
        assert_eq!(post("/instinct/mcp", &authority, TOKEN).0, 404);
        let (status, response) = post("/dot/mcp", &authority, TOKEN);
        assert_eq!(status, 200);
        assert!(response.get("result").is_some());
        assert!(lock_hub_queue(&state.db).is_err());
        worker.join().unwrap().unwrap();

        let foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        foreign.set_nonblocking(true).unwrap();
        assert_eq!(
            UnixStream::connect(&socket).err().unwrap().kind(),
            io::ErrorKind::ConnectionRefused
        );
        assert_eq!(
            foreign.accept().err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cloudflare_access_projects_only_the_configured_dot_mailbox() {
        use super::cloudflare_access::tests::{signed, verifier_with_claims};
        let (temp, _, mut state) = fixture();
        let bearer_file = temp.path().join("dot-mailbox.key");
        radio999k::private_file::replace_private(
            &bearer_file,
            format!("Bearer {TOKEN}\n").as_bytes(),
        )
        .unwrap();
        let (verifier, mut claims) = verifier_with_claims();
        Arc::get_mut(&mut state)
            .unwrap()
            .routes
            .0
            .get_mut("/dot/mcp")
            .unwrap()
            .access = Some(access::MailboxAccess {
            verifier,
            bearer_file,
        });
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"radio999k_recipients","arguments":{}}}"#;
        let authority = "127.0.0.1:1234";
        let call = |token: &str, assertions: &[Vec<u8>]| {
            let mut head = format!(
                "POST /dot/mcp HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n",
                body.len()
            );
            for assertion in assertions {
                head.push_str(&format!(
                    "Cf-Access-Jwt-Assertion: {}\r\n",
                    std::str::from_utf8(assertion).unwrap()
                ));
            }
            head.push_str("\r\n");
            let input = transport::parse_head(head.as_bytes(), authority, |path| {
                state.routes.get(path).map(|route| route.body_limit)
            })
            .unwrap();
            dispatch_hub_request(&state, authority, &input, body, &|| false, None)
        };
        let valid = signed(&claims);
        let WireResponse::Sdk(response) = call("opaque-fixture", std::slice::from_ref(&valid))
        else {
            panic!("expected authorized MCP response");
        };
        assert_eq!(
            success((
                response.status().as_u16(),
                serde_json::from_slice(response.body()).unwrap()
            ))["recipients"],
            json!(["test-peer"])
        );
        assert!(matches!(
            call(TOKEN, &[valid.clone(), valid]),
            WireResponse::Json((Err((403, _)), _))
        ));
        claims["aud"] = json!(["unrelated-mailbox-fixture"]);
        assert!(matches!(
            call(TOKEN, &[signed(&claims)]),
            WireResponse::Json((Err((403, _)), _))
        ));
        assert!(
            matches!(call(TOKEN, &[]), WireResponse::Sdk(response) if response.status() == 200)
        );
    }

    #[test]
    fn dot_can_list_and_address_registered_codex_sessions() {
        let (temp, mut queue, state) = fixture();
        let registration = temp.path().join("pigeonmq-cloud-dot-registration.json");
        private_json(
            &registration,
            json!({
                "dot":{"mailbox":"dot","revoked":false},
                "codex-local1":{"mailbox":"codex-local1","revoked":false},
                "codex-local-abcdef012345":{"mailbox":"codex-local-abcdef012345","revoked":false},
                "codex-local2":{"mailbox":"codex-local2","revoked":true},
                "test-peer":{"mailbox":"test-peer","revoked":false},
            }),
        );
        let call = |tool, args| success(post(&state, TOKEN, json!(1), tool, args));
        let listed = call("radio999k_recipients", json!({}));
        assert_eq!(
            listed["recipients"],
            json!(["codex-local-abcdef012345", "codex-local1", "test-peer"])
        );
        assert_eq!(listed["connection_status"], "unknown");
        call(
            "radio999k_send",
            json!({"recipient":"codex-local1","body":"first","idempotency_key":"session-one"}),
        );
        call(
            "radio999k_send",
            json!({"recipient":"codex-local-abcdef012345","body":"second","idempotency_key":"session-two"}),
        );
        for (recipient, body) in [
            ("codex-local1", "first"),
            ("codex-local-abcdef012345", "second"),
        ] {
            let delivery = queue
                .receive(recipient, Duration::from_secs(30))
                .unwrap()
                .unwrap();
            assert_eq!(delivery.message.envelope.body, body);
        }
    }

    #[test]
    fn dot_native_post_queues_reads_and_completes_atomically() {
        let (_temp, mut queue, state) = fixture();
        let db = &state.db;
        let call = |name: &str, args: Value| success(post(&state, TOKEN, json!(1), name, args));
        let outbound = call(
            "radio999k_send",
            json!({"recipient":"test-peer","body":"hello","idempotency_key":"wire-send"}),
        );
        let outbound_id = outbound["message_id"].as_str().unwrap();
        assert_eq!(
            queue
                .observe_ordinary(outbound_id, "test-peer")
                .unwrap()
                .envelope
                .sender,
            "dot"
        );
        let request = queue
            .send(Envelope {
                sender: "test-peer".into(),
                recipient: "dot".into(),
                body: "request".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message;
        let read = call("radio999k_read", json!({"message_id":request.id}));
        assert_eq!(read["body"], "request");
        assert_eq!(read["state"], "pending");
        let answer = call(
            "radio999k_reply",
            json!({"message_id":request.id,"body":"answer"}),
        );
        let replay = call(
            "radio999k_reply",
            json!({"message_id":request.id,"body":"answer"}),
        );
        assert_eq!(answer["reply_id"], replay["reply_id"]);
        assert_eq!(replay["duplicate"], true);
        let ack: Option<i64> = rusqlite::Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT acknowledged_at_ms FROM messages WHERE id=?1",
                [&request.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ack.is_some());
        let terminal = queue
            .reply_unclaimed_checked(
                "test-peer",
                outbound_id,
                Envelope {
                    sender: "test-peer".into(),
                    recipient: "dot".into(),
                    body: "terminal".into(),
                    in_reply_to: Some(outbound_id.into()),
                    correlation_id: Some(outbound_id.into()),
                    idempotency_key: Some(format!("gw-reply-{outbound_id}")),
                },
                || Ok(()),
            )
            .unwrap();
        let observed = call("radio999k_read", json!({"message_id":terminal.id}));
        assert_eq!(observed["state"], "reply_received");
        let confirmed = call(
            "radio999k_confirm_terminal",
            json!({"message_id":terminal.id}),
        );
        assert_eq!(confirmed["state"], "acknowledged");
        let repeated = call(
            "radio999k_confirm_terminal",
            json!({"message_id":terminal.id}),
        );
        assert_eq!(repeated["duplicate"], true);
        let connection = rusqlite::Connection::open(db).unwrap();
        let (ack, replies): (Option<i64>, i64) = connection.query_row(
            "SELECT acknowledged_at_ms,(SELECT COUNT(*) FROM messages r WHERE r.in_reply_to=m.id) FROM messages m WHERE m.id=?1",
            [&terminal.id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert!(ack.is_some());
        assert_eq!(replies, 0);
    }
    #[test]
    fn dot_storage_faults_are_retryable_and_correlated() {
        let (_temp, mut queue, state) = fixture();
        let call = |id: Value, name: &str, args: Value| post(&state, TOKEN, id, name, args);
        let filtered = |timeout_ms| json!({"timeout_ms":timeout_ms,"in_reply_to":"fixture-parent"});
        let expected = |id: Value, reason| {
            json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":-32603,"message":reason,"data":{"reason":reason,"retryable":true}}
            })
        };

        assert_eq!(
            success(call(json!(2), "radio999k_wait", filtered(0))),
            json!({"state":"timeout","message":null})
        );

        // A held rollback-journal lock makes reads fail without depending on timing.
        let lock = rusqlite::Connection::open(&state.db).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        for (id, arguments) in [
            (json!("zero-budget"), filtered(0)),
            (json!("bounded-wait"), filtered(10)),
            (json!(3), json!({"timeout_ms":0})),
        ] {
            let result = call(id.clone(), "radio999k_wait", arguments);
            assert_eq!(result, (503, expected(id, "storage_busy")));
        }
        assert_eq!(
            post(
                &state,
                "invalid-fixture-token",
                json!(4),
                "radio999k_wait",
                filtered(0)
            ),
            (401, json!({"error":{"code":"unauthorized"}}))
        );
        assert_eq!(
            call(json!("inbox-read"), "radio999k_inbox", json!({})),
            (503, expected(json!("inbox-read"), "storage_unavailable"))
        );
        lock.execute_batch("ROLLBACK").unwrap();

        let message = queue
            .send(Envelope {
                sender: "test-peer".into(),
                recipient: "dot".into(),
                body: "synthetic message".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message;
        lock.execute_batch(
            "CREATE TRIGGER fail_reply_binding BEFORE INSERT ON dot_event_replies
             BEGIN SELECT RAISE(ABORT, 'private fixture diagnostic'); END;",
        )
        .unwrap();
        assert_eq!(
            call(
                json!("reply-write"),
                "radio999k_reply",
                json!({"message_id":message.id,"body":"answer"})
            ),
            (503, expected(json!("reply-write"), "storage_unavailable"))
        );
        let observation = success(call(
            json!(5),
            "radio999k_read",
            json!({"message_id":message.id}),
        ));
        assert_eq!(observation["state"], "pending");
        let (status, missing) = call(json!(6), "radio999k_read", json!({"message_id":"missing"}));
        assert_eq!(status, 200);
        assert_eq!(missing["id"], 6);
        assert_eq!(missing["error"]["data"]["reason"], "message_not_available");
        assert_eq!(
            call(
                json!(7),
                "radio999k_send",
                json!({
                    "recipient":"test-peer, unregistered","body":"fixture","idempotency_key":"test-subset"
                })
            ),
            (400, json!({"error":{"code":"request_failed"}}))
        );
    }
}
