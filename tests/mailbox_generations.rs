use pigeonmq::{
    AckResult, Envelope, Error, Message, Outcome, Queue, RpcState,
    conversation::{ThreadRead, ThreadScope},
    inbox_page, mailbox_generation,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const LEASE: Duration = Duration::from_secs(60);

fn fixture() -> (tempfile::TempDir, PathBuf, Queue) {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("queue.sqlite3");
    let queue = Queue::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    (dir, path, queue)
}

fn envelope(sender: &str, recipient: &str, key: &str, body: &str) -> Envelope {
    Envelope {
        sender: sender.into(),
        recipient: recipient.into(),
        body: body.into(),
        correlation_id: None,
        in_reply_to: None,
        idempotency_key: Some(key.into()),
    }
}

fn answer(original: &Message, body: &str) -> Envelope {
    Envelope {
        sender: original.envelope.recipient.clone(),
        recipient: original.envelope.sender.clone(),
        body: body.into(),
        correlation_id: Some(
            original
                .envelope
                .correlation_id
                .clone()
                .unwrap_or_else(|| original.id.clone()),
        ),
        in_reply_to: Some(original.id.clone()),
        idempotency_key: Some(format!("reply-{}", original.id)),
    }
}

fn thread(db: &Path, mailbox: &str, peer: &str, conversation: &str) -> Vec<String> {
    let peers = [peer.into()];
    Queue::read_thread(
        db,
        ThreadScope {
            mailbox,
            peers: &peers,
            request_bytes: 8192,
        },
        &ThreadRead {
            conversation_id: conversation.into(),
            cursor: None,
        },
    )
    .unwrap()
    .messages
    .into_iter()
    .map(|message| message.message_id)
    .collect()
}

fn inbox(db: &Path, mailbox: &str, peer: &str, arguments: Value) -> Vec<String> {
    let queue = Queue::open_existing(db).unwrap();
    let page = inbox_page::list_with(
        db,
        mailbox,
        &arguments,
        Instant::now() + Duration::from_secs(5),
        || Ok(vec![peer.into()]),
        || Ok(()),
        |id, _| match queue.ordinary_metadata(id, mailbox) {
            Ok(value) => Ok(Some(value)),
            Err(Error::NotFound) => Ok(None),
            Err(_) => Err((503, "fixture_read_failed")),
        },
    )
    .unwrap();
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["id"].as_str().unwrap().to_owned())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct StoredMessage {
    id: String,
    conversation_id: String,
    body: String,
    parent: Option<String>,
    key: Option<String>,
    created_at_ms: i64,
    receipt: Option<String>,
    lease_until_ms: Option<i64>,
    attempts: i64,
    acknowledged_at_ms: Option<i64>,
}

fn stored(db: &Path, id: &str) -> StoredMessage {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT id,conversation_id,body,in_reply_to,idempotency_key,created_at_ms,
             receipt,lease_until_ms,attempts,acknowledged_at_ms FROM main.messages WHERE id=?",
            [id],
            |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    body: row.get(2)?,
                    parent: row.get(3)?,
                    key: row.get(4)?,
                    created_at_ms: row.get(5)?,
                    receipt: row.get(6)?,
                    lease_until_ms: row.get(7)?,
                    attempts: row.get(8)?,
                    acknowledged_at_ms: row.get(9)?,
                })
            },
        )
        .unwrap()
}

#[test]
fn same_public_mailbox_name_gets_new_history_without_erasing_old_rows() {
    let (_dir, db, mut queue) = fixture();
    let incoming = envelope("peer", "cue", "incoming", "old incoming body");
    let outgoing = envelope("cue", "peer", "outgoing", "old outgoing body");
    let old_incoming = queue.send(incoming.clone()).unwrap().message;
    let old_outgoing = queue.send(outgoing.clone()).unwrap().message;
    let old_linked = queue
        .reply_unclaimed_checked(
            "peer",
            &old_outgoing.id,
            answer(&old_outgoing, "old linked body"),
            || Ok(()),
        )
        .unwrap();
    let other = queue
        .send(envelope("another", "peer", "unrelated", "unrelated body"))
        .unwrap()
        .message;
    assert_eq!(
        thread(&db, "cue", "peer", &old_outgoing.conversation_id),
        vec![old_outgoing.id.clone(), old_linked.id.clone()],
    );
    assert_eq!(
        inbox(&db, "cue", "peer", json!({"scope":"all","query":"old"})).len(),
        3,
    );
    let old_data = [&old_incoming, &old_outgoing, &old_linked].map(|item| stored(&db, &item.id));

    mailbox_generation::retire(&db, "cue").unwrap();
    assert!(queue.send(incoming.clone()).is_err());
    assert!(queue.send(outgoing.clone()).is_err());
    mailbox_generation::reactivate(&db, "cue").unwrap();

    // Use an already-open handle and separately reopened readers.
    for (item, recipient) in [
        (&old_incoming, "cue"),
        (&old_outgoing, "peer"),
        (&old_linked, "cue"),
    ] {
        assert!(matches!(
            queue.observe_ordinary(&item.id, recipient),
            Err(Error::NotFound)
        ));
    }
    assert!(queue.scan_ordinary("cue", 0, None).unwrap().is_empty());
    assert!(
        queue
            .scan_ordinary("cue", 0, Some(&old_outgoing.id))
            .unwrap()
            .is_empty()
    );
    assert!(queue.receive("cue", LEASE).unwrap().is_none());
    assert!(
        queue
            .confirm_terminal_checked("cue", &old_linked.id, |_| Ok(()))
            .is_err()
    );
    assert!(
        queue
            .reply_unclaimed_checked(
                "cue",
                &old_incoming.id,
                answer(&old_incoming, "stale completion"),
                || Ok(()),
            )
            .is_err()
    );
    for (mailbox, peer) in [("cue", "peer"), ("peer", "cue")] {
        assert!(thread(&db, mailbox, peer, &old_outgoing.conversation_id).is_empty());
        assert!(inbox(&db, mailbox, peer, json!({"scope":"all","query":"old"})).is_empty());
    }
    assert_eq!(queue.observe_ordinary(&other.id, "peer").unwrap(), other);

    let fresh_incoming = queue.send(incoming.clone()).unwrap();
    let fresh_outgoing = queue.send(outgoing.clone()).unwrap();
    assert!(!fresh_incoming.duplicate && !fresh_outgoing.duplicate);
    assert_ne!(fresh_incoming.message.id, old_incoming.id);
    assert_ne!(fresh_outgoing.message.id, old_outgoing.id);
    assert_eq!(fresh_incoming.message.envelope, incoming);
    assert_eq!(fresh_outgoing.message.envelope, outgoing);
    assert!(queue.send(incoming).unwrap().duplicate);
    assert_eq!(
        queue.receive("cue", LEASE).unwrap().unwrap().message,
        fresh_incoming.message,
    );
    assert_eq!(
        thread(&db, "cue", "peer", &fresh_outgoing.message.conversation_id),
        vec![fresh_outgoing.message.id.clone()],
    );
    assert_eq!(
        inbox(&db, "cue", "peer", json!({"scope":"all","query":"old"})).len(),
        2,
    );
    assert_eq!(
        [&old_incoming, &old_outgoing, &old_linked].map(|item| stored(&db, &item.id)),
        old_data,
    );
}

#[test]
fn old_receipts_and_rpc_results_cannot_cross_a_generation_boundary() {
    let (_dir, db, mut queue) = fixture();
    let ordinary = envelope("peer", "instinct", "leased", "leased ordinary");
    let request = envelope("peer", "instinct", "pending-rpc", "pending RPC");
    let answered = envelope("instinct", "peer", "answered-rpc", "answered RPC");
    let old_ordinary = queue.send(ordinary.clone()).unwrap().message;
    let old_claim = queue.receive("instinct", LEASE).unwrap().unwrap();
    let old_request = queue.request(request.clone()).unwrap().message;
    let old_request_claim = queue.receive_request("instinct", LEASE).unwrap().unwrap();
    let old_answered = queue.request(answered.clone()).unwrap().message;
    let answered_claim = queue.receive_request("peer", LEASE).unwrap().unwrap();
    let outcome = Outcome::Success {
        value: json!({"complete":true}),
    };
    queue
        .respond(
            "peer",
            &old_answered.id,
            &answered_claim.receipt,
            outcome.clone(),
        )
        .unwrap();
    assert_eq!(
        queue.status("instinct", &old_answered.id).unwrap().result,
        Some(outcome.clone()),
    );
    let snapshots = [&old_ordinary, &old_request, &old_answered].map(|item| stored(&db, &item.id));

    mailbox_generation::retire(&db, "instinct").unwrap();
    mailbox_generation::reactivate(&db, "instinct").unwrap();
    drop(queue);
    let mut queue = Queue::open_existing(&db).unwrap();

    assert!(
        queue
            .ack("instinct", &old_ordinary.id, &old_claim.receipt)
            .is_err()
    );
    assert!(
        queue
            .renew_lease("instinct", &old_ordinary.id, &old_claim.receipt, LEASE)
            .is_err()
    );
    assert!(
        queue
            .reply_and_ack(
                "instinct",
                &old_ordinary.id,
                &old_claim.receipt,
                answer(&old_ordinary, "stale leased completion"),
            )
            .is_err()
    );
    assert!(queue.receive_request("instinct", LEASE).unwrap().is_none());
    assert!(queue.status("peer", &old_request.id).is_err());
    assert!(queue.status("instinct", &old_answered.id).is_err());
    assert!(
        queue
            .respond(
                "instinct",
                &old_request.id,
                &old_request_claim.receipt,
                outcome.clone()
            )
            .is_err()
    );

    let current = queue.send(ordinary).unwrap();
    assert!(!current.duplicate);
    let claim = queue.receive("instinct", LEASE).unwrap().unwrap();
    assert_eq!(claim.message, current.message);
    assert_eq!(
        queue
            .ack("instinct", &current.message.id, &claim.receipt)
            .unwrap(),
        AckResult::Acknowledged,
    );
    let current_request = queue.request(request).unwrap();
    let current_answered = queue.request(answered).unwrap();
    assert!(!current_request.duplicate && !current_answered.duplicate);
    assert_ne!(current_request.message.id, old_request.id);
    assert_ne!(current_answered.message.id, old_answered.id);
    let claim = queue.receive_request("peer", LEASE).unwrap().unwrap();
    assert_eq!(claim.message.id, current_answered.message.id);
    queue
        .respond("peer", &claim.message.id, &claim.receipt, outcome.clone())
        .unwrap();
    let status = queue.status("instinct", &claim.message.id).unwrap();
    assert_eq!(status.state, RpcState::Answered);
    assert_eq!(status.result, Some(outcome));
    assert_eq!(status.request.envelope.sender, "instinct");
    assert_eq!(status.request.envelope.recipient, "peer");

    assert_eq!(
        [&old_ordinary, &old_request, &old_answered].map(|item| stored(&db, &item.id)),
        snapshots,
    );
    let conn = Connection::open(&db).unwrap();
    let stored_result: String = conn
        .query_row(
            "SELECT outcome_json FROM main.rpc_requests WHERE message_id=?",
            params![old_answered.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Outcome>(&stored_result).unwrap(),
        Outcome::Success {
            value: json!({"complete":true})
        },
    );
    let schema: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(schema, 4);
}

#[test]
fn legacy_queue_remains_usable_and_refuses_unsafe_retirement() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("legacy.sqlite3");
    let queue = pigeonmq::Queue::open(&db).unwrap();
    drop(queue);
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("DROP TABLE pigeonmq_mailbox_generations")
        .unwrap();
    drop(connection);
    let mut queue = pigeonmq::Queue::open_existing(&db).unwrap();
    let sent = queue
        .send(pigeonmq::Envelope {
            sender: "fixture-sender".into(),
            recipient: "fixture-recipient".into(),
            body: "legacy fixture".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message;
    let before = std::fs::read(&db).unwrap();
    assert!(pigeonmq::mailbox_generation::retire(&db, "fixture-recipient").is_err());
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(
        queue
            .observe_ordinary(&sent.id, "fixture-recipient")
            .unwrap(),
        sent
    );
}
