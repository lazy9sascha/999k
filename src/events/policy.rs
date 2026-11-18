//! Source: PigeonMQ e399161, src/dot_service_preflight.rs (MIT).
//! Read-only Events policy preflight. No listener, subscription grant, credential
//! creation, schema preparation, or installed-service mutation.
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
    time::Duration,
};
use subtle::ConstantTimeEq;

#[derive(Debug, PartialEq, Eq)]
pub struct PreflightError {
    pub reason: &'static str,
    pub code: i32,
}
fn error(reason: &'static str) -> PreflightError {
    PreflightError {
        reason,
        code: -32602,
    }
}
impl From<std::io::Error> for PreflightError {
    fn from(_: std::io::Error) -> Self {
        error("setup_or_storage_error")
    }
}
impl From<rusqlite::Error> for PreflightError {
    fn from(_: rusqlite::Error) -> Self {
        error("setup_or_storage_error")
    }
}
type Result<T> = std::result::Result<T, PreflightError>;

// Deliberately not Debug/Serialize: callers must never print policy material.
pub struct EventsPolicy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    senders: Vec<String>,
    callback_hosts: Vec<String>,
    registered: bool,
    terminal_only: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyInput {
    principal: Value,
    token_sha256: Value,
    mailbox: Value,
    senders: Value,
    callback_hosts: Value,
    enabled: Value,
    #[serde(default)]
    registration: Option<std::path::PathBuf>,
}
fn identifier(value: &Value, limit: usize) -> Result<String> {
    let s = value.as_str().ok_or_else(|| error("invalid_string"))?;
    if s.len() > limit
        || s.chars()
            .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        || s.chars().any(|c| c < '\u{20}')
    {
        return Err(error("invalid_string"));
    }
    Ok(s.into())
}
fn names(value: &Value) -> Result<Vec<String>> {
    let values = value
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 32)
        .ok_or_else(|| error("invalid_policy"))?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in values {
        let name = identifier(value, 253)?;
        if !seen.insert(name.clone()) {
            return Err(error("invalid_policy"));
        }
        result.push(name);
    }
    Ok(result)
}
/// Reload authority at every subscription mutation and delivery boundary.
pub type PolicySource = std::sync::Arc<dyn Fn() -> Result<EventsPolicy> + Send + Sync>;

impl EventsPolicy {
    pub fn load(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(error("invalid_policy"));
        }
        let mut raw = Vec::new();
        file.take(16385).read_to_end(&mut raw)?;
        if raw.len() > 16384 {
            return Err(error("invalid_policy"));
        }
        // Serde structs can accept positional arrays; this policy requires an object.
        if raw.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            return Err(if serde_json::from_slice::<Value>(&raw).is_ok() {
                error("invalid_fields")
            } else {
                error("setup_or_storage_error")
            });
        }
        let input: PolicyInput = serde_json::from_slice(&raw).map_err(|e| {
            if e.is_syntax() || e.is_eof() {
                error("setup_or_storage_error")
            } else if e.to_string().contains("duplicate field") {
                error("duplicate_json_key")
            } else if e.to_string().contains("unknown field")
                || e.to_string().contains("missing field")
            {
                error("invalid_fields")
            } else {
                error("invalid_policy")
            }
        })?;
        let policy = Self::from_input(input, "dot", None)?;
        // Codex is a separate fixed principal, never a dot-policy alias.
        if policy.principal == "codex-cloud" {
            return Err(error("invalid_policy"));
        }
        Ok(policy)
    }
    /// Trusted host projection of the existing Codex bearer/registration policy.
    /// HTTP arguments cannot construct this authority.
    pub fn for_codex(
        token_sha256: String,
        senders: Vec<String>,
        callback_hosts: Vec<String>,
    ) -> Result<Self> {
        Self::from_input(
            PolicyInput {
                principal: Value::String("codex-cloud".into()),
                token_sha256: Value::String(token_sha256),
                mailbox: Value::String("codex-cloud".into()),
                senders: Value::Null,
                callback_hosts: serde_json::json!(callback_hosts),
                enabled: Value::Bool(true),
                registration: None,
            },
            "codex-cloud",
            Some(senders),
        )
    }
    /// Trusted standalone Dot projection. Only linked replies can wake this recipient.
    pub fn for_dot_replies(
        principal: String,
        token_sha256: String,
        senders: Vec<String>,
        callback_hosts: Vec<String>,
    ) -> Result<Self> {
        if principal == "codex-cloud" {
            return Err(error("invalid_policy"));
        }
        let mut policy = Self::from_input(
            PolicyInput {
                principal: Value::String(principal),
                token_sha256: Value::String(token_sha256),
                mailbox: Value::String("dot".into()),
                senders: Value::Null,
                callback_hosts: json!(callback_hosts),
                enabled: Value::Bool(true),
                registration: None,
            },
            "dot",
            Some(senders),
        )?;
        policy.terminal_only = true;
        Ok(policy)
    }
    pub fn terminal_only(&self) -> bool {
        self.terminal_only
    }
    fn from_input(
        input: PolicyInput,
        mailbox: &str,
        registered_senders: Option<Vec<String>>,
    ) -> Result<Self> {
        if input.mailbox != mailbox || input.enabled != true {
            return Err(PreflightError {
                reason: "policy_disabled",
                code: -32003,
            });
        }
        let principal = identifier(&input.principal, 128)?;
        let token_sha256 = input
            .token_sha256
            .as_str()
            .filter(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
            .ok_or_else(|| error("invalid_policy"))?
            .to_owned();
        let registered = input.registration.is_some() || registered_senders.is_some();
        // FIXME: policy senders is ignored when registration is set, otherwise used as a second allowlist; peer access should come from registration alone.
        let senders = if let Some(peers) = registered_senders {
            // Same central-registration projection as the dot binding; no peers is valid.
            peers
        } else if let Some(path) = &input.registration {
            crate::registration::active_peers(path, mailbox).map_err(|(_, reason)| error(reason))?
        } else {
            names(&input.senders)?
        };
        let callback_hosts = names(&input.callback_hosts)?;
        for host in &callback_hosts {
            let parts: Vec<_> = host.split(['.', '-']).collect();
            if parts.len() < 2
                || parts.iter().any(|s| {
                    s.is_empty()
                        || !s
                            .bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                })
            {
                return Err(error("invalid_policy"));
            }
        }
        if senders.iter().any(|s| s == mailbox) {
            return Err(error("self_reply_not_supported"));
        }
        Ok(Self {
            principal,
            mailbox: mailbox.to_owned(),
            token_sha256,
            senders,
            callback_hosts,
            registered,
            terminal_only: false,
        })
    }
    pub fn send_enabled(&self) -> bool {
        self.registered
    }
    pub fn principal(&self) -> &str {
        &self.principal
    }
    pub fn mailbox(&self) -> &str {
        &self.mailbox
    }
    pub fn allowed_senders(&self) -> &[String] {
        &self.senders
    }
    pub fn authorize(&self, token: &str) -> Result<&str> {
        if token.is_empty() || token.len() > 256 || !token.bytes().all(|c| (32..=126).contains(&c))
        {
            return Err(PreflightError {
                reason: "unauthorized",
                code: -32001,
            });
        }
        let hash: String = Sha256::digest(token.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if hash
            .as_bytes()
            .ct_eq(self.token_sha256.as_bytes())
            .unwrap_u8()
            != 1
        {
            return Err(PreflightError {
                reason: "unauthorized",
                code: -32001,
            });
        }
        Ok(&self.principal)
    }
    /// Validate an already-stored callback for inspection, without DNS or I/O.
    /// Actual callback transport must additionally validate and pin public DNS.
    pub fn callback_allowed(&self, url: &str) -> bool {
        if url.chars().count() > 2048 || url.chars().any(|c| c <= ' ') || url.contains('\\') {
            return false;
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("https") {
            return false;
        }
        if rest
            .split_once('#')
            .is_some_and(|(_, fragment)| !fragment.is_empty())
        {
            return false;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        // Match the existing inspector's empty-userinfo behavior. This does not
        // authorize transport; a future sender must apply its own URL/DNS gate.
        let authority = match authority.rsplit_once('@') {
            Some(("" | ":", host)) => host,
            Some(_) => return false,
            None => authority,
        };
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        if !port.is_empty()
            && (!port.bytes().all(|c| c.is_ascii_digit()) || port.parse::<u16>() != Ok(443))
        {
            return false;
        }
        self.callback_hosts.contains(&host.to_ascii_lowercase())
    }
}

pub fn inspect_state(
    db: &Path,
    policy: &EventsPolicy,
    now: f64,
    allow_nonexpiring: bool,
) -> Result<Value> {
    if !now.is_finite() {
        return Err(error("invalid_time"));
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    crate::validate_schema(&conn).map_err(|err| match err {
        crate::Error::Database(error) => error.into(),
        _ => error("requires_current_schema_queue"),
    })?;
    let tables: HashSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let initialized = [
        "dot_event_subscriptions",
        "dot_event_outbox",
        "dot_event_replies",
    ]
    .iter()
    .all(|t| tables.contains(*t));
    let mut result = json!({"state":"local_inspection", "agent":"dot", "wake_verified":false, "bridge_storage_initialized":initialized});
    if !initialized {
        return Ok(result);
    }
    let mut subscriptions = json!({"active":0,"expired":0,"inactive":0,"policy_blocked":0});
    let mut statement =
        conn.prepare("SELECT active,expires,url FROM dot_event_subscriptions WHERE owner=?1")?;
    let rows = statement.query_map([&policy.principal], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (active, expires, url) = row?;
        let category = if active == 0 {
            "inactive"
        } else if expires == 0.0 && !allow_nonexpiring {
            "policy_blocked"
        } else if expires != 0.0 && expires <= now {
            "expired"
        } else if policy.callback_allowed(&url) {
            "active"
        } else {
            "policy_blocked"
        };
        subscriptions[category] = json!(subscriptions[category].as_u64().unwrap_or(0) + 1);
    }
    let mut outbox = json!({"pending":0,"accepted":0,"stopped":0});
    let mut statement = conn.prepare("SELECT o.state,count(*) FROM dot_event_outbox o JOIN dot_event_subscriptions s ON s.id=o.subscription_id WHERE s.owner=?1 GROUP BY o.state")?;
    for row in statement.query_map([&policy.principal], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (state, count) = row?;
        if outbox.get(&state).is_some() {
            outbox[&state] = json!(count);
        }
    }
    result["subscriptions"] = subscriptions;
    result["outbox"] = outbox;
    if tables.contains("dot_subscription_diagnostic") {
        let diagnostic = conn.query_row("SELECT observed_at,outcome,reason,ttl_mode,granted_expires FROM dot_subscription_diagnostic WHERE owner=?1", [&policy.principal], |r| Ok(json!({"observed_at":r.get::<_,f64>(0)?,"outcome":r.get::<_,String>(1)?,"reason":r.get::<_,Option<String>>(2)?,"ttl_mode":r.get::<_,String>(3)?,"granted_expires":r.get::<_,Option<f64>>(4)?}))).optional()?;
        result["last_subscription_attempt"] = diagnostic.unwrap_or(Value::Null);
    }
    Ok(result)
}
