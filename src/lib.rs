//! Local durable inbox with leased delivery. Names/correlation are not authentication.
pub mod adapters;
pub mod arrival_notify;
pub mod conversation;
#[path = "mailbox/deferred.rs"]
mod deferred_mailbox;
pub mod deferred_terminal;
#[path = "adapters/dot/dot_mailbox.rs"]
pub mod dot_mailbox;
#[path = "adapters/dot/dot_mcp_host.rs"]
pub mod dot_mcp_host;
#[path = "adapters/dot/mod.rs"]
pub mod dot_profile;
#[path = "adapters/dot/dot_service_preflight.rs"]
pub mod dot_service_preflight;
pub mod events;
#[path = "adapters/grok_bot/mailbox.rs"]
pub mod grok_mailbox;
#[path = "adapters/grok_bot/grok_wake.rs"]
pub mod grok_wake;
pub mod hub_http;
pub mod inbox_page;
pub mod logging;
#[path = "mailbox/engine.rs"]
mod mailbox_engine;
use mailbox_engine::{Ack as CompletionAck, ReplyCommit, commit_reply, insert_message};
#[path = "mcp/catalog.rs"]
pub mod mailbox_catalog;
#[path = "mcp/mailbox_tools.rs"]
pub mod mailbox_tools;
#[path = "mcp/mcp_request.rs"]
pub mod mcp_request;
#[path = "mcp/mcp_sdk.rs"]
pub mod mcp_sdk;
#[path = "adapters/muse/mod.rs"]
pub mod muse_mailbox;
pub mod private_file;
mod queue_progress;
mod queue_read;
#[path = "receiver/receiver_api.rs"]
pub mod receiver_api;
#[path = "receiver/receiver_client.rs"]
pub mod receiver_client;
pub mod registration;
pub mod runtime_admission;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    #[error("idempotency key reused with different envelope")]
    Conflict,
    #[error("message not found for recipient")]
    NotFound,
    #[error("lease expired or receipt does not match the latest delivery")]
    StaleReceipt,
    #[error("unsupported database schema version: {0}")]
    Schema(i64),
}
pub type Result<T> = std::result::Result<T, Error>;

/// The only supported persistent queue schema. Existing databases are never converted.
pub const SCHEMA_VERSION: i64 = 4;
const MAX_ATTEMPTS: i64 = 5;

pub(crate) fn validate_schema(connection: &Connection) -> Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(Error::Schema(version));
    }
    let application: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if application != 1346851153 {
        return Err(Error::Invalid("not a PigeonMQ database"));
    }
    Ok(())
}

enum ReceiveSelection<'a> {
    Observed(i64, &'a str, Option<bool>),
    FirstRequest { id: &'a str, sender: &'a str },
}

/// Immutable envelope. Body is UTF-8 text; JSON may be carried as text.
/// An optional key is scoped to (sender, recipient) and retained with the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub correlation_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub idempotency_key: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub envelope: Envelope,
    pub created_at_ms: i64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct SendResult {
    pub message: Message,
    pub duplicate: bool,
}
/// Receipt rotates on redelivery; it is a fencing token, not an identity credential.
#[derive(Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub message: Message,
    pub receipt: String,
    pub lease_until_ms: i64,
    pub attempt: i64,
}
/// ID-scoped operator result. Does not expose the envelope or delivery receipt.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeadLetterInspection {
    DeadLettered {
        attempts: i64,
        dead_lettered_at_ms: i64,
    },
    NotDeadLetteredOrMissing,
}

#[derive(Debug, Serialize)]
pub struct DeadLetterEntry {
    id: String,
    #[serde(flatten)]
    inspection: DeadLetterInspection,
}

#[derive(Debug, Serialize)]
pub struct DeadLetterPage {
    entries: Vec<DeadLetterEntry>,
    next_continuation: Option<String>,
}

/// Aggregate of pending ordinary requests without a delivery claim.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct NeverClaimedBacklog {
    pub count: i64,
    pub oldest_age_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckResult {
    Acknowledged,
    AlreadyAcknowledged,
}

/// An application result, not a queue receipt or independently verified completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Success { value: serde_json::Value },
    Error { code: String, message: String },
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcState {
    Pending,
    Claimed,
    Answered,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcStatus {
    pub request: Message,
    pub state: RpcState,
    pub result: Option<Outcome>,
}

/// Existing ordinary request body bound of the shared mailbox reader.
pub const MAX_ORDINARY_REQUEST_BYTES: usize = 8192;

pub struct Queue {
    connection: Connection,
}
fn now_ms() -> Result<i64> {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("clock before Unix epoch"))?
        .as_millis();
    i64::try_from(n).map_err(|_| Error::Invalid("clock overflow"))
}

fn py_strip_empty(value: &str) -> bool {
    value.chars().all(|c| {
        matches!(
            c,
            '\t' | '\n'
                | '\x0b'
                | '\x0c'
                | '\r'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\x1f'
                | ' '
                | '\u{85}'
                | '\u{a0}'
        ) || ('\u{1680}'..='\u{1680}').contains(&c)
            || ('\u{2000}'..='\u{200a}').contains(&c)
            || ('\u{2028}'..='\u{2029}').contains(&c)
            || c == '\u{202f}'
            || c == '\u{205f}'
            || c == '\u{3000}'
    })
}

fn nonempty(value: &str) -> Result<()> {
    if py_strip_empty(value) {
        return Err(Error::Invalid(
            "names, identifiers and keys must not be blank",
        ));
    }
    Ok(())
}
fn read_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        envelope: Envelope {
            sender: row.get(1)?,
            recipient: row.get(2)?,
            body: row.get(3)?,
            correlation_id: row.get(4)?,
            in_reply_to: row.get(5)?,
            idempotency_key: row.get(6)?,
        },
        created_at_ms: row.get(7)?,
        conversation_id: row.get(8)?,
    })
}
const COLUMNS: &str = "id,sender,recipient,body,correlation_id,in_reply_to,idempotency_key,created_at_ms,conversation_id";

// Canonical adapter reply replay uses the exact key, never legacy-prefix aliases.
// The caller holds BEGIN IMMEDIATE through this lookup and any subsequent insert.
fn adapter_reply_replay(
    tx: &rusqlite::Transaction<'_>,
    envelope: &Envelope,
) -> Result<Option<Message>> {
    let (Some(original_id), Some(key)) = (&envelope.in_reply_to, &envelope.idempotency_key) else {
        return Ok(None);
    };
    let keys = [format!("gw-reply-{}", original_id)];
    if !keys.contains(key) {
        return Ok(None);
    }
    let original = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM messages WHERE id=?1 AND in_reply_to IS NULL AND NOT EXISTS (SELECT 1 FROM rpc_requests WHERE message_id=messages.id)"),
            [original_id],
            read_message,
        )
        .optional()?;
    let Some(original) = original else {
        return Ok(None);
    };
    if envelope.sender != original.envelope.recipient
        || envelope.recipient != original.envelope.sender
        || envelope.correlation_id.as_deref()
            != Some(
                original
                    .envelope
                    .correlation_id
                    .as_deref()
                    .unwrap_or(original_id),
            )
    {
        return Ok(None);
    }
    let mut statement = tx.prepare(&format!(
        "SELECT {COLUMNS} FROM messages WHERE sender=?1 AND recipient=?2
         AND idempotency_key = ?3"
    ))?;
    let mut matches = statement.query_map(
        params![envelope.sender, envelope.recipient, keys[0]],
        read_message,
    )?;
    let existing = matches.next().transpose()?;
    if matches.next().transpose()?.is_some() {
        return Err(Error::Conflict);
    }
    if let Some(message) = &existing {
        let was_rpc: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
            [&message.id],
            |r| r.get(0),
        )?;
        // A canonical key with a mismatched envelope requires reconciliation.
        // Never rewrite the stored envelope or acknowledge its original here.
        if was_rpc
            || message.envelope.body != envelope.body
            || message.envelope.in_reply_to != envelope.in_reply_to
            || message.envelope.correlation_id != envelope.correlation_id
        {
            return Err(Error::Conflict);
        }
    }
    Ok(existing)
}
// Inspect only: changing journal mode here could alter an existing installation.
fn validate_queue_journal(connection: &Connection) -> Result<()> {
    let mode: String = connection.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    // Explicit in-memory queues are test/local ephemeral storage, not durable files.
    let in_memory = connection.path().is_none_or(str::is_empty);
    if matches!(mode.as_str(), "delete" | "truncate" | "persist") || (in_memory && mode == "memory")
    {
        Ok(())
    } else {
        Err(Error::Invalid("unsupported_queue_journal_mode"))
    }
}
impl Queue {
    /// Open the current queue only. Never create a database or migrate its schema.
    // Q-001: open existing queue. No creation, no migration.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        validate_schema(&connection)?;
        validate_queue_journal(&connection)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { connection })
    }
    /// Open a current queue or create an empty queue. Never convert an existing schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != 0 {
            validate_schema(&connection)?;
        }
        // Reject old versions before journal checks, writable PRAGMAs, or DDL.
        validate_queue_journal(&connection)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            let application: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
            let tables: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            if tables != 0 || application != 0 {
                return Err(Error::Invalid("database is not empty"));
            }
            tx.execute_batch(
                "CREATE TABLE messages (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                conversation_id TEXT NOT NULL,
                sender TEXT NOT NULL, recipient TEXT NOT NULL, body TEXT NOT NULL,
                correlation_id TEXT, in_reply_to TEXT, idempotency_key TEXT,
                created_at_ms INTEGER NOT NULL,
                receipt TEXT, lease_until_ms INTEGER, attempts INTEGER NOT NULL DEFAULT 0,
                acknowledged_at_ms INTEGER, dead_lettered_at_ms INTEGER,
                UNIQUE(sender,recipient,idempotency_key)
            );
            CREATE INDEX inbox ON messages(recipient,acknowledged_at_ms,seq);
            CREATE INDEX message_thread ON messages(conversation_id,seq);
            CREATE TABLE rpc_requests (
                message_id TEXT PRIMARY KEY REFERENCES messages(id), outcome_json TEXT
            );
            PRAGMA application_id=1346851153;",
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else {
            validate_schema(&tx)?;
        }
        tx.commit()?;
        Ok(Self { connection })
    }
    /// Returns only after commit. Same key/envelope replays return the original message.
    // Q-001, Q-002, Q-005, Q-006, Q-007: commit, assign id/timestamp, nonblank, no body inspection, no field injection.
    pub fn send(&mut self, envelope: Envelope) -> Result<SendResult> {
        self.send_kind(envelope, false, None, None, || Ok(()))
    }
    /// Creates a request on the same queue. Request ID is the returned message ID.
    // RPC-001, RPC-002: store RPC request, return message id as request id.
    pub fn request(&mut self, envelope: Envelope) -> Result<SendResult> {
        self.send_kind(envelope, true, None, None, || Ok(()))
    }
    /// Trusted host reauthorization before lock acquisition and immediately before commit.
    pub fn send_checked(
        &mut self,
        envelope: Envelope,
        authorize: impl FnMut() -> Result<()>,
    ) -> Result<SendResult> {
        self.send_kind(envelope, false, None, None, authorize)
    }
    fn send_kind(
        &mut self,
        envelope: Envelope,
        rpc: bool,
        context: Option<&conversation::Context>,
        continuation: Option<conversation::Continuation<'_>>,
        mut authorize: impl FnMut() -> Result<()>,
    ) -> Result<SendResult> {
        authorize()?;
        nonempty(&envelope.sender)?;
        nonempty(&envelope.recipient)?;
        for value in [
            &envelope.correlation_id,
            &envelope.in_reply_to,
            &envelope.idempotency_key,
        ]
        .into_iter()
        .flatten()
        {
            nonempty(value)?;
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize()?;
        let thread = continuation
            .map(|from| from.resolve(&tx, &envelope))
            .transpose()?;
        let mut replay = None;
        if let Some(key) = &envelope.idempotency_key {
            let existing = tx.query_row(&format!("SELECT {COLUMNS} FROM messages WHERE sender=?1 AND recipient=?2 AND idempotency_key=?3"), params![envelope.sender,envelope.recipient,key], read_message).optional()?;
            if let Some(message) = existing {
                let was_rpc: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                    [&message.id],
                    |r| r.get(0),
                )?;
                if message.envelope != envelope || was_rpc != rpc {
                    return Err(Error::Conflict);
                }
                replay = Some(message);
            }
        }
        if !rpc {
            replay = adapter_reply_replay(&tx, &envelope)?.or(replay);
        }
        if let Some(message) = replay {
            if conversation::load(&tx, &message.id)?.as_ref() != context
                || thread
                    .as_ref()
                    .is_some_and(|root| *root != message.conversation_id)
                || (thread.is_none()
                    && message.envelope.in_reply_to.is_none()
                    && message.id != message.conversation_id)
            {
                return Err(Error::Conflict);
            }
            authorize()?;
            tx.commit()?;
            tracing::info!(
                component = "queue",
                event = "queue_send_replayed",
                outcome = "duplicate",
                message_id = message.id.as_str()
            );
            return Ok(SendResult {
                message,
                duplicate: true,
            });
        }
        let message = insert_message(&tx, envelope, now_ms()?, thread.as_deref())?;
        if let Some(context) = context {
            conversation::store(&tx, &message.id, context)?;
        }
        if rpc {
            tx.execute(
                "INSERT INTO rpc_requests(message_id) VALUES (?1)",
                [&message.id],
            )?;
        }
        authorize()?;
        tx.commit()?;
        tracing::info!(
            component = "queue",
            event = "queue_accepted",
            outcome = "committed",
            message_id = message.id.as_str()
        );
        Ok(SendResult {
            message,
            duplicate: false,
        })
    }
    /// Claims the oldest currently available message. No wait/poll loop is started.
    // Q-008, Q-009, Q-010, Q-011: claim oldest, rotate receipt, exclude RPC, positive lease.
    pub fn receive(&mut self, recipient: &str, lease: Duration) -> Result<Option<Delivery>> {
        self.receive_kind(recipient, lease, now_ms, false)
    }
    /// Claim only this sender's first ordinary request for this mailbox.
    pub fn receive_first_exact(
        &mut self,
        recipient: &str,
        id: &str,
        sender: &str,
        lease: Duration,
    ) -> Result<Option<Delivery>> {
        nonempty(id)?;
        nonempty(sender)?;
        self.receive_selected(
            recipient,
            lease,
            now_ms,
            false,
            Some(ReceiveSelection::FirstRequest { id, sender }),
            || Ok(()),
        )
    }
    /// Extends the live lease held by `receipt` to now + `lease`. An expired,
    /// stale, or acknowledged claim is never revived.
    pub fn renew_lease(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        lease: Duration,
    ) -> Result<i64> {
        self.renew_lease_guarded(recipient, id, receipt, lease, |_| Ok(()))
    }
    pub(crate) fn renew_lease_guarded(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        lease: Duration,
        authorize: impl Fn(&Connection) -> Result<()>,
    ) -> Result<i64> {
        let millis =
            i64::try_from(lease.as_millis()).map_err(|_| Error::Invalid("lease overflow"))?;
        if millis <= 0 {
            return Err(Error::Invalid("lease must be at least one millisecond"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx)?;
        let now = now_ms()?;
        let until = now
            .checked_add(millis)
            .ok_or(Error::Invalid("lease overflow"))?;
        let changed = tx.execute(
            "UPDATE messages SET lease_until_ms=?1 WHERE id=?2 AND recipient=?3 AND receipt=?4 AND acknowledged_at_ms IS NULL AND dead_lettered_at_ms IS NULL AND lease_until_ms>?5",
            params![until, id, recipient, receipt, now],
        )?;
        if changed != 1 {
            return Err(Error::StaleReceipt);
        }
        authorize(&tx)?;
        tx.commit()?;
        Ok(until)
    }
    /// Claims one RPC request using the queue's existing lease/receipt fence.
    // RPC-003, RPC-004, RPC-008, RPC-009: claim RPC only, use receipt/lease rules.
    pub fn receive_request(
        &mut self,
        recipient: &str,
        lease: Duration,
    ) -> Result<Option<Delivery>> {
        self.receive_kind(recipient, lease, now_ms, true)
    }
    #[cfg(test)]
    fn receive_with_clock(
        &mut self,
        recipient: &str,
        lease: Duration,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<Option<Delivery>> {
        self.receive_kind(recipient, lease, clock, false)
    }
    fn receive_kind(
        &mut self,
        recipient: &str,
        lease: Duration,
        clock: impl FnOnce() -> Result<i64>,
        rpc: bool,
    ) -> Result<Option<Delivery>> {
        self.receive_selected(recipient, lease, clock, rpc, None, || Ok(()))
    }
    fn receive_selected(
        &mut self,
        recipient: &str,
        lease: Duration,
        clock: impl FnOnce() -> Result<i64>,
        rpc: bool,
        candidate: Option<ReceiveSelection<'_>>,
        mut authorize: impl FnMut() -> Result<()>,
    ) -> Result<Option<Delivery>> {
        self.receive_selected_scoped(recipient, lease, clock, rpc, candidate, |_| authorize())
    }
    fn receive_selected_scoped(
        &mut self,
        recipient: &str,
        lease: Duration,
        clock: impl FnOnce() -> Result<i64>,
        rpc: bool,
        candidate: Option<ReceiveSelection<'_>>,
        mut authorize: impl FnMut(&Connection) -> Result<()>,
    ) -> Result<Option<Delivery>> {
        authorize(&self.connection)?;
        nonempty(recipient)?;

        let millis =
            i64::try_from(lease.as_millis()).map_err(|_| Error::Invalid("lease overflow"))?;
        if millis <= 0 {
            return Err(Error::Invalid("lease must be at least one millisecond"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx)?;
        let now = clock()?; // Read after acquiring the write lock, not before waiting for it.
        let until = now
            .checked_add(millis)
            .ok_or(Error::Invalid("lease overflow"))?;
        let (seq, id, first, sender) = match candidate {
            Some(ReceiveSelection::Observed(seq, id, first)) => (Some(seq), Some(id), first, None),
            Some(ReceiveSelection::FirstRequest { id, sender }) => {
                (None, Some(id), Some(true), Some(sender))
            }
            None => (None, None, None, None),
        };
        let message = loop {
            let message = tx.query_row(&format!("SELECT {COLUMNS} FROM messages WHERE recipient=?1 AND acknowledged_at_ms IS NULL AND dead_lettered_at_ms IS NULL AND (lease_until_ms IS NULL OR lease_until_ms<=?2) AND EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)=?3 AND (?4 IS NULL OR seq=?4) AND (?5 IS NULL OR id=?5) AND (?6 IS NULL OR (?6 AND attempts=0 AND receipt IS NULL) OR (NOT ?6 AND attempts>0)) AND (?7 IS NULL OR (sender=?7 AND in_reply_to IS NULL)) ORDER BY seq LIMIT 1"), params![recipient,now,rpc,seq,id,first,sender], read_message).optional()?;
            let Some(message) = message else {
                authorize(&tx)?;
                tx.commit()?;
                return Ok(None);
            };
            if !rpc {
                let changed = tx.execute(
                    "UPDATE messages SET dead_lettered_at_ms=?1 WHERE id=?2 AND recipient=?3
                     AND dead_lettered_at_ms IS NULL AND acknowledged_at_ms IS NULL
                     AND in_reply_to IS NULL AND attempts>=?4 AND receipt IS NOT NULL
                     AND lease_until_ms IS NOT NULL AND lease_until_ms<=?1
                     AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)
                     AND NOT EXISTS(SELECT 1 FROM messages reply WHERE reply.in_reply_to=messages.id)",
                    params![now, message.id, recipient, MAX_ATTEMPTS],
                )?;
                if changed != 0 {
                    authorize(&tx)?;
                    continue;
                }
            }
            break message;
        };
        let receipt = Uuid::new_v4().to_string();
        tx.execute(
            "UPDATE messages SET receipt=?1,lease_until_ms=?2,attempts=attempts+1 WHERE id=?3",
            params![receipt, until, message.id],
        )?;
        let attempt = tx.query_row(
            "SELECT attempts FROM messages WHERE id=?1",
            [&message.id],
            |r| r.get(0),
        )?;
        authorize(&tx)?;
        tx.commit()?;
        Ok(Some(Delivery {
            message,
            receipt,
            lease_until_ms: until,
            attempt,
        }))
    }
    /// Only the latest, unexpired delivery may ack. Repeating a successful receipt is safe.
    // Q-012, Q-013, Q-014, Q-015, Q-016: latest receipt, idempotent, stale rejection, RPC rejection.
    pub fn ack(&mut self, recipient: &str, id: &str, receipt: &str) -> Result<AckResult> {
        self.ack_with_clock(recipient, id, receipt, now_ms)
    }
    // #FEEDBACK: Only excluded delivery and conversation code called this wrapper. Remove it. Keep the ACK path and its receipt checks.
    pub(crate) fn ack_guarded(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        guard: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<AckResult> {
        self.ack_with_clock_and_guard(recipient, id, receipt, now_ms, guard)
    }
    fn ack_with_clock(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<AckResult> {
        self.ack_with_clock_and_guard(recipient, id, receipt, clock, |_| Ok(()))
    }
    fn ack_with_clock_and_guard(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        clock: impl FnOnce() -> Result<i64>,
        guard: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<AckResult> {
        nonempty(recipient)?;
        nonempty(id)?;
        nonempty(receipt)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = clock()?;
        let state: Option<(Option<String>, Option<i64>, Option<i64>)> = tx.query_row("SELECT receipt,lease_until_ms,acknowledged_at_ms FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL", params![id,recipient], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (stored, until, acknowledged) = state.ok_or(Error::NotFound)?;
        let rpc: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if rpc {
            return Err(Error::Invalid("RPC requests require respond, not ack"));
        }
        if stored.as_deref() != Some(receipt) {
            return Err(Error::StaleReceipt);
        }
        crate::runtime_admission::validate_terminal(&tx, id, recipient)?;
        guard(&tx)?;
        let result = if acknowledged.is_some() {
            AckResult::AlreadyAcknowledged
        } else {
            if until.is_none_or(|until| until <= now) {
                return Err(Error::StaleReceipt);
            }
            tx.execute(
                "UPDATE messages SET acknowledged_at_ms=?1 WHERE id=?2",
                params![now, id],
            )?;
            AckResult::Acknowledged
        };
        tx.commit()?;
        tracing::info!(
            component = "queue",
            event = "message_ack",
            outcome = if acknowledged.is_some() {
                "already_acknowledged"
            } else {
                "committed"
            },
            message_id = id
        );
        Ok(result)
    }
    /// Atomically sends a reply message and ACKs the original. Single transaction.
    /// Validates sender/recipient/correlation/in_reply_to against the original message.
    /// Prevents duplicate replies: if ANY reply exists for original_id, verify it matches.
    /// Returns the reply message. Fails if lease expired, receipt stale, or validation fails.
    // Q-041, Q-042, Q-043, Q-044, Q-045, Q-046, Q-047, Q-048, Q-049, Q-050, Q-051, Q-052, Q-053: final reply completion.
    pub fn reply_and_ack(
        &mut self,
        recipient: &str,
        original_id: &str,
        receipt: &str,
        reply_envelope: Envelope,
    ) -> Result<Message> {
        self.reply_and_ack_guarded(recipient, original_id, receipt, reply_envelope, || Ok(()))
    }
    pub(crate) fn reply_and_ack_guarded(
        &mut self,
        recipient: &str,
        original_id: &str,
        receipt: &str,
        reply_envelope: Envelope,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Message> {
        self.reply_and_ack_mode(
            recipient,
            original_id,
            Some(receipt),
            reply_envelope,
            |_| authorize(),
        )
    }
    // #FEEDBACK: Muse completes through muse_mailbox::reply. Remove this unused local receiver wrapper. Keep the existing lease-first queue path.
    pub(crate) fn reply_and_ack_scoped(
        &mut self,
        recipient: &str,
        original_id: &str,
        receipt: &str,
        reply_envelope: Envelope,
        authorize: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<Message> {
        self.reply_and_ack_mode(
            recipient,
            original_id,
            Some(receipt),
            reply_envelope,
            authorize,
        )
    }
    // #FEEDBACK: Only excluded profiles called this API. Remove this wrapper and its no-receipt branch. Muse uses muse_mailbox::reply, not this transaction.
    /// Deferred final completion. The claim and ordinary atomic completion share
    /// one transaction. An existing unresolved lease-first claim is never adopted.
    pub fn reply_unclaimed_checked(
        &mut self,
        recipient: &str,
        original_id: &str,
        reply_envelope: Envelope,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Message> {
        self.reply_and_ack_mode(recipient, original_id, None, reply_envelope, |_| {
            authorize()
        })
    }
    fn reply_and_ack_mode(
        &mut self,
        recipient: &str,
        original_id: &str,
        receipt: Option<&str>,
        reply_envelope: Envelope,
        authorize: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<Message> {
        nonempty(recipient)?;
        nonempty(original_id)?;
        if let Some(receipt) = receipt {
            nonempty(receipt)?;
        }
        nonempty(&reply_envelope.sender)?;
        nonempty(&reply_envelope.recipient)?;

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx)?;
        let now = now_ms()?;
        let owned_receipt;
        let receipt = if let Some(receipt) = receipt {
            receipt
        } else {
            type Claim = (Option<String>, Option<i64>, Option<i64>, i64);
            let claim: Option<Claim> = tx.query_row(
                "SELECT receipt,lease_until_ms,acknowledged_at_ms,attempts FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL",
                params![original_id,recipient], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
            ).optional()?;
            let (stored, until, acknowledged, attempts) = claim.ok_or(Error::NotFound)?;
            owned_receipt = if acknowledged.is_some() {
                // A native completion retry still belongs to its recorded runtime owner.
                match runtime_admission::read(&tx, original_id, recipient) {
                    Ok(_) => return Err(Error::Invalid("existing claim requires reconciliation")),
                    Err(Error::Invalid("missing_runtime_admission")) => {}
                    Err(error) => return Err(error),
                }
                stored.ok_or(Error::Conflict)?
            } else {
                if attempts != 0 || stored.is_some() || until.unwrap_or(0) > now {
                    return Err(Error::Invalid("existing claim requires reconciliation"));
                }
                let fresh = Uuid::new_v4().to_string();
                tx.execute(
                    "UPDATE messages SET receipt=?1,lease_until_ms=?2,attempts=1 WHERE id=?3",
                    params![
                        fresh,
                        now.checked_add(1000).ok_or(Error::Conflict)?,
                        original_id
                    ],
                )?;
                fresh
            };
            &owned_receipt
        };

        // Look up original message to validate sender/recipient/correlation
        let original: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT sender, recipient, correlation_id FROM messages WHERE id=?1",
                [original_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (orig_sender, orig_recipient, orig_correlation) = original.ok_or(Error::NotFound)?;

        // Validate original recipient matches
        if orig_recipient != recipient {
            return Err(Error::NotFound);
        }

        // RPC requests require respond, not reply_and_ack
        let rpc: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
            [original_id],
            |r| r.get(0),
        )?;
        if rpc {
            return Err(Error::Invalid("RPC requests require respond, not reply"));
        }

        // Check lease validity
        let state: Option<(Option<String>, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT receipt, lease_until_ms, acknowledged_at_ms FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL",
                params![original_id, recipient],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (stored, until, acknowledged) = state.ok_or(Error::NotFound)?;
        if stored.as_deref() != Some(receipt) {
            return Err(Error::StaleReceipt);
        }
        if acknowledged.is_none() && until.is_none_or(|u| u <= now) {
            return Err(Error::StaleReceipt);
        }

        // RPC requests require respond, not reply_and_ack (check before any reply processing)
        let rpc: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
            [original_id],
            |r| r.get(0),
        )?;
        if rpc {
            return Err(Error::Invalid("RPC requests require respond, not reply"));
        }

        // Common validation: empty body check
        if py_strip_empty(&reply_envelope.body) {
            return Err(Error::Invalid("empty reply body"));
        }
        // Common validation: size limit
        if reply_envelope.body.len() > 16384 {
            return Err(Error::Invalid("reply too large"));
        }

        // Keys have exact sender/recipient scope. Legacy prefixes are not aliases.
        // Existing replies for this original are reconciled separately below.
        if let Some(key) = &reply_envelope.idempotency_key {
            let other_msg: Option<String> = tx
                .query_row(
                    "SELECT id FROM messages WHERE sender=?1 AND recipient=?2 AND idempotency_key=?3 AND (in_reply_to != ?4 OR in_reply_to IS NULL)",
                    params![reply_envelope.sender, reply_envelope.recipient, key, original_id],
                    |r| r.get(0),
                )
                .optional()?;
            if other_msg.is_some() {
                return Err(Error::Conflict);
            }
        }

        // Check if original message itself is a reply (terminal) - block re-reply
        let orig_is_reply: bool = tx.query_row(
            "SELECT in_reply_to IS NOT NULL FROM messages WHERE id=?1",
            [original_id],
            |r| r.get(0),
        )?;
        if orig_is_reply {
            return Err(Error::Invalid("cannot reply to a reply"));
        }

        // If original already ACKed or has existing reply, route to recovery path
        // (full validation + ACK) instead of creating new reply
        let reply_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE in_reply_to=?1)",
            [original_id],
            |r| r.get(0),
        )?;
        if acknowledged.is_some() || reply_exists {
            // RPC check: don't ACK RPC via reply_and_ack
            let rpc: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                [original_id],
                |r| r.get(0),
            )?;
            if rpc {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Validate reply sender matches expected agent
            if reply_envelope.sender != recipient {
                return Err(Error::Invalid("reply sender must be recipient"));
            }
            // Validate reply recipient matches original sender
            if reply_envelope.recipient != orig_sender {
                return Err(Error::Invalid("reply recipient must be original sender"));
            }
            // Single reply validation: check count
            let reply_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM messages WHERE in_reply_to=?1",
                [original_id],
                |r| r.get(0),
            )?;
            if reply_count > 1 {
                return Err(Error::Conflict);
            }
            // Stored RPC check: if stored reply's own ID is RPC, reject
            let stored_rpc_self: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id IN (SELECT id FROM messages WHERE in_reply_to=?1))",
                [original_id],
                |r| r.get(0),
            )?;
            if stored_rpc_self {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Stored RPC check: if stored reply's parent (original) is RPC, reject
            let stored_rpc: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                [original_id],
                |r| r.get(0),
            )?;
            if stored_rpc {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            if stored_rpc {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Original correlation validation: reply must match original's correlation
            match (&reply_envelope.correlation_id, &orig_correlation) {
                (Some(c), Some(oc)) if c != oc => return Err(Error::Conflict),
                (Some(c), None) if c.as_str() != original_id => return Err(Error::Conflict),
                (None, Some(_)) => return Err(Error::Conflict),
                _ => {}
            }
            // Idempotency key validation in ACKed block (same as existing reply block):
            let stored_key2: Option<String> = tx
                .query_row(
                    "SELECT idempotency_key FROM messages WHERE in_reply_to=?1 LIMIT 1",
                    [original_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            let new_key2 = &reply_envelope.idempotency_key;
            if stored_key2 != *new_key2 {
                return Err(Error::Conflict);
            }
            if let Some(key) = new_key2 {
                let other_msg: Option<String> = tx
                    .query_row(
                        "SELECT id FROM messages WHERE sender=?1 AND recipient=?2 AND idempotency_key=?3 AND (in_reply_to != ?4 OR in_reply_to IS NULL)",
                        params![reply_envelope.sender, reply_envelope.recipient, key, original_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if other_msg.is_some() {
                    return Err(Error::Conflict);
                }
            }

            // Stored reply correlation validation
            let stored_corr: Option<String> = tx
                .query_row(
                    "SELECT correlation_id FROM messages WHERE in_reply_to=?1 LIMIT 1",
                    [original_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            // Reject if both correlations are None
            if stored_corr.is_none() && reply_envelope.correlation_id.is_none() {
                return Err(Error::Conflict);
            }
            match (&stored_corr, &reply_envelope.correlation_id) {
                (Some(sc), Some(c)) if sc != c => return Err(Error::Conflict),
                (Some(_), None) => return Err(Error::Conflict),
                (None, Some(c)) if c.as_str() != original_id => return Err(Error::Conflict),
                _ => {}
            }
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT id, body FROM messages WHERE in_reply_to=?1 AND sender=?2 LIMIT 1",
                    params![original_id, reply_envelope.sender],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((existing_id, existing_body)) = existing {
                // Query full stored envelope for comparison
                let stored: (String, String, Option<String>, Option<String>) = tx.query_row(
                    "SELECT sender, recipient, correlation_id, in_reply_to FROM messages WHERE id=?1",
                    [existing_id.clone()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
                // Full envelope must match (not just body)
                if stored.0 != reply_envelope.sender
                    || stored.1 != reply_envelope.recipient
                    || stored.2 != reply_envelope.correlation_id
                    || stored.3 != reply_envelope.in_reply_to
                    || existing_body != reply_envelope.body
                {
                    return Err(Error::Conflict);
                }
                let mut msg = tx.query_row(
                    &format!("SELECT {} FROM messages WHERE id=?1", COLUMNS),
                    [existing_id],
                    read_message,
                )?;
                msg.conversation_id = conversation::parent_thread(&tx, original_id)?;
                commit_reply(
                    tx,
                    ReplyCommit {
                        original_id,
                        reply_id: &msg.id,
                        reply_new: false,
                        ack: if acknowledged.is_none() {
                            CompletionAck::Timestamp { now }
                        } else {
                            CompletionAck::Unchanged
                        },
                        binding: None,
                    },
                    || Ok(()),
                    Error::from,
                )?;
                return Ok(msg);
            }
            return Err(Error::Invalid("original already acknowledged"));
        }

        // Validate reply envelope: sender must be the recipient (agent), recipient must be original sender
        if reply_envelope.sender != recipient {
            return Err(Error::Invalid("reply sender must be recipient"));
        }
        if reply_envelope.recipient != orig_sender {
            return Err(Error::Invalid("reply recipient must be original sender"));
        }
        // Validate in_reply_to matches original_id (parent)
        match &reply_envelope.in_reply_to {
            Some(ir) if ir != original_id => {
                return Err(Error::Invalid("in_reply_to must match original"));
            }
            None => return Err(Error::Invalid("in_reply_to required")),
            _ => {}
        }
        // Validate correlation if present
        // Correlation: allow fallback (orig=None, reply=Some(original_id))
        match (&reply_envelope.correlation_id, &orig_correlation) {
            (Some(corr), Some(orig_corr)) if corr != orig_corr => {
                return Err(Error::Invalid("correlation mismatch"));
            }
            (Some(corr), None) if corr.as_str() != original_id => {
                return Err(Error::Invalid("correlation mismatch"));
            }
            (None, Some(_)) => return Err(Error::Invalid("correlation required on reply")),
            (None, None) => return Err(Error::Invalid("correlation required on reply")),
            _ => {}
        }

        // Check for ANY existing reply (regardless of idempotency key) to prevent duplicates
        #[allow(clippy::type_complexity)]
        let existing_replies: Vec<(
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = {
            let mut stmt = tx.prepare("SELECT id, body, correlation_id, idempotency_key, sender FROM messages WHERE in_reply_to=?1")?;
            stmt.query_map(params![original_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        if !existing_replies.is_empty() {
            // Multiple replies = conflict
            if existing_replies.len() > 1 {
                return Err(Error::Conflict);
            }
            let (existing_id, existing_body, existing_corr, _, existing_sender) =
                &existing_replies[0];

            // Only replies from the same agent are valid replies
            if *existing_sender != reply_envelope.sender {
                return Err(Error::Conflict);
            }

            // Empty body = conflict
            if py_strip_empty(existing_body) {
                return Err(Error::Conflict);
            }

            // Verify full envelope matches (same result = idempotent retry)
            let stored_env: (String, String, Option<String>, Option<String>) = tx.query_row(
                "SELECT sender, recipient, correlation_id, in_reply_to FROM messages WHERE id=?1",
                [existing_id.clone()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            if stored_env.0 != reply_envelope.sender
                || stored_env.1 != reply_envelope.recipient
                || stored_env.2 != reply_envelope.correlation_id
                || stored_env.3 != reply_envelope.in_reply_to
                || *existing_body != reply_envelope.body
            {
                return Err(Error::Conflict);
            }
            // Idempotency key validation:
            // Simple key validation: keys must match (or both None)
            let stored_key: Option<String> = tx
                .query_row(
                    "SELECT idempotency_key FROM messages WHERE in_reply_to=?1 LIMIT 1",
                    [original_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            let new_key = &reply_envelope.idempotency_key;
            if stored_key != *new_key {
                return Err(Error::Conflict);
            }
            // Ownership: key must not be bound to a different message
            if let Some(key) = new_key {
                let other_msg: Option<String> = tx
                    .query_row(
                        "SELECT id FROM messages WHERE sender=?1 AND recipient=?2 AND idempotency_key=?3 AND (in_reply_to != ?4 OR in_reply_to IS NULL)",
                        params![reply_envelope.sender, reply_envelope.recipient, key, original_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if other_msg.is_some() {
                    return Err(Error::Conflict);
                }
            }

            // Verify correlation matches (required, not optional)
            match (existing_corr, &reply_envelope.correlation_id) {
                (Some(ec), Some(c)) if ec != c => return Err(Error::Conflict),
                (Some(_), None) => return Err(Error::Conflict),
                (None, Some(c)) if c.as_str() != original_id => return Err(Error::Conflict),
                _ => {}
            }
            // Verify reply recipient matches original sender (stored in reply)
            let reply_recipient: String = tx.query_row(
                "SELECT recipient FROM messages WHERE id=?1",
                [existing_id.clone()],
                |r| r.get(0),
            )?;
            if reply_recipient != orig_sender {
                return Err(Error::Conflict);
            }
            // Verify stored reply's in_reply_to points to original (not a standalone reply)
            let reply_parent: Option<String> = tx.query_row(
                "SELECT in_reply_to FROM messages WHERE id=?1",
                [existing_id.clone()],
                |r| r.get(0),
            )?;
            if reply_parent.as_deref() != Some(original_id) {
                return Err(Error::Conflict);
            }
            // Check if the original message is an RPC request (stored reply should not ACK RPC)
            let orig_rpc: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                [original_id],
                |r| r.get(0),
            )?;
            if orig_rpc {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Check if stored reply's own ID is an RPC request
            let stored_rpc: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                [existing_id.clone()],
                |r| r.get(0),
            )?;
            if stored_rpc {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Check if stored reply's parent (original) is an RPC request
            let orig_rpc2: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=?1)",
                [original_id],
                |r| r.get(0),
            )?;
            if orig_rpc2 {
                return Err(Error::Invalid("RPC requests require respond, not reply"));
            }
            // Query reply message before commit
            let mut msg = tx.query_row(
                &format!("SELECT {} FROM messages WHERE id=?1", COLUMNS),
                [existing_id.clone()],
                read_message,
            )?;
            msg.conversation_id = conversation::parent_thread(&tx, original_id)?;
            commit_reply(
                tx,
                ReplyCommit {
                    original_id,
                    reply_id: &msg.id,
                    reply_new: false,
                    ack: if acknowledged.is_none() {
                        CompletionAck::Timestamp { now }
                    } else {
                        CompletionAck::Unchanged
                    },
                    binding: None,
                },
                || Ok(()),
                Error::from,
            )?;
            return Ok(msg);
        }

        // Insert reply
        let conversation_id = conversation::parent_thread(&tx, original_id)?;
        let reply_msg = insert_message(&tx, reply_envelope, now, Some(&conversation_id))?;

        commit_reply(
            tx,
            ReplyCommit {
                original_id,
                reply_id: &reply_msg.id,
                reply_new: true,
                ack: if acknowledged.is_none() {
                    CompletionAck::Timestamp { now }
                } else {
                    CompletionAck::Unchanged
                },
                binding: None,
            },
            || Ok(()),
            Error::from,
        )?;
        Ok(reply_msg)
    }

    /// Atomically stores a result and ends the lease. Same receipt/result is retryable.
    // RPC-005, RPC-011, RPC-012, RPC-020, RPC-021, RPC-022, RPC-023, RPC-024, RPC-025: atomic result+ack, receipt, idempotent, conflict, null rejection.
    pub fn respond(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        outcome: Outcome,
    ) -> Result<Outcome> {
        self.respond_with_clock(recipient, id, receipt, outcome, now_ms)
    }
    fn respond_with_clock(
        &mut self,
        recipient: &str,
        id: &str,
        receipt: &str,
        outcome: Outcome,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<Outcome> {
        nonempty(recipient)?;
        nonempty(id)?;
        nonempty(receipt)?;
        match &outcome {
            Outcome::Success { value } if value.is_null() => {
                return Err(Error::Invalid("success result must not be null"));
            }
            Outcome::Error { code, message } => {
                nonempty(code)?;
                nonempty(message)?;
            }
            _ => {}
        }
        let encoded = serde_json::to_string(&outcome)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = clock()?;
        type ResponseRow = (Option<String>, Option<i64>, Option<String>);
        let row: Option<ResponseRow> = tx.query_row("SELECT m.receipt,m.lease_until_ms,r.outcome_json FROM messages m JOIN rpc_requests r ON r.message_id=m.id WHERE m.id=?1 AND m.recipient=?2 AND m.dead_lettered_at_ms IS NULL", params![id,recipient], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (stored, until, previous) = row.ok_or(Error::NotFound)?;
        if stored.as_deref() != Some(receipt) {
            return Err(Error::StaleReceipt);
        }
        if let Some(previous) = previous {
            let previous: Outcome = serde_json::from_str(&previous)?;
            if previous != outcome {
                return Err(Error::Conflict);
            }
            tx.commit()?;
            return Ok(previous);
        }
        if until.is_none_or(|deadline| deadline <= now) {
            return Err(Error::StaleReceipt);
        }
        tx.execute(
            "UPDATE rpc_requests SET outcome_json=?1 WHERE message_id=?2",
            params![encoded, id],
        )?;
        tx.execute(
            "UPDATE messages SET acknowledged_at_ms=?1 WHERE id=?2",
            params![now, id],
        )?;
        tx.commit()?;
        Ok(outcome)
    }
    /// Requester label scoping is isolation for cooperating callers, not authentication.
    // RPC-006, RPC-007, RPC-026, RPC-027, RPC-028, RPC-029, RPC-030: consistent view, requester isolation, state rules.
    pub fn status(&self, requester: &str, id: &str) -> Result<RpcStatus> {
        nonempty(requester)?;
        nonempty(id)?;
        // One SELECT snapshot keeps the envelope, lease and result consistent.
        let row = self.connection.query_row(&format!("SELECT {COLUMNS},lease_until_ms,(SELECT outcome_json FROM rpc_requests WHERE message_id=messages.id) FROM messages WHERE id=?1 AND sender=?2 AND EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)"), params![id,requester], |r| Ok((read_message(r)?,r.get::<_,Option<i64>>(9)?,r.get::<_,Option<String>>(10)?))).optional()?.ok_or(Error::NotFound)?;
        let result = row
            .2
            .map(|value| serde_json::from_str::<Outcome>(&value))
            .transpose()?;
        let now = now_ms()?;
        let state = if result.is_some() {
            RpcState::Answered
        } else if row.1.is_some_and(|until| until > now) {
            RpcState::Claimed
        } else {
            RpcState::Pending
        };
        Ok(RpcStatus {
            request: row.0,
            state,
            result,
        })
    }
}

/// Called only after the reply and original ACK transaction has committed.
pub(crate) fn trace_reply_commit(
    original_id: &str,
    reply_id: &str,
    reply_new: bool,
    ack_new: bool,
) {
    tracing::info!(
        component = "queue",
        event = "reply_stored",
        outcome = if reply_new {
            "committed"
        } else {
            "already_stored"
        },
        message_id = original_id,
        reply_id
    );
    tracing::info!(
        component = "queue",
        event = "original_ack",
        outcome = if ack_new {
            "committed"
        } else {
            "already_acknowledged"
        },
        message_id = original_id,
        reply_id
    );
}

/// Called only after the terminal confirmation transaction has committed.
pub(crate) fn trace_terminal_commit(message_id: &str, duplicate: bool) {
    tracing::info!(
        component = "queue",
        event = "terminal_confirmed",
        outcome = if duplicate {
            "already_confirmed"
        } else {
            "committed"
        },
        message_id
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_fences_old_receipt_and_redelivers() {
        let mut q = Queue::open(":memory:").unwrap();
        q.send(Envelope {
            sender: "a".into(),
            recipient: "b".into(),
            body: "hi".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap();
        let first = q
            .receive_with_clock("b", Duration::from_millis(10), || Ok(100))
            .unwrap()
            .unwrap();
        assert!(
            q.receive_with_clock("b", Duration::from_millis(10), || Ok(109))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            q.ack_with_clock("b", &first.message.id, &first.receipt, || Ok(110)),
            Err(Error::StaleReceipt)
        ));
        let second = q
            .receive_with_clock("b", Duration::from_millis(10), || Ok(110))
            .unwrap()
            .unwrap();
        assert_eq!(first.message, second.message);
        assert_eq!(second.attempt, 2);
        assert_ne!(first.receipt, second.receipt);
        assert!(matches!(
            q.ack_with_clock("b", &first.message.id, &first.receipt, || Ok(111)),
            Err(Error::StaleReceipt)
        ));
        assert_eq!(
            q.ack_with_clock("b", &second.message.id, &second.receipt, || Ok(111))
                .unwrap(),
            AckResult::Acknowledged
        );
        assert_eq!(
            q.ack_with_clock("b", &second.message.id, &second.receipt, || Ok(999))
                .unwrap(),
            AckResult::AlreadyAcknowledged
        );
    }
    #[test]
    fn dlq_waits_for_final_expiry_preserves_claim_and_skips_to_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let mut q = Queue::open(&path).unwrap();
        let envelope = Envelope {
            sender: "a".into(),
            recipient: "b".into(),
            body: "keep".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        };
        let id = q.send(envelope.clone()).unwrap().message.id;
        let sweep = q
            .start_sweep("b", queue_progress::ObservationQuery::FirstDelivery, || {
                Ok(())
            })
            .unwrap();
        for _ in 0..3 {
            q.observe_page(
                "b",
                &queue_progress::ObservationQuery::FirstDelivery,
                &sweep,
                100,
                || Ok(()),
            )
            .unwrap();
        }
        assert_eq!(
            q.connection
                .query_row("SELECT attempts FROM messages WHERE id=?1", [&id], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let mut last = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let delivery = q
                .receive_with_clock("b", Duration::from_millis(10), || Ok(90 + attempt * 10))
                .unwrap()
                .unwrap();
            assert_eq!(delivery.attempt, attempt);
            last = Some(delivery);
        }
        let last = last.unwrap();
        assert!(
            q.receive_with_clock("b", Duration::from_millis(10), || Ok(149))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            q.connection
                .query_row(
                    "SELECT dead_lettered_at_ms FROM messages WHERE id=?1",
                    [&id],
                    |r| r.get::<_, Option<i64>>(0)
                )
                .unwrap(),
            None
        );
        let next = q.send(envelope).unwrap().message.id;
        let delivered = q
            .receive_with_clock("b", Duration::from_millis(10), || Ok(150))
            .unwrap()
            .unwrap();
        assert_eq!(delivered.message.id, next);
        assert_eq!(delivered.attempt, 1);
        let state: (Option<i64>, i64, Option<String>, Option<i64>, Option<i64>, String) = q.connection.query_row(
            "SELECT dead_lettered_at_ms,attempts,receipt,lease_until_ms,acknowledged_at_ms,body FROM messages WHERE id=?1", [&id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
        ).unwrap();
        assert_eq!(
            state,
            (
                Some(150),
                MAX_ATTEMPTS,
                Some(last.receipt.clone()),
                Some(150),
                None,
                "keep".into()
            )
        );
        assert!(
            q.ack_with_clock("b", &id, &last.receipt, || Ok(151))
                .is_err()
        );
        assert_eq!(
            q.ordinary_metadata(&id, "b").unwrap()["state"],
            "dead_lettered"
        );
        for query in [
            queue_progress::ObservationQuery::FirstDelivery,
            queue_progress::ObservationQuery::Unacknowledged,
            queue_progress::ObservationQuery::Recovery,
        ] {
            let sweep = q.start_sweep("b", query.clone(), || Ok(())).unwrap();
            assert!(
                q.observe_page("b", &query, &sweep, 100, || Ok(()))
                    .unwrap()
                    .messages
                    .iter()
                    .all(|(_, m)| m.id != id)
            );
        }
        drop(q);
        let q = Queue::open_existing(&path).unwrap();
        assert_eq!(
            q.ordinary_metadata(&id, "b").unwrap()["state"],
            "dead_lettered"
        );
        assert_eq!(q.observe_ordinary(&id, "b").unwrap().envelope.body, "keep");
    }

    #[test]
    fn dlq_does_not_quarantine_rpc_terminal_or_acknowledged_messages() {
        for (rpc, terminal) in [(true, false), (false, true), (false, false)] {
            let mut q = Queue::open(":memory:").unwrap();
            let e = Envelope {
                sender: "a".into(),
                recipient: "b".into(),
                body: "keep".into(),
                correlation_id: None,
                in_reply_to: terminal.then(|| "parent".into()),
                idempotency_key: None,
            };
            let id = if rpc { q.request(e) } else { q.send(e) }
                .unwrap()
                .message
                .id;
            let mut last = None;
            for attempt in 1..=MAX_ATTEMPTS {
                last = q
                    .receive_kind(
                        "b",
                        Duration::from_millis(10),
                        || Ok(90 + attempt * 10),
                        rpc,
                    )
                    .unwrap();
            }
            if !rpc && !terminal {
                q.ack_with_clock("b", &id, &last.unwrap().receipt, || Ok(149))
                    .unwrap();
                assert!(
                    q.receive_with_clock("b", Duration::from_millis(10), || Ok(150))
                        .unwrap()
                        .is_none()
                );
            } else {
                assert_eq!(
                    q.receive_kind("b", Duration::from_millis(10), || Ok(150), rpc)
                        .unwrap()
                        .unwrap()
                        .attempt,
                    MAX_ATTEMPTS + 1
                );
            }
            assert_eq!(
                q.connection
                    .query_row(
                        "SELECT dead_lettered_at_ms FROM messages WHERE id=?1",
                        [&id],
                        |r| r.get::<_, Option<i64>>(0)
                    )
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    fn invalid_lease_overflow_does_not_consume_message() {
        let mut q = Queue::open(":memory:").unwrap();
        q.send(Envelope {
            sender: "a".into(),
            recipient: "b".into(),
            body: "hi".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap();
        assert!(matches!(
            q.receive_with_clock("b", Duration::from_millis(10), || Ok(i64::MAX - 5)),
            Err(Error::Invalid("lease overflow"))
        ));
        assert!(matches!(
            q.receive("b", Duration::MAX),
            Err(Error::Invalid("lease overflow"))
        ));
        let delivery = q
            .receive_with_clock("b", Duration::from_millis(10), || Ok(100))
            .unwrap()
            .unwrap();
        assert_eq!(delivery.attempt, 1);
        assert_eq!(
            q.ack_with_clock("b", &delivery.message.id, &delivery.receipt, || Ok(109))
                .unwrap(),
            AckResult::Acknowledged
        );
    }

    #[test]
    fn busy_writer_error_keeps_queue_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let mut q = Queue::open(&path).unwrap();
        q.connection.busy_timeout(Duration::from_millis(1)).unwrap();
        let mut blocker = Connection::open(&path).unwrap();
        let lock = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let e = Envelope {
            sender: "a".into(),
            recipient: "b".into(),
            body: "hi".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: Some("one".into()),
        };
        assert!(
            matches!(q.send(e.clone()), Err(Error::Database(rusqlite::Error::SqliteFailure(error, _))) if error.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        lock.rollback().unwrap();
        assert!(!q.send(e).unwrap().duplicate);
        assert_eq!(
            q.receive("b", Duration::from_secs(30))
                .unwrap()
                .unwrap()
                .attempt,
            1
        );
    }
    #[test]
    fn rejects_negative_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", -1).unwrap();
        drop(connection);
        assert!(matches!(Queue::open(path), Err(Error::Schema(-1))));
    }
    #[test]
    fn rpc_expiry_redelivery_fences_results_and_preserves_pending() {
        let mut q = Queue::open(":memory:").unwrap();
        let id = q
            .request(Envelope {
                sender: "a".into(),
                recipient: "b".into(),
                body: "calculate".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        let d = q
            .receive_kind("b", Duration::from_millis(10), || Ok(100), true)
            .unwrap()
            .unwrap();
        let result = Outcome::Success {
            value: serde_json::json!({"answer":5}),
        };
        assert!(matches!(
            q.respond_with_clock("b", &id, &d.receipt, result.clone(), || Ok(110)),
            Err(Error::StaleReceipt)
        ));
        assert_eq!(q.status("a", &id).unwrap().state, RpcState::Pending);
        let next = q
            .receive_kind("b", Duration::from_millis(10), || Ok(110), true)
            .unwrap()
            .unwrap();
        assert_eq!(next.attempt, 2);
        assert!(matches!(
            q.respond_with_clock("b", &id, &d.receipt, result.clone(), || Ok(111)),
            Err(Error::StaleReceipt)
        ));
        q.respond_with_clock("b", &id, &next.receipt, result.clone(), || Ok(111))
            .unwrap();
        assert_eq!(
            q.respond_with_clock("b", &id, &next.receipt, result.clone(), || Ok(999))
                .unwrap(),
            result
        );
        assert_eq!(q.status("a", &id).unwrap().state, RpcState::Answered);
    }
}

#[cfg(test)]
mod queue_journal_tests {
    use super::*;

    #[test]
    fn disk_modes_are_checked_without_changing_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("journal.db")).unwrap();
        for mode in ["DELETE", "TRUNCATE", "PERSIST", "OFF", "MEMORY"] {
            db.pragma_update(None, "journal_mode", mode).unwrap();
            let result = validate_queue_journal(&db);
            if matches!(mode, "OFF" | "MEMORY") {
                assert!(matches!(
                    result,
                    Err(Error::Invalid("unsupported_queue_journal_mode"))
                ));
            } else {
                result.unwrap();
            }
            let actual: String = db
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            assert_eq!(actual, mode.to_ascii_lowercase());
        }
    }
}
