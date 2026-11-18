use pigeonmq::{
    Delivery, Envelope, Error, Message, Queue,
    receiver_api::{self, Action, Request},
    runtime_admission::Record,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{cell::Cell, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    registry: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        Queue::open(&db).unwrap();
        let registry = dir.path().join("registration.json");
        std::fs::write(
            &registry,
            serde_json::to_vec(&json!({
                "worker":{"mailbox":"worker","revoked":false},
                "dot":{"mailbox":"dot","revoked":false},
                "other":{"mailbox":"other","revoked":false}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            _dir: dir,
            db,
            registry,
        }
    }
    fn request(&self, action: Action) -> Request {
        Request {
            recipient: "worker".into(),
            session_id: "selected-session".into(),
            profile_id: "fixture-native".into(),
            action,
        }
    }
    fn run(&self, action: Action) -> pigeonmq::Result<Value> {
        receiver_api::execute(
            &self.db,
            &self.registry,
            "worker",
            self.request(action),
            || Ok(()),
        )
    }
    fn send(&self, rpc: bool) -> Message {
        let envelope = Envelope {
            sender: "dot".into(),
            recipient: "worker".into(),
            body: "synthetic request".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        };
        let mut q = Queue::open_existing(&self.db).unwrap();
        if rpc {
            q.request(envelope).unwrap().message
        } else {
            q.send(envelope).unwrap().message
        }
    }
    fn claim(&self) -> Delivery {
        serde_json::from_value(self.run(Action::Claim { lease_seconds: 60 }).unwrap()).unwrap()
    }
    fn begin(&self, d: &Delivery) -> Record {
        serde_json::from_value(
            self.run(Action::Begin {
                message_id: d.message.id.clone(),
                receipt: d.receipt.clone(),
                payload_digest: "a".repeat(64),
            })
            .unwrap(),
        )
        .unwrap()
    }
    fn update(&self, d: &Delivery, record: Record) -> Record {
        serde_json::from_value(
            self.run(Action::Update {
                receipt: d.receipt.clone(),
                record: record.into(),
            })
            .unwrap(),
        )
        .unwrap()
    }
    fn completed(&self, d: &Delivery, body: &str) {
        let mut record = self.begin(d);
        record.admission_id = Some("native-admission".into());
        record.stage = "admitted".into();
        let mut record = self.update(d, record);
        record.stage = "model_completed".into();
        record.answer_digest = Some(digest(body));
        self.update(d, record);
    }
    fn state(&self, d: &Delivery) -> Value {
        self.run(Action::State {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
        })
        .unwrap()
    }
    fn complete(&self, d: &Delivery, body: Option<&str>, failure: bool) -> pigeonmq::Result<Value> {
        self.run(Action::Complete {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
            body: body.map(str::to_owned),
            failure,
        })
    }
}
fn digest(s: &str) -> String {
    Sha256::digest(serde_json::to_vec(s).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn ordinary_lifecycle_keeps_owner_cas_reply_and_ack_atomic_and_replayable() {
    let f = Fixture::new();
    let original = f.send(false);
    let d = f.claim();
    assert_eq!(d.message.id, original.id);
    assert_eq!(d.attempt, 1);
    let mut record = f.begin(&d);
    assert_eq!(record.principal_id, "worker");
    assert_eq!(record.session_id, "selected-session");
    assert!(
        f.run(Action::Begin {
            message_id: original.id.clone(),
            receipt: d.receipt.clone(),
            payload_digest: "a".repeat(64)
        })
        .is_err()
    );
    let loaded: Record = serde_json::from_value(
        f.run(Action::Load {
            message_id: original.id.clone(),
            receipt: d.receipt.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loaded, record);
    record.admission_id = Some("admission".into());
    record.stage = "admitted".into();
    let stale = record.clone();
    let mut record = f.update(&d, record);
    assert!(
        f.run(Action::Update {
            receipt: d.receipt.clone(),
            record: stale.into()
        })
        .is_err()
    );
    record.stage = "model_completed".into();
    record.answer_digest = Some(digest("answer"));
    f.update(&d, record);
    let renewed = f
        .run(Action::Renew {
            message_id: original.id.clone(),
            receipt: d.receipt.clone(),
            lease_seconds: 120,
        })
        .unwrap();
    assert!(renewed.as_i64().unwrap() > d.lease_until_ms);
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
    assert!(f.complete(&d, Some("wrong answer"), false).is_err());
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
    let reply: Message =
        serde_json::from_value(f.complete(&d, Some("answer"), false).unwrap()).unwrap();
    assert_eq!(
        reply.envelope.in_reply_to.as_deref(),
        Some(original.id.as_str())
    );
    assert_eq!(
        reply.envelope.correlation_id.as_deref(),
        Some(original.id.as_str())
    );
    assert_eq!(reply.envelope.sender, "worker");
    assert_eq!(reply.envelope.recipient, "dot");
    assert_eq!(
        reply.envelope.idempotency_key,
        Some(format!("gw-reply-{}", original.id))
    );
    assert_eq!(f.state(&d), json!({"acknowledged":true}));
    let repeated: Message =
        serde_json::from_value(f.complete(&d, Some("answer"), false).unwrap()).unwrap();
    assert_eq!(repeated.id, reply.id);
    assert!(
        f.run(Action::Renew {
            message_id: original.id,
            receipt: d.receipt,
            lease_seconds: 60
        })
        .is_err()
    );
}

#[test]
fn receipt_identity_and_namespace_cannot_be_spoofed() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    let record = f.begin(&d);
    for action in [
        Action::Renew {
            message_id: d.message.id.clone(),
            receipt: "wrong".into(),
            lease_seconds: 60,
        },
        Action::Begin {
            message_id: d.message.id.clone(),
            receipt: "wrong".into(),
            payload_digest: "a".repeat(64),
        },
        Action::Load {
            message_id: d.message.id.clone(),
            receipt: "wrong".into(),
        },
        Action::Update {
            receipt: "wrong".into(),
            record: record.clone().into(),
        },
        Action::State {
            message_id: d.message.id.clone(),
            receipt: "wrong".into(),
        },
        Action::Complete {
            message_id: d.message.id.clone(),
            receipt: "wrong".into(),
            body: Some("answer".into()),
            failure: true,
        },
    ] {
        assert!(f.run(action).is_err());
    }
    for field in ["principal", "session", "profile", "mailbox"] {
        let mut request = f.request(Action::Load {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
        });
        let principal = if field == "principal" {
            "another-principal"
        } else {
            "worker"
        };
        if field == "session" {
            request.session_id = "another-session".into();
        }
        if field == "profile" {
            request.profile_id = "another-profile".into();
        }
        if field == "mailbox" {
            request.recipient = "other".into();
        }
        assert!(receiver_api::execute(&f.db, &f.registry, principal, request, || Ok(())).is_err());
    }
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
}

#[test]
fn proof_is_required_for_success_and_failure_is_an_explicit_completion() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    assert!(f.complete(&d, Some("answer"), false).is_err());
    assert!(f.complete(&d, None, false).is_err());
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
    let failed = f
        .complete(&d, Some("Native rejected the submission."), true)
        .unwrap();
    assert_eq!(f.state(&d), json!({"acknowledged":true}));
    assert_eq!(
        f.complete(&d, Some("Native rejected the submission."), true)
            .unwrap(),
        failed
    );
    assert!(f.complete(&d, Some("changed failure"), true).is_err());
}

#[test]
fn terminal_consumption_acks_only_the_answer_and_never_creates_a_reply() {
    let f = Fixture::new();
    let mut q = Queue::open_existing(&f.db).unwrap();
    let original = q
        .send(Envelope {
            sender: "worker".into(),
            recipient: "dot".into(),
            body: "question".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message;
    let terminal = q
        .send(Envelope {
            sender: "dot".into(),
            recipient: "worker".into(),
            body: "terminal".into(),
            correlation_id: Some(original.id.clone()),
            in_reply_to: Some(original.id),
            idempotency_key: None,
        })
        .unwrap()
        .message;
    let d = f.claim();
    assert_eq!(d.message.id, terminal.id);
    assert!(f.complete(&d, None, false).is_err());
    f.completed(&d, "consumed");
    assert!(f.complete(&d, Some("reply loop"), false).is_err());
    let returned: Message = serde_json::from_value(f.complete(&d, None, false).unwrap()).unwrap();
    assert_eq!(returned.id, terminal.id);
    assert_eq!(f.state(&d), json!({"acknowledged":true}));
    assert_eq!(
        f.complete(&d, None, false).unwrap(),
        serde_json::to_value(returned).unwrap()
    );
    let count: i64 = Connection::open(&f.db)
        .unwrap()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn invalid_terminal_relationship_and_rpc_are_never_claimed() {
    let f = Fixture::new();
    let rpc = f.send(true);
    assert_eq!(
        f.run(Action::Claim { lease_seconds: 60 }).unwrap(),
        Value::Null
    );
    assert!(
        f.run(Action::State {
            message_id: rpc.id,
            receipt: "wrong".into()
        })
        .is_err()
    );
    let ordinary = f.send(false);
    let conn = Connection::open(&f.db).unwrap();
    conn.execute(
        "UPDATE messages SET in_reply_to='missing',correlation_id='missing' WHERE id=?1",
        [ordinary.id.clone()],
    )
    .unwrap();
    assert!(f.run(Action::Claim { lease_seconds: 60 }).is_err());
    let attempts: i64 = conn
        .query_row(
            "SELECT attempts FROM messages WHERE id=?1",
            [ordinary.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 0);
}

#[test]
fn revocation_after_lock_rolls_back_admission_and_claim() {
    let f = Fixture::new();
    let original = f.send(false);
    let calls = Cell::new(0);
    let result = receiver_api::execute(
        &f.db,
        &f.registry,
        "worker",
        f.request(Action::Claim { lease_seconds: 60 }),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Invalid("revoked"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    let conn = Connection::open(&f.db).unwrap();
    let attempts: i64 = conn
        .query_row(
            "SELECT attempts FROM messages WHERE id=?1",
            [&original.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 0);
    let d = f.claim();
    let calls = Cell::new(0);
    let result = receiver_api::execute(
        &f.db,
        &f.registry,
        "worker",
        f.request(Action::Begin {
            message_id: original.id,
            receipt: d.receipt,
            payload_digest: "a".repeat(64),
        }),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Invalid("revoked"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='runtime_admissions')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!exists);
}

#[test]
fn renew_rejects_expired_acknowledged_zero_overflow_and_revoked_registration() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    for seconds in [0, u64::MAX] {
        assert!(
            f.run(Action::Renew {
                message_id: d.message.id.clone(),
                receipt: d.receipt.clone(),
                lease_seconds: seconds
            })
            .is_err()
        );
    }
    Connection::open(&f.db)
        .unwrap()
        .execute(
            "UPDATE messages SET lease_until_ms=0 WHERE id=?1",
            [&d.message.id],
        )
        .unwrap();
    assert!(
        f.run(Action::Renew {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
            lease_seconds: 60
        })
        .is_err()
    );
    let mut registry: Value = serde_json::from_slice(&std::fs::read(&f.registry).unwrap()).unwrap();
    registry["dot"]["revoked"] = json!(true);
    std::fs::write(&f.registry, serde_json::to_vec(&registry).unwrap()).unwrap();
    assert!(
        f.run(Action::Authorize {
            message_id: Some(d.message.id)
        })
        .is_err()
    );
    assert_eq!(
        f.run(Action::Authorize { message_id: None }).unwrap(),
        Value::Null
    );
    assert!(f.run(Action::Claim { lease_seconds: 60 }).is_err());
    let fresh = Fixture::new();
    assert!(fresh.run(Action::Claim { lease_seconds: 0 }).is_err());
    assert!(
        fresh
            .run(Action::Claim {
                lease_seconds: u64::MAX
            })
            .is_err()
    );
    // The public queue renewal shares the same positive duration validation.
    assert!(
        Queue::open_existing(&fresh.db)
            .unwrap()
            .renew_lease("worker", "id", "receipt", Duration::ZERO)
            .is_err()
    );
}

#[test]
fn wire_rejects_spoofed_identity_unknown_fields_and_uses_explicit_operations() {
    let f = Fixture::new();
    let request = f.request(Action::Authorize { message_id: None });
    let mut value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["action"]["operation"], "authorize");
    value["principal_id"] = json!("another");
    assert!(serde_json::from_value::<Request>(value).is_err());
    assert!(
        serde_json::from_value::<Action>(
            json!({"operation":"claim","lease_seconds":30,"mailbox":"other"})
        )
        .is_err()
    );
}

#[test]
fn inspection_reconciles_lost_admission_response_without_inventing_an_id() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    let mut before = f.begin(&d);
    assert!(before.admission_id.is_none());
    before.stage = "requires_reconciliation".into();
    before.reason = Some("response_lost".into());
    let before = f.update(&d, before);
    let action = || Action::Reconcile {
        message_id: d.message.id.clone(),
        receipt: d.receipt.clone(),
        answer_digest: digest("recovered answer"),
    };
    let reconciled: Record = serde_json::from_value(f.run(action()).unwrap()).unwrap();
    assert_eq!(reconciled.stage, "reconciled_completed");
    assert_eq!(reconciled.admission_id, None);
    assert_eq!(reconciled.input_id, before.input_id);
    assert_eq!(reconciled.session_id, before.session_id);
    assert_eq!(reconciled.revision, before.revision + 1);
    let replay: Record = serde_json::from_value(f.run(action()).unwrap()).unwrap();
    assert_eq!(replay, reconciled);
    assert!(
        f.run(Action::Reconcile {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
            answer_digest: digest("different answer")
        })
        .is_err()
    );
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
    let completed = f.complete(&d, Some("recovered answer"), false).unwrap();
    assert_eq!(f.state(&d), json!({"acknowledged":true}));
    assert_eq!(
        f.complete(&d, Some("recovered answer"), false).unwrap(),
        completed
    );
    let after_ack: Record = serde_json::from_value(f.run(action()).unwrap()).unwrap();
    assert_eq!(after_ack, reconciled);
}

#[test]
fn reconciliation_requires_current_receipt_scoped_record_and_valid_immutable_digest() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    let action = |receipt: String| Action::Reconcile {
        message_id: d.message.id.clone(),
        receipt,
        answer_digest: digest("answer"),
    };
    assert!(f.run(action(d.receipt.clone())).is_err()); // No recorded owner exists yet.
    let before = f.begin(&d);
    assert!(f.run(action("wrong".into())).is_err());
    for field in ["principal", "session", "profile", "mailbox"] {
        let mut request = f.request(action(d.receipt.clone()));
        let principal = if field == "principal" {
            "another-principal"
        } else {
            "worker"
        };
        if field == "session" {
            request.session_id = "another-session".into();
        }
        if field == "profile" {
            request.profile_id = "another-profile".into();
        }
        if field == "mailbox" {
            request.recipient = "other".into();
        }
        assert!(receiver_api::execute(&f.db, &f.registry, principal, request, || Ok(())).is_err());
    }
    assert!(
        f.run(Action::Reconcile {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
            answer_digest: "bad-digest".into()
        })
        .is_err()
    );
    let loaded: Record = serde_json::from_value(
        f.run(Action::Load {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loaded, before);
    let mut invalid = before;
    invalid.stage = "reconciled_completed".into();
    assert!(
        f.run(Action::Update {
            receipt: d.receipt.clone(),
            record: invalid.into()
        })
        .is_err()
    );
    assert_eq!(f.state(&d), json!({"acknowledged":false}));
}

#[test]
fn revocation_before_renew_commit_rolls_back_the_extension() {
    let f = Fixture::new();
    f.send(false);
    let d = f.claim();
    let calls = Cell::new(0);
    let result = receiver_api::execute(
        &f.db,
        &f.registry,
        "worker",
        f.request(Action::Renew {
            message_id: d.message.id.clone(),
            receipt: d.receipt.clone(),
            lease_seconds: 120,
        }),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Invalid("revoked"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    let until: i64 = Connection::open(&f.db)
        .unwrap()
        .query_row(
            "SELECT lease_until_ms FROM messages WHERE id=?1",
            [d.message.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(until, d.lease_until_ms);
}
