// Source: 999K e399161, src/codex_events_tests.rs (MIT), Cloud cases only.
use super::*;
use radio999k::{
    Envelope, Queue, events::delivery::EventDelivery, events::subscriptions::SubscriptionStore,
};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
const TOKEN: &str = "PUBLIC_CODEX_EVENTS_FIXTURE_NOT_A_CREDENTIAL";
fn put(path: &std::path::Path, value: &Value) {
    std::fs::write(path, value.to_string()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn context() -> Context<'static> {
    Context::new(Instant::now() + Duration::from_secs(20), &|| false)
}
fn secret() -> String {
    use base64::Engine;
    format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([7; 32])
    )
}
fn subscription(recipient: &str) -> Value {
    json!({"name":"pigeonmq.message.created","arguments":{"recipient":recipient},"delivery":{"mode":"webhook","url":"https://callback.example.com/events","secret":secret()}})
}
fn echo(_: &EventsPolicy, request: SignedCallback<'_>) -> Result<CallbackResponse, CallbackError> {
    let body: Value = serde_json::from_slice(request.body).unwrap();
    assert_eq!(body["type"], "verification");
    assert!(
        radio999k::events::callback_transport::signed_headers(&request)
            .unwrap()
            .contains_key("webhook-signature")
    );
    Ok(CallbackResponse {
        status: 200,
        body: serde_json::to_vec(&json!({"challenge":body["challenge"]})).unwrap(),
    })
}
struct Fixture {
    _dir: tempfile::TempDir,
    profile: Profile,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = Profile {
            hosted: None,
            identity: crate::codex_mcp::Identity::BuiltIn(
                radio999k::mailbox_catalog::codex().mailbox,
            ),
            db: dir.path().join("queue.db"),
            policy: dir.path().join("codex-policy.json"),
            registration: dir.path().join("registration.json"),
        };
        drop(Queue::open(&profile.db).unwrap());
        std::fs::set_permissions(&profile.db, std::fs::Permissions::from_mode(0o600)).unwrap();
        put(
            &profile.registration,
            &json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":false}}),
        );
        put(
            &profile.policy,
            &json!({"principals":[{"id":"codex-cloud","token_sha256":Sha256::digest(TOKEN).iter().map(|b|format!("{b:02x}")).collect::<String>(),"operations":["read","wait","reply","send"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}],"events":{"callback_hosts":["callback.example.com"],"resolver":std::env::current_exe().unwrap(),"allow_nonexpiring":false}}),
        );
        Self { _dir: dir, profile }
    }
    fn runtime(&self) -> Runtime {
        Runtime::new(&self.profile).unwrap().unwrap()
    }
    fn conn(&self) -> Connection {
        Connection::open(&self.profile.db).unwrap()
    }
    fn prepared(&self) -> Runtime {
        let r = self.runtime();
        r.prepare().unwrap();
        r.check().unwrap();
        r
    }
    fn send(&self, sender: &str, recipient: &str) -> String {
        Queue::open_existing(&self.profile.db)
            .unwrap()
            .send(Envelope {
                sender: sender.into(),
                recipient: recipient.into(),
                body: "fixture request".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id
    }
    fn state(&self, id: &str) -> (i64, Option<i64>) {
        self.conn()
            .query_row(
                "SELECT attempts,acknowledged_at_ms FROM messages WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
    fn rpc(&self, r: &Runtime, method: &str, params: Value) -> Value {
        self.profile
            .dispatch_with_events(
                &[format!("Bearer {TOKEN}").into_bytes()],
                &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                    .to_string()
                    .into_bytes(),
                Some(r),
                &context(),
            )
            .unwrap()
            .unwrap()
    }
    fn tool(&self, r: &Runtime, name: &str, args: Value) -> Value {
        let response = self.rpc(r, "tools/call", json!({"name":name,"arguments":args}));
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}
#[test]
fn discovery_and_preparation_are_explicit_and_preserve_dot_grants() {
    let f = Fixture::new();
    // Generic bearer policies accept either hex case; Events must preserve that authority.
    let mut policy: Value =
        serde_json::from_slice(&std::fs::read(&f.profile.policy).unwrap()).unwrap();
    policy["principals"][0]["token_sha256"] = policy["principals"][0]["token_sha256"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase()
        .into();
    put(&f.profile.policy, &policy);
    let r = f.runtime();
    assert!(r.check().is_err());
    assert_eq!(
        f.conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='dot_event_subscriptions'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    r.prepare().unwrap();
    let init=f.rpc(&r,"initialize",json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}));
    assert_eq!(init["result"]["capabilities"]["events"], json!({}));
    let catalog = f.rpc(&r, "events/list", json!({}));
    assert_eq!(
        catalog["result"]["events"][0]["inputSchema"]["properties"]["recipient"]["enum"],
        json!(["codex-cloud"])
    );
    assert!(
        r.event_with("wrong", "events/list", &json!({}), &context(), echo)
            .is_err()
    );
    assert!(
        r.event_with(
            TOKEN,
            "events/subscribe",
            &subscription("dot"),
            &context(),
            echo
        )
        .is_err()
    );
    let grant = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    let repeated = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    assert_eq!(grant["id"], repeated["id"]);
    assert!(
        r.event_with(
            TOKEN,
            "events/list",
            &json!({"recipient":"dot"}),
            &context(),
            echo
        )
        .is_err()
    );
    let policy = f.profile.policy.with_file_name("dot-policy.json");
    put(
        &policy,
        &json!({"principal":"dot-owner","token_sha256":Sha256::digest(TOKEN).iter().map(|b|format!("{b:02x}")).collect::<String>(),"mailbox":"dot","senders":["codex-cloud"],"callback_hosts":["callback.example.com"],"enabled":true}),
    );
    let dot = SubscriptionStore::new(&f.profile.db, &policy, false);
    dot.prepare("dot-owner", TOKEN).unwrap();
    let dg = dot
        .subscribe(
            "dot-owner",
            TOKEN,
            &subscription("dot"),
            events_runtime::wall_time,
            echo,
        )
        .unwrap();
    assert_ne!(grant["id"], dg["id"]);
    let code = f.send("dot", "codex-cloud");
    let other = f.send("codex-cloud", "dot");
    let mut seen = vec![];
    r.tick_with(&context(), |_, req| {
        let v: Value = serde_json::from_slice(req.body).unwrap();
        seen.push(v["data"]["message_id"].as_str().unwrap().to_owned());
        assert_eq!(v["data"]["recipient"], "codex-cloud");
        assert_eq!(v["data"]["conversation_id"], code);
        assert!(v["data"].get("body").is_none());
        Ok(CallbackResponse {
            status: 200,
            body: vec![],
        })
    })
    .unwrap();
    assert_eq!(seen, vec![code.clone()]);
    assert_eq!(f.state(&code), (0, None));
    assert_eq!(f.state(&other), (0, None));
    let dl = EventDelivery::new(&f.profile.db, &policy, false);
    let ids = dl.enqueue(events_runtime::wall_time).unwrap();
    assert_eq!(ids, vec![dg["id"].as_str().unwrap().to_owned()]);
    let attempt = dl
        .reserve(&ids[0], events_runtime::wall_time)
        .unwrap()
        .unwrap();
    assert_eq!(attempt.message_id(), other);
    assert_eq!(attempt.mailbox(), "dot");
    let mut unsub = subscription("codex-cloud");
    unsub["delivery"].as_object_mut().unwrap().remove("secret");
    r.event_with(TOKEN, "events/unsubscribe", &unsub, &context(), echo)
        .unwrap();
    let active: i64 = f
        .conn()
        .query_row(
            "SELECT active FROM dot_event_subscriptions WHERE id=?1",
            [dg["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(active, 1);
}
#[test]
fn idle_arrival_delivery_completion_and_terminal_confirmation_use_shared_queue() {
    let f = Fixture::new();
    let r = f.prepared();
    r.event_with(
        TOKEN,
        "events/subscribe",
        &subscription("codex-cloud"),
        &context(),
        echo,
    )
    .unwrap();
    r.tick_with(&context(), |_, _| panic!("empty inbox must not notify"))
        .unwrap();
    let id = f.send("dot", "codex-cloud");
    r.tick_with(&context(), |_, req| {
        assert_eq!(
            serde_json::from_slice::<Value>(req.body).unwrap()["data"]["message_id"],
            id
        );
        Ok(CallbackResponse {
            status: 200,
            body: vec![],
        })
    })
    .unwrap();
    assert_eq!(f.state(&id), (0, None));
    assert_eq!(
        f.tool(&r, "radio999k_read", json!({"message_id":id}))["id"],
        id
    );
    let reply = f.tool(
        &r,
        "radio999k_reply",
        json!({"message_id":id,"body":"synthetic handler result, not model inference"}),
    );
    assert_eq!(reply["state"], "replied");
    assert!(f.state(&id).1.is_some());
    let restarted = f.runtime();
    restarted.check().unwrap();
    restarted
        .tick_with(&context(), |_, _| panic!("accepted event must not replay"))
        .unwrap();
    let outgoing = f.send("codex-cloud", "dot");
    let original_id = outgoing.clone();
    let terminal = Queue::open_existing(&f.profile.db)
        .unwrap()
        .send(Envelope {
            sender: "dot".into(),
            recipient: "codex-cloud".into(),
            body: "terminal fixture".into(),
            correlation_id: Some(outgoing.clone()),
            in_reply_to: Some(outgoing),
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    r.tick_with(&context(), |_, req| {
        assert_eq!(
            serde_json::from_slice::<Value>(req.body).unwrap()["data"]["message_id"],
            terminal
        );
        Ok(CallbackResponse {
            status: 200,
            body: vec![],
        })
    })
    .unwrap();
    assert!(f.tool(&r, "radio999k_wait", json!({"timeout_ms":0}))["message"].is_null());
    assert_eq!(
        f.tool(
            &r,
            "radio999k_wait",
            json!({"timeout_ms":0,"in_reply_to":original_id})
        )["message"]["id"],
        terminal
    );
    f.tool(&r, "radio999k_read", json!({"message_id":terminal}));
    assert_eq!(f.state(&terminal), (0, None));
    let count: i64 = f
        .conn()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let confirmation = f.tool(
        &r,
        "radio999k_confirm_terminal",
        json!({"message_id":terminal}),
    );
    assert_eq!(confirmation["duplicate"], false);
    assert!(f.state(&terminal).1.is_some());
    assert!(
        f.tool(
            &r,
            "radio999k_wait",
            json!({"timeout_ms":0,"in_reply_to":original_id})
        )["message"]
            .is_null()
    );
    assert_eq!(f.state(&original_id), (0, None));
    assert_eq!(
        f.tool(
            &r,
            "radio999k_confirm_terminal",
            json!({"message_id":terminal})
        )["duplicate"],
        true
    );
    assert_eq!(
        f.conn()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        count
    );
    let denied = f.rpc(
        &r,
        "tools/call",
        json!({"name":"radio999k_confirm_terminal","arguments":{"message_id":id}}),
    );
    assert_eq!(denied["result"]["isError"], true);
}
#[test]
fn expiry_revocation_and_callback_failure_never_grant_or_ack() {
    let f = Fixture::new();
    let r = f.prepared();
    assert!(
        r.event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            |_, _| Ok(CallbackResponse {
                status: 200,
                body: br#"{"challenge":"wrong"}"#.to_vec()
            })
        )
        .is_err()
    );
    assert_eq!(
        f.conn()
            .query_row("SELECT count(*) FROM dot_event_subscriptions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let grant = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    let id = f.send("dot", "codex-cloud");
    f.conn()
        .execute(
            "UPDATE dot_event_subscriptions SET expires=1 WHERE id=?1",
            [grant["id"].as_str().unwrap()],
        )
        .unwrap();
    r.tick_with(&context(), |_, _| panic!("expired grant"))
        .unwrap();
    assert_eq!(f.state(&id), (0, None));
    r.event_with(
        TOKEN,
        "events/subscribe",
        &subscription("codex-cloud"),
        &context(),
        echo,
    )
    .unwrap();
    let mut policy: Value =
        serde_json::from_slice(&std::fs::read(&f.profile.policy).unwrap()).unwrap();
    policy["principals"][0]["revoked"] = json!(true);
    put(&f.profile.policy, &policy);
    assert!(
        r.tick_with(&context(), |_, _| panic!("revoked authority"))
            .is_err()
    );
    assert!(
        r.event_with(TOKEN, "events/list", &json!({}), &context(), echo)
            .is_err()
    );
    assert_eq!(f.state(&id), (0, None));
}
#[test]
fn changed_authority_during_verification_rolls_back_grant() {
    let f = Fixture::new();
    let r = f.prepared();
    let result = r.event_with(
        TOKEN,
        "events/subscribe",
        &subscription("codex-cloud"),
        &context(),
        |p, req| {
            let response = echo(p, req);
            let mut policy: Value =
                serde_json::from_slice(&std::fs::read(&f.profile.policy).unwrap()).unwrap();
            policy["principals"][0]["operations"] = json!(["send"]);
            put(&f.profile.policy, &policy);
            response
        },
    );
    assert!(result.is_err());
    assert_eq!(
        f.conn()
            .query_row("SELECT count(*) FROM dot_event_subscriptions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn worker_lifetime_and_native_lock_preserve_single_consumer() {
    let f = Fixture::new();
    let r = Arc::new(f.prepared());
    let occupied = InstanceLock::acquire_codex(&f.profile.db).unwrap();
    assert!(r.clone().start(None).is_err());
    drop(occupied);
    let worker = r
        .clone()
        .start(Some(Instant::now() + Duration::from_secs(1)))
        .unwrap();
    assert!(InstanceLock::acquire_codex(&f.profile.db).is_err());
    drop(worker);
    let free = InstanceLock::acquire_codex(&f.profile.db).unwrap();
    drop(free);
    let journal = PathBuf::from(format!("{}.codex-receiver.json", f.profile.db.display()));
    std::fs::write(journal, b"pending preserved").unwrap();
    let worker = r.clone().start(None).unwrap();
    drop(worker);
    f.conn().pragma_update(None, "user_version", 2).unwrap();
    let before = std::fs::read(&f.profile.db).unwrap();
    assert!(r.check().is_err());
    assert!(r.start(None).is_err());
    assert_eq!(std::fs::read(&f.profile.db).unwrap(), before);
}
#[test]
fn codex_retry_identity_survives_restart_and_old_grant_cannot_finish_new_one() {
    let f = Fixture::new();
    let r = f.prepared();
    let grant = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    let id = f.send("dot", "codex-cloud");
    let ledger = EventDelivery::with_policy(&f.profile.db, r.policy.clone(), false);
    let now = events_runtime::wall_time();
    ledger.enqueue(|| now).unwrap();
    let old = ledger
        .reserve(grant["id"].as_str().unwrap(), || now)
        .unwrap()
        .unwrap();
    ledger.finish(&old, 503).unwrap();
    assert!(
        ledger
            .reserve(grant["id"].as_str().unwrap(), || now)
            .unwrap()
            .is_none()
    );
    let restarted = EventDelivery::with_policy(&f.profile.db, f.runtime().policy, false);
    let retry = restarted
        .reserve(grant["id"].as_str().unwrap(), || now + 3.0)
        .unwrap()
        .unwrap();
    assert_eq!(old.event_id(), retry.event_id());
    r.event_with(
        TOKEN,
        "events/subscribe",
        &subscription("codex-cloud"),
        &context(),
        echo,
    )
    .unwrap();
    assert_eq!(
        restarted.finish(&retry, 410).unwrap(),
        radio999k::events::delivery::DeliveryOutcome::Superseded
    );
    assert_eq!(
        f.conn()
            .query_row(
                "SELECT active FROM dot_event_subscriptions WHERE id=?1",
                [grant["id"].as_str().unwrap()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(f.state(&id), (0, None));
}
#[test]
fn confirmation_rejects_claimed_terminal_and_revoked_sender() {
    let f = Fixture::new();
    let r = f.prepared();
    let outgoing = f.send("codex-cloud", "dot");
    let terminal = Queue::open_existing(&f.profile.db)
        .unwrap()
        .send(Envelope {
            sender: "dot".into(),
            recipient: "codex-cloud".into(),
            body: "answer".into(),
            correlation_id: Some(outgoing.clone()),
            in_reply_to: Some(outgoing),
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    let mut q = Queue::open_existing(&f.profile.db).unwrap();
    q.receive("codex-cloud", Duration::from_secs(30)).unwrap();
    assert!(
        q.confirm_terminal_checked("codex-cloud", &terminal, |_| Ok(()))
            .is_err()
    );
    assert!(f.state(&terminal).1.is_none());
    f.conn()
        .execute(
            "UPDATE messages SET attempts=0,lease_until_ms=NULL,receipt=NULL WHERE id=?1",
            params![terminal],
        )
        .unwrap();
    let mut registration: Value =
        serde_json::from_slice(&std::fs::read(&f.profile.registration).unwrap()).unwrap();
    registration["dot"]["revoked"] = json!(true);
    put(&f.profile.registration, &registration);
    let denied = f.rpc(
        &r,
        "tools/call",
        json!({"name":"radio999k_confirm_terminal","arguments":{"message_id":terminal}}),
    );
    assert_eq!(denied["result"]["isError"], true);
    assert!(f.state(&terminal).1.is_none());
}

#[test]
fn revoking_all_peers_stops_delivery_but_preserves_owner_unsubscribe() {
    let f = Fixture::new();
    let r = f.prepared();
    let grant = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    let id = f.send("dot", "codex-cloud");
    put(
        &f.profile.registration,
        &json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":true}}),
    );
    r.tick_with(&context(), |_, _| panic!("revoked peer must not deliver"))
        .unwrap();
    let mut unsub = subscription("codex-cloud");
    unsub["delivery"].as_object_mut().unwrap().remove("secret");
    let unsubscribed = f.rpc(&r, "events/unsubscribe", unsub);
    assert_eq!(unsubscribed["result"], json!({}), "{unsubscribed}");
    assert_eq!(f.state(&id), (0, None));
    assert_eq!(
        f.conn()
            .query_row(
                "SELECT active FROM dot_event_subscriptions WHERE id=?1",
                [grant["id"].as_str().unwrap()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn inline_discovery_then_exact_read_preserves_queue_state() {
    let f = Fixture::new();
    let r = f.prepared();
    let id = f.send("dot", "codex-cloud");
    let before = f.state(&id);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}});
    let discovered = f.rpc(&r, "server/discover", json!({"_meta":meta}));
    assert_eq!(discovered["result"]["resultType"], "complete");
    assert_eq!(discovered["result"]["capabilities"]["events"], json!({}));
    for method in ["tools/list", "events/list"] {
        let response = f.rpc(&r, method, json!({"_meta":meta}));
        assert!(response.get("result").is_some(), "{response}");
    }
    let read = f.rpc(
        &r,
        "tools/call",
        json!({"_meta":meta,"name":"radio999k_read","arguments":{"message_id":id}}),
    );
    let body: Value =
        serde_json::from_str(read["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["id"], id);
    assert_eq!(f.state(&id), before);
    for method in ["server/discover", "events/subscribe"] {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
            "io.modelcontextprotocol/protocolVersion":"2099-01-01"
        }}})
        .to_string();
        assert_eq!(
            f.profile.dispatch_with_events(
                &[format!("Bearer {TOKEN}").into_bytes()],
                body.as_bytes(),
                Some(&r),
                &context(),
            ),
            Err((400, "unsupported_protocol_version")),
        );
    }
    assert_eq!(f.state(&id), before);
}

#[test]
fn callback_diagnostics_follow_persisted_failure_transitions() {
    use radio999k::events::delivery::DeliveryOutcome;

    // A parallel test can register a tracing callsite before this thread sees it,
    // caching no interest despite the thread-local subscriber below.
    const CHILD: &str = "PIGEONMQ_TEST_CODEX_CALLBACK_DIAGNOSTICS";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "codex_events::tests::callback_diagnostics_follow_persisted_failure_transitions",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed;"),
            "isolated callback diagnostics check failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let f = Fixture::new();
    let r = f.prepared();
    let grant = r
        .event_with(
            TOKEN,
            "events/subscribe",
            &subscription("codex-cloud"),
            &context(),
            echo,
        )
        .unwrap();
    let sid = grant["id"].as_str().unwrap();
    let ledger = EventDelivery::with_policy(&f.profile.db, r.policy.clone(), false);
    let trace = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::WARN)
        .with_writer(trace.reopen().unwrap())
        .finish();
    let pending = |id: &str| {
        f.conn()
            .query_row(
                "SELECT state,attempts FROM dot_event_outbox WHERE message_id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
    };

    let exhausted = f.send("dot", "codex-cloud");
    let rejected;
    let uncertain;
    {
        let _capture = tracing::subscriber::set_default(subscriber);
        r.tick_with(&context(), |_, request| {
            assert!(!request.body.is_empty());
            Err(CallbackError("callback_timeout"))
        })
        .unwrap();
        assert_eq!(pending(&exhausted), ("pending".into(), 1));
        let now = events_runtime::wall_time();
        for number in 2..=5 {
            let attempt = ledger
                .reserve(sid, || now + f64::from(number) * 40.0)
                .unwrap()
                .unwrap();
            assert_eq!(attempt.attempt(), i64::from(number));
            assert_eq!(
                ledger.finish(&attempt, 503).unwrap(),
                DeliveryOutcome::Applied
            );
            if number == 5 {
                assert_eq!(
                    ledger.finish(&attempt, 503).unwrap(),
                    DeliveryOutcome::Superseded
                );
            }
        }
        assert_eq!(pending(&exhausted), ("stopped".into(), 5));

        rejected = f.send("dot", "codex-cloud");
        r.tick_with(&context(), |_, _| {
            Ok(CallbackResponse {
                status: 403,
                body: b"PUBLIC_CALLBACK_BODY_SENTINEL".to_vec(),
            })
        })
        .unwrap();
        assert_eq!(pending(&rejected), ("stopped".into(), 1));

        uncertain = f.send("dot", "codex-cloud");
        ledger.enqueue(|| now).unwrap();
        for number in 1..=5 {
            let attempt = ledger
                .reserve(sid, || now + f64::from(number) * 40.0)
                .unwrap()
                .unwrap();
            assert_eq!(attempt.attempt(), i64::from(number));
            if number < 5 {
                ledger.finish(&attempt, 503).unwrap();
            }
        }
        assert_eq!(pending(&uncertain), ("pending".into(), 5));
        assert!(ledger.reserve(sid, || now + 225.0).unwrap().is_none());
        assert_eq!(pending(&uncertain), ("pending".into(), 5));
        for _ in 0..2 {
            assert!(ledger.reserve(sid, || now + 240.0).unwrap().is_none());
        }
        assert_eq!(pending(&uncertain), ("stopped".into(), 5));
    }

    let text = std::fs::read_to_string(trace.path()).unwrap();
    let fields: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["fields"].clone())
        .filter(|fields| fields["component"] == "events_callback")
        .collect();
    assert_eq!(
        fields,
        [
            json!({"component":"events_callback","event":"failure","outcome":"retry_scheduled","attempt":1,"decision_status":503}),
            json!({"component":"events_callback","event":"failure","outcome":"retry_exhausted","attempt":5,"decision_status":503}),
            json!({"component":"events_callback","event":"failure","outcome":"rejected","attempt":1,"decision_status":403}),
            json!({"component":"events_callback","event":"failure","outcome":"retry_scheduled","attempt":1,"decision_status":503}),
            json!({"component":"events_callback","event":"failure","outcome":"reservation_exhausted","count":1}),
        ],
    );
    for private in [
        sid,
        exhausted.as_str(),
        rejected.as_str(),
        uncertain.as_str(),
        TOKEN,
        &secret(),
        "callback.example.com",
        "PUBLIC_CALLBACK_BODY_SENTINEL",
        "fixture request",
    ] {
        assert!(!text.contains(private));
    }
    for id in [exhausted, rejected, uncertain] {
        assert_eq!(f.state(&id), (0, None));
    }
}
