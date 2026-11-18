//! Existing-session admission metadata in the queue's SQLite database.
//! This local backend never connects to agents and stores no bodies or credentials.
use crate::{Error, Result, now_ms};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub message_id: String,
    pub recipient: String,
    pub session_id: String,
    pub principal_id: String,
    pub profile_id: String,
    pub payload_digest: String,
    pub input_id: String,
    pub envelope_digest: String,
    pub admission_id: Option<String>,
    pub stage: String,
    pub answer_digest: Option<String>,
    pub reason: Option<String>,
    pub revision: i64,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Begin {
        message_id: String,
        recipient: String,
        session_id: String,
        principal_id: String,
        profile_id: String,
        payload_digest: String,
        envelope_digest: String,
    },
    Load {
        message_id: String,
        recipient: String,
    },
    Update {
        record: Record,
    },
}
fn identifier(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid_runtime_identifier"));
    }
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("invalid_runtime_digest"));
    }
    Ok(())
}
// The queue host, not the native adapter, validates terminal authority.
// Ordinary requests keep their existing admission rules.
pub(crate) fn validate_terminal(connection: &Connection, id: &str, recipient: &str) -> Result<()> {
    let valid: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE m.id=?1 AND m.recipient=?2 AND
         (m.in_reply_to IS NULL OR (
           NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id) AND
           EXISTS(SELECT 1 FROM messages original WHERE original.id=m.in_reply_to
             AND original.in_reply_to IS NULL
             AND original.sender=m.recipient AND original.recipient=m.sender
             AND COALESCE(original.correlation_id,original.id)=m.correlation_id
             AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=original.id)))))",
        params![id, recipient],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(Error::Invalid("invalid_terminal_relationship"));
    }
    Ok(())
}
pub(crate) fn read(connection: &Connection, id: &str, recipient: &str) -> Result<Record> {
    validate_terminal(connection, id, recipient)?;
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_admissions')", [], |r| r.get(0))?;
    if !exists {
        return Err(Error::Invalid("missing_runtime_admission"));
    }
    connection.query_row("SELECT message_id,recipient,session_id,input_id,envelope_digest,admission_id,stage,answer_digest,reason,revision,principal_id,profile_id,payload_digest FROM runtime_admissions WHERE message_id=?1 AND recipient=?2", params![id, recipient], |r| Ok(Record {
        message_id:r.get(0)?, recipient:r.get(1)?, session_id:r.get(2)?, input_id:r.get(3)?, envelope_digest:r.get(4)?, admission_id:r.get(5)?, stage:r.get(6)?, answer_digest:r.get(7)?, reason:r.get(8)?, revision:r.get(9)?,principal_id:r.get(10)?,profile_id:r.get(11)?,payload_digest:r.get(12)?
    })).optional()?.ok_or(Error::Invalid("missing_runtime_admission"))
}
/// The authenticated host rechecks authority and receipt after acquiring the DB lock.
pub(crate) fn execute_guarded(
    path: impl AsRef<Path>,
    request: Request,
    authorize: impl Fn(&Connection) -> Result<()>,
) -> Result<Record> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    crate::validate_queue_journal(&connection)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    crate::validate_schema(&connection)?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    match request {
        Request::Load {
            message_id,
            recipient,
        } => {
            identifier(&message_id)?;
            identifier(&recipient)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
            authorize(&tx)?;
            let record = read(&tx, &message_id, &recipient)?;
            authorize(&tx)?;
            tx.commit()?;
            Ok(record)
        }
        Request::Begin {
            message_id,
            recipient,
            session_id,
            principal_id,
            profile_id,
            payload_digest,
            envelope_digest,
        } => {
            identifier(&message_id)?;
            identifier(&recipient)?;
            identifier(&session_id)?;
            identifier(&principal_id)?;
            identifier(&profile_id)?;
            digest(&payload_digest)?;
            digest(&envelope_digest)?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            authorize(&tx)?;
            let now = now_ms()?;
            let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages m WHERE id=?1 AND recipient=?2 AND acknowledged_at_ms IS NULL AND dead_lettered_at_ms IS NULL AND attempts=1 AND lease_until_ms>?3 AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id))",params![message_id,recipient,now],|r|r.get(0))?;
            if !eligible {
                return Err(Error::Invalid("first_live_claim_required_for_admission"));
            }
            validate_terminal(&tx, &message_id, &recipient)?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS runtime_admissions (
                message_id TEXT PRIMARY KEY REFERENCES messages(id), recipient TEXT NOT NULL,
                session_id TEXT NOT NULL,input_id TEXT NOT NULL,envelope_digest TEXT NOT NULL,
                principal_id TEXT NOT NULL,profile_id TEXT NOT NULL,payload_digest TEXT NOT NULL,
                admission_id TEXT,stage TEXT NOT NULL,answer_digest TEXT,reason TEXT,
                revision INTEGER NOT NULL CHECK(revision>=0));",
            )?;
            let existing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_admissions WHERE message_id=?1)",
                [&message_id],
                |r| r.get(0),
            )?;
            if existing {
                return Err(Error::Invalid("existing_attempt_requires_reconciliation"));
            }
            let input_id = format!("pigeon-{message_id}");
            tx.execute("INSERT INTO runtime_admissions(message_id,recipient,session_id,input_id,envelope_digest,principal_id,profile_id,payload_digest,stage,revision) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'before_submit',0)",params![message_id,recipient,session_id,input_id,envelope_digest,principal_id,profile_id,payload_digest])?;
            let result = read(&tx, &message_id, &recipient)?;
            authorize(&tx)?;
            tx.commit()?;
            Ok(result)
        }
        Request::Update { record } => {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            authorize(&tx)?;
            let old = read(&tx, &record.message_id, &record.recipient)?;
            if record.principal_id != old.principal_id
                || record.profile_id != old.profile_id
                || record.payload_digest != old.payload_digest
                || record.session_id != old.session_id
                || record.input_id != old.input_id
                || record.envelope_digest != old.envelope_digest
                || record.revision != old.revision
            {
                return Err(Error::Invalid("runtime_identity_or_revision_mismatch"));
            }
            if !matches!(
                record.stage.as_str(),
                "admitted"
                    | "pending"
                    | "model_completed"
                    | "reconciled_completed"
                    | "requires_reconciliation"
            ) {
                return Err(Error::Invalid("invalid_runtime_transition"));
            }
            if let Some(ref aid) = record.admission_id {
                identifier(aid)?;
            }
            if old.admission_id.is_some() && record.admission_id != old.admission_id {
                return Err(Error::Invalid("runtime_admission_changed"));
            }
            if old.admission_id.is_none()
                && record.admission_id.is_some()
                && record.stage != "admitted"
            {
                return Err(Error::Invalid("runtime_admission_not_recorded"));
            }
            if !matches!(
                record.stage.as_str(),
                "requires_reconciliation" | "reconciled_completed"
            ) && record.admission_id.is_none()
            {
                return Err(Error::Invalid("missing_runtime_admission_id"));
            }
            if let Some(ref value) = record.answer_digest {
                digest(value)?;
            }
            if old.answer_digest.is_some() && record.answer_digest != old.answer_digest {
                return Err(Error::Invalid("runtime_answer_changed"));
            }
            if matches!(
                record.stage.as_str(),
                "model_completed" | "reconciled_completed"
            ) && record.answer_digest.is_none()
            {
                return Err(Error::Invalid("missing_runtime_answer_digest"));
            }
            if let Some(ref reason) = record.reason
                && (reason.is_empty()
                    || reason.len() > 96
                    || !reason
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
            {
                return Err(Error::Invalid("invalid_runtime_reason"));
            }
            let revision = old
                .revision
                .checked_add(1)
                .ok_or(Error::Invalid("runtime_revision_overflow"))?;
            tx.execute("UPDATE runtime_admissions SET admission_id=?1,stage=?2,answer_digest=?3,reason=?4,revision=?5 WHERE message_id=?6 AND recipient=?7",params![record.admission_id,record.stage,record.answer_digest,record.reason,revision,record.message_id,record.recipient])?;
            let result = read(&tx, &record.message_id, &record.recipient)?;
            authorize(&tx)?;
            tx.commit()?;
            Ok(result)
        }
    }
}
