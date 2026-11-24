//! A local notifier passes a destination and message identifier to an owner-selected command.
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
const MANAGED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
// The production provider's idempotency window is 24 hours. Stop retrying before it can expire.
const RETRY_WINDOW: i64 = 20 * 60 * 60;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    queue_device: u64,
    queue_inode: u64,
    mailboxes: BTreeMap<String, Cursor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayTarget {
    pub recipient: String,
    pub generation: String,
    pub after: i64,
}

pub type RelayConfig = BTreeMap<String, RelayTarget>;

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    after: i64,
    reservation: Option<Reservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    seq: i64,
    started_at: i64,
}

fn invalid_state() -> io::Error {
    io::Error::other("arrival notification state unavailable")
}

fn legacy_mailbox_error() -> io::Error {
    io::Error::other("mailbox generation requires managed relay --config")
}

fn private_descriptor(path: &Path) -> io::Result<File> {
    private_file::open_private_file(path)
        .map_err(|_| io::Error::other("arrival notifier requires an owner-private file"))
}

/// Serialize relay cursor and configuration changes with dispatch.
pub fn lock_state(path: &Path) -> io::Result<File> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .extension()
            .is_some_and(|extension| extension == "lock")
    {
        return Err(invalid_state());
    }
    let parent = path.parent().ok_or_else(invalid_state)?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.is_dir()
        // SAFETY: geteuid has no preconditions; comparing owner is a TOCTOU-checked metadata read.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderFailure {
    Start,
    Timeout,
    Exit,
    Wait,
}

impl ProviderFailure {
    fn diagnostic(&self) -> &'static str {
        match self {
            Self::Start => "provider_start_failed. Check the command and interpreter paths.",
            Self::Timeout => "provider_timed_out. Check provider connectivity and response time.",
            Self::Exit => {
                "provider_exited_unsuccessfully. Check the provider command and its authentication."
            }
            Self::Wait => "provider_wait_failed. Check that the command can run from launchd.",
        }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.diagnostic())
    }
}

impl std::error::Error for ProviderFailure {}

/// Report a fixed diagnostic in the managed service loop without exposing underlying errors.
pub fn safe_failure(error: &(dyn std::error::Error + 'static)) -> &'static str {
    error.downcast_ref::<ProviderFailure>().map_or(
        "relay_sweep_incomplete. Check the queue, registration and relay configuration.",
        ProviderFailure::diagnostic,
    )
}

fn send(
    runtime: &tokio::runtime::Runtime,
    command: &Path,
    recipient: &str,
    id: &str,
    managed: bool,
    timeout: Duration,
) -> Result<(), ProviderFailure> {
    runtime.block_on(async {
        let mut command = tokio::process::Command::new(command);
        command
            .args([recipient, id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if managed {
            command.env("PATH", MANAGED_PATH);
        }
        let mut child = command.spawn().map_err(|_| ProviderFailure::Start)?;
        let status = tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| ProviderFailure::Timeout)?
            .map_err(|_| ProviderFailure::Wait)?;
        status.success().then_some(()).ok_or(ProviderFailure::Exit)
    })
}

struct Destination<'a> {
    mailbox: &'a str,
    target: Option<&'a RelayTarget>,
}

fn sweep(
    conn: &Connection,
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    state: &mut State,
    destination: Destination<'_>,
    mut send: impl FnMut(&str, &str) -> Result<(), ProviderFailure>,
) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let Destination { mailbox, target } = destination;
    let recipient = target.map_or(mailbox, |item| item.recipient.as_str());
    let expected_storage = target.map_or(mailbox, |item| item.generation.as_str());
    let mut delivered = 0;
    let mut expired = 0;
    for _ in 0..LIMIT {
        let _dispatch = crate::mailbox_generation::lock_dispatch(queue)?;
        if crate::mailbox_generation::current_storage(conn, mailbox)?.as_deref()
            != Some(expected_storage)
        {
            if target.is_none() {
                return Err(legacy_mailbox_error().into());
            }
            break;
        }
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
                send(recipient, &id)?;
                delivered += 1;
            } else {
                expired += 1;
            }
        }
        let cursor = state.mailboxes.get_mut(mailbox).ok_or_else(invalid_state)?;
        cursor.after = seq;
        cursor.reservation = None;
        save(state_path, state)?;
    }
    Ok((delivered, expired))
}

fn open_run(
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    command: &Path,
) -> Result<(fs::Metadata, File, Connection), Box<dyn std::error::Error>> {
    if [queue, registration, command]
        .iter()
        .any(|path| !path.is_absolute())
    {
        return Err(io::Error::other("invalid arrival notifier configuration").into());
    }
    let queue_meta = private_descriptor(queue)?.metadata()?;
    if private_descriptor(command)?.metadata()?.mode() & 0o100 == 0 {
        return Err(
            io::Error::other("arrival notification command must be owner-executable").into(),
        );
    }
    let lock = lock_state(state_path)?;
    let conn = Connection::open_with_flags(queue, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    crate::validate_schema(&conn)?;
    crate::mailbox_generation::scope_connection(&conn)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok((queue_meta, lock, conn))
}

fn verify_state(state: &State, queue_meta: &fs::Metadata) -> io::Result<()> {
    if state.queue_device != queue_meta.dev()
        || state.queue_inode != queue_meta.ino()
        || state.mailboxes.values().any(|cursor| {
            cursor.after < 0
                || cursor
                    .reservation
                    .as_ref()
                    .is_some_and(|attempt| attempt.seq <= cursor.after || attempt.started_at <= 0)
        })
    {
        return Err(invalid_state());
    }
    Ok(())
}

fn dispatch<'a>(
    conn: &Connection,
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    command: &Path,
    state: &mut State,
    destinations: impl IntoIterator<Item = Destination<'a>>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut delivered = 0;
    let mut expired = 0;
    let mut failed = false;
    let mut provider_failure = None;
    for destination in destinations {
        let managed = destination.target.is_some();
        match sweep(
            conn,
            queue,
            registration,
            state_path,
            state,
            destination,
            |recipient, id| send(&runtime, command, recipient, id, managed, COMMAND_TIMEOUT),
        ) {
            Ok((sent, dropped)) => {
                delivered += sent;
                expired += dropped;
            }
            Err(error) => {
                failed = true;
                if managed && provider_failure.is_none() {
                    provider_failure = error.downcast_ref::<ProviderFailure>().copied();
                }
            }
        }
    }
    if let Some(error) = provider_failure {
        return Err(error.into());
    }
    if failed {
        return Err(io::Error::other("arrival notification sweep incomplete").into());
    }
    Ok(serde_json::json!({"state":"ready","delivered":delivered,"expired":expired}))
}

/// Validate the fixed mailbox list before starting a relay.
pub fn validate_static_mailboxes(mailboxes: &[String]) -> io::Result<()> {
    if mailboxes.is_empty()
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
        return Err(io::Error::other("invalid arrival notifier configuration"));
    }
    Ok(())
}

/// Run one bounded sweep. Installation initializes a high water mark before enabling notifications.
pub fn run(
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    command: &Path,
    mailboxes: &[String],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    validate_static_mailboxes(mailboxes)?;
    let (queue_meta, _lock, conn) = open_run(queue, registration, state_path, command)?;
    {
        let _dispatch = crate::mailbox_generation::lock_dispatch(queue)?;
        for mailbox in mailboxes {
            if crate::mailbox_generation::current_storage(&conn, mailbox)?.as_deref()
                != Some(mailbox.as_str())
            {
                return Err(legacy_mailbox_error().into());
            }
        }
    }
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
                            generation: None,
                        },
                    )
                })
                .collect(),
        };
        save(state_path, &state)?;
        return Ok(serde_json::json!({"state":"initialized","delivered":0}));
    };
    verify_state(&state, &queue_meta)?;
    if state.mailboxes.len() != mailboxes.len()
        || !mailboxes.iter().all(|mailbox| {
            state
                .mailboxes
                .get(mailbox)
                .is_some_and(|cursor| cursor.generation.is_none())
        })
    {
        return Err(invalid_state().into());
    }
    dispatch(
        &conn,
        queue,
        registration,
        state_path,
        command,
        &mut state,
        mailboxes.iter().map(|mailbox| Destination {
            mailbox,
            target: None,
        }),
    )
}

fn read_config(path: &Path) -> io::Result<RelayConfig> {
    let invalid = || io::Error::other("invalid arrival notifier recipient configuration");
    let mut bytes = Vec::new();
    private_descriptor(path)?
        .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(invalid());
    }
    let value = crate::mcp_request::strict_json(&bytes).map_err(|_| invalid())?;
    let targets: RelayConfig = serde_json::from_value(value).map_err(|_| invalid())?;
    if targets.len() > 16
        || targets.iter().any(|(mailbox, target)| {
            crate::deferred_mailbox::identifier(mailbox).is_err()
                || (target.generation != *mailbox
                    && !crate::mailbox_generation::valid_storage(&target.generation))
                || target.after < 0
                || target.recipient.trim().is_empty()
                || target.recipient.len() > 254
                || target.recipient.chars().any(char::is_control)
        })
    {
        return Err(invalid());
    }
    Ok(targets)
}

/// Reload the private recipient map and preserve the progress of unchanged generations.
pub fn run_config(
    queue: &Path,
    registration: &Path,
    state_path: &Path,
    command: &Path,
    config_path: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if !config_path.is_absolute() || config_path == state_path {
        return Err(io::Error::other("invalid arrival notifier recipient configuration").into());
    }
    let (queue_meta, _lock, conn) = open_run(queue, registration, state_path, command)?;
    let config = read_config(config_path)?;
    let existing = load(state_path)?;
    let fresh = existing.is_none();
    let mut state = existing.unwrap_or_else(|| State {
        queue_device: queue_meta.dev(),
        queue_inode: queue_meta.ino(),
        mailboxes: BTreeMap::new(),
    });
    verify_state(&state, &queue_meta)?;
    let mailboxes = config
        .iter()
        .map(|(mailbox, target)| {
            let current = state
                .mailboxes
                .get(mailbox)
                .filter(|cursor| {
                    cursor.generation.as_ref() == Some(&target.generation)
                        && cursor.after >= target.after
                })
                .cloned()
                .unwrap_or_else(|| Cursor {
                    after: target.after,
                    reservation: None,
                    generation: Some(target.generation.clone()),
                });
            (mailbox.clone(), current)
        })
        .collect();
    if fresh || mailboxes != state.mailboxes {
        state.mailboxes = mailboxes;
        save(state_path, &state)?;
    }
    dispatch(
        &conn,
        queue,
        registration,
        state_path,
        command,
        &mut state,
        config.iter().map(|(mailbox, target)| Destination {
            mailbox,
            target: Some(target),
        }),
    )
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn provider_diagnostics_distinguish_start_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let path = dir.path().join("fixture-provider");
        let invoke = || {
            send(
                &runtime,
                &path,
                "fixture@example.invalid",
                "fixture-id",
                true,
                Duration::from_millis(25),
            )
        };
        assert_eq!(invoke(), Err(ProviderFailure::Start));
        fs::write(&path, b"#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(invoke(), Err(ProviderFailure::Timeout));
        assert!(safe_failure(&ProviderFailure::Wait).starts_with("provider_wait_failed"));
        assert_eq!(
            safe_failure(&io::Error::other("synthetic private provider diagnostic")),
            "relay_sweep_incomplete. Check the queue, registration and relay configuration."
        );
    }
}
