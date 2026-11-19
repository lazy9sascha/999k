//! A mailbox's public name remains stable across replacement of its storage identity.
//! The owner-controlled lifecycle owns credentials and must retire the former binding.
use crate::{Error, Result, validate_queue_journal, validate_schema};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

const RESERVED: &str = "pigeonmq-generation:";
pub(crate) const SCHEMA: &str = "
    CREATE TABLE pigeonmq_mailbox_generations (
        storage TEXT PRIMARY KEY,
        mailbox TEXT NOT NULL,
        active INTEGER NOT NULL CHECK(active IN (0, 1))
    ) STRICT;
    CREATE UNIQUE INDEX pigeonmq_one_active_generation
        ON pigeonmq_mailbox_generations(mailbox) WHERE active = 1;
    CREATE INDEX pigeonmq_past_generations ON pigeonmq_mailbox_generations(mailbox);";

fn prepared(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema
          WHERE type='table' AND name='pigeonmq_mailbox_generations')",
        [],
        |row| row.get(0),
    )
}

/// Install the same logical projection on every application message connection.
/// A database created before the generation catalog uses its original table.
pub fn scope_connection(connection: &Connection) -> rusqlite::Result<()> {
    if prepared(connection)? {
        connection.execute_batch(
            "CREATE TEMP VIEW IF NOT EXISTS messages AS
             SELECT m.seq, m.id, m.conversation_id,
                    COALESCE(s.mailbox, m.sender) AS sender,
                    COALESCE(r.mailbox, m.recipient) AS recipient,
                    m.body, m.correlation_id, m.in_reply_to, m.idempotency_key,
                    m.created_at_ms, m.receipt, m.lease_until_ms, m.attempts,
                    m.acknowledged_at_ms, m.dead_lettered_at_ms
             FROM main.messages m
             LEFT JOIN main.pigeonmq_mailbox_generations s ON s.storage = m.sender
             LEFT JOIN main.pigeonmq_mailbox_generations r ON r.storage = m.recipient
             WHERE COALESCE(s.active, 1) = 1 AND COALESCE(r.active, 1) = 1;",
        )?;
    }
    Ok(())
}

/// Return None for a deliberately retired name or a raw storage identity.
pub fn current_storage(connection: &Connection, name: &str) -> rusqlite::Result<Option<String>> {
    if !prepared(connection)? {
        return Ok(Some(name.into()));
    }
    if name.starts_with(RESERVED) {
        return Ok(None);
    }
    let state: Option<(String, bool)> = connection
        .query_row(
            "SELECT storage,active FROM main.pigeonmq_mailbox_generations
             WHERE mailbox = ?1 ORDER BY active DESC LIMIT 1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match state {
        Some((storage, true)) => Some(storage),
        Some((_, false)) => None,
        None => Some(name.into()),
    })
}

/// Report whether the existing queue can safely participate in generation changes.
pub fn supports(path: &Path) -> Result<bool> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    validate_schema(&connection)?;
    Ok(prepared(&connection)?)
}

/// Inspect the active storage identity using an existing queue only.
pub fn active_storage(path: &Path, mailbox: &str) -> Result<Option<String>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    validate_schema(&connection)?;
    Ok(current_storage(&connection, mailbox)?)
}

/// Allocate an opaque identity for a staged replacement. This does not activate it.
pub fn new_storage() -> String {
    format!("{RESERVED}{}", uuid::Uuid::new_v4().simple())
}

pub fn valid_storage(storage: &str) -> bool {
    storage.strip_prefix(RESERVED).is_some_and(|suffix| {
        uuid::Uuid::parse_str(suffix).is_ok_and(|uuid| uuid.simple().to_string() == suffix)
    })
}

pub(crate) fn check_link(connection: &Connection, parent: &str) -> rusqlite::Result<()> {
    let retired: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.messages WHERE id=?1)
          AND NOT EXISTS(SELECT 1 FROM messages WHERE id=?1)",
        [parent],
        |row| row.get(0),
    )?;
    if retired {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

/// The file is a cooperative fence for callback and relay dispatch. It contains no data.
pub fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("generation.lock")
}

fn guard(path: &Path, mode: libc::c_int) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path(path))?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(io::Error::other("mailbox generation lock is not private"));
    }
    // SAFETY: the guard owns a live descriptor and returns it to hold the lock.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Hold this guard through the eligibility check and the external dispatch.
pub fn lock_dispatch(path: &Path) -> io::Result<Option<File>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(io::Error::other)?;
    if prepared(&connection).map_err(io::Error::other)? {
        guard(path, libc::LOCK_SH).map(Some)
    } else {
        Ok(None)
    }
}

fn owner_transaction(
    path: &Path,
    mailbox: &str,
    action: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    crate::deferred_mailbox::identifier(mailbox).map_err(|_| Error::Invalid("invalid_mailbox"))?;
    if mailbox.starts_with(RESERVED) {
        return Err(Error::Invalid("invalid_mailbox"));
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    validate_schema(&connection)?;
    validate_queue_journal(&connection)?;
    if !prepared(&connection)? {
        // A legacy process may have opened the original table directly.
        return Err(Error::Invalid("mailbox_generation_catalog_missing"));
    }
    let _guard = guard(path, libc::LOCK_EX)
        .map_err(|_| Error::Invalid("mailbox_generation_dispatch_in_progress"))?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    action(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Retire the current storage identity. Repeating this operation is a no-op.
/// This is a storage operation. The caller must separately revoke the old credential.
pub fn retire(path: &Path, mailbox: &str) -> Result<()> {
    owner_transaction(path, mailbox, |tx| {
        tx.execute(
            "INSERT INTO main.pigeonmq_mailbox_generations(storage,mailbox,active)
             SELECT ?1,?1,0
             WHERE NOT EXISTS(SELECT 1 FROM main.pigeonmq_mailbox_generations WHERE mailbox=?1)",
            [mailbox],
        )?;
        tx.execute(
            "UPDATE main.pigeonmq_mailbox_generations SET active=0 WHERE mailbox=?1 AND active=1",
            [mailbox],
        )?;
        Ok(())
    })
}

/// Activate a new storage identity for a retired public name. Only a lifecycle
/// that creates a distinct authenticated binding may expose this generation.
pub fn reactivate(path: &Path, mailbox: &str) -> Result<()> {
    activate(path, mailbox, None)
}

/// Commit the exact generation recorded alongside a newly staged credential.
pub fn activate_staged(path: &Path, mailbox: &str, storage: &str) -> Result<()> {
    if !valid_storage(storage) {
        return Err(Error::Invalid("invalid_mailbox_generation"));
    }
    activate(path, mailbox, Some(storage))
}

fn activate(path: &Path, mailbox: &str, staged: Option<&str>) -> Result<()> {
    owner_transaction(path, mailbox, |tx| {
        match current_storage(tx, mailbox)? {
            Some(storage) if storage == mailbox => {
                return Err(Error::Invalid("mailbox_not_retired"));
            }
            Some(storage) if staged.is_none_or(|staged| staged == storage) => return Ok(()),
            Some(_) => return Err(Error::Invalid("mailbox_generation_conflict")),
            None => {}
        }
        let storage = staged.map_or_else(new_storage, str::to_owned);
        tx.execute(
            "INSERT INTO main.pigeonmq_mailbox_generations(storage,mailbox,active) VALUES(?1,?2,1)",
            params![storage, mailbox],
        )?;
        Ok(())
    })
}
