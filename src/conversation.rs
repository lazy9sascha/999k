//! Protected per-copy context, not a second queue or a membership authority.
//! Only an authenticated host may call these APIs. Raw bodies/correlation cannot
//! create a role. Host policy is rechecked for every participant and operation.
use crate::{Envelope, Error, Queue, Result, SendResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    To,
    Cc,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub mailbox: String,
    pub role: Role,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub conversation_id: String,
    pub participants: Vec<Participant>,
}
pub(super) fn identifier(s: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > 128 || s.chars().any(char::is_control) {
        return Err(Error::Invalid("invalid_conversation_identifier"));
    }
    Ok(())
}
impl Context {
    pub(crate) fn validate(&self, envelope: &Envelope) -> Result<()> {
        identifier(&self.conversation_id)?;
        // FIXME(yagni): 32-participant cap with no stated basis
        if self.participants.is_empty() || self.participants.len() > 32 {
            return Err(Error::Invalid("invalid_participants"));
        }
        for (i, p) in self.participants.iter().enumerate() {
            identifier(&p.mailbox)?;
            if self.participants[..i]
                .iter()
                .any(|a| a.mailbox == p.mailbox)
            {
                return Err(Error::Invalid("duplicate_participant"));
            }
        }
        if envelope.in_reply_to.is_some()
            || envelope.sender == envelope.recipient
            || envelope.idempotency_key.is_none()
            || !self
                .participants
                .iter()
                .any(|p| p.mailbox == envelope.recipient)
        {
            return Err(Error::Invalid("invalid_conversation_copy"));
        }
        identifier(envelope.idempotency_key.as_deref().unwrap())?;
        Ok(())
    }
}
pub(crate) fn load(c: &Connection, id: &str) -> Result<Option<Context>> {
    let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='message_participation')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let raw: Option<String> = c
        .query_row(
            "SELECT context_json FROM message_participation WHERE message_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|s| serde_json::from_str(&s).map_err(Error::from))
        .transpose()
}
pub(crate) fn store(c: &Connection, id: &str, context: &Context) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS message_participation(message_id TEXT PRIMARY KEY REFERENCES messages(id), context_json TEXT NOT NULL)")?;
    c.execute(
        "INSERT INTO message_participation(message_id,context_json) VALUES (?1,?2)",
        params![id, serde_json::to_string(context)?],
    )?;
    Ok(())
}
impl Queue {
    /// One explicitly selected copy. The host preflights the entire selection
    /// before calling this for any copy; partial acceptance stays per-copy.
    /// `authorize` must check current registration and route policy, not labels.
    pub fn send_conversation_checked(
        &mut self,
        envelope: Envelope,
        context: Context,
        mut authorize: impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<SendResult> {
        context.validate(&envelope)?;
        let sender = envelope.sender.clone();
        self.send_kind(envelope, false, Some(&context), None, || {
            for p in &context.participants {
                authorize(&sender, &p.mailbox)?;
            }
            Ok(())
        })
    }
}

/// The route assembles this from current read authority, never from MCP arguments.
pub struct ThreadScope<'a> {
    pub mailbox: &'a str,
    pub peers: &'a [String],
    pub request_bytes: i64,
}

pub(super) const VISIBLE: &str = "((m.sender=?2 AND m.recipient IN (SELECT value FROM json_each(?3)))
 OR (m.recipient=?2 AND m.sender IN (SELECT value FROM json_each(?3))))
 AND m.dead_lettered_at_ms IS NULL
 AND typeof(m.body)='text'
 AND length(CAST(m.body AS BLOB)) BETWEEN 1 AND CASE WHEN m.in_reply_to IS NULL THEN ?4 ELSE 16384 END
 AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id)
 AND (m.in_reply_to IS NULL OR EXISTS(
     SELECT 1 FROM messages p WHERE p.id=m.in_reply_to AND p.in_reply_to IS NULL
     AND p.sender=m.recipient AND p.recipient=m.sender
     AND p.conversation_id=m.conversation_id AND p.acknowledged_at_ms IS NOT NULL
     AND m.correlation_id=COALESCE(p.correlation_id,p.id)
     AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=p.id)))";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadRead {
    pub conversation_id: String,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadPage {
    pub messages: Vec<ThreadMessage>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadMessage {
    pub message_id: String,
    pub conversation_id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub in_reply_to: Option<String>,
}

pub(crate) fn parent_thread(c: &Connection, message_id: &str) -> rusqlite::Result<String> {
    c.query_row(
        "SELECT conversation_id FROM messages WHERE id=?1 AND in_reply_to IS NULL
         AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=messages.id)",
        [message_id],
        |r| r.get(0),
    )
}

pub(crate) struct Continuation<'a> {
    id: &'a str,
    scope: ThreadScope<'a>,
}

impl Continuation<'_> {
    pub(crate) fn resolve(&self, c: &Connection, envelope: &Envelope) -> Result<String> {
        if envelope.sender != self.scope.mailbox || envelope.in_reply_to.is_some() {
            return Err(Error::Invalid("invalid_continuation_sender"));
        }
        identifier(self.id)?;
        let peers = serde_json::to_string(self.scope.peers)?;
        c.query_row(
            &format!("SELECT m.conversation_id FROM messages m WHERE m.id=?1 AND {VISIBLE}"),
            params![self.id, self.scope.mailbox, peers, self.scope.request_bytes],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(Error::NotFound)
    }
}

impl Queue {
    /// Continue from a visible message; the trusted route rechecks send/read policy around commit.
    pub fn continue_checked(
        &mut self,
        envelope: Envelope,
        from: &str,
        scope: ThreadScope<'_>,
        authorize: impl FnMut() -> Result<()>,
    ) -> Result<SendResult> {
        self.send_kind(
            envelope,
            false,
            None,
            Some(Continuation { id: from, scope }),
            authorize,
        )
    }
}

/// Adapter helper for request decoding and a final current-scope check.
pub fn read_thread_tool(
    db: &std::path::Path,
    mailbox: &str,
    request_bytes: i64,
    args: &serde_json::Value,
    mut authorize: impl FnMut() -> std::result::Result<Vec<String>, (u16, &'static str)>,
) -> std::result::Result<serde_json::Value, (u16, &'static str)> {
    let read = serde_json::from_value(args.clone()).map_err(|_| (400, "invalid_fields"))?;
    let peers = authorize()?;
    let result = Queue::read_thread(
        db,
        ThreadScope {
            mailbox,
            peers: &peers,
            request_bytes,
        },
        &read,
    )
    .map_err(|error| match error {
        Error::Invalid("invalid_thread_cursor") => (400, "invalid_thread_cursor"),
        Error::Invalid("invalid_conversation_identifier") => (400, "invalid_fields"),
        _ => (503, "storage_unavailable"),
    });
    if authorize()? != peers {
        return Err((403, "thread_scope_changed"));
    }
    serde_json::to_value(result?).map_err(|_| (500, "internal_error"))
}
