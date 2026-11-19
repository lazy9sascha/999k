//! Source: PigeonMQ e399161, src/dot_subscriptions.rs (MIT).
//! Shared Events subscription lifecycle. Only an authenticated explicit request can
//! create/refresh a grant. Challenge transport is injected for synthetic tests;
//! production must use CallbackTransport, never an unsigned sender.
use super::{
    callback_transport::{self, CallbackError, CallbackResponse, SignedCallback},
    policy::{EventsPolicy, PolicySource, PreflightError},
};
use chrono::{DateTime, Datelike, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use subtle::ConstantTimeEq;
const BRIDGE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS dot_event_subscriptions(id TEXT PRIMARY KEY,owner TEXT NOT NULL,url TEXT NOT NULL,secret TEXT NOT NULL,previous_secret TEXT,rotation_until REAL,expires REAL NOT NULL,active INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS dot_event_replies(message_id TEXT PRIMARY KEY,reply_id TEXT NOT NULL UNIQUE);
         CREATE TABLE IF NOT EXISTS dot_event_outbox(subscription_id TEXT NOT NULL,message_id TEXT NOT NULL,event_id TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_at REAL NOT NULL DEFAULT 0,state TEXT NOT NULL DEFAULT 'pending',PRIMARY KEY(subscription_id,message_id));
         CREATE TABLE IF NOT EXISTS dot_subscription_generations(subscription_id TEXT PRIMARY KEY,generation INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS dot_subscription_diagnostic(owner TEXT PRIMARY KEY,observed_at REAL NOT NULL,outcome TEXT NOT NULL,reason TEXT,ttl_mode TEXT NOT NULL,granted_expires REAL);";
// Dot-only additive storage. Codex and legacy schema requirements are unchanged.
const DOT_CUTOFF_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS dot_subscription_cutoffs(subscription_id TEXT PRIMARY KEY,after_seq INTEGER NOT NULL);";
const EVENT: &str = "pigeonmq.message.created";
type Result<T> = std::result::Result<T, PreflightError>;
fn error(reason: &'static str) -> PreflightError {
    PreflightError {
        reason,
        code: -32602,
    }
}
fn live(cancelled: &mut impl FnMut() -> bool) -> Result<()> {
    if cancelled() {
        Err(PreflightError {
            reason: "request_cancelled",
            code: -32021,
        })
    } else {
        Ok(())
    }
}
fn challenge_failed() -> PreflightError {
    PreflightError {
        reason: "challenge_failed",
        code: -32015,
    }
}
fn sha(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn fields(value: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let obj = value.as_object().ok_or_else(|| error("invalid_fields"))?;
    if required.iter().any(|key| !obj.contains_key(*key))
        || obj
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(error("invalid_fields"));
    }
    Ok(())
}
fn now(clock: &mut impl FnMut() -> f64) -> Result<f64> {
    let value = clock();
    if !value.is_finite() || value < 0.0 {
        Err(error("invalid_time"))
    } else {
        Ok(value)
    }
}
pub fn timestamp(seconds: f64) -> Result<String> {
    let micros = (seconds * 1_000_000.0).round_ties_even();
    if !micros.is_finite() || micros < i64::MIN as f64 || micros > i64::MAX as f64 {
        return Err(error("invalid_time"));
    }
    let time = DateTime::<Utc>::from_timestamp_micros(micros as i64)
        .ok_or_else(|| error("invalid_time"))?;
    if !(1..=9999).contains(&time.year()) {
        return Err(error("invalid_time"));
    }
    let base = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let fraction = time.timestamp_subsec_micros();
    Ok(if fraction == 0 {
        format!("{base}Z")
    } else {
        format!("{base}.{fraction:06}Z")
    })
}
/// Zero is an explicit nonexpiring request only with deployment opt-in.
pub fn subscription_duration(params: &Value, allow_nonexpiring: bool) -> Result<f64> {
    match params.get("ttlMs") {
        None => Ok(3600.0),
        Some(Value::Null) => Ok(if allow_nonexpiring { 0.0 } else { 3600.0 }),
        Some(value) => value
            .as_u64()
            .filter(|v| *v >= 1000)
            .map(|v| v.min(3_600_000) as f64 / 1000.0)
            .ok_or_else(|| error("invalid_ttl")),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ColumnSchema {
    name: String,
    kind: String,
    required: i64,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
    collation: Option<Vec<u8>>,
    autoincrement: bool,
}
fn columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<ColumnSchema>> {
    let mut columns = conn
        .prepare(&format!("PRAGMA table_xinfo({table})"))?
        .query_map([], |r| {
            Ok(ColumnSchema {
                name: r.get(1)?,
                kind: r.get::<_, String>(2)?.to_ascii_uppercase(),
                required: r.get(3)?,
                default: r.get(4)?,
                primary_key: r.get(5)?,
                hidden: r.get(6)?,
                collation: None,
                autoincrement: false,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for column in &mut columns {
        let metadata = conn.column_metadata(None, table, column.name.as_str())?;
        column.collation = metadata.1.map(|name| name.to_bytes().to_vec());
        column.autoincrement = metadata.4;
    }
    Ok(columns)
}
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UniqueIndex {
    origin: String,
    partial: i64,
    // Preserve collation and direction as well as ordered key columns.
    columns: Vec<(i64, Option<String>, i64, String)>,
}
fn unique_indexes(conn: &Connection, table: &str) -> rusqlite::Result<Vec<UniqueIndex>> {
    let indexes: Vec<(String, String, i64)> = conn
        .prepare(&format!("PRAGMA index_list({table})"))?
        .query_map([], |r| {
            if r.get::<_, i64>(2)? != 0 {
                Ok(Some((r.get(1)?, r.get(3)?, r.get(4)?)))
            } else {
                Ok(None)
            }
        })?
        .filter_map(|r| r.transpose())
        .collect::<rusqlite::Result<_>>()?;
    let mut result = Vec::new();
    for (name, origin, partial) in indexes {
        let escaped = name.replace('"', "\"\"");
        let rows = conn
            .prepare(&format!("PRAGMA index_xinfo(\"{escaped}\")"))?
            .query_map([], |r| {
                if r.get::<_, i64>(5)? != 0 {
                    Ok(Some((r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                } else {
                    Ok(None)
                }
            })?
            .filter_map(|r| r.transpose())
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result.push(UniqueIndex {
            origin,
            partial,
            columns: rows,
        });
    }
    result.sort();
    Ok(result)
}
pub fn validate_bridge_schema(conn: &Connection) -> Result<()> {
    crate::validate_schema(conn).map_err(|err| match err {
        crate::Error::Database(error) => error.into(),
        _ => error("requires_current_schema_queue"),
    })?;
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(BRIDGE_SCHEMA)?;
    for table in [
        "dot_event_subscriptions",
        "dot_event_replies",
        "dot_event_outbox",
        "dot_subscription_generations",
        "dot_subscription_diagnostic",
    ] {
        if columns(conn, table)? != columns(&expected, table)?
            || unique_indexes(conn, table)? != unique_indexes(&expected, table)?
        {
            return Err(error("incompatible_bridge_schema"));
        }
    }
    Ok(())
}

/// Dot subscriptions retain their first successful grant boundary across refresh.
pub fn validate_dot_cutoff_schema(conn: &Connection) -> Result<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(DOT_CUTOFF_SCHEMA)?;
    let table = "dot_subscription_cutoffs";
    if columns(conn, table)? != columns(&expected, table)?
        || unique_indexes(conn, table)? != unique_indexes(&expected, table)?
    {
        return Err(error("incompatible_dot_cutoff_schema"));
    }
    Ok(())
}
pub(super) fn dot_start_seq(conn: &Connection, sid: &str) -> Result<i64> {
    let seq: i64 = conn.query_row(
        "SELECT after_seq FROM dot_subscription_cutoffs WHERE subscription_id=?1",
        [sid],
        |row| row.get::<_, i64>(0),
    )?;
    if seq < 0 {
        return Err(error("invalid_dot_subscription_cutoff"));
    }
    Ok(seq)
}

pub struct SubscriptionStore {
    db: PathBuf,
    policy: PolicySource,
    allow_nonexpiring: bool,
    verified: Mutex<HashMap<(String, String, String), f64>>,
}
impl SubscriptionStore {
    pub fn new(db: impl AsRef<Path>, policy: impl AsRef<Path>, allow_nonexpiring: bool) -> Self {
        let path = policy.as_ref().to_owned();
        Self::with_policy(
            db,
            std::sync::Arc::new(move || EventsPolicy::load(&path)),
            allow_nonexpiring,
        )
    }
    pub fn with_policy(
        db: impl AsRef<Path>,
        policy: PolicySource,
        allow_nonexpiring: bool,
    ) -> Self {
        Self {
            db: db.as_ref().to_owned(),
            policy,
            allow_nonexpiring,
            verified: Mutex::new(HashMap::new()),
        }
    }
    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        crate::validate_schema(&conn).map_err(|err| match err {
            crate::Error::Database(error) => error.into(),
            _ => error("requires_current_schema_queue"),
        })?;
        crate::mailbox_generation::scope_connection(&conn)?;
        Ok(conn)
    }
    fn authorize(&self, owner: &str, token: &str) -> Result<EventsPolicy> {
        let policy = (self.policy)().map_err(|err| {
            if err.reason == "setup_or_storage_error" {
                PreflightError {
                    reason: "policy_unavailable",
                    code: -32003,
                }
            } else {
                err
            }
        })?;
        if policy.authorize(token)? != owner {
            return Err(PreflightError {
                reason: "unauthorized",
                code: -32001,
            });
        }
        Ok(policy)
    }
    /// Explicit setup only; remote list/subscribe never initializes storage.
    pub fn prepare(&self, owner: &str, token: &str) -> Result<()> {
        self.authorize(owner, token)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.authorize(owner, token)?;
        tx.execute_batch(BRIDGE_SCHEMA)?;
        validate_bridge_schema(&tx)?;
        if self.authorize(owner, token)?.dot_subscription_cutoff() {
            tx.execute_batch(DOT_CUTOFF_SCHEMA)?;
            validate_dot_cutoff_schema(&tx)?;
        }
        self.authorize(owner, token)?;
        tx.commit()?;
        Ok(())
    }
    pub fn prepare_local(&self) -> Result<()> {
        (self.policy)()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        (self.policy)()?;
        tx.execute_batch(BRIDGE_SCHEMA)?;
        validate_bridge_schema(&tx)?;
        if (self.policy)()?.dot_subscription_cutoff() {
            tx.execute_batch(DOT_CUTOFF_SCHEMA)?;
            validate_dot_cutoff_schema(&tx)?;
        }
        (self.policy)()?;
        tx.commit()?;
        Ok(())
    }
    fn stored_owner(connection: &Connection, policy: &EventsPolicy) -> Result<String> {
        generation_owner(connection, policy)?.ok_or_else(|| error("mailbox_retired"))
    }

    fn identity(
        &self,
        owner: &str,
        token: &str,
        params: &Value,
        secret: bool,
    ) -> Result<(String, String, EventsPolicy)> {
        let policy = self.authorize(owner, token)?;
        fields(
            params,
            &["name", "arguments", "delivery"],
            if secret {
                &["cursor", "ttlMs", "_meta"]
            } else {
                &["_meta"]
            },
        )?;
        if params.get("_meta").is_some_and(|v| !v.is_object()) {
            return Err(error("invalid_metadata"));
        }
        if params["name"] != EVENT || params["arguments"] != json!({"recipient":policy.mailbox()}) {
            return Err(error("unsupported_subscription"));
        }
        let delivery = &params["delivery"];
        fields(
            delivery,
            if secret {
                &["mode", "url", "secret"]
            } else {
                &["mode", "url"]
            },
            &[],
        )?;
        if delivery["mode"] != "webhook" {
            return Err(error("unsupported_delivery"));
        }
        let url = delivery["url"].as_str().ok_or(PreflightError {
            reason: "invalid_callback",
            code: -32015,
        })?;
        callback_transport::validate_callback_url(&policy, url).map_err(|_| {
            // identity() has already authenticated the caller and fixed the event/recipient.
            if secret
                && policy.dot_subscription_cutoff()
                && let Some(hostname) = rejected_callback_hostname(url)
            {
                eprintln!(
                    "{}",
                    json!({"dot_events":"callback_host_rejected","hostname":hostname})
                );
            }
            PreflightError {
                reason: "invalid_callback",
                code: -32015,
            }
        })?;
        let stored_owner = Self::stored_owner(&self.connect()?, &policy)?;
        let identity = serde_json::to_vec(&json!([stored_owner, url, EVENT, params["arguments"]]))
            .map_err(|_| error("invalid_input"))?;
        Ok((format!("sub_{}", sha(&identity)), stored_owner, policy))
    }
    fn diagnostic(
        &self,
        owner: &str,
        params: &Value,
        outcome: &str,
        reason: Option<&str>,
        expires: Option<f64>,
        at: f64,
    ) {
        let ttl = match params.get("ttlMs") {
            None => "omitted",
            Some(Value::Null) => "null",
            Some(v) if v.as_u64().is_some_and(|v| v >= 1000) => "finite",
            _ => "invalid",
        };
        // Best effort only: diagnostics cannot reverse or hide a committed grant.
        if let Ok(conn) = self.connect()
            && let Ok(policy) = (self.policy)()
            && policy.principal() == owner
            && let Ok(Some(owner)) = generation_owner(&conn, &policy)
        {
            let _=conn.execute("INSERT INTO dot_subscription_diagnostic VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(owner) DO UPDATE SET observed_at=excluded.observed_at,outcome=excluded.outcome,reason=excluded.reason,ttl_mode=excluded.ttl_mode,granted_expires=excluded.granted_expires",params![owner,at,outcome,reason,ttl,expires]);
        }
    }
    /// Synchronous trusted-local caller. HTTP hosts must use subscribe_guarded.
    pub fn subscribe<C, S>(
        &self,
        owner: &str,
        token: &str,
        params: &Value,
        clock: C,
        sender: S,
    ) -> Result<Value>
    where
        C: FnMut() -> f64,
        S: for<'a> FnMut(
            &EventsPolicy,
            SignedCallback<'a>,
        ) -> std::result::Result<CallbackResponse, CallbackError>,
    {
        self.subscribe_guarded(owner, token, params, clock, sender, || false)
    }
    pub fn subscribe_guarded<C, S, G>(
        &self,
        owner: &str,
        token: &str,
        params: &Value,
        mut clock: C,
        mut sender: S,
        mut cancelled: G,
    ) -> Result<Value>
    where
        G: FnMut() -> bool,
        C: FnMut() -> f64,
        S: for<'a> FnMut(
            &EventsPolicy,
            SignedCallback<'a>,
        ) -> std::result::Result<CallbackResponse, CallbackError>,
    {
        self.authorize(owner, token)?;
        let result = self.subscribe_inner(
            owner,
            token,
            params,
            &mut clock,
            &mut sender,
            &mut cancelled,
        );
        let observed = clock();
        match &result {
            Ok((_, expires)) => {
                self.diagnostic(owner, params, "accepted", None, Some(*expires), observed)
            }
            Err(e) => self.diagnostic(
                owner,
                params,
                if e.reason == "setup_or_storage_error" {
                    "failed"
                } else {
                    "rejected"
                },
                Some(e.reason),
                None,
                observed,
            ),
        }
        result.map(|(value, _)| value)
    }
    fn subscribe_inner<C, S, G>(
        &self,
        owner: &str,
        token: &str,
        params: &Value,
        clock: &mut C,
        sender: &mut S,
        cancelled: &mut G,
    ) -> Result<(Value, f64)>
    where
        G: FnMut() -> bool,
        C: FnMut() -> f64,
        S: for<'a> FnMut(
            &EventsPolicy,
            SignedCallback<'a>,
        ) -> std::result::Result<CallbackResponse, CallbackError>,
    {
        live(cancelled)?;
        let (sid, stored_owner, policy) = self.identity(owner, token, params, true)?;
        if params.get("cursor").is_some_and(|v| !v.is_null()) {
            return Err(error("replay_not_supported"));
        }
        let url = params["delivery"]["url"]
            .as_str()
            .ok_or_else(|| error("invalid_callback"))?;
        let secret = params["delivery"]["secret"]
            .as_str()
            .ok_or_else(|| error("invalid_signing_secret"))?;
        callback_transport::validate_signing_secret(secret)
            .map_err(|_| error("invalid_signing_secret"))?;
        let duration = subscription_duration(params, self.allow_nonexpiring)?;
        let cache_key = (stored_owner.clone(), url.to_owned(), sha(secret.as_bytes()));
        let current = now(clock)?;
        let verified = {
            let mut cache = self
                .verified
                .lock()
                .map_err(|_| error("setup_or_storage_error"))?;
            cache.retain(|_, until| *until > current);
            cache.get(&cache_key).is_some_and(|until| *until > current)
        };
        if !verified {
            let challenge = uuid::Uuid::new_v4().to_string();
            let event_id = format!("verify_{}", uuid::Uuid::new_v4());
            let body = serde_json::to_vec(&json!({"type":"verification","challenge":challenge}))
                .map_err(|_| error("invalid_input"))?;
            let start = now(clock)?;
            live(cancelled)?;
            let response = sender(
                &policy,
                SignedCallback {
                    url,
                    subscription_id: &sid,
                    event_id: &event_id,
                    body: &body,
                    current_secret: secret,
                    previous_secret: None,
                    signed_at: start,
                },
            )
            .map_err(|_| challenge_failed())?;
            let echoed =
                crate::mcp_request::strict_json(&response.body).map_err(|_| challenge_failed())?;
            let value = echoed
                .get("challenge")
                .and_then(Value::as_str)
                .ok_or_else(challenge_failed)?;
            let finished = now(clock)?;
            if !(200..300).contains(&response.status)
                || value.as_bytes().ct_eq(challenge.as_bytes()).unwrap_u8() != 1
                || finished - start > 10.0
            {
                return Err(challenge_failed());
            }
            let mut cache = self
                .verified
                .lock()
                .map_err(|_| error("setup_or_storage_error"))?;
            if cache.len() < 256 {
                cache.insert(cache_key, finished + 60.0);
            }
        }
        let policy = self.authorize(owner, token)?;
        callback_transport::validate_callback_url(&policy, url).map_err(|_| PreflightError {
            reason: "invalid_callback",
            code: -32015,
        })?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = self.authorize(owner, token)?;
        if Self::stored_owner(&tx, &policy)? != stored_owner {
            return Err(error("mailbox_generation_changed"));
        }
        callback_transport::validate_callback_url(&policy, url).map_err(|_| PreflightError {
            reason: "invalid_callback",
            code: -32015,
        })?;
        live(cancelled)?;
        let granted = now(clock)?;
        let expiry = if duration == 0.0 {
            0.0
        } else {
            granted + duration
        };
        let refresh = if expiry == 0.0 {
            None
        } else {
            Some(timestamp(expiry)?)
        };
        let old: Option<(String, bool, f64)> = tx
            .query_row(
                "SELECT secret,active,expires FROM dot_event_subscriptions WHERE id=?1",
                [&sid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let refresh_existing = old.as_ref().is_some_and(|(_, active, expires)| {
            *active && (*expires > granted || (*expires == 0.0 && self.allow_nonexpiring))
        });
        let previous = old.map(|(secret, _, _)| secret).filter(|old| old != secret);
        let rotation = previous.as_ref().map(|_| granted + 60.0);
        tx.execute("INSERT INTO dot_event_subscriptions VALUES (?1,?2,?3,?4,?5,?6,?7,1) ON CONFLICT(id) DO UPDATE SET secret=excluded.secret,previous_secret=COALESCE(excluded.previous_secret,dot_event_subscriptions.previous_secret),rotation_until=CASE WHEN excluded.previous_secret IS NULL THEN dot_event_subscriptions.rotation_until ELSE excluded.rotation_until END,expires=excluded.expires,active=1",params![sid,stored_owner,url,secret,previous,rotation,expiry])?;
        if policy.dot_subscription_cutoff() {
            // The write transaction serializes this boundary with message insertion.
            // AUTOINCREMENT high water survives deletion and does not depend on clocks.
            // Active refresh retains the boundary. A new or expired grant starts fresh.
            tx.execute(
                "INSERT INTO dot_subscription_cutoffs(subscription_id,after_seq) VALUES (?1,COALESCE((SELECT seq FROM sqlite_sequence WHERE name='messages'),0)) ON CONFLICT(subscription_id) DO UPDATE SET after_seq=excluded.after_seq WHERE ?2",
                params![sid, !refresh_existing],
            )?;
            dot_start_seq(&tx, &sid)?;
        }
        tx.execute("INSERT INTO dot_subscription_generations VALUES (?1,1) ON CONFLICT(subscription_id) DO UPDATE SET generation=dot_subscription_generations.generation+1",[&sid])?;
        tx.execute("UPDATE dot_event_outbox SET attempts=0,next_at=0 WHERE subscription_id=?1 AND state='pending'",[&sid])?;
        let current = self.authorize(owner, token)?;
        callback_transport::validate_callback_url(&current, url).map_err(|_| PreflightError {
            reason: "invalid_callback",
            code: -32015,
        })?;
        live(cancelled)?;
        tx.commit()?;
        Ok((
            json!({"id":sid,"refreshBefore":refresh,"cursor":null,"truncated":false}),
            expiry,
        ))
    }
    pub fn unsubscribe(&self, owner: &str, token: &str, params: &Value) -> Result<Value> {
        self.unsubscribe_guarded(owner, token, params, || false)
    }
    pub fn unsubscribe_guarded(
        &self,
        owner: &str,
        token: &str,
        params: &Value,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Value> {
        live(&mut cancelled)?;
        let (sid, stored_owner, _) = self.identity(owner, token, params, false)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = self.authorize(owner, token)?;
        if Self::stored_owner(&tx, &current)? != stored_owner {
            return Err(error("mailbox_generation_changed"));
        }
        live(&mut cancelled)?;
        tx.execute(
            "UPDATE dot_event_subscriptions SET active=0 WHERE id=?1 AND owner=?2",
            params![sid, stored_owner],
        )?;
        self.authorize(owner, token)?;
        live(&mut cancelled)?;
        tx.commit()?;
        Ok(json!({}))
    }
}

/// Keep the existing owner and subscription IDs in the initial generation.
pub(super) fn generation_owner(
    connection: &Connection,
    policy: &EventsPolicy,
) -> rusqlite::Result<Option<String>> {
    Ok(
        crate::mailbox_generation::current_storage(connection, policy.mailbox())?.map(|storage| {
            if storage == policy.mailbox() {
                policy.principal().to_owned()
            } else {
                format!("{storage}/{}", policy.principal())
            }
        }),
    )
}

// For protected operator diagnostics only. Never return a URL or client data.
fn rejected_callback_hostname(raw: &str) -> Option<String> {
    if raw.len() > 2048 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let url = url::Url::parse(raw).ok()?;
    let authority = raw
        .strip_prefix("https://")?
        .split(['/', '?', '#'])
        .next()?;
    if authority.contains('@')
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        return None;
    }
    let url::Host::Domain(host) = url.host()? else {
        return None;
    };
    if host.len() > 253
        || !host.contains('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return None;
    }
    Some(host.to_owned())
}

#[cfg(test)]
mod hostname_diagnostic_tests {
    use super::rejected_callback_hostname;
    #[test]
    fn emits_only_a_valid_https_hostname() {
        assert_eq!(
            rejected_callback_hostname(
                "https://Receiver.example.com/private-token?secret=NEVER_LOG"
            ),
            Some("receiver.example.com".into())
        );
        for raw in [
            "http://receiver.example.com/private",
            "https://user:secret@receiver.example.com",
            "https://@receiver.example.com",
            "https://receiver.example.com/#secret",
            "https://receiver.example.com:8443/path",
            "https://127.0.0.1/path",
            "https://[::1]/path",
            "https://receiver.example.com\n/path",
            "https://receiver.example.com./path",
            "https://bad_label.example/path",
        ] {
            assert!(rejected_callback_hostname(raw).is_none());
        }
    }
}
