//! Shared-engine characterization through the existing adapter APIs.
use pigeonmq::{Envelope, Queue, grok_mailbox as grok, muse_mailbox as muse};
use serde_json::Value;
use std::path::Path;

fn prepare(db: &Path, mailbox: &str) {
    if mailbox == "muse" {
        muse::prepare(db).unwrap();
    } else {
        grok::prepare(db).unwrap();
    }
}
fn read(db: &Path, mailbox: &str, id: &str) -> Result<Value, &'static str> {
    if mailbox == "muse" {
        muse::read(
            db,
            id,
            &muse::MuseScope {
                allowed_senders: vec!["peer".into()],
            },
        )
        .map(|value| serde_json::to_value(value).unwrap())
        .map_err(|e| e.reason)
    } else {
        grok::read(
            db,
            id,
            &grok::GrokScope {
                allowed_senders: vec!["peer".into()],
            },
        )
        .map(|value| serde_json::to_value(value).unwrap())
        .map_err(|e| e.reason)
    }
}
fn reply(
    db: &Path,
    mailbox: &str,
    id: &str,
    mut authorize: impl FnMut() -> Result<Vec<String>, &'static str>,
) -> Result<Value, &'static str> {
    if mailbox == "muse" {
        muse::reply(db, id, "answer", || {
            authorize()
                .map(|allowed_senders| muse::MuseScope { allowed_senders })
                .map_err(muse::MailboxError::new)
        })
        .map(|value| serde_json::to_value(value).unwrap())
        .map_err(|e| e.reason)
    } else {
        grok::reply(db, id, "answer", || {
            authorize()
                .map(|allowed_senders| grok::GrokScope { allowed_senders })
                .map_err(grok::MailboxError::new)
        })
        .map(|value| serde_json::to_value(value).unwrap())
        .map_err(|e| e.reason)
    }
}
fn send(queue: &mut Queue, recipient: &str) -> String {
    queue
        .send(Envelope {
            sender: "peer".into(),
            recipient: recipient.into(),
            body: "request".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id
}

#[test]
fn agents_share_queue_but_not_messages_or_reply_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    let mut queue = Queue::open(&db).unwrap();
    for mailbox in ["muse", "grok-bot"] {
        prepare(&db, mailbox);
    }
    let muse_id = send(&mut queue, "muse");
    let grok_id = send(&mut queue, "grok-bot");
    for (mailbox, id, other) in [
        ("muse", &muse_id, &grok_id),
        ("grok-bot", &grok_id, &muse_id),
    ] {
        assert_eq!(
            read(&db, mailbox, other).unwrap_err(),
            "message_not_available"
        );
        assert_eq!(read(&db, mailbox, id).unwrap()["recipient"], mailbox);
        let first = reply(&db, mailbox, id, || Ok(vec!["peer".into()])).unwrap();
        let replay = reply(&db, mailbox, id, || Ok(vec!["peer".into()])).unwrap();
        assert_eq!(first["reply_id"], replay["reply_id"]);
        assert_eq!(first["duplicate"], false);
        assert_eq!(replay["duplicate"], true);
    }
    let conn = rusqlite::Connection::open(&db).unwrap();
    let muse_bound: String = conn
        .query_row("SELECT message_id FROM muse_mailbox_replies", [], |r| {
            r.get(0)
        })
        .unwrap();
    let grok_bound: String = conn
        .query_row("SELECT message_id FROM grok_mailbox_replies", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(muse_bound, muse_id);
    assert_eq!(grok_bound, grok_id);
}

#[test]
fn existing_scope_differences_and_unbounded_peer_counts_remain() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queue.db");
    drop(Queue::open(&db).unwrap());
    for mailbox in ["muse", "grok-bot"] {
        prepare(&db, mailbox);
    }
    assert_eq!(
        muse::scan(
            &db,
            &muse::MuseScope {
                allowed_senders: vec!["muse".into()]
            }
        )
        .unwrap_err()
        .reason,
        "forbidden"
    );
    assert!(
        grok::scan(
            &db,
            &grok::GrokScope {
                allowed_senders: vec!["grok-bot".into()]
            }
        )
        .unwrap()
        .is_none()
    );
    for peers in [vec![], (0..64).map(|i| format!("peer-{i}")).collect()] {
        assert!(
            muse::scan(
                &db,
                &muse::MuseScope {
                    allowed_senders: peers.clone()
                }
            )
            .unwrap()
            .is_none()
        );
        assert!(
            grok::scan(
                &db,
                &grok::GrokScope {
                    allowed_senders: peers
                }
            )
            .unwrap()
            .is_none()
        );
    }
}

#[test]
fn both_agents_roll_back_reply_and_ack_on_final_authority_failure() {
    for mailbox in ["muse", "grok-bot"] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let mut queue = Queue::open(&db).unwrap();
        prepare(&db, mailbox);
        let id = send(&mut queue, mailbox);
        let mut checks = 0;
        assert_eq!(
            reply(&db, mailbox, &id, || {
                checks += 1;
                if checks == 3 {
                    Err("forbidden")
                } else {
                    Ok(vec!["peer".into()])
                }
            })
            .unwrap_err(),
            "forbidden"
        );
        assert_eq!(checks, 3);
        assert_eq!(read(&db, mailbox, &id).unwrap()["state"], "pending");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE in_reply_to=?",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            reply(&db, mailbox, &id, || Ok(vec!["peer".into()])).unwrap()["duplicate"],
            false
        );
    }
}

// These distinct implementations fail to compile if adapter types become aliases.
trait AdapterOwned {}
impl AdapterOwned for muse::MailboxError {}
impl AdapterOwned for grok::MailboxError {}
impl AdapterOwned for muse::ObservedMessage {}
impl AdapterOwned for grok::ObservedMessage {}
impl AdapterOwned for muse::Completion {}
impl AdapterOwned for grok::Completion {}
impl AdapterOwned for muse::ScanPage {}
impl AdapterOwned for grok::ScanPage {}
#[test]
fn each_adapter_keeps_its_nominal_public_types() {
    fn accepts<T: AdapterOwned>() {}
    accepts::<muse::MailboxError>();
    accepts::<grok::MailboxError>();
    accepts::<muse::ObservedMessage>();
    accepts::<grok::ObservedMessage>();
    accepts::<muse::Completion>();
    accepts::<grok::Completion>();
    accepts::<muse::ScanPage>();
    accepts::<grok::ScanPage>();
}
