//! Real binary/loopback checks. Uses only temporary state and no provider calls.
use pigeonmq::{Envelope, Queue, events::runtime::InstanceLock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
    sync::mpsc,
    time::Duration,
};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_for_hub(child: &mut Child) -> (Value, BufReader<ChildStdout>) {
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let _ = send.send((line, stdout));
    });
    let (line, stdout) = receive
        .recv_timeout(Duration::from_secs(5))
        .expect("hub did not report readiness");
    reader.join().unwrap();
    let ready: Value = serde_json::from_str(&line).expect("invalid hub startup record");
    assert_eq!(ready["transport"], "hub-local-http");
    (ready, stdout)
}
fn post(port: u16, path: &str, token: &str, method: &str, params: Value) -> (u16, Value) {
    let (status, body) = post_raw(port, path, token, method, params, true);
    (status, serde_json::from_str(&body).unwrap())
}
fn post_raw(
    port: u16,
    path: &str,
    token: &str,
    method: &str,
    params: Value,
    accept: bool,
) -> (u16, String) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let headers = params["_meta"]["io.modelcontextprotocol/protocolVersion"]
        .as_str()
        .map(|version| {
            let name = params["name"]
                .as_str()
                .filter(|_| method == "tools/call")
                .map(|name| format!("Mcp-Name: {name}\r\n"))
                .unwrap_or_default();
            format!("MCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n{name}")
        })
        .unwrap_or_default();
    let accept = if accept {
        "Accept: application/json, text/event-stream\r\n"
    } else {
        ""
    };
    post_body(port, path, token, &format!("{accept}{headers}"), &body)
}
fn post_body(port: u16, path: &str, token: &str, headers: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write!(s,"POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{body}",body.len()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.to_owned(),
    )
}
#[test]
fn prepared_codex_events_are_exposed_only_on_the_existing_authenticated_hub_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    let policy = dir
        .path()
        .join(pigeonmq::mailbox_catalog::codex_http().policy_file);
    let registration = dir.path().join("pigeonmq-cloud-registration.json");
    drop(Queue::open(&db).unwrap());
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    for (path, value) in [
        (
            &policy,
            json!({"principals":[{"id":"codex-cloud","token_sha256":Sha256::digest(b"fixture").iter().map(|b|format!("{b:02x}")).collect::<String>(),"operations":["read","wait","reply","send"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}],"events":{"callback_hosts":["callback.example.com"],"resolver":env!("CARGO_BIN_EXE_pigeonmq-dot-resolve")}}),
        ),
        (
            &registration,
            json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":false}}),
        ),
    ] {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let prepare = Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
        .args(["codex-prepare", "--queue"])
        .arg(&db)
        .arg("--auth-policy")
        .arg(&policy)
        .arg("--registration")
        .arg(&registration)
        .output()
        .unwrap();
    assert!(
        prepare.status.success(),
        "{}",
        String::from_utf8_lossy(&prepare.stderr)
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .arg("--bundled-profiles")
            .args(["--port", &port.to_string(), "--seconds", "2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    // The listener binds before the Events worker owns its lock.
    let (ready, mut stdout) = wait_for_hub(&mut child.0);
    assert!(InstanceLock::acquire_codex(&db).is_err());
    let (_, disabled) = post(port, "/dot/mcp", "fixture", "tools/list", json!({}));
    assert_eq!(disabled["error"]["code"], "profile_disabled");

    assert_eq!(
        post(port, "/codex/mcp", "wrong", "events/list", json!({})).0,
        401
    );
    assert_ne!(
        post(port, "/mcp", "fixture", "events/list", json!({})).0,
        200
    );
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientCapabilities":{}});
    let (code, discovery) = post(
        port,
        "/codex/mcp",
        "fixture",
        "server/discover",
        json!({"_meta":meta}),
    );
    assert_eq!(code, 200, "{discovery}");
    assert_eq!(discovery["result"]["resultType"], "complete");
    assert_eq!(discovery["result"]["capabilities"]["events"], json!({}));
    for method in ["tools/list", "events/list"] {
        let (code, response) = post(port, "/codex/mcp", "fixture", method, json!({"_meta":meta}));
        assert_eq!(code, 200, "{response}");
        assert!(response.get("result").is_some());
    }
    let (code, body) = post_raw(
        port,
        "/codex/mcp",
        "fixture",
        "events/list",
        json!({}),
        false,
    );
    assert_eq!(code, 406);
    assert!(body.contains("application/json and text/event-stream"));
    let (code, catalog) = post(port, "/codex/mcp", "fixture", "events/list", json!({}));
    assert_eq!(code, 200);
    assert_eq!(
        catalog["result"]["events"][0]["inputSchema"]["properties"]["recipient"]["enum"],
        json!(["codex-cloud"])
    );
    let (_, init) = post(
        port,
        "/codex/mcp",
        "fixture",
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(init["result"]["capabilities"]["events"], json!({}));
    let (_, invalid) = post(
        port,
        "/codex/mcp",
        "fixture",
        "events/subscribe",
        json!({"name":"pigeonmq.message.created","arguments":{"recipient":"dot"},"delivery":{"mode":"webhook","url":"https://callback.example.com/events","secret":"SYNTHETIC_EVENT_SIGNING_SECRET"}}),
    );
    assert_eq!(invalid["error"]["message"], "unsupported_subscription");
    let incoming = Queue::open_existing(&db)
        .unwrap()
        .send(Envelope {
            sender: "dot".into(),
            recipient: "codex-cloud".into(),
            body: "fixture ordinary request".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    let (_, reply) = post(
        port,
        "/codex/mcp",
        "fixture",
        "tools/call",
        json!({"name":"pigeonmq_reply","arguments":{"message_id":incoming,"body":"\u{1}".repeat(16384)}}),
    );
    let result: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["state"], "replied");
    assert!(child.0.wait().unwrap().success());
    let owner = InstanceLock::acquire_codex(&db).unwrap();
    drop(owner);
    let mut extra = String::new();
    stdout.read_to_string(&mut extra).unwrap();
    assert!(extra.trim().is_empty(), "unexpected hub stdout");
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!stderr.contains("SYNTHETIC_EVENT_SIGNING_SECRET"));
    assert_eq!(ready["codex_events_enabled"], true);
    assert_eq!(ready["runtime_verified"], false);
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert!(
        conn.query_row(
            "SELECT acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?1",
            [incoming],
            |r| r.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM dot_event_subscriptions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(!PathBuf::from(format!("{}.dot-bridge.lock", db.display())).exists());
}

#[test]
fn codex_mailbox_without_events_serves_manual_tools_without_preparation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    let policy = dir
        .path()
        .join(pigeonmq::mailbox_catalog::codex_http().policy_file);
    let registration = dir.path().join("pigeonmq-cloud-registration.json");
    let mut queue = Queue::open(&db).unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token = "synthetic-mailbox-without-events";
    for (path, value) in [
        (
            &policy,
            json!({"principals":[{"id":"codex-cloud","token_sha256":Sha256::digest(token).iter().map(|byte|format!("{byte:02x}")).collect::<String>(),"operations":["read","wait","reply","send"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}]}),
        ),
        (
            &registration,
            json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"peer":{"mailbox":"peer","revoked":false}}),
        ),
    ] {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let incoming = queue
        .send(Envelope {
            sender: "peer".into(),
            recipient: "codex-cloud".into(),
            body: "manual mailbox request".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    drop(queue);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // No Events settings, resolver, or codex-prepare command is supplied.
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
            .env_clear()
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .arg("--bundled-profiles")
            .args(["--port", &port.to_string(), "--seconds", "2"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (ready, mut stdout) = wait_for_hub(&mut child.0);
    drop(
        InstanceLock::acquire_codex(&db).expect("mailbox-only mode must not own an Events worker"),
    );
    {
        let (status, body) = post_raw(port, "/codex/mcp", token, "ping", json!({}), false);
        assert_eq!(status, 406);
        assert!(body.contains("application/json and text/event-stream"));
        assert_eq!(
            post_raw(
                port,
                "/codex/mcp",
                "invalid-fixture",
                "ping",
                json!({}),
                false
            )
            .0,
            401
        );
        let (status, reply) = post(
            port,
            "/codex/mcp",
            token,
            "tools/call",
            json!({"name":"pigeonmq_read","arguments":{"message_id":"missing-fixture"}}),
        );
        assert_eq!(status, 200);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["isError"], true);
        for method in ["resources/list", "resources/templates/list", "prompts/list"] {
            let (_, reply) = post(port, "/codex/mcp", token, method, json!({}));
            assert_eq!(reply["error"]["code"], -32601, "{method}: {reply}");
        }
        let (status, reply) = post(
            port,
            "/codex/mcp",
            token,
            "server/discover",
            json!({"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }}),
        );
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["resultType"], "complete");
        assert_eq!(
            reply["result"]["supportedVersions"],
            json!(["2025-03-26", "2026-07-28"])
        );
    }
    let (status, initialized) = post(
        port,
        "/codex/mcp",
        token,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"mailbox-only-fixture","version":"1"}}),
    );
    assert_eq!(status, 200, "{initialized}");
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "pigeonmq-codex"
    );
    assert!(
        initialized["result"]["capabilities"]
            .get("events")
            .is_none()
    );
    let (status, listed) = post(port, "/codex/mcp", token, "tools/list", json!({}));
    assert_eq!(status, 200, "{listed}");
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "pigeonmq_confirm_terminal",
            "pigeonmq_inbox",
            "pigeonmq_read",
            "pigeonmq_read_thread",
            "pigeonmq_recipients",
            "pigeonmq_reply",
            "pigeonmq_send",
            "pigeonmq_wait"
        ]
    );
    for method in ["events/list", "events/subscribe", "events/unsubscribe"] {
        let (status, value) = post(port, "/codex/mcp", token, method, json!({}));
        assert_eq!(status, 404, "{method}: {value}");
        assert_eq!(value["error"]["code"], "events_disabled");
    }
    let unpack = |response: Value| -> Value {
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let (status, read) = post(
        port,
        "/codex/mcp",
        token,
        "tools/call",
        json!({"name":"pigeonmq_read","arguments":{"message_id":incoming}}),
    );
    assert_eq!(status, 200, "{read}");
    let read = unpack(read);
    assert_eq!(read["id"], incoming);
    assert_eq!(read["envelope"]["body"], "manual mailbox request");
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT attempts,acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?1",
            [&incoming],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
        )
        .unwrap(),
        (0, false)
    );
    let (status, replied) = post(
        port,
        "/codex/mcp",
        token,
        "tools/call",
        json!({"name":"pigeonmq_reply","arguments":{"message_id":incoming,"body":"manual answer"}}),
    );
    assert_eq!(status, 200, "{replied}");
    let replied = unpack(replied);
    assert_eq!(replied["state"], "replied");
    let reply_id = replied["reply_id"].as_str().unwrap();
    let final_state: (bool, String, String) = conn.query_row(
        "SELECT original.acknowledged_at_ms IS NOT NULL,reply.body,reply.in_reply_to FROM messages original JOIN messages reply ON reply.id=?2 WHERE original.id=?1",
        [&incoming, reply_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(final_state, (true, "manual answer".into(), incoming));
    assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('dot_event_subscriptions','dot_event_outbox','dot_subscription_generations')",
        [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert!(child.0.wait().unwrap().success());
    let mut extra = String::new();
    stdout.read_to_string(&mut extra).unwrap();
    assert!(extra.trim().is_empty(), "unexpected hub stdout");
    assert_eq!(ready["codex_enabled"], true);
    assert_eq!(ready["codex_events_enabled"], false);
    assert_eq!(ready["runtime_verified"], false);
}

#[test]
fn codex_registration_does_not_grant_generic_targets_even_with_the_same_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    let policy = dir
        .path()
        .join(pigeonmq::mailbox_catalog::codex_http().policy_file);
    let registration = dir.path().join("pigeonmq-cloud-registration.json");
    drop(Queue::open(&db).unwrap());
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token = "synthetic-shared-policy";
    let mut policy_value = json!({"principals":[{"id":"codex-cloud","token_sha256":Sha256::digest(token).iter().map(|byte|format!("{byte:02x}")).collect::<String>(),
        "operations":["send","request","receive","read"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}]});
    let write = |path: &std::path::Path, value: &Value| {
        pigeonmq::private_file::replace_private(path, &serde_json::to_vec(value).unwrap()).unwrap();
    };
    write(&policy, &policy_value);
    write(
        &registration,
        &json!({
            "codex-cloud":{"mailbox":"codex-cloud","revoked":false},
            "peer":{"mailbox":"peer","revoked":false}
        }),
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
            .env_clear()
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .arg("--auth-policy")
            .arg(&policy)
            .arg("--bundled-profiles")
            .args(["--port", &port.to_string(), "--seconds", "5"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (ready, _stdout) = wait_for_hub(&mut child.0);
    assert_eq!(ready["generic_enabled"], true);
    let codex = |name, arguments| {
        let (status, response) = post(
            port,
            "/codex/mcp",
            token,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        );
        assert_eq!(status, 200, "{response}");
        let result = &response["result"];
        (
            result["isError"].clone(),
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        )
    };
    let generic =
        |route, recipient, key| {
            let (status, response) = post_body(port, route, token, "", &json!({
            "sender":"codex-cloud","recipient":recipient,"body":"fixture","idempotency_key":key
        }).to_string());
            (status, serde_json::from_str::<Value>(&response).unwrap())
        };
    let (is_error, recipients) = codex("pigeonmq_recipients", json!({}));
    assert_eq!(is_error, false);
    assert_eq!(recipients["recipients"], json!(["peer"]));
    assert_eq!(
        codex(
            "pigeonmq_send",
            json!({
                "recipient":"peer","body":"Codex fixture","idempotency_key":"codex-to-peer"
            })
        )
        .0,
        false
    );
    for route in ["/messages/send", "/rpc/request"] {
        assert_eq!(generic(route, "peer", "missing-grant").0, 403);
        assert_eq!(generic(route, "codex-cloud", "missing-self-grant").0, 403);
    }

    // An explicit generic grant does not affect registration-backed Codex discovery.
    policy_value["principals"][0]["send_to"] = json!(["codex-cloud", "generic-only"]);
    write(&policy, &policy_value);
    assert_eq!(codex("pigeonmq_recipients", json!({})).1, recipients);
    assert_eq!(
        codex(
            "pigeonmq_send",
            json!({
                "recipient":"generic-only","body":"fixture","idempotency_key":"not-registered"
            })
        )
        .0,
        true
    );
    assert_eq!(
        generic("/messages/send", "generic-only", "send-explicit").0,
        200
    );

    // Authorized self traffic takes the normal enqueue and delivery paths.
    let (is_error, own_mail) = codex(
        "pigeonmq_send",
        json!({
            "recipient":"codex-cloud","body":"Codex note","idempotency_key":"self-note"
        }),
    );
    assert_eq!(is_error, false);
    let own_id = own_mail["message"]["id"].as_str().unwrap();
    assert_eq!(
        codex("pigeonmq_read", json!({"message_id":own_id})).1["envelope"]["body"],
        "Codex note"
    );
    let (status, rpc) = generic("/rpc/request", "codex-cloud", "self-rpc");
    assert_eq!(status, 200, "{rpc}");
    let (status, delivery) = post_body(
        port,
        "/rpc/receive",
        token,
        "",
        r#"{"recipient":"codex-cloud","lease_seconds":30}"#,
    );
    assert_eq!(status, 200, "{delivery}");
    let delivery: Value = serde_json::from_str(&delivery).unwrap();
    assert_eq!(delivery["message"]["id"], rpc["message"]["id"]);
    assert_eq!(delivery["attempt"], 1);
    assert_eq!(
        Queue::inspect_dead_letter(&db.canonicalize().unwrap(), own_id).unwrap(),
        pigeonmq::DeadLetterInspection::NotDeadLetteredOrMissing
    );
}
