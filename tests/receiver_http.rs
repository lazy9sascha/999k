//! Synthetic main-binary HTTP coverage. No native agent or external connection.
use pigeonmq::{
    Delivery, Envelope, Message, Queue, hub_http,
    receiver_api::{Action, Request},
    runtime_admission::Record,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const TOKEN: &str = "synthetic-receiver-http-only";
const OTHER_TOKEN: &str = "synthetic-other-owner-only";

fn digest(value: &impl serde::Serialize) -> String {
    Sha256::digest(serde_json::to_vec(value).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Hub {
    child: Child,
    port: u16,
    db: PathBuf,
    _home: tempfile::TempDir,
}
impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Hub {
    fn start(receiver: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let db = home.path().join("queue.sqlite3");
        drop(Queue::open(&db).unwrap());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let policy = home.path().join("policy.json");
        let principals: Vec<Value> = [("receiver-test", TOKEN), ("other-owner", OTHER_TOKEN)]
            .into_iter()
            .map(|(id, token)| {
                json!({
                    "id": id, "token_sha256": Sha256::digest(token).iter()
                        .map(|byte| format!("{byte:02x}")).collect::<String>(),
                    "operations": ["receive", "reply"], "mailboxes": ["codex-local"],
                    "send_to": [], "revoked": false,
                })
            })
            .collect();
        fs::write(&policy, json!({"principals": principals}).to_string()).unwrap();
        let registry = home.path().join("registration.json");
        fs::write(
            &registry,
            json!({
                "codex-local": {"mailbox": "codex-local", "revoked": false},
                "peer": {"mailbox": "peer", "revoked": false},
            })
            .to_string(),
        )
        .unwrap();
        for path in [&policy, &registry] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        command
            .env_clear()
            .args(["serve-hub", "--queue"])
            .arg(&db)
            .args(["--port", &port.to_string(), "--seconds", "30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if receiver {
            let manifest = home.path().join("profiles.json");
            pigeonmq::private_file::replace_private(
                &manifest,
                &serde_json::to_vec(&json!({"registration":registry,"profiles":[
                    {"adapter":"codex-local","route":"/receiver","policy":policy}
                ]}))
                .unwrap(),
            )
            .unwrap();
            command.arg("--profiles-file").arg(manifest);
        } else {
            command.arg("--auth-policy").arg(&policy);
        }
        let mut hub = Self {
            child: command.spawn().unwrap(),
            port,
            db,
            _home: home,
        };
        let stdout = hub.child.stdout.take().unwrap();
        let (sent, received) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).unwrap();
            let _ = sent.send(line);
        });
        let line = received
            .recv_timeout(Duration::from_secs(5))
            .expect("hub did not report readiness");
        reader.join().unwrap();
        let ready: Value = serde_json::from_str(&line).expect("invalid startup record");
        assert_eq!(ready["transport"], "hub-local-http");
        assert_eq!(ready["receiver_enabled"], receiver);
        assert_eq!(ready["generic_enabled"], !receiver);
        hub
    }
    fn send(&self) -> Message {
        Queue::open_existing(&self.db)
            .unwrap()
            .send(Envelope {
                sender: "peer".into(),
                recipient: "codex-local".into(),
                body: "synthetic receiver wire request".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
    }
    fn request(action: Action) -> Request {
        Request {
            recipient: "codex-local".into(),
            session_id: "selected-session".into(),
            profile_id: "synthetic-profile".into(),
            action,
        }
    }
    fn post(&self, token: &str, request: &Request) -> (u16, Value) {
        hub_http::post(
            self.port,
            "/receiver",
            token,
            &serde_json::to_value(request).unwrap(),
        )
        .unwrap()
    }
    fn call(&self, action: Action) -> Value {
        let (status, value) = self.post(TOKEN, &Self::request(action));
        assert_eq!(status, 200, "{value}");
        value
    }
    fn stored(&self, id: &str) -> (i64, bool, i64) {
        Connection::open(&self.db).unwrap().query_row(
            "SELECT attempts,acknowledged_at_ms IS NOT NULL,(SELECT count(*) FROM messages WHERE in_reply_to=?1) FROM messages WHERE id=?1",
            [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap()
    }
}

#[test]
fn explicit_receiver_http_preserves_authority_receipts_and_atomic_completion() {
    let hub = Hub::start(true);
    let original = hub.send();
    let claim = Hub::request(Action::Claim { lease_seconds: 30 });
    assert_eq!(hub.post("wrong-synthetic-token", &claim).0, 401);
    let mut wrong_mailbox = claim.clone();
    wrong_mailbox.recipient = "peer".into();
    assert_eq!(hub.post(TOKEN, &wrong_mailbox).0, 403);
    assert_eq!(hub.stored(&original.id), (0, false, 0));

    let wire_delivery = hub.call(claim.action);
    assert_eq!(wire_delivery["message"]["message_id"], original.id);
    assert_eq!(wire_delivery["message"]["conversation_id"], original.id);
    let delivery: Delivery = serde_json::from_value(wire_delivery).unwrap();
    assert_eq!(delivery.message.id, original.id);
    assert_eq!(delivery.attempt, 1);
    assert_eq!(hub.stored(&original.id), (1, false, 0));
    let renewed = hub.call(Action::Renew {
        message_id: original.id.clone(),
        receipt: delivery.receipt.clone(),
        lease_seconds: 120,
    });
    assert!(renewed.as_i64().unwrap() > delivery.lease_until_ms);

    let mut record: Record = serde_json::from_value(hub.call(Action::Begin {
        message_id: original.id.clone(),
        receipt: delivery.receipt.clone(),
        payload_digest: "a".repeat(64),
    }))
    .unwrap();
    assert_eq!(record.principal_id, "receiver-test");
    let load = Hub::request(Action::Load {
        message_id: original.id.clone(),
        receipt: delivery.receipt.clone(),
    });
    for (token, session) in [(OTHER_TOKEN, "selected-session"), (TOKEN, "other-session")] {
        let mut wrong_owner = load.clone();
        wrong_owner.session_id = session.into();
        let (status, value) = hub.post(token, &wrong_owner);
        assert_eq!(status, 403, "{value}");
        assert_eq!(value["error"]["code"], "runtime_owner_mismatch");
    }
    let premature = Hub::request(Action::Complete {
        message_id: original.id.clone(),
        receipt: delivery.receipt.clone(),
        body: Some("answer".into()),
        failure: false,
    });
    assert_ne!(hub.post(TOKEN, &premature).0, 200);
    assert_eq!(hub.stored(&original.id), (1, false, 0));

    record.admission_id = Some("synthetic-admission".into());
    record.stage = "admitted".into();
    record = serde_json::from_value(hub.call(Action::Update {
        receipt: delivery.receipt.clone(),
        record: record.into(),
    }))
    .unwrap();
    record.stage = "model_completed".into();
    record.answer_digest = Some(digest(&"answer"));
    hub.call(Action::Update {
        receipt: delivery.receipt.clone(),
        record: record.into(),
    });
    assert_eq!(hub.stored(&original.id), (1, false, 0));
    let (status, value) = hub.post(TOKEN, &premature);
    assert_eq!(status, 200, "{value}");
    let reply: Message = serde_json::from_value(value).unwrap();
    assert_eq!(reply.conversation_id, original.id);
    assert_eq!(reply.envelope.sender, "codex-local");
    assert_eq!(reply.envelope.recipient, "peer");
    assert_eq!(reply.envelope.body, "answer");
    assert_eq!(
        reply.envelope.in_reply_to.as_deref(),
        Some(original.id.as_str())
    );
    assert_eq!(
        reply.envelope.correlation_id.as_deref(),
        Some(original.id.as_str())
    );
    assert_eq!(hub.stored(&original.id), (1, true, 1));
    let (status, value) = hub.post(TOKEN, &premature);
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        serde_json::from_value::<Message>(value).unwrap().id,
        reply.id
    );
    assert_eq!(hub.stored(&original.id), (1, true, 1));
}

#[test]
fn disabled_receiver_does_not_fall_back_to_generic_bearer_policy() {
    let hub = Hub::start(false);
    let message = hub.send();
    let (status, _) = hub.post(TOKEN, &Hub::request(Action::Claim { lease_seconds: 30 }));
    assert_eq!(status, 404);
    assert_eq!(hub.stored(&message.id), (0, false, 0));
}
