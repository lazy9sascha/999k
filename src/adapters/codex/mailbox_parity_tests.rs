//! Offline parity through both real adapter APIs. No HTTP, Events or external targets.
use super::*;
use radio999k::{
    dot_mailbox::Mailbox, grok_mailbox as grok, mcp_request::Context, muse_mailbox as muse,
};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};
const TOKEN: &str = "PUBLIC_MAILBOX_PARITY_NOT_A_CREDENTIAL";
const OWNER: &str = "dot-fixture";
struct Fixture {
    _dir: tempfile::TempDir,
    codex: Profile,
    dot: Mailbox,
}
fn put(path: &Path, value: Value) {
    std::fs::write(path, value.to_string()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn context() -> Context<'static> {
    Context::new(Instant::now() + Duration::from_secs(10), &|| false)
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        let registration = dir.path().join("registration.json");
        let dot_policy = dir.path().join("dot.json");
        let policy = dir.path().join("codex.json");
        let digest = Sha256::digest(TOKEN)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        drop(Queue::open(&db).unwrap());
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        put(
            &registration,
            json!({"dot":{"mailbox":"dot","revoked":false},"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"muse":{"mailbox":"muse","revoked":false},"grok-bot":{"mailbox":"grok-bot","revoked":false}}),
        );
        put(
            &dot_policy,
            json!({"principal":OWNER,"mailbox":"dot","token_sha256":digest,"enabled":true,"registration":registration}),
        );
        put(
            &policy,
            json!({"principals":[{"id":"codex-cloud","token_sha256":digest,"operations":["read","wait","reply","send"],"send_to":[],"mailboxes":["codex-cloud"],"revoked":false}]}),
        );
        muse::prepare(&db).unwrap();
        grok::prepare(&db).unwrap();
        radio999k::dot_profile::check(&db, &dot_policy, true).unwrap();
        Self {
            dot: Mailbox::new(db.clone(), dot_policy),
            codex: Profile {
                hosted: None,
                identity: crate::codex_mcp::Identity::BuiltIn(
                    radio999k::mailbox_catalog::codex().mailbox,
                ),
                db,
                policy,
                registration,
            },
            _dir: dir,
        }
    }
    fn queue(&self) -> Queue {
        Queue::open_existing(&self.codex.db).unwrap()
    }
    fn connection(&self) -> Connection {
        Connection::open(&self.codex.db).unwrap()
    }
    fn request(&self, recipient: &str) -> String {
        self.queue()
            .send(Envelope {
                sender: if recipient == "dot" {
                    "codex-cloud"
                } else {
                    "dot"
                }
                .into(),
                recipient: recipient.into(),
                body: "request".into(),
                correlation_id: Some("parity-correlation".into()),
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id
    }
    fn reply(&self, mailbox: &str, id: &str, body: &str) -> Result<Value, (i32, &'static str)> {
        if mailbox == "muse" {
            muse::reply(&self.codex.db, id, body, || {
                Ok(muse::MuseScope {
                    allowed_senders: vec!["dot".into()],
                })
            })
            .map(|value| serde_json::to_value(value).unwrap())
            .map_err(|e| (0, e.reason))
        } else if mailbox == "grok-bot" {
            grok::reply(&self.codex.db, id, body, || {
                Ok(grok::GrokScope {
                    allowed_senders: vec!["dot".into()],
                })
            })
            .map(|value| serde_json::to_value(value).unwrap())
            .map_err(|e| (0, e.reason))
        } else if mailbox == "dot" {
            self.dot
                .reply(OWNER, TOKEN, id, body, &context())
                .map_err(|e| (e.code, e.reason))
        } else {
            let profile = match mailbox {
                "codex-cloud" => &self.codex,
                _ => panic!("unknown fixture profile"),
            };
            profile
                .call(
                    &[format!("Bearer {TOKEN}").into_bytes()],
                    "radio999k_reply",
                    json!({"message_id":id,"body":body}),
                )
                .map_err(|(code, reason)| (i32::from(code), reason))
        }
    }
    fn pending(&self, id: &str) {
        let state:(Option<String>,Option<i64>,i64,Option<i64>)=self.connection().query_row("SELECT receipt,lease_until_ms,attempts,acknowledged_at_ms FROM messages WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(state, (None, None, 0, None));
        assert_eq!(
            self.connection()
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM messages WHERE in_reply_to=?",
                    [id],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            self.connection()
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM dot_event_replies WHERE message_id=?",
                    [id],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
    }
    fn schema(&self) -> Vec<(String, String)> {
        self.connection()
            .prepare("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}
#[test]
fn shared_insert_keeps_adapter_wire_shapes_bindings_and_database_identity() {
    let f = Fixture::new();
    let schema = f.schema();
    for mailbox in ["dot", "codex-cloud"] {
        let id = f.request(mailbox);
        let first = f.reply(mailbox, &id, "answer 🕊").unwrap();
        let reply_id = first["reply_id"].as_str().unwrap();
        if mailbox == "dot" {
            assert_eq!(
                first,
                json!({"state":"replied","reply_id":reply_id,"duplicate":false})
            );
            assert_eq!(
                f.reply(mailbox, &id, "answer 🕊").unwrap(),
                json!({"state":"replied","reply_id":reply_id,"duplicate":true})
            );
            let binding: String = f
                .connection()
                .query_row(
                    "SELECT reply_id FROM dot_event_replies WHERE message_id=?",
                    [&id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(binding, reply_id);
        } else {
            assert_eq!(first, json!({"state":"replied","reply_id":reply_id}));
            assert_eq!(f.reply(mailbox, &id, "answer 🕊").unwrap(), first);
            assert_eq!(
                f.connection()
                    .query_row::<i64, _, _>(
                        "SELECT COUNT(*) FROM dot_event_replies WHERE message_id=?",
                        [&id],
                        |r| r.get(0)
                    )
                    .unwrap(),
                0
            );
        }
        let recipient = if mailbox == "dot" {
            "codex-cloud"
        } else {
            "dot"
        };
        let stored = f.queue().observe_ordinary(reply_id, recipient).unwrap();
        assert_eq!(
            stored.envelope,
            Envelope {
                sender: mailbox.into(),
                recipient: recipient.into(),
                body: "answer 🕊".into(),
                correlation_id: Some("parity-correlation".into()),
                in_reply_to: Some(id.clone()),
                idempotency_key: Some(format!("gw-reply-{id}"))
            }
        );
        assert!(stored.created_at_ms > 0);
        let acknowledged:bool=f.connection().query_row("SELECT receipt IS NOT NULL AND attempts=1 AND acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?",[&id],|r|r.get(0)).unwrap();
        assert!(acknowledged);
        assert_eq!(
            f.reply(mailbox, &id, "changed").unwrap_err(),
            if mailbox == "dot" {
                (-32009, "reply_conflict")
            } else {
                (409, "conflict")
            }
        );
    }
    assert_eq!(f.schema(), schema);
    assert_eq!(
        f.connection()
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        radio999k::SCHEMA_VERSION
    );
    assert_eq!(
        f.connection()
            .pragma_query_value::<i64, _>(None, "application_id", |r| r.get(0))
            .unwrap(),
        1346851153
    );
}
#[test]
fn insert_ack_and_dot_binding_failures_roll_back_all_completion_writes() {
    for mailbox in ["dot", "codex-cloud"] {
        for stage in ["insert", "ack", "binding"] {
            if mailbox != "dot" && stage == "binding" {
                continue;
            }
            let f = Fixture::new();
            let id = f.request(mailbox);
            let trigger = match stage {
                "insert" => {
                    "CREATE TRIGGER fail_completion BEFORE INSERT ON messages WHEN NEW.in_reply_to IS NOT NULL BEGIN SELECT RAISE(ABORT,'fixture'); END;"
                }
                "ack" => {
                    "CREATE TRIGGER fail_completion BEFORE UPDATE OF acknowledged_at_ms ON messages BEGIN SELECT RAISE(ABORT,'fixture'); END;"
                }
                "binding" => {
                    "CREATE TRIGGER fail_completion BEFORE INSERT ON dot_event_replies BEGIN SELECT RAISE(ABORT,'fixture'); END;"
                }
                _ => unreachable!(),
            };
            f.connection().execute_batch(trigger).unwrap();
            assert_eq!(
                f.reply(mailbox, &id, "answer").unwrap_err(),
                if mailbox == "dot" {
                    (-32602, "setup_or_storage_error")
                } else {
                    (503, "storage_unavailable")
                }
            );
            f.pending(&id);
            f.connection()
                .execute_batch("DROP TRIGGER fail_completion")
                .unwrap();
            f.reply(mailbox, &id, "answer").unwrap();
        }
    }
}
#[test]
fn existing_profile_body_limits_and_native_claim_boundaries_remain() {
    for (mailbox, limit) in [("dot", 8192), ("codex-cloud", 16384)] {
        let f = Fixture::new();
        let id = f.request(mailbox);
        assert_eq!(
            f.reply(mailbox, &id, &"a".repeat(limit + 1)).unwrap_err(),
            if mailbox == "dot" {
                (-32602, "invalid_body")
            } else {
                (400, "invalid_fields")
            }
        );
        f.pending(&id);
        f.reply(mailbox, &id, &"a".repeat(limit)).unwrap();
        let claimed = f.request(mailbox);
        let lease = f
            .queue()
            .receive(mailbox, Duration::from_secs(30))
            .unwrap()
            .unwrap();
        assert_eq!(lease.message.id, claimed);
        let before = std::fs::read(&f.codex.db).unwrap();
        assert_eq!(
            f.reply(mailbox, &claimed, "must not adopt").unwrap_err(),
            if mailbox == "dot" {
                (-32009, "existing_claim_requires_reconciliation")
            } else {
                (400, "invalid_input")
            }
        );
        assert_eq!(std::fs::read(&f.codex.db).unwrap(), before);
    }
}
#[test]
fn terminal_answers_and_other_mailboxes_cannot_be_completed_as_requests() {
    for mailbox in ["dot", "codex-cloud"] {
        let f = Fixture::new();
        let other = if mailbox == "dot" {
            "codex-cloud"
        } else {
            "dot"
        };
        let outgoing = f.request(other);
        let terminal = f
            .queue()
            .send(Envelope {
                sender: other.into(),
                recipient: mailbox.into(),
                body: "terminal".into(),
                correlation_id: Some("parity-correlation".into()),
                in_reply_to: Some(outgoing.clone()),
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        let before = std::fs::read(&f.codex.db).unwrap();
        assert_eq!(
            f.reply(mailbox, &terminal, "no loop").unwrap_err(),
            if mailbox == "dot" {
                (-32009, "reply_is_terminal")
            } else {
                (409, "reply_is_terminal")
            }
        );
        assert_eq!(
            f.reply(mailbox, &outgoing, "wrong mailbox").unwrap_err(),
            if mailbox == "dot" {
                (-32004, "message_not_available")
            } else {
                (404, "message_not_available")
            }
        );
        assert_eq!(std::fs::read(&f.codex.db).unwrap(), before);
    }
}
#[test]
fn dot_final_cancellation_and_codex_revocation_leave_no_completion() {
    let f = Fixture::new();
    let id = f.request("dot");
    let checks = AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, Ordering::SeqCst) == 2;
    let context = Context::new(Instant::now() + Duration::from_secs(10), &cancelled);
    assert_eq!(
        f.dot
            .reply(OWNER, TOKEN, &id, "answer", &context)
            .unwrap_err(),
        radio999k::mcp_request::Fault::new(-32021, "wait_cancelled")
    );
    assert_eq!(checks.load(Ordering::SeqCst), 3);
    f.pending(&id);
    let id = f.request("codex-cloud");
    put(
        &f.codex.registration,
        json!({"dot":{"mailbox":"dot","revoked":true},"codex-cloud":{"mailbox":"codex-cloud","revoked":false}}),
    );
    assert_eq!(
        f.reply("codex-cloud", &id, "answer").unwrap_err(),
        (403, "sender_revoked_or_mailbox_mismatch")
    );
    f.pending(&id);
}

const ALL: [&str; 4] = ["muse", "grok-bot", "dot", "codex-cloud"];
fn reply_table(mailbox: &str) -> Option<&'static str> {
    match mailbox {
        "muse" => Some("muse_mailbox_replies"),
        "grok-bot" => Some("grok_mailbox_replies"),
        "dot" => Some("dot_event_replies"),
        _ => None,
    }
}

#[test]
fn all_profiles_keep_completion_replay_and_table_ownership() {
    let f = Fixture::new();
    let schema = f.schema();
    for mailbox in ALL {
        let id = f.request(mailbox);
        let first = f.reply(mailbox, &id, "answer 🕊").unwrap();
        let replay = f.reply(mailbox, &id, "answer 🕊").unwrap();
        assert_eq!(first["reply_id"], replay["reply_id"]);
        assert_eq!(first["state"], "replied");
        if reply_table(mailbox).is_some() {
            assert_eq!(first["duplicate"], false);
            assert_eq!(replay["duplicate"], true);
        } else {
            assert!(first.get("duplicate").is_none());
            assert_eq!(first, replay);
        }
        for table in [
            "muse_mailbox_replies",
            "grok_mailbox_replies",
            "dot_event_replies",
        ] {
            let binding: Option<String> = f
                .connection()
                .query_row(
                    &format!("SELECT reply_id FROM {table} WHERE message_id=?"),
                    [&id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(
                binding.as_deref(),
                if reply_table(mailbox) == Some(table) {
                    first["reply_id"].as_str()
                } else {
                    None
                }
            );
        }
        let claimed_and_acked: bool = f.connection().query_row(
            "SELECT receipt IS NOT NULL AND attempts=1 AND acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?", [&id], |r| r.get(0)
        ).unwrap();
        assert!(claimed_and_acked);
        assert_eq!(
            f.reply(mailbox, &id, "changed").unwrap_err().1,
            if reply_table(mailbox).is_some() {
                "reply_conflict"
            } else {
                "conflict"
            }
        );
    }
    assert_eq!(f.schema(), schema);
}

#[test]
fn all_profiles_roll_back_each_completion_write_stage() {
    for mailbox in ALL {
        for stage in ["insert", "ack", "binding"] {
            if stage == "binding" && reply_table(mailbox).is_none() {
                continue;
            }
            let f = Fixture::new();
            let id = f.request(mailbox);
            let trigger = match stage {
                "insert" => "CREATE TRIGGER fail_completion BEFORE INSERT ON messages WHEN NEW.in_reply_to IS NOT NULL BEGIN SELECT RAISE(ABORT,'fixture'); END;".to_owned(),
                "ack" => "CREATE TRIGGER fail_completion BEFORE UPDATE OF acknowledged_at_ms ON messages BEGIN SELECT RAISE(ABORT,'fixture'); END;".to_owned(),
                "binding" => format!("CREATE TRIGGER fail_completion BEFORE INSERT ON {} BEGIN SELECT RAISE(ABORT,'fixture'); END;", reply_table(mailbox).unwrap()),
                _ => unreachable!(),
            };
            f.connection().execute_batch(&trigger).unwrap();
            assert_eq!(
                f.reply(mailbox, &id, "answer").unwrap_err().1,
                if mailbox == "dot" {
                    "setup_or_storage_error"
                } else {
                    "storage_unavailable"
                }
            );
            f.pending(&id);
            for table in [
                "muse_mailbox_replies",
                "grok_mailbox_replies",
                "dot_event_replies",
            ] {
                assert_eq!(
                    f.connection()
                        .query_row::<i64, _, _>(
                            &format!("SELECT COUNT(*) FROM {table} WHERE message_id=?"),
                            [&id],
                            |r| r.get(0)
                        )
                        .unwrap(),
                    0
                );
            }
            f.connection()
                .execute_batch("DROP TRIGGER fail_completion")
                .unwrap();
            f.reply(mailbox, &id, "answer").unwrap();
        }
    }
}

#[test]
fn all_profiles_keep_body_limits_and_do_not_adopt_claims() {
    for mailbox in ALL {
        let f = Fixture::new();
        let id = f.request(mailbox);
        let limit = if mailbox == "dot" { 8192 } else { 16384 };
        assert_eq!(
            f.reply(mailbox, &id, &"a".repeat(limit + 1)).unwrap_err().1,
            if mailbox == "codex-cloud" {
                "invalid_fields"
            } else {
                "invalid_body"
            }
        );
        f.pending(&id);
        f.reply(mailbox, &id, &"a".repeat(limit)).unwrap();
        let id = f.request(mailbox);
        let lease = f
            .queue()
            .receive(mailbox, Duration::from_secs(30))
            .unwrap()
            .unwrap();
        assert_eq!(lease.message.id, id);
        let before = std::fs::read(&f.codex.db).unwrap();
        assert_eq!(
            f.reply(mailbox, &id, "must not adopt").unwrap_err().1,
            if mailbox == "codex-cloud" {
                "invalid_input"
            } else {
                "existing_claim_requires_reconciliation"
            }
        );
        assert_eq!(std::fs::read(&f.codex.db).unwrap(), before);
    }
}

#[test]
fn bound_profiles_reject_unbound_replay_but_queue_profiles_recover_it() {
    for mailbox in ALL {
        let f = Fixture::new();
        let id = f.request(mailbox);
        let raw = f
            .queue()
            .send(Envelope {
                sender: mailbox.into(),
                recipient: if mailbox == "dot" {
                    "codex-cloud"
                } else {
                    "dot"
                }
                .into(),
                body: "answer".into(),
                correlation_id: Some("parity-correlation".into()),
                in_reply_to: Some(id.clone()),
                idempotency_key: Some(format!("gw-reply-{id}")),
            })
            .unwrap()
            .message;
        let before = std::fs::read(&f.codex.db).unwrap();
        if reply_table(mailbox).is_some() {
            assert_eq!(
                f.reply(mailbox, &id, "answer").unwrap_err().1,
                "reconciliation_required"
            );
            assert_eq!(std::fs::read(&f.codex.db).unwrap(), before);
        } else {
            let recovered = f.reply(mailbox, &id, "answer").unwrap();
            assert_eq!(recovered["reply_id"], raw.id);
            assert_eq!(f.reply(mailbox, &id, "answer").unwrap(), recovered);
        }
    }
}
