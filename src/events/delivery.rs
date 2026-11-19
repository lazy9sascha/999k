//! Source: PigeonMQ e399161, src/dot_event_delivery.rs (MIT).
//! Shared Events delivery ledger over the existing queue and subscription tables.
//! This module never signs, sends, grants, creates tables, claims, or acknowledges.
//! A trusted host must retain its instance lock and use the approved signed,
//! DNS-pinned HTTPS transport with its existing ten-second callback bound.
//! Callback success is not native consumption.
use super::policy::{EventsPolicy, PolicySource, PreflightError};
use crate::mailbox_engine::PLAIN_MESSAGE as PLAIN;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, params, params_from_iter,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

type Result<T> = std::result::Result<T, PreflightError>;
fn error(reason: &'static str) -> PreflightError {
    PreflightError {
        reason,
        code: -32602,
    }
}
fn valid_identifier(value: &str) -> bool {
    !value
        .chars()
        .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        && value.len() <= 1024
        && !value.chars().any(|c| c < '\u{20}')
}
struct Message {
    stored_seq: i64,
    typed: bool,
    id: String,
    conversation_id: String,
    sender: String,
    body: String,
    correlation: Option<String>,
    parent: Option<String>,
    created_ms: i64,
}
impl Message {
    fn valid(&self) -> bool {
        let limit = if self.parent.is_some() { 16384 } else { 8192 };
        self.typed
            && self.body.len() <= limit
            && !self
                .body
                .chars()
                .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
            && valid_identifier(&self.id)
            && valid_identifier(&self.conversation_id)
            && valid_identifier(&self.sender)
            && self.correlation.as_deref().is_none_or(valid_identifier)
            && self.parent.as_deref().is_none_or(valid_identifier)
    }
}
fn message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    // SQLite affinity does not guarantee text. A malformed message is a stopped
    // outbox item, not a storage failure that blocks later eligible messages.
    fn text(
        row: &rusqlite::Row<'_>,
        index: usize,
        optional: bool,
        typed: &mut bool,
    ) -> rusqlite::Result<Option<String>> {
        use rusqlite::types::ValueRef;
        match row.get_ref(index)? {
            ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Ok(Some(text.to_owned())),
                Err(_) => {
                    *typed = false;
                    Ok(None)
                }
            },
            ValueRef::Null if optional => Ok(None),
            _ => {
                *typed = false;
                Ok(None)
            }
        }
    }
    let mut typed = true;
    let id = text(row, 0, false, &mut typed)?.unwrap_or_default();
    let sender = text(row, 1, false, &mut typed)?.unwrap_or_default();
    let body = text(row, 2, false, &mut typed)?.unwrap_or_default();
    let correlation = text(row, 3, true, &mut typed)?;
    let parent = text(row, 4, true, &mut typed)?;
    let conversation_id = text(row, 7, false, &mut typed)?.unwrap_or_default();
    let created_ms = match row.get_ref(5)? {
        rusqlite::types::ValueRef::Integer(value) => value,
        _ => {
            typed = false;
            0
        }
    };
    Ok(Message {
        stored_seq: row.get(6)?,
        typed,
        id,
        conversation_id,
        sender,
        body,
        correlation,
        parent,
        created_ms,
    })
}

struct Subscription {
    owner: String,
    id: String,
    url: String,
    secret: String,
    previous: Option<String>,
    rotation_until: Option<f64>,
    generation: i64,
}

/// Private fields prevent an arbitrary caller-supplied status from fabricating
/// a delivery reservation. No Debug/Serialize because this holds signing keys.
pub struct DeliveryAttempt {
    database: PathBuf,
    mailbox: String,
    subscription: Subscription,
    message_id: String,
    conversation_id: String,
    event_id: String,
    attempt: i64,
    sender: String,
    correlation: String,
    created_ms: i64,
    terminal_reply_to: Option<String>,
}
impl DeliveryAttempt {
    pub fn terminal_reply_to(&self) -> Option<&str> {
        self.terminal_reply_to.as_deref()
    }
    pub fn mailbox(&self) -> &str {
        &self.mailbox
    }
    pub fn owner(&self) -> &str {
        &self.subscription.owner
    }
    pub fn subscription_id(&self) -> &str {
        &self.subscription.id
    }
    pub fn event_id(&self) -> &str {
        &self.event_id
    }
    pub fn message_id(&self) -> &str {
        &self.message_id
    }
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    pub fn sender(&self) -> &str {
        &self.sender
    }
    pub fn correlation_id(&self) -> &str {
        &self.correlation
    }
    pub fn created_at_ms(&self) -> i64 {
        self.created_ms
    }
    pub fn callback_url(&self) -> &str {
        &self.subscription.url
    }
    pub fn signing_secret(&self) -> &str {
        &self.subscription.secret
    }
    pub fn previous_signing_secret(&self, signed_at: f64) -> Option<&str> {
        if self
            .subscription
            .rotation_until
            .is_some_and(|until| until > signed_at)
        {
            self.subscription.previous.as_deref()
        } else {
            None
        }
    }
    pub fn attempt(&self) -> i64 {
        self.attempt
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Applied,
    Superseded,
}

pub struct EventDelivery {
    db: PathBuf,
    policy: PolicySource,
    allow_nonexpiring: bool,
}
impl EventDelivery {
    /// Configuration only. Never prepares schema or creates a database.
    pub fn new(db: impl AsRef<Path>, policy: impl AsRef<Path>, allow_nonexpiring: bool) -> Self {
        let path = policy.as_ref().to_owned();
        Self::with_policy(
            db,
            std::sync::Arc::new(move || EventsPolicy::load(&path)),
            allow_nonexpiring,
        )
    }
    pub fn with_policy(
        db: impl AsRef<Path>,
        policy: PolicySource,
        allow_nonexpiring: bool,
    ) -> Self {
        Self {
            db: db.as_ref().to_owned(),
            policy,
            allow_nonexpiring,
        }
    }
    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        crate::validate_schema(&conn).map_err(|err| match err {
            crate::Error::Database(error) => error.into(),
            _ => error("requires_current_schema_queue"),
        })?;
        Ok(conn)
    }
    fn policy(&self) -> Result<EventsPolicy> {
        (self.policy)().map_err(|err| {
            if err.reason == "setup_or_storage_error" {
                PreflightError {
                    reason: "policy_unavailable",
                    code: -32003,
                }
            } else {
                err
            }
        })
    }
    fn time(now: f64) -> Result<()> {
        if now.is_finite() {
            Ok(())
        } else {
            Err(error("invalid_time"))
        }
    }

    /// Enqueue at most 100 eligible messages per currently authorized subscription.
    /// The identifiers returned are internal tick candidates, not public output.
    pub fn enqueue(&self, mut clock: impl FnMut() -> f64) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = self.policy()?; // Revalidate after acquiring the storage lock.
        let now = clock();
        Self::time(now)?;
        let subscriptions:Vec<(String,String)>=tx.prepare("SELECT id,url FROM dot_event_subscriptions WHERE active=1 AND owner=?1 AND (expires>?2 OR (expires=0 AND ?3))")?
            .query_map(params![policy.principal(),now,self.allow_nonexpiring],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<std::result::Result<_,_>>()?;
        let mut candidates = Vec::new();
        for (sid, url) in subscriptions {
            if !policy.callback_allowed(&url) {
                continue;
            }
            candidates.push(sid.clone());
            let marks = vec!["?"; policy.allowed_senders().len()].join(",");
            let cutoff = if policy.dot_subscription_cutoff() {
                Some(super::subscriptions::dot_start_seq(&tx, &sid)?)
            } else {
                None
            };
            let fresh = if cutoff.is_some() { "AND m.seq>?" } else { "" };
            let sql = format!(
                "SELECT m.id,m.sender,m.body,m.correlation_id,m.in_reply_to,m.created_at_ms,m.seq,m.conversation_id FROM messages m WHERE m.recipient=? AND m.dead_lettered_at_ms IS NULL AND m.acknowledged_at_ms IS NULL AND m.attempts=0 AND m.sender IN ({marks}) AND {PLAIN} {fresh} AND NOT EXISTS(SELECT 1 FROM dot_event_outbox o WHERE o.message_id=m.id AND o.subscription_id=?) ORDER BY m.seq LIMIT 100"
            );
            let mut args: Vec<rusqlite::types::Value> = std::iter::once(policy.mailbox())
                .chain(policy.allowed_senders().iter().map(String::as_str))
                .map(|value| value.to_owned().into())
                .collect();
            if let Some(seq) = cutoff {
                args.push(seq.into());
            }
            args.push(sid.clone().into());
            let rows: Vec<Message> = tx
                .prepare(&sql)?
                .query_map(params_from_iter(args), message)?
                .collect::<std::result::Result<_, _>>()?;
            for row in rows {
                let key =
                    serde_json::to_vec(&[&sid, &row.id]).map_err(|_| error("invalid_input"))?;
                let digest: String = Sha256::digest(key)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let state = if row.valid() { "pending" } else { "stopped" };
                // Copy the stored primary key inside SQLite, without text coercion.
                // Even invalid UTF-8/BLOB IDs remain identifiable and deduplicated.
                tx.execute("INSERT OR IGNORE INTO dot_event_outbox(subscription_id,message_id,event_id,state) SELECT ?1,m.id,?3,?4 FROM messages m WHERE m.seq=?2",params![sid,row.stored_seq,format!("evt_{digest}"),state])?;
            }
        }
        tx.commit()?;
        Ok(candidates)
    }

    /// Atomically reserve an attempt before network I/O. Retry backoff persists
    /// across restart. The caller must not automatically resend an old attempt.
    pub fn reserve(
        &self,
        sid: &str,
        mut clock: impl FnMut() -> f64,
    ) -> Result<Option<DeliveryAttempt>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = self.policy()?;
        let now = clock();
        Self::time(now)?;
        let sub=tx.query_row("SELECT s.id,s.url,s.secret,s.previous_secret,s.rotation_until,COALESCE(g.generation,0) FROM dot_event_subscriptions s LEFT JOIN dot_subscription_generations g ON g.subscription_id=s.id WHERE s.id=?1 AND active=1 AND owner=?2 AND (expires>?3 OR (expires=0 AND ?4))",params![sid,policy.principal(),now,self.allow_nonexpiring],|r|Ok(Subscription { owner:policy.principal().to_owned(), id:r.get(0)?,url:r.get(1)?,secret:r.get(2)?,previous:r.get(3)?,rotation_until:r.get(4)?,generation:r.get(5)? })).optional()?;
        let Some(sub) = sub else { return Ok(None) };
        if !policy.callback_allowed(&sub.url) {
            return Ok(None);
        };
        let cutoff = if policy.dot_subscription_cutoff() {
            Some(super::subscriptions::dot_start_seq(&tx, sid)?)
        } else {
            None
        };
        // A fifth callback can still be in flight. Keep its result applicable
        // until its persisted reservation/backoff deadline, including restarts.
        let exhausted = tx.execute("UPDATE dot_event_outbox SET state='stopped' WHERE subscription_id=?1 AND state='pending' AND attempts>=5 AND next_at<=?2",params![sid,now])?;
        let commit = |tx: rusqlite::Transaction<'_>| -> Result<()> {
            tx.commit()?;
            if exhausted != 0 {
                tracing::warn!(
                    target: "pigeonmq::events",
                    component = "events_callback", event = "failure",
                    outcome = "reservation_exhausted", count = exhausted
                );
            }
            Ok(())
        };
        struct Candidate {
            seq: i64,
            id: Option<String>,
            event_id: Option<String>,
            attempts: Option<i64>,
        }
        let fresh = if cutoff.is_some() { "AND m.seq>?3" } else { "" };
        let mut args: Vec<rusqlite::types::Value> = vec![sid.to_owned().into(), now.into()];
        if let Some(seq) = cutoff {
            args.push(seq.into());
        }
        // Filter before LIMIT so a stale outbox entry cannot block a fresh reply.
        let item = tx.query_row(&format!("SELECT m.seq,o.message_id,o.event_id,o.attempts FROM dot_event_outbox o JOIN messages m ON m.id=o.message_id WHERE o.subscription_id=?1 AND o.state='pending' AND o.attempts<5 AND o.next_at<=?2 AND m.dead_lettered_at_ms IS NULL AND m.acknowledged_at_ms IS NULL AND m.attempts=0 {fresh} ORDER BY m.seq LIMIT 1"),params_from_iter(args),|r|Ok(Candidate {
            seq: r.get(0)?, id: r.get(1).ok(), event_id: r.get(2).ok(), attempts: r.get(3).ok(),
        })).optional()?;
        let Some(item) = item else {
            commit(tx)?;
            return Ok(None);
        };
        let valid = item.id.as_deref().is_some_and(valid_identifier)
            && item.event_id.as_deref().is_some_and(valid_identifier)
            && item.attempts.is_some_and(|v| (0..5).contains(&v));
        if !valid {
            // Preserve SQLite identity when a persisted field is malformed.
            // One corrupt item must not abort every later delivery pass.
            tx.execute("UPDATE dot_event_outbox SET state='stopped' WHERE subscription_id=?1 AND message_id=(SELECT id FROM messages WHERE seq=?2)",params![sid,item.seq])?;
            commit(tx)?;
            return Ok(None);
        }
        let (id, event_id, attempts) = (
            item.id.expect("validated id"),
            item.event_id.expect("validated event id"),
            item.attempts.expect("validated attempts"),
        );
        let row=tx.query_row(&format!("SELECT m.id,m.sender,m.body,m.correlation_id,m.in_reply_to,m.created_at_ms,m.seq,m.conversation_id FROM messages m WHERE m.id=?1 AND m.recipient=?2 AND {PLAIN}"),params![id,policy.mailbox()],message).optional()?;
        let row = row.filter(|r| {
            r.id.len() <= 128
                && r.valid()
                && policy.allowed_senders().contains(&r.sender)
                && cutoff.is_none_or(|seq| r.stored_seq > seq)
        });
        let Some(row) = row else {
            tx.execute("UPDATE dot_event_outbox SET state='stopped' WHERE subscription_id=?1 AND message_id=?2",params![sid,id])?;
            commit(tx)?;
            return Ok(None);
        };
        let attempt = attempts + 1;
        tx.execute("UPDATE dot_event_outbox SET attempts=?1,next_at=?2 WHERE subscription_id=?3 AND message_id=?4",params![attempt,now+(1u32<<attempt) as f64,sid,id])?;
        commit(tx)?;
        Ok(Some(DeliveryAttempt {
            database: self.db.clone(),
            mailbox: policy.mailbox().to_owned(),
            subscription: sub,
            message_id: id,
            conversation_id: row.conversation_id,
            event_id,
            attempt,
            sender: row.sender,
            correlation: row.correlation.unwrap_or(row.id),
            created_ms: row.created_ms,
            terminal_reply_to: if policy.dot_subscription_cutoff() {
                row.parent
            } else {
                None
            },
        }))
    }

    /// Apply a response only to the exact persisted generation and attempt.
    /// Status means signed callback delivery, never message consumption or ACK.
    pub fn finish(&self, attempt: &DeliveryAttempt, status: u16) -> Result<DeliveryOutcome> {
        if attempt.database != self.db || (self.policy)()?.mailbox() != attempt.mailbox {
            return Err(error("delivery_database_mismatch"));
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let generation:Option<i64>=tx.query_row("SELECT COALESCE(g.generation,0) FROM dot_event_subscriptions s LEFT JOIN dot_subscription_generations g ON g.subscription_id=s.id WHERE s.id=?1",[&attempt.subscription.id],|r|r.get(0)).optional()?;
        if generation != Some(attempt.subscription.generation) {
            return Ok(DeliveryOutcome::Superseded);
        };
        let retryable = status == 408 || status == 429 || status >= 500;
        let (state, diagnostic) = if (200..300).contains(&status) {
            ("accepted", None)
        } else if retryable && attempt.attempt < 5 {
            (
                "pending",
                (attempt.attempt == 1).then_some("retry_scheduled"),
            )
        } else {
            (
                "stopped",
                Some(if retryable {
                    "retry_exhausted"
                } else {
                    "rejected"
                }),
            )
        };
        let changed=tx.execute("UPDATE dot_event_outbox SET state=?1 WHERE subscription_id=?2 AND message_id=?3 AND attempts=?4 AND state='pending'",params![state,attempt.subscription.id,attempt.message_id,attempt.attempt])?;
        if changed != 0 && status == 410 {
            tx.execute(
                "UPDATE dot_event_subscriptions SET active=0 WHERE id=?1",
                [&attempt.subscription.id],
            )?;
        }
        tx.commit()?;
        if changed != 0
            && let Some(outcome) = diagnostic
        {
            tracing::warn!(
                target: "pigeonmq::events",
                component = "events_callback", event = "failure", outcome,
                attempt = attempt.attempt, decision_status = status
            );
        }
        Ok(if changed == 0 {
            DeliveryOutcome::Superseded
        } else {
            DeliveryOutcome::Applied
        })
    }
}
