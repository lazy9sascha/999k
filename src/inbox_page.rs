//! Read-only metadata traversal. Continuation is not handoff, admission, claim or ACK.
use crate::{Error, Queue, conversation::VISIBLE, mailbox_tools::ordinary_request_limit};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, Instant},
};

type Fault = (u16, &'static str);
const MAX_CANDIDATES: usize = 100;
pub const MAX_QUERY_LENGTH: usize = 256;
pub const MAX_CONTINUATION_LENGTH: usize = 107;
pub const ARGUMENTS: &[&str] = &["scope", "query", "order", "limit", "continuation"];

// Candidates are ordered by durable enqueue sequence (including timestamp ties).
// Keep mutable delivery state out of this filter so claims, ACKs and dead letters
// cannot shift page boundaries. Exact observation validates each returned row.
const CANDIDATES: &str = "(
    (m.recipient=?1 AND m.sender IN (SELECT value FROM json_each(?2)))
    OR (?3 AND m.sender=?1 AND m.recipient IN (SELECT value FROM json_each(?2))))
    AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id)
    AND (m.in_reply_to IS NULL OR EXISTS(SELECT 1 FROM messages p WHERE p.id=m.in_reply_to
        AND p.sender=m.recipient AND p.recipient=m.sender AND p.in_reply_to IS NULL
        AND m.correlation_id=COALESCE(p.correlation_id,p.id)
        AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=p.id)))";

struct Position {
    after: i64,
    upper: i64,
}

fn decode(token: &str, fingerprint: &str, high: i64) -> Result<Position, Fault> {
    let invalid = (400, "invalid_continuation");
    let rest = token.strip_prefix("v1:").ok_or(invalid)?;
    let (scope, rest) = rest.split_once(':').ok_or(invalid)?;
    if scope != fingerprint {
        return Err(invalid);
    }
    let (after, upper) = rest.split_once(':').ok_or(invalid)?;
    let position = Position {
        after: after.parse().map_err(|_| invalid)?,
        upper: upper.parse().map_err(|_| invalid)?,
    };
    if position.after < 0 || position.after >= position.upper || position.upper > high {
        return Err(invalid);
    }
    Ok(position)
}

struct PageRequest {
    all: bool,
    newest: bool,
    query: Option<String>,
    limit: usize,
}

impl PageRequest {
    fn fingerprint(
        &self,
        conn: &Connection,
        mailbox: &str,
        peers: &[String],
    ) -> Result<String, Fault> {
        let storage_mailbox = crate::mailbox_generation::current_storage(conn, mailbox)
            .map_err(storage)?
            .ok_or((403, "mailbox_generation_retired"))?;
        let storage_peers = peers
            .iter()
            .map(|peer| crate::mailbox_generation::current_storage(conn, peer))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        let legacy = storage_mailbox == mailbox
            && peers
                .iter()
                .zip(&storage_peers)
                .all(|(peer, stored)| stored.as_ref() == Some(peer));
        let scope = if legacy {
            serde_json::to_vec(&(mailbox, peers, self.all, self.newest, &self.query))
        } else {
            serde_json::to_vec(&(
                mailbox,
                storage_mailbox,
                peers,
                storage_peers,
                self.all,
                self.newest,
                &self.query,
            ))
        }
        .map_err(|_| (503, "serialization_failed"))?;
        Ok(Sha256::digest(scope)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
}

fn read_sent(
    queue: &Queue,
    id: &str,
    mailbox: &str,
    recipient: &str,
    permitted: &str,
) -> Result<Option<Value>, Fault> {
    // A sender already has body visibility through read_thread. Reuse that
    // predicate for sent rows and project the same bounded metadata as inbox.
    let visible: bool = queue.connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM messages m WHERE m.id=?1 AND m.sender=?2 AND {VISIBLE})"),
        params![id, mailbox, permitted, ordinary_request_limit(recipient) as i64],
        |r| r.get(0),
    ).map_err(storage)?;
    if !visible {
        return Ok(None);
    }
    match queue.ordinary_metadata(id, recipient) {
        Ok(value) => Ok((value["state"] != "dead_lettered").then(|| metadata_fields(&value))),
        Err(Error::NotFound | Error::Invalid(_)) => Ok(None),
        Err(Error::Database(error)) => Err(storage(error)),
        Err(_) => Err((503, "storage_unavailable")),
    }
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
        "conversation_id": value["conversation_id"],
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
    if args.keys().any(|key| !ARGUMENTS.contains(&key.as_str())) {
        return Err((400, "invalid_fields"));
    }
    let all = match args.get("scope").map(Value::as_str) {
        None | Some(Some("inbox")) => false,
        Some(Some("all")) => true,
        _ => return Err((400, "invalid_scope")),
    };
    let newest = match args.get("order").map(Value::as_str) {
        None | Some(Some("newest")) => true,
        Some(Some("oldest")) => false,
        _ => return Err((400, "invalid_order")),
    };
    let query = args
        .get("query")
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.is_empty() && text.chars().count() <= MAX_QUERY_LENGTH)
                .map(str::to_ascii_lowercase)
                .ok_or((400, "invalid_query"))
        })
        .transpose()?;
    let limit = match args.get("limit") {
        None => 20,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=50).contains(n))
            .ok_or((400, "invalid_limit"))? as usize,
    };
    let token = args
        .get("continuation")
        .map(|value| {
            value
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= MAX_CONTINUATION_LENGTH)
                .ok_or((400, "invalid_continuation"))
        })
        .transpose()?;
    let request = PageRequest {
        all,
        newest,
        query,
        limit,
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
    crate::mailbox_generation::scope_connection(&c).map_err(storage)?;
    c.pragma_update(None, "query_only", true).map_err(storage)?;
    let tx = c.transaction().map_err(storage)?;
    let permitted = serde_json::to_string(&allowed).map_err(|_| (503, "serialization_failed"))?;
    let fingerprint = request.fingerprint(&tx, mailbox, &allowed)?;
    let high: i64 = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM messages m WHERE {CANDIDATES}"),
            params![mailbox, permitted, request.all],
            |r| r.get(0),
        )
        .map_err(storage)?;
    check_deadline(deadline)?;
    let mut position = match token {
        Some(t) => decode(t, &fingerprint, high)?,
        None => Position {
            after: 0,
            upper: high,
        },
    };
    // New arrivals append to the scoped sequence. Skip those newer rows when
    // continuing in reverse order, without revealing the global queue sequence.
    let (direction, offset) = if request.newest {
        ("DESC", high - position.upper + position.after)
    } else {
        ("ASC", position.after)
    };
    let mut stmt = tx
        .prepare(&format!(
            "SELECT CASE WHEN length(CAST(m.id AS BLOB))<=128 THEN m.id ELSE NULL END, m.recipient,
                    CASE WHEN ?4 IS NULL THEN 1 ELSE COALESCE(instr(lower(m.body),?4)>0,0) END
             FROM messages m WHERE {CANDIDATES} ORDER BY m.seq {direction} LIMIT ?5 OFFSET ?6"
        ))
        .map_err(storage)?;
    let candidates = stmt
        .query_map(
            params![
                mailbox,
                permitted,
                request.all,
                request.query,
                (position.upper - position.after).min(MAX_CANDIDATES as i64),
                offset
            ],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    drop(stmt);
    tx.commit().map_err(storage)?;
    let queue = Queue { connection: c };
    let mut messages = Vec::new();
    // Search must not let unreadable rows affect pagination based on their
    // bodies. Apply the match only after the row passes current observation.
    for (id, recipient, matches_query) in candidates {
        check_deadline(deadline)?;
        if senders(authorize()?) != allowed {
            return Err((403, "scope_changed"));
        }
        position.after += 1;
        if let Some(id) = id {
            let message = if recipient == mailbox {
                read(&id, &allowed)?
            } else {
                read_sent(&queue, &id, mailbox, &recipient, &permitted)?
            };
            if let Some(message) = message
                && matches_query
            {
                messages.push(message);
            }
        }
        if messages.len() == request.limit {
            break;
        }
    }
    if senders(authorize()?) != allowed {
        return Err((403, "scope_changed"));
    }
    check_deadline(deadline)?;
    let next = (position.after < position.upper)
        .then(|| format!("v1:{fingerprint}:{}:{}", position.after, position.upper));
    Ok(json!({"messages": messages, "next_continuation": next, "metadata_only": true}))
}
