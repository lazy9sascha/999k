use pigeonmq::{
    Envelope, Queue,
    muse_mailbox::{self as m, MailboxError, MuseScope},
};
use rusqlite::{Connection, params};
use std::path::PathBuf;
struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: Connection,
}
fn scope() -> MuseScope {
    MuseScope {
        allowed_senders: vec!["alice".into()],
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        drop(Queue::open(&path).unwrap());
        assert!(!m::is_prepared(&path).unwrap());
        m::prepare(&path).unwrap();
        assert!(m::is_prepared(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        Self {
            _dir: dir,
            path,
            db,
        }
    }
    fn send(
        &self,
        sender: &str,
        recipient: &str,
        parent: Option<String>,
        correlation: Option<String>,
        rpc: bool,
    ) -> String {
        let mut q = Queue::open(&self.path).unwrap();
        let e = Envelope {
            sender: sender.into(),
            recipient: recipient.into(),
            body: "question".into(),
            correlation_id: correlation,
            in_reply_to: parent,
            idempotency_key: None,
        };
        if rpc { q.request(e) } else { q.send(e) }
            .unwrap()
            .message
            .id
    }
    fn request(&self) -> String {
        self.send("alice", "muse", None, None, false)
    }
    fn reply(&self, id: &str, text: &str) -> m::Result<m::Completion> {
        m::reply(&self.path, id, text, || Ok(scope()))
    }
    fn count(&self) -> i64 {
        self.db
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }
    fn untouched(&self, id: &str) {
        let row: (i64, Option<String>, Option<i64>) = self
            .db
            .query_row(
                "SELECT attempts,receipt,acknowledged_at_ms FROM messages WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (0, None, None));
    }
}
#[test]
fn observation_never_advances_or_claims() {
    let f = Fixture::new();
    let first = f.request();
    let second = f.request();
    for _ in 0..2 {
        assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().id, first);
    }
    assert_eq!(
        m::read(&f.path, &second, &scope()).unwrap().state,
        "pending"
    );
    f.untouched(&first);
    f.untouched(&second);
    f.reply(&first, "answer").unwrap();
    assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().id, second);
}
#[test]
fn atomic_reply_exact_replay_and_conflict() {
    let f = Fixture::new();
    let id = f.request();
    let a = f.reply(&id, "answer").unwrap();
    let b = f.reply(&id, "answer").unwrap();
    assert!(!a.duplicate);
    assert!(b.duplicate);
    assert_eq!(a.reply_id, b.reply_id);
    assert_eq!(
        f.reply(&id, "different").unwrap_err().reason,
        "reply_conflict"
    );
    assert_eq!(f.count(), 2);
    let row: (String,String,String,String,String) = f.db.query_row("SELECT sender,recipient,correlation_id,in_reply_to,idempotency_key FROM messages WHERE id=?",[a.reply_id],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert_eq!(
        row,
        (
            "muse".into(),
            "alice".into(),
            id.clone(),
            id.clone(),
            format!("gw-reply-{id}")
        )
    );
    assert_eq!(m::read(&f.path, &id, &scope()).unwrap().state, "replied");
}
#[test]
fn rpc_sender_mailbox_and_unrelated_replies_are_excluded() {
    let f = Fixture::new();
    for id in [
        f.send("alice", "muse", None, None, true),
        f.send("mallory", "muse", None, None, false),
        f.send("alice", "dot", None, None, false),
        f.send("alice", "muse", Some("missing".into()), None, false),
    ] {
        assert_eq!(
            m::read(&f.path, &id, &scope()).unwrap_err().reason,
            "message_not_available"
        );
        assert_eq!(
            f.reply(&id, "answer").unwrap_err().reason,
            "message_not_available"
        );
        f.untouched(&id);
    }
    assert!(m::scan(&f.path, &scope()).unwrap().is_none());
}
#[test]
fn terminal_observation_does_not_ack_or_reply() {
    let f = Fixture::new();
    let parent = f.send("muse", "alice", None, None, false);
    let id = f.send("alice", "muse", Some(parent.clone()), Some(parent), false);
    assert_eq!(
        m::read(&f.path, &id, &scope()).unwrap().state,
        "reply_received"
    );
    assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().id, id);
    assert_eq!(
        f.reply(&id, "answer").unwrap_err().reason,
        "reply_is_terminal"
    );
    f.untouched(&id);
}
#[test]
fn expired_claims_and_orphan_receipts_require_reconciliation() {
    for mutation in [
        "attempts=1,lease_until_ms=0",
        "receipt='synthetic-receipt'",
        "lease_until_ms=9223372036854775807",
    ] {
        let f = Fixture::new();
        let id = f.request();
        f.db.execute(&format!("UPDATE messages SET {mutation} WHERE id=?"), [&id])
            .unwrap();
        assert!(m::scan(&f.path, &scope()).unwrap().is_none());
        assert_eq!(
            m::read(&f.path, &id, &scope()).unwrap().state,
            "reconciliation_required"
        );
        assert_eq!(
            f.reply(&id, "answer").unwrap_err().reason,
            "existing_claim_requires_reconciliation"
        );
        assert_eq!(f.count(), 1);
    }
}
#[test]
fn authorization_is_rechecked_under_write_lock_including_replays() {
    for completed in [false, true] {
        let f = Fixture::new();
        let id = f.request();
        if completed {
            f.reply(&id, "answer").unwrap();
        }
        let before = f.count();
        let mut calls = 0;
        let result = m::reply(&f.path, &id, "answer", || {
            calls += 1;
            if calls == 2 {
                let c = Connection::open(&f.path).unwrap();
                c.busy_timeout(std::time::Duration::ZERO).unwrap();
                assert!(c.execute_batch("BEGIN IMMEDIATE").is_err());
                return Err(MailboxError::new("forbidden"));
            }
            Ok(scope())
        });
        assert_eq!(result.unwrap_err().reason, "forbidden");
        assert_eq!(calls, 2);
        assert_eq!(f.count(), before);
    }
}
#[test]
fn changed_sender_scope_after_lock_blocks_completion() {
    let f = Fixture::new();
    let id = f.request();
    let mut calls = 0;
    let e = m::reply(&f.path, &id, "answer", || {
        calls += 1;
        Ok(if calls == 1 {
            scope()
        } else {
            MuseScope {
                allowed_senders: vec!["bob".into()],
            }
        })
    })
    .unwrap_err();
    assert_eq!(e.reason, "message_not_available");
    f.untouched(&id);
}
#[test]
fn storage_failure_rolls_back_reply_and_ack() {
    let f = Fixture::new();
    let id = f.request();
    f.db.execute_batch("CREATE TRIGGER stop_binding BEFORE INSERT ON muse_mailbox_replies BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
    assert_eq!(
        f.reply(&id, "answer").unwrap_err().reason,
        "storage_unavailable"
    );
    assert_eq!(f.count(), 1);
    f.untouched(&id);
}
#[test]
fn prior_unmatched_reply_requires_reconciliation() {
    let f = Fixture::new();
    let id = f.request();
    f.send("muse", "alice", Some(id.clone()), Some(id.clone()), false);
    assert_eq!(
        f.reply(&id, "answer").unwrap_err().reason,
        "reconciliation_required"
    );
    assert_eq!(f.count(), 2);
    f.untouched(&id);
}
#[test]
fn input_and_scope_bounds_are_enforced() {
    let f = Fixture::new();
    let id = f.request();
    for body in [" ".to_owned(), "\u{1c}".to_owned(), "é".repeat(8193)] {
        assert_eq!(f.reply(&id, &body).unwrap_err().reason, "invalid_body");
    }
    for scope in [
        MuseScope {
            allowed_senders: vec!["muse".into()],
        },
        MuseScope {
            allowed_senders: vec!["bad sender".into()],
        },
    ] {
        assert_eq!(m::scan(&f.path, &scope).unwrap_err().reason, "forbidden");
    }
    assert_eq!(
        m::read(&f.path, "bad id", &scope()).unwrap_err().reason,
        "invalid_identifier"
    );
    f.db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params!["é".repeat(8193), id],
    )
    .unwrap();
    assert!(m::scan(&f.path, &scope()).unwrap().is_none());
    f.untouched(&id);
}
#[test]
fn prepare_requires_existing_marked_queue() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("missing.db");
    assert!(m::prepare(&p).is_err());
    assert!(!p.exists());
    let c = Connection::open(&p).unwrap();
    assert_eq!(
        m::prepare(&p).unwrap_err().reason,
        "requires_current_schema_queue"
    );
    let count: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn bounded_pages_skip_malformed_rows_without_hiding_pending_requests() {
    let f = Fixture::new();
    for _ in 0..101 {
        let id = f.request();
        f.db.execute("UPDATE messages SET body='' WHERE id=?", [id])
            .unwrap();
    }
    let good = f.request();
    let first = m::scan_page(&f.path, &scope(), 0, false).unwrap();
    assert_eq!(first.scanned, m::SCAN_PAGE_SIZE);
    assert!(first.message.is_none());
    let second = m::scan_page(&f.path, &scope(), first.next_seq, false).unwrap();
    assert_eq!(second.scanned, 2);
    assert_eq!(second.message.unwrap().id, good);
    f.untouched(&good);
    // Each new observation starts over, rather than accepting previous progress.
    let again = m::scan_page(&f.path, &scope(), 0, false).unwrap();
    assert_eq!(again.next_seq, first.next_seq);
    assert_eq!(again.scanned, 100);
    assert!(m::scan(&f.path, &scope()).unwrap().is_none());
    assert_eq!(
        m::scan_page(&f.path, &scope(), -1, false)
            .unwrap_err()
            .reason,
        "invalid_scan_position"
    );
}

#[test]
fn pending_request_precedes_older_terminal_and_terminal_repeats_without_ack() {
    let f = Fixture::new();
    let parent = f.send("muse", "alice", None, None, false);
    let terminal = f.send("alice", "muse", Some(parent.clone()), Some(parent), false);
    let pending = f.request();
    assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().id, pending);
    assert_eq!(
        m::scan_page(&f.path, &scope(), 0, false)
            .unwrap()
            .message
            .unwrap()
            .id,
        pending
    );
    f.reply(&pending, "answer").unwrap();
    for _ in 0..2 {
        assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().id, terminal);
        assert_eq!(
            m::scan_page(&f.path, &scope(), 0, true)
                .unwrap()
                .message
                .unwrap()
                .id,
            terminal
        );
        f.untouched(&terminal);
    }
}
#[test]
fn raw_page_bound_applies_before_mailbox_and_rpc_filters() {
    let f = Fixture::new();
    for i in 0..100 {
        if i % 2 == 0 {
            f.send("alice", "elsewhere", None, None, false);
        } else {
            f.send("alice", "muse", None, None, true);
        }
    }
    let pending = f.request();
    let first = m::scan_page(&f.path, &scope(), 0, false).unwrap();
    assert_eq!(first.scanned, 100);
    assert!(first.message.is_none());
    let second = m::scan_page(&f.path, &scope(), first.next_seq, false).unwrap();
    assert_eq!(second.scanned, 1);
    assert_eq!(second.message.unwrap().id, pending);
    f.untouched(&pending);
}

#[test]
fn final_authorization_failure_rolls_back_new_completion_and_blocks_replay() {
    for completed in [false, true] {
        for revoke_sender in [false, true] {
            let f = Fixture::new();
            let id = f.request();
            if completed {
                f.reply(&id, "answer").unwrap();
            }
            let before = f.count();
            let mut calls = 0;
            let error = m::reply(&f.path, &id, "answer", || {
                calls += 1;
                if calls == 3 {
                    if revoke_sender {
                        return Ok(MuseScope {
                            allowed_senders: vec!["bob".into()],
                        });
                    }
                    return Err(MailboxError::new("forbidden"));
                }
                Ok(scope())
            })
            .unwrap_err();
            assert_eq!(calls, 3);
            assert_eq!(
                error.reason,
                if revoke_sender {
                    "message_not_available"
                } else {
                    "forbidden"
                }
            );
            assert_eq!(f.count(), before);
            if !completed {
                f.untouched(&id);
                let bindings: i64 =
                    f.db.query_row("SELECT count(*) FROM muse_mailbox_replies", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(bindings, 0);
            }
        }
    }
}

#[test]
fn request_and_reply_accept_16384_utf8_bytes_and_reject_one_more() {
    let f = Fixture::new();
    let id = f.request();
    let answer = "é".repeat(8192);
    assert_eq!(answer.len(), 16384);
    assert_eq!(
        f.reply(&id, &format!("{answer}x")).unwrap_err().reason,
        "invalid_body"
    );
    f.untouched(&id);
    let first = f.reply(&id, &answer).unwrap();
    let retry = f.reply(&id, &answer).unwrap();
    assert!(retry.duplicate);
    assert_eq!(first.reply_id, retry.reply_id);
    let request = f.request();
    f.db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params![answer, request],
    )
    .unwrap();
    assert_eq!(m::read(&f.path, &request, &scope()).unwrap().body, answer);
    assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().body, answer);
    f.untouched(&request);
    f.db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params![format!("{answer}x"), request],
    )
    .unwrap();
    assert_eq!(
        m::read(&f.path, &request, &scope()).unwrap_err().reason,
        "invalid_body"
    );
    f.untouched(&request);
}

#[test]
fn terminal_read_and_scan_accept_16384_utf8_bytes_without_ack() {
    let f = Fixture::new();
    let parent = f.send("muse", "alice", None, None, false);
    let terminal = f.send("alice", "muse", Some(parent.clone()), Some(parent), false);
    let answer = "é".repeat(8192);
    f.db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params![answer, terminal],
    )
    .unwrap();
    assert_eq!(m::read(&f.path, &terminal, &scope()).unwrap().body, answer);
    assert_eq!(m::scan(&f.path, &scope()).unwrap().unwrap().body, answer);
    f.untouched(&terminal);
    f.db.execute(
        "UPDATE messages SET body=? WHERE id=?",
        params![format!("{answer}x"), terminal],
    )
    .unwrap();
    assert_eq!(
        m::read(&f.path, &terminal, &scope()).unwrap_err().reason,
        "invalid_body"
    );
    f.untouched(&terminal);
}

#[test]
fn empty_and_large_peer_sets_preserve_sender_isolation() {
    let f = Fixture::new();
    let id = f.request();
    let mut scope = MuseScope {
        allowed_senders: vec![],
    };
    assert!(m::scan(&f.path, &scope).unwrap().is_none());
    assert_eq!(
        m::read(&f.path, &id, &scope).unwrap_err().reason,
        "message_not_available"
    );
    assert!(m::reply(&f.path, &id, "answer", || Ok(scope.clone())).is_err());
    scope.allowed_senders = (0..33).map(|i| format!("peer-{i}")).collect();
    assert!(m::scan(&f.path, &scope).unwrap().is_none());
    scope.allowed_senders.push("alice".into());
    assert_eq!(m::read(&f.path, &id, &scope).unwrap().id, id);
    assert_eq!(m::scan(&f.path, &scope).unwrap().unwrap().id, id);
    let first = m::reply(&f.path, &id, "answer", || Ok(scope.clone())).unwrap();
    scope.allowed_senders.retain(|peer| peer != "alice");
    assert_eq!(
        m::reply(&f.path, &id, "answer", || Ok(scope.clone()))
            .unwrap_err()
            .reason,
        "message_not_available"
    );
    assert!(!first.duplicate);
}
