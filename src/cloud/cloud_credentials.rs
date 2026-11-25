//! Prepare Cloud bearers and explicit private handoffs. Agent use requires separate verification.
use radio999k::mailbox_catalog as catalog;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) const KEYCHAIN_SERVICE: &str = "PigeonMQ Cloud";

#[derive(clap::Args)]
pub(crate) struct CredentialOptions {
    #[arg(long)]
    home: PathBuf,
    #[arg(long, value_enum)]
    app: CredentialProfile,
    #[command(subcommand)]
    action: CredentialAction,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CredentialProfile {
    Muse,
    Dot,
    GrokBot,
    CodexCloud,
    CodexLocal,
}
impl CredentialProfile {
    fn http_profile(self) -> &'static catalog::HttpProfile {
        match self {
            Self::Muse => catalog::by_adapter(catalog::Adapter::Muse),
            Self::Dot => catalog::by_adapter(catalog::Adapter::Dot),
            Self::GrokBot => catalog::by_adapter(catalog::Adapter::Grok),
            Self::CodexCloud => catalog::codex_http(),
            Self::CodexLocal => catalog::by_adapter(catalog::Adapter::Receiver),
        }
    }
    fn slug(self) -> &'static str {
        self.http_profile().id
    }
    fn generic(self) -> bool {
        matches!(self, Self::CodexCloud | Self::CodexLocal)
    }
    fn policy_path(self, home: &Path) -> PathBuf {
        home.join(self.http_profile().policy_file)
    }
}

#[derive(clap::Subcommand)]
enum CredentialAction {
    /// Check local custody and policy matching. Does not verify agent receipt.
    Status,
    /// Recover a matching existing bearer. Never changes its policy or replaces another bearer.
    Import {
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Prepare an explicit private file for a native client or tunnel header reference.
    Export {
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long, value_enum, default_value = "token")]
        format: CredentialFormat,
    },
    /// Explicitly replace an unrecoverable digest, keeping grants and a durable repair token.
    Reissue {
        #[arg(long)]
        token_file: PathBuf,
        /// Explicitly select the existing local token as authority. Never replaces that stored token.
        #[arg(long)]
        use_stored_token: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CredentialFormat {
    Token,
    AuthorizationHeader,
}

trait SecretStore {
    fn get(&mut self, account: &str) -> io::Result<Option<Vec<u8>>>;
    fn put_missing(&mut self, account: &str, bearer: &[u8]) -> io::Result<()>;
}

fn valid_bearer(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > 256 || !bytes.iter().all(|b| (33..=126).contains(b)) {
        return Err(io::Error::other("invalid Cloud bearer"));
    }
    Ok(())
}

fn read_token_file(path: &Path) -> io::Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute private token path required"));
    }
    let file = radio999k::private_file::open_private_file(path)
        .map_err(|_| io::Error::other("private token file unavailable"))?;
    let mut bytes = Vec::new();
    file.take(259).read_to_end(&mut bytes)?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    valid_bearer(&bytes)?;
    Ok(bytes)
}

fn export_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions, Permissions},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            },
        },
    };
    if !path.is_absolute() {
        return Err(io::Error::other("absolute private export path required"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("private export directory required"))?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("export file name required"))?
            .as_bytes(),
    )
    .map_err(|_| io::Error::other("invalid export file name"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other("owner-private export directory required"));
    }
    // SAFETY: the verified directory descriptor and NUL-terminated name remain live.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        // SAFETY: open through the same directory. Never follow a link or block on a FIFO.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::other("existing export file is unsafe"));
        }
        // SAFETY: openat returned a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::other("existing export file is unsafe"));
        }
        let mut existing = Vec::new();
        (&mut file).take(264).read_to_end(&mut existing)?;
        return if existing == bytes {
            file.sync_all()?;
            directory.sync_all()
        } else {
            Err(io::Error::other(
                "existing export differs; no file replaced",
            ))
        };
    }
    // SAFETY: exclusive creation returned a new descriptor owned by this File.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    file.set_permissions(Permissions::from_mode(0o600))?;
    // A failed write or sync leaves this private file. Never truncate it on retry.
    file.write_all(bytes)?;
    file.sync_all()?;
    directory.sync_all()
}

fn credentials_with(
    options: &CredentialOptions,
    store: &mut impl SecretStore,
) -> io::Result<Value> {
    credentials_with_rng(options, store, random_repair_bearer)
}

fn random_repair_bearer() -> io::Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        keychain::random_repair_bearer()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(io::Error::other("Cloud bearer issuance requires macOS"))
    }
}

fn valid_repair_bearer(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(io::Error::other(
            "Repair token must contain exactly 64 hexadecimal bytes. No credential changed.",
        ));
    }
    Ok(())
}

fn read_repair_file(path: &Path) -> io::Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute private repair path required"));
    }
    let file = radio999k::private_file::open_private_file(path)
        .map_err(|_| io::Error::other("private repair file unavailable"))?;
    let mut bytes = Vec::new();
    file.take(65).read_to_end(&mut bytes)?;
    valid_repair_bearer(&bytes)?;
    Ok(bytes)
}

fn credentials_with_rng(
    options: &CredentialOptions,
    store: &mut impl SecretStore,
    mut generate: impl FnMut() -> io::Result<Vec<u8>>,
) -> io::Result<Value> {
    let policy_path = options.app.policy_path(&options.home);
    if options.app.generic() {
        crate::auth::validate_private(&policy_path)
            .map_err(|_| io::Error::other("valid private Codex bearer policy required"))?;
    }
    let policy_before = private_bytes(&policy_path)?;
    let policy: Value = serde_json::from_slice(&policy_before)
        .map_err(|_| io::Error::other("invalid Cloud JSON"))?;
    let principal = if options.app.generic() {
        policy["principals"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["id"] == options.app.slug())
            })
            .filter(|entry| {
                entry["revoked"] == false
                    && entry["mailboxes"].as_array().is_some_and(|boxes| {
                        boxes.iter().any(|mailbox| mailbox == options.app.slug())
                    })
            })
            .ok_or_else(|| io::Error::other("active exact Codex principal and mailbox required"))?
    } else {
        if policy["mailbox"] != options.app.slug() || policy["enabled"] != true {
            return Err(io::Error::other(
                "expected an enabled hub policy for this app",
            ));
        }
        &policy
    };
    let expected = principal["token_sha256"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            io::Error::other("existing Cloud bearer digest required; prepare the policy first")
        })?;
    let account = account_for(&options.home, options.app.slug())?;
    let local = store.get(&account)?;
    if private_bytes(&policy_path)? != policy_before {
        return Err(io::Error::other(
            "Policy changed during credential lookup. No credential action was applied.",
        ));
    }
    if let Some(bytes) = &local {
        valid_bearer(bytes)?;
    }
    let matches = local
        .as_ref()
        .is_some_and(|bytes| sha256(bytes).eq_ignore_ascii_case(expected));
    let mut status = serde_json::json!({"app":options.app.slug(),"local_available":local.is_some(),
        "matches_policy":matches,"agent_use":"unverified"});
    match &options.action {
        CredentialAction::Status => status["state"] = "local_credential_status".into(),
        CredentialAction::Import { token_file } => {
            let bearer = read_token_file(token_file)?;
            if !sha256(&bearer).eq_ignore_ascii_case(expected) {
                return Err(io::Error::other(
                    "imported bearer does not match policy; no credential changed",
                ));
            }
            if local.as_ref().is_some_and(|value| value != &bearer) {
                return Err(io::Error::other(
                    "stored bearer differs; no credential replaced",
                ));
            }
            store.put_missing(&account, &bearer)?;
            status["state"] = "credential_imported".into();
            status["local_available"] = true.into();
            status["matches_policy"] = true.into();
        }
        CredentialAction::Export { token_file, format } => {
            if !matches {
                return Err(io::Error::other(
                    "matching local bearer required; import the existing token without changing policy",
                ));
            }
            let bearer = local
                .as_deref()
                .ok_or_else(|| io::Error::other("local bearer unavailable"))?;
            let bytes = match format {
                CredentialFormat::Token => bearer.to_vec(),
                CredentialFormat::AuthorizationHeader => [b"Bearer ".as_slice(), bearer].concat(),
            };
            export_private(token_file, &bytes)?;
            status["state"] = "private_credential_file_prepared".into();
        }
        CredentialAction::Reissue {
            token_file,
            use_stored_token,
        } => {
            let candidate = match std::fs::symlink_metadata(token_file) {
                Ok(_) => Some(if *use_stored_token {
                    read_token_file(token_file)?
                } else {
                    read_repair_file(token_file)?
                }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if *use_stored_token && local.is_none() {
                return Err(io::Error::other(
                    "No local bearer exists for --use-stored-token. No credential changed.",
                ));
            }
            if local.is_some()
                && (candidate.is_some() || !use_stored_token)
                && candidate.as_ref() != local.as_ref()
            {
                return Err(io::Error::other(
                    "Stored bearer exists without this matching repair file. No credential replaced.",
                ));
            }
            let candidate = if *use_stored_token {
                local
                    .as_ref()
                    .ok_or_else(|| io::Error::other("local bearer required"))?
                    .clone()
            } else if let Some(bytes) = candidate {
                bytes
            } else {
                let bytes = generate()?;
                valid_repair_bearer(&bytes)?;
                bytes
            };
            let current_digest = sha256(&candidate);
            if options.app.generic()
                && policy["principals"].as_array().is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry["id"] != options.app.slug()
                            && entry["token_sha256"]
                                .as_str()
                                .is_some_and(|digest| digest.eq_ignore_ascii_case(&current_digest))
                    })
                })
            {
                return Err(io::Error::other(
                    "Repair token belongs to another principal. No credential changed.",
                ));
            }
            export_private(token_file, &candidate)?;
            // Keep the exact repair token before either durable credential destination changes.
            store.put_missing(&account, &candidate)?;
            if private_bytes(&policy_path)? != policy_before {
                return Err(io::Error::other(
                    "Policy changed during credential repair. The repair token is retained. Recheck before retrying.",
                ));
            }
            let changed = !current_digest.eq_ignore_ascii_case(expected);
            if changed {
                let mut updated = policy.clone();
                if options.app.generic() {
                    let entries = updated["principals"]
                        .as_array_mut()
                        .ok_or_else(|| io::Error::other("Codex principal list required"))?;
                    let selected = entries
                        .iter_mut()
                        .find(|entry| entry["id"] == options.app.slug())
                        .ok_or_else(|| io::Error::other("selected Codex principal required"))?;
                    selected["token_sha256"] = current_digest.into();
                } else {
                    updated["token_sha256"] = current_digest.into();
                }
                radio999k::private_file::replace_private(
                    &policy_path,
                    &serde_json::to_vec_pretty(&updated)?,
                )?;
            }
            status["state"] = "credential_reissued_pending_secure_field".into();
            status["local_available"] = true.into();
            status["matches_policy"] = true.into();
            status["policy_changed"] = changed.into();
        }
    }
    Ok(status)
}

#[cfg(target_os = "macos")]
struct KeychainStore;
#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn get(&mut self, account: &str) -> io::Result<Option<Vec<u8>>> {
        keychain::get(account)
    }
    fn put_missing(&mut self, account: &str, bearer: &[u8]) -> io::Result<()> {
        keychain::put_missing(account, bearer)
    }
}

pub(crate) fn run_credentials(options: &CredentialOptions) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        println!("{}", credentials_with(options, &mut KeychainStore)?);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = options;
        Err(io::Error::other(
            "Cloud credential storage requires macOS Keychain",
        ))
    }
}

pub(crate) fn local_custody(home: &Path, app: App) -> io::Result<Value> {
    #[cfg(target_os = "macos")]
    {
        credentials_with(
            &CredentialOptions {
                home: home.into(),
                app: match app {
                    App::Muse => CredentialProfile::Muse,
                    App::Dot => CredentialProfile::Dot,
                    App::GrokBot => CredentialProfile::GrokBot,
                },
                action: CredentialAction::Status,
            },
            &mut KeychainStore,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, app);
        Err(io::Error::other(
            "Cloud custody check requires macOS Keychain",
        ))
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum App {
    Muse,
    Dot,
    GrokBot,
}

impl App {
    pub(crate) fn for_profile(profile: &catalog::HttpProfile) -> Option<Self> {
        match profile.adapter {
            catalog::Adapter::Muse => Some(Self::Muse),
            catalog::Adapter::Dot => Some(Self::Dot),
            catalog::Adapter::Grok => Some(Self::GrokBot),
            _ => None,
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        self.http_profile().id
    }

    pub(crate) fn http_profile(self) -> &'static catalog::HttpProfile {
        match self {
            Self::Muse => catalog::by_adapter(catalog::Adapter::Muse),
            Self::Dot => catalog::by_adapter(catalog::Adapter::Dot),
            Self::GrokBot => catalog::by_adapter(catalog::Adapter::Grok),
        }
    }
}

fn private_json(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&private_bytes(path)?)
        .map_err(|_| io::Error::other("invalid Cloud JSON"))
}

fn private_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let file = radio999k::private_file::open_private_file(path)
        .map_err(|_| io::Error::other("private hub file unavailable"))?;
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err(io::Error::other("Hub file too large"));
    }
    Ok(bytes)
}

pub(crate) fn policy_path(home: &Path, app: App) -> std::path::PathBuf {
    home.join(app.http_profile().policy_file)
}

fn sha256(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn matching_bearer(expected: &str, bearer: &[u8]) -> bool {
    sha256(bearer).eq_ignore_ascii_case(expected)
}

fn require_local_custody(status: &Value) -> io::Result<()> {
    if status["local_available"] != true || status["matches_policy"] != true {
        return Err(io::Error::other(
            "Matching local bearer required. Use credentials status, then import a surviving token or explicitly reissue a lost token. No clipboard changed.",
        ));
    }
    Ok(())
}

pub(crate) fn account(home: &Path, app: App) -> io::Result<String> {
    account_for(home, app.slug())
}

fn account_for(home: &Path, slug: &str) -> io::Result<String> {
    use std::os::unix::ffi::OsStrExt;
    let path = home.canonicalize()?;
    let hash = sha256(path.as_os_str().as_bytes());
    Ok(format!("{slug}:{hash}"))
}

pub(crate) fn codex_setup_bearer(home: &Path, create: bool) -> io::Result<Option<Vec<u8>>> {
    #[cfg(target_os = "macos")]
    {
        let account = account_for(home, "codex-cloud")?;
        if create {
            keychain::get_or_create(&account).map(Some)
        } else {
            keychain::get(&account)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, create);
        Err(io::Error::other("Codex setup requires macOS Keychain"))
    }
}

/// Resolve only an explicitly marked fresh policy. Keep all grants and existing digests.
pub(crate) fn provision(policy: &Path, app: App) -> io::Result<()> {
    let value = private_json(policy)?;
    if matches!(app, App::Muse | App::GrokBot) && value.get("senders").is_some() {
        return Err(io::Error::other(crate::auth::RetiredSenders));
    }
    if value["token_sha256"] != "keychain" {
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err(io::Error::other("Keychain provisioning requires macOS"))
    }
    #[cfg(target_os = "macos")]
    {
        let mut value = value;
        if value["mailbox"] != app.slug() || value["enabled"] != true {
            return Err(io::Error::other(
                "expected an enabled hub policy for this app",
            ));
        }
        let home = policy
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if policy.file_name() != policy_path(home, app).file_name() {
            return Err(io::Error::other(
                "Keychain requires the standard hub policy name",
            ));
        }
        // Local issuance only. Explicit export or secure-field entry does not prove agent use.
        // FIXME: remote-agent token delivery is undecided. See .local/design/token-passing.md.
        let bearer = keychain::get_or_create(&account(home, app)?)?;
        value["token_sha256"] = sha256(&bearer).into();
        radio999k::private_file::replace_private(policy, &serde_json::to_vec_pretty(&value)?)
    }
}

#[cfg(target_os = "macos")]
// NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
pub(crate) fn read_keychain_bearer(home: &Path, app: App) -> io::Result<Vec<u8>> {
    keychain::get(&account(home, app)?)?
        .ok_or_else(|| io::Error::other("No Cloud bearer in Keychain"))
}

/// Read only the paired webhook item for this queue home.
pub(crate) fn read_grok_wake_keychain(home: &Path) -> io::Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        let account = account_for(home, "grok-wake")?;
        keychain::get_raw(&account, radio999k::grok_wake::MAX_KEYCHAIN_PAIR)?
            .ok_or_else(|| io::Error::other("No Grok webhook in Keychain"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Err(io::Error::other("Grok webhook Keychain requires macOS"))
    }
}

#[cfg(any(target_os = "macos", test))]
fn import_grok_wake_pair(home: &Path, expected_host: &str) -> io::Result<Vec<u8>> {
    use radio999k::grok_wake;
    let url = grok_wake::read_wake_file(&home.join(grok_wake::URL_FILE))?;
    let target = grok_wake::parse_url(&url).map_err(io::Error::other)?;
    if !target
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case(expected_host))
        || target.port_or_known_default() != Some(443)
    {
        return Err(io::Error::other(
            "Grok webhook does not match the approved HTTPS host",
        ));
    }
    let auth = grok_wake::read_wake_file(&home.join(grok_wake::AUTH_FILE))?;
    grok_wake::encode_keychain_pair(url.trim(), auth.trim()).map_err(io::Error::other)
}

#[cfg(test)]
#[test]
fn grok_wake_file_import_checks_host_and_descriptor_without_changing_sources() {
    use radio999k::grok_wake::{AUTH_FILE, KEYCHAIN_MARKER, URL_FILE, encode_keychain_pair};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let url = dir.path().join(URL_FILE);
    let auth = dir.path().join(AUTH_FILE);
    let original_url = b"https://wake.invalid/routine\n";
    let original_auth = b"Authorization: Bearer synthetic\n";
    radio999k::private_file::replace_private(&url, original_url).unwrap();
    radio999k::private_file::replace_private(&auth, original_auth).unwrap();
    assert_eq!(
        import_grok_wake_pair(dir.path(), "wake.invalid").unwrap(),
        encode_keychain_pair(
            "https://wake.invalid/routine",
            "Authorization: Bearer synthetic"
        )
        .unwrap()
    );
    assert!(import_grok_wake_pair(dir.path(), "other.invalid").is_err());
    assert_eq!(fs::read(&url).unwrap(), original_url);
    assert_eq!(fs::read(&auth).unwrap(), original_auth);
    assert!(!dir.path().join(KEYCHAIN_MARKER).exists());
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(import_grok_wake_pair(dir.path(), "wake.invalid").is_err());
    let other = dir.path().join("other");
    fs::rename(&auth, &other).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&other, &auth).unwrap();
    assert!(import_grok_wake_pair(dir.path(), "wake.invalid").is_err());
    fs::remove_file(&auth).unwrap();
    symlink(&other, &auth).unwrap();
    assert!(import_grok_wake_pair(dir.path(), "wake.invalid").is_err());
}

/// Set up the paired item. Neither secret is accepted through argv.
pub(crate) fn setup_grok_wake(
    home: &Path,
    from_private_files: Option<&str>,
    stage_only: bool,
) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(home)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "Grok webhook requires an owner-private queue directory",
            ));
        }
        let pair = if let Some(expected_host) = from_private_files {
            import_grok_wake_pair(home, expected_host)?
        } else {
            let url = hidden_tty(c"Grok webhook HTTPS URL: ")?;
            let auth = hidden_tty(c"Grok webhook Authorization: ")?;
            radio999k::grok_wake::encode_keychain_pair(url.trim(), auth.trim())
                .map_err(io::Error::other)?
        };
        let account = account_for(home, "grok-wake")?;
        keychain::put_raw_missing(&account, &pair, radio999k::grok_wake::MAX_KEYCHAIN_PAIR)?;
        if stage_only {
            println!("Grok webhook item staged in Keychain. Wake selection did not change.");
            return Ok(());
        }
        radio999k::private_file::replace_private(
            &home.join(radio999k::grok_wake::KEYCHAIN_MARKER),
            b"keychain\n",
        )?;
        println!("Grok webhook stored in Keychain. Restart the hub to select it.");
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, from_private_files, stage_only);
        Err(io::Error::other("Grok webhook Keychain requires macOS"))
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn hidden_tty(prompt: &std::ffi::CStr) -> io::Result<String> {
    use std::ffi::{c_char, c_int};
    unsafe extern "C" {
        fn readpassphrase(
            prompt: *const c_char,
            buf: *mut c_char,
            size: usize,
            flags: c_int,
        ) -> *mut c_char;
    }
    // Darwin readpassphrase.h: echo is off by default; REQUIRE_TTY forbids stdin fallback.
    const RPP_REQUIRE_TTY: c_int = 0x02;
    let mut buf = [0u8; 2048];
    // SAFETY: prompt and buffer remain live during the call and sizes are exact.
    let result = unsafe {
        readpassphrase(
            prompt.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            RPP_REQUIRE_TTY,
        )
    };
    let value = if result.is_null() {
        Err(io::Error::other("Hidden input requires a local terminal"))
    } else {
        std::ffi::CStr::from_bytes_until_nul(&buf)
            .ok()
            .filter(|text| text.to_bytes().len() < buf.len() - 1)
            .and_then(|text| text.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("Invalid or too-long hidden input"))
    };
    buf.fill(0);
    value
}

fn copy(bytes: &[u8]) -> io::Result<()> {
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let wrote = child.stdin.take().expect("piped stdin").write_all(bytes);
    let status = child.wait()?;
    wrote?;
    if !status.success() {
        return Err(io::Error::other("cannot copy to the macOS clipboard"));
    }
    Ok(())
}

pub(crate) fn handoff(home: &Path, app: App, copy_token: bool) -> io::Result<()> {
    let policy = private_json(&policy_path(home, app))?;
    if policy["mailbox"] != app.slug() || policy["enabled"] != true {
        return Err(io::Error::other(
            "expected an enabled hub policy for this app",
        ));
    }
    if copy_token {
        #[cfg(not(target_os = "macos"))]
        return Err(io::Error::other("Keychain handoff requires macOS"));
        #[cfg(target_os = "macos")]
        {
            let bearer = keychain::get(&account(home, app)?)?
                .ok_or_else(|| io::Error::other("no Keychain bearer for this Cloud app"))?;
            if !policy["token_sha256"]
                .as_str()
                .is_some_and(|expected| matching_bearer(expected, &bearer))
            {
                return Err(io::Error::other(
                    "Keychain bearer does not match the hub policy",
                ));
            }
            copy(&bearer)?;
            println!(
                "copied_pending_secure_field: paste only into the app's secure credential field. Agent use remains unverified."
            );
            return Ok(());
        }
    }

    require_local_custody(&local_custody(home, app)?)?;
    let state = private_json(&home.join("pigeonmq-cloud-state.json"))?;
    let origin = crate::cloud_transport::handoff_origin(&state).ok_or_else(|| {
        io::Error::other(
            "automatic handoff requires a connected built-in tunnel. For an external HTTPS origin, configure the connector manually",
        )
    })?;
    let endpoint = format!("{origin}{}", app.http_profile().route);
    let secret_input = match app {
        App::Muse => "Ask me to enter the Bearer token in the Secure Credentials Store.",
        App::Dot | App::GrokBot => {
            "If the app offers a secure Bearer token field, ask me to enter it there. Otherwise report that this connection method is unavailable."
        }
    };
    copy(format!(
        "Set up a new MCP connector named 999k-{} at {endpoint}. Keep any existing 999K connector unchanged. {secret_input} Never ask me to send the token in chat. After setup, list the connector tools and report whether it connected. Do not send or acknowledge a message.",
        app.slug()
    ).as_bytes())?;
    println!("Copied the connector prompt.");
    Ok(())
}

#[cfg(target_os = "macos")]
mod keychain;

#[cfg(all(test, unix))]
mod credential_tests {
    use super::*;
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

    const TOKEN: &[u8] = b"PUBLIC_SYNTHETIC_CREDENTIAL_TEST_ONLY";

    #[derive(Default)]
    struct MemoryStore {
        items: BTreeMap<String, Vec<u8>>,
        writes: usize,
    }
    impl SecretStore for MemoryStore {
        fn get(&mut self, account: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(self.items.get(account).cloned())
        }
        fn put_missing(&mut self, account: &str, bearer: &[u8]) -> io::Result<()> {
            if let Some(stored) = self.items.get(account) {
                if stored != bearer {
                    return Err(io::Error::other("fixture refuses overwrite"));
                }
            } else {
                self.items.insert(account.into(), bearer.into());
                self.writes += 1;
            }
            Ok(())
        }
    }
    struct HookStore<F> {
        inner: MemoryStore,
        put: F,
    }
    impl<F: FnMut(&mut MemoryStore, &str, &[u8]) -> io::Result<()>> SecretStore for HookStore<F> {
        fn get(&mut self, account: &str) -> io::Result<Option<Vec<u8>>> {
            self.inner.get(account)
        }
        fn put_missing(&mut self, account: &str, bearer: &[u8]) -> io::Result<()> {
            (self.put)(&mut self.inner, account, bearer)
        }
    }
    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn fixture(app: CredentialProfile) -> (tempfile::TempDir, CredentialOptions) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let policy = if app.generic() {
            serde_json::json!({"principals":[
                {"id":app.slug(),"token_sha256":sha256(TOKEN),"operations":["read","wait","reply","send"],
                    "mailboxes":[app.slug()],"send_to":["peer"],"revoked":false},
                {"id":"other","token_sha256":sha256(b"OTHER_PUBLIC_FIXTURE"),"operations":["read"],
                    "mailboxes":["other"],"send_to":[],"revoked":false}],
                "events":{"callback_hosts":["callback.example.com"],"resolver":"/fixture/resolver","allow_nonexpiring":false}})
        } else {
            serde_json::json!({
                "principal":app.slug(),"mailbox":app.slug(),"enabled":true,
                "token_sha256":sha256(TOKEN),"operations":["read","wait","reply","send"],
                "expires_at_ms":9999999999999u64,
            })
        };
        write(&app.policy_path(dir.path()), policy.to_string().as_bytes());
        let options = CredentialOptions {
            home: dir.path().into(),
            app,
            action: CredentialAction::Status,
        };
        (dir, options)
    }
    fn stored(options: &CredentialOptions) -> MemoryStore {
        let mut store = MemoryStore::default();
        store.items.insert(
            account_for(&options.home, options.app.slug()).unwrap(),
            TOKEN.into(),
        );
        store
    }

    #[test]
    fn status_distinguishes_local_custody_from_policy_matching_and_agent_use() {
        let (_dir, options) = fixture(CredentialProfile::Muse);
        let mut store = MemoryStore::default();
        let absent = credentials_with(&options, &mut store).unwrap();
        assert_eq!(absent["local_available"], false);
        assert_eq!(absent["matches_policy"], false);
        store.items.insert(
            account_for(&options.home, options.app.slug()).unwrap(),
            b"OTHER_PUBLIC_FIXTURE".to_vec(),
        );
        let mismatch = credentials_with(&options, &mut store).unwrap();
        assert_eq!(mismatch["local_available"], true);
        assert_eq!(mismatch["matches_policy"], false);
        let result = credentials_with(&options, &mut stored(&options)).unwrap();
        assert_eq!(result["matches_policy"], true);
        assert_eq!(result["agent_use"], "unverified");
        assert!(
            !result
                .to_string()
                .contains(std::str::from_utf8(TOKEN).unwrap())
        );
        assert!(!result.to_string().contains(&sha256(TOKEN)));
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn import_recovers_only_matching_bearer_and_preserves_policy_and_store() {
        for app in [
            CredentialProfile::Muse,
            CredentialProfile::Dot,
            CredentialProfile::GrokBot,
        ] {
            let (dir, mut options) = fixture(app);
            let policy = app.policy_path(dir.path());
            let before = fs::read(&policy).unwrap();
            let input = dir.path().join("input-token");
            write(&input, TOKEN);
            options.action = CredentialAction::Import {
                token_file: input.clone(),
            };
            let mut store = MemoryStore::default();
            for _ in 0..2 {
                let result = credentials_with(&options, &mut store).unwrap();
                assert_eq!(result["state"], "credential_imported");
                assert_eq!(result["agent_use"], "unverified");
            }
            assert_eq!(store.writes, 1);
            assert_eq!(fs::read(&policy).unwrap(), before);
            for ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
                write(&input, &[TOKEN, ending].concat());
                assert_eq!(
                    credentials_with(&options, &mut store).unwrap()["matches_policy"],
                    true
                );
            }
            assert_eq!(store.writes, 1);
            write(&input, b"OTHER_PUBLIC_FIXTURE");
            assert!(credentials_with(&options, &mut store).is_err());
            write(&input, TOKEN);
            let account = account_for(dir.path(), app.slug()).unwrap();
            store
                .items
                .insert(account.clone(), b"EXISTING_DIFFERENT_FIXTURE".to_vec());
            assert!(credentials_with(&options, &mut store).is_err());
            assert_eq!(store.items[&account], b"EXISTING_DIFFERENT_FIXTURE");
            assert_eq!(store.writes, 1);
            assert_eq!(fs::read(&policy).unwrap(), before);
        }
    }

    #[test]
    fn import_rejects_unsafe_paths_and_invalid_bearers_before_storage() {
        let (dir, mut options) = fixture(CredentialProfile::Muse);
        let input = dir.path().join("input-token");
        let mut store = MemoryStore::default();
        options.action = CredentialAction::Import {
            token_file: input.clone(),
        };
        for bytes in [
            b"".to_vec(),
            vec![b'a'; 257],
            b"has space".to_vec(),
            b"has\nnewline".to_vec(),
            vec![255],
        ] {
            write(&input, &bytes);
            assert!(credentials_with(&options, &mut store).is_err());
        }
        let maximum = vec![b'x'; 256];
        for ending in [b"".as_slice(), b"\n".as_slice(), b"\r\n".as_slice()] {
            write(&input, &[maximum.as_slice(), ending].concat());
            assert_eq!(read_token_file(&input).unwrap(), maximum);
        }
        write(&input, TOKEN);
        fs::set_permissions(&input, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(credentials_with(&options, &mut store).is_err());
        fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("linked-token");
        std::os::unix::fs::symlink(&input, &link).unwrap();
        options.action = CredentialAction::Import { token_file: link };
        assert!(credentials_with(&options, &mut store).is_err());
        let hard = dir.path().join("hard-token");
        fs::hard_link(&input, &hard).unwrap();
        options.action = CredentialAction::Import { token_file: hard };
        assert!(credentials_with(&options, &mut store).is_err());
        let public = dir.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        let public_token = public.join("token");
        write(&public_token, TOKEN);
        options.action = CredentialAction::Import {
            token_file: public_token,
        };
        assert!(credentials_with(&options, &mut store).is_err());
        options.action = CredentialAction::Import {
            token_file: "relative-token".into(),
        };
        assert!(credentials_with(&options, &mut store).is_err());
        assert!(store.items.is_empty());
    }

    #[test]
    fn export_prepares_private_native_and_header_files_without_replacement_or_receipt_claim() {
        let (dir, mut options) = fixture(CredentialProfile::Muse);
        let policy = options.app.policy_path(dir.path());
        let before = fs::read(&policy).unwrap();
        let mut store = stored(&options);
        for (name, format, expected) in [
            ("native-token", CredentialFormat::Token, TOKEN.to_vec()),
            (
                "tunnel-header",
                CredentialFormat::AuthorizationHeader,
                [b"Bearer ".as_slice(), TOKEN].concat(),
            ),
        ] {
            let output = dir.path().join(name);
            options.action = CredentialAction::Export {
                token_file: output.clone(),
                format,
            };
            for _ in 0..2 {
                let result = credentials_with(&options, &mut store).unwrap();
                assert_eq!(result["state"], "private_credential_file_prepared");
                assert_eq!(result["agent_use"], "unverified");
                assert!(
                    !result
                        .to_string()
                        .contains(std::str::from_utf8(TOKEN).unwrap())
                );
            }
            assert_eq!(fs::read(&output).unwrap(), expected);
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
            write(&output, b"DO_NOT_REPLACE");
            assert!(credentials_with(&options, &mut store).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"DO_NOT_REPLACE");
        }
        assert_eq!(fs::read(&policy).unwrap(), before);
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn export_requires_matching_store_and_rejects_links_and_public_directories() {
        let (dir, mut options) = fixture(CredentialProfile::Muse);
        let output = dir.path().join("output");
        options.action = CredentialAction::Export {
            token_file: output.clone(),
            format: CredentialFormat::Token,
        };
        assert!(credentials_with(&options, &mut MemoryStore::default()).is_err());
        assert!(!output.exists());
        let victim = dir.path().join("victim");
        write(&victim, b"UNCHANGED");
        std::os::unix::fs::symlink(&victim, &output).unwrap();
        assert!(credentials_with(&options, &mut stored(&options)).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"UNCHANGED");
        fs::remove_file(&output).unwrap();
        fs::hard_link(&victim, &output).unwrap();
        assert!(credentials_with(&options, &mut stored(&options)).is_err());
        let public = dir.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        options.action = CredentialAction::Export {
            token_file: public.join("output"),
            format: CredentialFormat::Token,
        };
        assert!(credentials_with(&options, &mut stored(&options)).is_err());
        assert!(!public.join("output").exists());
    }

    #[test]
    fn codex_profiles_import_check_and_export_without_changing_generic_policy() {
        for app in [CredentialProfile::CodexCloud, CredentialProfile::CodexLocal] {
            let (dir, mut options) = fixture(app);
            let path = app.policy_path(dir.path());
            let before = fs::read(&path).unwrap();
            let input = dir.path().join("input-token");
            write(&input, &[TOKEN, b"\n"].concat());
            let mut store = MemoryStore::default();
            let absent = credentials_with(&options, &mut store).unwrap();
            assert_eq!(absent["local_available"], false);
            options.action = CredentialAction::Import { token_file: input };
            assert_eq!(
                credentials_with(&options, &mut store).unwrap()["matches_policy"],
                true
            );
            options.action = CredentialAction::Status;
            let status = credentials_with(&options, &mut store).unwrap();
            assert_eq!(status["app"], app.slug());
            assert_eq!(status["local_available"], true);
            assert_eq!(status["agent_use"], "unverified");
            assert!(
                !status
                    .to_string()
                    .contains(std::str::from_utf8(TOKEN).unwrap())
            );
            assert!(!status.to_string().contains(&sha256(TOKEN)));
            for format in [
                CredentialFormat::Token,
                CredentialFormat::AuthorizationHeader,
            ] {
                let name = match format {
                    CredentialFormat::Token => "native-token",
                    CredentialFormat::AuthorizationHeader => "tunnel-header",
                };
                let output = dir.path().join(name);
                options.action = CredentialAction::Export {
                    token_file: output.clone(),
                    format,
                };
                assert_eq!(
                    credentials_with(&options, &mut store).unwrap()["agent_use"],
                    "unverified"
                );
                let expected = match format {
                    CredentialFormat::Token => TOKEN.to_vec(),
                    CredentialFormat::AuthorizationHeader => {
                        [b"Bearer ".as_slice(), TOKEN].concat()
                    }
                };
                assert_eq!(fs::read(output).unwrap(), expected);
            }
            assert_eq!(store.writes, 1);
            assert_eq!(fs::read(path).unwrap(), before);
            assert!(
                store
                    .items
                    .contains_key(&account_for(dir.path(), app.slug()).unwrap())
            );
        }
    }

    #[test]
    fn codex_credentials_require_exact_active_principal_and_matching_mailbox_schema() {
        for app in [CredentialProfile::CodexCloud, CredentialProfile::CodexLocal] {
            let (dir, options) = fixture(app);
            let path = app.policy_path(dir.path());
            let policy = private_json(&path).unwrap();
            for (field, value) in [
                ("id", serde_json::json!("foreign-profile")),
                ("revoked", serde_json::json!(true)),
                ("mailboxes", serde_json::json!(["wrong-mailbox"])),
                ("operations", serde_json::json!(["unknown-operation"])),
            ] {
                let mut changed = policy.clone();
                changed["principals"][0][field] = value;
                write(&path, changed.to_string().as_bytes());
                assert!(credentials_with(&options, &mut stored(&options)).is_err());
            }
            write(
                &path,
                serde_json::json!({"principal":app.slug(),"mailbox":app.slug(),
                "enabled":true,"token_sha256":sha256(TOKEN)})
                .to_string()
                .as_bytes(),
            );
            assert!(credentials_with(&options, &mut stored(&options)).is_err());
        }
    }

    #[test]
    fn reissue_saves_one_token_first_and_changes_only_the_selected_digest_for_every_profile() {
        for app in [
            CredentialProfile::Muse,
            CredentialProfile::Dot,
            CredentialProfile::GrokBot,
            CredentialProfile::CodexCloud,
            CredentialProfile::CodexLocal,
        ] {
            let (dir, mut options) = fixture(app);
            let path = app.policy_path(dir.path());
            let before = private_json(&path).unwrap();
            let registration = dir.path().join("registration.json");
            write(
                &registration,
                b"{\"peer\":{\"mailbox\":\"peer\",\"revoked\":false}}",
            );
            let registration_before = fs::read(&registration).unwrap();
            let file = dir.path().join("repair-token");
            options.action = CredentialAction::Reissue {
                token_file: file.clone(),
                use_stored_token: false,
            };
            let repair = vec![b'a'; 64];
            let calls = std::cell::Cell::new(0);
            let mut store = HookStore {
                inner: MemoryStore::default(),
                put: |inner: &mut MemoryStore, account: &str, bearer: &[u8]| {
                    assert_eq!(fs::read(&file).unwrap(), bearer);
                    inner.put_missing(account, bearer)
                },
            };
            let result = credentials_with_rng(&options, &mut store, || {
                calls.set(calls.get() + 1);
                Ok(repair.clone())
            })
            .unwrap();
            assert_eq!(result["state"], "credential_reissued_pending_secure_field");
            assert_eq!(result["policy_changed"], true);
            assert_eq!(result["agent_use"], "unverified");
            assert!(
                !result
                    .to_string()
                    .contains(std::str::from_utf8(&repair).unwrap())
            );
            let mut expected = before;
            if app.generic() {
                expected["principals"][0]["token_sha256"] = sha256(&repair).into();
            } else {
                expected["token_sha256"] = sha256(&repair).into();
            }
            assert_eq!(private_json(&path).unwrap(), expected);
            assert_eq!(fs::read(&registration).unwrap(), registration_before);
            let committed = fs::read(&path).unwrap();
            assert_eq!(
                credentials_with_rng(&options, &mut store, || panic!(
                    "must reuse saved repair token"
                ))
                .unwrap()["policy_changed"],
                false
            );
            assert_eq!(fs::read(path).unwrap(), committed);
            assert_eq!(store.inner.writes, 1);
            assert_eq!(calls.get(), 1);
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn reissue_resumes_the_same_candidate_after_store_failures_without_new_randomness() {
        for after_insert in [false, true] {
            let (dir, mut options) = fixture(CredentialProfile::GrokBot);
            let path = options.app.policy_path(dir.path());
            let before = fs::read(&path).unwrap();
            let file = dir.path().join("repair-token");
            options.action = CredentialAction::Reissue {
                token_file: file.clone(),
                use_stored_token: false,
            };
            let mut first = true;
            let mut store = HookStore {
                inner: MemoryStore::default(),
                put: |inner: &mut MemoryStore, account: &str, bearer: &[u8]| {
                    assert_eq!(fs::read(&file).unwrap(), bearer);
                    if std::mem::take(&mut first) {
                        if after_insert {
                            inner.put_missing(account, bearer)?;
                        }
                        return Err(io::Error::other("synthetic store interruption"));
                    }
                    inner.put_missing(account, bearer)
                },
            };
            assert!(credentials_with_rng(&options, &mut store, || Ok(vec![b'a'; 64])).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fs::read(&file).unwrap(), vec![b'a'; 64]);
            assert_eq!(
                credentials_with_rng(&options, &mut store, || panic!("no second token")).unwrap()["matches_policy"],
                true
            );
            assert_eq!(store.inner.writes, 1);
        }
    }

    #[test]
    fn reissue_preserves_a_concurrently_changed_policy_and_can_resume_its_exact_candidate() {
        let (dir, mut options) = fixture(CredentialProfile::GrokBot);
        let path = options.app.policy_path(dir.path());
        let mut changed = private_json(&path).unwrap();
        changed["operations"] = serde_json::json!(["read"]);
        let file = dir.path().join("repair-token");
        options.action = CredentialAction::Reissue {
            token_file: file,
            use_stored_token: false,
        };
        let mut change = true;
        let mut store = HookStore {
            inner: MemoryStore::default(),
            put: |inner: &mut MemoryStore, account: &str, bearer: &[u8]| {
                inner.put_missing(account, bearer)?;
                if std::mem::take(&mut change) {
                    write(&path, changed.to_string().as_bytes());
                }
                Ok(())
            },
        };
        assert!(credentials_with_rng(&options, &mut store, || Ok(vec![b'a'; 64])).is_err());
        assert_eq!(private_json(&path).unwrap(), changed);
        credentials_with_rng(&options, &mut store, || panic!("reuse candidate")).unwrap();
        let mut expected = changed.clone();
        expected["token_sha256"] = sha256(&[b'a'; 64]).into();
        assert_eq!(private_json(&path).unwrap(), expected);
    }

    #[test]
    fn reissue_rejects_partial_candidates_invalid_rng_and_other_principal_token_collisions() {
        let (dir, mut options) = fixture(CredentialProfile::GrokBot);
        let policy = options.app.policy_path(dir.path());
        let before = fs::read(&policy).unwrap();
        let file = dir.path().join("repair-token");
        options.action = CredentialAction::Reissue {
            token_file: file.clone(),
            use_stored_token: false,
        };
        let mut store = MemoryStore::default();
        write(&file, b"aaaa");
        assert!(
            credentials_with_rng(&options, &mut store, || panic!(
                "partial file must not generate"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&file).unwrap(), b"aaaa");
        assert_eq!(fs::read(&policy).unwrap(), before);
        assert_eq!(store.writes, 0);
        fs::remove_file(&file).unwrap();
        assert!(credentials_with_rng(&options, &mut store, || Ok(b"incomplete".to_vec())).is_err());
        assert!(!file.exists());
        assert_eq!(store.writes, 0);
        for app in [CredentialProfile::CodexCloud, CredentialProfile::CodexLocal] {
            let (dir, mut options) = fixture(app);
            let policy = app.policy_path(dir.path());
            let mut value = private_json(&policy).unwrap();
            value["principals"][1]["token_sha256"] = sha256(&[b'a'; 64]).into();
            write(&policy, value.to_string().as_bytes());
            let before = fs::read(&policy).unwrap();
            let file = dir.path().join("repair-token");
            write(&file, &[b'a'; 64]);
            options.action = CredentialAction::Reissue {
                token_file: file,
                use_stored_token: false,
            };
            assert!(
                credentials_with_rng(&options, &mut store, || panic!("reuse file only")).is_err()
            );
            assert_eq!(fs::read(policy).unwrap(), before);
            assert_eq!(store.writes, 0);
        }
    }

    #[test]
    fn mismatching_stored_token_requires_explicit_selection_and_is_never_replaced() {
        let (dir, mut options) = fixture(CredentialProfile::GrokBot);
        let policy = options.app.policy_path(dir.path());
        let original = private_json(&policy).unwrap();
        let mut mismatched = original.clone();
        mismatched["token_sha256"] = "f".repeat(64).into();
        write(&policy, mismatched.to_string().as_bytes());
        let mut store = stored(&options);
        let original_store = store.items.clone();
        let file = dir.path().join("repair-token");
        options.action = CredentialAction::Reissue {
            token_file: file.clone(),
            use_stored_token: false,
        };
        assert!(
            credentials_with_rng(&options, &mut store, || panic!(
                "must not replace stored token"
            ))
            .is_err()
        );
        assert!(!file.exists());
        options.action = CredentialAction::Reissue {
            token_file: file.clone(),
            use_stored_token: true,
        };
        let result = credentials_with_rng(&options, &mut store, || {
            panic!("must use selected stored token")
        })
        .unwrap();
        assert_eq!(result["policy_changed"], true);
        assert_eq!(result["agent_use"], "unverified");
        assert_eq!(fs::read(&file).unwrap(), TOKEN);
        assert_eq!(store.items, original_store);
        assert_eq!(store.writes, 0);
        assert_eq!(private_json(&policy).unwrap(), original);
        write(&file, b"DIFFERENT_PUBLIC_FIXTURE");
        let before = fs::read(&policy).unwrap();
        assert!(credentials_with_rng(&options, &mut store, || panic!("no RNG")).is_err());
        assert_eq!(fs::read(policy).unwrap(), before);
        assert_eq!(store.items, original_store);
    }

    #[test]
    fn policy_change_during_lookup_prevents_stale_status_and_every_mutation() {
        struct RotatingStore {
            inner: MemoryStore,
            path: PathBuf,
            policy: Value,
        }
        impl SecretStore for RotatingStore {
            fn get(&mut self, account: &str) -> io::Result<Option<Vec<u8>>> {
                write(&self.path, self.policy.to_string().as_bytes());
                self.inner.get(account)
            }
            fn put_missing(&mut self, _: &str, _: &[u8]) -> io::Result<()> {
                panic!("changed policy must stop before store writes")
            }
        }
        for action in 0..4 {
            let (dir, mut options) = fixture(CredentialProfile::GrokBot);
            let path = options.app.policy_path(dir.path());
            let mut changed = private_json(&path).unwrap();
            changed["token_sha256"] = "f".repeat(64).into();
            let input = dir.path().join("input");
            write(&input, TOKEN);
            let output = dir.path().join("output");
            options.action = match action {
                0 => CredentialAction::Status,
                1 => CredentialAction::Import { token_file: input },
                2 => CredentialAction::Export {
                    token_file: output.clone(),
                    format: CredentialFormat::Token,
                },
                _ => CredentialAction::Reissue {
                    token_file: output.clone(),
                    use_stored_token: true,
                },
            };
            let mut store = RotatingStore {
                inner: stored(&options),
                path: path.clone(),
                policy: changed.clone(),
            };
            assert!(
                credentials_with_rng(&options, &mut store, || panic!(
                    "no RNG after changed lookup"
                ))
                .is_err()
            );
            assert_eq!(private_json(&path).unwrap(), changed);
            assert!(!output.exists());
            assert_eq!(store.inner.writes, 0);
        }
    }

    #[test]
    fn handoff_matches_uppercase_digest_and_requires_local_custody_without_receipt_claims() {
        assert!(matching_bearer(&sha256(TOKEN).to_ascii_uppercase(), TOKEN));
        assert!(!matching_bearer(&sha256(TOKEN), b"WRONG_PUBLIC_FIXTURE"));
        for value in [
            serde_json::json!({"local_available":false,"matches_policy":false}),
            serde_json::json!({"local_available":true,"matches_policy":false}),
        ] {
            assert!(require_local_custody(&value).is_err());
        }
        assert!(require_local_custody(&serde_json::json!({"local_available":true,"matches_policy":true,"agent_use":"unverified"})).is_ok());
    }
}
