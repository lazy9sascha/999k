use pigeonmq::{Envelope, Queue, grok_mailbox, grok_wake, hub_http};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_pigeonmq");
const ORIGIN: &str = "https://private-origin.example";
const TOKEN: &str = "fixture-bearer-sentinel";
const WAKE_URL: &str = "http://private-wake.example";
const WAKE_AUTH: &str = "fixture-wake-header-sentinel";
const BODY: &str = "fixture-request-body-sentinel";

fn private(path: &Path, body: impl AsRef<[u8]>) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn runner(home: &Path, port: u16, manifest: Option<&Path>, operator_health: bool) -> Runner {
    let mut command = Command::new(BIN);
    command.args(["run", "--home"]).arg(home);
    if let Some(manifest) = manifest {
        command.arg("--profiles-file").arg(manifest);
    }
    if operator_health {
        command.arg("--operator-health");
    }
    Runner(
        command
            .args(["--port", &port.to_string()])
            .env("PIGEONMQ_TUNNEL", "external")
            .env("PIGEONMQ_PUBLIC_URL", ORIGIN)
            .stdout(fs::File::create(home.join("stdout")).unwrap())
            .stderr(fs::File::create(home.join("stderr")).unwrap())
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

struct Runner(Child);
impl Runner {
    fn stop(&mut self) -> bool {
        if let Some(status) = self.0.try_wait().unwrap() {
            return status.success();
        }
        // SAFETY: this test owns the unreaped child named by this PID.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let limit = Instant::now() + Duration::from_secs(6);
        while Instant::now() < limit {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status.success();
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
        false
    }
}
impl Drop for Runner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn health(home: &Path, ready: bool) {
    let output = Command::new(BIN)
        .args(["health", "--home"])
        .arg(home)
        .output()
        .unwrap();
    let status = serde_json::from_slice::<Value>(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(i32::from(!ready)), "{status}");
    assert_eq!(status["local_ready"], ready);
    assert_eq!(status["remote_verified"], false);
    assert_eq!(status["reason"].is_null(), ready);
    if ready {
        assert_eq!(status["hub"]["state"], "ready");
        assert_eq!(status["queue"]["state"], "readable");
        assert_eq!(status["queue"]["never_claimed_backlog"]["count"], 1);
        assert!(status["queue"]["never_claimed_backlog"]["oldest_age_ms"].is_u64());
        assert_eq!(status["tunnel"]["state"], "externally_managed");
        assert_eq!(status["tunnel"]["reason"], "public_probe_not_requested");
        assert_eq!(status["delivery"]["grok_wake"]["reason"], "auth_blocked");
        assert!(status["delivery"]["grok_wake"]["last_attempt"].is_null());
        for value in [ORIGIN, TOKEN, WAKE_URL, WAKE_AUTH, BODY] {
            assert!(!status.to_string().contains(value));
        }
    }
    assert!(output.stderr.is_empty());
}

fn health_http(stream: &mut (impl Read + Write), request: &str) -> (u16, Value) {
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.contains("Cache-Control: no-store"));
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(body).unwrap(),
    )
}

fn assert_isolated_operator_health(home: &Path, db: &Path, port: u16, queue_id: &str) {
    let socket = home.join("pigeonmq-cloud-health.sock");
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let send = |request| {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        health_http(&mut stream, request)
    };
    let get = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let (code, report) = send(get);
    assert_eq!(code, 200, "{report}");
    assert_eq!(report["local_ready"], true);
    assert_eq!(report["remote_verified"], false);
    assert_eq!(report["tunnel"]["reason"], "public_probe_not_requested");
    assert_eq!(report["queue"]["never_claimed_backlog"]["count"], 1);
    assert!(report["queue"]["never_claimed_backlog"]["oldest_age_ms"].is_u64());
    for value in [ORIGIN, TOKEN, WAKE_URL, WAKE_AUTH, BODY, queue_id] {
        assert!(!report.to_string().contains(value));
    }
    assert_eq!(
        send("POST /health HTTP/1.1\r\nHost: localhost\r\n\r\n").0,
        405
    );
    assert_eq!(
        send("GET /health?probe_public=true HTTP/1.1\r\nHost: localhost\r\n\r\n").0,
        404
    );
    assert_eq!(
        send("GET /health HTTP/1.1\r\nHost: localhost\r\nOrigin: https://fixture.test\r\n\r\n").0,
        403
    );
    assert_eq!(
        send("GET /health HTTP/1.1\r\nHost: fixture.test\r\n\r\n").0,
        403
    );

    let mut hub = TcpStream::connect(("127.0.0.1", port)).unwrap();
    hub.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    let (code, response) = health_http(
        &mut hub,
        &format!(
            "POST /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(code, 404);
    assert_eq!(response, json!({"error":{"code":"route_not_found"}}));
    // Make only this synthetic queue unreadable to operator inspection.
    fs::set_permissions(db, fs::Permissions::from_mode(0o644)).unwrap();
    let (code, report) = send(get);
    fs::set_permissions(db, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(code, 503, "{report}");
    assert_eq!(report["reason"], "queue_unavailable");
    assert_eq!(report["local_ready"], false);
    assert!(report["queue"]["never_claimed_backlog"].is_null());
}

fn assert_grok_requires_accept(port: u16) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let ping = br#"{"jsonrpc":"2.0","id":"grok-sdk-probe","method":"ping"}"#;
    write!(
        stream,
        "POST /grok/mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        ping.len(),
    )
    .unwrap();
    stream.write_all(ping).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert_eq!(response.split_whitespace().nth(1), Some("406"));
}

fn wait_for(path: &Path, expected: &str) -> Value {
    let limit = Instant::now() + Duration::from_secs(6);
    while Instant::now() < limit {
        if let Some(value) = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value["state"] == expected)
        {
            return value;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let state = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    panic!(
        "fixture never reported {expected}; last state: {}, reason: {}",
        state["state"], state["reason"]
    );
}

#[test]
fn isolated_runner_restart_preserves_grok_queue_and_recovers_wake() {
    assert_isolated_runner_restart(false);
}

#[test]
fn isolated_runner_uses_explicit_grok_wake_files_and_keeps_state_in_home() {
    assert_isolated_runner_restart(true);
}

fn assert_isolated_runner_restart(explicit: bool) {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path();
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).unwrap();
    health(home, false);
    assert_eq!(fs::read_dir(home).unwrap().count(), 0);

    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let db = home.join("pigeonmq-cloud-queue.sqlite3");
    let registration = home.join("pigeonmq-cloud-registration.json");
    let mut queue = Queue::open(&db).unwrap();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    grok_mailbox::prepare(&db).unwrap();
    let id = queue
        .send(Envelope {
            sender: "fixture-peer".into(),
            recipient: "grok-bot".into(),
            body: BODY.into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    drop(queue);
    private(
        &registration,
        json!({
            "grok-bot":{"mailbox":"grok-bot","revoked":false},
            "fixture-peer":{"mailbox":"fixture-peer","revoked":false}
        })
        .to_string(),
    );
    private(
        &home.join("pigeonmq-cloud-grok-bot-policy.json"),
        json!({
            "principal":"grok-bot","mailbox":"grok-bot",
            "token_sha256":Sha256::digest(TOKEN).iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
            "operations":["read"],"enabled":true,"expires_at_ms":4102444800000_u64,
            "registration":registration
        })
        .to_string(),
    );
    let external_files = tempfile::tempdir().unwrap();
    fs::set_permissions(external_files.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let wake_dir = if explicit {
        external_files.path()
    } else {
        home
    };
    let manifest = explicit.then(|| {
        let path = home.join("profiles.json");
        private(
            &path,
            json!({"profiles":[{
                "adapter":"grok-bot", "route":"/grok/mcp",
                "policy":"pigeonmq-cloud-grok-bot-policy.json", "wake_files_dir":wake_dir
            }]})
            .to_string(),
        );
        path
    });
    // The real notifier rejects this non-HTTPS URL before any network request.
    private(&wake_dir.join(grok_wake::URL_FILE), WAKE_URL);
    private(&wake_dir.join(grok_wake::AUTH_FILE), WAKE_AUTH);
    let wake_state = home.join(grok_wake::STATE_FILE);
    let mut ledger = grok_wake::WakeState::load(wake_state.clone());
    ledger.delivered("previous-wake");
    // Recovery retries this recorded failure without waking unseen history.
    ledger.failed(&id, Some(503));
    ledger.save().unwrap();

    let state_path = home.join("pigeonmq-cloud-state.json");
    let wake_status = home.join(grok_wake::STATUS_FILE);
    let mut previous_host = Value::Null;
    let operator_socket = home.join("pigeonmq-cloud-health.sock");
    if explicit {
        // Model a previous abrupt exit: the pathname remains, but no one listens.
        drop(std::os::unix::net::UnixListener::bind(&operator_socket).unwrap());
    }
    for _ in 0..2 {
        let mut child = runner(home, port, manifest.as_deref(), explicit);
        let state = wait_for(&state_path, "local_ready");
        assert_eq!(state["public_origin"], ORIGIN);
        assert_eq!(state["remote_verified"], false);
        assert_eq!(state["local_port"], port);
        assert_ne!(state["host_pid"], previous_host);
        previous_host = state["host_pid"].clone();
        wait_for(&wake_status, "auth_blocked");
        if explicit {
            assert!(!wake_dir.join(grok_wake::STATUS_FILE).exists());
            assert!(!wake_dir.join(grok_wake::STATE_FILE).exists());
            assert!(db.with_extension("grok-wake.lock").is_file());
        }
        health(home, true);
        if explicit {
            assert_isolated_operator_health(home, &db, port, &id);
        } else {
            assert!(!operator_socket.exists());
        }
        assert_grok_requires_accept(port);

        let (status, response) = hub_http::post(
            port,
            "/grok/mcp",
            TOKEN,
            &json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"pigeonmq_read","arguments":{"message_id":id}}
            }),
        )
        .unwrap();
        assert_eq!(status, 200, "{response}");
        let message: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(message["id"], id);
        assert_eq!(message["body"], BODY);
        assert_eq!(message["state"], "pending");
        assert!(child.stop());
        assert!(!operator_socket.exists());
        health(home, false);
        fs::remove_file(&wake_status).unwrap();
        assert!(!grok_wake::WakeState::load(wake_state.clone()).due("previous-wake"));

        let stdout = fs::read_to_string(home.join("stdout")).unwrap();
        let stderr = fs::read_to_string(home.join("stderr")).unwrap();
        for sentinel in [ORIGIN, TOKEN, WAKE_URL, WAKE_AUTH, BODY, &id] {
            assert!(!stdout.contains(sentinel));
            assert!(!stderr.contains(sentinel));
        }
        let records: Vec<Value> = stderr
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            records
                .iter()
                .any(|row| row["fields"]["event"] == "wake_started")
        );
        assert!(
            records
                .iter()
                .any(|row| row["fields"]["error_code"] == "invalid_url")
        );
    }
}
