//! OBS-01..07 and PROG-03/05: bounded traversal, not a state snapshot or execution grant.
use crate::{COLUMNS, Delivery, Error, Message, Queue, Result, nonempty, now_ms, read_message};
use rusqlite::{OptionalExtension, params};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationQuery {
    FirstDelivery,
    Unacknowledged,
    Recovery,
    ReplyTo(String),
}
/// Host-owned in-memory position. Never use a cursor as authorization or handoff evidence.
#[derive(Clone, Debug)]
pub struct Sweep {
    recipient: String,
    query: ObservationQuery,
    after: i64,
    upper: i64,
}
impl Sweep {
    pub fn after(&self) -> i64 {
        self.after
    }
    /// A single-result host can retain progress only through the row it returned.
    pub(crate) fn after_observed(&self, seq: i64) -> Result<Self> {
        if seq <= 0 || seq > self.after {
            return Err(Error::Invalid("unexamined sweep position"));
        }
        let mut next = self.clone();
        next.after = seq;
        Ok(next)
    }
    pub fn upper(&self) -> i64 {
        self.upper
    }
}
#[derive(Debug)]
pub struct ObservationPage {
    pub messages: Vec<(i64, Message)>,
    /// Only the sequence is disclosed for an invalid envelope.
    pub invalid_sequences: Vec<i64>,
    pub continuation: Sweep,
    pub exhausted: bool,
}
impl Queue {
    /// OBS-03/04. The authenticated host must authorize each operation, not only sweep creation.
    pub fn start_sweep(
        &self,
        recipient: &str,
        query: ObservationQuery,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<Sweep> {
        authorize()?;
        nonempty(recipient)?;
        self.validate_observation_query(recipient, &query)?;
        let upper = self.connection.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE recipient=?1",
            [recipient],
            |r| r.get(0),
        )?;
        Ok(Sweep {
            recipient: recipient.into(),
            query,
            after: 0,
            upper,
        })
    }
    fn validate_observation_query(
        &self,
        recipient: &str,
        query: &ObservationQuery,
    ) -> Result<Option<Message>> {
        if let ObservationQuery::ReplyTo(id) = query {
            // OBS-05: caller owns the original sending mailbox; no RPC or reply chains.
            let original = self.connection.query_row(&format!("SELECT {COLUMNS} FROM messages WHERE id=?1 AND sender=?2 AND in_reply_to IS NULL AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)"), params![id, recipient], read_message).optional()?.ok_or(Error::NotFound)?;
            return Ok(Some(original));
        }
        Ok(None)
    }
    /// OBS-01/02/04/06/07. Budget counts examined rows, including ineligible and invalid rows.
    pub fn observe_page(
        &self,
        recipient: &str,
        query: &ObservationQuery,
        sweep: &Sweep,
        budget: usize,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<ObservationPage> {
        authorize()?;
        if recipient != sweep.recipient || query != &sweep.query || !(1..=100).contains(&budget) {
            return Err(Error::Invalid("incompatible sweep or page budget"));
        }
        let original = self.validate_observation_query(recipient, query)?;
        let now = now_ms()?;
        let mut statement = self.connection.prepare("SELECT seq,id,acknowledged_at_ms IS NULL AND attempts=0 AND receipt IS NULL AND (lease_until_ms IS NULL OR lease_until_ms<=?4), attempts>0, EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id), acknowledged_at_ms IS NULL, dead_lettered_at_ms IS NOT NULL FROM messages WHERE recipient=?1 AND seq>?2 AND seq<=?3 ORDER BY seq LIMIT ?5")?;
        let rows = statement
            .query_map(
                params![recipient, sweep.after, sweep.upper, now, budget as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1).ok(),
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, bool>(5)?,
                        row.get::<_, bool>(6)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut continuation = sweep.clone();
        let mut messages = Vec::new();
        let mut invalid_sequences = Vec::new();
        for (seq, id, first, recovery, rpc, pending, dead_lettered) in rows {
            continuation.after = seq;
            if rpc || dead_lettered {
                continue;
            }
            let Some(_id) = id.filter(|id| nonempty(id).is_ok()) else {
                invalid_sequences.push(seq);
                continue;
            };
            let message = match self
                .connection
                .query_row(
                    &format!("SELECT {COLUMNS} FROM messages WHERE seq=?1 AND recipient=?2"),
                    params![seq, recipient],
                    read_message,
                )
                .optional()
                .map_err(Error::from)
            {
                Ok(Some(message)) => message,
                Ok(None)
                | Err(Error::Database(rusqlite::Error::InvalidColumnType(..)))
                | Err(Error::Database(rusqlite::Error::FromSqlConversionFailure(..))) => {
                    invalid_sequences.push(seq);
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Common queue envelopes permit empty bodies and have no profile byte limit.
            // Only stored type errors and common nonblank-label violations are invalid.
            if nonempty(&message.envelope.sender).is_err()
                || nonempty(&message.envelope.recipient).is_err()
                || [
                    &message.envelope.correlation_id,
                    &message.envelope.in_reply_to,
                    &message.envelope.idempotency_key,
                ]
                .into_iter()
                .flatten()
                .any(|value| nonempty(value).is_err())
            {
                invalid_sequences.push(seq);
                continue;
            }
            let matches = match query {
                ObservationQuery::FirstDelivery => first,
                ObservationQuery::Unacknowledged => pending,
                ObservationQuery::Recovery => recovery,
                ObservationQuery::ReplyTo(_) => {
                    let original = original.as_ref().expect("validated reply original");
                    message.envelope.in_reply_to.as_deref() == Some(original.id.as_str())
                        && message.envelope.sender == original.envelope.recipient
                        && message.envelope.correlation_id.as_deref()
                            == Some(
                                original
                                    .envelope
                                    .correlation_id
                                    .as_deref()
                                    .unwrap_or(&original.id),
                            )
                }
            };
            if matches {
                messages.push((seq, message));
            }
        }
        let more: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE recipient=?1 AND seq>?2 AND seq<=?3)",
            params![recipient, continuation.after, continuation.upper],
            |r| r.get(0),
        )?;
        Ok(ObservationPage {
            messages,
            invalid_sequences,
            continuation,
            exhausted: !more,
        })
    }
    /// PROG-05: revalidate the selected ordinary item under the existing claim fence.
    /// This does not grant native resubmission authority. Reconcile durable admission evidence first.
    pub fn claim_observed(
        &mut self,
        recipient: &str,
        sweep: &Sweep,
        seq: i64,
        id: &str,
        lease: Duration,
        mut authorize: impl FnMut() -> Result<()>,
    ) -> Result<Option<Delivery>> {
        authorize()?;
        if recipient != sweep.recipient || seq <= 0 || seq > sweep.upper || seq > sweep.after {
            return Err(Error::Invalid("candidate outside examined sweep"));
        }
        if matches!(sweep.query, ObservationQuery::ReplyTo(_)) {
            return Err(Error::Invalid("reply observation is not claim authority"));
        }
        let first = match sweep.query {
            ObservationQuery::FirstDelivery => Some(true),
            ObservationQuery::Recovery => Some(false),
            _ => None,
        };
        self.receive_selected(
            recipient,
            lease,
            now_ms,
            false,
            Some(crate::ReceiveSelection::Observed(seq, id, first)),
            authorize,
        )
    }
}
