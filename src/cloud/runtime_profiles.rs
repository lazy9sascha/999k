//! Private, explicitly mounted local mailbox registration. The normal runner never loads it.
use crate::codex_mcp::{Identity, Profile};
use pigeonmq::{Queue, mailbox_catalog as catalog, private_file::open_private_file, registration};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[path = "runtime_triage.rs"]
mod triage;

const ROOT: &str = "local-mailboxes";
const QUEUE: &str = "mailboxes.sqlite3";
const MANIFEST: &str = "profile.json";
const POLICY: &str = "policy.json";
const REGISTRY: &str = "registration.json";
const TOKEN: &str = "bearer-token";
const LOCK: &str = ".init.lock";
const OBSERVE_TOOLS: [(&str, &str, Capability); 2] = [
    ("pigeonmq_read", "read", Capability::Read),
    ("pigeonmq_wait", "wait", Capability::Wait),
];

#[derive(Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Capability {
    Observe,
    Read,
    Wait,
}

impl Capability {
    fn for_operations(operations: &[Value]) -> Option<Self> {
        [Self::Observe, Self::Read, Self::Wait]
            .into_iter()
            .find(|capability| {
                capability.tools().count() == operations.len()
                    && capability
                        .tools()
                        .all(|(_, name)| operations.iter().any(|op| op == name))
            })
    }

    fn tools(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        OBSERVE_TOOLS
            .into_iter()
            .filter_map(move |(tool, operation, required)| {
                (self == Self::Observe || self == required).then_some((tool, operation))
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    mailbox: String,
    capability: Capability,
}

#[derive(Clone)]
pub(crate) struct LocalIdentity {
    mailbox: String,
    route: String,
    capability: Capability,
}

impl LocalIdentity {
    pub(crate) fn mailbox(&self) -> &str {
        &self.mailbox
    }
    pub(crate) fn route(&self) -> &str {
        &self.route
    }
    pub(crate) fn tool_enabled(&self, name: &str) -> bool {
        self.capability.tools().any(|(tool, operation)| {
            tool == name || (name == pigeonmq::mailbox_tools::READ_THREAD && operation == "read")
        })
    }
}

pub(crate) struct Mailboxes {
    pub(crate) queue: PathBuf,
    pub(crate) profiles: Vec<Profile>,
}

fn error(reason: &'static str) -> io::Error {
    io::Error::other(reason)
}

fn identity(name: &str) -> io::Result<LocalIdentity> {
    let route = format!("/{name}/mcp");
    if !triage::valid_slug(name)
        || catalog::http_by_route(&route).is_some()
        || catalog::http_by_id(name).is_some()
        || catalog::by_mailbox(name).is_some()
        || matches!(name, "mailbox" | "receiver")
    {
        return Err(error("invalid or reserved local mailbox name"));
    }
    Ok(LocalIdentity {
        mailbox: name.into(),
        route,
        capability: Capability::Observe,
    })
}

fn owner_directory(path: &Path) -> io::Result<PathBuf> {
    let metadata = path.symlink_metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(error("owner-private mailbox directory required"));
    }
    path.canonicalize()
}

fn new_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

fn create(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn private(path: &Path) -> io::Result<fs::File> {
    open_private_file(path).map_err(|_| error("private mailbox file unavailable"))
}

fn private_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    private(path)?.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(error("mailbox configuration too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| error("invalid mailbox configuration"))
}

fn root(home: &Path, create_home: bool) -> io::Result<PathBuf> {
    if !home.is_absolute() {
        return Err(error("an explicit absolute local mailbox home is required"));
    }
    if create_home
        && let Err(err) = new_directory(home)
        && err.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(err);
    }
    let home = owner_directory(home)?;
    let root = home.join(ROOT);
    if create_home
        && matches!(root.symlink_metadata(), Err(err) if err.kind() == io::ErrorKind::NotFound)
    {
        for entry in fs::read_dir(&home)? {
            let entry = entry?;
            if entry.file_name() != triage::DIRECTORY {
                return Err(error(
                    "choose a dedicated local mailbox home containing only optional triage input",
                ));
            }
            owner_directory(&entry.path())?;
        }
        match new_directory(&root) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    owner_directory(&root)
}

fn init_lock(root: &Path) -> io::Result<fs::File> {
    let path = root.join(LOCK);
    match create(&path, b"") {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let file = private(&path)?;
    // SAFETY: the live descriptor owns the cooperative lock for the init call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn binding(root: &Path, queue: &Path, name: &str) -> io::Result<Profile> {
    let mut identity = identity(name)?;
    let dir = owner_directory(&root.join(name))?;
    let manifest: Manifest = private_json(&dir.join(MANIFEST))?;
    if manifest.mailbox != identity.mailbox {
        return Err(error(
            "mailbox configuration does not match its registration",
        ));
    }
    identity.capability = manifest.capability;
    let registration = dir.join(REGISTRY);
    let entries = registration::load_registrations(&registration)
        .map_err(|_| error("invalid local mailbox registration"))?;
    // Different local bindings remain independent even if the owner edits a registry.
    if entries.0.len() != 1
        || !entries
            .0
            .get(name)
            .is_some_and(|entry| entry.mailbox == name && !entry.revoked)
    {
        return Err(error(
            "local mailbox registration must contain only its own identity",
        ));
    }
    let profile = Profile {
        identity: Identity::Local(identity),
        db: queue.into(),
        policy: dir.join(POLICY),
        registration,
    };
    profile.validate().map_err(|(_, reason)| error(reason))?;
    Ok(profile)
}

pub(crate) fn load(home: &Path) -> io::Result<Mailboxes> {
    let root = root(home, false)?;
    let queue = root.join(QUEUE);
    private(&queue)?;
    let mut profiles = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == QUEUE || name == LOCK {
            continue;
        }
        if !entry.file_type()?.is_dir() {
            return Err(error("unexpected local mailbox registration entry"));
        }
        let name = name
            .to_str()
            .ok_or_else(|| error("invalid local mailbox name"))?;
        profiles.push(binding(&root, &queue, name)?);
    }
    if profiles.is_empty() {
        return Err(error("no local mailbox registrations"));
    }
    Ok(Mailboxes { queue, profiles })
}

pub(crate) fn init(home: &Path, name: &str) -> io::Result<Value> {
    let identity = identity(name)?;
    let requested = triage::read(home, name)?;
    let capability = requested.unwrap_or(Capability::Observe);
    let root = root(home, true)?;
    let _lock = init_lock(&root)?;
    let queue = root.join(QUEUE);
    match queue.symlink_metadata() {
        Ok(_) => {
            private(&queue)?;
            Queue::open_existing(&queue).map_err(|_| error("local mailbox queue unavailable"))?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            create(&queue, b"")?;
            Queue::open(&queue).map_err(|_| error("local mailbox queue initialization failed"))?;
        }
        Err(err) => return Err(err),
    }
    let dir = root.join(&identity.mailbox);
    let token = dir.join(TOKEN);
    let created = match new_directory(&dir) {
        Ok(()) => {
            // Two random UUIDv4 values provide about 244 random bits without a new dependency.
            let bearer = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let digest: String = Sha256::digest(&bearer)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            create(
                &dir.join(POLICY),
                &serde_json::to_vec(&json!({
                    "principals":[{"id":name,"token_sha256":digest,"operations":capability.tools().map(|(_, op)| op).collect::<Vec<_>>(),
                        "send_to":[],"mailboxes":[name],"revoked":false}]
                }))?,
            )?;
            create(
                &dir.join(REGISTRY),
                &serde_json::to_vec(&json!({
                    name: {"mailbox":name,"revoked":false}
                }))?,
            )?;
            create(&token, bearer.as_bytes())?;
            // Publish the binding last. An interrupted partial setup never enables a route.
            create(
                &dir.join(MANIFEST),
                &serde_json::to_vec(&json!({
                    "mailbox":name,"capability":capability
                }))?,
            )?;
            true
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
        Err(err) => return Err(err),
    };
    let profile = binding(&root, &queue, name)?;
    if requested.is_some_and(|requested| {
        matches!(&profile.identity, Identity::Local(local) if local.capability != requested)
    }) {
        return Err(error("triage operations differ from this staged mailbox; use a new dedicated home to stage a different policy"));
    }
    let file = private(&token)?;
    if file.metadata()?.mode() & 0o777 != 0o600 {
        return Err(error("local mailbox bearer must have mode 0600"));
    }
    let mut bearer = Vec::new();
    file.take(65).read_to_end(&mut bearer)?;
    if bearer.len() != 64 || !bearer.iter().all(u8::is_ascii_hexdigit) {
        return Err(error("invalid local mailbox bearer"));
    }
    let header = [b"Bearer ".iter().copied().chain(bearer).collect()];
    crate::auth::authenticate_private(&profile.policy, &header)
        .and_then(|principal| principal.identity(name))
        .map_err(|_| error("local mailbox bearer does not match its policy"))?;
    Ok(json!({
        "mailbox":name,"route":identity.route,"token_file":token,
        "created":created,"runtime":"local_only","remote_verified":false
    }))
}
