//! The optional private launchd companions.
use super::{Args, QUEUE};
use radio999k::{mailbox_catalog as catalog, private_file::MAX_PRIVATE_CONFIG_BYTES};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdin, Command},
    time::{Instant, timeout},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    version: u32,
    queue_identity: QueueIdentity,
    dot_tunnel: Option<Tunnel>,
    codex_tunnel: Option<Tunnel>,
    email_relay: Option<Relay>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueIdentity {
    device: u64,
    inode: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tunnel {
    executable: PathBuf,
    args: Vec<String>,
    working_directory: PathBuf,
    home: PathBuf,
    path: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relay {
    registration: PathBuf,
    state: PathBuf,
    command: PathBuf,
    mailboxes: Vec<String>,
    interval_secs: u64,
}

fn absolute(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|s| path.is_absolute() && !s.chars().any(char::is_control))
}

impl Manifest {
    pub(super) fn load(path: &Path) -> io::Result<Self> {
        let invalid = || io::Error::other("invalid_companions_file");
        if !absolute(path) {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        super::private(path)
            .map_err(|_| io::Error::other("companions_file_unavailable"))?
            .take(MAX_PRIVATE_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_PRIVATE_CONFIG_BYTES {
            return Err(invalid());
        }
        let value: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if value.version != 1
            || value.queue_identity.inode == 0
            || (value.dot_tunnel.is_none()
                && value.codex_tunnel.is_none()
                && value.email_relay.is_none())
        {
            return Err(invalid());
        }
        for tunnel in [&value.dot_tunnel, &value.codex_tunnel]
            .into_iter()
            .flatten()
        {
            if !absolute(&tunnel.executable)
                || !absolute(&tunnel.working_directory)
                || super::private_home(&tunnel.working_directory).is_err()
                || !absolute(&tunnel.home)
                || super::private_home(&tunnel.home).is_err()
                || tunnel.path.is_empty()
                || tunnel.path.iter().any(|dir| !absolute(dir))
                || std::env::join_paths(&tunnel.path).is_err()
                || tunnel
                    .args
                    .iter()
                    .any(|arg| arg.is_empty() || arg.chars().any(char::is_control))
            {
                return Err(invalid());
            }
        }
        if let Some(relay) = &value.email_relay
            && (!absolute(&relay.registration)
                || !absolute(&relay.state)
                || !absolute(&relay.command)
                || !(1..=3600).contains(&relay.interval_secs)
                || radio999k::arrival_notify::validate_static_mailboxes(&relay.mailboxes).is_err())
        {
            return Err(invalid());
        }
        Ok(value)
    }

    pub(super) fn queue_identity(&self) -> (u64, u64) {
        (self.queue_identity.device, self.queue_identity.inode)
    }

    pub(super) fn roles(&self, a: &Args) -> Result<Vec<Role>, &'static str> {
        let mut roles = Vec::new();
        for (name, tunnel, profile) in [
            (
                "dot_tunnel",
                &self.dot_tunnel,
                catalog::by_adapter(catalog::Adapter::Dot),
            ),
            ("codex_tunnel", &self.codex_tunnel, catalog::codex_http()),
        ] {
            if let Some(tunnel) = tunnel {
                let route = match &a.manifest {
                    Some(manifest) => manifest.route(profile),
                    None => super::configured(a, profile).then_some(profile.route),
                }
                .ok_or("companion_hub_profile_required")?;
                let target = format!("http://127.0.0.1:{}{route}", a.port);
                if !matches_tunnel(&tunnel.args, &target) {
                    return Err("companion_tunnel_args_invalid");
                }
                let mut role = Role::new(
                    name,
                    &tunnel.executable,
                    tunnel.args.iter().map(OsString::from).collect(),
                );
                role.environment = Some((
                    tunnel.working_directory.clone(),
                    tunnel.home.clone(),
                    std::env::join_paths(&tunnel.path).map_err(|_| "companion_path_invalid")?,
                ));
                roles.push(role);
            }
        }
        if let Some(relay) = &self.email_relay {
            let mut args = vec![
                "notify-arrivals".into(),
                "--queue".into(),
                a.home.join(QUEUE).into(),
                "--registration".into(),
                relay.registration.clone().into(),
                "--state".into(),
                relay.state.clone().into(),
                "--command".into(),
                relay.command.clone().into(),
                "--interval-secs".into(),
                relay.interval_secs.to_string().into(),
            ];
            for mailbox in &relay.mailboxes {
                args.push(format!("--mailbox={mailbox}").into());
            }
            roles.push(Role::new("email_relay", &a.binary, args));
        }
        Ok(roles)
    }
}

// Keep the approved credentials and other options. Require one exact upstream route.
fn matches_tunnel(args: &[String], target: &str) -> bool {
    let mut args = args.iter();
    if args.next().map(String::as_str) != Some("run") {
        return false;
    }
    let mut route = None;
    while let Some(arg) = args.next() {
        let (key, value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
        match key {
            "--mcp.server-url" => {
                let Some(value) = value.or_else(|| args.next().map(String::as_str)) else {
                    return false;
                };
                if route.replace(value).is_some() {
                    return false;
                }
            }
            "--control-plane.api-key" | "--mcp.extra-headers" | "--mcp.discovery-extra-headers" => {
                let Some(value) = value.or_else(|| args.next().map(String::as_str)) else {
                    return false;
                };
                let reference = if key == "--control-plane.api-key" {
                    value
                } else {
                    let Some((header, reference)) = value.split_once(':') else {
                        return false;
                    };
                    if header.is_empty()
                        || !header
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    {
                        return false;
                    }
                    reference.trim()
                };
                if !reference
                    .strip_prefix("file:")
                    .is_some_and(|p| absolute(Path::new(p)))
                {
                    return false;
                }
            }
            "--" | "--mcp.command" | "--embedded-mcp-stub" | "--embedded-stateless-mcp-stub" => {
                return false;
            }
            _ => {}
        }
    }
    route == Some(target)
}

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

pub(super) struct Role {
    name: &'static str,
    executable: PathBuf,
    args: Vec<OsString>,
    environment: Option<(PathBuf, PathBuf, OsString)>,
    process: Option<Running>,
    retry_at: Instant,
    delay: Duration,
}

struct Running {
    guard: Child,
    lease: ChildStdin,
    pid: u32,
    group: u32,
    started: Instant,
}

impl Role {
    fn new(name: &'static str, executable: &Path, args: Vec<OsString>) -> Self {
        Self {
            name,
            executable: executable.into(),
            args,
            environment: None,
            process: None,
            retry_at: Instant::now(),
            delay: RETRY_MIN,
        }
    }

    async fn spawn(&self, binary: &Path, lock: RawFd) -> io::Result<Running> {
        let mut command = Command::new(binary);
        command
            .arg("companion-guard")
            .arg(lock.to_string())
            .arg("--")
            .arg(&self.executable)
            .args(&self.args)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some((cwd, home, path)) = &self.environment {
            command
                .current_dir(cwd)
                .env_clear()
                .env("HOME", home)
                .env("PATH", path);
        }
        inherit_lock(&mut command, lock);
        let mut guard = command.spawn()?;
        let group = guard.id().expect("spawned guard");
        let lease = guard.stdin.take().expect("piped lease");
        let mut stdout = guard.stdout.take().expect("piped guard status");
        let mut pid = [0; 4];
        if !matches!(
            timeout(Duration::from_secs(2), stdout.read_exact(&mut pid)).await,
            Ok(Ok(_))
        ) {
            drop(lease);
            reap_group(&mut guard, group).await;
            return Err(io::Error::other("companion_spawn_failed"));
        }
        Ok(Running {
            guard,
            lease,
            pid: u32::from_ne_bytes(pid),
            group,
            started: Instant::now(),
        })
    }

    pub(super) async fn tick(&mut self, binary: &Path, lock: &std::fs::File) {
        if let Some(process) = &self.process {
            if !leader_exited(process.group) {
                return;
            }
            self.stop().await;
            self.retry();
        }
        if Instant::now() < self.retry_at {
            return;
        }
        match self.spawn(binary, lock.as_raw_fd()).await {
            Ok(process) => self.process = Some(process),
            Err(_) => self.retry(),
        }
    }

    fn retry(&mut self) {
        self.retry_at = Instant::now() + self.delay;
        self.delay = (self.delay * 2).min(RETRY_MAX);
        tracing::warn!(
            component = "cloud_service",
            role = self.name,
            outcome = "companion_retrying"
        );
    }

    pub(super) async fn stop(&mut self) {
        if let Some(Running {
            mut guard,
            lease,
            started,
            group,
            ..
        }) = self.process.take()
        {
            if started.elapsed() >= RETRY_MAX {
                self.delay = RETRY_MIN;
            }
            drop(lease);
            reap_group(&mut guard, group).await;
        }
    }

    pub(super) fn status(&self) -> (&'static str, Value) {
        (
            self.name,
            match &self.process {
                Some(running) => json!({"state":"running","pid":running.pid}),
                None if Instant::now() < self.retry_at => json!({"state":"retrying"}),
                None => json!({"state":"starting"}),
            },
        )
    }
}

async fn reap_group(guard: &mut Child, group: u32) {
    let end = Instant::now() + Duration::from_secs(1);
    while !leader_exited(group) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The unreaped guard owns this group ID even if it died before cleanup.
    signal_group(group, libc::SIGKILL);
    let _ = guard.wait().await;
}

pub(super) fn inherit_lock(command: &mut Command, lock: RawFd) {
    // SAFETY: fcntl is async-signal-safe. The child holds the lock until it exits.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(lock, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn lock_exec_flag(lock: RawFd, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl checks the validity of the integer descriptor.
    if lock < 3 || unsafe { libc::fcntl(lock, libc::F_SETFD, flag) } < 0 {
        return Err(io::Error::other("invalid_run_lock_descriptor"));
    }
    Ok(())
}

pub(crate) fn keep_hub_lease(lock: RawFd) -> io::Result<()> {
    lock_exec_flag(lock, libc::FD_CLOEXEC)?;
    std::thread::Builder::new()
        .name("hub-run-lease".into())
        .spawn(|| {
            let mut byte = [0];
            // This read blocks until the runner closes its only write end.
            let _ = io::stdin().read(&mut byte);
            std::process::exit(1);
        })?;
    Ok(())
}

pub(crate) fn guard(lock: RawFd, argv: &[OsString]) -> io::Result<()> {
    let invalid = || io::Error::other("invalid_companion_guard");
    let (program, args) = argv.split_first().ok_or_else(invalid)?;
    let mut lease = io::stdin().lock();
    // SAFETY: getpgrp/getpid have no preconditions. The role joins this group.
    let group = unsafe { libc::getpgrp() };
    if group <= 0 || group != unsafe { libc::getpid() } {
        return Err(invalid());
    }
    // SAFETY: The handler does nothing. The role's exec restores the default action.
    unsafe { libc::signal(libc::SIGTERM, keep_guard as *const () as libc::sighandler_t) };
    lock_exec_flag(lock, libc::FD_CLOEXEC)?;
    // SAFETY: stdin is the lease pipe. Its read end belongs to this process.
    if unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(invalid());
    }
    if !lease_open(&mut lease) {
        return Ok(());
    }
    let mut child = std::process::Command::new(program)
        .args(args)
        .process_group(group)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| io::Error::other("companion_spawn_failed"))?;
    let pid = child.id();
    let mut output = io::stdout().lock();
    let announced = output
        .write_all(&pid.to_ne_bytes())
        .and_then(|()| output.flush())
        .is_ok();
    while announced && lease_open(&mut lease) && !leader_exited(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    signal_group(group as u32, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(400));
    let _ = child.kill();
    let _ = child.wait();
    // Stop any descendants that ignored SIGTERM. This also ends this guard.
    signal_group(group as u32, libc::SIGKILL);
    Err(io::Error::other("companion_group_cleanup_failed"))
}

extern "C" fn keep_guard(_: libc::c_int) {}

fn signal_group(group: u32, signal: libc::c_int) {
    // SAFETY: The caller is the group leader or its parent and has not reaped it.
    unsafe { libc::kill(-(group as i32), signal) };
}

fn lease_open(lease: &mut impl Read) -> bool {
    match lease.read(&mut [0]) {
        Err(error) => matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ),
        Ok(0) => false, // The parent closed the pipe.
        Ok(_) => false, // The lease does not carry data.
    }
}

fn leader_exited(pid: u32) -> bool {
    // SAFETY: waitid initializes the info on a match. A zero pid also covers no match.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return io::Error::last_os_error().kind() != io::ErrorKind::Interrupted;
    }
    // SAFETY: waitid returns a SIGCHLD siginfo or leaves the zeroed pid.
    unsafe { info.si_pid() != 0 }
}

pub(super) fn status_snapshot(roles: &[Role]) -> Value {
    Value::Object(
        roles
            .iter()
            .map(|role| {
                let (name, state) = role.status();
                (name.into(), state)
            })
            .collect(),
    )
}

pub(super) async fn tick_and_status(roles: &mut [Role], a: &Args, lock: &std::fs::File) -> Value {
    for role in roles.iter_mut() {
        role.tick(&a.binary, lock).await;
    }
    status_snapshot(roles)
}
