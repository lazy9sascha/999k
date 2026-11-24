//! Real binary/loopback checks. Uses only temporary state and no provider calls.
use radio999k::{Queue, events::runtime::InstanceLock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn post(port: u16, path: &str, token: &str, method: &str, params: Value) -> (u16, Value) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    let mut headers = "Accept: application/json, text/event-stream\r\n".to_owned();
    if let Some(version) = params["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str() {
        headers.push_str(&format!(
            "MCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n"
        ));
    }
    let (status, body) = post_raw(port, path, token, &body, &headers);
    (status, serde_json::from_str(&body).unwrap())
}

fn post_raw(port: u16, path: &str, token: &str, body: &str, headers: &str) -> (u16, String) {
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
fn dot_events_run_only_on_authenticated_dot_route_with_explicit_preparation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    let policy = dir.path().join(
        radio999k::mailbox_catalog::by_adapter(radio999k::mailbox_catalog::Adapter::Dot)
            .policy_file,
    );
    let registration = dir.path().join("registration.json");
    drop(Queue::open(&db).unwrap());
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    for (path, value) in [
        (
            &policy,
            json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot","enabled":true,"token_sha256":Sha256::digest(b"fixture").iter().map(|b|format!("{b:02x}")).collect::<String>(),"registration":registration,"events":{"callback_hosts":["callback.example.com"],"resolver":env!("CARGO_BIN_EXE_999k-dot-resolve")}}),
        ),
        (
            &registration,
            json!({"dot":{"mailbox":"dot","revoked":false},"muse":{"mailbox":"muse","revoked":false}}),
        ),
    ] {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let prep = Command::new(env!("CARGO_BIN_EXE_999k"))
        .args(["dot-prepare", "--queue"])
        .arg(&db)
        .arg("--auth-policy")
        .arg(&policy)
        .output()
        .unwrap();
    assert!(
        prep.status.success(),
        "{}",
        String::from_utf8_lossy(&prep.stderr)
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_999k"))
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .arg("--bundled-profiles")
            .args(["--port", &port.to_string(), "--seconds", "2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(Instant::now() < until, "startup timeout");
        assert!(child.0.try_wait().unwrap().is_none(), "hub exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The socket binds before the Events worker starts. Wait for HTTP dispatch.
    assert_eq!(
        post(port, "/dot/mcp", "wrong", "events/list", json!({})).0,
        401
    );
    let legacy = r#"{"jsonrpc":"2.0","id":1,"method":"events/list","params":{}}"#;
    assert_eq!(post_raw(port, "/dot/mcp", "wrong", legacy, "").0, 401);
    assert_eq!(post_raw(port, "/dot/mcp", "fixture", legacy, "").0, 406);
    let (status, reply) = post_raw(
        port,
        "/dot/mcp",
        "fixture",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "Accept: application/json, text/event-stream\r\n",
    );
    assert_eq!(status, 202);
    assert!(reply.is_empty());
    assert!(InstanceLock::acquire(&db).is_err());
    assert!(InstanceLock::acquire_codex(&db).is_ok());
    let (status, init) = post(
        port,
        "/dot/mcp",
        "fixture",
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(status, 200);
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["capabilities"]["events"], json!({}));
    let meta = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}});
    let (status, discovered) = post(port, "/dot/mcp", "fixture", "server/discover", meta.clone());
    assert_eq!(status, 200);
    assert_eq!(discovered["result"]["capabilities"]["events"], json!({}));
    let (status, defs) = post(port, "/dot/mcp", "fixture", "events/list", meta.clone());
    assert_eq!(status, 200);
    let modern = json!({"jsonrpc":"2.0","id":2,"method":"events/list","params":meta});
    assert_eq!(
        post_raw(
            port,
            "/dot/mcp",
            "fixture",
            &modern.to_string(),
            "Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list\r\n",
        ).0,
        400
    );
    assert_eq!(
        defs["result"]["events"][0]["inputSchema"]["properties"]["recipient"]["enum"],
        json!(["dot"])
    );
    let (_, denied) = post(
        port,
        "/dot/mcp",
        "fixture",
        "events/subscribe",
        json!({"name":"pigeonmq.message.created","arguments":{"recipient":"codex-cloud"},"delivery":{"mode":"webhook","url":"https://callback.example.com/events","secret":"invalid"}}),
    );
    assert!(denied.get("error").is_some());
    for path in ["/codex/mcp", "/muse/mcp", "/grok/mcp", "/mcp"] {
        assert_eq!(post(port, path, "fixture", "events/list", json!({})).0, 404);
    }
    let count: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM dot_event_subscriptions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    let mut output = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    let ready: Value = serde_json::from_str(output.lines().next().unwrap()).unwrap();
    assert_eq!(ready["dot_events_enabled"], true);
    assert_eq!(ready["codex_events_enabled"], false);
}

#[test]
fn dot_prepare_adds_only_its_binding_to_an_existing_shared_manifest() {
    use std::{fs, os::fd::AsRawFd, path::Path};

    fn put(path: &Path, bytes: impl AsRef<[u8]>) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    drop(Queue::open(&db).unwrap());
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let manifest = dir.path().join("shared-profiles.json");
    let dot = dir.path().join("dot-policy.json");
    let muse = dir.path().join("muse-policy.json");
    let registration = dir.path().join("registration.json");
    let digest = |token: &str| {
        Sha256::digest(token.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let dot_policy = json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot","enabled":true,
        "token_sha256":digest("dot-test-only"),"registration":registration});
    let muse_policy = json!({"principal":"muse","mailbox":"muse","enabled":true,
        "token_sha256":digest("muse-test-only"),"registration":registration,
        "expires_at_ms":u64::MAX,"operations":["read"]});
    let registrations =
        json!({"dot":{"mailbox":"dot","revoked":false},"muse":{"mailbox":"muse","revoked":false}});
    put(&dot, dot_policy.to_string());
    let other_dot = dir.path().join("other-dot.json");
    put(&other_dot, dot_policy.to_string());
    put(&muse, muse_policy.to_string());
    put(&registration, registrations.to_string());
    radio999k::muse_mailbox::prepare(&db).unwrap();

    let binary = env!("CARGO_BIN_EXE_999k");
    let prepare = || {
        Command::new(binary)
            .args(["dot-prepare", "--queue"])
            .arg(&db)
            .arg("--auth-policy")
            .arg(&dot)
            .arg("--profiles-file")
            .arg(&manifest)
            .output()
            .unwrap()
    };
    assert!(!prepare().status.success());
    assert!(!manifest.exists());

    let muse_entry =
        json!({"adapter":"muse","route":"/custom-muse/mcp","policy":"muse-policy.json"});
    let original = json!({"registration":"registration.json","profiles":[muse_entry]});
    for invalid in [
        "{\"profiles\":[],\"profiles\":[]}".to_owned(),
        json!({"profiles":[muse_entry,{"adapter":"dot","route":"/different-dot/mcp","policy":dot}]}).to_string(),
        json!({"profiles":[muse_entry,{"adapter":"dot","route":"/dot/mcp","policy":other_dot}]}).to_string(),
        json!({"profiles":[{"adapter":"muse","route":"/custom-muse/mcp","policy":dot}]}).to_string(),
    ] {
        put(&manifest, &invalid);
        assert!(!prepare().status.success());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), invalid);
    }
    let original = original.to_string();
    put(&manifest, &original);
    let lock = fs::File::open(&manifest).unwrap();
    // SAFETY: the fixture keeps its own descriptor open until after the CLI exits.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(!prepare().status.success());
    drop(lock);
    let mut disabled = dot_policy.clone();
    disabled["enabled"] = json!(false);
    put(&dot, disabled.to_string());
    assert!(!prepare().status.success());
    assert_eq!(fs::read_to_string(&dot).unwrap(), disabled.to_string());
    assert_eq!(fs::read_to_string(&manifest).unwrap(), original);
    put(&dot, dot_policy.to_string());
    assert!(radio999k::dot_profile::check(&db, &dot, false).is_err());

    let result = prepare();
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap(),
        json!({"state":"dot_storage_prepared","binding_added":true,"runtime_verified":false})
    );
    let added = fs::read(&manifest).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&added).unwrap(),
        json!({"registration":"registration.json","profiles":[muse_entry,
            {"adapter":"dot","route":"/dot/mcp","policy":dot}]})
    );
    assert_eq!(
        fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let repeated = prepare();
    assert!(repeated.status.success(), "{:?}", repeated);
    assert_eq!(
        serde_json::from_slice::<Value>(&repeated.stdout).unwrap()["binding_added"],
        false
    );
    assert_eq!(fs::read(&manifest).unwrap(), added);
    for (path, prior) in [
        (&dot, dot_policy),
        (&muse, muse_policy),
        (&registration, registrations),
    ] {
        assert_eq!(fs::read_to_string(path).unwrap(), prior.to_string());
    }

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = ChildGuard(
        Command::new(binary)
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .arg("--profiles-file")
            .arg(&manifest)
            .args(["--port", &port.to_string(), "--seconds", "2"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < until, "startup timeout");
        assert!(child.0.try_wait().unwrap().is_none(), "hub exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    for (route, token) in [
        ("/dot/mcp", "dot-test-only"),
        ("/custom-muse/mcp", "muse-test-only"),
    ] {
        let (status, reply) = post(port, route, token, "tools/list", json!({}));
        assert_eq!(status, 200, "{reply}");
        assert!(reply["result"]["tools"].is_array(), "{reply}");
    }
    assert_eq!(
        post(port, "/dot/mcp", "muse-test-only", "tools/list", json!({})).0,
        401
    );
    assert_eq!(
        post(port, "/muse/mcp", "muse-test-only", "tools/list", json!({})).0,
        404
    );
}
