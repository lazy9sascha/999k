//! Best-effort Grok Bot wake notifier. See docs/quickstart.md, "Optional: Grok Bot wake".
//! Read-only queue observation. It never claims, leases, ACKs, or writes a message.
//! Each wake body holds only message_id, conversation_id, and mailbox. Credentials never enter
//! logs or errors. A missing wake loses nothing.
use crate::mailbox_engine::PLAIN_MESSAGE;
use reqwest::{Client, header::HeaderValue};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const MAILBOX: &str = "grok-bot";
pub const URL_FILE: &str = "grok-webhook-url";
pub const AUTH_FILE: &str = "grok-webhook-auth";
pub const KEYCHAIN_MARKER: &str = "grok-webhook-keychain";
pub const MAX_KEYCHAIN_PAIR: usize = 8192;
const MAX_WAKE_FILE_BYTES: u64 = 4096;
pub const STATE_FILE: &str = "grok-wake-state.json";
const TRIES: usize = 3;
const BACKOFF: [u64; 2] = [2, 5];
const MAX_ROUNDS: u64 = 3;
const RATE_COOLDOWN: Duration = Duration::from_secs(300);
const HOURLY_CAP: usize = 30;
const POLL: Duration = Duration::from_secs(5);
const STATE_CAP: usize = 2000;
/// Bound the scan to never-claimed rows. The mailbox revalidates candidates before delivery.
const PENDING: &str =
    "SELECT m.seq,m.id FROM messages m WHERE m.recipient=?1 AND m.seq>?3
 AND m.acknowledged_at_ms IS NULL AND m.dead_lettered_at_ms IS NULL AND m.attempts=0 AND m.receipt IS NULL
 AND COALESCE(m.lease_until_ms,0)<=?2
 AND {PLAIN_MESSAGE}
 AND NOT EXISTS(SELECT 1 FROM grok_mailbox_replies g WHERE g.message_id=m.id)
 ORDER BY m.seq LIMIT 50";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WakeError(pub &'static str);
type Result<T> = std::result::Result<T, WakeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    Retry,
    AuthBlock,
    RateLimited,
    Rejected,
}
pub fn classify(status: u16) -> Outcome {
    match status {
        200..=299 => Outcome::Delivered,
        401 | 403 => Outcome::AuthBlock,
        429 => Outcome::RateLimited,
        408 | 500..=599 => Outcome::Retry,
        _ => Outcome::Rejected,
    }
}

pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

pub fn wake_body(message_id: &str, conversation_id: &str) -> Vec<u8> {
    json!({"message_id": message_id, "conversation_id": conversation_id, "mailbox": MAILBOX})
        .to_string()
        .into_bytes()
}

/// Accepts "Authorization: <value>" or a bare "<value>". Returns the header value.
pub fn parse_auth(text: &str) -> Result<HeaderValue> {
    let line = text.trim();
    let value = match line.get(..14) {
        Some(p) if p.eq_ignore_ascii_case("authorization:") => line[14..].trim(),
        _ => line,
    };
    if value.is_empty() || value.contains(['\r', '\n']) {
        return Err(WakeError("invalid_header"));
    }
    let mut v = HeaderValue::from_str(value).map_err(|_| WakeError("invalid_header"))?;
    v.set_sensitive(true);
    Ok(v)
}

pub fn parse_url(text: &str) -> Result<url::Url> {
    let raw = text.trim();
    let u = url::Url::parse(raw).map_err(|_| WakeError("invalid_url"))?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
    {
        return Err(WakeError("invalid_url"));
    }
    Ok(u)
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeychainPair {
    url: String,
    authorization: String,
}

pub fn encode_keychain_pair(url: &str, authorization: &str) -> Result<Vec<u8>> {
    parse_url(url)?;
    parse_auth(authorization)?;
    let bytes = serde_json::to_vec(&KeychainPair {
        url: url.into(),
        authorization: authorization.into(),
    })
    .map_err(|_| WakeError("invalid_keychain_pair"))?;
    if bytes.len() > MAX_KEYCHAIN_PAIR {
        return Err(WakeError("invalid_keychain_pair"));
    }
    Ok(bytes)
}

fn decode_keychain_pair(bytes: &[u8]) -> Result<(url::Url, HeaderValue)> {
    if bytes.len() > MAX_KEYCHAIN_PAIR {
        return Err(WakeError("invalid_keychain_pair"));
    }
    let pair: KeychainPair =
        serde_json::from_slice(bytes).map_err(|_| WakeError("invalid_keychain_pair"))?;
    Ok((parse_url(&pair.url)?, parse_auth(&pair.authorization)?))
}

fn read_private(p: &Path) -> Result<String> {
    let meta = fs::metadata(p).map_err(|_| WakeError("credential_unavailable"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(WakeError("credential_file_mode"));
    }
    fs::read_to_string(p).map_err(|_| WakeError("credential_unavailable"))
}

/// Read one small, owner-private Grok credential from its checked descriptor.
pub fn read_wake_file(path: &Path) -> io::Result<String> {
    let file = crate::private_file::open_private_file(path)
        .map_err(|_| io::Error::other("Grok webhook file is not owner-private"))?;
    let meta = file.metadata()?;
    if meta.permissions().mode() & 0o7777 != 0o600 || meta.len() > MAX_WAKE_FILE_BYTES {
        return Err(io::Error::other(
            "Grok webhook file must be a small mode 600 file",
        ));
    }
    let mut text = String::new();
    file.take(MAX_WAKE_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| io::Error::other("Grok webhook file is not valid text"))?;
    if text.len() as u64 > MAX_WAKE_FILE_BYTES {
        return Err(io::Error::other("Grok webhook file is too large"));
    }
    Ok(text)
}

fn keychain_marker_selected(marker: &Path) -> bool {
    !matches!(fs::symlink_metadata(marker), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// Validate an explicit source without touching conventional credential files or Keychain.
pub fn validate_files_dir(home: &Path, dir: &Path) -> io::Result<()> {
    if keychain_marker_selected(&home.join(KEYCHAIN_MARKER)) {
        return Err(io::Error::other(
            "Grok wake_files_dir conflicts with the default Keychain marker",
        ));
    }
    for name in [URL_FILE, AUTH_FILE] {
        read_wake_file(&dir.join(name)).map_err(|_| {
            io::Error::other("Grok wake_files_dir requires two small owner-private mode 600 files")
        })?;
    }
    Ok(())
}

fn fingerprint(c: &WakeConfig) -> Option<Vec<(u128, u64)>> {
    if c.keychain.is_some() {
        return None;
    }
    [&c.url_file, &c.auth_file]
        .iter()
        .map(|p| {
            let m = fs::metadata(p).ok()?;
            let t = m
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_nanos();
            Some((t, m.len()))
        })
        .collect()
}

pub fn pending(db: &Path, now_ms: i64) -> Result<Vec<String>> {
    Ok(pending_page(db, now_ms, 0)?.0)
}
// Ephemeral scan progress only; not an ACK, client cursor or durable queue state.
fn pending_page(db: &Path, now_ms: i64, after: i64) -> Result<(Vec<String>, i64)> {
    let storage = |_| WakeError("storage_unavailable");
    let c = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(storage)?;
    c.busy_timeout(Duration::from_secs(2)).map_err(storage)?;
    c.pragma_update(None, "query_only", true).map_err(storage)?;
    crate::validate_schema(&c).map_err(|err| match err {
        crate::Error::Database(error) => storage(error),
        _ => WakeError("requires_current_schema_queue"),
    })?;
    let mut s = c
        .prepare(&PENDING.replace("{PLAIN_MESSAGE}", PLAIN_MESSAGE))
        .map_err(storage)?;
    let rows = s
        .query_map(rusqlite::params![MAILBOX, now_ms, after], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1).ok()))
        })
        .map_err(storage)?;
    let mut ids = Vec::new();
    let mut count = 0;
    let mut next = after;
    for row in rows {
        let (seq, id) = row.map_err(storage)?;
        count += 1;
        next = seq;
        if let Some(id) = id.filter(|id| valid_id(id)) {
            ids.push(id);
        }
    }
    Ok((ids, if count == 50 { next } else { 0 }))
}

/// State: {"notified":{id:ts},"suppressed":{id:ts},"failed":{id:{"rounds":n,"last":code}}}.
/// Same format as tools/grok_wake_notifier.py so a switch does not re-notify.
pub struct WakeState {
    path: PathBuf,
    doc: Map<String, Value>,
}
impl WakeState {
    pub fn load(path: PathBuf) -> Self {
        let mut doc = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for k in ["notified", "suppressed", "failed"] {
            if !doc.get(k).is_some_and(Value::is_object) {
                doc.insert(k.into(), json!({}));
            }
        }
        Self { path, doc }
    }
    fn map(&self, k: &str) -> &Map<String, Value> {
        self.doc[k].as_object().expect("object")
    }
    fn map_mut(&mut self, k: &str) -> &mut Map<String, Value> {
        self.doc
            .get_mut(k)
            .and_then(Value::as_object_mut)
            .expect("object")
    }
    pub fn due(&self, id: &str) -> bool {
        !self.map("notified").contains_key(id)
            && !self.map("suppressed").contains_key(id)
            && self
                .map("failed")
                .get(id)
                .and_then(|f| f["rounds"].as_u64())
                .unwrap_or(0)
                < MAX_ROUNDS
    }
    pub fn delivered(&mut self, id: &str) {
        self.map_mut("notified").insert(id.into(), json!(unix_s()));
        self.map_mut("failed").remove(id);
    }
    pub fn failed(&mut self, id: &str, last: Option<u16>) -> u64 {
        let f = self
            .map_mut("failed")
            .entry(id.to_string())
            .or_insert(json!({"rounds":0}));
        let rounds = f["rounds"].as_u64().unwrap_or(0) + 1;
        *f = json!({"rounds": rounds, "last": last});
        rounds
    }
    pub fn save(&mut self) -> Result<()> {
        for k in ["notified", "suppressed", "failed"] {
            let m = self.map_mut(k);
            while m.len() > STATE_CAP {
                let oldest = m
                    .iter()
                    .min_by_key(|(_, v)| v.as_i64().unwrap_or(0))
                    .map(|(k, _)| k.clone())
                    .unwrap();
                m.remove(&oldest);
            }
        }
        crate::private_file::replace_private(
            &self.path,
            Value::Object(self.doc.clone()).to_string().as_bytes(),
        )
        .map_err(|_| WakeError("state_unavailable"))
    }
}

fn unix_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

type CredentialPair = (url::Url, HeaderValue);
type KeychainRead = dyn Fn(&Path) -> std::io::Result<Vec<u8>> + Send + Sync;

struct KeychainSource {
    marker: PathBuf,
    read: Box<KeychainRead>,
    credentials: OnceLock<Result<CredentialPair>>,
}

#[derive(Clone)]
pub struct WakeConfig {
    pub db: PathBuf,
    pub url_file: PathBuf,
    pub auth_file: PathBuf,
    pub state_file: PathBuf,
    pub stop: Option<Arc<AtomicBool>>,
    keychain: Option<Arc<KeychainSource>>,
    explicit_files: bool,
}
impl WakeConfig {
    pub fn discover(db: &Path) -> Option<Self> {
        Self::discover_with_keychain(db, |_| Err(std::io::Error::other("Keychain unavailable")))
    }

    /// Presence of the marker selects Keychain and never falls back to files.
    pub fn discover_with_keychain(
        db: &Path,
        read: impl Fn(&Path) -> std::io::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Option<Self> {
        let dir = db.parent()?;
        let marker = dir.join(KEYCHAIN_MARKER);
        let selected = keychain_marker_selected(&marker);
        let c = Self {
            db: db.to_owned(),
            url_file: dir.join(URL_FILE),
            auth_file: dir.join(AUTH_FILE),
            state_file: dir.join(STATE_FILE),
            stop: None,
            keychain: selected.then(|| {
                Arc::new(KeychainSource {
                    marker,
                    read: Box::new(read),
                    credentials: OnceLock::new(),
                })
            }),
            explicit_files: false,
        };
        (selected || c.url_file.is_file() && c.auth_file.is_file()).then_some(c)
    }

    /// Use only the fixed files from a directory already validated at manifest load.
    pub fn from_files_dir(db: &Path, dir: &Path) -> Self {
        Self {
            db: db.to_owned(),
            url_file: dir.join(URL_FILE),
            auth_file: dir.join(AUTH_FILE),
            state_file: db.with_file_name(STATE_FILE),
            stop: None,
            keychain: None,
            explicit_files: true,
        }
    }

    fn credentials(&self) -> Result<CredentialPair> {
        if let Some(source) = &self.keychain {
            return source
                .credentials
                .get_or_init(|| {
                    crate::private_file::open_private_file(&source.marker)
                        .map_err(|_| WakeError("credential_unavailable"))?;
                    let home = source
                        .marker
                        .parent()
                        .ok_or(WakeError("credential_unavailable"))?;
                    let bytes =
                        (source.read)(home).map_err(|_| WakeError("credential_unavailable"))?;
                    decode_keychain_pair(&bytes)
                })
                .clone();
        }
        let read = if self.explicit_files {
            |path: &Path| read_wake_file(path).map_err(|_| WakeError("credential_unavailable"))
        } else {
            read_private
        };
        Ok((
            parse_url(&read(&self.url_file)?)?,
            parse_auth(&read(&self.auth_file)?)?,
        ))
    }
}

/// Discover with an explicit stop flag for safe shutdown.
pub fn discover_with_stop(db: &Path, stop: Arc<AtomicBool>) -> Option<WakeConfig> {
    let mut c = WakeConfig::discover(db)?;
    c.stop = Some(stop);
    Some(c)
}

fn client() -> Result<Client> {
    Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| WakeError("wake_transport"))
}

/// One POST. The configured source supplies both credentials.
async fn post_once(c: &WakeConfig, message_id: &str, conversation_id: &str) -> Result<u16> {
    let (url, auth) = c.credentials()?;
    let r = client()?
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::AUTHORIZATION, auth)
        .body(wake_body(message_id, conversation_id))
        .send()
        .await
        .map_err(|_| WakeError("wake_transport"))?;
    Ok(r.status().as_u16())
}

/// Bounded attempts for one ID. Returns the last status and the final outcome.
async fn notify(
    c: &WakeConfig,
    id: &str,
    authorize: &(impl Fn(&str) -> Option<String> + Send + Sync),
) -> (Option<u16>, Outcome) {
    let mut last = None;
    for i in 0..TRIES {
        let Some(conversation_id) = authorize(id) else {
            return (None, Outcome::Rejected);
        };
        match post_once(c, id, &conversation_id).await {
            Ok(status) => {
                last = Some(status);
                tracing::info!(
                    component = "grok_wake",
                    event = "wake_response",
                    status,
                    attempt = i + 1
                );
                let o = classify(status);
                if o != Outcome::Retry {
                    return (last, o);
                }
            }
            Err(WakeError(
                r @ ("invalid_url"
                | "invalid_header"
                | "invalid_keychain_pair"
                | "credential_file_mode"
                | "credential_unavailable"),
            )) => {
                tracing::warn!(
                    component = "grok_wake",
                    event = "wake_config_failed",
                    error_code = r
                );
                return (None, Outcome::AuthBlock);
            }
            Err(WakeError(error_code)) => tracing::warn!(
                component = "grok_wake",
                event = "wake_send_failed",
                error_code,
                attempt = i + 1
            ),
        }
        if i + 1 < TRIES {
            tokio::time::sleep(Duration::from_secs(BACKOFF[i.min(BACKOFF.len() - 1)])).await;
        }
    }
    (last, Outcome::Retry)
}

pub const STATUS_FILE: &str = "grok-wake-status.json";
/// Visible notifier state next to the wake state file. Never holds credentials.
fn set_status(c: &WakeConfig, state: &str, detail: Value) {
    let path = c.state_file.with_file_name(STATUS_FILE);
    let body = json!({"state": state, "detail": detail, "updated": unix_s(),"worker_pid":std::process::id()});
    if crate::private_file::replace_private(&path, body.to_string().as_bytes()).is_err() {
        tracing::warn!(component = "grok_wake", event = "wake_status_save_failed");
    }
}

/// Singleton lock next to the queue. Validated: regular file, one link, owned
/// by this user, mode 0600, not a symlink.
fn acquire_lock(db: &Path) -> std::result::Result<fs::File, &'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;
    let lock_path = db.with_extension("grok-wake.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("lock_file_is_symlink");
    }
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|_| "lock_file_unavailable")?;
    let meta = lock_file.metadata().map_err(|_| "lock_file_unavailable")?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o777 != 0o600
    {
        return Err("unsafe_lock_file");
    }
    if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("already_running");
    }
    Ok(lock_file)
}

/// Runs on its own thread with its own runtime. It never touches the store path,
/// so a slow or failed wake cannot block or fail a message commit. The notifier
/// is optional: a lock problem (already running, unsafe lock file) stops only
/// this thread and is marked failed in the status file; the hub keeps serving.
pub fn spawn(
    c: WakeConfig,
    deadline: Option<Instant>,
    authorize: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let stop = c.stop.clone();
    std::thread::Builder::new()
        .name("grok-wake".into())
        .spawn(move || {
            let _lock = match acquire_lock(&c.db) {
                Ok(lock) => lock,
                Err(reason) => {
                    tracing::warn!(
                        component = "grok_wake",
                        event = "wake_start_failed",
                        error_code = reason
                    );
                    return set_status(&c, "failed", json!({"reason": reason}));
                }
            };
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => {
                    tracing::warn!(
                        component = "grok_wake",
                        event = "wake_start_failed",
                        error_code = "runtime_unavailable"
                    );
                    return set_status(&c, "failed", json!({"reason": "runtime_unavailable"}));
                }
            };
            if c.keychain.is_some() && c.credentials().is_err() {
                tracing::warn!(
                    component = "grok_wake",
                    event = "wake_start_failed",
                    error_code = "keychain_unavailable"
                );
                return set_status(
                    &c,
                    "auth_blocked",
                    json!({"reason": "keychain_unavailable"}),
                );
            }
            set_status(&c, "running", Value::Null);
            rt.block_on(run(c, deadline, stop, &authorize));
        })
}

async fn run(
    c: WakeConfig,
    deadline: Option<Instant>,
    stop: Option<Arc<AtomicBool>>,
    authorize: &(impl Fn(&str) -> Option<String> + Send + Sync),
) {
    let mut state = WakeState::load(c.state_file.clone());
    let mut auth_block: Option<Option<Vec<(u128, u64)>>> = None;
    let mut rate_until = Instant::now();
    let mut sent: Vec<Instant> = Vec::new();
    let mut after_seq = 0;
    tracing::info!(component = "grok_wake", event = "wake_started");
    while !stop.as_ref().is_some_and(|s| s.load(Ordering::Acquire))
        && deadline.is_none_or(|d| Instant::now() < d)
    {
        sweep(
            &c,
            &mut state,
            &mut auth_block,
            &mut rate_until,
            &mut sent,
            &mut after_seq,
            authorize,
        )
        .await;
        let poll_start = Instant::now();
        while poll_start.elapsed() < POLL {
            if stop.as_ref().is_some_and(|s| s.load(Ordering::Acquire)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn sweep(
    c: &WakeConfig,
    state: &mut WakeState,
    auth_block: &mut Option<Option<Vec<(u128, u64)>>>,
    rate_until: &mut Instant,
    sent: &mut Vec<Instant>,
    after_seq: &mut i64,
    authorize: &(impl Fn(&str) -> Option<String> + Send + Sync),
) {
    if let Some(fp) = auth_block {
        if *fp == fingerprint(c) {
            return;
        }
        *auth_block = None;
        tracing::info!(component = "grok_wake", event = "wake_auth_block_cleared");
        set_status(c, "running", json!({"cleared": "auth_blocked"}));
    }
    if Instant::now() < *rate_until {
        return;
    }
    let ids = match pending_page(&c.db, unix_s() * 1000, *after_seq) {
        Ok((ids, next)) => {
            *after_seq = next;
            ids
        }
        Err(WakeError(error_code)) => {
            tracing::warn!(
                component = "grok_wake",
                event = "wake_scan_failed",
                error_code
            );
            return;
        }
    };
    let mut seen = HashSet::new();
    let todo: Vec<String> = ids
        .into_iter()
        .filter(|id| state.due(id) && seen.insert(id.clone()))
        .collect();
    for id in todo {
        sent.retain(|t| t.elapsed() < Duration::from_secs(3600));
        if sent.len() >= HOURLY_CAP {
            tracing::warn!(component = "grok_wake", event = "wake_hourly_cap_reached");
            return;
        }
        if authorize(&id).is_none() {
            continue;
        }
        sent.push(Instant::now());
        let (status, outcome) = notify(c, &id, authorize).await;
        if outcome == Outcome::Delivered {
            state.delivered(&id);
        } else if (status.is_some() || outcome == Outcome::Retry)
            && state.failed(&id, status) >= MAX_ROUNDS
        {
            tracing::warn!(component = "grok_wake", event = "wake_attempts_exhausted");
        }
        if state.save().is_err() {
            tracing::warn!(component = "grok_wake", event = "wake_state_save_failed");
        }
        match outcome {
            // Stops are recorded in the status file too, so a stopped notifier
            // does not look "running" to anyone who reads only that file.
            Outcome::AuthBlock => {
                *auth_block = Some(fingerprint(c));
                set_status(c, "auth_blocked", json!({"status": status}));
                tracing::warn!(
                    component = "grok_wake",
                    event = "wake_auth_blocked",
                    credential_source = if c.keychain.is_some() {
                        "keychain"
                    } else {
                        "file"
                    }
                );
                return;
            }
            Outcome::RateLimited => {
                *rate_until = Instant::now() + RATE_COOLDOWN;
                set_status(
                    c,
                    "rate_limited",
                    json!({"until": unix_s() + RATE_COOLDOWN.as_secs() as i64, "status": status}),
                );
                tracing::warn!(
                    component = "grok_wake",
                    event = "wake_rate_limited",
                    cooldown_seconds = RATE_COOLDOWN.as_secs()
                );
                return;
            }
            Outcome::Delivered => {
                set_status(c, "running", json!({"outcome":"accepted","status":status}))
            }
            Outcome::Retry => set_status(c, "running", json!({"outcome":"failed","status":status})),
            Outcome::Rejected if status.is_some() => {
                set_status(c, "running", json!({"outcome":"rejected","status":status}));
            }
            Outcome::Rejected => {}
        }
    }
}

#[cfg(test)]
#[path = "grok_wake_tests.rs"]
mod tests;
