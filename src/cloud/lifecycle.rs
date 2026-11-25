//! Staged registration of existing built-in profiles and explicit launchd activation.
use crate::{
    lifecycle_profile,
    lifecycle_service::{
        HOSTED_LABEL, HUB_LABEL, LaunchControl, RELAY_LABEL, SystemLaunchctl, ensure_plist,
        verify_plist,
    },
    mailbox_startup::{Binding, Manifest, QUEUE, REGISTRATION, bindings, valid_route},
};
use radio999k::{
    arrival_notify::{RelayConfig, RelayTarget, lock_state},
    mailbox_catalog as catalog, mailbox_generation as generation, private_file,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

const DIRECTORY: &str = ".pigeonmq-lifecycle";
const STATE: &str = "profiles.json";
const RELAY_CONFIG: &str = "relay-profiles.json";
const RELAY_STATE: &str = "relay-state.json";
const HOSTED_OWNER: &str = "hosted-unix-owner.json";
const DEFAULT_PORT: u16 = 54843;

fn error(reason: &'static str) -> io::Error {
    io::Error::other(reason)
}

pub(crate) fn supported(name: &str) -> bool {
    matches!(
        name,
        "dot" | "codex-cloud" | "muse" | "grok-bot" | "cue" | "instinct"
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    route: String,
    policy: PathBuf,
    wake_files_dir: Option<PathBuf>,
}

impl Source {
    fn from(binding: &Binding) -> Self {
        Self {
            route: binding.route.clone(),
            policy: binding.policy.clone(),
            wake_files_dir: binding.wake_files_dir.clone(),
        }
    }

    fn binding(
        &self,
        name: &str,
        registration: &Path,
        generation: Option<String>,
    ) -> io::Result<Binding> {
        let descriptor = catalog::http_by_id(name)
            .filter(|_| supported(name))
            .ok_or_else(|| error("profile lifecycle is unavailable"))?;
        if !valid_route(descriptor, &self.route) || !self.policy.is_absolute() {
            return Err(error("invalid protected lifecycle binding"));
        }
        Ok(Binding {
            descriptor,
            route: self.route.clone(),
            policy: self.policy.clone(),
            registration: Some(registration.into()),
            wake_files_dir: self.wake_files_dir.clone(),
            generation,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Staged,
    Active,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    source: Source,
    policy: PathBuf,
    token_file: Option<PathBuf>,
    generation: Option<String>,
    email_recipient: Option<String>,
    relay_after: Option<i64>,
    #[serde(default)]
    provisioned: bool,
    phase: Phase,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    port: u16,
    registration: PathBuf,
    relay_command: Option<PathBuf>,
    profiles: BTreeMap<String, Profile>,
    #[serde(default, skip_serializing_if = "is_false")]
    local_stdio: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finite_cue_installation: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Profile {
    fn binding(&self, name: &str, registration: &Path) -> io::Result<Binding> {
        let mut binding = self
            .source
            .binding(name, registration, self.generation.clone())?;
        if !self.policy.is_absolute()
            || self
                .generation
                .as_deref()
                .is_some_and(|id| !generation::valid_storage(id))
        {
            return Err(error("invalid protected lifecycle generation"));
        }
        binding.policy = self.policy.clone();
        Ok(binding)
    }

    fn storage<'a>(&'a self, name: &'a str) -> &'a str {
        self.generation.as_deref().unwrap_or(name)
    }
}

pub(crate) struct Options<'a> {
    pub(crate) profiles_file: Option<&'a Path>,
    pub(crate) email_recipient: Option<&'a str>,
    pub(crate) relay_command: Option<&'a Path>,
    pub(crate) port: Option<u16>,
    pub(crate) local_stdio: bool,
    pub(crate) apply: bool,
}

fn private_dir(path: &Path) -> io::Result<PathBuf> {
    crate::cloud_setup::require_home(path).map_err(|cause| {
        if cause.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                cause.kind(),
                "lifecycle home is missing; run bare 999k init or create an owner-private directory",
            )
        } else {
            cause
        }
    })
}

fn mkdir_private(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    private_dir(path)?;
    Ok(())
}

fn absent(path: &Path) -> io::Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    private_file::open_private_file(path).map_err(|_| error("private lifecycle file unavailable"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    open_private(path)?
        .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(error("lifecycle configuration exceeds its size limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| error("invalid lifecycle configuration"))
}

fn write_json(path: &Path, data: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(data)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(error("lifecycle configuration exceeds its size limit"));
    }
    private_file::replace_private(path, &bytes)
}

fn lock(dir: &Path) -> io::Result<File> {
    lock_until(dir, std::time::Duration::ZERO)
}

fn lock_until(dir: &Path, wait: std::time::Duration) -> io::Result<File> {
    let path = dir.join("lifecycle.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    open_private(&path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        // SAFETY: the lock is held by the live returned descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let cause = io::Error::last_os_error();
        if cause.kind() != io::ErrorKind::WouldBlock {
            return Err(cause);
        }
        if std::time::Instant::now() >= deadline {
            return Err(error("another lifecycle change is in progress"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn configured(home: &Path, name: &str, manifest: Option<&Path>) -> io::Result<Binding> {
    let descriptor = catalog::http_by_id(name)
        .filter(|_| supported(name))
        .ok_or_else(|| error("profile lifecycle is unavailable"))?;
    if let Some(path) = manifest {
        let file = Manifest::load(path, home)?;
        let entries = bindings(home, Some(&file));
        let registration = entries
            .iter()
            .find_map(|entry| entry.registration.clone())
            .unwrap_or_else(|| home.join(REGISTRATION));
        let mut selected = entries
            .into_iter()
            .find(|entry| entry.descriptor.id == name)
            .ok_or_else(|| error("profile is absent from the selected manifest"))?;
        selected.registration = Some(registration);
        return Ok(selected);
    }
    let policy = home.join(descriptor.policy_file);
    open_private(&policy)?;
    let mut selected = Binding::bundled(descriptor, policy, home);
    selected.registration = Some(home.join(REGISTRATION));
    Ok(selected)
}

fn registration(binding: &Binding, home: &Path) -> PathBuf {
    binding
        .registration
        .clone()
        .unwrap_or_else(|| home.join(REGISTRATION))
}

fn validate_email(name: &str, value: &str) -> io::Result<()> {
    let address = value.split_once('@');
    if !matches!(name, "cue" | "instinct")
        || value.len() > 254
        || !address.is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && local
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._%+-".contains(&c))
                && domain
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
        })
    {
        return Err(error(
            "a valid email recipient is supported for Cue and Instinct",
        ));
    }
    Ok(())
}

fn validate_command(command: &Path) -> io::Result<()> {
    if !command.is_absolute() || open_private(command)?.metadata()?.mode() & 0o100 == 0 {
        return Err(error(
            "relay command must be an absolute owner-private executable",
        ));
    }
    Ok(())
}

fn registration_data(path: &Path) -> io::Result<Value> {
    radio999k::registration::load_registrations(path)
        .map_err(|_| error("invalid mailbox registration"))?;
    read_json(path)
}

fn set_revoked(path: &Path, name: &str, revoked: bool, provisioned: bool) -> io::Result<()> {
    let mut current = registration_data(path)?;
    let entries = current
        .as_object_mut()
        .ok_or_else(|| error("invalid mailbox registration"))?;
    match entries.get_mut(name) {
        Some(entry) if entry["mailbox"] == name && entry["revoked"].is_boolean() => {
            if entry["revoked"] == revoked {
                return Ok(());
            }
            entry["revoked"] = json!(revoked);
        }
        None if provisioned => {
            entries.insert(name.into(), json!({"mailbox":name,"revoked":revoked}));
        }
        _ => return Err(error("profile needs an existing exact registration")),
    }
    write_json(path, &current)
}

fn fresh_profile(name: &str) -> bool {
    matches!(name, "cue" | "instinct")
}

fn init_home(home: &Path, name: &str, manifest: Option<&Path>) -> io::Result<PathBuf> {
    let home = private_dir(home)?;
    let fresh = fresh_profile(name) && manifest.is_none();
    let queue = home.join(QUEUE);
    if fresh && (absent(&queue)? || queue.symlink_metadata()?.len() == 0) {
        let path = home.join(REGISTRATION);
        for entry in fs::read_dir(&home)? {
            let entry = entry?;
            if entry.path() != path && entry.path() != queue {
                return Err(error(
                    "fresh installation needs an empty owner-private home",
                ));
            }
        }
        if absent(&path)? {
            write_json(&path, &json!({}))?;
        } else if registration_data(&path)? != json!({}) {
            return Err(error("fresh installation needs an unused registration"));
        }
        if absent(&queue)? {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&queue)?
                .sync_all()?;
        }
        open_private(&queue)?;
        radio999k::Queue::open(&queue).map_err(io::Error::other)?;
    }
    Ok(home)
}

fn unused_mailbox(queue: &Path, name: &str) -> io::Result<bool> {
    let connection = Connection::open_with_flags(queue, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(io::Error::other)?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(io::Error::other)?;
    connection
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM main.messages WHERE sender=?1 OR recipient=?1)",
            [name],
            |row| row.get(0),
        )
        .map_err(io::Error::other)
}

fn stage_fresh(
    home: &Path,
    name: &str,
    state: &Registry,
    email: Option<&str>,
) -> io::Result<Profile> {
    let queue = home.join(QUEUE);
    let descriptor = catalog::http_by_id(name)
        .ok_or_else(|| error("prepare this connector's protected policy before init"))?;
    if !absent(&home.join(descriptor.policy_file))?
        || registration_data(&state.registration)?.get(name).is_some()
        || !unused_mailbox(&queue, name)?
    {
        return Err(error(
            "existing profile files or queue rows need explicit review",
        ));
    }
    let directory = home.join(DIRECTORY).join(format!("initial-{name}"));
    mkdir_private(&directory)?;
    let result = crate::reply_mailbox_setup::stage_local(&directory, name, &state.registration)?;
    let policy = result["policy"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| error("missing staged policy"))?;
    let token = result["token_file"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| error("missing staged Bearer"))?;
    Ok(Profile {
        source: Source {
            route: descriptor.route.into(),
            policy: policy.clone(),
            wake_files_dir: None,
        },
        policy,
        token_file: Some(token),
        generation: None,
        email_recipient: email.map(str::to_owned),
        relay_after: None,
        provisioned: true,
        phase: Phase::Staged,
    })
}

fn verify_staged(profile: &Profile, name: &str, registration: &Path) -> io::Result<()> {
    let Some(token) = &profile.token_file else {
        return Ok(());
    };
    open_private(token)?;
    open_private(&profile.policy)?;
    let stage = token
        .parent()
        .ok_or_else(|| error("invalid staged Bearer location"))?;
    if profile.generation.is_some() {
        let (policy, repeated_token) = lifecycle_profile::stage_replacement(
            stage,
            &profile.source.binding(name, registration, None)?,
        )?;
        if policy != profile.policy || &repeated_token != token {
            return Err(error("staged credential paths changed"));
        }
    } else {
        crate::reply_mailbox_setup::stage_local(stage, name, registration)?;
    }
    Ok(())
}

fn high_water(path: &Path) -> io::Result<i64> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(io::Error::other)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(io::Error::other)?;
    conn.query_row(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='messages'),0)",
        [],
        |row| row.get(0),
    )
    .map_err(io::Error::other)
}

fn response(name: &str, profile: &Profile, runtime_verified: bool) -> Value {
    let status = match profile.phase {
        Phase::Staged => "staged",
        Phase::Active => "active",
        Phase::Retired => "retired",
    };
    let mut data = json!({ "mailbox":name, "route":profile.source.route, "state":status, "runtime_verified":runtime_verified });
    if let Some(file) = &profile.token_file {
        data["token_file"] = json!(file);
    }
    data
}

struct Jobs<'a> {
    service: &'a dyn LaunchControl,
    directory: &'a Path,
    executable: &'a Path,
    installation: Option<&'a InstallPaths>,
}

struct Job {
    label: &'static str,
    args: Vec<String>,
    path: PathBuf,
    loaded: bool,
    exists: bool,
}

impl Job {
    fn inspect(jobs: &Jobs<'_>, label: &'static str, mut args: Vec<String>) -> io::Result<Self> {
        let exe = jobs
            .executable
            .to_str()
            .filter(|_| jobs.executable.is_absolute())
            .ok_or_else(|| error("lifecycle executable must be an absolute path"))?;
        args.insert(0, exe.into());
        let path = jobs.directory.join(format!("{label}.plist"));
        let exists = verify_plist(jobs.directory, label, &args)?.is_some();
        let loaded = jobs.service.loaded(label, &path)?;
        if loaded && !exists {
            return Err(error("launchd label is already occupied"));
        }
        Ok(Self {
            label,
            args,
            path,
            loaded,
            exists,
        })
    }

    fn start(&self, jobs: &Jobs<'_>) -> io::Result<()> {
        let (path, _) = ensure_plist(jobs.directory, self.label, &self.args)?;
        jobs.service.bootstrap(&path)
    }
}

fn path_arg(path: &Path) -> io::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| error("lifecycle path must be UTF-8"))
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HostedOwner {
    installation: String,
    binary: PathBuf,
    cloudflared: PathBuf,
}

fn read_hosted_owner(home: &Path) -> io::Result<Option<HostedOwner>> {
    let path = home.join(DIRECTORY).join(HOSTED_OWNER);
    if absent(&path)? {
        Ok(None)
    } else {
        read_json(&path).map(Some)
    }
}

pub(crate) fn hosted_cue_mode(home: &Path, installation: uuid::Uuid) -> io::Result<bool> {
    let state: Registry = read_json(&home.join(DIRECTORY).join(STATE))?;
    match state.finite_cue_installation {
        None => Ok(false),
        Some(value) if value == installation.to_string() => Ok(true),
        _ => Err(error(
            "finite hosted Cue mode belongs to another installation",
        )),
    }
}

pub(crate) fn verify_finite_cue_pairing_bearer(home: &Path, bearer: &str) -> io::Result<bool> {
    let state_path = home.join(DIRECTORY).join(STATE);
    if absent(&state_path)? {
        return Ok(false);
    }
    let state: Registry = read_json(&state_path)?;
    let Some(installation) = &state.finite_cue_installation else {
        return Ok(false);
    };
    let unavailable = || error("active finite hosted Cue Bearer required");
    let owner = read_hosted_owner(home)?.ok_or_else(unavailable)?;
    let (id, descriptor) = hosted_scope(home, &owner)?;
    let profile = state.profiles.get("cue").ok_or_else(unavailable)?;
    if *installation != id.to_string()
        || profile.phase != Phase::Active
        || profile.source.route != "/cue/mcp"
        || !scope_matches(&state, &descriptor)
        || !descriptor.mcp_paths.iter().any(|path| path == "/cue/mcp")
    {
        return Err(unavailable());
    }
    let generation = crate::local_http::protect_pairing_generation(
        &home.join(QUEUE),
        "cue",
        profile.storage("cue"),
    )?;
    let scope = crate::hosted_mailbox_bearer::Scope::new(home, id);
    let _grant = scope.lock(false)?;
    if !hosted_cue_mode(home, id)? {
        return Err(unavailable());
    }
    scope
        .authorize(&[format!("Bearer {bearer}").into_bytes()])
        .map_err(|_| unavailable())?;
    crate::auth::active_hosted_cue_policy(&profile.policy).map_err(|_| unavailable())?;
    radio999k::registration::validate_registration(&state.registration, "cue", "cue")
        .map_err(|_| unavailable())?;
    drop(generation);
    Ok(true)
}

pub(crate) fn hosted_cue_bearer_status(home: &Path) -> io::Result<Value> {
    let home = private_dir(home)?;
    let owner = read_hosted_owner(&home)?.ok_or_else(|| error("no managed hosted Unix owner"))?;
    let (installation, _) = hosted_scope(&home, &owner)?;
    if !hosted_cue_mode(&home, installation)? {
        return Ok(json!({"state":"legacy"}));
    }
    let scope = crate::hosted_mailbox_bearer::Scope::new(&home, installation);
    let _guard = scope.lock(false)?;
    Ok(scope.status())
}

pub(crate) fn hosted_cue_bearer(home: &Path, rotate: Option<(u64, &Path)>) -> io::Result<()> {
    let home = private_dir(home)?;
    let dir = private_dir(&home.join(DIRECTORY))?;
    let _owner_guard = lock(&dir)?;
    let owner = read_hosted_owner(&home)?.ok_or_else(|| error("no managed hosted Unix owner"))?;
    let (installation, descriptor) = hosted_scope(&home, &owner)?;
    let state_path = dir.join(STATE);
    let mut state: Registry = read_json(&state_path)?;
    if !scope_matches(&state, &descriptor)
        || !descriptor.mcp_paths.iter().any(|path| path == "/cue/mcp")
        || !state
            .profiles
            .get("cue")
            .is_some_and(|row| row.phase == Phase::Active && row.source.route == "/cue/mcp")
    {
        return Err(error("managed hosted Cue route is unavailable"));
    }
    // A previous binary does not enforce finite grants. Require the owned
    // managed job to be stopped before the first activation, and hold the
    // existing queue lease so another application hub cannot enter.
    let _first_activation_queue = if rotate.is_some()
        && state.finite_cue_installation.as_deref() != Some(&owner.installation)
    {
        let (directory, executable, installation_paths) = system_jobs()?;
        let jobs = Jobs {
            service: &SystemLaunchctl,
            directory: &directory,
            executable: &executable,
            installation: Some(&installation_paths),
        };
        if managed_hub_job(&jobs, &home)?.loaded {
            return Err(error(
                "stop the owned managed hub before enabling finite hosted Cue Bearers",
            ));
        }
        Some(crate::local_http::lock_hub_queue(&home.join(QUEUE))?)
    } else {
        None
    };
    let scope = crate::hosted_mailbox_bearer::Scope::new(&home, installation);
    let _grant_guard = scope.lock(true)?;
    if let Some((seconds, path)) = rotate {
        // Once opted in, an interrupted rotation or deleted grant fails closed.
        scope.rotate_locked(seconds, path, || {
            if state.finite_cue_installation.as_deref() != Some(&owner.installation) {
                state.finite_cue_installation = Some(owner.installation.clone());
                write_json(&state_path, &state)?;
            }
            Ok(())
        })
    } else if state.finite_cue_installation.as_deref() == Some(&owner.installation) {
        scope.revoke_locked()
    } else {
        Err(error("finite hosted Cue Bearer mode is not enabled"))
    }
}

fn hosted_scope(
    home: &Path,
    owner: &HostedOwner,
) -> io::Result<(uuid::Uuid, crate::hosted_registration::OriginDescriptor)> {
    let (id, scope, _) = crate::hosted_enrollment::staged_unix(home)
        .map_err(io::Error::other)?
        .ok_or_else(|| error("hosted Unix enrollment is not staged"))?;
    if id.to_string() != owner.installation {
        return Err(error("hosted ownership does not match the installation"));
    }
    Ok((id, scope))
}

fn scope_matches(state: &Registry, scope: &crate::hosted_registration::OriginDescriptor) -> bool {
    let active = active_routes(state);
    state.port == scope.hub_port && scope.mcp_paths.iter().all(|route| active.contains(route))
}

fn hosted_job(jobs: &Jobs<'_>, home: &Path) -> io::Result<Job> {
    Job::inspect(
        jobs,
        HOSTED_LABEL,
        vec![
            "serve-hosted-connector".into(),
            "--home".into(),
            path_arg(home)?,
        ],
    )
}

fn managed_hub_job(jobs: &Jobs<'_>, home: &Path) -> io::Result<Job> {
    Job::inspect(
        jobs,
        HUB_LABEL,
        vec!["serve-managed".into(), "--home".into(), path_arg(home)?],
    )
}

fn stop_hosted(jobs: &Jobs<'_>, home: &Path, owner: &HostedOwner) -> io::Result<()> {
    if owner.binary != jobs.executable {
        return Err(error("hosted LaunchAgent belongs to another installation"));
    }
    let job = hosted_job(jobs, home)?;
    if job.loaded {
        jobs.service.bootout(HOSTED_LABEL)?;
    }
    if job.exists {
        fs::remove_file(job.path)?;
        File::open(jobs.directory)?.sync_all()?;
    }
    let dir = home.join(DIRECTORY);
    fs::remove_file(dir.join(HOSTED_OWNER))?;
    File::open(dir)?.sync_all()
}

fn relay_config(state: &Registry) -> io::Result<RelayConfig> {
    state
        .profiles
        .iter()
        .filter_map(|(name, profile)| {
            if profile.phase != Phase::Active {
                return None;
            }
            profile.email_recipient.as_ref().map(|recipient| {
                Ok((
                    name.clone(),
                    RelayTarget {
                        recipient: recipient.clone(),
                        generation: profile.storage(name).into(),
                        after: profile
                            .relay_after
                            .ok_or_else(|| error("relay activation boundary is missing"))?,
                    },
                ))
            })
        })
        .collect()
}

fn schedule_jobs(jobs: &Jobs<'_>, home: &Path, state: &Registry) -> io::Result<(Job, Option<Job>)> {
    let dir = home.join(DIRECTORY);
    let hub = Job::inspect(
        jobs,
        HUB_LABEL,
        vec!["serve-managed".into(), "--home".into(), path_arg(home)?],
    )?;
    let relay_path = jobs.directory.join(format!("{RELAY_LABEL}.plist"));
    let relay = if let Some(command) = &state.relay_command {
        Some(Job::inspect(
            jobs,
            RELAY_LABEL,
            vec![
                "notify-arrivals".into(),
                "--queue".into(),
                path_arg(&home.join(QUEUE))?,
                "--registration".into(),
                path_arg(&state.registration)?,
                "--state".into(),
                path_arg(&dir.join(RELAY_STATE))?,
                "--command".into(),
                path_arg(command)?,
                "--config".into(),
                path_arg(&dir.join(RELAY_CONFIG))?,
                "--interval-secs".into(),
                "5".into(),
            ],
        )?)
    } else {
        if !absent(&relay_path)? || jobs.service.loaded(RELAY_LABEL, &relay_path)? {
            return Err(error(
                "unmanaged relay conflicts with the requested lifecycle",
            ));
        }
        None
    };
    Ok((hub, relay))
}

fn active_routes(state: &Registry) -> Vec<String> {
    state
        .profiles
        .values()
        .filter(|profile| profile.phase == Phase::Active)
        .map(|profile| profile.source.route.clone())
        .collect()
}

fn change(
    jobs: &Jobs<'_>,
    home: &Path,
    current: &Registry,
    mut next: Registry,
    name: &str,
    phase_changed: bool,
) -> io::Result<()> {
    let dir = home.join(DIRECTORY);
    let queue = home.join(QUEUE);
    let stdio_changed = next.local_stdio && !current.local_stdio;
    let email_added = !phase_changed
        && current.profiles[name].email_recipient.is_none()
        && next.profiles[name].email_recipient.is_some();
    let want_relay = next
        .profiles
        .values()
        .any(|p| p.phase == Phase::Active && p.email_recipient.is_some());
    if want_relay && let Some(installation) = jobs.installation {
        let command = next
            .relay_command
            .as_deref()
            .ok_or_else(|| error("managed relay requires a durable installed command"))?;
        validate_command(command)?;
        installation.require_executable(command)?;
    }
    let (hub, relay) = schedule_jobs(jobs, home, &next)?;
    let relay_state = dir.join(RELAY_STATE);
    let relay_path = dir.join(RELAY_CONFIG);
    let previous_relay = if absent(&relay_path)? {
        RelayConfig::new()
    } else {
        read_json(&relay_path)?
    };
    let relay_configuration_changed = if phase_changed || email_added {
        want_relay || !previous_relay.is_empty()
    } else {
        relay_config(&next)? != previous_relay
    };
    let dispatch_guard = if relay_configuration_changed {
        Some(lock_state(&relay_state)?)
    } else {
        None
    };
    let adding = phase_changed && next.profiles[name].phase == Phase::Active;
    let registration_before = if adding {
        let mut original = Vec::new();
        open_private(&next.registration)?
            .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
            .read_to_end(&mut original)?;
        if original.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
            return Err(error("lifecycle registration exceeds its size limit"));
        }
        Some(original)
    } else {
        None
    };
    if next.profiles[name].phase == Phase::Retired
        && let Some(owner) = read_hosted_owner(home)?
        && !hosted_scope(home, &owner).is_ok_and(|(_, scope)| scope_matches(&next, &scope))
    {
        stop_hosted(jobs, home, &owner)?;
    }
    if (phase_changed || stdio_changed) && hub.loaded {
        jobs.service.bootout(HUB_LABEL)?;
    }
    let transition = (|| {
        if phase_changed {
            let profile = next
                .profiles
                .get_mut(name)
                .ok_or_else(|| error("missing lifecycle profile"))?;
            match profile.phase {
                Phase::Retired => {
                    generation::retire(&queue, name).map_err(io::Error::other)?;
                    set_revoked(&next.registration, name, true, profile.provisioned)?;
                }
                Phase::Active => {
                    if let Some(id) = &profile.generation {
                        generation::activate_staged(&queue, name, id).map_err(io::Error::other)?;
                    }
                    if profile.generation.is_some() || profile.provisioned {
                        set_revoked(&next.registration, name, false, profile.provisioned)?;
                    }
                    let binding = profile.binding(name, &next.registration)?;
                    if profile.provisioned && profile.generation.is_none() {
                        match name {
                            "dot" => radio999k::dot_profile::check(&queue, &binding.policy, true)?,
                            "muse" => radio999k::muse_mailbox::prepare(&queue)
                                .map_err(io::Error::other)?,
                            "grok-bot" => radio999k::grok_mailbox::prepare(&queue)
                                .map_err(io::Error::other)?,
                            _ => {}
                        }
                    }
                    lifecycle_profile::validate(&queue, &binding)?;
                    if profile.email_recipient.is_some() && profile.relay_after.is_none() {
                        profile.relay_after = Some(high_water(&queue)?);
                    }
                }
                Phase::Staged => return Err(error("cannot apply an incomplete profile")),
            }
            write_json(&dir.join(STATE), &next)?;
        }
        if stdio_changed && !phase_changed {
            write_json(&dir.join(STATE), &next)?;
        }
        let routes = active_routes(&next);
        if !routes.is_empty() {
            if !hub.loaded || phase_changed || stdio_changed {
                hub.start(jobs)?;
            }
            jobs.service.hub_ready(&hub.path, next.port, &routes)?;
            if next.local_stdio {
                jobs.service.stdio_ready(&hub.path, home, &routes)?;
            }
        }
        Ok::<_, io::Error>(())
    })();
    if let Err(err) = transition {
        if registration_before.is_some() || (stdio_changed && !phase_changed) {
            // An unsuccessful start leaves the attempted profile staged. If a fresh
            // storage identity was allocated, the same staged Bearer can still retry it.
            // The former generation remains retired and the relay map is still unchanged.
            // Stdio-only activation reuses this rollback so an unloaded prior hub is
            // not started and a loaded prior hub is restored.
            if jobs.service.loaded(HUB_LABEL, &hub.path)? {
                jobs.service.bootout(HUB_LABEL)?;
            }
            write_json(&dir.join(STATE), current)?;
            if let Some(registration_before) = registration_before.as_ref()
                && fs::read(&current.registration)? != *registration_before
            {
                private_file::replace_private(&current.registration, registration_before)?;
            }
            if hub.loaded && !active_routes(current).is_empty() {
                hub.start(jobs)?;
                jobs.service
                    .hub_ready(&hub.path, current.port, &active_routes(current))?;
            } else if !hub.exists && verify_plist(jobs.directory, hub.label, &hub.args)?.is_some() {
                fs::remove_file(&hub.path)?;
                File::open(jobs.directory)?.sync_all()?;
            }
        } else if phase_changed
            && hub.loaded
            && current
                .profiles
                .iter()
                .filter(|(_, p)| p.phase == Phase::Active)
                .all(|(name, p)| {
                    generation::active_storage(&queue, name)
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(p.storage(name))
                })
        {
            // A retired binding cannot be restored. Recover only if removal
            // failed before its queue identity changed.
            if !jobs.service.loaded(HUB_LABEL, &hub.path)? {
                hub.start(jobs)?;
            }
        }
        return Err(err);
    }

    if stdio_changed && !phase_changed {
        return Ok(());
    }
    if email_added {
        // The relay-state lock fences this activation against delivery by the
        // existing notifier. The new mailbox starts at the current queue edge.
        next.profiles
            .get_mut(name)
            .ok_or_else(|| error("missing lifecycle profile"))?
            .relay_after = Some(high_water(&queue)?);
        write_json(&dir.join(STATE), &next)?;
    }
    let current_relay = relay_config(&next)?;
    if current_relay != previous_relay || (!current_relay.is_empty() && absent(&relay_path)?) {
        write_json(&relay_path, &current_relay)?;
    }
    drop(dispatch_guard);
    if let Some(relay) = relay {
        if current_relay.is_empty() {
            if relay.loaded {
                jobs.service.bootout(RELAY_LABEL)?;
            }
            if relay.exists {
                fs::remove_file(relay.path)?;
                File::open(jobs.directory)?.sync_all()?;
            }
        } else if !relay.loaded {
            relay.start(jobs)?;
        }
    }
    Ok(())
}

struct InstallPaths {
    home: PathBuf,
    temporary: Vec<PathBuf>,
}

impl InstallPaths {
    fn require_executable(&self, path: &Path) -> io::Result<PathBuf> {
        let invalid = || {
            error(
                "managed launchd requires a durable installed executable outside temporary, build, and Git checkout directories",
            )
        };
        let executable = path.canonicalize()?;
        let locations =
            std::iter::once(executable.as_path()).chain((path != executable).then_some(path));
        let metadata = executable.symlink_metadata()?;
        // SAFETY: geteuid has no arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o022 != 0
            || !matches!(metadata.uid(), 0) && metadata.uid() != uid
            || locations
                .clone()
                .any(|location| self.temporary.iter().any(|root| location.starts_with(root)))
        {
            return Err(invalid());
        }

        // A packaged install can sit underneath a package manager's own Git tree.
        // Recognize the install receipt in the physical package prefix only.
        let package = executable
            .parent()
            .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
            .and_then(Path::parent)
            .filter(|dir| {
                dir.join("INSTALL_RECEIPT.json")
                    .symlink_metadata()
                    .is_ok_and(|receipt| {
                        receipt.is_file()
                            && receipt.uid() == metadata.uid()
                            && receipt.mode() & 0o022 == 0
                    })
            });
        // Check the configured spelling too. launchd could otherwise retain a
        // temporary alias to an executable that is physically installed.
        for directory in locations.flat_map(|location| location.ancestors().skip(1)) {
            if directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    let name = name.to_ascii_lowercase();
                    matches!(name.as_str(), "temp" | "scratch" | "temporaryitems")
                        || (name == "tmp" && directory.starts_with(&self.home))
                        || name.starts_with("scratch-")
                })
                || directory.join(".fingerprint").is_dir()
                || (directory
                    .file_name()
                    .is_some_and(|name| name == "debug" || name == "release")
                    && directory
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == "target"))
            {
                return Err(invalid());
            }
            if absent(&directory.join(".git"))?
                || package
                    .is_some_and(|prefix| prefix != directory && prefix.starts_with(directory))
            {
                continue;
            }
            if !directory.join("Cargo.toml").is_file()
                && (directory == self.home
                    || directory.canonicalize().ok().as_deref() == Some(self.home.as_path()))
            {
                continue;
            }
            return Err(invalid());
        }
        Ok(executable)
    }
}

fn system_jobs() -> io::Result<(PathBuf, PathBuf, InstallPaths)> {
    system_jobs_for(&std::env::current_exe()?)
}

fn system_jobs_for(binary: &Path) -> io::Result<(PathBuf, PathBuf, InstallPaths)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| error("absolute user home is unavailable"))?
        .canonicalize()?;
    let directory = home.join("Library/LaunchAgents");
    let installation = InstallPaths {
        home,
        temporary: ["/tmp", "/var/tmp", "/var/folders", "/dev/shm"]
            .into_iter()
            .map(PathBuf::from)
            .chain([std::env::temp_dir()])
            .flat_map(|root| [root.canonicalize().ok(), Some(root)].into_iter().flatten())
            .filter(|root| root.is_absolute())
            .collect(),
    };
    let executable = installation.require_executable(binary)?;
    Ok((directory, executable, installation))
}

pub(crate) fn schedule_hosted_unix(home: &Path) -> io::Result<bool> {
    let dir = home.join(DIRECTORY);
    if absent(&dir)? {
        return Ok(false);
    }
    let dir = private_dir(&dir)?;
    let state = dir.join(STATE);
    if absent(&state)? {
        return Ok(false);
    }
    let Some((_, scope, _)) =
        crate::hosted_enrollment::staged_unix(home).map_err(io::Error::other)?
    else {
        return Ok(false);
    };
    if !scope_matches(&read_json(&state)?, &scope) {
        return Ok(false);
    }
    let (directory, executable, installation) = system_jobs()?;
    let cloudflared = crate::hosted_connector::preflight(None).map_err(|_| {
        error("Install cloudflared on this Mac, check that cloudflared --version works in the setup terminal, and repeat hosted setup")
    })?;
    let cloudflared = installation.require_executable(&cloudflared)?;
    let jobs = Jobs {
        service: &SystemLaunchctl,
        directory: &directory,
        executable: &executable,
        installation: Some(&installation),
    };
    schedule_hosted_with(&jobs, home, &cloudflared)
}

fn schedule_hosted_with(jobs: &Jobs<'_>, home: &Path, cloudflared: &Path) -> io::Result<bool> {
    let dir = private_dir(&home.join(DIRECTORY))?;
    let _guard = lock(&dir)?;
    let state: Registry = read_json(&dir.join(STATE))?;
    let Some((id, scope, _)) =
        crate::hosted_enrollment::staged_unix(home).map_err(io::Error::other)?
    else {
        return Ok(false);
    };
    if !scope_matches(&state, &scope) {
        return Ok(false);
    }
    let owner = HostedOwner {
        installation: id.to_string(),
        binary: jobs.executable.into(),
        cloudflared: cloudflared.into(),
    };
    let previous = read_hosted_owner(home)?;
    if previous.as_ref().is_some_and(|old| old != &owner) {
        return Err(error("hosted LaunchAgent belongs to another installation"));
    }
    let hosted = hosted_job(jobs, home)?;
    if previous.is_none() && hosted.exists {
        return Err(error(
            "hosted LaunchAgent is not owned by the selected home",
        ));
    }
    let hub = managed_hub_job(jobs, home)?;
    let origin = scope.origin().map_err(io::Error::other)?;
    let socket = origin.socket_path();
    let was_hosted = previous.is_some();
    if !was_hosted {
        write_json(&dir.join(HOSTED_OWNER), &owner)?;
    }
    let readiness = || {
        jobs.service.hosted_ready(
            &hub.path,
            Path::new(&socket),
            scope.hub_port,
            &scope.mcp_paths,
        )
    };
    if !hub.loaded {
        hub.start(jobs)?;
    } else if !was_hosted || (!hosted.loaded && readiness().is_err()) {
        // An interrupted first apply may have saved the receipt before restarting
        // the former TCP-only hub. Reload only the verified hub job.
        jobs.service.bootout(HUB_LABEL)?;
        hub.start(jobs)?;
    }
    readiness()?;
    if !hosted.loaded {
        hosted.start(jobs)?;
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
pub(crate) fn serve_hosted_connector(home: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let home = private_dir(home)?;
    let dir = private_dir(&home.join(DIRECTORY))?;
    let owner = read_hosted_owner(&home)?
        .ok_or_else(|| error("hosted LaunchAgent is not owned by this home"))?;
    let (id, scope) = hosted_scope(&home, &owner)?;
    let (directory, executable, installation) = system_jobs()?;
    if owner.binary != executable
        || owner.cloudflared != installation.require_executable(&owner.cloudflared)?
    {
        return Err(error("hosted LaunchAgent executable changed"));
    }
    let jobs = Jobs {
        service: &SystemLaunchctl,
        directory: &directory,
        executable: &executable,
        installation: Some(&installation),
    };
    let hosted = hosted_job(&jobs, &home)?;
    if !hosted.loaded
        || !SystemLaunchctl::owns_running_job(HOSTED_LABEL, &hosted.path, std::process::id())?
    {
        return Err(error(
            "connector process is not the owned hosted LaunchAgent",
        ));
    }
    let hub = managed_hub_job(&jobs, &home)?;
    if !hub.loaded {
        return Err(error("owned managed hub is not loaded"));
    }
    let origin = scope.origin().map_err(io::Error::other)?;
    let socket = crate::hosted_origin_local::attest_system_path(&origin, id)?;
    jobs.service
        .hosted_ready(&hub.path, &socket, scope.hub_port, &scope.mcp_paths)?;
    // An in-progress retirement still owns this lock. Do not start within it.
    let _owner = lock(&dir)?;
    if read_hosted_owner(&home)?.as_ref() != Some(&owner)
        || !scope_matches(&read_json(&dir.join(STATE))?, &scope)
        || hosted_scope(&home, &owner)? != (id, scope)
    {
        return Err(error("hosted LaunchAgent scope changed"));
    }
    Err(std::process::Command::new(&owner.cloudflared)
        .args(["tunnel", "run", "--token-file"])
        .arg(home.join(crate::hosted_connector::TOKEN_FILE))
        .exec())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn serve_hosted_connector(_: &Path) -> io::Result<()> {
    Err(error("999K hosted LaunchAgents require macOS"))
}

/// Return the existing managed hub queue path without opening it.
#[allow(dead_code)]
pub(crate) fn managed_hub_queue(home: &Path) -> Option<PathBuf> {
    let queue = home.join("pigeonmq-cloud-queue.sqlite3");
    let state = home.join(DIRECTORY).join(STATE);
    if queue.is_file() && state.is_file() {
        Some(queue)
    } else {
        None
    }
}
/// A pending marker distinguishes an interrupted first run from an existing
/// hub whose credentials are missing. Never provision over the latter.
pub(crate) fn orca_sender_file(home: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    orca_sender_file_with(home, init)
}

fn orca_sender_file_with(
    path: &Path,
    activate: impl Fn(&Path, &str, &Options<'_>) -> io::Result<Value>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    mkdir_private(path)?;
    let home = private_dir(path)?;
    // Lock the directory itself so checking for an empty home adds no files.
    let guard = File::open(&home)?;
    guard.lock()?;
    let pending = home.join(".orca-first-install-pending");
    let sender = "codex-cloud";
    if absent(&pending)? {
        if fs::read_dir(&home)?.next().transpose()?.is_some() {
            return crate::orca_onboard::resolve_bearer_file(&home, sender);
        }
        write_json(&pending, &1u8)?;
    }
    if read_json::<u8>(&pending)? != 1 {
        return Err(error("invalid Orca first-install state").into());
    }
    let queue = home.join(QUEUE);
    if !absent(&queue)? && open_private(&queue)?.metadata()?.len() == 0 {
        // Resume after the first initialization created the private file but
        // exited before SQLite wrote the schema.
        radio999k::Queue::open(&queue)?;
    }
    crate::cloud_setup::initialize_home(&home)?;
    let registration = home.join(REGISTRATION);
    let entries = json!({
        "codex-cloud":{"mailbox":"codex-cloud","revoked":false}
    });
    if absent(&registration)? {
        write_json(&registration, &entries)?;
    } else if registration_data(&registration)? != entries {
        return Err(
            error("Orca bootstrap registration changed; existing settings preserved").into(),
        );
    }
    crate::reply_mailbox_setup::stage_local(&home, sender, &registration)?;
    activate(&home, sender, &orca_options())?;
    fs::remove_file(pending)?;
    File::open(&home)?.sync_all()?;
    println!("Local hub and sender ready.");
    crate::orca_onboard::resolve_bearer_file(&home, sender)
}

pub(crate) fn init(home: &Path, name: &str, options: &Options<'_>) -> io::Result<Value> {
    init_mode(home, name, options, false)
}

fn orca_options() -> Options<'static> {
    Options {
        profiles_file: None,
        email_recipient: None,
        relay_command: None,
        port: None,
        local_stdio: true,
        apply: true,
    }
}

pub(crate) fn add_orca_peer(home: &Path, name: &str) -> io::Result<Value> {
    init_mode(home, name, &orca_options(), true)
}

pub(crate) fn add_orca_peer_with_email(
    home: &Path,
    name: &str,
    recipient: &str,
    relay_command: Option<&Path>,
) -> io::Result<Value> {
    let options = Options {
        email_recipient: Some(recipient),
        relay_command,
        ..orca_options()
    };
    init_mode(home, name, &options, true)
}

fn init_mode(home: &Path, name: &str, options: &Options<'_>, selected: bool) -> io::Result<Value> {
    if options.apply {
        let (directory, executable, installation) = system_jobs()?;
        let jobs = Jobs {
            service: &SystemLaunchctl,
            directory: &directory,
            executable: &executable,
            installation: Some(&installation),
        };
        init_selected(home, name, options, Some(&jobs), selected)
    } else {
        init_with(home, name, options, None)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn codex_preview_stdio() -> io::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    const MARKER: &[u8] = b"pigeonmq-codex-local-preview-v1\n";
    const MAILBOX: &str = "cue";

    fn digest(reader: &mut impl Read) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        let mut bytes = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            hash.update(&bytes[..count]);
        }
        Ok(hash.finalize().into())
    }

    let user = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| error("Codex preview needs an absolute user home"))?
        .canonicalize()?;
    let prefix = private_dir(&user.join(".pmq-codex-preview"))?;
    let mut marker = Vec::new();
    open_private(&prefix.join("enable"))?
        .take((MARKER.len() + 1) as u64)
        .read_to_end(&mut marker)?;
    if marker != MARKER {
        return Err(error(
            "enable this Codex preview in a disposable macOS account",
        ));
    }
    // Codex can open a discovery connection while the thread connection starts.
    let guard = lock_until(&prefix, std::time::Duration::from_secs(8))?;
    let home = prefix.join("hub");
    let durable = prefix.join("pigeonmq");
    let running = std::env::current_exe()?;
    let expected = digest(&mut File::open(&running)?)?;
    if absent(&durable)? {
        let temporary = prefix.join(format!(".binary-{}", uuid::Uuid::new_v4()));
        let stage = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o700)
                .open(&temporary)?;
            io::copy(&mut File::open(&running)?, &mut file)?;
            file.flush()?;
            file.sync_all()?;
            if digest(&mut open_private(&temporary)?)? != expected {
                return Err(error("Codex preview binary changed during installation"));
            }
            crate::lifecycle_service::publish_new(&temporary, &durable)?;
            Ok(())
        })();
        // Only remove this attempt's unique staging file.
        if stage.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        stage?;
        File::open(&prefix)?.sync_all()?;
    }
    if digest(&mut open_private(&durable)?)? != expected {
        return Err(error("a different Codex preview binary is installed"));
    }
    let (directory, executable, installation) = system_jobs_for(&durable)?;
    let jobs = Jobs {
        service: &SystemLaunchctl,
        directory: &directory,
        executable: &executable,
        installation: Some(&installation),
    };
    managed_hub_job(&jobs, &home)?;
    for label in [HOSTED_LABEL, RELAY_LABEL] {
        let path = directory.join(format!("{label}.plist"));
        if !absent(&path)? || jobs.service.loaded(label, &path)? {
            return Err(error(
                "an existing hosted or relay job conflicts with the preview",
            ));
        }
    }
    mkdir_private(&home)?;
    let credentials = home.join(DIRECTORY).join("initial-cue");
    let state_path = home.join(DIRECTORY).join(STATE);
    if !absent(&state_path)? {
        let state: Registry = read_json(&state_path)?;
        if state.registration != home.join(REGISTRATION)
            || state.relay_command.is_some()
            || state.profiles.len() != 1
            || !state.profiles.get(MAILBOX).is_some_and(|profile| {
                profile.provisioned
                    && profile.phase != Phase::Retired
                    && profile.email_recipient.is_none()
                    && profile.source.route == "/cue/mcp"
                    && profile
                        .token_file
                        .as_deref()
                        .is_some_and(|token| token.parent() == Some(&credentials))
            })
        {
            return Err(error("Codex preview refuses an existing foreign hub home"));
        }
    }
    let result = init_with(
        &home,
        MAILBOX,
        &Options {
            profiles_file: None,
            email_recipient: None,
            relay_command: None,
            port: None,
            local_stdio: true,
            apply: true,
        },
        Some(&jobs),
    )?;
    let bearer = result["token_file"]
        .as_str()
        .map(Path::new)
        .filter(|path| path.parent() == Some(&credentials))
        .ok_or_else(|| error("Codex preview mailbox bearer is unavailable"))?;
    drop(guard);
    crate::stdio_mcp::run(&home, MAILBOX, bearer, None)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn codex_preview_stdio() -> io::Result<()> {
    Err(error("Codex preview requires macOS"))
}

fn init_with(
    home: &Path,
    name: &str,
    options: &Options<'_>,
    jobs: Option<&Jobs<'_>>,
) -> io::Result<Value> {
    init_selected(home, name, options, jobs, false)
}

fn init_selected(
    home: &Path,
    name: &str,
    options: &Options<'_>,
    jobs: Option<&Jobs<'_>>,
    selected: bool,
) -> io::Result<Value> {
    let home = if selected {
        if !matches!(name, "dot" | "cue" | "instinct" | "muse" | "grok-bot") {
            return Err(error("unsupported Orca peer"));
        }
        private_dir(home)?
    } else {
        init_home(home, name, options.profiles_file)?
    };
    let queue = home.join(QUEUE);
    let dir = home.join(DIRECTORY);
    let state_path = dir.join(STATE);
    if selected && absent(&state_path)? {
        return Err(error("connect this peer on an existing managed hub"));
    }
    open_private(&queue)?;
    let prepared = generation::supports(&queue).map_err(io::Error::other)?;
    if !prepared {
        if options.apply
            || options.email_recipient.is_some()
            || options.relay_command.is_some()
            || options.port.is_some()
        {
            return Err(error(
                "legacy queue supports read-only profile recognition; lifecycle requires the generation catalog",
            ));
        }
        let binding = configured(&home, name, options.profiles_file)?;
        lifecycle_profile::validate(&queue, &binding)?;
        return Ok(
            json!({"mailbox":name,"route":binding.route,"state":"existing","lifecycle":"read_only","runtime_verified":false}),
        );
    }
    if let Some(email) = options.email_recipient {
        validate_email(name, email)?;
    }
    if let Some(command) = options.relay_command {
        validate_command(command)?;
    }

    mkdir_private(&dir)?;
    let _owner = lock(&dir)?;
    let mut state = if absent(&state_path)? {
        let registration = if options.profiles_file.is_some() {
            registration(&configured(&home, name, options.profiles_file)?, &home)
        } else {
            home.join(REGISTRATION)
        };
        Registry {
            port: options.port.unwrap_or(DEFAULT_PORT),
            registration,
            relay_command: None,
            profiles: BTreeMap::new(),
            local_stdio: false,
            finite_cue_installation: None,
        }
    } else {
        read_json::<Registry>(&state_path)?
    };
    if options.local_stdio && !options.apply {
        return Err(error(
            "enable managed stdio with --apply on an active profile in this home",
        ));
    }
    if options.port.is_some_and(|value| value != state.port)
        || options.relay_command.is_some_and(|value| {
            state
                .relay_command
                .as_deref()
                .is_some_and(|old| value != old)
        })
    {
        return Err(error(
            "different lifecycle settings require an explicit separate home",
        ));
    }
    let before = state.clone();
    if state.relay_command.is_none()
        && options.email_recipient.is_none()
        && options.relay_command.is_some()
    {
        return Err(error(
            "select an email recipient when configuring the relay command",
        ));
    }
    if state.relay_command.is_none() {
        state.relay_command = options.relay_command.map(Path::to_path_buf);
    }
    if options.email_recipient.is_some() && state.relay_command.is_none() {
        return Err(error(
            "configure an owner-private relay command with the email recipient",
        ));
    }
    if selected {
        if !state.local_stdio || active_routes(&state).is_empty() {
            return Err(error(
                "enable local stdio on an active managed hub before connecting a peer",
            ));
        }
        let registrations = registration_data(&state.registration)?;
        let profile = state.profiles.get(name);
        if profile.is_some_and(|p| p.phase == Phase::Retired)
            || registrations
                .get(name)
                .is_some_and(|r| r["revoked"] != false)
            || (profile.is_none()
                && (!absent(
                    &home.join(
                        catalog::http_by_id(name)
                            .ok_or_else(|| error("unknown peer"))?
                            .policy_file,
                    ),
                )? || registrations.get(name).is_some()))
        {
            return Err(error(
                "Existing peer settings need review before connecting this mailbox",
            ));
        }
    }
    if let Some(previous) = state.profiles.get(name) {
        if let Some(manifest) = options.profiles_file {
            let input = configured(&home, name, Some(manifest))?;
            if Source::from(&input) != previous.source
                || registration(&input, &home) != state.registration
            {
                return Err(error(
                    "selected settings differ from the protected lifecycle profile",
                ));
            }
        }
        if previous.phase != Phase::Retired {
            if let (true, Some(jobs), Some(recipient)) = (selected, jobs, options.email_recipient)
                && previous.phase == Phase::Active
                && previous.email_recipient.is_none()
            {
                lifecycle_profile::validate(&queue, &previous.binding(name, &state.registration)?)?;
                let mut next = state.clone();
                next.profiles
                    .get_mut(name)
                    .ok_or_else(|| error("missing lifecycle profile"))?
                    .email_recipient = Some(recipient.to_owned());
                change(jobs, &home, &before, next, name, false)?;
                let active = read_json::<Registry>(&state_path)?;
                return Ok(response(name, &active.profiles[name], true));
            }
            if options
                .email_recipient
                .is_some_and(|value| Some(value) != previous.email_recipient.as_deref())
            {
                return Err(error(
                    "email recipient differs from the staged or active settings",
                ));
            }
            if previous.phase == Phase::Active {
                lifecycle_profile::validate(&queue, &previous.binding(name, &state.registration)?)?;
            }
        }
    }

    match state.profiles.get(name).cloned() {
        Some(previous) if previous.phase == Phase::Retired => {
            if generation::active_storage(&queue, name)
                .map_err(io::Error::other)?
                .is_some()
            {
                return Err(error(
                    "retired profile is inconsistent with its queue generation",
                ));
            }
            let newer = generation::new_storage();
            let credential_dir = dir.join(newer.replace(':', "-"));
            mkdir_private(&credential_dir)?;
            let (policy, token_file) = lifecycle_profile::stage_replacement(
                &credential_dir,
                &previous.binding(name, &state.registration)?,
            )?;
            state.profiles.insert(
                name.into(),
                Profile {
                    source: previous.source,
                    policy,
                    token_file: Some(token_file),
                    generation: Some(newer),
                    email_recipient: options.email_recipient.map(str::to_owned),
                    relay_after: None,
                    provisioned: previous.provisioned,
                    phase: Phase::Staged,
                },
            );
        }
        None if (fresh_profile(name) || selected)
            && options.profiles_file.is_none()
            && absent(&home.join(catalog::http_by_id(name).unwrap().policy_file))? =>
        {
            let profile = stage_fresh(&home, name, &state, options.email_recipient)?;
            state.profiles.insert(name.into(), profile);
        }
        None => {
            let binding = configured(&home, name, options.profiles_file)?;
            if registration(&binding, &home) != state.registration {
                return Err(error("profile uses a different protected registration"));
            }
            if generation::active_storage(&queue, name)
                .map_err(io::Error::other)?
                .as_deref()
                != Some(name)
            {
                return Err(error(
                    "profile generation has no matching lifecycle binding",
                ));
            }
            lifecycle_profile::validate(&queue, &binding)?;
            state.profiles.insert(
                name.into(),
                Profile {
                    source: Source::from(&binding),
                    policy: binding.policy,
                    token_file: None,
                    generation: None,
                    email_recipient: options.email_recipient.map(str::to_owned),
                    relay_after: None,
                    provisioned: false,
                    phase: Phase::Staged,
                },
            );
        }
        _ => {}
    }
    if state.profiles[name].phase == Phase::Staged {
        verify_staged(&state.profiles[name], name, &state.registration)?;
    }
    if state != before {
        write_json(&state_path, &state)?;
    }
    if let Some(jobs) = jobs {
        let mut next = state.clone();
        next.local_stdio |= options.local_stdio;
        let row = next
            .profiles
            .get_mut(name)
            .ok_or_else(|| error("missing lifecycle profile"))?;
        let update = row.phase == Phase::Staged;
        row.phase = Phase::Active;
        change(jobs, &home, &state, next, name, update)?;
        state = read_json(&state_path)?;
    }
    Ok(response(name, &state.profiles[name], jobs.is_some()))
}

pub(crate) fn remove(home: &Path, name: &str, apply: bool) -> io::Result<Value> {
    if apply {
        let (directory, executable, installation) = system_jobs()?;
        let jobs = Jobs {
            service: &SystemLaunchctl,
            directory: &directory,
            executable: &executable,
            installation: Some(&installation),
        };
        remove_with(home, name, Some(&jobs))
    } else {
        remove_with(home, name, None)
    }
}

fn remove_with(home: &Path, name: &str, jobs: Option<&Jobs<'_>>) -> io::Result<Value> {
    let home = private_dir(home)?;
    let queue = home.join(QUEUE);
    open_private(&queue)?;
    if !generation::supports(&queue).map_err(io::Error::other)? {
        return Err(error(
            "legacy queue lacks the generation catalog; removal is unavailable",
        ));
    }
    let dir = private_dir(&home.join(DIRECTORY))?;
    let state_path = dir.join(STATE);
    let _owner = lock(&dir)?;
    let state: Registry = read_json(&state_path)?;
    let profile = state
        .profiles
        .get(name)
        .ok_or_else(|| error("profile is not managed by this home"))?;
    let Some(jobs) = jobs else {
        return Ok(
            json!({"mailbox":name,"state": if profile.phase == Phase::Retired {"retired"} else {"remove_pending"}}),
        );
    };
    registration_data(&state.registration)?;
    let mut next = state.clone();
    let row = next
        .profiles
        .get_mut(name)
        .ok_or_else(|| error("missing lifecycle profile"))?;
    let changed = row.phase != Phase::Retired;
    row.phase = Phase::Retired;
    row.relay_after = None;
    row.email_recipient = None;
    change(jobs, &home, &state, next, name, changed)?;
    Ok(json!({"mailbox":name,"state":"retired"}))
}

pub(crate) fn check_manual_stage(home: &Path, mailbox: &str) -> io::Result<()> {
    let dir = home.join(DIRECTORY);
    if !absent(&dir)? {
        let state_path = dir.join(STATE);
        if !absent(&state_path)?
            && read_json::<Registry>(&state_path)?
                .profiles
                .contains_key(mailbox)
        {
            return Err(error("use managed init for this profile"));
        }
    }
    Ok(())
}

pub(crate) fn serve(home: &Path) -> io::Result<()> {
    let home = private_dir(home)?;
    let state: Registry = read_json(&home.join(DIRECTORY).join(STATE))?;
    let queue = home.join(QUEUE);
    let ports = state.port;
    let hosted = (|| {
        let owner = read_hosted_owner(&home).ok()??;
        let (id, scope) = hosted_scope(&home, &owner).ok()?;
        scope_matches(&state, &scope).then_some((id, scope))
    })();
    let local_stdio = state.local_stdio && !active_routes(&state).is_empty();
    let routes = state
        .profiles
        .into_iter()
        .filter(|(_, p)| p.phase == Phase::Active)
        .map(|(name, profile)| profile.binding(&name, &state.registration))
        .collect::<io::Result<Vec<_>>>()?;
    if let Some((id, scope)) = hosted {
        crate::local_http::serve_managed_hosted_hub(queue, routes, ports, id, &scope, local_stdio)
    } else {
        crate::local_http::serve_managed_hub(queue, routes, ports, local_stdio)
    }
}

#[cfg(test)]
#[path = "lifecycle_fresh_tests.rs"]
mod fresh_tests;
#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
