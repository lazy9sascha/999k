//! A local notifier sends only mailbox and message identifiers to an owner-selected command.
use crate::{mailbox_engine::PLAIN_MESSAGE, private_file, registration};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const LIMIT: u32 = 50;
// The production provider's idempotency window is 24 hours. Stop retrying before it can expire.
const RETRY_WINDOW: i64 = 20 * 60 * 60;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    queue_device: u64,
    queue_inode: u64,
    mailboxes: BTreeMap<String, Cursor>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    after: i64,
    reservation: Option<Reservation>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    seq: i64,
    started_at: i64,
}

fn invalid_state() -> io::Error {
    io::Error::other("arrival notification state unavailable")
}

fn private_descriptor(path: &Path) -> io::Result<File> {
    private_file::open_private_file(path)
        .map_err(|_| io::Error::other("arrival notifier requires an owner-private file"))
}

fn acquire_lock(path: &Path) -> io::Result<File> {
    let parent = path.parent().ok_or_else(invalid_state)?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.is_dir()
        || parent_meta.uid() != unsafe { libc::geteuid() }
        || parent_meta.mode() & 0o077 != 0
    {
        return Err(invalid_state());
    }
    let path = path.with_extension("lock");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let file = private_descriptor(&path)?;
    // SAFETY: the descriptor belongs to file and its lifetime covers the operation.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("arrival notifier already running"));
    }
    Ok(file)
}

fn save(path: &Path, state: &State) -> io::Result<()> {
    private_file::replace_private(path, &serde_json::to_vec(state)?)
}

fn load(path: &Path) -> io::Result<Option<State>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let mut bytes = Vec::new();
            private_descriptor(path)?
                .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
                return Err(invalid_state());
            }
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| invalid_state())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn now() -> io::Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    i64::try_from(seconds.as_secs()).map_err(io::Error::other)
}

type Candidate = (i64, Option<String>, Option<String>);

fn next(
    conn: &Connection,
    mailbox: &str,
    after: i64,
    time: i64,
) -> rusqlite::Result<Option<Candidate>> {
    let sql = format!(
        "SELECT m.seq,m.id,m.sender FROM messages m
         WHERE m.recipient=?1 AND m.seq>?2 AND m.attempts=0 AND m.receipt IS NULL
         AND COALESCE(m.lease_until_ms,0)<=?3 AND m.dead_lettered_at_ms IS NULL
         AND m.acknowledged_at_ms IS NULL AND {PLAIN_MESSAGE}
         ORDER BY m.seq LIMIT 1"
    );
    conn.query_row(
        &sql,
        params![mailbox, after, time.saturating_mul(1_000)],
        |row| Ok((row.get(0)?, row.get(1).ok(), row.get(2).ok())),
    )
    .optional()
}

fn allowed(path: &Path, mailbox: &str, sender: &str) -> io::Result<bool> {
    if mailbox == sender || crate::deferred_mailbox::identifier(sender).is_err() {
        return Ok(false);
    }
    match registration::validate_registration(path, sender, mailbox) {
        Ok(()) => Ok(true),
        Err((403, _)) => Ok(false),
        Err(_) => Err(io::Error::other(
            "arrival notifier registration unavailable",
        )),
    }
}

fn send(runtime: &tokio::runtime::Runtime, command: &Path, mailbox: &str, id: &str) -> bool {
    runtime.block_on(async {
        let mut command = tokio::process::Command::new(command);
        command
            .args([mailbox, id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        matches!(
            tokio::time::timeout(Duration::from_secs(20), command.status()).await,
            Ok(Ok(status)) if status.success()
        )
    })
}

fn sweep(
    conn: &Connection,
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    state: &mut State,
    mailbox: &str,
    mut send: impl FnMut(&str, &str) -> bool,
) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let mut delivered = 0;
    let mut expired = 0;
    for _ in 0..LIMIT {
        let _dispatch = crate::mailbox_generation::lock_dispatch(queue)?;
        let time = now()?;
        let cursor = &state.mailboxes[mailbox];
        let Some((seq, id, sender)) = next(conn, mailbox, cursor.after, time)? else {
            break;
        };
        let id = id.filter(|id| crate::deferred_mailbox::identifier(id).is_ok());
        if let (Some(id), Some(sender)) = (id, sender)
            && allowed(registration, mailbox, &sender)?
        {
            let started = cursor
                .reservation
                .as_ref()
                .filter(|attempt| attempt.seq == seq)
                .map_or(time, |attempt| attempt.started_at);
            if (0..RETRY_WINDOW).contains(&(time.saturating_sub(started))) {
                state
                    .mailboxes
                    .get_mut(mailbox)
                    .ok_or_else(invalid_state)?
                    .reservation = Some(Reservation {
                    seq,
                    started_at: started,
                });
                save(state_path, state)?;
                if !send(mailbox, &id) {
                    return Err(io::Error::other("arrival notification command failed").into());
                }
                delivered += 1;
            } else {
                expired += 1;
            }
        }
        state.mailboxes.insert(
            mailbox.into(),
            Cursor {
                after: seq,
                reservation: None,
            },
        );
        save(state_path, state)?;
    }
    Ok((delivered, expired))
}

/// Run one bounded sweep. Installation initializes a high water mark before enabling notifications.
pub fn run(
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    command: &Path,
    mailboxes: &[String],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if [queue, registration, state_path, command]
        .iter()
        .any(|path| !path.is_absolute())
        || state_path
            .extension()
            .is_some_and(|extension| extension == "lock")
        || mailboxes.is_empty()
        || mailboxes.len() > 16
        || mailboxes
            .iter()
            .any(|name| crate::deferred_mailbox::identifier(name).is_err())
        || mailboxes
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != mailboxes.len()
    {
        return Err(io::Error::other("invalid arrival notifier configuration").into());
    }
    let queue_meta = private_descriptor(queue)?.metadata()?;
    if private_descriptor(command)?.metadata()?.mode() & 0o100 == 0 {
        return Err(
            io::Error::other("arrival notification command must be owner-executable").into(),
        );
    }
    let _lock = acquire_lock(state_path)?;
    let conn = Connection::open_with_flags(queue, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    crate::validate_schema(&conn)?;
    crate::mailbox_generation::scope_connection(&conn)?;
    conn.pragma_update(None, "query_only", true)?;
    let Some(mut state) = load(state_path)? else {
        for mailbox in mailboxes {
            registration::active_peers(registration, mailbox).map_err(|_| {
                io::Error::other("arrival notifier mailbox registration unavailable")
            })?;
        }
        let high_water = conn.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='messages'),0)",
            [],
            |row| row.get(0),
        )?;
        let state = State {
            queue_device: queue_meta.dev(),
            queue_inode: queue_meta.ino(),
            mailboxes: mailboxes
                .iter()
                .map(|mailbox| {
                    (
                        mailbox.clone(),
                        Cursor {
                            after: high_water,
                            reservation: None,
                        },
                    )
                })
                .collect(),
        };
        save(state_path, &state)?;
        return Ok(serde_json::json!({"state":"initialized","delivered":0}));
    };
    if state.queue_device != queue_meta.dev()
        || state.queue_inode != queue_meta.ino()
        || state.mailboxes.len() != mailboxes.len()
        || !mailboxes
            .iter()
            .all(|name| state.mailboxes.contains_key(name))
        || state.mailboxes.values().any(|cursor| {
            cursor.after < 0
                || cursor
                    .reservation
                    .as_ref()
                    .is_some_and(|attempt| attempt.seq <= cursor.after || attempt.started_at <= 0)
        })
    {
        return Err(invalid_state().into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut delivered = 0;
    let mut expired = 0;
    let mut failed = false;
    for mailbox in mailboxes {
        match sweep(
            &conn,
            queue,
            registration,
            state_path,
            &mut state,
            mailbox,
            |mailbox, id| send(&runtime, command, mailbox, id),
        ) {
            Ok((sent, dropped)) => {
                delivered += sent;
                expired += dropped;
            }
            Err(_) => failed = true,
        }
    }
    if failed {
        return Err(io::Error::other("arrival notification sweep incomplete").into());
    }
    Ok(serde_json::json!({"state":"ready","delivered":delivered,"expired":expired}))
}
