//! Read-only metadata traversal. Continuation is not handoff, admission, claim or ACK.
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

type Fault = (u16, &'static str);
const MAX_CANDIDATES: usize = 100;
// Ordinals within the authorized ordinary-message scope, never global queue sequence.
// This prefilter limits traversal metadata; exact read remains the authority check.
const SCOPE: &str = "m.recipient=?1 AND m.sender IN (SELECT value FROM json_each(?2))
AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id)
AND (m.in_reply_to IS NULL OR EXISTS(SELECT 1 FROM messages p WHERE p.id=m.in_reply_to
AND p.sender=?1 AND p.recipient=m.sender AND p.in_reply_to IS NULL
AND m.correlation_id=COALESCE(p.correlation_id,p.id)
AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=p.id)))";
struct Position {
    after: i64,
    upper: i64,
}
fn decode(token: &str, high: i64) -> Result<Position, Fault> {
    let invalid = (400, "invalid_continuation");
    // FIXME(yagni): 39-char continuation pre-check - i64 parse below already rejects overflow
    if token.len() > 39 || !token.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return Err(invalid);
    }
    let (after, upper) = token.split_once(':').ok_or(invalid)?;
    let p = Position {
        after: after.parse().map_err(|_| invalid)?,
        upper: upper.parse().map_err(|_| invalid)?,
    };
    if p.after < 0 || p.after >= p.upper || p.upper > high {
        return Err(invalid);
    }
    Ok(p)
}
fn senders(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}
fn storage(error: rusqlite::Error) -> Fault {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        (499, "request_cancelled")
    } else {
        (503, "storage_unavailable")
    }
}
fn check_deadline(deadline: Instant) -> Result<(), Fault> {
    if Instant::now() >= deadline {
        Err((499, "request_cancelled"))
    } else {
        Ok(())
    }
}
fn sql_deadline(c: &mut Connection, deadline: Instant) -> Result<(), Fault> {
    check_deadline(deadline)?;
    // No lock retry can outlive the budget. SQL execution checks the same absolute deadline.
    c.busy_timeout(Duration::ZERO).map_err(storage)?;
    c.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(storage)
}
/// Return the same metadata fields as the registered mailbox inboxes.
pub fn metadata_fields(value: &Value) -> Value {
    json!({
        "id": value["id"], "sender": value["sender"], "recipient": value["recipient"],
        "correlation_id": value["correlation_id"], "in_reply_to": value["in_reply_to"],
        "state": value["state"], "reply_id": value["reply_id"],
    })
}
fn muse_metadata(
    db: &Path,
    mailbox: &str,
    id: &str,
    allowed: &[String],
) -> Result<Option<Value>, Fault> {
    if allowed.is_empty() {
        return Ok(None);
    }
    // Reuse exact-read eligibility, terminal relationship checks and state projection.
    // The bounded body is discarded here and never enters the page response.
    let observed = match mailbox {
        "muse" => crate::muse_mailbox::read(
            db,
            id,
            &crate::muse_mailbox::MuseScope {
                allowed_senders: allowed.to_vec(),
            },
        )
        .map_err(|e| e.reason)
        .and_then(|m| serde_json::to_value(m).map_err(|_| "serialization_failed")),

        _ => return Err((403, "forbidden")),
    };
    match observed {
        Ok(value) => Ok(Some(metadata_fields(&value))),
        Err("message_not_available" | "invalid_body" | "invalid_identifier") => Ok(None),
        Err("forbidden") => Err((403, "forbidden")),
        Err(reason) => Err((503, reason)),
    }
}

pub fn list_muse(
    db: &Path,
    mailbox: &str,
    arguments: &Value,
    deadline: Instant,
    authorize: impl FnMut() -> Result<Vec<String>, Fault>,
) -> Result<Value, Fault> {
    list_with(
        db,
        mailbox,
        arguments,
        deadline,
        authorize,
        || {
            let prepared = match mailbox {
                "muse" => crate::muse_mailbox::is_prepared(db).map_err(|e| (503, e.reason))?,

                _ => return Err((403, "forbidden")),
            };
            if prepared {
                Ok(())
            } else {
                Err((503, "mailbox_not_prepared"))
            }
        },
        |id, allowed| muse_metadata(db, mailbox, id, allowed),
    )
}

pub fn list_with(
    db: &Path,
    mailbox: &str,
    arguments: &Value,
    deadline: Instant,
    mut authorize: impl FnMut() -> Result<Vec<String>, Fault>,
    ready: impl FnOnce() -> Result<(), Fault>,
    mut read: impl FnMut(&str, &[String]) -> Result<Option<Value>, Fault>,
) -> Result<Value, Fault> {
    let args = arguments.as_object().ok_or((400, "invalid_fields"))?;
    if args
        .keys()
        .any(|key| key != "limit" && key != "continuation")
    {
        return Err((400, "invalid_fields"));
    }
    let limit = match args.get("limit") {
        None => 20,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=50).contains(n))
            .ok_or((400, "invalid_limit"))? as usize,
    };
    let token = match args.get("continuation") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| !s.is_empty() && s.len() <= 39)
                .ok_or((400, "invalid_continuation"))?,
        ),
    };
    let allowed = senders(authorize()?);
    check_deadline(deadline)?;
    ready()?;
    let mut c = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(storage)?;
    sql_deadline(&mut c, deadline)?;
    c.pragma_update(None, "query_only", true).map_err(storage)?;
    let tx = c.transaction().map_err(storage)?;
    let permitted = serde_json::to_string(&allowed).map_err(|_| (503, "serialization_failed"))?;
    let high: i64 = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM messages m WHERE {SCOPE}"),
            rusqlite::params![mailbox, permitted],
            |r| r.get(0),
        )
        .map_err(storage)?;
    check_deadline(deadline)?;
    let mut position = match token {
        Some(t) => decode(t, high)?,
        None => Position {
            after: 0,
            upper: high,
        },
    };
    let mut stmt = tx
        .prepare(&format!(
            "SELECT CASE WHEN length(CAST(m.id AS BLOB))<=128 THEN m.id ELSE NULL END FROM messages m WHERE {SCOPE} ORDER BY m.seq LIMIT ?3 OFFSET ?4"
        ))
        .map_err(storage)?;
    let candidates = stmt
        .query_map(
            rusqlite::params![
                mailbox,
                permitted,
                (position.upper - position.after).min(MAX_CANDIDATES as i64),
                position.after
            ],
            |r| r.get::<_, Option<String>>(0),
        )
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    drop(stmt);
    tx.commit().map_err(storage)?;
    check_deadline(deadline)?;
    let mut messages = Vec::new();
    let mut consumed = 0;
    for id in &candidates {
        check_deadline(deadline)?;
        if senders(authorize()?) != allowed {
            return Err((403, "scope_changed"));
        }
        position.after += 1;
        consumed += 1;
        if let Some(id) = id
            && let Some(message) = read(id, &allowed)?
        {
            messages.push(message);
        }
        if messages.len() == limit {
            break;
        }
    }
    if senders(authorize()?) != allowed {
        return Err((403, "scope_changed"));
    }
    check_deadline(deadline)?;
    let exhausted = position.after >= position.upper
        || (consumed == candidates.len() && candidates.len() < MAX_CANDIDATES);
    let next = if exhausted {
        None
    } else {
        Some(format!("{}:{}", position.after, position.upper))
    };
    Ok(json!({"messages": messages, "next_continuation": next, "metadata_only": true}))
}
