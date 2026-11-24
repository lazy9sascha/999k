//! Readiness and owner-private Bearer staging for the existing built-in adapters.
use crate::{auth, codex_events, local_http, mailbox_startup::Binding};
use radio999k::{
    dot_profile,
    dot_service_preflight::DotEvents,
    mailbox_catalog::{Adapter, EventsCapability},
    private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file},
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const TOKEN_LEN: usize = 64;
const UNSUPPORTED: &str = "policy does not define one supported mailbox bearer";

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Read,
    Wait,
    Reply,
    Send,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DotPolicy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    enabled: bool,
    registration: PathBuf,
    #[serde(default)]
    events: Option<DotEvents>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedPolicy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    operations: Vec<Operation>,
    enabled: bool,
    expires_at_ms: u64,
    registration: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedGrant {
    id: String,
    token_sha256: String,
    operations: Vec<Operation>,
    send_to: Vec<String>,
    mailboxes: Vec<String>,
    revoked: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedPolicy {
    principals: [SharedGrant; 1],
    #[serde(default)]
    events: Option<auth::Events>,
}

fn ordinary_operations(operations: &[Operation]) -> bool {
    !operations.is_empty()
        && operations
            .iter()
            .enumerate()
            .all(|(index, op)| !operations[..index].contains(op))
}

fn valid_digest(value: &str) -> bool {
    value.len() == TOKEN_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn exact_registration(binding: &Binding, actual: &Path) -> bool {
    actual.is_absolute()
        && binding
            .registration
            .as_deref()
            .is_none_or(|expected| expected == actual)
}

fn read_private(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let mut bytes = Vec::new();
    open_private_file(path)
        .map_err(|_| io::Error::other("unsafe mailbox policy or staging file"))?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "mailbox policy or staging file is too large",
        ));
    }
    Ok(Some(bytes))
}

struct Template {
    value: Value,
    token_pointer: &'static str,
}

fn template(binding: &Binding) -> io::Result<Template> {
    let bytes = read_private(&binding.policy, MAX_PRIVATE_CONFIG_BYTES)?
        .ok_or_else(|| io::Error::other("mailbox policy is missing"))?;
    parse_template(binding, &bytes)
}

fn parse_template(binding: &Binding, bytes: &[u8]) -> io::Result<Template> {
    let unsupported = |_| io::Error::other(UNSUPPORTED);
    let mailbox = binding.descriptor.id;
    // Parse through strict typed policy shapes before cloning JSON so duplicate
    // fields, extra authorities, and unrecognized grants cannot be adopted.
    let token_pointer = match (binding.descriptor.adapter, mailbox) {
        (Adapter::Dot, "dot") => {
            let policy: DotPolicy = serde_json::from_slice(bytes).map_err(unsupported)?;
            if policy.mailbox != mailbox
                || !policy.enabled
                || !exact_registration(binding, &policy.registration)
                || policy.principal == "codex-cloud"
                || policy.principal.trim().is_empty()
                || policy.principal.len() > 128
                || policy.principal.chars().any(char::is_control)
                || !valid_digest(&policy.token_sha256)
                || policy
                    .token_sha256
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase())
            {
                return Err(io::Error::other(UNSUPPORTED));
            }
            if let Some(settings) = policy.events {
                if !settings.resolver.is_absolute() {
                    return Err(io::Error::other(UNSUPPORTED));
                }
                radio999k::events::policy::EventsPolicy::for_dot_mailbox(
                    policy.principal,
                    policy.token_sha256,
                    Vec::new(),
                    settings.callback_hosts,
                )
                .map_err(|_| io::Error::other(UNSUPPORTED))?;
            }
            "/token_sha256"
        }
        (Adapter::Muse, "muse") | (Adapter::Grok, "grok-bot") => {
            let policy: ScopedPolicy = serde_json::from_slice(bytes).map_err(unsupported)?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_millis();
            if policy.principal != mailbox
                || policy.mailbox != mailbox
                || !policy.enabled
                || now >= u128::from(policy.expires_at_ms)
                || !exact_registration(binding, &policy.registration)
                || !ordinary_operations(&policy.operations)
                || !valid_digest(&policy.token_sha256)
            {
                return Err(io::Error::other(UNSUPPORTED));
            }
            "/token_sha256"
        }
        (Adapter::SharedMailbox, "codex-cloud" | "cue" | "instinct") => {
            let policy: SharedPolicy = serde_json::from_slice(bytes).map_err(unsupported)?;
            let [grant] = policy.principals;
            if grant.id != mailbox
                || grant.mailboxes != [mailbox]
                || grant.revoked
                || !grant.send_to.is_empty()
                || !ordinary_operations(&grant.operations)
                || !valid_digest(&grant.token_sha256)
            {
                return Err(io::Error::other(UNSUPPORTED));
            }
            if let Some(settings) = policy.events {
                if mailbox != "codex-cloud"
                    || !settings.resolver.is_absolute()
                    || !grant.operations.contains(&Operation::Read)
                {
                    return Err(io::Error::other(UNSUPPORTED));
                }
                radio999k::events::policy::EventsPolicy::for_codex(
                    grant.token_sha256.to_ascii_lowercase(),
                    Vec::new(),
                    settings.callback_hosts,
                )
                .map_err(|_| io::Error::other(UNSUPPORTED))?;
            }
            "/principals/0/token_sha256"
        }
        _ => return Err(io::Error::other(UNSUPPORTED)),
    };
    Ok(Template {
        value: serde_json::from_slice(bytes).map_err(unsupported)?,
        token_pointer,
    })
}

/// Apply the serving adapter's readiness checks without provisioning or changing files.
pub(crate) fn validate(queue: &Path, binding: &Binding) -> io::Result<()> {
    template(binding)?;
    match binding.descriptor.adapter {
        Adapter::Dot => dot_profile::check_with_generation(
            queue,
            &binding.policy,
            false,
            binding.generation.as_deref(),
        )
        .map(|_| ()),
        Adapter::Muse if local_http::check_muse(queue, &binding.policy)? => Ok(()),
        Adapter::Grok if local_http::check_grok(queue, &binding.policy)? => Ok(()),
        Adapter::Muse | Adapter::Grok => Err(io::Error::other("mailbox storage is unprepared")),
        Adapter::SharedMailbox => {
            let profile = binding
                .mailbox(queue)
                .ok_or_else(|| io::Error::other("mailbox registration is missing"))?;
            profile
                .validate()
                .map_err(|(_, reason)| io::Error::other(reason))?;
            if binding
                .descriptor
                .mailbox
                .is_some_and(|profile| profile.events == EventsCapability::Codex)
                && let Some(runtime) = codex_events::Runtime::new_with_generation(
                    &profile,
                    binding.generation.as_deref(),
                )
                .map_err(|(_, reason)| io::Error::other(reason))?
            {
                runtime
                    .check()
                    .map_err(|fault| io::Error::other(fault.reason))?;
            }
            Ok(())
        }
        Adapter::Receiver => Err(io::Error::other(UNSUPPORTED)),
    }
}

fn staging_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::other("staging directory must be absolute"));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid takes no arguments.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other("owner-private staging directory required"));
    }
    Ok(directory)
}

fn create_staged(directory: &File, name: &std::ffi::CStr, bytes: &[u8]) -> io::Result<()> {
    // SAFETY: directory and the terminated name remain live for the call.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_int,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let written = (|| {
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if written.is_err() {
        // SAFETY: this call removes only the entry created by openat above.
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    }
    written?;
    directory.sync_all()
}

/// Clone the existing narrow policy and change only its Bearer digest.
/// The lifecycle controller serializes calls and publishes the files separately.
pub(crate) fn stage_replacement(dir: &Path, binding: &Binding) -> io::Result<(PathBuf, PathBuf)> {
    let mut template = template(binding)?;
    let directory = staging_directory(dir)?;
    let policy_path = dir.join("policy.json");
    let token_path = dir.join("bearer-token");
    let staged_policy = read_private(&policy_path, MAX_PRIVATE_CONFIG_BYTES)?;
    let (token, create_token) = match read_private(&token_path, TOKEN_LEN as u64)? {
        Some(token)
            if token.len() == TOKEN_LEN
                && token
                    .iter()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
        {
            (token, false)
        }
        Some(_) => return Err(io::Error::other("staged mailbox bearer is invalid")),
        None if staged_policy.is_some() => {
            return Err(io::Error::other(
                "staged mailbox policy is missing its bearer",
            ));
        }
        None => {
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            (token.into_bytes(), true)
        }
    };
    let digest: String = Sha256::digest(&token)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let former_digest = template
        .value
        .pointer_mut(template.token_pointer)
        .ok_or_else(|| io::Error::other(UNSUPPORTED))?;
    if former_digest
        .as_str()
        .is_some_and(|previous| previous.eq_ignore_ascii_case(&digest))
    {
        return Err(io::Error::other(
            "staged mailbox bearer repeats the previous authority",
        ));
    }
    *former_digest = Value::String(digest);

    if let Some(staged_policy) = staged_policy {
        // Reparse strictly before treating an existing credential as staged.
        if parse_template(binding, &staged_policy)?.value != template.value {
            return Err(io::Error::other(
                "staged mailbox policy differs from expected authority",
            ));
        }
    } else {
        let encoded = serde_json::to_vec(&template.value).map_err(io::Error::other)?;
        if create_token {
            create_staged(&directory, c"bearer-token", &token)?;
        }
        create_staged(&directory, c"policy.json", &encoded)?;
    }
    Ok((policy_path, token_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use radio999k::{Queue, mailbox_catalog::http_by_id, private_file::replace_private};
    use serde_json::json;
    use std::fs;

    fn private_dir(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
    }

    fn cue_binding(home: &Path, policy: Value) -> Binding {
        let policy_path = home.join("cue.json");
        let registration = home.join("registration.json");
        replace_private(&policy_path, &serde_json::to_vec(&policy).unwrap()).unwrap();
        replace_private(
            &registration,
            br#"{"cue":{"mailbox":"cue","revoked":false}}"#,
        )
        .unwrap();
        Binding {
            descriptor: http_by_id("cue").unwrap(),
            route: "/cue/mcp".into(),
            policy: policy_path,
            registration: Some(registration),
            wake_files_dir: None,
            generation: None,
        }
    }

    fn cue_policy() -> Value {
        json!({"principals":[{
            "id":"cue", "mailboxes":["cue"], "operations":["read","wait","reply","send"],
            "revoked":false, "send_to":[], "token_sha256":"f".repeat(64)
        }]})
    }

    #[test]
    fn replacement_preserves_the_policy_scope_and_reuses_only_its_own_staged_bearer() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), Permissions::from_mode(0o700)).unwrap();
        let policy = cue_policy();
        let binding = cue_binding(home.path(), policy.clone());
        let queue = home.path().join("queue");
        drop(Queue::open(&queue).unwrap());
        fs::set_permissions(&queue, Permissions::from_mode(0o600)).unwrap();
        validate(&queue, &binding).unwrap();
        let original = fs::read(&binding.policy).unwrap();
        let staged = home.path().join("staged");
        private_dir(&staged);
        let (staged_policy, token) = stage_replacement(&staged, &binding).unwrap();
        let first_token = fs::read(&token).unwrap();
        let first_policy = fs::read(&staged_policy).unwrap();
        let mut clone: Value = serde_json::from_slice(&first_policy).unwrap();
        assert_ne!(
            clone["principals"][0]["token_sha256"],
            policy["principals"][0]["token_sha256"]
        );
        clone["principals"][0]["token_sha256"] = policy["principals"][0]["token_sha256"].clone();
        assert_eq!(clone, policy);
        assert_eq!(fs::read(&binding.policy).unwrap(), original);
        assert_eq!(
            stage_replacement(&staged, &binding).unwrap(),
            (staged_policy.clone(), token.clone())
        );
        assert_eq!(fs::read(&token).unwrap(), first_token);
        assert_eq!(fs::read(&staged_policy).unwrap(), first_policy);
        assert_eq!(fs::metadata(&token).unwrap().mode() & 0o777, 0o600);

        // Removal can revoke the registration before the replacement is staged.
        replace_private(
            binding.registration.as_ref().unwrap(),
            br#"{"cue":{"mailbox":"cue","revoked":true}}"#,
        )
        .unwrap();
        assert!(validate(&queue, &binding).is_err());
        assert!(stage_replacement(&staged, &binding).is_ok());
    }

    #[test]
    fn rejects_broad_grants_and_never_overwrites_a_conflicting_stage() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), Permissions::from_mode(0o700)).unwrap();
        let mut broad = cue_policy();
        broad["principals"][0]["mailboxes"] = json!(["cue", "other"]);
        let binding = cue_binding(home.path(), broad);
        let staged = home.path().join("staged");
        private_dir(&staged);
        assert!(stage_replacement(&staged, &binding).is_err());
        assert_eq!(fs::read_dir(&staged).unwrap().count(), 0);

        let binding = cue_binding(home.path(), cue_policy());
        let (policy, token) = stage_replacement(&staged, &binding).unwrap();
        let bearer = fs::read(&token).unwrap();
        let mut altered: Value = serde_json::from_slice(&fs::read(&policy).unwrap()).unwrap();
        altered["principals"][0]["operations"] = json!(["read"]);
        let unexpected = serde_json::to_vec(&altered).unwrap();
        replace_private(&policy, &unexpected).unwrap();
        assert!(stage_replacement(&staged, &binding).is_err());
        assert_eq!(fs::read(&policy).unwrap(), unexpected);
        assert_eq!(fs::read(&token).unwrap(), bearer);
    }
}
