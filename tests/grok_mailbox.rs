use radio999k::{Envelope, Queue, grok_mailbox as m};
use rusqlite::{Connection, params};
use std::{fs::Permissions, os::unix::fs::PermissionsExt};

#[test]
fn request_and_reply_accept_16384_utf8_bytes_and_reject_one_more() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    let mut queue = Queue::open(&path).unwrap();
    m::prepare(&path).unwrap();
    let scope = || m::GrokScope {
        allowed_senders: vec!["alice".into()],
    };
    let body = "é".repeat(8192);
    assert_eq!(body.len(), 16384);
    let id = queue
        .send(Envelope {
            sender: "alice".into(),
            recipient: "grok-bot".into(),
            body: body.clone(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    assert_eq!(m::read(&path, &id, &scope()).unwrap().body, body);
    assert_eq!(m::scan(&path, &scope()).unwrap().unwrap().body, body);
    let db = Connection::open(&path).unwrap();
    let oversized = format!("{body}x");
    db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params![oversized, id],
    )
    .unwrap();
    assert_eq!(
        m::read(&path, &id, &scope()).unwrap_err().reason,
        "invalid_body"
    );
    db.execute("UPDATE messages SET body=? WHERE id=?", params![body, id])
        .unwrap();
    assert_eq!(
        m::reply(&path, &id, &oversized, || Ok(scope()))
            .unwrap_err()
            .reason,
        "invalid_body"
    );
    let untouched: (i64, Option<i64>) = db
        .query_row(
            "SELECT attempts,acknowledged_at_ms FROM messages WHERE id=?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(untouched, (0, None));
    let first = m::reply(&path, &id, &body, || Ok(scope())).unwrap();
    let retry = m::reply(&path, &id, &body, || Ok(scope())).unwrap();
    assert!(retry.duplicate);
    assert_eq!(first.reply_id, retry.reply_id);
}

#[test]
fn grok_wake_accepts_pending_ordinary_and_valid_terminal_in_the_same_scope() {
    let dir = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let db = dir.path().join("queue.db");
    let mut queue = Queue::open(&db).unwrap();
    m::prepare(&db).unwrap();
    let scope = m::GrokScope {
        allowed_senders: vec!["alice".into()],
    };
    let envelope = |sender: &str, recipient: &str| Envelope {
        sender: sender.into(),
        recipient: recipient.into(),
        body: "synthetic fixture".into(),
        correlation_id: None,
        in_reply_to: None,
        idempotency_key: None,
    };
    let ordinary = queue.send(envelope("alice", "grok-bot")).unwrap().message;
    let parent = queue.send(envelope("grok-bot", "alice")).unwrap().message;
    let mut answer = envelope("alice", "grok-bot");
    answer.in_reply_to = Some(parent.id.clone());
    answer.correlation_id = Some(parent.id.clone());
    let terminal = queue
        .reply_unclaimed_checked("alice", &parent.id, answer, || Ok(()))
        .unwrap();
    let denied = queue.send(envelope("mallory", "grok-bot")).unwrap().message;
    let ids = radio999k::grok_wake::pending(&db, i64::MAX).unwrap();
    for (id, conversation) in [
        (&ordinary.id, &ordinary.conversation_id),
        (&terminal.id, &parent.conversation_id),
    ] {
        assert!(ids.contains(id));
        assert_eq!(
            m::wake_candidate(&db, id, &scope).unwrap(),
            Some(("alice".into(), conversation.clone()))
        );
        let body = radio999k::grok_wake::wake_body(id, conversation);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"message_id":id,"conversation_id":conversation,"mailbox":"grok-bot"})
        );
    }
    assert!(m::wake_candidate(&db, &denied.id, &scope).is_err());
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE messages SET acknowledged_at_ms=1 WHERE id=?",
        [&terminal.id],
    )
    .unwrap();
    assert_eq!(m::wake_candidate(&db, &terminal.id, &scope).unwrap(), None);
    conn.execute(
        "UPDATE messages SET acknowledged_at_ms=NULL, correlation_id='wrong' WHERE id=?",
        [&terminal.id],
    )
    .unwrap();
    assert!(m::wake_candidate(&db, &terminal.id, &scope).is_err());
    let state = dir.path().join(radio999k::grok_wake::STATE_FILE);
    let mut ledger = radio999k::grok_wake::WakeState::load(state.clone());
    ledger.delivered(&ordinary.id);
    ledger.delivered(&terminal.id);
    ledger.save().unwrap();
    let restarted = radio999k::grok_wake::WakeState::load(state);
    assert!(!restarted.due(&ordinary.id));
    assert!(!restarted.due(&terminal.id));
    assert!(restarted.due(&denied.id));
}
