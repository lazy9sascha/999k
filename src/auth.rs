//! Minimal bearer/operation/mailbox policy. No credential creation or Keychain.
use radio999k::private_file::MAX_PRIVATE_CONFIG_BYTES as MAX_POLICY;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::Read, path::Path};
use subtle::ConstantTimeEq;

type Fault = (u16, &'static str);

pub(crate) const RETIRED_SENDERS_CODE: &str = "remove_legacy_senders_from_policy";
pub(crate) const RETIRED_SENDERS_HELP: &str = "Remove only the top-level `senders` field from the Muse or Grok Bot policy. The central registration controls peer access.";

#[derive(Debug, thiserror::Error)]
#[error("{RETIRED_SENDERS_CODE}: {RETIRED_SENDERS_HELP}")]
pub(crate) struct RetiredSenders;

pub(crate) fn retired_senders_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

pub(crate) fn local_policy_error(reason: &'static str, fallback: &'static str) -> std::io::Error {
    if reason == RETIRED_SENDERS_CODE {
        std::io::Error::other(RetiredSenders)
    } else {
        std::io::Error::other(fallback)
    }
}

pub(crate) fn public_policy_fault(fault: Fault) -> Fault {
    if fault.1 == RETIRED_SENDERS_CODE {
        (503, "auth_unavailable")
    } else {
        fault
    }
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Request,
    Send,
    Read,
    Wait,
    Reply,
    Receive,
    Respond,
    Status,
    InspectDlq,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    principals: Vec<Entry>,
    // Codex Cloud projects these settings into the shared Events runtime.
    #[serde(default)]
    events: Option<Events>,
}
/// Explicit Codex Events configuration in the existing protected bearer policy.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Events {
    pub callback_hosts: Vec<String>,
    pub resolver: std::path::PathBuf,
    #[serde(default)]
    // FIXME(ponytail): speculative Events option; setup and tests keep it false and expiring subscriptions already work.
    pub allow_nonexpiring: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    token_sha256: String,
    operations: Vec<Operation>,
    // Generic send and request routes use this list. Registered mailbox routes use registration.
    send_to: Vec<String>,
    mailboxes: Vec<String>,
    revoked: bool,
}
pub struct Principal {
    pub id: String,
    operations: Vec<Operation>,
    send_to: Vec<String>,
    mailboxes: Vec<String>,
}
impl Principal {
    pub fn operation(&self, op: Operation) -> Result<(), Fault> {
        if self.operations.contains(&op) {
            Ok(())
        } else {
            Err((403, "forbidden"))
        }
    }
    pub fn identity(&self, label: &str) -> Result<(), Fault> {
        if self.id == label {
            Ok(())
        } else {
            Err((403, "forbidden"))
        }
    }
    pub fn target(&self, label: &str) -> Result<(), Fault> {
        if self.send_to.iter().any(|v| v == label) {
            Ok(())
        } else {
            Err((403, "forbidden"))
        }
    }
    pub fn mailboxes(&self) -> &[String] {
        &self.mailboxes
    }
    pub fn mailbox(&self, label: &str) -> Result<(), Fault> {
        if self.mailboxes.iter().any(|v| v == label) {
            Ok(())
        } else {
            Err((403, "forbidden"))
        }
    }
}
pub(crate) fn digest(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub(crate) fn read_private_policy(path: &Path) -> Result<Vec<u8>, Fault> {
    let fail = (503, "auth_unavailable");
    let mut bytes = Vec::new();
    open_private_file(path)?
        .take(MAX_POLICY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| fail)?;
    if bytes.len() as u64 > MAX_POLICY {
        return Err(fail);
    }
    Ok(bytes)
}
fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128
}
fn load_generic_policy(path: &Path) -> Result<Vec<(Entry, [u8; 32])>, Fault> {
    load_policy_file(path).map(|(entries, _)| entries)
}
type LoadedPolicy = (Vec<(Entry, [u8; 32])>, Option<Events>);
fn load_policy_file(path: &Path) -> Result<LoadedPolicy, Fault> {
    let fail = (503, "auth_unavailable");
    let bytes = read_private_policy(path)?;
    let policy: Policy = serde_json::from_slice(&bytes).map_err(|_| fail)?;
    if policy.principals.is_empty() {
        return Err(fail);
    }
    let mut identities = HashSet::new();
    let mut hashes = HashSet::new();
    let mut entries = Vec::new();
    for entry in policy.principals {
        let hash = digest(&entry.token_sha256).ok_or(fail)?;
        let mut operations = HashSet::new();
        if !label(&entry.id)
            || !identities.insert(entry.id.clone())
            || !hashes.insert(hash)
            || entry.operations.is_empty()
            || !entry.operations.iter().all(|op| operations.insert(op))
            || !entry
                .send_to
                .iter()
                .chain(&entry.mailboxes)
                .all(|v| label(v))
        {
            return Err(fail);
        }
        entries.push((entry, hash));
    }
    Ok((entries, policy.events))
}
/// Local Events projection from the same policy and registry used by Codex tools.
pub fn codex_events(
    path: &Path,
    registration: &Path,
) -> Result<Option<(Events, radio999k::events::policy::EventsPolicy)>, Fault> {
    let (entries, settings) = load_policy_file(path)?;
    let Some(settings) = settings else {
        return Ok(None);
    };
    if !settings.resolver.is_absolute() {
        return Err((503, "invalid_events_resolver"));
    }
    let (entry, _) = entries
        .into_iter()
        .find(|(entry, _)| entry.id == "codex-cloud")
        .ok_or((403, "forbidden"))?;
    if entry.revoked
        || !entry.operations.contains(&Operation::Read)
        || !entry.mailboxes.iter().any(|m| m == "codex-cloud")
    {
        return Err((403, "forbidden"));
    }
    radio999k::registration::validate_registration(registration, "codex-cloud", "codex-cloud")?;
    let peers = radio999k::registration::active_peers(registration, "codex-cloud")?;
    let policy = radio999k::events::policy::EventsPolicy::for_codex(
        entry.token_sha256.to_ascii_lowercase(),
        peers,
        settings.callback_hosts.clone(),
    )
    .map_err(|e| (503, e.reason))?;
    Ok(Some((settings, policy)))
}
/// Shared hub policies require owner-only files and parent directories.
// AUTH-006: protect policy and parent directories.
pub fn validate_private(path: &Path) -> Result<(), Fault> {
    load_generic_policy(path).map(|_| ())
}
// AUTH-007: extract the presented bearer token digest.
pub(crate) fn presented_token_digest(authorization: &[Vec<u8>]) -> Result<[u8; 32], Fault> {
    let bad = (401, "unauthorized");
    if authorization.len() != 1 {
        return Err(bad);
    }
    let value = std::str::from_utf8(&authorization[0]).map_err(|_| bad)?;
    let token = value.strip_prefix("Bearer ").ok_or(bad)?;
    if token.is_empty() || token.len() > 256 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad);
    }
    Ok(Sha256::digest(token.as_bytes()).into())
}

// AUTH-007: authenticate private callers.
pub fn authenticate_private(path: &Path, authorization: &[Vec<u8>]) -> Result<Principal, Fault> {
    authenticate_generic(load_generic_policy(path)?, authorization)
}
/// Preserve the live Cue policy without accepting its local Bearer on the hosted route.
pub(crate) fn active_hosted_cue_policy(path: &Path) -> Result<Principal, Fault> {
    let (entry, _) = load_generic_policy(path)?
        .into_iter()
        .find(|(entry, _)| entry.id == "cue" && !entry.revoked)
        .ok_or((403, "forbidden"))?;
    Ok(principal(entry))
}

fn principal(entry: Entry) -> Principal {
    Principal {
        id: entry.id,
        operations: entry.operations,
        send_to: entry.send_to,
        mailboxes: entry.mailboxes,
    }
}
fn authenticate_generic(
    entries: Vec<(Entry, [u8; 32])>,
    authorization: &[Vec<u8>],
) -> Result<Principal, Fault> {
    let bad = (401, "unauthorized");
    let presented = presented_token_digest(authorization)?;
    // Compare every entry; never use normal string equality for bearer verification.
    let mut matched = None;
    for (index, (entry, hash)) in entries.iter().enumerate() {
        let equal = hash.ct_eq(&presented).unwrap_u8();
        if equal == 1 && !entry.revoked {
            matched = Some(index);
        }
    }
    let index = matched.ok_or(bad)?;
    let (entry, _) = entries.into_iter().nth(index).ok_or(bad)?;
    Ok(principal(entry))
}

/// Send and Read use the same registered peers at a single write checkpoint.
pub(crate) fn check_registered_send(
    allowed: &[String],
    recipient: &str,
    thread_peers: Option<&[String]>,
    require_read: impl FnOnce() -> Result<(), Fault>,
) -> Result<(), Fault> {
    if !allowed.iter().any(|peer| peer == recipient) {
        return Err((403, "forbidden"));
    }
    if let Some(previous) = thread_peers {
        require_read()?;
        if allowed != previous {
            return Err((403, "thread_scope_changed"));
        }
    }
    Ok(())
}

// Muse has a separate policy schema and must never fall back to generic or dot auth.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MuseOperation {
    Read,
    Wait,
    Reply,
    Send,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MusePolicy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    // Record presence, not content. No value of the retired field is accepted.
    #[serde(
        default,
        rename = "senders",
        deserialize_with = "retired_senders_present"
    )]
    retired_senders: bool,
    operations: Vec<MuseOperation>,
    enabled: bool,
    expires_at_ms: u64,
    #[serde(default)]
    registration: Option<String>,
}

pub struct MusePrincipal {
    operations: Vec<MuseOperation>,
    expires_at_ms: u64,
    registration: Option<String>,
}

fn muse_now_ms() -> Result<u64, Fault> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| (503, "auth_unavailable"))?;
    u64::try_from(elapsed.as_millis()).map_err(|_| (503, "auth_unavailable"))
}

impl MusePrincipal {
    pub(crate) fn validate_pairing_registration(&self) -> Result<(), Fault> {
        let path = self
            .registration
            .as_deref()
            .ok_or((503, "registration_unavailable"))?;
        radio999k::registration::validate_registration(Path::new(path), "muse", "muse")
    }
    fn registered_peers(&self) -> Result<Vec<String>, Fault> {
        radio999k::registration::active_peers(
            Path::new(
                self.registration
                    .as_deref()
                    .ok_or((503, "registration_unavailable"))?,
            ),
            "muse",
        )
    }
    pub(crate) fn require_operation(&self, operation: MuseOperation) -> Result<(), Fault> {
        if muse_now_ms()? >= self.expires_at_ms {
            return Err((401, "unauthorized"));
        }
        if !self.operations.contains(&operation) {
            return Err((403, "forbidden"));
        }
        Ok(())
    }
    /// This is a request snapshot, never a cacheable authentication session.
    pub fn scope(
        &self,
        operation: MuseOperation,
    ) -> Result<radio999k::muse_mailbox::MuseScope, Fault> {
        self.require_operation(operation)?;
        Ok(radio999k::muse_mailbox::MuseScope {
            allowed_senders: self.registered_peers()?,
        })
    }
}

/// Open through a verified directory descriptor to avoid parent replacement races.
/// Only the immediate parent is constrained; ancestors remain trusted installation paths.
use radio999k::private_file::open_private_file;

/// Metadata check only; does not read database or credential contents.
pub fn require_private_file(path: &Path) -> Result<(), Fault> {
    open_private_file(path).map(|_| ())
}

fn load_muse(path: &Path) -> Result<(MusePrincipal, [u8; 32]), Fault> {
    let fail = (503, "auth_unavailable");
    let bytes = read_private_policy(path)?;
    let policy: MusePolicy = serde_json::from_slice(&bytes).map_err(|_| fail)?;
    if policy.retired_senders {
        return Err((503, RETIRED_SENDERS_CODE));
    }
    let hash = digest(&policy.token_sha256).ok_or(fail)?;
    let mut operations = HashSet::new();
    if policy.principal != "muse"
        || policy.mailbox != "muse"
        || policy.operations.is_empty()
        || !policy.operations.iter().all(|o| operations.insert(o))
    {
        return Err(fail);
    }
    if !policy.enabled || muse_now_ms()? >= policy.expires_at_ms {
        return Err((401, "unauthorized"));
    }
    Ok((
        MusePrincipal {
            operations: policy.operations,
            expires_at_ms: policy.expires_at_ms,
            registration: policy.registration,
        },
        hash,
    ))
}

pub fn validate_muse(path: &Path) -> Result<(), Fault> {
    let (principal, _) = load_muse(path)?;
    principal.registered_peers().map(|_| ())
}

pub fn authenticate_muse(path: &Path, authorization: &[Vec<u8>]) -> Result<MusePrincipal, Fault> {
    let (principal, hash) = load_muse(path).map_err(public_policy_fault)?;
    let presented = presented_token_digest(authorization)?;
    if hash.ct_eq(&presented).unwrap_u8() != 1 {
        return Err((401, "unauthorized"));
    }
    Ok(principal)
}

/// Trusted local installation only. Never use this as an HTTP authentication fallback.
pub fn local_muse_scope(
    path: &Path,
    operation: MuseOperation,
) -> Result<radio999k::muse_mailbox::MuseScope, Fault> {
    load_muse(path)?.0.scope(operation)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    const PUBLIC: &str = "public-muse-test-only-not-a-credential";

    fn policy() -> Value {
        json!({"principal":"muse", "mailbox":"muse",
            "token_sha256":Sha256::digest(PUBLIC.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "operations":["read","wait","reply"],
            "enabled":true, "expires_at_ms":u64::MAX})
    }
    fn write(path: &Path, value: Value) {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn directory() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn headers() -> Vec<Vec<u8>> {
        vec![format!("Bearer {PUBLIC}").into_bytes()]
    }
    #[test]
    fn generic_policy_accepts_all_unique_operations_and_rejects_invalid_sets() {
        let dir = directory();
        let path = dir.path().join("generic.json");
        let mut policy = json!({"principals":[{"id":"fixture","token_sha256":"0".repeat(64),
            "operations":["request","send","read","wait","reply","receive","respond","status","inspect_dlq"],
            "send_to":[],"mailboxes":[],"revoked":false}]});
        write(&path, policy.clone());
        assert_eq!(validate_private(&path), Ok(()));
        for operations in [
            json!([]),
            json!(["send", "send"]),
            json!(["not_an_operation"]),
        ] {
            policy["principals"][0]["operations"] = operations;
            write(&path, policy.clone());
            assert_eq!(validate_private(&path), Err((503, "auth_unavailable")));
        }
    }
    #[test]
    fn generic_policy_can_use_more_than_32_explicit_grants_within_the_file_limit() {
        let dir = directory();
        let path = dir.path().join("generic.json");
        let grants: Vec<_> = (0..33).map(|i| format!("mailbox-{i}")).collect();
        let mut principals: Vec<_> = (0..32)
            .map(|i| {
                json!({
                    "id":format!("other-{i}"), "token_sha256":format!("{i:064x}"),
                    "operations":["read"], "send_to":[], "mailboxes":[], "revoked":false
                })
            })
            .collect();
        principals.push(json!({
            "id":"fixture",
            "token_sha256":Sha256::digest(PUBLIC.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "operations":["send"], "send_to":grants, "mailboxes":grants, "revoked":false
        }));
        write(&path, json!({"principals":principals}));
        let p = authenticate_private(&path, &headers()).unwrap();
        assert_eq!(p.target("mailbox-32"), Ok(()));
        assert_eq!(p.mailbox("mailbox-32"), Ok(()));
        assert_eq!(p.target("mailbox-33"), Err((403, "forbidden")));
        assert_eq!(p.mailbox("mailbox-33"), Err((403, "forbidden")));
    }
    #[test]
    fn separate_muse_auth_and_operation_scope() {
        let dir = directory();
        let path = dir.path().join("public-muse.json");
        let mut value = policy();
        let registry = dir.path().join("registration.json");
        write(
            &registry,
            json!({"muse":{"mailbox":"muse","revoked":false},"dot":{"mailbox":"dot","revoked":false},"hermes":{"mailbox":"hermes","revoked":false}}),
        );
        value["registration"] = json!(registry);
        value["operations"] = json!(["read"]);
        write(&path, value);
        let principal = authenticate_muse(&path, &headers()).unwrap();
        assert_eq!(
            principal
                .scope(MuseOperation::Read)
                .unwrap()
                .allowed_senders,
            vec!["dot", "hermes"]
        );
        assert!(matches!(
            principal.scope(MuseOperation::Reply),
            Err((403, "forbidden"))
        ));
        assert!(validate_private(&path).is_err());
        assert!(authenticate_muse(&path, &[]).is_err());
        assert!(authenticate_muse(&path, &[headers()[0].clone(), headers()[0].clone()]).is_err());
        assert!(authenticate_muse(&path, &[b"Bearer wrong-public-fixture".to_vec()]).is_err());
        assert!(local_muse_scope(&path, MuseOperation::Read).is_ok());
        assert!(local_muse_scope(&path, MuseOperation::Wait).is_err());
    }
    #[test]
    fn muse_requires_current_central_registration_for_every_operation() {
        let dir = directory();
        let path = dir.path().join("policy.json");
        let registry = dir.path().join("registration.json");
        let mut value = policy();
        value["operations"] = json!(["read", "wait", "reply", "send"]);
        write(&path, value.clone());
        assert_eq!(validate_muse(&path), Err((503, "registration_unavailable")));
        for operation in [
            MuseOperation::Read,
            MuseOperation::Wait,
            MuseOperation::Reply,
            MuseOperation::Send,
        ] {
            assert!(matches!(
                local_muse_scope(&path, operation),
                Err((503, "registration_unavailable"))
            ));
        }
        value["registration"] = json!(registry);
        write(&path, value);
        let mut entries = json!({"muse":{"mailbox":"muse","revoked":false},"codex-cloud":{"mailbox":"codex-cloud","revoked":false}});
        write(&registry, entries.clone());
        let principal = authenticate_muse(&path, &headers()).unwrap();
        assert_eq!(
            principal
                .scope(MuseOperation::Read)
                .unwrap()
                .allowed_senders,
            vec!["codex-cloud"]
        );
        assert!(validate_muse(&path).is_ok());
        entries["codex-cloud"]["revoked"] = json!(true);
        write(&registry, entries.clone());
        assert!(
            principal
                .scope(MuseOperation::Read)
                .unwrap()
                .allowed_senders
                .is_empty()
        );
        entries["muse"]["revoked"] = json!(true);
        write(&registry, entries);
        assert_eq!(validate_muse(&path), Err((403, "forbidden")));
        assert!(matches!(
            principal.scope(MuseOperation::Read),
            Err((403, "forbidden"))
        ));
    }
    #[test]
    fn muse_uses_registry_peers_and_rejects_retired_senders() {
        let dir = directory();
        let path = dir.path().join("policy.json");
        let registry = dir.path().join("registration.json");
        for peer_count in [0, 33] {
            let mut entries = json!({"muse":{"mailbox":"muse","revoked":false}});
            for i in 0..peer_count {
                let name = format!("peer-{i}");
                entries[&name] = json!({"mailbox":name,"revoked":false});
            }
            write(&registry, entries);
            let mut value = policy();
            value["registration"] = json!(registry);
            write(&path, value.clone());
            let principal = authenticate_muse(&path, &headers()).unwrap();
            assert_eq!(
                principal
                    .scope(MuseOperation::Read)
                    .unwrap()
                    .allowed_senders
                    .len(),
                peer_count
            );
            for obsolete in [Value::Null, json!(["fixture-only"]), json!({})] {
                value["senders"] = obsolete;
                write(&path, value.clone());
                assert_eq!(validate_muse(&path), Err((503, RETIRED_SENDERS_CODE)));
                assert_eq!(
                    authenticate_muse(&path, &headers()).err(),
                    Some((503, "auth_unavailable"))
                );
            }
        }
    }
    #[test]
    fn policy_reload_rejects_disabled_expired_and_malformed() {
        let dir = directory();
        let path = dir.path().join("public-muse.json");
        write(&path, policy());
        assert!(authenticate_muse(&path, &headers()).is_ok());
        for (field, invalid) in [
            ("enabled", json!(false)),
            ("expires_at_ms", json!(0)),
            ("expires_at_ms", json!(-1)),
            ("expires_at_ms", json!(1.5)),
            ("principal", json!("dot")),
            ("mailbox", json!("dot")),
            ("operations", json!([])),
            ("operations", json!(["read", "read"])),
            (
                "operations",
                json!(["read", "wait", "reply", "send", "read"]),
            ),
            ("operations", json!(["unknown_operation"])),
            ("token_sha256", json!("not-a-digest")),
            ("unknown", json!(true)),
        ] {
            let mut value = policy();
            value[field] = invalid;
            write(&path, value);
            assert!(authenticate_muse(&path, &headers()).is_err());
        }
        let mut value = policy();
        value.as_object_mut().unwrap().remove("enabled");
        write(&path, value);
        assert!(validate_muse(&path).is_err());
        std::fs::write(&path, vec![b' '; MAX_POLICY as usize + 1]).unwrap();
        assert!(validate_muse(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(validate_muse(&path).is_err());
    }
    #[test]
    fn private_file_and_parent_reject_unsafe_paths() {
        let dir = directory();
        let path = dir.path().join("public-muse.json");
        write(&path, policy());
        assert!(require_private_file(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_muse(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(validate_muse(&link).is_err());
        let hard = dir.path().join("hard");
        std::fs::hard_link(&path, &hard).unwrap();
        assert!(validate_muse(&path).is_err());
        std::fs::remove_file(hard).unwrap();
        assert!(validate_muse(dir.path()).is_err());
        let fifo = dir.path().join("fifo");
        use std::os::unix::ffi::OsStrExt;
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated temporary fixture path.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(validate_muse(&fifo).is_err());
        let parent_link = dir.path().join("parent-link");
        std::os::unix::fs::symlink(dir.path(), &parent_link).unwrap();
        assert!(validate_muse(&parent_link.join("public-muse.json")).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_muse(&path).is_err());
    }
}
