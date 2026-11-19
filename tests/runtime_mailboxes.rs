//! Synthetic, real-binary coverage for isolated mailbox registration and boundaries.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const BIN: &str = env!("CARGO_BIN_EXE_pigeonmq");

struct Hub(Child);
impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn init(home: &Path, name: &str) -> (Value, Vec<u8>) {
    let out = Command::new(BIN)
        .args(["init", name, "--home"])
        .arg(home)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let token_path = PathBuf::from(value["token_file"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let token = fs::read(&token_path).unwrap();
    assert_eq!(token.len(), 64);
    assert!(!out.stdout.windows(token.len()).any(|bytes| bytes == token));
    assert!(!out.stderr.windows(token.len()).any(|bytes| bytes == token));
    (value, token)
}

fn request(port: u16, route: &str, token: &[u8], method: &str, params: Value) -> (u16, Value) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    post(
        port,
        route,
        token,
        "application/json, text/event-stream",
        body,
    )
}

fn post(port: u16, route: &str, token: &[u8], accept: &str, body: Value) -> (u16, Value) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "POST {route} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\nAccept: {accept}\r\n\
         Content-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    if !token.is_empty() {
        stream.write_all(b"Authorization: Bearer ").unwrap();
        stream.write_all(token).unwrap();
        stream.write_all(b"\r\n").unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
    let mut output = String::new();
    stream.read_to_string(&mut output).unwrap();
    let (head, body) = output.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.into())),
    )
}

fn triage_home(home: &Path) -> PathBuf {
    let triage = home.join("triage");
    fs::create_dir_all(&triage).unwrap();
    for directory in [home, &triage] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    triage
}

fn write_triage(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn observe_record(agent: &str) -> Value {
    json!({"agent":agent, "service":"Synthetic service", "surface":"Loopback fixture",
        "adapter_path":"local_mcp", "initiator":"agent", "transport":"streamable_http",
        "auth":"bearer", "setup_owner":"operator_settings", "wake":"none",
        "operations":["read"], "send_to":[], "evidence":"candidate", "next_step":"Check local tools."})
}

#[test]
fn multiple_runtime_names_share_rmcp_and_keep_credentials_and_peers_separate() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("sandbox");
    // This mailbox name is unknown until test runtime.
    let name = format!("fixture-{}", uuid::Uuid::new_v4().simple());
    assert!(pigeonmq::mailbox_catalog::http_by_id(&name).is_none());
    let read_route = format!("/{name}/mcp");
    let record = triage_home(&home).join(format!("{name}.json"));
    write_triage(
        &record,
        &serde_json::to_vec(&observe_record(&name)).unwrap(),
    );
    let (read_only, read_token) = init(&home, &name);
    assert_eq!(read_only["route"], read_route);
    assert_eq!(read_only["remote_verified"], false);
    let policy_path = home.join("local-mailboxes").join(&name).join("policy.json");
    let policy = fs::read(&policy_path).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&policy).unwrap()["principals"][0]["operations"],
        json!(["read"])
    );
    let mut changed = observe_record(&name);
    changed["operations"] = json!(["wait"]);
    write_triage(&record, &serde_json::to_vec(&changed).unwrap());
    let refused = Command::new(BIN)
        .args(["init", &name, "--home"])
        .arg(&home)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("differ from this staged mailbox"));
    fs::remove_file(record).unwrap();
    let (same, same_token) = init(&home, &name);
    assert_eq!(same["created"], false);
    assert_eq!(same_token, read_token);
    assert_eq!(fs::read(policy_path).unwrap(), policy);
    let (synthetic, synthetic_token) = init(&home, "synthetic-pull");
    assert_eq!(synthetic["route"], "/synthetic-pull/mcp");
    // This is a synthetic local registration, not the unverified Vooy app's record.
    let mut vooy_record = observe_record("vooy");
    vooy_record["operations"] = json!(["wait"]);
    write_triage(
        &home.join("triage/vooy.json"),
        &serde_json::to_vec(&vooy_record).unwrap(),
    );
    let (vooy, vooy_token) = init(&home, "vooy");
    assert_eq!(vooy["route"], "/vooy/mcp");
    let profile_dir = home.join("local-mailboxes/vooy");
    let read_config =
        |file| serde_json::from_slice::<Value>(&fs::read(profile_dir.join(file)).unwrap()).unwrap();
    assert_eq!(
        read_config("profile.json"),
        json!({"mailbox":"vooy","capability":"wait"})
    );
    let policy = read_config("policy.json");
    assert_eq!(policy["principals"][0]["operations"], json!(["wait"]));
    assert_eq!(policy["principals"][0]["mailboxes"], json!(["vooy"]));
    assert_eq!(policy["principals"][0]["send_to"], json!([]));
    assert_eq!(vooy["runtime"], "local_only");
    assert_eq!(vooy["remote_verified"], false);
    assert_eq!(vooy["created"], true);
    assert!(synthetic_token != vooy_token);
    let (again, same_token) = init(&home, "vooy");
    assert_eq!(again["created"], false);
    assert!(same_token == vooy_token);
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut hub = Hub(Command::new(BIN)
        .args(["serve-hub", "--mailbox-home"])
        .arg(&home)
        .args(["--port", &port.to_string(), "--seconds", "10"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    let mut line = String::new();
    BufReader::new(hub.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ready: Value = serde_json::from_str(&line).expect("sandbox hub did not emit readiness");
    assert_eq!(ready["local_mailbox_enabled"], true);
    assert_eq!(ready["local_mailbox_port"], port);
    assert_eq!(ready["grok_enabled"], false);
    assert_eq!(ready["codex_enabled"], false);
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let (status, denied) = post(
        port,
        &read_route,
        &read_token,
        "application/json",
        list.clone(),
    );
    assert_eq!(status, 406);
    assert!(
        denied
            .as_str()
            .unwrap()
            .contains("must accept both application/json and text/event-stream")
    );
    assert_eq!(
        post(port, &read_route, &vooy_token, "application/json", list).0,
        401
    );
    let (status, visible) = request(port, &read_route, &read_token, "tools/list", json!({}));
    assert_eq!(status, 200);
    let mut visible: Vec<_> = visible["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    visible.sort_unstable();
    assert_eq!(
        visible,
        ["pigeonmq_inbox", "pigeonmq_read", "pigeonmq_read_thread"]
    );
    let mut queue =
        pigeonmq::Queue::open_existing(home.join("local-mailboxes/mailboxes.sqlite3")).unwrap();
    let message = queue
        .send(pigeonmq::Envelope {
            sender: name.clone(),
            recipient: name,
            body: "synthetic observation".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message;
    drop(queue);
    let (status, observed) = request(
        port,
        &read_route,
        &read_token,
        "tools/call",
        json!({"name":"pigeonmq_read","arguments":{"message_id":message.id}}),
    );
    assert_eq!(status, 200);
    let content: Value =
        serde_json::from_str(observed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(content["envelope"]["body"], "synthetic observation");
    assert_eq!(content["message_id"], message.id);
    assert_eq!(content["conversation_id"], message.id);
    let (_, thread) = request(
        port,
        &read_route,
        &read_token,
        "tools/call",
        json!({"name":"pigeonmq_read_thread","arguments":{"conversation_id":message.id}}),
    );
    let result: Value =
        serde_json::from_str(thread["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["messages"][0]["message_id"], message.id);
    let (_, forbidden) = request(
        port,
        &read_route,
        &read_token,
        "tools/call",
        json!({"name":"pigeonmq_wait","arguments":{"timeout_ms":0}}),
    );
    assert!(forbidden.get("error").is_some() || forbidden["result"]["isError"] == true);
    for (route, token) in [
        (read_route.as_str(), &read_token),
        ("/vooy/mcp", &vooy_token),
    ] {
        let (status, initialized) = request(
            port,
            route,
            token,
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"local-fixture","version":"1"}}),
        );
        assert_eq!(status, 200);
        assert_eq!(
            initialized["result"]["serverInfo"]["name"],
            "pigeonmq-local-mailbox"
        );
        assert!(initialized["result"]["capabilities"]["events"].is_null());
    }
    for (route, token, expected) in [
        (
            "/synthetic-pull/mcp",
            &synthetic_token,
            &[
                "pigeonmq_inbox",
                "pigeonmq_read",
                "pigeonmq_read_thread",
                "pigeonmq_wait",
            ][..],
        ),
        ("/vooy/mcp", &vooy_token, &["pigeonmq_wait"][..]),
    ] {
        let (status, response) = request(port, route, token, "tools/list", json!({}));
        assert_eq!(status, 200, "{route}: {response}");
        let mut names: Vec<_> = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, expected);
        let (status, response) = request(
            port,
            route,
            token,
            "tools/call",
            json!({"name":"pigeonmq_wait","arguments":{"timeout_ms":0}}),
        );
        assert_eq!(status, 200, "{route}: {response}");
        let result: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(result, json!({"message":null}));
    }
    let (_, forbidden) = request(
        port,
        "/vooy/mcp",
        &vooy_token,
        "tools/call",
        json!({"name":"pigeonmq_inbox","arguments":{"scope":"all","query":"synthetic"}}),
    );
    assert!(forbidden.get("error").is_some() || forbidden["result"]["isError"] == true);
    assert_eq!(
        request(port, "/vooy/mcp", &[], "tools/list", json!({})).0,
        401
    );
    assert_eq!(
        request(port, "/vooy/mcp", &synthetic_token, "tools/list", json!({})).0,
        401
    );
    assert_eq!(
        request(
            port,
            "/not-staged/mcp",
            &vooy_token,
            "tools/list",
            json!({})
        )
        .0,
        404
    );
    for route in ["/vooy/mcp/", "/vooy/mcp?test=1", "/mcp", "/muse/mcp"] {
        assert_eq!(
            request(port, route, &vooy_token, "tools/list", json!({})).0,
            404
        );
    }
    let (_, denied) = request(
        port,
        "/vooy/mcp",
        &vooy_token,
        "tools/call",
        json!({
            "name":"pigeonmq_send",
            "arguments":{"recipient":"synthetic-pull","body":"synthetic marker","idempotency_key":"one"}
        }),
    );
    assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    let registration = home.join("local-mailboxes/vooy/registration.json");
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&registration).unwrap()).unwrap(),
        json!({"vooy":{"mailbox":"vooy","revoked":false}})
    );
    fs::write(
        registration,
        br#"{"vooy":{"mailbox":"vooy","revoked":true}}"#,
    )
    .unwrap();
    assert_eq!(
        request(port, "/vooy/mcp", &vooy_token, "tools/list", json!({})).0,
        403
    );
}

#[test]
fn local_init_rejects_reserved_routes_and_reuses_no_existing_cloud_home() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("cloud");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let existing = home.join("unrelated-config");
    fs::write(&existing, b"kept").unwrap();
    for name in ["vooy", "codex", "receiver", "../vooy", "dot", "grok-bot"] {
        let result = Command::new(BIN)
            .args(["init", name, "--home"])
            .arg(&home)
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
    assert_eq!(fs::read(&existing).unwrap(), b"kept");
    assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
    let isolated_home = temp.path().join("isolated");
    assert!(
        !Command::new(BIN)
            .args(["init", "codex", "--home"])
            .arg(&isolated_home)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!isolated_home.exists());
    assert!(
        !Command::new(BIN)
            .args(["serve-hub", "--mailbox-home"])
            .arg(&home)
            .args(["--port", "1", "--continuous"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn invalid_or_unsupported_triage_never_stages_a_mailbox() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("sandbox");
    let triage = triage_home(&home);
    let run = |name: &str| {
        Command::new(BIN)
            .args(["init", name, "--home"])
            .arg(&home)
            .output()
            .unwrap()
    };
    let candidate = triage.join("vooy.json");
    write_triage(&candidate, include_bytes!("../schemas/examples/vooy.json"));
    let refused = run("vooy");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("verify remote support separately"));
    let path = triage.join("fixture-blocked.json");
    for (key, value, reason) in [
        ("agent", json!("different"), "agent must match"),
        ("wake", json!("webhook"), "wake:none"),
        ("send_to", json!(["synthetic-peer"]), "cannot grant peer"),
        (
            "operations",
            json!(["reply", "ack"]),
            "supports only read and wait",
        ),
        (
            "operations",
            json!(["read", "read"]),
            "invalid triage record",
        ),
        ("transport", json!("invented"), "invalid triage record"),
        ("unexpected_field", json!(true), "invalid triage record"),
    ] {
        let mut record = observe_record("fixture-blocked");
        record[key] = value;
        write_triage(&path, &serde_json::to_vec(&record).unwrap());
        let output = run("fixture-blocked");
        assert!(!output.status.success(), "{key}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{key}"
        );
        assert!(output.stdout.is_empty(), "{key}");
        assert!(!home.join("local-mailboxes").exists(), "{key}");
    }
    let valid = serde_json::to_vec(&observe_record("fixture-blocked")).unwrap();
    write_triage(&path, &valid);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!run("fixture-blocked").status.success());
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(candidate, path).unwrap();
    assert!(!run("fixture-blocked").status.success());
    assert!(!home.join("local-mailboxes").exists());
}
