//! Receiver-level completion failure recovery, not a live native transport test.
//! Synthetic native responses are supplied once; SQLite failure and restart must
//! preserve the final result without producing another native-admission request.
use pigeonmq::{Envelope, Queue, adapters::codex::receiver::Binding};
mod receiver_support;
use receiver_support::open;
use rusqlite::Connection;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn completion_storage_failure_preserves_final_and_blocks_new_admission_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue");
    let mut queue = Queue::open(&db).unwrap();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let request = queue
        .send(Envelope {
            sender: "dot".into(),
            recipient: "codex-local".into(),
            body: "question".into(),
            correlation_id: Some("corr".into()),
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message;
    drop(queue);
    let registration = dir.path().join("registration.json");
    fs::write(
        &registration,
        r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#,
    )
    .unwrap();
    fs::set_permissions(&registration, fs::Permissions::from_mode(0o600)).unwrap();
    let binding = Binding {
        thread_id: "thread".into(),
        endpoint: dir.path().join("unused.sock"),
        cwd: dir.path().into(),
        approved_inputs: vec!["question".into()],
        registration,
    };
    let conn = Connection::open(&db).unwrap();
    {
        let mut receiver = open(&db, binding.clone()).unwrap();
        let params = receiver.claim().unwrap().unwrap();
        receiver.admitted(&params, &json!({"queuedSubmission": {
            "id":"admission", "clientUserMessageId":params["clientUserMessageId"], "input":params["input"]
        }})).unwrap();
        assert!(!receiver.notification(&json!({"method":"item/completed", "params": {
            "threadId":"thread", "turnId":"turn", "item": {"type":"userMessage", "clientId":params["clientUserMessageId"]}
        }})).unwrap());
        assert!(!receiver.notification(&json!({"method":"item/completed", "params": {
            "threadId":"thread", "turnId":"turn", "item": {"type":"agentMessage", "phase":"final_answer", "id":"answer", "text":"saved final"}
        }})).unwrap());
        assert!(
            receiver
                .notification(&json!({"method":"turn/completed", "params": {
                    "threadId":"thread", "turn": {"id":"turn", "status":"completed", "error":null}
                }}))
                .unwrap()
        );
        // Abort the ACK update, after reply insertion, exercising transaction rollback.
        conn.execute_batch("CREATE TRIGGER fail_ack BEFORE UPDATE OF acknowledged_at_ms ON messages WHEN NEW.acknowledged_at_ms IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END;").unwrap();
        assert_eq!(
            receiver.complete().unwrap_err(),
            "atomic_completion_requires_reconciliation"
        );
        assert!(receiver.has_pending());
        assert!(receiver.pending_completed());
        assert_eq!(
            receiver.claim().unwrap_err(),
            "pending_requires_reconciliation"
        );
    }
    let state: (i64, i64) = conn
        .query_row(
            "SELECT count(*), count(acknowledged_at_ms) FROM messages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, (1, 0));
    conn.execute_batch("DROP TRIGGER fail_ack").unwrap();
    let mut receiver = open(&db, binding).unwrap();
    assert!(receiver.pending_completed());
    assert_eq!(
        receiver.claim().unwrap_err(),
        "pending_requires_reconciliation"
    );
    // Explicit completion recovery only: no new admitted()/notification() calls.
    let reply = receiver.complete().unwrap();
    assert_eq!(reply.envelope.body, "saved final");
    assert_eq!(
        reply.envelope.in_reply_to.as_deref(),
        Some(request.id.as_str())
    );
    assert_eq!(
        reply.envelope.idempotency_key,
        Some(format!("gw-reply-{}", request.id))
    );
    assert!(!receiver.has_pending());
    assert_eq!(receiver.complete().unwrap_err(), "no_pending");
    let state: (i64, i64) = conn
        .query_row(
            "SELECT count(*), count(acknowledged_at_ms) FROM messages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, (2, 1));
}
