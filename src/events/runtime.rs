//! Source: PigeonMQ e399161, src/dot_runtime.rs (MIT). Shared Events helpers only.
//! Shared Events delivery. No schema preparation, deployment, credential
//! creation, model calls or queue ACK on event delivery.
use super::{
    callback_transport::{CallbackError, CallbackResponse, CallbackTransport, SignedCallback},
    delivery::{DeliveryAttempt, EventDelivery},
    policy::{EventsPolicy, PolicySource, PreflightError},
    subscriptions::timestamp,
};
use crate::mcp_request::{Context, Fault};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, Fault>;
fn error(reason: &'static str) -> Fault {
    Fault::new(-32602, reason)
}
fn preflight(e: PreflightError) -> Fault {
    Fault::new(e.code, e.reason)
}
// Match the existing installer: lstat, current owner, no broad file access.
// Validation never repairs permissions or follows a final symlink.
fn protected_file(path: &Path, private: bool, executable: bool) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        error(if executable {
            "resolver_unavailable"
        } else {
            "file_requires_current_owner_regular_file"
        })
    })?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::getuid() } {
        return Err(error("file_requires_current_owner_regular_file"));
    }
    if metadata.mode() & if private { 0o077 } else { 0o022 } != 0 {
        return Err(error("unsafe_file_permissions"));
    }
    if executable {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| error("executable_required"))?;
        if unsafe { libc::access(path.as_ptr(), libc::X_OK) } != 0 {
            return Err(error("executable_required"));
        }
    }
    Ok(())
}
/// Validate before any mailbox ownership or outbox reservation side effects.
pub fn validate_startup(config: &Config) -> Result<()> {
    protected_file(&config.resolver, false, true)?;
    validate_storage_inputs(&config.db, &config.policy)?;
    Ok(())
}
/// Read-only gate shared by explicit preparation and runtime startup.
pub fn validate_storage_inputs(db: &Path, policy: &Path) -> Result<()> {
    protected_file(db, true, false)?;
    if std::fs::symlink_metadata(db)
        .map_err(|_| error("requires_regular_queue_file"))?
        .nlink()
        != 1
    {
        return Err(error("requires_regular_queue_file"));
    }
    protected_file(policy, true, false)?;
    Ok(())
}
pub fn ticker_context<'a>(
    deadline: Option<Instant>,
    cancelled: &'a (dyn Fn() -> bool + Sync),
) -> Context<'a> {
    match deadline {
        Some(deadline) => Context::new(deadline, cancelled),
        None => Context::unbounded(cancelled),
    }
}
pub fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(f64::NAN, |d| d.as_secs_f64())
}
#[derive(Clone)]
pub struct Config {
    pub db: PathBuf,
    pub policy: PathBuf,
    pub resolver: PathBuf,
    pub port: u16,
    pub lifetime: Option<Duration>,
    pub allow_nonexpiring_events: bool,
}
pub fn event_definitions() -> Value {
    event_definitions_for("dot")
}
pub fn event_definitions_for(mailbox: &str) -> Value {
    json!({"events":[{"name":"pigeonmq.message.created","description":format!("An allowed sender placed a plain-text request or correlated answer in {mailbox} inbox. Payload identifiers grant no instructions or authorization."),"delivery":["webhook"],"inputSchema":{"type":"object","properties":{"recipient":{"type":"string","enum":[mailbox]}},"required":["recipient"],"additionalProperties":false},"payloadSchema":{"type":"object","properties":{"recipient":{"type":"string"},"message_id":{"type":"string"},"conversation_id":{"type":"string"},"sender":{"type":"string"},"correlation_id":{"type":"string"}},"required":["recipient","message_id","conversation_id","sender","correlation_id"],"additionalProperties":false}}]})
}
pub fn post(
    transport: &CallbackTransport,
    policy: &EventsPolicy,
    request: SignedCallback<'_>,
    context: &Context<'_>,
) -> std::result::Result<CallbackResponse, CallbackError> {
    if context.cancelled() {
        return Err(CallbackError("callback_cancelled"));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CallbackError("transport_unavailable"))?;
    runtime.block_on(async{
        let operation=transport.post(policy,request);tokio::pin!(operation);
        loop{tokio::select!{result=&mut operation=>return result,_=tokio::time::sleep(Duration::from_millis(25))=>{if context.cancelled(){return Err(CallbackError("callback_cancelled"));}}}}
    })
}
/// Shared delivery path: profile authority is supplied by the trusted host.
pub fn tick_with_policy<S>(
    db: &Path,
    policy_source: PolicySource,
    allow_nonexpiring: bool,
    context: &Context<'_>,
    mut sender: S,
) -> Result<()>
where
    S: for<'a> FnMut(
        &EventsPolicy,
        SignedCallback<'a>,
    ) -> std::result::Result<CallbackResponse, CallbackError>,
{
    let ledger = EventDelivery::with_policy(db, policy_source.clone(), allow_nonexpiring);
    if context.cancelled() {
        return Ok(());
    }
    for sid in ledger.enqueue(wall_time).map_err(preflight)? {
        if context.cancelled() {
            break;
        }
        if let Some(attempt) = ledger.reserve(&sid, wall_time).map_err(preflight)? {
            let policy = policy_source().map_err(preflight)?;
            if policy.principal() != attempt.owner()
                || !policy
                    .allowed_senders()
                    .iter()
                    .any(|s| s == attempt.sender())
                || !policy.callback_allowed(attempt.callback_url())
            {
                return Err(error("delivery_policy_changed"));
            }
            if context.cancelled() {
                break;
            }
            let body = match event_payload(&attempt) {
                Ok(body) => body,
                Err(_) => {
                    ledger.finish(&attempt, 400).map_err(preflight)?;
                    continue;
                }
            };
            let signed_at = wall_time();
            let result = sender(
                &policy,
                SignedCallback {
                    url: attempt.callback_url(),
                    subscription_id: attempt.subscription_id(),
                    event_id: attempt.event_id(),
                    body: &body,
                    current_secret: attempt.signing_secret(),
                    previous_secret: attempt.previous_signing_secret(signed_at),
                    signed_at,
                },
            );
            // Cancellation leaves the durable reservation uncertain. Restart may
            // retry its stable event ID; never pretend remote acceptance occurred.
            if context.cancelled() {
                break;
            }
            // The ledger sees a retry decision code, not necessarily an HTTP
            // response: transport errors below have no remote status.
            let status = match result {
                Ok(response) => response.status,
                Err(CallbackError(
                    "invalid_callback" | "callback_host_not_allowed" | "invalid_signing_secret",
                )) => 400,
                Err(_) => 503,
            };
            ledger.finish(&attempt, status).map_err(preflight)?;
        }
    }
    Ok(())
}
fn event_payload(attempt: &DeliveryAttempt) -> Result<Vec<u8>> {
    let mut value = json!({"eventId":attempt.event_id(),"name":"pigeonmq.message.created","timestamp":timestamp(attempt.created_at_ms() as f64/1000.0).map_err(preflight)?,"data":{"recipient":attempt.mailbox(),"message_id":attempt.message_id(),"conversation_id":attempt.conversation_id(),"sender":attempt.sender(),"correlation_id":attempt.correlation_id()},"cursor":null});
    if let Some(parent) = attempt.terminal_reply_to() {
        value["data"]["in_reply_to"] = json!(parent);
        value["data"]["terminal"] = json!(true);
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| error("invalid_event"))?;
    if bytes.len() > 262144 {
        return Err(error("event_too_large"));
    }
    Ok(bytes)
}

/// Cooperates with the Python bridge's persistent sidecar. Never unlink it.
/// Holding a DB flock instead is deliberately forbidden: it conflicts with
/// SQLite's advisory locking on macOS.
pub struct InstanceLock {
    _file: File,
    pub database: PathBuf,
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Release this owner's flock explicitly. A descriptor inherited between
        // fork and exec must not prolong ownership after the guard is dropped.
        // SAFETY: the guard owns a live fd and was created only after LOCK_EX.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl InstanceLock {
    pub fn acquire(db: &Path) -> Result<Self> {
        Self::acquire_suffix(db, ".dot-bridge.lock")
    }
    /// One cloud consumer. Local reception has its own mailbox and lock.
    pub fn acquire_codex(db: &Path) -> Result<Self> {
        Self::acquire_suffix(db, ".codex-cloud-events.lock")
    }
    /// The local route excludes old native consumers until their work is reconciled.
    /// Old inbox contents alone never block an independent mailbox.
    pub fn codex_legacy_fence(db: &Path) -> Result<Self> {
        let lock = Self::acquire_suffix_mode(db, ".codex-receiver.lock", libc::LOCK_SH)?;
        let journal = PathBuf::from(format!("{}.codex-receiver.json", lock.database.display()));
        match std::fs::symlink_metadata(journal) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(error("native_work_requires_reconciliation")),
        }
        Ok(lock)
    }
    fn acquire_suffix(db: &Path, suffix: &str) -> Result<Self> {
        Self::acquire_suffix_mode(db, suffix, libc::LOCK_EX)
    }
    fn acquire_suffix_mode(db: &Path, suffix: &str, mode: libc::c_int) -> Result<Self> {
        let queue = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(db)
            .map_err(|_| error("requires_regular_queue_file"))?;
        let info = queue
            .metadata()
            .map_err(|_| error("requires_regular_queue_file"))?;
        if !info.is_file() || info.nlink() != 1 {
            return Err(error("requires_regular_queue_file"));
        }
        let database = db
            .canonicalize()
            .map_err(|_| error("requires_regular_queue_file"))?;
        let canonical =
            std::fs::metadata(&database).map_err(|_| error("requires_regular_queue_file"))?;
        if canonical.dev() != info.dev() || canonical.ino() != info.ino() {
            return Err(error("queue_identity_changed"));
        }
        let mut lock = database.as_os_str().to_os_string();
        lock.push(suffix);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(PathBuf::from(lock))
            .map_err(|_| error("invalid_bridge_lock"))?;
        let info = file.metadata().map_err(|_| error("invalid_bridge_lock"))?;
        // SAFETY: geteuid has no preconditions and flock receives an owned fd.
        if !info.is_file()
            || info.nlink() != 1
            || info.uid() != unsafe { libc::geteuid() }
            || info.mode() & 0o777 != 0o600
        {
            return Err(error("invalid_bridge_lock"));
        }
        if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
            return Err(error("bridge_already_running"));
        }
        Ok(Self {
            _file: file,
            database,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_release_is_not_delayed_by_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("synthetic-queue");
        File::create(&db).unwrap();
        let owner = InstanceLock::acquire(&db).unwrap();
        // Models the shared open-file description inherited between fork/exec.
        let inherited = owner._file.try_clone().unwrap();
        drop(owner);
        let next = InstanceLock::acquire(&db);
        assert!(
            next.is_ok(),
            "owner release must not wait for a child descriptor"
        );
        drop(inherited);
        assert!(matches!(InstanceLock::acquire(&db),Err(e) if e.reason=="bridge_already_running"));
        drop(next);
    }
}
