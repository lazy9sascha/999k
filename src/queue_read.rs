//! Read-only queue observations and owner-private inspection.
use crate::conversation::{
    ThreadMessage, ThreadPage, ThreadRead, ThreadScope, VISIBLE, identifier,
};
use crate::{
    COLUMNS, DeadLetterEntry, DeadLetterInspection, DeadLetterPage, Error,
    MAX_ORDINARY_REQUEST_BYTES, Message, NeverClaimedBacklog, Queue, Result, nonempty, now_ms,
    private_file, read_message, runtime_admission, validate_queue_journal, validate_schema,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{io::Read, path::Path, time::Duration};

const DEAD_LETTER_PAGE_SIZE: usize = 50;
const DEAD_LETTER_REQUEST: &str = "dead_lettered_at_ms IS NOT NULL
    AND in_reply_to IS NULL AND acknowledged_at_ms IS NULL
    AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)";
const DEAD_LETTER_MAILBOXES: &str = "recipient IN (SELECT value FROM json_each(?2))";

fn read_dead_letter(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeadLetterEntry> {
    Ok(DeadLetterEntry {
        id: row.get(0)?,
        inspection: DeadLetterInspection::DeadLettered {
            attempts: row.get(1)?,
            dead_lettered_at_ms: row.get(2)?,
        },
    })
}

const PAGE_SIZE: usize = 20;

impl Queue {
    /// Trusted-host observation only: no claim, receipt, ACK, or RPC visibility.
    // OBS-01, OBS-02: observation without claim or ACK.
    pub fn observe_ordinary(&self, id: &str, recipient: &str) -> Result<Message> {
        nonempty(id)?;
        nonempty(recipient)?;
        self.connection.query_row(
            &format!("SELECT {COLUMNS} FROM messages WHERE id=?1 AND recipient=?2 AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)
             AND length(CAST(body AS BLOB)) BETWEEN 1 AND CASE WHEN in_reply_to IS NULL THEN {MAX_ORDINARY_REQUEST_BYTES} ELSE 16384 END
             AND length(CAST(id AS BLOB))<=128 AND length(CAST(sender AS BLOB))<=128 AND length(CAST(recipient AS BLOB))<=128
             AND (correlation_id IS NULL OR length(CAST(correlation_id AS BLOB))<=128)
             AND (in_reply_to IS NULL OR length(CAST(in_reply_to AS BLOB))<=128)
             AND (idempotency_key IS NULL OR length(CAST(idempotency_key AS BLOB))<=128)"),
            params![id, recipient], read_message,
        ).optional()?.ok_or(Error::NotFound)
    }
    /// Ordinary inbox metadata. Exact envelope bounds and terminal relationships still apply.
    pub fn ordinary_metadata(&self, id: &str, recipient: &str) -> Result<serde_json::Value> {
        let message = self.observe_ordinary(id, recipient)?;
        runtime_admission::validate_terminal(&self.connection, id, recipient)?;
        let (acked, attempts, lease_until, reply_id, dead_lettered): (bool, i64, Option<i64>, Option<String>, Option<i64>) = self.connection.query_row(
            "SELECT m.acknowledged_at_ms IS NOT NULL,m.attempts,m.lease_until_ms,CASE WHEN m.acknowledged_at_ms IS NOT NULL THEN
             (SELECT r.id FROM messages r WHERE r.sender=m.recipient AND r.recipient=m.sender AND r.in_reply_to=m.id
              AND r.correlation_id=COALESCE(m.correlation_id,m.id) AND r.idempotency_key='gw-reply-'||m.id
              AND length(CAST(r.id AS BLOB))<=128 AND NOT EXISTS(SELECT 1 FROM rpc_requests p WHERE p.message_id=r.id) LIMIT 1) END,
             m.dead_lettered_at_ms
             FROM messages m WHERE m.id=?1 AND m.recipient=?2", params![id,recipient],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        )?;
        let observed_at = now_ms()?;
        let e = message.envelope;
        let state = if dead_lettered.is_some() {
            "dead_lettered"
        } else if e.in_reply_to.is_some() {
            "reply_received"
        } else if reply_id.is_some() {
            "replied"
        } else if acked {
            "acknowledged"
        } else if attempts > 0 {
            if lease_until.is_some_and(|until| until > observed_at) {
                "claimed"
            } else {
                "reconciliation_required"
            }
        } else {
            "pending"
        };
        Ok(
            serde_json::json!({"id":message.id,"sender":e.sender,"recipient":e.recipient,
            "message_id":message.id,"conversation_id":message.conversation_id,"correlation_id":e.correlation_id,"in_reply_to":e.in_reply_to,"state":state,"reply_id":reply_id}),
        )
    }
    pub const ORDINARY_SCAN_PAGE_SIZE: usize = 100;

    /// Observe requests by default, or answers to one original. Filter before the page limit.
    /// Keep sequence positions for rows that are not readable by exact observation.
    /// No claim, ACK or schema changes.
    pub fn scan_ordinary(
        &self,
        recipient: &str,
        after_seq: i64,
        in_reply_to: Option<&str>,
    ) -> Result<Vec<(i64, Option<Message>)>> {
        nonempty(recipient)?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT seq,id FROM messages WHERE recipient=?1 AND seq>?2
             AND ((?4 IS NULL AND in_reply_to IS NULL) OR (?4 IS NOT NULL AND in_reply_to=?4))
             AND acknowledged_at_ms IS NULL AND dead_lettered_at_ms IS NULL AND attempts=0
             AND receipt IS NULL AND (lease_until_ms IS NULL OR lease_until_ms<=?3)
             AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)
             ORDER BY seq LIMIT {}",
            Self::ORDINARY_SCAN_PAGE_SIZE
        ))?;
        let ids = statement
            .query_map(
                params![recipient, after_seq, now_ms()?, in_reply_to],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Use the same stored-envelope bounds as direct observation.
        ids.into_iter()
            .map(|(seq, id)| match self.observe_ordinary(&id, recipient) {
                Ok(message) => Ok((seq, Some(message))),
                Err(Error::NotFound) => Ok((seq, None)),
                Err(error) => Err(error),
            })
            .collect()
    }

    /// Newest page first; each returned page is in queue order.
    /// Zero visible rows are indistinguishable from an unknown conversation.
    pub fn read_thread(
        db: &std::path::Path,
        scope: ThreadScope<'_>,
        read: &ThreadRead,
    ) -> Result<ThreadPage> {
        identifier(&read.conversation_id)?;
        let peers = serde_json::to_string(scope.peers)?;
        let mut c = Self::open_inspection_queue(db)?;
        let tx = c.transaction()?;
        let before: Option<i64> = match &read.cursor {
            Some(cursor) => {
                identifier(cursor)?;
                Some(tx.query_row(
                    &format!("SELECT m.seq FROM messages m WHERE m.conversation_id=?1 AND {VISIBLE} AND m.id=?5"),
                    params![read.conversation_id, scope.mailbox, peers, scope.request_bytes, cursor],
                    |r| r.get(0),
                ).optional()?.ok_or(Error::Invalid("invalid_thread_cursor"))?)
            }
            None => None,
        };
        let mut stmt = tx.prepare(&format!(
            "SELECT m.id,m.conversation_id,m.sender,m.recipient,m.body,m.in_reply_to
             FROM messages m WHERE m.conversation_id=?1 AND {VISIBLE}
             AND (?5 IS NULL OR m.seq<?5) ORDER BY m.seq DESC LIMIT ?6"
        ))?;
        let mut messages = stmt
            .query_map(
                params![
                    read.conversation_id,
                    scope.mailbox,
                    peers,
                    scope.request_bytes,
                    before,
                    PAGE_SIZE as i64 + 1
                ],
                |r| {
                    Ok(ThreadMessage {
                        message_id: r.get(0)?,
                        conversation_id: r.get(1)?,
                        sender: r.get(2)?,
                        recipient: r.get(3)?,
                        body: r.get(4)?,
                        in_reply_to: r.get(5)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if messages.len() > PAGE_SIZE {
            messages.pop();
            messages.last().map(|m| m.message_id.clone())
        } else {
            None
        };
        messages.reverse();
        Ok(ThreadPage {
            messages,
            next_cursor,
        })
    }

    /// Inspect one quarantined ordinary request in an owner-private current queue.
    /// No writes, claims, repair, migration, or file creation.
    pub fn inspect_dead_letter(path: &Path, id: &str) -> Result<DeadLetterInspection> {
        Ok(Self::dead_letter_entry(path, id, None)?
            .map_or(DeadLetterInspection::NotDeadLetteredOrMissing, |entry| {
                entry.inspection
            }))
    }
    /// Inspect one quarantined ordinary request in the caller's authorized mailboxes.
    pub fn inspect_mailbox_dead_letter(
        path: &Path,
        mailboxes: &[String],
        id: &str,
    ) -> Result<Option<DeadLetterEntry>> {
        Self::dead_letter_entry(path, id, Some(mailboxes))
    }
    fn dead_letter_entry(
        path: &Path,
        id: &str,
        mailboxes: Option<&[String]>,
    ) -> Result<Option<DeadLetterEntry>> {
        nonempty(id)?;
        if id.len() > 128 {
            return Err(Error::Invalid("message ID too long"));
        }
        let connection = Self::open_inspection_queue(path)?;
        let scope = mailboxes.map(serde_json::to_string).transpose()?;
        let scoped = if scope.is_some() {
            format!("AND {DEAD_LETTER_MAILBOXES}")
        } else {
            String::new()
        };
        let args = rusqlite::params_from_iter(std::iter::once(id).chain(scope.as_deref()));
        Ok(connection
            .query_row(
                &format!(
                    "SELECT id,attempts,dead_lettered_at_ms FROM messages
                WHERE id=?1 AND {DEAD_LETTER_REQUEST} {scoped}"
                ),
                args,
                read_dead_letter,
            )
            .optional()?)
    }
    /// List one bounded page in queue order. The continuation identifies the last visible entry.
    pub fn list_mailbox_dead_letters(
        path: &Path,
        mailboxes: &[String],
        continuation: Option<&str>,
    ) -> Result<DeadLetterPage> {
        if continuation.is_some_and(|id| id.is_empty() || id.len() > 128) {
            return Err(Error::Invalid("invalid_continuation"));
        }
        let connection = Self::open_inspection_queue(path)?;
        let scope = serde_json::to_string(mailboxes)?;
        // Recheck scope for the cursor itself. Never reveal an internal sequence number.
        let after = match continuation {
            Some(id) => connection
                .query_row(
                    &format!(
                        "SELECT seq FROM messages WHERE id=?1
                    AND {DEAD_LETTER_REQUEST} AND {DEAD_LETTER_MAILBOXES}"
                    ),
                    params![id, scope],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .ok_or(Error::Invalid("invalid_continuation"))?,
            None => 0,
        };
        let mut stmt = connection.prepare(&format!(
            "SELECT id,attempts,dead_lettered_at_ms FROM messages WHERE seq>?1
             AND {DEAD_LETTER_REQUEST} AND {DEAD_LETTER_MAILBOXES} ORDER BY seq LIMIT ?3"
        ))?;
        let mut entries = stmt
            .query_map(
                params![after, scope, DEAD_LETTER_PAGE_SIZE as i64 + 1],
                read_dead_letter,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_continuation = if entries.len() > DEAD_LETTER_PAGE_SIZE {
            entries.pop();
            entries.last().map(|entry| entry.id.clone())
        } else {
            None
        };
        Ok(DeadLetterPage {
            entries,
            next_continuation,
        })
    }
    /// Check that the owner-private queue and its core tables can be read without writes.
    pub fn check_readable(path: &Path) -> Result<()> {
        let connection = Self::open_inspection_queue(path)?;
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages), EXISTS(SELECT 1 FROM rpc_requests)",
            [],
            |_| Ok(()),
        )?;
        Ok(())
    }

    /// Inspect pending ordinary requests without claiming or changing them.
    pub fn inspect_never_claimed_backlog(path: &Path) -> Result<NeverClaimedBacklog> {
        let connection = Self::open_inspection_queue(path)?;
        let (count, oldest): (i64, Option<i64>) = connection.query_row(
            "SELECT count(*), min(created_at_ms) FROM messages
             WHERE attempts=0 AND receipt IS NULL AND lease_until_ms IS NULL
             AND acknowledged_at_ms IS NULL AND dead_lettered_at_ms IS NULL
             AND in_reply_to IS NULL
             AND NOT EXISTS(SELECT 1 FROM rpc_requests WHERE message_id=messages.id)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let now = now_ms()?;
        Ok(NeverClaimedBacklog {
            count,
            oldest_age_ms: oldest.map(|created| now.saturating_sub(created).max(0)),
        })
    }

    fn open_inspection_queue(path: &Path) -> Result<Connection> {
        use rusqlite::OpenFlags;

        let mut file = private_file::open_private_file(path)
            .map_err(|_| Error::Invalid("owner_private_queue_required"))?;
        let mut header = [0; 20];
        file.read_exact(&mut header)
            .map_err(|_| Error::Invalid("queue_header_unavailable"))?;
        if !header.starts_with(b"SQLite format 3\0") {
            return Err(Error::Invalid("not a SQLite database"));
        }
        // SQLite can create WAL sidecars even with a read-only connection.
        // Header bytes 18 and 19 must indicate the rollback journal format.
        if header[18..] != [1, 1] {
            return Err(Error::Invalid("unsupported_queue_journal_mode"));
        }
        // macOS can report /var rather than /private/var in absolute paths.
        // The original descriptor already checked the direct parent and file.
        // Normalize only ancestors so SQLite still refuses a final symlink.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = parent
            .canonicalize()
            .map_err(|_| Error::Invalid("owner_private_queue_required"))?
            .join(
                path.file_name()
                    .ok_or(Error::Invalid("owner_private_queue_required"))?,
            );
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        validate_schema(&connection)?;
        validate_queue_journal(&connection)?;
        crate::mailbox_generation::scope_connection(&connection)?;
        Ok(connection)
    }
}
