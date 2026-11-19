//! Authenticated native dot mailbox. Read/wait never claim or acknowledge;
//! completion and its reply binding share the original SQLite transaction.
use crate::mailbox_engine::{Ack, ReplyCommit, ReplyTable, commit_reply, insert_message};
use crate::{
    MAX_ORDINARY_REQUEST_BYTES,
    dot_mcp_host::{Context, Fault},
    dot_service_preflight::DotPolicy,
    mailbox_tools::correlation_is_readable,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, Fault>;
const PLAIN: &str = "(m.in_reply_to IS NULL OR EXISTS (SELECT 1 FROM messages parent WHERE parent.id=m.in_reply_to AND parent.sender='dot' AND parent.recipient=m.sender AND parent.in_reply_to IS NULL AND m.correlation_id=COALESCE(parent.correlation_id,parent.id) AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=parent.id))) AND NOT EXISTS(SELECT 1 FROM rpc_requests r WHERE r.message_id=m.id)";
const COLUMNS: &str = "m.seq,m.id,m.sender,m.body,m.correlation_id,m.in_reply_to,m.acknowledged_at_ms,m.lease_until_ms,m.attempts,m.conversation_id";
fn fault(reason: &'static str) -> Fault {
    Fault::new(-32602, reason)
}
fn storage(error: rusqlite::Error) -> Fault {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        fault("storage_busy")
    } else {
        fault("setup_or_storage_error")
    }
}
fn queue_storage(error: crate::Error) -> Fault {
    match error {
        crate::Error::Database(error) => storage(error),
        _ => fault("setup_or_storage_error"),
    }
}
fn conflict(reason: &'static str) -> Fault {
    Fault::new(-32009, reason)
}
fn blank(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}
fn identifier(s: &str, limit: usize) -> Result<()> {
    if blank(s) || s.len() > limit || s.chars().any(|c| c < '\u{20}') {
        Err(fault("invalid_string"))
    } else {
        Ok(())
    }
}
fn body(s: &str, limit: usize) -> Result<()> {
    if blank(s) || s.len() > limit {
        Err(fault("invalid_body"))
    } else {
        Ok(())
    }
}
fn now() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| i64::try_from(v.as_millis()).ok())
        .ok_or_else(|| fault("setup_or_storage_error"))
}
#[derive(Clone)]
struct Message {
    seq: i64,
    id: String,
    conversation_id: String,
    sender: String,
    body: String,
    correlation: Option<String>,
    parent: Option<String>,
    ack: Option<i64>,
    lease: Option<i64>,
    attempts: i64,
}
impl Message {
    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            seq: r.get(0)?,
            id: r.get(1)?,
            sender: r.get(2)?,
            body: r.get(3)?,
            correlation: r.get(4)?,
            parent: r.get(5)?,
            ack: r.get(6)?,
            lease: r.get(7)?,
            attempts: r.get(8)?,
            conversation_id: r.get(9)?,
        })
    }
    fn validate(&self) -> Result<()> {
        body(
            &self.body,
            if self.parent.is_some() {
                16384
            } else {
                MAX_ORDINARY_REQUEST_BYTES
            },
        )?;
        for v in [
            Some(&self.id),
            Some(&self.sender),
            self.correlation.as_ref(),
            self.parent.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            identifier(v, 1024)?;
        }
        Ok(())
    }
    fn observed(&self, reply: Option<String>) -> Value {
        let mut v = json!({"id":self.id,"message_id":self.id,"conversation_id":self.conversation_id,"sender":self.sender,"recipient":"dot","body":self.body,"correlation_id":self.correlation.as_deref().unwrap_or(&self.id),"state":if reply.is_some(){"replied"}else{"pending"},"reply_id":reply});
        if let Some(parent) = &self.parent {
            v["state"] = json!("reply_received");
            v["in_reply_to"] = json!(parent);
        }
        v
    }
}
// In-memory traversal only. It is neither a public cursor nor handoff evidence.
struct WaitProgress {
    owner: String,
    terminal_cursor: Option<String>,
    senders: Vec<String>,
    sweep: Option<crate::queue_progress::Sweep>,
}
pub struct Mailbox {
    db: PathBuf,
    policy: PathBuf,
    wait: Mutex<Option<WaitProgress>>,
}
impl Mailbox {
    pub fn new(db: PathBuf, policy: PathBuf) -> Self {
        Self {
            db,
            policy,
            wait: Mutex::new(None),
        }
    }
    fn access(&self, owner: &str, bearer: &str, context: &Context<'_>) -> Result<DotPolicy> {
        if context.cancelled() {
            return Err(Fault::new(-32021, "wait_cancelled"));
        }
        let policy = DotPolicy::load(&self.policy).map_err(|e| {
            if e.reason == "setup_or_storage_error" {
                Fault::new(-32003, "policy_unavailable")
            } else {
                Fault::new(e.code, e.reason)
            }
        })?;
        if policy
            .authorize(bearer)
            .map_err(|e| Fault::new(e.code, e.reason))?
            != owner
        {
            return Err(Fault::new(-32001, "unauthorized"));
        }
        Ok(policy)
    }
    fn connect(&self, write: bool, budget: Duration) -> rusqlite::Result<Connection> {
        let c = Connection::open_with_flags(
            &self.db,
            (if write {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            }) | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.busy_timeout(budget)?;
        crate::mailbox_generation::scope_connection(&c)?;
        Ok(c)
    }
    fn schema(c: &Connection) -> Result<()> {
        crate::validate_schema(c).map_err(|err| match err {
            crate::Error::Database(error) => storage(error),
            _ => fault("requires_current_schema_queue"),
        })?;
        c.prepare("SELECT message_id,reply_id FROM dot_event_replies LIMIT 0")
            .map_err(storage)?;
        Ok(())
    }
    fn message(c: &Connection, id: &str, policy: &DotPolicy) -> Result<Message> {
        identifier(id, 128)?;
        let row=c.query_row(&format!("SELECT {COLUMNS} FROM messages m WHERE m.id=? AND m.recipient='dot' AND m.dead_lettered_at_ms IS NULL AND {PLAIN}"),[id],Message::row).optional().map_err(storage)?;
        let message = row
            .filter(|m| policy.allowed_senders().contains(&m.sender))
            .ok_or_else(|| Fault::new(-32004, "message_not_available"))?;
        message.validate()?;
        Ok(message)
    }
    pub fn read(
        &self,
        owner: &str,
        bearer: &str,
        message_id: &str,
        context: &Context<'_>,
    ) -> Result<Value> {
        let policy = self.access(owner, bearer, context)?;
        let c = self
            .connect(false, Duration::from_millis(50))
            .map_err(storage)?;
        Self::schema(&c)?;
        let m = Self::message(&c, message_id, &policy)?;
        let reply = c
            .query_row(
                "SELECT reply_id FROM dot_event_replies WHERE message_id=?",
                [message_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(storage)?;
        let current = self.access(owner, bearer, context)?;
        if !current.allowed_senders().contains(&m.sender) {
            return Err(Fault::new(-32004, "message_not_available"));
        }
        self.projected(&m, reply)
    }
    pub fn inbox(
        &self,
        owner: &str,
        bearer: &str,
        args: &Value,
        context: &Context<'_>,
    ) -> Result<Value> {
        use crate::inbox_page;
        let status = |error: Fault| {
            let code = match error.code {
                -32602 => 400,
                -32001 => 401,
                -32003 => 403,
                -32021 => 499,
                _ => 503,
            };
            (code, error.reason)
        };
        inbox_page::list_with(
            &self.db,
            "dot",
            args,
            context.deadline,
            || {
                self.access(owner, bearer, context)
                    .map(|policy| policy.allowed_senders().to_vec())
                    .map_err(status)
            },
            || Ok(()),
            |id, _| match self.read(owner, bearer, id, context) {
                Ok(value) => Ok(Some(inbox_page::metadata_fields(&value))),
                Err(error)
                    if matches!(
                        error.reason,
                        "message_not_available"
                            | "invalid_body"
                            | "invalid_string"
                            | "invalid_conversation_context"
                    ) =>
                {
                    Ok(None)
                }
                Err(error) => Err(status(error)),
            },
        )
        .map_err(|(status, reason)| {
            Fault::new(
                match status {
                    400 => -32602,
                    401 => -32001,
                    403 => -32003,
                    499 => -32021,
                    _ => -32603,
                },
                reason,
            )
        })
    }
    pub fn read_thread(
        &self,
        owner: &str,
        bearer: &str,
        args: &Value,
        context: &Context<'_>,
    ) -> Result<Value> {
        let read: crate::conversation::ThreadRead =
            serde_json::from_value(args.clone()).map_err(|_| fault("invalid_fields"))?;
        let policy = self.access(owner, bearer, context)?;
        let page = crate::Queue::read_thread(
            &self.db,
            crate::conversation::ThreadScope {
                mailbox: "dot",
                peers: policy.allowed_senders(),
                request_bytes: 8192,
            },
            &read,
        )
        .map_err(|error| match error {
            crate::Error::Invalid("invalid_thread_cursor") => fault("invalid_thread_cursor"),
            crate::Error::Invalid("invalid_conversation_identifier") => fault("invalid_fields"),
            error => queue_storage(error),
        });
        if self.access(owner, bearer, context)?.allowed_senders() != policy.allowed_senders() {
            return Err(Fault::new(-32003, "thread_scope_changed"));
        }
        serde_json::to_value(page?).map_err(|_| fault("internal_error"))
    }
    fn projected(&self, m: &Message, reply: Option<String>) -> Result<Value> {
        let c = self
            .connect(false, Duration::from_millis(50))
            .map_err(storage)?;
        let mut observed = m.observed(reply);
        if let Some(participation) =
            crate::conversation::load(&c, &m.id).map_err(|error| match error {
                crate::Error::Database(error) => storage(error),
                _ => fault("invalid_conversation_context"),
            })?
        {
            let envelope = crate::Envelope {
                sender: m.sender.clone(),
                recipient: "dot".into(),
                body: m.body.clone(),
                correlation_id: m.correlation.clone(),
                in_reply_to: m.parent.clone(),
                idempotency_key: c
                    .query_row(
                        "SELECT idempotency_key FROM messages WHERE id=?1",
                        [&m.id],
                        |r| r.get(0),
                    )
                    .map_err(storage)?,
            };
            participation
                .validate(&envelope)
                .map_err(|_| fault("invalid_conversation_context"))?;
            observed["conversation"] = json!(participation);
        }
        Ok(observed)
    }
    /// Authenticated consumer confirms this terminal body reached its approved
    /// receiving boundary. This does not assert downstream model processing.
    pub fn confirm_terminal(
        &self,
        owner: &str,
        bearer: &str,
        message_id: &str,
        context: &Context<'_>,
    ) -> Result<Value> {
        self.access(owner, bearer, context)?;
        let mut c = self
            .connect(
                true,
                context
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(2)),
            )
            .map_err(storage)?;
        Self::schema(&c)?;
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let policy = self.access(owner, bearer, context)?;
        let m = Self::message(&tx, message_id, &policy)?;
        let duplicate = crate::deferred_terminal::confirm(&tx, "dot", message_id, now()?).map_err(
            |e| match e {
                crate::Error::Database(e) => storage(e),
                crate::Error::Invalid(reason) => conflict(reason),
                _ => conflict("invalid_terminal_relationship"),
            },
        )?;
        self.commit_access(owner, bearer, &m.sender, context)?;
        tx.commit().map_err(storage)?;
        crate::trace_terminal_commit(message_id, duplicate);
        Ok(json!({"state":"acknowledged","message_id":message_id,"duplicate":duplicate}))
    }
    pub fn wait(
        &self,
        owner: &str,
        bearer: &str,
        timeout_ms: u64,
        cursor: Option<&str>,
        context: &Context<'_>,
    ) -> Result<Value> {
        self.wait_common(owner, bearer, timeout_ms, cursor, context)
    }
    /// Existing default wait projected through bounded common observation.
    fn wait_common(
        &self,
        owner: &str,
        bearer: &str,
        timeout_ms: u64,
        cursor: Option<&str>,
        context: &Context<'_>,
    ) -> Result<Value> {
        if timeout_ms > 10000 {
            return Err(fault("invalid_wait_timeout"));
        }
        if let Some(c) = cursor {
            identifier(c, 128)?;
        }
        let mut guard = self
            .wait
            .try_lock()
            .map_err(|_| Fault::new(-32020, "wait_already_active"))?;
        let deadline = (Instant::now() + Duration::from_millis(timeout_ms)).min(context.deadline);
        let query = crate::queue_progress::ObservationQuery::FirstDelivery;
        let initial = self.access(owner, bearer, context)?;
        if guard.as_ref().is_none_or(|p| {
            p.owner != owner
                || p.terminal_cursor.as_deref() != cursor
                || p.senders != initial.allowed_senders()
        }) {
            *guard = Some(WaitProgress {
                owner: owner.into(),
                terminal_cursor: cursor.map(str::to_owned),
                senders: initial.allowed_senders().to_vec(),
                sweep: None,
            });
        }
        let progress = guard.as_mut().unwrap();
        let mut sweep = progress.sweep.clone();
        let mut senders = progress.senders.clone();

        loop {
            let policy = self.access(owner, bearer, context)?;
            if senders != policy.allowed_senders() {
                senders = policy.allowed_senders().to_vec();
                sweep = None;
            }
            // Do not wait on SQLite locks inside a bounded observation.
            let scan = (|| -> Result<_> {
                let c = self.connect(false, Duration::ZERO).map_err(storage)?;
                Self::schema(&c)?;
                let after = if let Some(id) = cursor {
                    let m = Self::message(&c, id, &policy)?;
                    if m.parent.is_none() {
                        return Err(fault("cursor_requires_terminal_answer"));
                    }
                    m.seq
                } else {
                    0
                };
                let queue = crate::Queue { connection: c };
                let authority_error = std::cell::RefCell::new(None);
                let authorize = || match self.access(owner, bearer, context) {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        *authority_error.borrow_mut() = Some(error);
                        Err(crate::Error::Invalid("observation_authority_changed"))
                    }
                };
                let map_error = |error| {
                    if let Some(error) = authority_error.borrow_mut().take() {
                        return error;
                    }
                    match error {
                        crate::Error::Database(error) => storage(error),
                        crate::Error::NotFound => Fault::new(-32004, "message_not_available"),
                        _ => fault("observation_failed"),
                    }
                };
                if sweep.is_none() {
                    sweep = Some(
                        queue
                            .start_sweep("dot", query.clone(), authorize)
                            .map_err(map_error)?,
                    );
                }
                let page = queue
                    .observe_page("dot", &query, sweep.as_ref().unwrap(), 100, authorize)
                    .map_err(map_error)?;
                let mut rows = Vec::new();
                let observed_at = now()?;
                for (seq, message) in page.messages {
                    if !policy.allowed_senders().contains(&message.envelope.sender) {
                        continue;
                    }
                    // Profile disclosure/relationship bounds remain separate from common validity.
                    // Stored fields use the profile 1KiB bound, not the 128-byte tool-argument bound.
                    let row = queue.connection.query_row(
                        &format!("SELECT {COLUMNS} FROM messages m WHERE m.seq=?1 AND m.recipient='dot' AND m.dead_lettered_at_ms IS NULL AND {PLAIN}"),
                        [seq], Message::row).optional().map_err(storage)?;
                    let Some(m) = row else {
                        continue;
                    };
                    if m.ack.is_none()
                        && m.attempts == 0
                        && m.lease.is_none_or(|until| until <= observed_at)
                        && (m.parent.is_none() || seq > after)
                    {
                        let replied: bool = queue.connection.query_row("SELECT EXISTS(SELECT 1 FROM dot_event_replies WHERE message_id=?1)", [&m.id], |r| r.get(0)).map_err(storage)?;
                        if !replied {
                            rows.push((seq, Some(m)));
                        }
                    }
                }
                sweep = Some(page.continuation);
                Ok((rows, page.exhausted))
            })();
            let (rows, exhausted) = scan?;
            let current = self.access(owner, bearer, context)?;
            if current.allowed_senders() != senders {
                continue;
            }
            for (seq, row) in &rows {
                let current = self.access(owner, bearer, context)?;
                if current.allowed_senders() != senders {
                    sweep = None;
                    break;
                }
                if let Some(m) = row
                    && current.allowed_senders().contains(&m.sender)
                    && m.validate().is_ok()
                {
                    let message = self.projected(m, None)?;
                    // Returned rows do not pin the sweep. Revisit pending work in the next sweep.
                    let scanned = sweep.as_ref().unwrap();
                    progress.sweep = if exhausted && *seq == scanned.after() {
                        None
                    } else {
                        Some(
                            scanned
                                .after_observed(*seq)
                                .map_err(|_| fault("observation_failed"))?,
                        )
                    };
                    progress.senders = senders;
                    return Ok(json!({"state":"message","message":message}));
                }
            }
            progress.senders = senders.clone();
            progress.sweep = if exhausted { None } else { sweep.clone() };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(json!({"state":"timeout","message":null}));
            }
            if exhausted {
                sweep = None;
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
        }
    }
    pub fn wait_filtered(
        &self,
        owner: &str,
        bearer: &str,
        timeout_ms: u64,
        filter: (Option<&str>, Option<&str>),
        context: &Context<'_>,
    ) -> Result<Value> {
        let (cursor, in_reply_to) = filter;
        if in_reply_to.is_none() {
            return self.wait_common(owner, bearer, timeout_ms, cursor, context);
        }
        if let Some(id) = in_reply_to {
            identifier(id, 128)?;
        }
        if timeout_ms > 10000 {
            return Err(fault("invalid_wait_timeout"));
        }
        if let Some(c) = cursor {
            identifier(c, 128)?;
        }
        let _guard = self
            .wait
            .try_lock()
            .map_err(|_| Fault::new(-32020, "wait_already_active"))?;
        let deadline = (Instant::now() + Duration::from_millis(timeout_ms)).min(context.deadline);
        let mut skipped = 0;
        let mut senders = Vec::new();
        loop {
            let policy = self.access(owner, bearer, context)?;
            if senders != policy.allowed_senders() {
                senders = policy.allowed_senders().to_vec();
                skipped = 0;
            }
            // Retry outside SQLite so separate statements cannot each consume
            // the remaining deadline as an independent busy timeout.
            let scan = (|| -> Result<Vec<(i64, Option<Message>)>> {
                let c = self.connect(false, Duration::ZERO).map_err(storage)?;
                Self::schema(&c)?;
                let after = if let Some(id) = cursor {
                    let m = Self::message(&c, id, &policy)?;
                    if m.parent.is_none() {
                        return Err(fault("cursor_requires_terminal_answer"));
                    }
                    m.seq
                } else {
                    0
                };
                let mut statement=c.prepare(&format!("SELECT {COLUMNS} FROM messages m WHERE m.recipient='dot' AND m.seq>?1 AND (m.in_reply_to IS NULL OR m.seq>?2) AND m.acknowledged_at_ms IS NULL AND m.dead_lettered_at_ms IS NULL AND m.attempts=0 AND (m.lease_until_ms IS NULL OR m.lease_until_ms<=?3) AND m.sender IN (SELECT value FROM json_each(?4)) AND {PLAIN} AND (?5 IS NULL OR m.in_reply_to=?5) AND NOT EXISTS(SELECT 1 FROM dot_event_replies d WHERE d.message_id=m.id) ORDER BY m.seq LIMIT 100")).map_err(storage)?;
                let rows = statement
                    .query_map(
                        params![
                            skipped,
                            after,
                            now()?,
                            serde_json::to_string(&senders)
                                .map_err(|_| fault("setup_or_storage_error"))?,
                            in_reply_to
                        ],
                        |r| Ok((r.get::<_, i64>(0)?, Message::row(r).ok())),
                    )
                    .map_err(storage)?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
            })();
            let rows = match scan {
                Ok(rows) => rows,
                Err(e) if e.reason == "storage_busy" => {
                    self.access(owner, bearer, context)?;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(e);
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(100)));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let current = self.access(owner, bearer, context)?;
            if current.allowed_senders() != senders {
                continue;
            }
            for (seq, row) in &rows {
                let current = self.access(owner, bearer, context)?;
                if current.allowed_senders() != senders {
                    skipped = 0;
                    break;
                }
                if let Some(m) = row
                    && current.allowed_senders().contains(&m.sender)
                    && m.validate().is_ok()
                {
                    return Ok(json!({"state":"message","message":self.projected(m, None)?}));
                }
                skipped = *seq;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(json!({"state":"timeout","message":null}));
            }
            if rows.len() < 100 || rows.last().is_some_and(|r| skipped < r.0) {
                skipped = 0;
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
        }
    }
    /// Registered send targets only; presence is not inferred from registration.
    pub fn recipients(&self, owner: &str, bearer: &str, context: &Context<'_>) -> Result<Value> {
        let policy = self.access(owner, bearer, context)?;
        Ok(json!({"recipients":policy.allowed_senders(),"connection_status":"unknown"}))
    }
    /// New ordinary request only: fixed dot sender, central peers, no ACK.
    pub fn send(
        &self,
        owner: &str,
        bearer: &str,
        recipient: &str,
        text: &str,
        key: &str,
        context: &Context<'_>,
    ) -> Result<Value> {
        self.send_with_conversation(owner, bearer, recipient, text, key, None, None, context)
    }
    /// Protected context is supplied explicitly, never parsed from the body.
    #[allow(clippy::too_many_arguments)]
    pub fn send_with_conversation(
        &self,
        owner: &str,
        bearer: &str,
        recipient: &str,
        text: &str,
        key: &str,
        participation: Option<crate::conversation::Context>,
        continuation: Option<&str>,
        context: &Context<'_>,
    ) -> Result<Value> {
        identifier(recipient, 128)?;
        if let Some(parent) = continuation {
            identifier(parent, 128)?;
            if participation.is_some() {
                return Err(fault("invalid_conversation_context"));
            }
        }
        identifier(key, 128)?;
        body(text, MAX_ORDINARY_REQUEST_BYTES)?;
        if participation
            .as_ref()
            .is_some_and(|p| !correlation_is_readable("dot", recipient, &p.conversation_id))
        {
            return Err(fault("invalid_conversation_identifier"));
        }
        let authorize = |target: &str| -> Result<()> {
            let policy = self.access(owner, bearer, context)?;
            if !policy.allowed_senders().iter().any(|p| p == target) {
                return Err(Fault::new(-32003, "recipient_not_authorized"));
            }
            Ok(())
        };
        authorize(recipient)?;
        if let Some(participation) = &participation {
            for participant in &participation.participants {
                authorize(&participant.mailbox)?;
            }
        }
        let mut queue = crate::Queue::open_existing(&self.db).map_err(queue_storage)?;
        let mut denied = None;
        let envelope = crate::Envelope {
            sender: "dot".into(),
            recipient: recipient.into(),
            body: text.into(),
            correlation_id: participation.as_ref().map(|p| p.conversation_id.clone()),
            in_reply_to: None,
            idempotency_key: Some(key.into()),
        };
        let policy = continuation
            .map(|_| self.access(owner, bearer, context))
            .transpose()?;
        let mut check = |target: &str| {
            authorize(target)
                .and_then(|()| {
                    if let Some(before) = &policy
                        && self.access(owner, bearer, context)?.allowed_senders()
                            != before.allowed_senders()
                    {
                        return Err(Fault::new(-32003, "thread_scope_changed"));
                    }
                    Ok(())
                })
                .map_err(|e| {
                    denied = Some(e);
                    crate::Error::Invalid("authorization_changed")
                })
        };
        let result = match (participation, continuation, policy.as_ref()) {
            (Some(participation), _, _) => {
                queue.send_conversation_checked(envelope, participation, |_, target| check(target))
            }
            (None, Some(parent), Some(policy)) => queue.continue_checked(
                envelope,
                parent,
                crate::conversation::ThreadScope {
                    mailbox: "dot",
                    peers: policy.allowed_senders(),
                    request_bytes: 8192,
                },
                || check(recipient),
            ),
            _ => queue.send_checked(envelope, || check(recipient)),
        };
        if let Some(e) = denied {
            return Err(e);
        }
        let result = result.map_err(|e| match e {
            crate::Error::Conflict => conflict("send_conflict"),
            crate::Error::NotFound => Fault::new(-32004, "message_not_available"),
            crate::Error::Invalid(reason) => fault(reason),
            error => queue_storage(error),
        })?;
        Ok(
            json!({"state":"queued","message_id":result.message.id,"conversation_id":result.message.conversation_id,"duplicate":result.duplicate}),
        )
    }
    pub fn reply(
        &self,
        owner: &str,
        bearer: &str,
        message_id: &str,
        text: &str,
        context: &Context<'_>,
    ) -> Result<Value> {
        body(text, 8192)?;
        identifier(message_id, 128)?;
        self.access(owner, bearer, context)?;
        let mut c = self
            .connect(
                true,
                context
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(2)),
            )
            .map_err(storage)?;
        Self::schema(&c)?;
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        Self::schema(&tx)?;
        let policy = self.access(owner, bearer, context)?;
        let m = Self::message(&tx, message_id, &policy)?;
        if m.parent.is_some() {
            return Err(conflict("reply_is_terminal"));
        }
        let key = format!("gw-reply-{}", m.id);
        let correlation = m.correlation.as_deref().unwrap_or(&m.id);
        type Old = (String, String, Option<String>, Option<String>);
        let old:Option<Old>=tx.query_row("SELECT id,body,in_reply_to,correlation_id FROM messages WHERE sender='dot' AND recipient=?1 AND idempotency_key=?2",params![m.sender,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(storage)?;
        // Raw send may have created a different answer without binding or ACK.
        // Canonical-key absence must never authorize a second stored identity.
        let identities: Vec<String> = tx
            .prepare("SELECT id FROM messages WHERE sender='dot' AND in_reply_to=?1")
            .map_err(storage)?
            .query_map([&m.id], |r| r.get(0))
            .map_err(storage)?
            .collect::<std::result::Result<_, _>>()
            .map_err(storage)?;
        if identities.len() > 1
            || identities
                .first()
                .is_some_and(|id| old.as_ref().is_none_or(|old| &old.0 != id))
        {
            return Err(conflict("reconciliation_required"));
        }
        if let Some((id, old_body, parent, old_correlation)) = old {
            let binding: Option<String> = tx
                .query_row(
                    "SELECT reply_id FROM dot_event_replies WHERE message_id=?",
                    [&m.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(storage)?;
            if binding.as_deref() != Some(&id) {
                return Err(conflict("reconciliation_required"));
            }
            if old_body != text
                || parent.as_deref() != Some(&m.id)
                || old_correlation.as_deref() != Some(correlation)
            {
                return Err(conflict("reply_conflict"));
            }
            if m.ack.is_none() {
                return Err(conflict("reconciliation_required"));
            }
            identifier(&id, 1024)?;
            commit_reply(
                tx,
                ReplyCommit {
                    original_id: &m.id,
                    reply_id: &id,
                    reply_new: false,
                    ack: Ack::Unchanged,
                    binding: None,
                },
                || self.commit_access(owner, bearer, &m.sender, context),
                storage,
            )?;
            return Ok(json!({"state":"replied","reply_id":id,"duplicate":true}));
        }
        let now = now()?;
        if m.ack.is_some() || m.lease.unwrap_or(0) > now || m.attempts != 0 {
            return Err(conflict("existing_claim_requires_reconciliation"));
        }
        let thread = crate::conversation::parent_thread(&tx, &m.id).map_err(storage)?;
        let id = insert_message(
            &tx,
            crate::Envelope {
                sender: "dot".into(),
                recipient: m.sender.clone(),
                body: text.into(),
                correlation_id: Some(correlation.into()),
                in_reply_to: Some(m.id.clone()),
                idempotency_key: Some(key),
            },
            now,
            Some(&thread),
        )
        .map_err(storage)?
        .id;
        commit_reply(
            tx,
            ReplyCommit {
                original_id: &m.id,
                reply_id: &id,
                reply_new: true,
                ack: Ack::DeferredClaim {
                    now,
                    until: now + 1000,
                },
                binding: Some(ReplyTable::Dot),
            },
            || self.commit_access(owner, bearer, &m.sender, context),
            storage,
        )?;
        Ok(json!({"state":"replied","reply_id":id,"duplicate":false}))
    }
    fn commit_access(
        &self,
        owner: &str,
        bearer: &str,
        sender: &str,
        context: &Context<'_>,
    ) -> Result<()> {
        if !self
            .access(owner, bearer, context)?
            .allowed_senders()
            .iter()
            .any(|s| s == sender)
        {
            return Err(Fault::new(-32004, "message_not_available"));
        }
        Ok(())
    }
}
