//! Shared deferred mailbox engine for Muse and Grok. Adapters retain their own scope and reply table.
use crate::mailbox_engine::{Ack, ReplyCommit, ReplyTable, commit_reply, insert_message};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
// Internal UTF-8 body defaults, not vendor acceptance limits.
pub const MAX_REQUEST_BYTES: usize = 16384;
pub const MAX_REPLY_BYTES: usize = 16384;
const PLAIN: &str = "NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id)
AND (m.in_reply_to IS NULL OR EXISTS(SELECT 1 FROM messages p WHERE p.id=m.in_reply_to
AND p.sender='{mailbox}' AND p.recipient=m.sender AND p.in_reply_to IS NULL
AND m.correlation_id=COALESCE(p.correlation_id,p.id)
AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=p.id)))";
const COLUMNS: &str = "m.id,m.sender,CASE WHEN length(CAST(m.body AS BLOB))<=16384 THEN m.body ELSE '' END,m.correlation_id,m.in_reply_to,m.acknowledged_at_ms,m.attempts,m.receipt,m.lease_until_ms,m.conversation_id";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{reason}")]
pub struct MailboxError {
    pub reason: &'static str,
}
impl MailboxError {
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}
impl From<rusqlite::Error> for MailboxError {
    fn from(_: rusqlite::Error) -> Self {
        Self::new("storage_unavailable")
    }
}
pub type Result<T> = std::result::Result<T, MailboxError>;
#[derive(Debug, Serialize)]
pub struct ObservedMessage {
    pub id: String,
    pub conversation_id: String,
    pub sender: String,
    pub recipient: &'static str,
    pub body: String,
    pub correlation_id: String,
    pub in_reply_to: Option<String>,
    pub state: &'static str,
    pub reply_id: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Completion {
    pub state: &'static str,
    pub reply_id: String,
    pub duplicate: bool,
}
pub(crate) fn identifier(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 128
        || !s
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
    {
        return Err(MailboxError::new("invalid_identifier"));
    }
    Ok(())
}
pub(crate) fn body(s: &str, limit: usize) -> Result<()> {
    if s.len() > limit
        || s.chars()
            .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
    {
        return Err(MailboxError::new("invalid_body"));
    }
    Ok(())
}

fn now_ms() -> Result<i64> {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MailboxError::new("storage_unavailable"))?
        .as_millis();
    i64::try_from(t).map_err(|_| MailboxError::new("storage_unavailable"))
}
fn schema(c: &Connection) -> Result<()> {
    crate::validate_schema(c).map_err(|err| match err {
        crate::Error::Database(error) => error.into(),
        _ => MailboxError::new("requires_current_schema_queue"),
    })
}

fn connect(p: impl AsRef<Path>, ro: bool) -> Result<Connection> {
    let flags = if ro {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let c = Connection::open_with_flags(p, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    // Host checks cancellation between pages and before commit.
    // This timeout limits lock waits only; SQL execution is not interrupted.
    c.busy_timeout(Duration::from_secs(2))?;
    if ro {
        c.pragma_update(None, "query_only", true)?;
    } else {
        c.pragma_update(None, "synchronous", "FULL")?;
    }
    schema(&c)?;
    Ok(c)
}

struct Original {
    id: String,
    sender: String,
    body: String,
    correlation: Option<String>,
    parent: Option<String>,
    acknowledged: Option<i64>,
    attempts: i64,
    receipt: Option<String>,
    lease: Option<i64>,
    conversation_id: String,
}
impl Original {
    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            sender: r.get(1)?,
            body: r.get(2)?,
            correlation: r.get(3)?,
            parent: r.get(4)?,
            acknowledged: r.get(5)?,
            attempts: r.get(6)?,
            receipt: r.get(7)?,
            lease: r.get(8)?,
            conversation_id: r.get(9)?,
        })
    }
    fn validate(&self) -> Result<()> {
        identifier(&self.id)?;
        identifier(&self.sender)?;
        body(
            &self.body,
            if self.parent.is_some() {
                MAX_REPLY_BYTES
            } else {
                MAX_REQUEST_BYTES
            },
        )?;
        for s in [&self.correlation, &self.parent].into_iter().flatten() {
            identifier(s)?;
        }
        Ok(())
    }
    fn claimed(&self, now: i64) -> bool {
        self.acknowledged.is_some()
            || self.attempts != 0
            || self.receipt.is_some()
            || self.lease.unwrap_or(0) > now
    }
    fn correlation(&self) -> &str {
        self.correlation.as_deref().unwrap_or(&self.id)
    }
}

/// A bounded internal scan page. Never serialize this as client cursor progress.
#[derive(Debug)]
pub struct ScanPage {
    pub message: Option<ObservedMessage>,
    pub next_seq: i64,
    pub scanned: usize,
}
pub const SCAN_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy)]
pub(crate) enum Mailbox {
    Muse,
    Grok,
}
impl Mailbox {
    fn name(&self) -> &'static str {
        match self {
            Self::Muse => "muse",
            Self::Grok => "grok-bot",
        }
    }
    fn reply_table(&self) -> ReplyTable {
        match self {
            Self::Muse => ReplyTable::Muse,
            Self::Grok => ReplyTable::Grok,
        }
    }
    fn table(&self) -> &'static str {
        self.reply_table().name()
    }
    fn plain(&self) -> String {
        PLAIN.replace("{mailbox}", self.name())
    }
    fn scope_valid(&self, s: &[String]) -> Result<()> {
        if s.iter().any(|value| {
            identifier(value).is_err() || matches!(self, Self::Muse) && value == self.name()
        }) {
            return Err(MailboxError::new("forbidden"));
        }
        Ok(())
    }
    fn prepared(&self, c: &Connection) -> Result<bool> {
        let exists: bool = c.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='{table}')",
                table = self.table()
            ),
            [],
            |r| r.get(0),
        )?;
        if exists {
            c.prepare(&format!(
                "SELECT message_id,reply_id FROM {table} LIMIT 0",
                table = self.table()
            ))?;
        }
        Ok(exists)
    }
    fn require_prepared(&self, c: &Connection) -> Result<()> {
        if !self.prepared(c)? {
            return Err(MailboxError::new("mailbox_not_prepared"));
        }
        Ok(())
    }
    /// Explicit table-only installation. Never creates a queue or migrates its schema.
    pub fn prepare(&self, p: impl AsRef<Path>) -> Result<()> {
        let mut c = connect(p, false)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        tx.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {table}(message_id TEXT PRIMARY KEY,reply_id TEXT NOT NULL UNIQUE)" , table = self.table()))?;
        self.require_prepared(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn is_prepared(&self, p: impl AsRef<Path>) -> Result<bool> {
        self.prepared(&connect(p, true)?)
    }
    fn original(&self, c: &Connection, id: &str, s: &[String]) -> Result<Original> {
        let r = c
        .query_row(
            &format ! ("SELECT {COLUMNS} FROM messages m WHERE m.id=? AND m.recipient='{mailbox}' AND m.dead_lettered_at_ms IS NULL AND {plain}" , mailbox = self.name() , plain = self.plain()),
            [id],
            Original::row,
        )
        .optional()?
        .filter(|r| s.contains(&r.sender))
        .ok_or_else(|| MailboxError::new("message_not_available"))?;
        r.validate()?;
        Ok(r)
    }
    fn binding(&self, c: &Connection, id: &str) -> Result<Option<String>> {
        Ok(c.query_row(
            &format!(
                "SELECT reply_id FROM {table} WHERE message_id=?",
                table = self.table()
            ),
            [id],
            |r| r.get(0),
        )
        .optional()?)
    }
    fn observed(&self, c: &Connection, r: Original) -> Result<ObservedMessage> {
        let reply_id = self.binding(c, &r.id)?;
        if let Some(id) = &reply_id {
            identifier(id)?;
        }
        let state = if r.parent.is_some() {
            "reply_received"
        } else if reply_id.is_some() {
            "replied"
        } else if r.claimed(now_ms()?) {
            "reconciliation_required"
        } else {
            "pending"
        };
        let correlation_id = r.correlation().to_owned();
        Ok(ObservedMessage {
            id: r.id,
            conversation_id: r.conversation_id,
            sender: r.sender,
            recipient: self.name(),
            body: r.body,
            correlation_id,
            in_reply_to: r.parent,
            state,
            reply_id,
        })
    }
    /// Snapshot observation only; does not claim or ACK, even for terminal answers.
    pub fn read(&self, p: impl AsRef<Path>, id: &str, s: &[String]) -> Result<ObservedMessage> {
        identifier(id)?;
        self.scope_valid(s)?;
        let mut c = connect(p, true)?;
        let tx = c.transaction()?;
        self.require_prepared(&tx)?;
        self.observed(&tx, self.original(&tx, id, s)?)
    }
    /// Bounded convenience: one raw request page, then one terminal page only if
    /// that request page exhausted the raw queue. None may mean more pages remain.
    /// Hosts needing complete observation must use cancellable scan_page sweeps.
    pub fn scan(&self, p: impl AsRef<Path>, s: &[String]) -> Result<Option<ObservedMessage>> {
        let page = self.scan_page(p.as_ref(), s, 0, false)?;
        if page.message.is_some() || page.scanned == SCAN_PAGE_SIZE {
            return Ok(page.message);
        }
        Ok(self.scan_page(p.as_ref(), s, 0, true)?.message)
    }
    /// Each page reads at most 100 RAW sequence candidates before mailbox/protocol
    /// filtering. terminal=false observes requests; terminal=true observes answers.
    /// Trusted host exhausts request pages first, then starts terminal pages at zero.
    /// Reset both phases each sweep; reauthorize/check cancellation between pages.
    /// after_seq is ephemeral host state, never client input or durable progress.
    pub fn scan_page(
        &self,
        p: impl AsRef<Path>,
        s: &[String],
        after_seq: i64,
        terminal: bool,
    ) -> Result<ScanPage> {
        self.scope_valid(s)?;
        if after_seq < 0 {
            return Err(MailboxError::new("invalid_scan_position"));
        }
        let mut c = connect(p, true)?;
        let tx = c.transaction()?;
        self.require_prepared(&tx)?;
        // Do not put sender, mailbox, claim, or RPC predicates before this LIMIT.
        let mut stmt = tx.prepare("SELECT seq,id,recipient,in_reply_to IS NOT NULL FROM messages WHERE seq>? ORDER BY seq LIMIT 100")?;
        let mut rows = stmt.query([after_seq])?;
        let mut page = ScanPage {
            message: None,
            next_seq: after_seq,
            scanned: 0,
        };
        let now = now_ms()?;
        while let Some(row) = rows.next()? {
            page.next_seq = row.get(0)?;
            page.scanned += 1;
            let id: String = row.get(1)?;
            let recipient: String = row.get(2)?;
            let is_terminal: bool = row.get(3)?;
            if recipient != self.name() || is_terminal != terminal || identifier(&id).is_err() {
                continue;
            }
            let r = match self.original(&tx, &id, s) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.reason,
                        "message_not_available" | "invalid_body" | "invalid_identifier"
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            if r.claimed(now) || self.binding(&tx, &id)?.is_some() {
                continue;
            }
            page.message = Some(self.observed(&tx, r)?);
            break;
        }
        Ok(page)
    }
    /// Revalidates trusted authorization after acquiring the write lock, even on retry.
    pub fn reply(
        &self,
        p: impl AsRef<Path>,
        id: &str,
        text: &str,
        mut authorize: impl FnMut() -> Result<Vec<String>>,
    ) -> Result<Completion> {
        identifier(id)?;
        body(text, MAX_REPLY_BYTES)?;
        self.scope_valid(&authorize()?)?;
        let mut c = connect(p, false)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema(&tx)?;
        self.require_prepared(&tx)?;
        let s = authorize()?;
        self.scope_valid(&s)?;
        let r = self.original(&tx, id, &s)?;
        if r.parent.is_some() {
            return Err(MailboxError::new("reply_is_terminal"));
        }
        let key = format!("gw-reply-{id}");
        let bound = self.binding(&tx, id)?;
        type Stored = (String, String, Option<String>, Option<String>);
        let old: Option<Stored> = tx.query_row(&format!("SELECT id,CASE WHEN length(CAST(body AS BLOB))<=16384 THEN body ELSE '' END,in_reply_to,correlation_id FROM messages WHERE sender='{mailbox}' AND recipient=? AND idempotency_key=?" , mailbox = self.name()),
        params![r.sender,key],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let unmatched: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM messages WHERE sender='{mailbox}' AND recipient=? AND in_reply_to=? AND (idempotency_key IS NULL OR idempotency_key<>?))" , mailbox = self.name()),
        params![r.sender,id,key],|r| r.get(0))?;
        if unmatched {
            return Err(MailboxError::new("reconciliation_required"));
        }
        if let Some((reply_id, old_body, parent, correlation)) = old {
            identifier(&reply_id)?;
            if bound.as_deref() != Some(reply_id.as_str()) || r.acknowledged.is_none() {
                return Err(MailboxError::new("reconciliation_required"));
            }
            if old_body != text
                || parent.as_deref() != Some(id)
                || correlation.as_deref() != Some(r.correlation())
            {
                return Err(MailboxError::new("reply_conflict"));
            }
            commit_reply(
                tx,
                ReplyCommit {
                    original_id: id,
                    reply_id: &reply_id,
                    reply_new: false,
                    ack: Ack::Unchanged,
                    binding: None,
                },
                || {
                    let latest = authorize()?;
                    self.scope_valid(&latest)?;
                    if !latest.contains(&r.sender) {
                        return Err(MailboxError::new("message_not_available"));
                    }
                    Ok(())
                },
                MailboxError::from,
            )?;
            return Ok(Completion {
                state: "replied",
                reply_id,
                duplicate: true,
            });
        }
        let now = now_ms()?;
        if bound.is_some() || r.claimed(now) {
            return Err(MailboxError::new("existing_claim_requires_reconciliation"));
        }
        let until = now
            .checked_add(1000)
            .ok_or_else(|| MailboxError::new("storage_unavailable"))?;
        let thread = crate::conversation::parent_thread(&tx, id)?;
        let reply_id = insert_message(
            &tx,
            crate::Envelope {
                sender: self.name().into(),
                recipient: r.sender.clone(),
                body: text.into(),
                correlation_id: Some(r.correlation().into()),
                in_reply_to: Some(id.into()),
                idempotency_key: Some(key),
            },
            now,
            Some(&thread),
        )?
        .id;
        commit_reply(
            tx,
            ReplyCommit {
                original_id: id,
                reply_id: &reply_id,
                reply_new: true,
                ack: Ack::DeferredClaim { now, until },
                binding: Some(self.reply_table()),
            },
            || {
                let latest = authorize()?;
                self.scope_valid(&latest)?;
                if !latest.contains(&r.sender) {
                    return Err(MailboxError::new("message_not_available"));
                }
                Ok(())
            },
            MailboxError::from,
        )?;
        Ok(Completion {
            state: "replied",
            reply_id,
            duplicate: false,
        })
    }
}
