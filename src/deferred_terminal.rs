//! Shared deferred terminal confirmation. Callback delivery/read alone never calls this.
use crate::{Error, Queue, Result, now_ms};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

type TerminalState = (Option<String>, i64, Option<i64>, Option<i64>);

/// Called only inside the authenticated host's completion transaction.
pub(crate) fn confirm(tx: &Transaction<'_>, recipient: &str, id: &str, now: i64) -> Result<bool> {
    let state: Option<TerminalState> = tx.query_row(
        "SELECT in_reply_to,attempts,lease_until_ms,acknowledged_at_ms FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL",
        params![id,recipient], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let (parent, attempts, lease, ack) = state.ok_or(Error::NotFound)?;
    if parent.is_none() {
        return Err(Error::Invalid("terminal_answer_required"));
    }
    crate::runtime_admission::validate_terminal(tx, id, recipient)?;
    if attempts != 0 || lease.unwrap_or(0) > now {
        return Err(Error::Invalid("existing_claim_requires_reconciliation"));
    }
    let duplicate = ack.is_some();
    if !duplicate {
        tx.execute("UPDATE messages SET acknowledged_at_ms=?1 WHERE id=?2 AND recipient=?3 AND acknowledged_at_ms IS NULL AND attempts=0",params![now,id,recipient])?;
    }
    Ok(duplicate)
}
impl Queue {
    /// The caller confirms exact answer receipt and reauthorizes before and after mutation.
    /// No reply, claim, schema preparation, or native-admission evidence is created.
    pub fn confirm_terminal_checked(
        &mut self,
        recipient: &str,
        id: &str,
        mut authorize: impl FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        let observed = self.observe_ordinary(id, recipient)?;
        authorize(&observed.envelope.sender)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sender: String = tx
            .query_row(
                "SELECT sender FROM messages WHERE id=?1 AND recipient=?2 AND dead_lettered_at_ms IS NULL",
                params![id, recipient],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        authorize(&sender)?;
        let duplicate = confirm(&tx, recipient, id, now_ms()?)?;
        authorize(&sender)?;
        tx.commit()?;
        crate::trace_terminal_commit(id, duplicate);
        Ok(duplicate)
    }
}
