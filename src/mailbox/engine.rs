//! Transaction machinery shared by mailbox completion strategies.
//! Callers retain validation, connection setup, replay decisions and authorization timing.
use crate::{Envelope, Message};
use rusqlite::{Transaction, params};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum ReplyTable {
    Muse,
    Grok,
    Dot,
}
impl ReplyTable {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Muse => "muse_mailbox_replies",
            Self::Grok => "grok_mailbox_replies",
            Self::Dot => "dot_event_replies",
        }
    }
}

/// Strategies select the existing ACK shape. No strategy adopts a native claim here.
pub(crate) enum Ack {
    Unchanged,
    Timestamp { now: i64 },
    DeferredClaim { now: i64, until: i64 },
}
pub(crate) struct ReplyCommit<'a> {
    pub original_id: &'a str,
    pub reply_id: &'a str,
    pub reply_new: bool,
    pub ack: Ack,
    pub binding: Option<ReplyTable>,
}

/// Inserts only inside the caller's transaction. It does not authorize or commit.
pub(crate) fn insert_message(
    tx: &Transaction<'_>,
    envelope: Envelope,
    created_at_ms: i64,
    thread: Option<&str>,
) -> rusqlite::Result<Message> {
    let id = Uuid::new_v4().to_string();
    let message = Message {
        conversation_id: thread.unwrap_or(&id).into(),
        id,
        envelope,
        created_at_ms,
    };
    let e = &message.envelope;
    tx.execute(
        "INSERT INTO messages(id,sender,recipient,body,correlation_id,in_reply_to,idempotency_key,created_at_ms,conversation_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![message.id,e.sender,e.recipient,e.body,e.correlation_id,e.in_reply_to,e.idempotency_key,message.created_at_ms,message.conversation_id],
    )?;
    Ok(message)
}

/// Writes ACK and binding after the reply, then checks final authority and commits.
/// Dropping the transaction rolls back all writes on any error. Traces follow commit only.
pub(crate) fn commit_reply<E>(
    tx: Transaction<'_>,
    plan: ReplyCommit<'_>,
    authorize: impl FnOnce() -> Result<(), E>,
    storage: impl Fn(rusqlite::Error) -> E,
) -> Result<(), E> {
    let ack_new = !matches!(plan.ack, Ack::Unchanged);
    if !plan.reply_new {
        // A stored reply can acquire the canonical ID only after the completion
        // path validates the relationship and rechecks authority below.
        let thread = crate::conversation::parent_thread(&tx, plan.original_id).map_err(&storage)?;
        tx.execute(
            "UPDATE messages SET conversation_id=?1 WHERE id=?2 AND in_reply_to=?3 AND conversation_id<>?1",
            params![thread, plan.reply_id, plan.original_id],
        ).map_err(&storage)?;
    }
    match plan.ack {
        Ack::Unchanged => {}
        Ack::Timestamp { now } => {
            tx.execute(
                "UPDATE messages SET acknowledged_at_ms=?1 WHERE id=?2",
                params![now, plan.original_id],
            )
            .map_err(&storage)?;
        }
        Ack::DeferredClaim { now, until } => {
            tx.execute(
                "UPDATE messages SET receipt=?1,lease_until_ms=?2,attempts=1,acknowledged_at_ms=?3 WHERE id=?4",
                params![Uuid::new_v4().to_string(), until, now, plan.original_id],
            ).map_err(&storage)?;
        }
    }
    if let Some(table) = plan.binding {
        tx.execute(
            &format!(
                "INSERT INTO {}(message_id,reply_id) VALUES (?1,?2)",
                table.name()
            ),
            params![plan.original_id, plan.reply_id],
        )
        .map_err(&storage)?;
    }
    authorize()?;
    tx.commit().map_err(storage)?;
    crate::trace_reply_commit(plan.original_id, plan.reply_id, plan.reply_new, ack_new);
    Ok(())
}
