//! Bearer and central registration checks for the isolated Cloud Grok Bot profile.
use super::Fault;
use crate::auth::{
    RETIRED_SENDERS_CODE, presented_token_digest, public_policy_fault, retired_senders_present,
};
use pigeonmq::{grok_mailbox::GrokScope, registration::load_registrations};
use serde::Deserialize;
use std::{collections::HashSet, path::Path};
use subtle::ConstantTimeEq;

fn now_ms() -> Result<u64, Fault> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| (503, "auth_unavailable"))?;
    u64::try_from(elapsed.as_millis()).map_err(|_| (503, "auth_unavailable"))
}

// Grok-bot is a separate authenticated profile of the existing loopback transport.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(super) enum GrokOperation {
    Send,
    Read,
    Wait,
    Reply,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrokPolicy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    #[serde(
        default,
        rename = "senders",
        deserialize_with = "retired_senders_present"
    )]
    retired_senders: bool,
    operations: Vec<GrokOperation>,
    enabled: bool,
    expires_at_ms: u64,
    #[serde(default)]
    registration: Option<String>,
}

pub(super) struct GrokPrincipal {
    operations: Vec<GrokOperation>,
    expires_at_ms: u64,
    registration: Option<String>,
}

impl GrokPrincipal {
    /// Path to the common registration file, if configured.
    pub(super) fn registration_path(&self) -> Option<&str> {
        self.registration.as_deref()
    }
    pub(super) fn require_operation(&self, operation: GrokOperation) -> Result<(), Fault> {
        if now_ms()? >= self.expires_at_ms {
            return Err((401, "unauthorized"));
        }
        if !self.operations.contains(&operation) {
            return Err((403, "forbidden"));
        }
        Ok(())
    }
    pub(super) fn scope(&self, operation: GrokOperation) -> Result<GrokScope, Fault> {
        self.require_operation(operation)?;
        Ok(GrokScope {
            allowed_senders: grok_registered_peers(self)?,
        })
    }
}

fn grok_sender(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

// FIXME(yagni): load_grok/authenticate_grok copy load_muse/authenticate_muse in auth.rs - one shared single-principal loader is enough
fn load_grok(path: &Path) -> Result<(GrokPrincipal, [u8; 32]), Fault> {
    let fail = (503, "auth_unavailable");
    let bytes = crate::auth::read_private_policy(path)?;
    let policy: GrokPolicy = serde_json::from_slice(&bytes).map_err(|_| fail)?;
    if policy.retired_senders {
        return Err((503, RETIRED_SENDERS_CODE));
    }
    let hash = crate::auth::digest(&policy.token_sha256).ok_or(fail)?;
    let mut operations = HashSet::new();
    if policy.principal != "grok-bot"
        || policy.mailbox != "grok-bot"
        || policy.operations.is_empty()
        || !policy.operations.iter().all(|o| operations.insert(o))
    {
        return Err(fail);
    }
    if !policy.enabled || now_ms()? >= policy.expires_at_ms {
        return Err((401, "unauthorized"));
    }
    Ok((
        GrokPrincipal {
            operations: policy.operations,
            expires_at_ms: policy.expires_at_ms,
            registration: policy.registration,
        },
        hash,
    ))
}

pub(crate) fn validate_grok_setup_policy(
    path: &Path,
    registration: &Path,
    token_sha256: &str,
) -> Result<(), Fault> {
    let (principal, hash) = load_grok(path)?;
    if principal.registration_path().map(Path::new) != Some(registration)
        || crate::auth::digest(token_sha256) != Some(hash)
    {
        return Err((409, "grok_setup_policy_conflict"));
    }
    Ok(())
}

pub(super) fn validate_grok(path: &Path) -> Result<(), Fault> {
    grok_registered_peers(&load_grok(path)?.0).map(|_| ())
}

pub(super) fn authenticate_grok(
    path: &Path,
    authorization: &[Vec<u8>],
) -> Result<GrokPrincipal, Fault> {
    let (principal, hash) = load_grok(path).map_err(public_policy_fault)?;
    let presented = presented_token_digest(authorization)?;
    if hash.ct_eq(&presented).unwrap_u8() != 1 {
        return Err((401, "unauthorized"));
    }
    Ok(principal)
}

/// Trusted local installation only. Never use this as an HTTP authentication fallback.
pub(super) fn local_grok_scope(path: &Path, operation: GrokOperation) -> Result<GrokScope, Fault> {
    load_grok(path)?.0.scope(operation)
}

fn grok_registered_peers(principal: &GrokPrincipal) -> Result<Vec<String>, Fault> {
    let path = principal
        .registration_path()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or((503, "auth_unavailable"))?;
    let registered = load_registrations(path)?;
    if !registered
        .0
        .get("grok-bot")
        .is_some_and(|e| !e.revoked && e.mailbox == "grok-bot")
    {
        return Err((403, "forbidden"));
    }
    Ok(registered
        .0
        .into_iter()
        .filter_map(|(id, entry)| {
            (!entry.revoked && entry.mailbox == id && grok_sender(&id)).then_some(id)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn grok_registration_requires_an_absolute_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let registry = directory.path().join("registration.json");
        let policy = directory.path().join("policy.json");
        let write = |path: &Path, value: serde_json::Value| {
            pigeonmq::private_file::replace_private(path, value.to_string().as_bytes()).unwrap();
        };
        write(
            &registry,
            json!({"grok-bot":{"mailbox":"grok-bot","revoked":false}}),
        );
        // Refer to the same real file relative to the process cwd without changing it.
        let relative = std::env::current_dir()
            .unwrap()
            .ancestors()
            .skip(1)
            .map(|_| "..")
            .collect::<std::path::PathBuf>()
            .join(registry.strip_prefix("/").unwrap());
        assert_eq!(
            relative.canonicalize().unwrap(),
            registry.canonicalize().unwrap()
        );
        let mut value = json!({
            "principal": "grok-bot", "mailbox": "grok-bot", "token_sha256": "a".repeat(64),
            "operations": ["read"], "enabled": true, "expires_at_ms": u64::MAX,
            "registration": registry,
        });
        write(&policy, value.clone());
        assert_eq!(validate_grok(&policy), Ok(()));
        value["registration"] = json!(relative);
        write(&policy, value);
        assert_eq!(validate_grok(&policy), Err((503, "auth_unavailable")));
    }

    #[test]
    fn operations_must_be_nonempty_unique_and_known() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("policy.json");
        let mut policy = json!({
            "principal": "grok-bot", "mailbox": "grok-bot", "token_sha256": "a".repeat(64),
            "enabled": true, "expires_at_ms": u64::MAX,
        });
        for (operations, valid) in [
            (json!(["send", "read", "wait", "reply"]), true),
            (json!([]), false),
            (json!(["read", "read"]), false),
            (json!(["send", "read", "wait", "reply", "read"]), false),
            (json!(["send", "read", "wait", "reply", "receive"]), false),
        ] {
            policy["operations"] = operations;
            fs::write(&path, policy.to_string()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(load_grok(&path).is_ok(), valid, "{}", policy["operations"]);
        }
    }

    #[test]
    fn grok_rejects_any_value_of_the_retired_senders_key() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("policy.json");
        let mut policy = json!({
            "principal": "grok-bot", "mailbox": "grok-bot", "token_sha256": "a".repeat(64),
            "operations": ["read"], "enabled": true, "expires_at_ms": u64::MAX,
        });
        for obsolete in [serde_json::Value::Null, json!(["fixture-only"]), json!({})] {
            policy["senders"] = obsolete;
            fs::write(&path, policy.to_string()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(validate_grok(&path), Err((503, RETIRED_SENDERS_CODE)));
            assert_eq!(
                authenticate_grok(&path, &[]).err(),
                Some((503, "auth_unavailable"))
            );
        }
    }
}
