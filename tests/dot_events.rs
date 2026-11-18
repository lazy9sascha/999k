//! Offline fixtures only. Callback transport is injected and never reaches a provider.
use base64::Engine;
use pigeonmq::{
    Envelope, Queue,
    dot_profile::{self, DotBackend, DotHost, events::Runtime},
    events::{
        callback_transport::{CallbackError, CallbackResponse, SignedCallback, signed_headers},
        policy::EventsPolicy,
        runtime::InstanceLock,
    },
    mcp_request::Context,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
const TOKEN: &str = "PUBLIC_DOT_EVENTS_FIXTURE_NOT_A_CREDENTIAL";
fn put(p: &Path, v: &Value) {
    std::fs::write(p, v.to_string()).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn context() -> Context<'static> {
    Context::new(Instant::now() + Duration::from_secs(20), &|| false)
}
fn rpc(host: &DotHost, token: &str, method: &str, params: Value) -> (u16, Value) {
    let authority = "127.0.0.1:54843";
    let headers = [
        ("Host".into(), authority.as_bytes().to_vec()),
        ("Content-Type".into(), b"application/json".to_vec()),
        (
            "Accept".into(),
            b"application/json, text/event-stream".to_vec(),
        ),
    ];
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
    match host.dispatch_streamable_http(
        (authority, "/dot/mcp"),
        &headers,
        token,
        body.as_bytes(),
        &context(),
    ) {
        Ok(response) => (
            response.status().as_u16(),
            serde_json::from_slice(response.body()).unwrap(),
        ),
        Err((status, reason)) => (status, json!({"error":reason})),
    }
}
fn grant(recipient: &str) -> Value {
    json!({"name":"pigeonmq.message.created","arguments":{"recipient":recipient},"delivery":{"mode":"webhook","url":"https://callback.example.com/events","secret":format!("whsec_{}",base64::engine::general_purpose::STANDARD.encode([7;32]))}})
}
fn echo(_: &EventsPolicy, req: SignedCallback<'_>) -> Result<CallbackResponse, CallbackError> {
    assert!(
        signed_headers(&req)
            .unwrap()
            .contains_key("webhook-signature")
    );
    let b: Value = serde_json::from_slice(req.body).unwrap();
    assert_eq!(b["type"], "verification");
    Ok(CallbackResponse {
        status: 200,
        body: serde_json::to_vec(&json!({"challenge":b["challenge"]})).unwrap(),
    })
}
struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    policy: PathBuf,
    registry: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        let policy = dir.path().join("dot.json");
        let registry = dir.path().join("registry.json");
        drop(Queue::open(&db).unwrap());
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        put(
            &registry,
            &json!({"dot":{"mailbox":"dot","revoked":false},"muse":{"mailbox":"muse","revoked":false},"grok-bot":{"mailbox":"grok-bot","revoked":false}}),
        );
        put(
            &policy,
            &json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot","enabled":true,"token_sha256":Sha256::digest(TOKEN).iter().map(|b| format!("{b:02x}")).collect::<String>(),"registration":registry,"events":{"callback_hosts":["callback.example.com"],"resolver":std::env::current_exe().unwrap()}}),
        );
        Self {
            _dir: dir,
            db,
            policy,
            registry,
        }
    }
    fn runtime(&self) -> Runtime {
        Runtime::new(&self.db, &self.policy).unwrap().unwrap()
    }
    fn prepared(&self) -> Runtime {
        dot_profile::check(&self.db, &self.policy, true).unwrap();
        let r = self.runtime();
        r.check().unwrap();
        r
    }
    fn conn(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }
    fn envelope(sender: &str, recipient: &str) -> Envelope {
        Envelope {
            sender: sender.into(),
            recipient: recipient.into(),
            body: "synthetic fixture body".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        }
    }
    fn send(&self, sender: &str, recipient: &str) -> String {
        Queue::open_existing(&self.db)
            .unwrap()
            .send(Self::envelope(sender, recipient))
            .unwrap()
            .message
            .id
    }
    fn terminal(&self) -> (String, String) {
        let parent = self.send("dot", "muse");
        let mut e = Self::envelope("muse", "dot");
        e.in_reply_to = Some(parent.clone());
        e.correlation_id = Some(parent.clone());
        e.idempotency_key = Some(format!("reply-{parent}"));
        let id = Queue::open_existing(&self.db)
            .unwrap()
            .reply_unclaimed_checked("muse", &parent, e, || Ok(()))
            .unwrap()
            .id;
        (parent, id)
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
}
#[test]
fn dot_inbox_pages_exact_read_metadata_without_claiming_or_acknowledging() {
    let f = Fixture::new();
    f.prepared();
    let host = DotBackend::host(f.db.clone(), f.policy.clone());
    let first = f.send("muse", "dot");
    f.send("muse", "elsewhere");
    f.send("unregistered", "dot");
    let (_, terminal) = f.terminal();
    let (_, tools) = rpc(&host, TOKEN, "tools/list", json!({}));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "pigeonmq_inbox")
    );

    let list = |token, args| {
        rpc(
            &host,
            token,
            "tools/call",
            json!({"name":"pigeonmq_inbox","arguments":args}),
        )
    };
    let data = |response: Value| {
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str::<Value>(response["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap()
    };
    assert_eq!(list("wrong", json!({})).0, 401);
    let page = data(list(TOKEN, json!({"limit":1})).1);
    assert_eq!(page["metadata_only"], true);
    assert_eq!(page["messages"][0]["id"], first);
    assert_eq!(page["messages"][0]["state"], "pending");
    assert!(page["messages"][0].get("body").is_none());
    let next = data(list(TOKEN, json!({"continuation":page["next_continuation"]})).1);
    assert_eq!(next["messages"].as_array().unwrap().len(), 1);
    assert_eq!(next["messages"][0]["id"], terminal);
    assert_eq!(next["messages"][0]["state"], "reply_received");
    assert!(next["messages"][0].get("body").is_none());
    assert!(next["next_continuation"].is_null());
    assert_eq!(f.state(&first), (0, None));
    assert_eq!(f.state(&terminal), (0, None));
    assert!(
        list(TOKEN, json!({"recipient":"elsewhere"}))
            .1
            .get("error")
            .is_some()
    );

    put(
        &f.registry,
        &json!({"dot":{"mailbox":"dot","revoked":false},
        "muse":{"mailbox":"muse","revoked":true}}),
    );
    assert_eq!(data(list(TOKEN, json!({})).1)["messages"], json!([]));
}
#[test]
fn dot_event_auth_discovery_and_exact_recipient_are_fail_closed() {
    let f = Fixture::new();
    let r = Arc::new(f.prepared());
    let host = DotBackend::host_with_events(f.db.clone(), f.policy.clone(), Some(r.clone()));
    assert_eq!(rpc(&host, "wrong", "events/list", json!({})).0, 401);
    let (status, d) = rpc(
        &host,
        TOKEN,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(status, 200);
    assert_eq!(d["result"]["capabilities"]["events"], json!({}));
    let (_, defs) = rpc(&host, TOKEN, "events/list", json!({}));
    assert_eq!(
        defs["result"]["events"][0]["inputSchema"]["properties"]["recipient"]["enum"],
        json!(["dot"])
    );
    assert_eq!(
        defs["result"]["events"][0]["payloadSchema"]["properties"]["terminal"]["const"],
        true
    );
    for (token, recipient) in [("wrong", "dot"), (TOKEN, "codex-cloud"), (TOKEN, "muse")] {
        assert!(
            r.event_with(
                token,
                "events/subscribe",
                &grant(recipient),
                &context(),
                |_, _| panic!("unauthorized callback")
            )
            .is_err()
        );
    }
    let mut foreign = grant("dot");
    foreign["delivery"]["url"] = json!("https://other.example.com/events");
    assert!(
        r.event_with(
            TOKEN,
            "events/subscribe",
            &foreign,
            &context(),
            |_, _| panic!("unapproved callback host")
        )
        .is_err()
    );
    let g = r
        .event_with(TOKEN, "events/subscribe", &grant("dot"), &context(), echo)
        .unwrap();
    let owner: String = f
        .conn()
        .query_row(
            "SELECT owner FROM dot_event_subscriptions WHERE id=?1",
            [g["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner, "pigeonmq-cloud-dot");
    let expires: f64 = f
        .conn()
        .query_row(
            "SELECT expires FROM dot_event_subscriptions WHERE owner='pigeonmq-cloud-dot'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(expires > pigeonmq::events::runtime::wall_time());

    put(
        &f.registry,
        &json!({"dot":{"mailbox":"dot","revoked":true},"muse":{"mailbox":"muse","revoked":false}}),
    );
    assert!(
        r.event_with(
            TOKEN,
            "events/list",
            &json!({}),
            &context(),
            |_, _| panic!()
        )
        .is_err()
    );
    assert_ne!(
        rpc(&host, TOKEN, "events/list", json!({})).1["error"],
        Value::Null
    );
}
#[test]
fn only_terminal_replies_wake_with_signed_metadata_and_survive_restart_without_ack() {
    let f = Fixture::new();
    let r = f.prepared();
    let ordinary = f.send("muse", "dot");
    f.send("muse", "grok-bot");
    let (_, old_reply) = f.terminal();
    r.event_with(TOKEN, "events/subscribe", &grant("dot"), &context(), echo)
        .unwrap();
    r.tick_with(&context(), |_, _| panic!("old terminal reply delivered"))
        .unwrap();
    let cutoff: i64 = f
        .conn()
        .query_row("SELECT after_seq FROM dot_subscription_cutoffs", [], |r| {
            r.get(0)
        })
        .unwrap();
    let (parent, reply) = f.terminal();
    let (_, acked) = f.terminal();
    f.conn()
        .execute(
            "UPDATE messages SET acknowledged_at_ms=1 WHERE id=?1",
            [acked],
        )
        .unwrap();
    let (_, invalid) = f.terminal();
    f.conn()
        .execute(
            "UPDATE messages SET correlation_id='wrong' WHERE id=?1",
            [invalid],
        )
        .unwrap();
    // Refresh retains the initial boundary and does not drop the new pending reply.
    r.event_with(TOKEN, "events/subscribe", &grant("dot"), &context(), echo)
        .unwrap();
    assert_eq!(
        f.conn()
            .query_row("SELECT after_seq FROM dot_subscription_cutoffs", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        cutoff
    );
    let mut count = 0;
    r.tick_with(&context(),|_,req| {count+=1;assert!(signed_headers(&req).unwrap().contains_key("webhook-signature"));let b:Value=serde_json::from_slice(req.body).unwrap();assert_eq!(b["data"],json!({"recipient":"dot","message_id":reply,"conversation_id":parent,"sender":"muse","correlation_id":parent,"in_reply_to":parent,"terminal":true}));assert!(!String::from_utf8_lossy(req.body).contains("synthetic fixture body"));Ok(CallbackResponse{status:200,body:vec![]})}).unwrap();
    assert_eq!(count, 1);
    assert_eq!(f.state(&reply), (0, None));
    assert_eq!(f.state(&ordinary), (0, None));
    assert_eq!(f.state(&old_reply), (0, None));
    assert_eq!(
        f.conn()
            .query_row(
                "SELECT count(*) FROM dot_event_outbox WHERE message_id=?1",
                [&old_reply],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert!(f.state(&parent).1.is_some());
    drop(r);
    f.runtime()
        .tick_with(&context(), |_, _| {
            panic!("delivered reply repeated after restart")
        })
        .unwrap();
    let host = DotBackend::host(f.db.clone(), f.policy.clone());
    let (status, result) = rpc(
        &host,
        TOKEN,
        "tools/call",
        json!({"name":"pigeonmq_reply","arguments":{"message_id":reply,"body":"must not auto reply"}}),
    );
    assert_eq!(status, 200);
    assert_eq!(result["error"]["data"]["reason"], "reply_is_terminal");
    assert_eq!(f.state(&reply), (0, None));
}
#[test]
fn events_are_opt_in_expiring_and_use_a_separate_dot_lock() {
    let f = Fixture::new();
    let r = f.prepared();
    let worker = Arc::new(r)
        .start(Some(Instant::now() + Duration::from_secs(2)))
        .unwrap();
    assert!(InstanceLock::acquire(&f.db).is_err());
    let codex = InstanceLock::acquire_codex(&f.db).unwrap();
    drop(worker);
    drop(codex);
    let mut p: Value = serde_json::from_slice(&std::fs::read(&f.policy).unwrap()).unwrap();
    p["principal"] = json!("codex-cloud");
    put(&f.policy, &p);
    assert!(Runtime::new(&f.db, &f.policy).is_err());
    p["principal"] = json!("pigeonmq-cloud-dot");
    p.as_object_mut().unwrap().remove("events");
    put(&f.policy, &p);
    assert!(Runtime::new(&f.db, &f.policy).unwrap().is_none());
    let host = DotBackend::host(f.db.clone(), f.policy.clone());
    let (_, response) = rpc(&host, TOKEN, "events/list", json!({}));
    assert_eq!(response["error"]["data"]["reason"], "events_disabled");
    let (status, response) = rpc(
        &host,
        TOKEN,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    assert_eq!(status, 200);
    assert!(response["result"]["capabilities"].get("events").is_none());
}

#[test]
fn invalid_events_preparation_makes_no_schema_change() {
    let f = Fixture::new();
    let mut policy: Value = serde_json::from_slice(&std::fs::read(&f.policy).unwrap()).unwrap();
    policy["events"]["resolver"] = json!("relative-resolver");
    put(&f.policy, &policy);
    assert!(dot_profile::check(&f.db, &f.policy, true).is_err());
    let count: i64 = f
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'dot_event_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    let old = Fixture::new();
    let runtime = Arc::new(old.prepared());
    old.conn().pragma_update(None, "user_version", 2).unwrap();
    let before = std::fs::read(&old.db).unwrap();
    assert_eq!(
        runtime.check().unwrap_err().reason,
        "requires_current_schema_queue"
    );
    assert!(runtime.start(None).is_err());
    assert_eq!(std::fs::read(&old.db).unwrap(), before);
}

#[test]
fn fresh_cutoff_is_fail_closed_and_reactivation_starts_fresh() {
    let f = Fixture::new();
    let r = f.prepared();
    let subscribe = || {
        r.event_with(TOKEN, "events/subscribe", &grant("dot"), &context(), echo)
            .unwrap()
    };
    let grant_result = subscribe();
    let sid = grant_result["id"].as_str().unwrap();
    assert_eq!(
        f.conn()
            .query_row("SELECT after_seq FROM dot_subscription_cutoffs", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    let (_, old) = f.terminal();
    f.conn()
        .execute("UPDATE dot_event_subscriptions SET expires=1", [])
        .unwrap();
    subscribe();
    // A stale pending item cannot block a newer eligible reply at reservation.
    f.conn().execute("INSERT INTO dot_event_outbox(subscription_id,message_id,event_id) VALUES (?1,?2,'stale-fixture')", rusqlite::params![sid,old]).unwrap();
    let (_, fresh) = f.terminal();
    let mut delivered = vec![];
    r.tick_with(&context(), |_, req| {
        let v: Value = serde_json::from_slice(req.body).unwrap();
        delivered.push(v["data"]["message_id"].as_str().unwrap().to_owned());
        Ok(CallbackResponse {
            status: 200,
            body: vec![],
        })
    })
    .unwrap();
    assert_eq!(delivered, vec![fresh]);
    assert_eq!(f.state(&old), (0, None));
    let mut unsubscribe = grant("dot");
    unsubscribe["delivery"]
        .as_object_mut()
        .unwrap()
        .remove("secret");
    r.event_with(
        TOKEN,
        "events/unsubscribe",
        &unsubscribe,
        &context(),
        |_, _| panic!(),
    )
    .unwrap();
    let (_, gap) = f.terminal();
    subscribe();
    r.tick_with(&context(), |_, _| panic!("replayed inactive interval"))
        .unwrap();
    assert_eq!(f.state(&gap), (0, None));
    let (_, pending) = f.terminal();
    // Corruption or absence must not turn fresh-only into backlog delivery.
    f.conn()
        .execute("UPDATE dot_subscription_cutoffs SET after_seq=-1", [])
        .unwrap();
    assert!(
        r.tick_with(&context(), |_, _| panic!("invalid cutoff callback"))
            .is_err()
    );
    f.conn()
        .execute("DELETE FROM dot_subscription_cutoffs", [])
        .unwrap();
    assert!(
        r.tick_with(&context(), |_, _| panic!("missing cutoff callback"))
            .is_err()
    );
    assert_eq!(f.state(&pending), (0, None));
}
