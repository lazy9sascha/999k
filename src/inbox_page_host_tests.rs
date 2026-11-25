//! Synthetic Muse MCP requests through the production SDK handler.
use super::*;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    policy: PathBuf,
    registry: PathBuf,
}
fn private(path: &std::path::Path, value: &Value) {
    std::fs::write(path, value.to_string()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
impl Fixture {
    fn new(mailbox: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        let mut q = Queue::open(&db).unwrap();
        for sender in ["dot", "blocked", "dot"] {
            q.send(Envelope {
                sender: sender.into(),
                recipient: mailbox.into(),
                body: "EXACT_READ_FIXTURE_BODY".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap();
        }
        radio999k::muse_mailbox::prepare(&db).unwrap();
        let registry = dir.path().join("registration.json");
        private(
            &registry,
            &json!({mailbox:{"mailbox":mailbox,"revoked":false},"dot":{"mailbox":"dot","revoked":false}}),
        );
        let policy = dir.path().join("policy.json");
        let hash: String = Sha256::digest(b"public-page-fixture")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        private(
            &policy,
            &json!({"principal":mailbox,"mailbox":mailbox,"token_sha256":hash,"operations":["read","wait","reply"],"enabled":true,"expires_at_ms":u64::MAX,"registration":registry}),
        );
        Self {
            _dir: dir,
            db,
            policy,
            registry,
        }
    }
    fn state(&self) -> MuseState {
        MuseState {
            db: self.db.clone(),
            policy: self.policy.clone(),
            deadline: None,
            waiting: AtomicBool::new(false),
            active: Mutex::new(HashMap::new()),
        }
    }

    fn auth() -> Vec<Vec<u8>> {
        vec![b"Bearer public-page-fixture".to_vec()]
    }

    fn dispatch(&self, body: &[u8]) -> transport::Response {
        match request(&self.state(), &Self::auth(), body, STANDARD_HEADERS) {
            Ok(response) => (
                Ok(serde_json::from_slice(response.body()).ok()),
                response.status().as_u16(),
            ),
            Err(fault) => (Err(fault), fault.0),
        }
    }

    fn call_wire(&self, name: &str, args: Value) -> transport::Response {
        let raw = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        self.dispatch(raw.to_string().as_bytes())
    }

    fn call(&self, name: &str, args: Value) -> Value {
        let (response, status) = self.call_wire(name, args);
        assert_eq!(status, 200);
        let response = response.unwrap().unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        tool_text(&response)
    }

    fn store(&self, sender: &str, recipient: &str, body: &str) -> String {
        Queue::open_existing(&self.db)
            .unwrap()
            .send(Envelope {
                sender: sender.into(),
                recipient: recipient.into(),
                body: body.into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id
    }

    fn tie_timestamps(&self) {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .execute("UPDATE messages SET created_at_ms=1000", [])
            .unwrap();
    }
}

const STANDARD_HEADERS: &str =
    "Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-03-26\r\n";

fn request(
    state: &MuseState,
    authorization: &[Vec<u8>],
    body: &[u8],
    headers: &str,
) -> Result<http::Response<Vec<u8>>, Fault> {
    let authority = "127.0.0.1:1234";
    let head = format!(
        "POST /muse/mcp HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n",
        body.len(),
    );
    let mut input = transport::parse_head(head.as_bytes(), authority, route_body_limit)?;
    input.authorization = authorization.to_vec();
    let deadline = Instant::now() + Duration::from_secs(15);
    dispatch_muse(
        state,
        authority,
        &input,
        body,
        (MAX_MUSE_ENVELOPE, deadline),
    )
}

fn tool_text(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn assert_tool_error((response, status): transport::Response, reason: &str) {
    assert_eq!(status, 200);
    let response = response.unwrap().unwrap();
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(tool_text(&response), json!({"error":reason}));
}

fn assert_protocol_error(
    (response, status): transport::Response,
    expected_status: u16,
    id: Value,
    code: i64,
) {
    assert_eq!(status, expected_status);
    let response = response.unwrap().unwrap();
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], id);
    assert!(response.get("result").is_none(), "{response}");
    assert_eq!(response["error"]["code"], code, "{response}");
}

#[test]
fn server_lists_metadata_tool_and_preserves_exact_read() {
    for mailbox in ["muse"] {
        let f = Fixture::new(mailbox);
        let before = std::fs::read(&f.db).unwrap();
        let (tools, status) =
            f.dispatch(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#);
        assert_eq!(status, 200);
        let tools = tools.unwrap().unwrap();
        let tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "p-999k_inbox")
            .unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        let first = f.call("p-999k_inbox", json!({"limit":1}));
        let next = f.call(
            "p-999k_inbox",
            json!({"continuation":first["next_continuation"]}),
        );
        assert_eq!(first["messages"].as_array().unwrap().len(), 1);
        assert_eq!(next["messages"].as_array().unwrap().len(), 1);
        assert_ne!(first["messages"][0]["id"], next["messages"][0]["id"]);
        for page in [&first, &next] {
            assert!(page["messages"][0].get("body").is_none());
            assert_eq!(page["messages"][0]["sender"], "dot");
            assert_eq!(page["messages"][0]["recipient"], mailbox);
        }
        let read = f.call(
            "p-999k_read",
            json!({"message_id":first["messages"][0]["id"]}),
        );
        assert_eq!(read["body"], "EXACT_READ_FIXTURE_BODY");
        assert_eq!(std::fs::read(&f.db).unwrap(), before);
    }
}
#[test]
fn page_rechecks_read_permission_registry_and_credential() {
    for mailbox in ["muse"] {
        for condition in [
            "sender",
            "mailbox",
            "operation",
            "disabled",
            "expired",
            "token",
        ] {
            // Each denial starts from an independently successful, authorized fixture.
            let f = Fixture::new(mailbox);
            let before = std::fs::read(&f.db).unwrap();
            let first = f.call("p-999k_inbox", json!({"limit":1}));
            assert_eq!(first["messages"].as_array().unwrap().len(), 1);
            let mut policy: Value =
                serde_json::from_slice(&std::fs::read(&f.policy).unwrap()).unwrap();
            let original_policy = policy.clone();
            let registry = json!({mailbox:{"mailbox":mailbox,"revoked":false},"dot":{"mailbox":"dot","revoked":false}});
            let expected = match condition {
                "sender" => {
                    private(
                        &f.registry,
                        &json!({mailbox:{"mailbox":mailbox,"revoked":false},"dot":{"mailbox":"dot","revoked":true}}),
                    );
                    None
                }
                "mailbox" => {
                    private(
                        &f.registry,
                        &json!({mailbox:{"mailbox":mailbox,"revoked":true},"dot":{"mailbox":"dot","revoked":false}}),
                    );
                    Some((403, "forbidden"))
                }
                "operation" => {
                    policy["operations"] = json!(["wait"]);
                    Some((403, "forbidden"))
                }
                "disabled" => {
                    policy["enabled"] = json!(false);
                    Some((401, "unauthorized"))
                }
                "expired" => {
                    policy["expires_at_ms"] = json!(0);
                    Some((401, "unauthorized"))
                }
                "token" => {
                    policy["token_sha256"] = json!("0".repeat(64));
                    Some((401, "unauthorized"))
                }
                _ => unreachable!(),
            };
            private(&f.policy, &policy);
            if let Some((status, reason)) = expected {
                for args in [
                    json!({}),
                    json!({"continuation":first["next_continuation"]}),
                ] {
                    let wire = f.call_wire("p-999k_inbox", args);
                    if status == 401 {
                        assert_eq!(wire.0, Err((status, reason)), "{mailbox}/{condition}");
                    } else {
                        assert_tool_error(wire, reason);
                    }
                }
            } else {
                assert!(
                    f.call("p-999k_inbox", json!({}))["messages"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert_tool_error(
                    f.call_wire(
                        "p-999k_inbox",
                        json!({"continuation":first["next_continuation"]}),
                    ),
                    "invalid_continuation",
                );
            }
            private(&f.policy, &original_policy);
            private(&f.registry, &registry);
            assert_eq!(f.call("p-999k_inbox", json!({"limit":1})), first);
            assert_eq!(std::fs::read(&f.db).unwrap(), before);
        }
    }
}
#[test]
fn mcp_rejects_invalid_page_fields_and_limits() {
    for mailbox in ["muse"] {
        let f = Fixture::new(mailbox);
        let before = std::fs::read(&f.db).unwrap();
        for args in [
            json!({"limit":0}),
            json!({"limit":51}),
            json!({"limit":"1"}),
        ] {
            assert_tool_error(f.call_wire("p-999k_inbox", args), "invalid_limit");
        }
        for args in [
            json!({"continuation":"-1:3"}),
            json!({"continuation":"0:99999"}),
        ] {
            assert_tool_error(f.call_wire("p-999k_inbox", args), "invalid_continuation");
        }
        for args in [
            json!({"sender":"dot"}),
            json!({"recipient":"other"}),
            json!({"receipt":"synthetic"}),
            json!({"cursor":1}),
        ] {
            assert_protocol_error(f.call_wire("p-999k_inbox", args), 200, json!(1), -32602);
        }
        assert_eq!(
            f.dispatch(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"p-999k_inbox","arguments":{"limit":1,"limit":2}}}"#),
            (Err((400, "invalid_fields")), 400),
        );
        assert_eq!(std::fs::read(&f.db).unwrap(), before);
    }
}

fn page_ids(page: &Value) -> Vec<&str> {
    assert_eq!(page["metadata_only"], true);
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            assert!(message.get("body").is_none());
            message["id"].as_str().unwrap()
        })
        .collect()
}

#[test]
fn mailbox_pages_keep_tie_order_and_exclude_new_arrivals_until_refreshed() {
    for order in ["newest", "oldest"] {
        let f = Fixture::new("muse");
        let first = f.store("dot", "muse", "mailbox-page sent to Muse");
        let second = f.store("muse", "dot", "mailbox-page sent by Muse");
        let third = f.store("dot", "muse", "mailbox-page sent to Muse again");
        f.tie_timestamps();
        let expected = if order == "newest" {
            [&third, &second, &first]
        } else {
            [&first, &second, &third]
        };
        let first_page = f.call(
            "p-999k_inbox",
            json!({"scope":"all","query":"mailbox-page","order":order,"limit":1}),
        );
        assert_eq!(page_ids(&first_page), [expected[0].as_str()]);

        let arrived = f.store("dot", "muse", "mailbox-page arrived between requests");
        f.tie_timestamps();
        let before = std::fs::read(&f.db).unwrap();
        let next = f.call(
            "p-999k_inbox",
            json!({"scope":"all","query":"mailbox-page","order":order,"limit":2,
                   "continuation":first_page["next_continuation"]}),
        );
        assert_eq!(
            page_ids(&next),
            [expected[1].as_str(), expected[2].as_str()]
        );
        if !next["next_continuation"].is_null() {
            let last = f.call(
                "p-999k_inbox",
                json!({"scope":"all","query":"mailbox-page","order":order,
                       "continuation":next["next_continuation"]}),
            );
            assert!(page_ids(&last).is_empty());
            assert!(last["next_continuation"].is_null());
        }

        let fresh = f.call(
            "p-999k_inbox",
            json!({"scope":"all","query":"mailbox-page","order":order}),
        );
        let fresh = page_ids(&fresh);
        if order == "newest" {
            assert_eq!(fresh, [&arrived, &third, &second, &first]);
        } else {
            assert_eq!(fresh, [&first, &second, &third, &arrived]);
        }
        assert_eq!(std::fs::read(&f.db).unwrap(), before);
    }
}

#[test]
fn mailbox_search_matches_literal_body_only_within_current_read_scope() {
    let f = Fixture::new("muse");
    let received = f.store("dot", "muse", "before fIXTURE_%' 뒤에");
    let sent = f.store("muse", "dot", "before fixture_%' 뒤에");
    for (sender, recipient) in [("blocked", "muse"), ("muse", "blocked"), ("dot", "blocked")] {
        f.store(sender, recipient, "before fixture_%' 뒤에");
    }
    f.store("dot", "muse", "before fixtureXwild' 뒤에");

    let query = "FiXtUrE_%' 뒤에";
    let inbox = f.call("p-999k_inbox", json!({"query":query}));
    assert_eq!(page_ids(&inbox), [received.as_str()]);
    let all = f.call("p-999k_inbox", json!({"scope":"all","query":query}));
    assert_eq!(page_ids(&all), [sent.as_str(), received.as_str()]);
    assert!(inbox["next_continuation"].is_null());
    assert!(all["next_continuation"].is_null());
    let exact = f.call("p-999k_read", json!({"message_id":received}));
    assert_eq!(exact["body"], "before fIXTURE_%' 뒤에");
}

#[test]
fn mailbox_all_search_includes_a_linked_sent_reply() {
    let f = Fixture::new("muse");
    let original = f.store("dot", "muse", "synthetic original request");
    let reply = f.call(
        "p-999k_reply",
        json!({"message_id":original,"body":"outbound-search-fixture linked-marker"}),
    );
    let unacknowledged = f.store("dot", "muse", "another original still pending");
    Queue::open_existing(&f.db)
        .unwrap()
        .send(Envelope {
            sender: "muse".into(),
            recipient: "dot".into(),
            body: "outbound-search-fixture".into(),
            correlation_id: Some(unacknowledged.clone()),
            in_reply_to: Some(unacknowledged),
            idempotency_key: None,
        })
        .unwrap();

    let inbox = f.call("p-999k_inbox", json!({"query":"outbound-search-fixture"}));
    assert!(page_ids(&inbox).is_empty());
    let all = f.call(
        "p-999k_inbox",
        json!({"scope":"all","query":"outbound-search-fixture"}),
    );
    assert_eq!(page_ids(&all), [reply["reply_id"].as_str().unwrap()]);
    assert_eq!(all["messages"][0]["in_reply_to"], original);

    // Matching an unreadable body must not be visible through pagination.
    let first_page = |query| {
        f.call(
            "p-999k_inbox",
            json!({"scope":"all","query":query,"order":"oldest","limit":1}),
        )
    };
    let shared_term = first_page("outbound-search-fixture");
    let linked_only = first_page("linked-marker");
    assert_eq!(page_ids(&shared_term), page_ids(&linked_only));
    assert_eq!(
        shared_term["next_continuation"].is_null(),
        linked_only["next_continuation"].is_null()
    );
}

#[test]
fn mailbox_continuation_binds_scope_query_and_order() {
    let f = Fixture::new("muse");
    let page = f.call(
        "p-999k_inbox",
        json!({"query":"EXACT_READ_FIXTURE_BODY","limit":1}),
    );
    let cursor = &page["next_continuation"];
    assert!(cursor.is_string());
    for args in [
        json!({"scope":"all","query":"EXACT_READ_FIXTURE_BODY","continuation":cursor}),
        json!({"query":"FIXTURE","continuation":cursor}),
        json!({"query":"EXACT_READ_FIXTURE_BODY","order":"oldest","continuation":cursor}),
        json!({"continuation":cursor}),
    ] {
        assert_tool_error(f.call_wire("p-999k_inbox", args), "invalid_continuation");
    }

    let next = f.call(
        "p-999k_inbox",
        json!({"scope":"inbox","query":"EXACT_READ_FIXTURE_BODY","order":"newest",
               "continuation":cursor}),
    );
    assert_eq!(page_ids(&next).len(), 1);
    assert_ne!(page_ids(&page), page_ids(&next));
    assert!(next["next_continuation"].is_null());
}

#[test]
fn muse_host_accepts_empty_and_large_registries() {
    let f = Fixture::new("muse");
    let mut registry = json!({"muse":{"mailbox":"muse","revoked":false}});
    private(&f.registry, &registry);
    assert!(
        f.call("p-999k_inbox", json!({}))["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.call("p-999k_wait", json!({"timeout_ms":0}))["state"],
        "timeout"
    );
    for i in 0..33 {
        let name = format!("peer-{i}");
        registry[&name] = json!({"mailbox":name,"revoked":false});
    }
    registry["dot"] = json!({"mailbox":"dot","revoked":false});
    private(&f.registry, &registry);
    let page = f.call("p-999k_inbox", json!({}));
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    let id = page["messages"][0]["id"].clone();
    assert_eq!(
        f.call("p-999k_read", json!({"message_id":id}))["sender"],
        "dot"
    );
    registry["dot"]["revoked"] = json!(true);
    private(&f.registry, &registry);
    let (reply, status) = f.call_wire("p-999k_reply", json!({"message_id":id,"body":"answer"}));
    assert_eq!(status, 200);
    assert_eq!(reply.unwrap().unwrap()["result"]["isError"], true);
}

#[test]
fn muse_mcp_distinguishes_transport_protocol_and_tool_errors() {
    let f = Fixture::new("muse");
    let state = f.state();
    assert_eq!(
        request(&state, &[], b"{", STANDARD_HEADERS).unwrap_err(),
        (401, "unauthorized")
    );
    for bad in [
        b"{".as_slice(),
        b"[]",
        br#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#,
    ] {
        assert_eq!(
            f.dispatch(bad),
            (Ok(None), 415),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    assert_protocol_error(
        f.dispatch(br#"{"jsonrpc":"2.0","id":"fixture","method":"unknown_method"}"#),
        200,
        json!("fixture"),
        -32601,
    );
    assert_protocol_error(
        f.call_wire("unknown_tool", json!({})),
        200,
        json!(1),
        -32602,
    );
    assert_tool_error(
        f.call_wire("p-999k_read", json!({"message_id":"missing-fixture"})),
        "message_not_available",
    );

    let pending = f.call("p-999k_inbox", json!({"limit":1}))["messages"][0]["id"].clone();
    let args = json!({"message_id":pending,"body":"synthetic reply"});
    let first = f.call("p-999k_reply", args.clone());
    let repeat = f.call("p-999k_reply", args);
    assert_eq!(repeat["reply_id"], first["reply_id"]);
    assert_eq!(repeat["duplicate"], true);
    assert_tool_error(
        f.call_wire(
            "p-999k_reply",
            json!({"message_id":pending,"body":"different synthetic reply"}),
        ),
        "reply_conflict",
    );
}

#[test]
fn muse_sdk_negotiates_headers_and_does_not_start_sessions() {
    let f = Fixture::new("muse");
    let state = f.state();
    let auth = Fixture::auth();
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}"#;
    for headers in ["", "Accept: application/json\r\n"] {
        assert_eq!(request(&state, &auth, init, headers).unwrap().status(), 406);
    }
    let reply = request(&state, &auth, init, STANDARD_HEADERS).unwrap();
    assert_eq!(reply.status(), 200);
    assert!(!reply.headers().contains_key("mcp-session-id"));
    let init: Value = serde_json::from_slice(reply.body()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "999k-muse");
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert!(init["result"]["capabilities"].get("events").is_none());

    let discovery = br#"{"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let headers =
        "Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\n";
    assert_eq!(
        request(
            &state,
            &auth,
            discovery,
            &format!("{headers}Mcp-Method: tools/list\r\n")
        )
        .unwrap()
        .status(),
        400
    );
    let reply = request(
        &state,
        &auth,
        discovery,
        &format!("{headers}Mcp-Method: server/discover\r\n"),
    )
    .unwrap();
    assert_eq!(reply.status(), 200);
    let reply: Value = serde_json::from_slice(reply.body()).unwrap();
    assert_eq!(reply["result"]["resultType"], "complete", "{reply}");
}

#[test]
fn muse_sdk_cancels_an_active_wait_only_after_acceptance() {
    let f = Fixture::new("muse");
    private(
        &f.registry,
        &json!({"muse":{"mailbox":"muse","revoked":false}}),
    );
    let state = &f.state();
    let auth = &Fixture::auth();
    let wait = br#"{"jsonrpc":"2.0","id":"fixture-cancel","method":"tools/call","params":{"name":"p-999k_wait","arguments":{"timeout_ms":4000}}}"#;
    let cancel = br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"fixture-cancel"}}"#;

    thread::scope(|scope| {
        let worker = scope.spawn(|| request(state, auth, wait, STANDARD_HEADERS));
        let until = Instant::now() + Duration::from_secs(2);
        while !state.waiting.load(Ordering::Acquire) {
            assert!(Instant::now() < until, "wait did not start");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            request(state, &[], cancel, STANDARD_HEADERS).unwrap_err(),
            (401, "unauthorized")
        );
        assert_eq!(request(state, auth, cancel, "").unwrap().status(), 406);
        let key = RequestId::String("fixture-cancel".into());
        assert!(!state.active.lock().unwrap()[&key].load(Ordering::Acquire));
        assert_eq!(
            request(state, auth, wait, STANDARD_HEADERS).unwrap_err(),
            (409, "request_already_active")
        );
        let reply = request(state, auth, cancel, STANDARD_HEADERS).unwrap();
        assert_eq!(reply.status(), 202);
        assert!(reply.body().is_empty());
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            (499, "request_cancelled")
        );
        assert!(!state.waiting.load(Ordering::Acquire));
        assert!(state.active.lock().unwrap().is_empty());
    });
}

#[test]
fn muse_continuation_rechecks_current_authority_inside_the_write() {
    let f = Fixture::new("muse");
    let state = f.state();
    let authorization = Fixture::auth();
    let active = state
        .begin(RequestId::String("checkpoint-fixture".into()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let parent = f.call("p-999k_inbox", json!({"limit":1}))["messages"][0]["id"].clone();
    let arguments = json!({
        "recipient":"dot", "body":"fixture continuation",
        "idempotency_key":"test-checkpoint", "continue_from_message_id":parent
    });
    let mut policy: Value = serde_json::from_slice(&std::fs::read(&f.policy).unwrap()).unwrap();
    policy["operations"] = json!(["send", "read"]);
    let registry = json!({
        "muse":{"mailbox":"muse","revoked":false},
        "dot":{"mailbox":"dot","revoked":false},
        "other":{"mailbox":"other","revoked":false}
    });

    for (checkpoint, change, expected) in [
        (2, "recipient", (403, "forbidden")),
        (3, "other", (403, "thread_scope_changed")),
        (3, "operation", (403, "forbidden")),
        (3, "expiry", (401, "unauthorized")),
        (3, "cancel", (499, "request_cancelled")),
    ] {
        private(&f.policy, &policy);
        private(&f.registry, &registry);
        active.cancelled.store(false, Ordering::Release);
        let mut seen = 0;
        let result = send_registered(
            &f.db,
            "muse",
            muse_mailbox::MAX_REQUEST_BYTES as i64,
            &arguments,
            || {
                state
                    .scope(&authorization, auth::MuseOperation::Read, &active, deadline)
                    .map(|scope| scope.allowed_senders)
            },
            |recipient, peers| {
                seen += 1;
                if seen == checkpoint {
                    match change {
                        "recipient" | "other" => {
                            let mut registry = registry.clone();
                            let peer = if change == "recipient" {
                                "dot"
                            } else {
                                "other"
                            };
                            registry[peer]["revoked"] = json!(true);
                            private(&f.registry, &registry);
                        }
                        "operation" | "expiry" => {
                            let mut policy = policy.clone();
                            if change == "operation" {
                                policy["operations"] = json!(["send"]);
                            } else {
                                policy["expires_at_ms"] = json!(0);
                            }
                            private(&f.policy, &policy);
                        }
                        "cancel" => active.cancelled.store(true, Ordering::Release),
                        _ => unreachable!(),
                    }
                }
                state.send_checkpoint(&authorization, &active, deadline, recipient, peers)
            },
        );
        assert_eq!(result, Err(expected), "{change}");
        assert_eq!(seen, checkpoint, "{change}");
        let n: i64 = rusqlite::Connection::open(&f.db)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE idempotency_key=?1",
                ["test-checkpoint"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{change}");
    }
}
