//! Common authenticated receiver operations. Native routing and inference stay with clients.
use crate::{
    COLUMNS, Envelope, Error, Message, Queue, Result, now_ms, read_message,
    runtime_admission::{self, Record, Request as Admission},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub recipient: String,
    pub session_id: String,
    pub profile_id: String,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Authorize {
        message_id: Option<String>,
    },
    Claim {
        lease_seconds: u64,
    },
    Renew {
        message_id: String,
        receipt: String,
        lease_seconds: u64,
    },
    Begin {
        message_id: String,
        receipt: String,
        payload_digest: String,
    },
    Load {
        message_id: String,
        receipt: String,
    },
    Update {
        receipt: String,
        record: Box<Record>,
    },
    Reconcile {
        message_id: String,
        receipt: String,
        answer_digest: String,
    },
    State {
        message_id: String,
        receipt: String,
    },
    Complete {
        message_id: String,
        receipt: String,
        body: Option<String>,
        #[serde(default)]
        failure: bool,
    },
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn identifier(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid_runtime_identifier"));
    }
    Ok(())
}
struct Scope<'a, F> {
    recipient: &'a str,
    session: &'a str,
    profile: &'a str,
    principal: &'a str,
    registration: &'a Path,
    authorize: F,
}
impl<F: Fn() -> Result<()>> Scope<'_, F> {
    fn authorize(&self) -> Result<()> {
        (self.authorize)()?;
        self.route(self.recipient)
    }
    fn route(&self, sender: &str) -> Result<()> {
        crate::registration::validate_registration(self.registration, sender, self.recipient)
            .map_err(|(_, reason)| Error::Invalid(reason))
    }
    fn message(&self, db: &Connection, id: &str) -> Result<Message> {
        identifier(id)?;
        let message = db.query_row(
            &format!("SELECT {COLUMNS} FROM messages WHERE id=?1 AND recipient=?2 AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)"),
            params![id,self.recipient], read_message,
        ).optional()?.ok_or(Error::NotFound)?;
        self.route(&message.envelope.sender)?;
        runtime_admission::validate_terminal(db, id, self.recipient)?;
        Ok(message)
    }
    fn record(&self, db: &Connection, id: &str) -> Result<Option<Record>> {
        let record = match runtime_admission::read(db, id, self.recipient) {
            Ok(record) => record,
            Err(Error::Invalid("missing_runtime_admission")) => return Ok(None),
            Err(error) => return Err(error),
        };
        if record.principal_id != self.principal
            || record.session_id != self.session
            || record.profile_id != self.profile
        {
            return Err(Error::Invalid("runtime_owner_mismatch"));
        }
        Ok(Some(record))
    }
    fn checked(
        &self,
        db: &Connection,
        id: &str,
        receipt: &str,
        live: bool,
    ) -> Result<(Message, bool)> {
        self.authorize()?;
        identifier(receipt)?;
        let message = self.message(db, id)?;
        let (stored,until,acked): (Option<String>,Option<i64>,bool) = db.query_row(
            "SELECT receipt,lease_until_ms,acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL",
            params![id,self.recipient], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        )?;
        let now = now_ms()?;
        if stored.as_deref() != Some(receipt) || (live && (acked || until.is_none_or(|u| u <= now)))
        {
            return Err(Error::StaleReceipt);
        }
        self.record(db, id)?;
        Ok((message, acked))
    }
    fn completed(
        &self,
        db: &Connection,
        id: &str,
        receipt: &str,
        body: Option<&String>,
        failure: bool,
    ) -> Result<()> {
        self.checked(db, id, receipt, false)?;
        if failure {
            return Ok(());
        }
        let record = self
            .record(db, id)?
            .ok_or(Error::Invalid("missing_runtime_admission"))?;
        if !matches!(
            record.stage.as_str(),
            "model_completed" | "reconciled_completed"
        ) || record.answer_digest.is_none()
        {
            return Err(Error::Invalid("native_completion_unproven"));
        }
        if let Some(body) = body
            && record.answer_digest.as_deref() != Some(digest(body)?.as_str())
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

/// The hub supplies the authenticated principal and reloads existing bearer policy in `authorize`.
pub fn execute(
    db: &Path,
    registration: &Path,
    principal_id: &str,
    request: Request,
    authorize: impl Fn() -> Result<()>,
) -> Result<Value> {
    let Request {
        recipient,
        session_id,
        profile_id,
        action,
    } = request;
    for value in [
        recipient.as_str(),
        session_id.as_str(),
        profile_id.as_str(),
        principal_id,
    ] {
        identifier(value)?;
    }
    let scope = Scope {
        recipient: &recipient,
        session: &session_id,
        profile: &profile_id,
        principal: principal_id,
        registration,
        authorize,
    };
    scope.authorize()?;
    let mut queue = Queue::open_existing(db)?;
    match action {
        Action::Authorize { message_id } => {
            if let Some(id) = message_id {
                scope.message(&queue.connection, &id)?;
                scope.record(&queue.connection, &id)?;
            }
            Ok(Value::Null)
        }
        Action::Claim { lease_seconds } => {
            let lease = Duration::from_secs(lease_seconds);
            let millis =
                i64::try_from(lease.as_millis()).map_err(|_| Error::Invalid("lease overflow"))?;
            if millis <= 0 {
                return Err(Error::Invalid("lease must be at least one millisecond"));
            }
            let delivery = loop {
                let candidate = queue.connection.query_row(
                    "SELECT seq,id FROM messages WHERE recipient=?1 AND dead_lettered_at_ms IS NULL AND acknowledged_at_ms IS NULL AND (lease_until_ms IS NULL OR lease_until_ms<=?2) AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id) ORDER BY seq LIMIT 1",
                    params![recipient,now_ms()?], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)),
                ).optional()?;
                let Some((seq, id)) = candidate else {
                    break None;
                };
                let delivery = queue.receive_selected_scoped(
                    &recipient,
                    lease,
                    now_ms,
                    false,
                    Some(crate::ReceiveSelection::Observed(seq, &id, None)),
                    |conn| {
                        scope.authorize()?;
                        scope.message(conn, &id)?;
                        scope.record(conn, &id)?;
                        Ok(())
                    },
                )?;
                if delivery.is_some() {
                    break delivery;
                }
                // A quarantined candidate must not hide the next available request.
                let quarantined: bool = queue.connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NOT NULL)",
                    params![id,recipient], |r| r.get(0),
                )?;
                if !quarantined {
                    break None;
                }
            };
            let mut value = serde_json::to_value(&delivery)?;
            if let Some(delivery) = delivery {
                value["message"]["message_id"] = json!(delivery.message.id);
            }
            Ok(value)
        }
        Action::Renew {
            message_id,
            receipt,
            lease_seconds,
        } => {
            let until = queue.renew_lease_guarded(
                &recipient,
                &message_id,
                &receipt,
                Duration::from_secs(lease_seconds),
                |conn| scope.checked(conn, &message_id, &receipt, true).map(|_| ()),
            )?;
            Ok(json!(until))
        }
        Action::Begin {
            message_id,
            receipt,
            payload_digest,
        } => {
            let message = scope.message(&queue.connection, &message_id)?;
            let record = runtime_admission::execute_guarded(
                db,
                Admission::Begin {
                    message_id: message_id.clone(),
                    recipient: recipient.clone(),
                    session_id: session_id.clone(),
                    principal_id: principal_id.into(),
                    profile_id: profile_id.clone(),
                    payload_digest,
                    envelope_digest: digest(&message)?,
                },
                |conn| scope.checked(conn, &message_id, &receipt, true).map(|_| ()),
            )?;
            Ok(serde_json::to_value(record)?)
        }
        Action::Load {
            message_id,
            receipt,
        } => {
            let record = runtime_admission::execute_guarded(
                db,
                Admission::Load {
                    message_id: message_id.clone(),
                    recipient: recipient.clone(),
                },
                |conn| {
                    scope
                        .checked(conn, &message_id, &receipt, false)
                        .map(|_| ())
                },
            )?;
            Ok(serde_json::to_value(record)?)
        }
        Action::Update { receipt, record } => {
            if record.recipient != recipient {
                return Err(Error::NotFound);
            }
            let id = record.message_id.clone();
            let record = runtime_admission::execute_guarded(
                db,
                Admission::Update { record: *record },
                |conn| scope.checked(conn, &id, &receipt, false).map(|_| ()),
            )?;
            Ok(serde_json::to_value(record)?)
        }
        Action::Reconcile {
            message_id,
            receipt,
            answer_digest,
        } => {
            // A native inspection report, not a fabricated admission or a new submission.
            let mut record = runtime_admission::execute_guarded(
                db,
                Admission::Load {
                    message_id: message_id.clone(),
                    recipient: recipient.clone(),
                },
                |conn| {
                    scope
                        .checked(conn, &message_id, &receipt, false)
                        .map(|_| ())
                },
            )?;
            if record
                .answer_digest
                .as_ref()
                .is_some_and(|digest| digest != &answer_digest)
            {
                return Err(Error::Conflict);
            }
            if record.stage == "reconciled_completed" {
                return Ok(serde_json::to_value(record)?);
            }
            record.stage = "reconciled_completed".into();
            record.answer_digest = Some(answer_digest);
            let record =
                runtime_admission::execute_guarded(db, Admission::Update { record }, |conn| {
                    scope
                        .checked(conn, &message_id, &receipt, false)
                        .map(|_| ())
                })?;
            Ok(serde_json::to_value(record)?)
        }
        Action::State {
            message_id,
            receipt,
        } => {
            let tx = queue
                .connection
                .transaction_with_behavior(TransactionBehavior::Deferred)?;
            let (_, acknowledged) = scope.checked(&tx, &message_id, &receipt, false)?;
            scope.authorize()?;
            tx.commit()?;
            Ok(json!({"acknowledged":acknowledged}))
        }
        Action::Complete {
            message_id,
            receipt,
            body,
            failure,
        } => {
            let message = scope.message(&queue.connection, &message_id)?;
            if message.envelope.in_reply_to.is_some() {
                if body.is_some() {
                    return Err(Error::Invalid("cannot reply to a reply"));
                }
                let acknowledged =
                    queue.ack_guarded(&recipient, &message_id, &receipt, |conn| {
                        scope.completed(conn, &message_id, &receipt, None, failure)
                    })?;
                if !failure {
                    let duplicate = matches!(acknowledged, crate::AckResult::AlreadyAcknowledged);
                    crate::trace_terminal_commit(&message_id, duplicate);
                }
                Ok(serde_json::to_value(message)?)
            } else {
                let body = body.ok_or(Error::Invalid("missing_reply_body"))?;
                let reply = Envelope {
                    sender: recipient.clone(),
                    recipient: message.envelope.sender,
                    body: body.clone(),
                    correlation_id: Some(
                        message
                            .envelope
                            .correlation_id
                            .unwrap_or(message_id.clone()),
                    ),
                    in_reply_to: Some(message_id.clone()),
                    idempotency_key: Some(format!("gw-reply-{message_id}")),
                };
                let result = queue.reply_and_ack_scoped(
                    &recipient,
                    &message_id,
                    &receipt,
                    reply,
                    |conn| scope.completed(conn, &message_id, &receipt, Some(&body), failure),
                )?;
                Ok(serde_json::to_value(result)?)
            }
        }
    }
}
