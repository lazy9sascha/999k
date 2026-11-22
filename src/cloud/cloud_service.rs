//! Foreground hub supervisor.
use crate::{
    cloud_credentials::{self, App},
    cloud_transport::{Provider, Transport},
    mailbox_startup::{self, Manifest, QUEUE},
};
#[path = "cloud_health.rs"]
mod diagnostics;
#[path = "operator_health.rs"]
mod operator_health;
#[path = "run_companions.rs"]
mod run_companions;
pub(crate) use diagnostics::health;
use pigeonmq::mailbox_catalog as catalog;
pub(crate) use run_companions::{guard as companion_guard, keep_hub_lease};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    os::fd::AsRawFd,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
    time::{Instant, timeout},
};
#[derive(clap::Args)]
pub(crate) struct RunOptions {
    #[arg(long, default_value = ".local")]
    home: PathBuf,
    /// Enable generic HTTP and MCP routes with this private policy.
    #[arg(long)]
    auth_policy: Option<PathBuf>,
    #[arg(long, default_value_t = 54843, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// Read tunnel settings from an explicit private file instead of the process environment.
    #[arg(long, value_name = "PATH")]
    env_file: Option<PathBuf>,
    /// Mount only the existing adapters listed in a private JSON profile manifest.
    #[arg(long, value_name = "PATH", conflicts_with = "auth_policy")]
    profiles_file: Option<PathBuf>,
    /// Supervise private companions from this owner-private JSON file. macOS only.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["local_only", "env_file"])]
    companions_file: Option<PathBuf>,
    /// For loopback testing only. Ignore tunnel variables and any env file.
    #[arg(long)]
    local_only: bool,
    /// Serve GET /health on pigeonmq-cloud-health.sock in the private hub home.
    ///
    /// Use curl --unix-socket <home>/pigeonmq-cloud-health.sock http://localhost/health.
    #[arg(long)]
    operator_health: bool,
}

struct Args {
    home: PathBuf,
    binary: PathBuf,
    auth_policy: Option<PathBuf>,
    port: u16,
    transport: Transport,
    manifest: Option<Manifest>,
    companions: Option<run_companions::Manifest>,
    delivery_enabled: Value,
    health_public_paths: Value,
    watchdog_interval: Duration,
    operator_health: bool,
}

fn queue_identity(a: &Args) -> Result<(u64, u64), &'static str> {
    let queue = private(&a.home.join(QUEUE))?
        .metadata()
        .map_err(|_| "queue_identity_unavailable")?;
    Ok((queue.dev(), queue.ino()))
}

fn private(path: &Path) -> Result<fs::File, &'static str> {
    pigeonmq::private_file::open_private_file(path).map_err(|_| "unsafe_private_path")
}
fn private_home(home: &Path) -> std::io::Result<fs::File> {
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(home)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::other("owner-private hub home required"));
    }
    Ok(directory)
}
const RUN_LOCK: &str = "pigeonmq-cloud-run.lock";
const STATE_FILE: &str = "pigeonmq-cloud-state.json";

fn runner_lock(home: &Path) -> std::io::Result<fs::File> {
    private_home(home)?;
    let path = home.join(RUN_LOCK);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // FIXME(yagni): empty run lock reopened with nlink/uid/0600 checks - flock alone is enough; a 0644 lock stops the hub
    let file = private(&path).map_err(std::io::Error::other)?;
    // SAFETY: the live descriptor owns the lock until this File closes, including on process exit.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}
fn record(home: &Path, mut value: Value) -> Result<(), &'static str> {
    private_home(home).map_err(|_| "state_write_failed")?;
    value["updated_at_utc"] =
        json!(chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339());
    let p = home.join(format!(".pigeonmq-cloud-state-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)?;
        file.write_all(serde_json::to_string(&value)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&p, home.join(STATE_FILE))?;
        Ok::<_, std::io::Error>(())
    })();
    let _ = fs::remove_file(&p);
    result.map_err(|_| "state_write_failed")
}
fn policy(a: &Args, profile: &catalog::HttpProfile) -> Option<PathBuf> {
    mailbox_startup::selected_policy(&a.home, a.manifest.as_ref(), profile)
}
fn configured(a: &Args, profile: &catalog::HttpProfile) -> bool {
    policy(a, profile).is_some()
}

fn host_command(a: &Args) -> Command {
    let mut c = Command::new(&a.binary);
    c.arg("serve-hub")
        .arg("--queue")
        .arg(a.home.join(QUEUE))
        .args(["--port", &a.port.to_string(), "--continuous"]);
    if let Some(manifest) = &a.manifest {
        c.arg("--profiles-file")
            .arg(manifest.source())
            .args(["--profiles-fingerprint", manifest.fingerprint()]);
    } else {
        c.arg("--bundled-profiles");
    }
    if let Some(p) = &a.auth_policy {
        c.arg("--auth-policy").arg(p);
    }
    c
}
fn profile(v: &Value, a: &Args) -> Result<(), &'static str> {
    let bindings = mailbox_startup::bindings(&a.home, a.manifest.as_ref());
    let events = match bindings
        .iter()
        .find(|binding| binding.descriptor == catalog::codex_http())
    {
        Some(binding) => crate::auth::codex_events(
            &binding.policy,
            binding
                .registration
                .as_deref()
                .ok_or("unexpected_host_profile")?,
        )
        .map_err(|_| "unexpected_host_profile")?
        .is_some(),
        None => false,
    };
    if v["transport"] != "hub-local-http"
        || v["profiles_fingerprint"].as_str() != a.manifest.as_ref().map(Manifest::fingerprint)
        || v["generic_enabled"].as_bool() != Some(a.auth_policy.is_some())
        || v["codex_events_enabled"].as_bool() != Some(events)
        || catalog::http_profiles().iter().any(|profile| {
            v[profile.enabled_key].as_bool()
                != Some(bindings.iter().any(|binding| binding.descriptor == profile))
        })
    {
        return Err("unexpected_host_profile");
    }
    Ok(())
}

fn add_public_endpoints(value: &mut Value, paths: &Value, origin: &str) {
    for profile in catalog::http_profiles() {
        if let Some(key) = profile.endpoint_key {
            value[key] = json!(paths[profile.id].as_str().map(|path| format!("{origin}{path}")));
        }
    }
}

// Process diagnostics never format an I/O error or relay a child's output.
fn process_io_failure(stage: &'static str, error: &io::Error) {
    tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage, outcome = "io_failed", io_kind = ?error.kind(), os_error = error.raw_os_error());
}

fn process_exited(stage: &'static str, status: ExitStatus) {
    tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage, outcome = "exited", exit_code = status.code(), signal = status.signal());
}

fn process_timeout(stage: &'static str) {
    tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage, outcome = "timed_out");
}

fn host_exited(host: &mut Child, stage: &'static str) -> bool {
    match host.try_wait() {
        Ok(None) => return false,
        Ok(Some(status)) => process_exited(stage, status),
        Err(error) => process_io_failure(stage, &error),
    }
    true
}

async fn preflight(a: &Args) -> Result<(), &'static str> {
    let queue = a.home.join(QUEUE);
    private(&queue)?;
    let bindings = mailbox_startup::bindings(&a.home, a.manifest.as_ref());
    if bindings.is_empty() {
        return Err("cloud_adapter_policy_required");
    }
    for binding in &bindings {
        if let Some(app) = App::for_profile(binding.descriptor) {
            private(&binding.policy)?;
            cloud_credentials::provision(&binding.policy, app).map_err(|error| {
                if error.get_ref().is_some_and(|e| e.is::<crate::auth::RetiredSenders>()) {
                    tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
                        stage = "policy", outcome = "rejected", profile = app.slug(),
                        error_code = crate::auth::RETIRED_SENDERS_CODE, guidance = crate::auth::RETIRED_SENDERS_HELP);
                    return crate::auth::RETIRED_SENDERS_CODE;
                }
                tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
                    stage = "credentials", outcome = "failed", profile = app.slug(),
                    error_code = "cloud_credential_provision_failed");
                "cloud_credential_provision_failed"
            })?;
        }
    }
    let mut muse = None;
    for binding in &bindings {
        let policy = &binding.policy;
        match binding.descriptor.adapter {
            catalog::Adapter::Muse => muse = Some(policy),
            catalog::Adapter::Dot => {
                pigeonmq::dot_profile::check(&queue, policy, false)
                    .map_err(|_| "dot_policy_or_existing_queue_unavailable")?;
            }
            catalog::Adapter::Grok => {
                if !crate::local_http::check_grok(&queue, policy)
                    .map_err(|_| "grok_policy_or_existing_queue_unavailable")?
                {
                    return Err("grok_policy_or_existing_queue_unavailable");
                }
            }
            catalog::Adapter::SharedMailbox => {
                let profile = binding
                    .mailbox(&queue)
                    .ok_or("codex_policy_or_existing_queue_unavailable")?;
                profile
                    .validate()
                    .map_err(|_| "codex_policy_or_existing_queue_unavailable")?;
                if profile
                    .definition()
                    .is_ok_and(|profile| profile.events == catalog::EventsCapability::Codex)
                    && let Some(events) = crate::codex_events::Runtime::new(&profile)
                        .map_err(|_| "codex_events_configuration_unavailable")?
                {
                    events
                        .check()
                        .map_err(|_| "codex_events_storage_unprepared")?;
                }
            }
            catalog::Adapter::Receiver => {
                private(policy)?;
                crate::auth::validate_private(policy).map_err(|_| "receiver_policy_unavailable")?;
                let registration = binding
                    .registration
                    .as_deref()
                    .ok_or("receiver_registration_unavailable")?;
                pigeonmq::registration::load_registrations(registration)
                    .map_err(|_| "receiver_registration_unavailable")?;
                pigeonmq::Queue::open_existing(&queue)
                    .map_err(|_| "receiver_existing_queue_unavailable")?;
            }
        }
    }
    if let Some(policy) = &a.auth_policy {
        if !policy.is_absolute() {
            return Err("generic_policy_absolute_path_required");
        }
        private(policy)?;
    }
    let Some(muse) = muse else { return Ok(()) };
    private(muse)?;
    let mut c = Command::new(&a.binary);
    c.arg("muse-check")
        .arg("--queue")
        .arg(a.home.join(QUEUE))
        .arg("--auth-policy")
        .arg(muse)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = match timeout(Duration::from_secs(15), c.status()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            process_io_failure("muse_preflight", &error);
            return Err("transient_preflight");
        }
        Err(_) => {
            process_timeout("muse_preflight");
            return Err("transient_preflight");
        }
    };
    if !status.success() {
        process_exited("muse_preflight", status);
        return Err("policy_or_existing_queue_unavailable");
    }
    Ok(())
}
fn public_health_paths(a: &Args, readiness: &Value) -> Value {
    let mut paths = serde_json::Map::new();
    for entry in catalog::http_profiles() {
        if entry.adapter == catalog::Adapter::Receiver || readiness[entry.enabled_key] != true {
            continue;
        }
        let route = match &a.manifest {
            Some(manifest) => manifest.route(entry),
            None => Some(entry.route),
        };
        if let Some(path) = route {
            paths.insert(entry.id.to_owned(), json!(path));
        }
    }
    if readiness["generic_enabled"] == true {
        paths.insert("generic".to_owned(), json!("/mcp"));
    }
    Value::Object(paths)
}

fn status(a: &Args, mut value: Value) {
    value["tunnel_mode"] = json!(a.transport.mode());
    value["delivery_enabled"] = a.delivery_enabled.clone();
    if value["local_hub"] == "running" {
        value["local_port"] = json!(a.port);
        if matches!(a.transport.mode(), "ssh" | "external") {
            value["health_public_paths"] = a.health_public_paths.clone();
        }
    }
    if record(&a.home, value).is_err() {
        tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "status_write", outcome = "failed", error_code = "state_write_failed");
    }
}
fn local_state(a: &Args, host: &Child, tunnel: &str, reason: Option<&str>) {
    if let Some(error_code) = reason {
        tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "tunnel", outcome = tunnel, error_code);
    } else {
        tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "tunnel", outcome = tunnel);
    }
    status(
        a,
        json!({"state":"degraded","local_hub":"running","tunnel":tunnel,
        "host_pid":host.id(),"tunnel_pid":null,"supervisor_pid":std::process::id(),
        "reason":reason,"public_origin":null}),
    );
}
async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        // The PID belongs to the child spawned by this process.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}

const TUNNEL_RETRY_MIN: Duration = Duration::from_secs(1);
const TUNNEL_RETRY_MAX: Duration = Duration::from_secs(30);
const TUNNEL_STABLE_FOR: Duration = Duration::from_secs(30);

struct ManagedTunnel {
    public_origin: Option<String>,
    stable_after: Option<Instant>,
    retry_delay: Duration,
}

impl ManagedTunnel {
    fn new() -> Self {
        Self {
            public_origin: None,
            stable_after: None,
            retry_delay: TUNNEL_RETRY_MIN,
        }
    }

    fn clear_connection(&mut self) {
        self.public_origin = None;
        self.stable_after = None;
    }

    fn ready(&mut self, origin: String, now: Instant) {
        self.public_origin = Some(origin);
        self.stable_after = Some(now + TUNNEL_STABLE_FOR);
    }

    fn observe_running(&mut self, now: Instant) {
        // A provider announcement alone does not rule out a crash loop.
        if self.stable_after.is_some_and(|deadline| now >= deadline) {
            self.retry_delay = TUNNEL_RETRY_MIN;
            self.stable_after = None;
        }
    }

    fn next_retry_delay(&mut self) -> Duration {
        let delay = self.retry_delay;
        self.retry_delay = (delay * 2).min(TUNNEL_RETRY_MAX);
        delay
    }
}

// The error boolean controls tunnel retries within this foreground runner.
async fn supervise(
    a: &Args,
    provider: Provider,
    host: &mut Child,
    tunnel: &mut Child,
    managed: &mut ManagedTunnel,
    term: &mut tokio::signal::unix::Signal,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<String, (&'static str, bool)> {
    let mut stdout = tunnel
        .stdout
        .take()
        .ok_or(("transient_tunnel_start", true))?;
    let mut stderr = tunnel
        .stderr
        .take()
        .ok_or(("transient_tunnel_start", true))?;
    let mut out = [0; 4096];
    let mut err = [0; 4096];
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let start = Instant::now();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = term.recv() => return Ok("stopped".into()),
            _ = interrupt.recv() => return Ok("stopped".into()),
            _ = tick.tick() => {
                let stage = if managed.public_origin.is_some() { "transient_connected" } else { "transient_tunnel_start" };
                // The outer runner records a host exit when it checks this result.
                if host.try_wait().map_err(|error| {
                    process_io_failure("host_wait", &error);
                    (stage, true)
                })?.is_some() {
                    return Err((stage, true));
                }
                if let Some(status) = tunnel.try_wait().map_err(|error| {
                    process_io_failure("tunnel_wait", &error);
                    (stage, true)
                })? {
                    process_exited("tunnel_wait", status);
                    let mut tail = Vec::new();
                    let _ = timeout(Duration::from_millis(100), (&mut stderr).take(8192).read_to_end(&mut tail)).await;
                    errors.extend_from_slice(&tail);
                    let text = String::from_utf8_lossy(&errors).to_ascii_lowercase();
                    let rejected = ["host key verification failed", "remote host identification has changed", "permission denied"]
                        .iter().any(|value| text.contains(value));
                    return Err(if rejected { ("ssh_auth_or_host_key_rejected", false) } else { (stage, true) });
                }
                managed.observe_running(Instant::now());
                if managed.public_origin.is_none() && start.elapsed() > Duration::from_secs(30) {
                    process_timeout("tunnel_ready");
                    return Err(("transient_tunnel_start", true));
                }
            },
            n = stderr.read(&mut err), if stderr_open => {
                let n = n.map_err(|error| {
                    process_io_failure("tunnel_stderr", &error);
                    ("transient_tunnel_start", true)
                })?;
                if n == 0 {
                    stderr_open = false;
                } else {
                    errors.extend_from_slice(&err[..n]);
                    if errors.len() > 8192 { errors.drain(..errors.len() - 8192); }
                }
            },
            n = stdout.read(&mut out), if stdout_open => {
                let n = n.map_err(|error| {
                    process_io_failure("tunnel_stdout", &error);
                    ("transient_tunnel_start", true)
                })?;
                if n == 0 { stdout_open = false; continue; }
                if managed.public_origin.is_some() { continue; }
                lines.extend_from_slice(&out[..n]);
                if lines.len() > 16384 { return Err(("unexpected_tunnel_output", false)); }
                while let Some(i) = lines.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = lines.drain(..=i).collect();
                    if let Ok(value) = serde_json::from_slice(&line)
                        && let Some(url) = provider.stable_origin(&value).map_err(|error| (error, false))? {
                        let mut state = json!({
                            "state": "connected", "local_hub":"running", "tunnel":"connected", "public_origin": url,
                            "tunnel_provider": provider.name,
                            "host_pid": host.id(), "tunnel_pid": tunnel.id(), "supervisor_pid": std::process::id(),
                            "generic_enabled": a.auth_policy.is_some()
                        });
                        add_public_endpoints(&mut state, &a.health_public_paths, &url);
                        status(a, state);
                        tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
                            stage = "tunnel", outcome = "connected");
                        println!("{}", json!({"state": "connected", "public_origin": url}));
                        managed.ready(url, Instant::now());
                        lines.clear();
                        break;
                    }
                }
            }
        }
    }
}

fn host_read_error(error: io::Error) -> &'static str {
    process_io_failure("host_ready", &error);
    if error.kind() == std::io::ErrorKind::InvalidData {
        "unexpected_host_profile"
    } else {
        "transient_host_start"
    }
}
async fn run(a: Args) -> i32 {
    run_with(
        a,
        |a, provider| provider.ssh_command(&a.home, a.port),
        std::future::pending(),
    )
    .await
}
async fn run_with(
    mut a: Args,
    mut tunnel_command: impl FnMut(&Args, Provider) -> Command,
    shutdown: impl std::future::Future<Output = ()>,
) -> i32 {
    let lock = match runner_lock(&a.home) {
        Ok(file) => file,
        Err(error) => {
            process_io_failure("runner_lock", &error);
            println!(
                "{}",
                json!({"state":"disconnected","reason":"cloud_runner_owned_or_home_unavailable"})
            );
            return 1;
        }
    };
    status(
        &a,
        json!({"state":"starting","local_hub":"stopped","tunnel":"stopped","public_origin":null}),
    );
    let mut roles = match a.companions.as_ref().map(|m| m.roles(&a)).transpose() {
        Ok(roles) => roles.unwrap_or_default(),
        Err(reason) => return finish(&a, reason, None, false),
    };
    // Keep the operator socket in the supervisor, separate from the tunneled child.
    let _operator_health = if a.operator_health {
        match operator_health::Server::start(&a.home).await {
            Ok(server) => Some(server),
            Err(error) => {
                process_io_failure("operator_health_bind", &error);
                return finish(&a, "operator_health_socket_unavailable", None, false);
            }
        }
    } else {
        None
    };
    let tunnel_mode = a.transport.mode();
    tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage = "run", outcome = "started", tunnel_mode);
    tokio::pin!(shutdown);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal registration");
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("signal registration");
    let mut expected_queue = a
        .companions
        .as_ref()
        .map(run_companions::Manifest::queue_identity);
    let mut backoff = Duration::from_millis(250);
    let mut managed = ManagedTunnel::new();
    let mut watchdog = diagnostics::Watchdog::default();
    loop {
        if let Some(expected) = &expected_queue {
            match queue_identity(&a) {
                Ok(current) if &current == expected => {}
                _ => {
                    return finish(&a, "queue_identity_changed_restart_required", None, false);
                }
            }
        }
        let checked = tokio::select! { result=preflight(&a)=>result, _=term.recv()=>Err("stopped"), _=interrupt.recv()=>Err("stopped"), _=&mut shutdown=>Err("stopped") };
        if let Err(reason) = checked {
            tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "preflight", outcome = if reason == "stopped" { "stopped" } else { "failed" },
            error_code = reason);
            return finish(&a, reason, None, reason.starts_with("transient_"));
        }
        tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage = "preflight", outcome = "completed");
        if expected_queue.is_none() {
            expected_queue = match queue_identity(&a) {
                Ok(identity) => Some(identity),
                Err(reason) => return finish(&a, reason, None, false),
            };
        }
        if a.companions.is_some() && queue_identity(&a).ok() != expected_queue {
            return finish(&a, "queue_identity_changed_restart_required", None, false);
        }
        let mut command = host_command(&a);
        if a.companions.is_some() {
            let fd = lock.as_raw_fd();
            run_companions::inherit_lock(&mut command, fd);
            command
                .arg("--run-lease-lock")
                .arg(fd.to_string())
                .stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut host = match command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(error) => {
                process_io_failure("host_spawn", &error);
                status(
                    &a,
                    json!({"state":"recovering","local_hub":"stopped","tunnel":"stopped","public_origin":null,"reason":"transient_host_start"}),
                );
                let stopped = tokio::select! {
                    _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                    _=tokio::time::sleep(backoff)=>false
                };
                if stopped {
                    return finish(&a, "stopped", None, false);
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
        stage = "host", outcome = "spawned");
        let mut host_output = BufReader::new(host.stdout.take().unwrap());
        let startup = async {
            let mut line = String::new();
            if (&mut host_output)
                .take(4097)
                .read_line(&mut line)
                .await
                .map_err(host_read_error)?
                == 0
            {
                tracing::warn!(target: "pigeonmq::cloud_service", component = "cloud_service",
                    stage = "host_ready", outcome = "eof");
                return Err("transient_host_start");
            }
            if line.len() > 4096 {
                return Err("unexpected_host_profile");
            }
            let readiness: Value =
                serde_json::from_str(&line).map_err(|_| "unexpected_host_profile")?;
            profile(&readiness, &a)?;
            if a.companions.is_some() {
                diagnostics::probe_owned_loopback(a.port, host.id())
                    .await
                    .map_err(|_| "transient_host_start")?;
            }
            Ok((
                json!({
                    "grok_wake": readiness["grok_wake_enabled"].as_bool(),
                    "dot_events": readiness["dot_events_enabled"].as_bool(),
                    "codex_events": readiness["codex_events_enabled"].as_bool(),
                }),
                public_health_paths(&a, &readiness),
            ))
        };
        let mut tunnel = None;
        managed.clear_connection();
        let started = tokio::select! { result=timeout(Duration::from_secs(10),startup)=>result, _=term.recv()=>Ok(Err("stopped")), _=interrupt.recv()=>Ok(Err("stopped")), _=&mut shutdown=>Ok(Err("stopped")) };
        let result = match started {
            Err(_) => {
                process_timeout("host_ready");
                Err(("transient_host_start", true))
            }
            Ok(Err(e)) => {
                if e == "transient_host_start" {
                    host_exited(&mut host, "host_start");
                }
                Err((e, e.starts_with("transient_")))
            }
            Ok(Ok((delivery_enabled, health_public_paths))) => {
                a.delivery_enabled = delivery_enabled;
                a.health_public_paths = health_public_paths;
                tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
                stage = "host", outcome = "ready");
                let supervise_ready_host = async {
                    match a.transport.managed_provider() {
                        None => {
                            let value = json!({"state":"local_ready","local_hub":"running","tunnel":if a.companions.is_some() {"private_companions"} else {"externally_managed"},
                    "public_origin":null,"host_pid":host.id(),"tunnel_pid":null,"supervisor_pid":std::process::id(),
                    "local_origin":format!("http://127.0.0.1:{}",a.port),"runtime_verified":false});
                            // Keep externally configured addresses out of supervisor stdout and logs.
                            let mut private_state = value.clone();
                            if let Some(origin) = a.transport.configured_origin() {
                                private_state["public_origin"] = json!(origin);
                                private_state["public_origin_source"] = json!("configured");
                                private_state["remote_verified"] = json!(false);
                                add_public_endpoints(&mut private_state, &a.health_public_paths, origin);
                            }
                            if a.companions.is_some() {
                                private_state["companions"] = run_companions::status_snapshot(&roles);
                            }
                            status(&a, private_state.clone());
                            println!("{value}");
                            loop {
                                if host_exited(&mut host, "host_wait") {
                                    break Err(("transient_host_exit", true));
                                }
                                if a.companions.is_some() {
                                    let stopped = tokio::select! {
                                        biased;
                                        _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                                        _=std::future::ready(())=>false
                                    };
                                    if stopped {
                                        break Ok("stopped".into());
                                    }
                                    if queue_identity(&a).ok() != expected_queue {
                                        break Err((
                                            "queue_identity_changed_restart_required",
                                            false,
                                        ));
                                    }
                                    let next = run_companions::tick_and_status(&mut roles, &a, &lock).await;
                                    if next != private_state["companions"] {
                                        private_state["companions"] = next;
                                        status(&a, private_state.clone());
                                    }
                                }
                                let stopped = tokio::select! {
                                    _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                                    _=tokio::time::sleep(Duration::from_millis(100))=>false
                                };
                                if stopped {
                                    break Ok("stopped".into());
                                }
                            }
                        }
                        Some(provider) => {
                            let mut retry = true;
                            loop {
                                if host_exited(&mut host, "host_wait") {
                                    break Err(("transient_host_exit", true));
                                }
                                if !retry {
                                    // A rejected SSH credential must not take down the local API.
                                    let stopped = tokio::select! {
                                        _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                                        _=tokio::time::sleep(Duration::from_millis(100))=>false
                                    };
                                    if stopped {
                                        break Ok("stopped".into());
                                    }
                                    continue;
                                }
                                if let Err(reason) = provider.check_local_files(&a.home) {
                                    local_state(&a, &host, "failed", Some(reason));
                                    retry = false;
                                    continue;
                                }
                                local_state(&a, &host, "starting", None);
                                managed.clear_connection();
                                let attempt = match tunnel_command(&a, provider)
                                    .stdin(Stdio::null())
                                    .stdout(Stdio::piped())
                                    .stderr(Stdio::piped())
                                    .kill_on_drop(true)
                                    .spawn()
                                {
                                    Err(error) => {
                                        process_io_failure("tunnel_spawn", &error);
                                        Err(("transient_tunnel_start", true))
                                    }
                                    Ok(child) => {
                                        tunnel = Some(child);
                                        tokio::select! {
                                            result=supervise(&a, provider, &mut host, tunnel.as_mut().unwrap(), &mut managed, &mut term, &mut interrupt)=>result,
                                            _=&mut shutdown=>Ok("stopped".into())
                                        }
                                    }
                                };
                                if let Some(mut child) = tunnel.take() {
                                    terminate(&mut child).await;
                                }
                                match attempt {
                                    // A requested stop wins even if the host exits during tunnel cleanup.
                                    Ok(result) => break Ok(result),
                                    Err((reason, should_retry)) => {
                                        if host_exited(&mut host, "host_wait") {
                                            break Err(("transient_host_exit", true));
                                        }
                                        retry = should_retry;
                                        local_state(
                                            &a,
                                            &host,
                                            if retry { "retrying" } else { "failed" },
                                            Some(reason),
                                        );
                                    }
                                }
                                if retry {
                                    let delay = managed.next_retry_delay();
                                    let stopped = tokio::select! {
                                        biased;
                                        _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                                        result=host.wait()=>{
                                            match result {
                                                Ok(status) => process_exited("host_wait", status),
                                                Err(error) => process_io_failure("host_wait", &error),
                                            }
                                            break Err(("transient_host_exit", true));
                                        },
                                        _=tokio::time::sleep(delay)=>false
                                    };
                                    if stopped {
                                        break Ok("stopped".into());
                                    }
                                }
                            }
                        }
                    }
                };
                tokio::select! {
                    biased;
                    result = supervise_ready_host => result,
                    () = watchdog.wait_for_failure(a.port, a.watchdog_interval) => Err(("transient_host_unhealthy", true)),
                }
            }
        };
        if let Some(c) = tunnel.as_mut() {
            terminate(c).await
        }
        if !roles.is_empty() {
            status(
                &a,
                json!({"state":"stopping","local_hub":"stopping","tunnel":"stopped","public_origin":null}),
            );
            for role in &mut roles {
                role.stop().await;
            }
        }
        terminate(&mut host).await;
        if let Err((
            reason @ ("transient_host_start" | "transient_host_exit" | "transient_host_unhealthy"),
            true,
        )) = result
        {
            status(
                &a,
                json!({"state":"recovering","local_hub":"stopped","tunnel":"stopped","public_origin":null,"reason":reason}),
            );
            tracing::warn!(target: "pigeonmq::cloud_service", component="cloud_service",
            stage="host_recovery", outcome="retrying", error_code=reason);
            let stopped = tokio::select! {
                _=term.recv()=>true, _=interrupt.recv()=>true, _=&mut shutdown=>true,
                _=tokio::time::sleep(backoff)=>false
            };
            if stopped {
                return finish(&a, "stopped", None, false);
            }
            backoff = (backoff * 2).min(Duration::from_secs(5));
            continue;
        }
        let (reason, retryable) = match result {
            Ok(_) => ("stopped", false),
            Err((r, retry)) => (r, retry),
        };
        return finish(&a, reason, managed.public_origin, retryable);
    }
}

fn finish(a: &Args, reason: &str, public_origin: Option<String>, retryable: bool) -> i32 {
    if reason == "stopped" {
        tracing::info!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "run", outcome = "stopped");
    } else {
        tracing::error!(target: "pigeonmq::cloud_service", component = "cloud_service",
            stage = "run", outcome = "failed", error_code = reason, retry = retryable);
    }
    status(
        a,
        json!({"state":"disconnected","local_hub":"stopped","tunnel":"stopped","reason":reason,"public_origin":public_origin,
            "host_pid":null,"tunnel_pid":null,"supervisor_pid":std::process::id()}),
    );
    println!("{}", json!({"state":"disconnected","reason":reason}));
    i32::from(reason != "stopped")
}
pub(crate) fn start(options: &RunOptions) -> std::io::Result<()> {
    let companions = options
        .companions_file
        .as_deref()
        .map(run_companions::Manifest::load)
        .transpose()?;
    let transport = if companions.is_some() {
        if !cfg!(target_os = "macos") || !options.home.is_absolute() {
            return Err(io::Error::other(
                "private_companions_require_macos_and_absolute_home",
            ));
        }
        Transport::PrivateCompanions
    } else {
        Transport::select(options.local_only, options.env_file.as_deref())
            .map_err(io::Error::other)?
    };
    let args = Args {
        home: options.home.clone(),
        binary: std::env::current_exe()?,
        auth_policy: options.auth_policy.clone(),
        port: options.port,
        transport,
        delivery_enabled: Value::Null,
        health_public_paths: Value::Null,
        watchdog_interval: diagnostics::WATCH_INTERVAL,
        operator_health: options.operator_health,
        companions,
        manifest: options
            .profiles_file
            .as_deref()
            .map(|path| Manifest::load(path, &options.home))
            .transpose()?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(run(args));
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_transport;
    use crate::mailbox_startup::REGISTRATION;
    use std::os::unix::fs::PermissionsExt;
    fn fixture(script: &str) -> (tempfile::TempDir, Args) {
        let d = tempfile::tempdir().unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for name in [
            "pigeonmq-cloud-fixture",
            "pigeonmq-cloud-muse-policy.json",
            "pigeonmq-cloud-queue.sqlite3",
            "pigeonmq-cloud-tunnel-ed25519",
            "pigeonmq-cloud-tunnel-known_hosts",
        ] {
            let p = d.path().join(name);
            fs::write(
                &p,
                match name {
                    "pigeonmq-cloud-fixture" => script,
                    "pigeonmq-cloud-muse-policy.json" => r#"{"token_sha256":"fixture"}"#,
                    _ => "fixture",
                },
            )
            .unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let a = Args {
            home: d.path().to_owned(),
            binary: d.path().join("pigeonmq-cloud-fixture"),
            auth_policy: None,
            port: 54843,
            transport: Transport::Managed(Provider::named("tunnl").unwrap()),
            manifest: None,
            companions: None,
            delivery_enabled: Value::Null,
            health_public_paths: Value::Null,
            watchdog_interval: diagnostics::WATCH_INTERVAL,
            operator_health: false,
        };
        (d, a)
    }
    #[tokio::test]
    async fn watchdog_restarts_a_hung_owned_child_once() {
        let (_directory, mut a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\nroot=${0%/*}\nprintf '%s\\n' \"$$\" >> \"$root/host-starts\"\nprintf '%s\\n' '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}'\nexec /bin/sleep 30\n",
        );
        a.transport = Transport::LocalOnly;
        a.watchdog_interval = Duration::from_millis(40);
        let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        a.port = port.local_addr().unwrap().port();
        drop(port);
        let starts = a.home.join("host-starts");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with(a, |_, _| panic!("local-only"), async {
            let _ = stopped.await;
        }));
        let count = || {
            fs::read_to_string(&starts)
                .unwrap_or_default()
                .lines()
                .count()
        };
        timeout(Duration::from_secs(4), async {
            while count() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(count(), 2);
        stop.send(()).unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn retired_senders_preflight_is_actionable_and_does_not_rewrite_policies() {
        for profile in [
            catalog::by_adapter(catalog::Adapter::Muse),
            catalog::by_adapter(catalog::Adapter::Grok),
        ] {
            let (_dir, a) = fixture("#!/bin/sh\nexit 99\n");
            let path = a.home.join(profile.policy_file);
            let policy = br#"{"token_sha256":"fixture","senders":null}"#;
            fs::write(&path, policy).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(preflight(&a).await, Err(crate::auth::RETIRED_SENDERS_CODE));
            assert_eq!(fs::read(&path).unwrap(), policy);
        }
    }

    fn codex_fixture() -> (tempfile::TempDir, Args) {
        let (dir, a) = fixture("#!/bin/sh\nexit 0\n");
        fs::remove_file(
            a.home
                .join(catalog::by_adapter(catalog::Adapter::Muse).policy_file),
        )
        .unwrap();
        fs::remove_file(a.home.join(QUEUE)).unwrap();
        pigeonmq::Queue::open(a.home.join(QUEUE)).unwrap();
        for (name, value) in [
            (
                catalog::codex_http().policy_file,
                json!({"principals":[{"id":"codex-cloud","token_sha256":"a".repeat(64),
                "operations":["read","wait","reply","send"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}]}),
            ),
            (
                catalog::by_adapter(catalog::Adapter::Receiver).policy_file,
                json!({"principals":[{"id":"codex-local","token_sha256":"b".repeat(64),
                "operations":["receive","reply"],"mailboxes":["codex-local"],"send_to":[],"revoked":false}]}),
            ),
            (
                REGISTRATION,
                json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},
                "codex-local":{"mailbox":"codex-local","revoked":false},"peer":{"mailbox":"peer","revoked":false}}),
            ),
        ] {
            let path = a.home.join(name);
            fs::write(&path, value.to_string()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::set_permissions(a.home.join(QUEUE), fs::Permissions::from_mode(0o600)).unwrap();
        (dir, a)
    }
    #[tokio::test]
    async fn explicit_manifest_pins_boot_and_advertises_only_selected_routes() {
        let (_dir, mut a) = fixture("#!/bin/sh\nexit 0\n");
        let policy = a.home.join("renamed.json");
        fs::write(&policy, r#"{"token_sha256":"fixture"}"#).unwrap();
        fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();
        let manifest = a.home.join("profiles.json");
        fs::write(&manifest, r#"{"profiles":[{"adapter":"muse","route":"/personal-muse/mcp","policy":"renamed.json"}]}"#).unwrap();
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        a.manifest = Some(Manifest::load(&manifest, &a.home).unwrap());
        assert_eq!(preflight(&a).await, Ok(()));
        let command = host_command(&a);
        let args: Vec<_> = command.as_std().get_args().collect();
        let fingerprint = a.manifest.as_ref().unwrap().fingerprint();
        assert!(
            args.windows(2)
                .any(|args| args[0] == "--profiles-file" && args[1] == manifest)
        );
        assert!(
            args.windows(2)
                .any(|args| args[0] == "--profiles-fingerprint" && args[1] == fingerprint)
        );
        assert!(!args.iter().any(|arg| *arg == "--muse-auth-policy"));
        let mut ready = json!({"transport":"hub-local-http","muse_enabled":true,"generic_enabled":false,
            "grok_enabled":false,"dot_enabled":false,"codex_enabled":false,"receiver_enabled":false,"instinct_enabled":false,"cue_enabled":false,
            "codex_events_enabled":false,"profiles_fingerprint":fingerprint});
        assert_eq!(profile(&ready, &a), Ok(()));
        ready["profiles_fingerprint"] = json!("changed");
        assert_eq!(profile(&ready, &a), Err("unexpected_host_profile"));
        let mut endpoints = json!({});
        let paths = public_health_paths(&a, &ready);
        add_public_endpoints(&mut endpoints, &paths, "https://fixture.example");
        assert_eq!(
            endpoints["muse_endpoint"],
            "https://fixture.example/personal-muse/mcp"
        );
        assert!(endpoints["grok_endpoint"].is_null());
        assert_eq!(paths, json!({"muse":"/personal-muse/mcp"}));
        fs::remove_file(&policy).unwrap();
        assert!(
            preflight(&a).await.is_err(),
            "must not fall back to the conventional policy"
        );
    }

    #[tokio::test]
    async fn normal_run_enables_codex_and_receiver_without_unrelated_profiles() {
        let (_dir, a) = codex_fixture();
        assert_eq!(preflight(&a).await, Ok(()));
        let command = host_command(&a);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == "--bundled-profiles"));
        let bindings = mailbox_startup::bindings(&a.home, None);
        assert_eq!(bindings.len(), 2);
        for binding in bindings {
            assert_eq!(binding.registration, Some(a.home.join(REGISTRATION)));
            assert_eq!(binding.policy, a.home.join(binding.descriptor.policy_file));
            assert_eq!(binding.route, binding.descriptor.route);
        }
        let ready = json!({"transport":"hub-local-http","muse_enabled":false,"generic_enabled":false,
            "grok_enabled":false,"dot_enabled":false,
            "codex_enabled":true,"receiver_enabled":true,"instinct_enabled":false,"cue_enabled":false,"codex_events_enabled":false});
        assert_eq!(profile(&ready, &a), Ok(()));
        assert_eq!(public_health_paths(&a, &ready), json!({"codex-cloud":"/codex/mcp"}));
        for key in ["codex_enabled", "receiver_enabled", "codex_events_enabled"] {
            let mut wrong = ready.clone();
            wrong[key] = json!(!ready[key].as_bool().unwrap());
            assert!(profile(&wrong, &a).is_err());
        }
        fs::remove_file(a.home.join(catalog::codex_http().policy_file)).unwrap();
        assert_eq!(preflight(&a).await, Ok(()));
        fs::remove_file(a.home.join(REGISTRATION)).unwrap();
        assert_eq!(
            preflight(&a).await,
            Err("receiver_registration_unavailable")
        );
    }
    #[tokio::test]
    async fn codex_preflight_requires_explicit_events_preparation() {
        let (_dir, a) = codex_fixture();
        let bindings = mailbox_startup::bindings(&a.home, a.manifest.as_ref());
        let codex = bindings
            .iter()
            .find(|binding| binding.descriptor == catalog::codex_http())
            .unwrap()
            .mailbox(&a.home.join(QUEUE))
            .unwrap();
        let mut policy: Value = serde_json::from_slice(&fs::read(&codex.policy).unwrap()).unwrap();
        policy["events"] = json!({"callback_hosts":["callback.example.com"],"resolver":a.binary});
        fs::write(&codex.policy, policy.to_string()).unwrap();
        assert_eq!(preflight(&a).await, Err("codex_events_storage_unprepared"));
        let conn = rusqlite::Connection::open(&codex.db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='dot_event_subscriptions'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        crate::codex_events::Runtime::new(&codex)
            .unwrap()
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(preflight(&a).await, Ok(()));
        assert_eq!(
            profile(
                &json!({"transport":"hub-local-http","muse_enabled":false,"generic_enabled":false,
            "grok_enabled":false,"dot_enabled":false,
            "codex_enabled":true,"receiver_enabled":true,"instinct_enabled":false,"cue_enabled":false,"codex_events_enabled":true}),
                &a
            ),
            Ok(())
        );
    }
    #[tokio::test]
    async fn configured_origin_keeps_the_same_profiles_and_never_starts_ssh() {
        let (_dir, mut a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\nprintf '%s\\n' '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}'\nexec /bin/sleep 30\n",
        );
        let default_command = host_command(&a);
        a.transport = Transport::External("https://pigeon.example".into());
        assert!(
            host_command(&a)
                .as_std()
                .get_args()
                .eq(default_command.as_std().get_args())
        );
        for name in [
            "pigeonmq-cloud-tunnel-ed25519",
            "pigeonmq-cloud-tunnel-known_hosts",
        ] {
            fs::remove_file(a.home.join(name)).unwrap();
        }
        let state_path = a.home.join("pigeonmq-cloud-state.json");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with(
            a,
            |_, _| panic!("configured transport must not start SSH"),
            async {
                let _ = stopped.await;
            },
        ));
        let state = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(bytes) = fs::read(&state_path)
                    && let Ok(state) = serde_json::from_slice::<Value>(&bytes)
                    && state["state"] == "local_ready"
                {
                    break state;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state["tunnel"], "externally_managed");
        assert_eq!(state["local_origin"], "http://127.0.0.1:54843");
        assert_eq!(state["public_origin"], "https://pigeon.example");
        assert_eq!(state["public_origin_source"], "configured");
        assert_eq!(state["remote_verified"], false);
        assert_eq!(state["health_public_paths"], json!({"muse":"/muse/mcp"}));
        assert_eq!(state["muse_endpoint"], "https://pigeon.example/muse/mcp");
        assert!(state["dot_endpoint"].is_null());
        assert!(state.get("receiver_endpoint").is_none());
        assert_eq!(cloud_transport::handoff_origin(&state), None);
        stop.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let stopped: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
        assert!(stopped["public_origin"].is_null());
    }
    #[tokio::test]
    async fn local_only_recovers_a_failed_host_without_ssh_or_changed_ownership() {
        let (dir, mut a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\nroot=${0%/*}\nprintf '%s\\n' \"$$\" >> \"$root/host-starts\"\nprintf '%s\\n' '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}'\nif [ ! -e \"$root/first-exit\" ]; then : > \"$root/first-exit\"; exit 7; fi\nexec /bin/sleep 30\n",
        );
        a.transport = Transport::LocalOnly;
        for name in [
            "pigeonmq-cloud-tunnel-ed25519",
            "pigeonmq-cloud-tunnel-known_hosts",
        ] {
            fs::remove_file(a.home.join(name)).unwrap();
        }
        let home = a.home.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with(
            a,
            |_, _| panic!("local-only must never start SSH"),
            async {
                let _ = stopped.await;
            },
        ));
        let state = timeout(Duration::from_secs(5), async {
            loop {
                let state = fs::read(home.join("pigeonmq-cloud-state.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                if fs::read_to_string(home.join("host-starts"))
                    .is_ok_and(|text| text.lines().count() == 2)
                    && state
                        .as_ref()
                        .is_some_and(|value| value["state"] == "local_ready")
                {
                    break state.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state["tunnel"], "externally_managed");
        assert!(state["public_origin"].is_null());
        assert_eq!(state["local_origin"], "http://127.0.0.1:54843");
        assert!(runner_lock(&home).is_err());
        let pids: Vec<i32> = fs::read_to_string(home.join("host-starts"))
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
        assert_ne!(unsafe { libc::kill(pids[0], 0) }, 0);
        assert_eq!(unsafe { libc::kill(pids[1], 0) }, 0);
        stop.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_ne!(unsafe { libc::kill(pids[1], 0) }, 0);
        assert!(runner_lock(dir.path()).is_ok());
    }
    #[tokio::test]
    async fn host_recovery_revalidates_current_registration() {
        for valid in [true, false] {
            let (dir, mut a) = codex_fixture();
            a.transport = Transport::LocalOnly;
            let registry = if valid {
                json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},
                    "codex-local":{"mailbox":"codex-local","revoked":false},
                    "peer":{"mailbox":"peer","revoked":true}})
                .to_string()
            } else {
                "{".into()
            };
            let script = format!(
                "#!/bin/sh\nroot=${{0%/*}}\nprintf '%s\\n' \"$$\" >> \"$root/host-starts\"\nprintf '%s\\n' '{{\"transport\":\"hub-local-http\",\"muse_enabled\":false,\"generic_enabled\":false,\"grok_enabled\":false,\"dot_enabled\":false,\"codex_enabled\":true,\"receiver_enabled\":true,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}}'\nif [ ! -e \"$root/first-exit\" ]; then : > \"$root/first-exit\"; printf '%s' '{registry}' > \"$root/pigeonmq-cloud-registration.json\"; exit 7; fi\nexec /bin/sleep 30\n"
            );
            fs::write(&a.binary, script).unwrap();
            let home = a.home.clone();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(run_with(
                a,
                |_, _| panic!("local-only must not start SSH"),
                async {
                    let _ = stopped.await;
                },
            ));
            if valid {
                timeout(Duration::from_secs(5), async {
                    loop {
                        if fs::read_to_string(home.join("host-starts"))
                            .is_ok_and(|text| text.lines().count() == 2)
                            && fs::read(home.join("pigeonmq-cloud-state.json"))
                                .ok()
                                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                                .is_some_and(|value| value["state"] == "local_ready")
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                stop.send(()).unwrap();
                assert_eq!(
                    timeout(Duration::from_secs(2), task)
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
            } else {
                assert_eq!(
                    timeout(Duration::from_secs(3), task)
                        .await
                        .unwrap()
                        .unwrap(),
                    1
                );
                assert_eq!(
                    fs::read_to_string(dir.path().join("host-starts"))
                        .unwrap()
                        .lines()
                        .count(),
                    1
                );
                let state: Value = serde_json::from_slice(
                    &fs::read(home.join("pigeonmq-cloud-state.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    state["reason"],
                    "codex_policy_or_existing_queue_unavailable"
                );
            }
        }
    }
    #[tokio::test]
    async fn host_recovery_rejects_a_replacement_queue() {
        let (dir, mut a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\nroot=${0%/*}\nprintf '%s\\n' started >> \"$root/host-starts\"\nprintf '%s\\n' '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}'\nprintf replacement > \"$root/replacement\"\n/bin/chmod 600 \"$root/replacement\"\n/bin/mv \"$root/replacement\" \"$root/pigeonmq-cloud-queue.sqlite3\"\nexit 7\n",
        );
        a.transport = Transport::LocalOnly;
        assert_eq!(
            timeout(
                Duration::from_secs(3),
                run_with(
                    a,
                    |_, _| panic!("local-only must not start SSH"),
                    std::future::pending()
                )
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("host-starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let state: Value = serde_json::from_slice(
            &fs::read(dir.path().join("pigeonmq-cloud-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["reason"], "queue_identity_changed_restart_required");
    }
    #[tokio::test]
    async fn requested_stop_wins_when_host_exits_during_tunnel_cleanup() {
        let (dir, a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\nroot=${0%/*}\nprintf '%s\\n' \"$$\" >> \"$root/host-starts\"\nprintf '%s\\n' '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false}'\nexec /bin/sleep 30\n",
        );
        let home = a.home.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with(
            a,
            |args, _| {
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    "trap 'kill -TERM \"$(cat \"$1/host-starts\")\"; /bin/sleep 0.1; : > \"$1/tunnel-stopped\"; exit 0' TERM\n: > \"$1/tunnel-ready\"\nwhile :; do /bin/sleep 0.02; done",
                    "fixture-tunnel",
                ]);
                command.arg(&args.home);
                command
            },
            async {
                let _ = stopped.await;
            },
        ));
        timeout(Duration::from_secs(5), async {
            while !home.join("tunnel-ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(home.join("tunnel-stopped").exists());
        assert_eq!(
            fs::read_to_string(home.join("host-starts"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let state: Value =
            serde_json::from_slice(&fs::read(home.join("pigeonmq-cloud-state.json")).unwrap())
                .unwrap();
        assert_eq!(state["reason"], "stopped");
        assert!(runner_lock(dir.path()).is_ok());
    }
    #[test]
    fn managed_tunnel_retry_delay_grows_and_caps_without_readiness() {
        let mut managed = ManagedTunnel::new();
        let now = Instant::now();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            managed.clear_connection();
            // Liveness without a validated provider origin is not recovery.
            managed.observe_running(now + Duration::from_secs(60));
            assert_eq!(managed.next_retry_delay(), Duration::from_secs(seconds));
        }
    }

    #[test]
    fn managed_tunnel_retry_delay_resets_only_after_stable_readiness() {
        let mut managed = ManagedTunnel::new();
        let now = Instant::now();
        let origin = "https://fixture.tunnl.gg";
        assert_eq!(managed.next_retry_delay(), Duration::from_secs(1));
        managed.ready(origin.into(), now);
        managed.observe_running(now + TUNNEL_STABLE_FOR - Duration::from_millis(1));
        assert_eq!(managed.next_retry_delay(), Duration::from_secs(2));

        managed.clear_connection();
        let next = now + TUNNEL_STABLE_FOR;
        managed.observe_running(next);
        assert_eq!(managed.next_retry_delay(), Duration::from_secs(4));
        managed.ready(origin.into(), next);
        managed.observe_running(next + TUNNEL_STABLE_FOR);
        assert_eq!(managed.next_retry_delay(), Duration::from_secs(1));
        assert_eq!(managed.next_retry_delay(), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn tunnel_failure_keeps_same_local_hub_until_normal_stop() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let (dir, mut a) = fixture(
            "#!/bin/sh\nif [ \"$1\" != serve-hub ]; then exit 0; fi\necho '{\"transport\":\"hub-local-http\",\"muse_enabled\":true,\"generic_enabled\":false,\"dot_enabled\":false,\"grok_enabled\":false,\"codex_enabled\":false,\"receiver_enabled\":false,\"instinct_enabled\":false,\"cue_enabled\":false,\"codex_events_enabled\":false,\"local_mailbox_enabled\":false,\"local_mailbox_port\":null}'\nexec /bin/sleep 30\n",
        );
        a.watchdog_interval = Duration::from_secs(60);
        let home = a.home.clone();
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_with(
            a,
            move |a, _| {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Command::new(a.home.join("missing-ssh-fixture"))
                } else {
                    Command::new("/usr/bin/false")
                }
            },
            async {
                let _ = stopped.await;
            },
        ));
        let retry_state = async |attempt| {
            timeout(Duration::from_secs(5), async {
                loop {
                    if attempts.load(Ordering::SeqCst) >= attempt
                        && let Ok(bytes) = fs::read(home.join(STATE_FILE))
                        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                        && value["tunnel"] == "retrying"
                    {
                        break value;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap()
        };
        let first = retry_state(1).await;
        let state = retry_state(2).await;
        let pid = state["host_pid"].as_u64().unwrap() as i32;
        assert_eq!(state["local_hub"], "running");
        assert_eq!(state["host_pid"], first["host_pid"]);
        assert_eq!(state["reason"], "transient_tunnel_start");
        assert!(state["public_origin"].is_null());
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        stop.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        let state: Value = serde_json::from_slice(
            &fs::read(dir.path().join("pigeonmq-cloud-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["local_hub"], "stopped");
        assert_eq!(state["tunnel"], "stopped");
    }
    #[tokio::test]
    async fn tunnel_reconnect_replaces_origin_without_restarting_local_hub() {
        let (_dir, a) = fixture(
            r#"#!/bin/sh
if [ "$1" != serve-hub ]; then exit 0; fi
printf '%s\n' '{"transport":"hub-local-http","muse_enabled":true,"generic_enabled":false,"dot_enabled":false,"grok_enabled":false,"codex_enabled":false,"receiver_enabled":false,"instinct_enabled":false,"cue_enabled":false,"codex_events_enabled":false}'
exec /bin/sleep 30
"#,
        );
        let home = a.home.clone();
        let muse_policy = home.join(catalog::http_by_id("muse").unwrap().policy_file);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let mut attempts = 0;
        let task = tokio::spawn(run_with(
            a,
            move |_, _| {
                attempts += 1;
                if attempts == 1 {
                    // The live child already validated and loaded this profile.
                    fs::remove_file(&muse_policy).unwrap();
                }
                let event = json!({"event":"tunnel","stable":true,
                    "url":format!("https://fixture-{attempts}.tunnl.gg")});
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    "printf '%s\n' \"$1\"; exec /bin/sleep \"$2\"",
                    "fixture-tunnel",
                    &event.to_string(),
                    if attempts == 1 { "0.5" } else { "30" },
                ]);
                command
            },
            async {
                let _ = stopped.await;
            },
        ));
        let mut observed = Vec::new();
        for attempt in 1..=2 {
            let origin = format!("https://fixture-{attempt}.tunnl.gg");
            let state = timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(value) = fs::read(home.join(STATE_FILE))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .filter(|value| {
                            value["state"] == "connected" && value["public_origin"] == origin
                        })
                    {
                        break value;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(state["muse_endpoint"], format!("{origin}/muse/mcp"));
            assert_eq!(state["health_public_paths"], json!({"muse":"/muse/mcp"}));
            observed.push(state);
        }
        assert_eq!(observed[0]["host_pid"], observed[1]["host_pid"]);
        assert_ne!(observed[0]["tunnel_pid"], observed[1]["tunnel_pid"]);
        stop.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn host_diagnostics_stay_on_inherited_stderr_with_valid_startup_profile() {
        const CHILD: &str = "PIGEONMQ_CLOUD_TEST_INHERITED_DIAGNOSTICS";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cloud_service::tests::host_diagnostics_stay_on_inherited_stderr_with_valid_startup_profile",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success());
            let records: Vec<Value> = std::str::from_utf8(&output.stderr)
                .unwrap()
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(
                records
                    .iter()
                    .any(|record| record["fields"]["event"] == "fixture_host_diagnostic")
            );
            assert!(
                records
                    .iter()
                    .any(|record| record["fields"]["stage"] == "host"
                        && record["fields"]["outcome"] == "ready")
            );
            assert!(
                records
                    .iter()
                    .any(|record| record["fields"]["stage"] == "run"
                        && record["fields"]["outcome"] == "stopped")
            );
            assert!(
                !String::from_utf8(output.stdout)
                    .unwrap()
                    .contains("fixture_host_diagnostic")
            );
            return;
        }
        pigeonmq::logging::init();
        let (_dir, args) = fixture(
            r#"#!/bin/sh
if [ "$1" != serve-hub ]; then exit 0; fi
printf '%s\n' '{"transport":"hub-local-http","muse_enabled":true,"generic_enabled":false,"dot_enabled":false,"grok_enabled":false,"codex_enabled":false,"receiver_enabled":false,"instinct_enabled":false,"cue_enabled":false,"codex_events_enabled":false,"local_mailbox_enabled":false,"local_mailbox_port":null}'
printf '%s\n' '{"level":"INFO","fields":{"component":"fixture_host","event":"fixture_host_diagnostic"}}' >&2
exec /bin/sleep 30
"#,
        );
        let state_path = args.home.join("pigeonmq-cloud-state.json");
        let stop_after_ready = async move {
            timeout(Duration::from_secs(5), async {
                loop {
                    if fs::read(&state_path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .is_some_and(|state| state["local_hub"] == "running")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        };
        assert_eq!(
            run_with(
                args,
                |_, _| Command::new("/usr/bin/false"),
                stop_after_ready
            )
            .await,
            0
        );
    }
    #[test]
    fn explicit_profiles_and_commands_match() {
        let (_d, mut a) = fixture("fixture");
        for generic in [false, true] {
            a.auth_policy = generic.then(|| a.home.join("generic"));
            let command = host_command(&a);
            let args: Vec<_> = command.as_std().get_args().collect();
            assert_eq!(
                args.iter().filter(|arg| **arg == "--auth-policy").count(),
                usize::from(generic)
            );
            assert_eq!(
                args.iter()
                    .filter(|arg| **arg == "--bundled-profiles")
                    .count(),
                1
            );
            let good = json!({
                "transport":"hub-local-http","muse_enabled":true,"generic_enabled":generic,
                "dot_enabled":false,"grok_enabled":false,"codex_enabled":false,
                "receiver_enabled":false,"instinct_enabled":false,"cue_enabled":false,"codex_events_enabled":false,
            });
            assert_eq!(profile(&good, &a), Ok(()));
            for key in [
                "transport",
                "muse_enabled",
                "generic_enabled",
                "dot_enabled",
                "grok_enabled",
                "codex_enabled",
                "receiver_enabled",
                "codex_events_enabled",
            ] {
                let mut bad = good.clone();
                bad.as_object_mut().unwrap().remove(key);
                assert_eq!(profile(&bad, &a), Err("unexpected_host_profile"), "{key}");
                bad[key] = json!("false");
                assert_eq!(profile(&bad, &a), Err("unexpected_host_profile"), "{key}");
            }
        }
    }
    #[test]
    fn ssh_keeps_pinned_noninteractive_authentication() {
        let (_d, a) = fixture("fixture");
        let c = a
            .transport
            .managed_provider()
            .unwrap()
            .ssh_command(&a.home, a.port);
        assert_eq!(c.as_std().get_program(), "/usr/bin/ssh");
        let args: Vec<_> = c
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        for s in [
            "StrictHostKeyChecking=yes",
            "IdentityAgent=none",
            "IdentitiesOnly=yes",
            "PasswordAuthentication=no",
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "stable@proxy.tunnl.gg",
            "output=json",
        ] {
            assert!(args.contains(&s.to_owned()), "{s}");
        }
    }
    #[test]
    fn stable_origin_is_exact_and_not_arbitrary_url() {
        let provider = Provider::named("tunnl").unwrap();
        assert_eq!(
            provider
                .stable_origin(
                    &json!({"event":"tunnel","stable":true,"url":"https://fixture.tunnl.gg/"})
                )
                .unwrap(),
            Some("https://fixture.tunnl.gg".into())
        );
        for u in [
            "http://fixture.tunnl.gg",
            "https://tunnl.gg",
            "https://tunnl.gg.evil.test",
            "https://user@fixture.tunnl.gg",
            "https://fixture.tunnl.gg/path",
            "https://fixture.tunnl.gg?secret=x",
            "https://fixture.tunnl.gg#x",
            "https://fixture.tunnl.gg:444",
        ] {
            assert!(
                provider
                    .stable_origin(&json!({"event":"tunnel","stable":true,"url":u}))
                    .is_err()
            );
        }
        assert!(
            provider
                .stable_origin(
                    &json!({"event":"tunnel","stable":false,"url":"https://fixture.tunnl.gg"})
                )
                .is_err()
        );
        assert_eq!(
            provider
                .stable_origin(&json!({"event":"request","body":"private"}))
                .unwrap(),
            None
        );
    }
    #[tokio::test]
    async fn private_input_failure_never_executes_checks() {
        let (d, a) = fixture("#!/bin/sh\necho called > \"$(dirname \"$0\")/called\"\n");
        let p = d
            .path()
            .join(catalog::by_adapter(catalog::Adapter::Muse).policy_file);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(preflight(&a).await, Err("unsafe_private_path"));
        assert!(!d.path().join("called").exists());
    }
    #[tokio::test]
    async fn policy_check_failure_is_permanent_and_spawn_failure_transient() {
        let (_d, a) = fixture("#!/bin/sh\nexit 2\n");
        assert_eq!(
            preflight(&a).await,
            Err("policy_or_existing_queue_unavailable")
        );
        let (_d, a) = fixture("#!/bin/sh\nexit 0\n");
        fs::set_permissions(&a.binary, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(preflight(&a).await, Err("transient_preflight"));
    }
    #[tokio::test]
    async fn child_failure_logs_only_status_and_io_categories() {
        const CHILD: &str = "PIGEONMQ_TEST_CHILD_FAILURE_DIAGNOSTICS";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cloud_service::tests::child_failure_logs_only_status_and_io_categories",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            for sentinel in [
                "MUSE_STDOUT_PRIVATE",
                "MUSE_STDERR_PRIVATE",
                "SSH_STDOUT_PRIVATE",
                "SSH_STDERR_PRIVATE",
            ] {
                assert!(!stderr.contains(sentinel) && !stdout.contains(sentinel));
            }
            let records: Vec<Value> = stderr
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()["fields"].clone())
                .collect();
            assert!(
                records
                    .iter()
                    .any(|record| record["stage"] == "muse_preflight"
                        && record["outcome"] == "exited"
                        && record["exit_code"] == 42)
            );
            assert!(
                records
                    .iter()
                    .any(|record| record["stage"] == "muse_preflight"
                        && record["outcome"] == "io_failed"
                        && record["io_kind"] == "PermissionDenied"
                        && record["os_error"] == libc::EACCES)
            );
            assert!(records.iter().any(|record| record["stage"] == "tunnel_wait"
                && record["outcome"] == "exited"
                && record["signal"] == libc::SIGTERM));
            return;
        }

        pigeonmq::logging::init();
        let (_dir, a) = fixture(
            "#!/bin/sh\nprintf '%s\\n' MUSE_STDOUT_PRIVATE\nprintf '%s\\n' MUSE_STDERR_PRIVATE >&2\nexit 42\n",
        );
        assert_eq!(
            preflight(&a).await,
            Err("policy_or_existing_queue_unavailable")
        );
        fs::set_permissions(&a.binary, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(preflight(&a).await, Err("transient_preflight"));
        assert_eq!(
            supervised("printf '%s\\n' SSH_STDOUT_PRIVATE; printf '%s\\n' SSH_STDERR_PRIVATE >&2; kill -TERM \"$$\"").await,
            Err(("transient_tunnel_start", true))
        );
    }
    #[tokio::test]
    async fn failed_preflight_clears_stale_connected_state_and_returns_failure() {
        let (dir, a) = fixture("#!/bin/sh\nexit 2\n");
        record(
            &a.home,
            json!({"state":"connected", "public_origin":"https://fixture.tunnl.gg"}),
        )
        .unwrap();
        assert_eq!(run(a).await, 1);
        let state: Value = serde_json::from_slice(
            &fs::read(dir.path().join("pigeonmq-cloud-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["state"], "disconnected");
        assert_eq!(state["local_hub"], "stopped");
        assert_eq!(state["reason"], "policy_or_existing_queue_unavailable");
        assert!(state["public_origin"].is_null());
    }
    #[tokio::test]
    async fn second_runner_cannot_replace_state_and_lock_releases_on_close() {
        let (dir, a) = fixture("#!/bin/sh\nexit 2\n");
        let lock = runner_lock(&a.home).unwrap();
        record(&a.home, json!({"state":"connected"})).unwrap();
        let state_path = dir.path().join("pigeonmq-cloud-state.json");
        let before = fs::read(&state_path).unwrap();
        assert_eq!(run(a).await, 1);
        assert_eq!(fs::read(state_path).unwrap(), before);
        drop(lock);
        assert!(runner_lock(dir.path()).is_ok());
    }
    #[test]
    fn runner_and_state_reject_unsafe_home() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(runner_lock(dir.path()).is_err());
        assert!(record(dir.path(), json!({"state":"connected"})).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn bad_profile_stops_host_before_any_tunnel() {
        let (d, a) = fixture(
            "#!/bin/sh\ncase \"$1\" in muse-check) exit 0;; esac\nprintf '%s\n' '{}'\nexec /bin/sleep 5\n",
        );
        assert_eq!(run(a).await, 1);
        let value: Value =
            serde_json::from_slice(&fs::read(d.path().join("pigeonmq-cloud-state.json")).unwrap())
                .unwrap();
        assert_eq!(value["reason"], "unexpected_host_profile");
    }
    async fn supervised(script: &str) -> Result<String, (&'static str, bool)> {
        let (_d, a) = fixture("fixture");
        let mut host = Command::new("/bin/sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut tunnel = Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        let result = timeout(
            Duration::from_secs(2),
            supervise(
                &a,
                a.transport.managed_provider().unwrap(),
                &mut host,
                &mut tunnel,
                &mut ManagedTunnel::new(),
                &mut term,
                &mut interrupt,
            ),
        )
        .await
        .unwrap();
        terminate(&mut tunnel).await;
        terminate(&mut host).await;
        result
    }
    #[tokio::test]
    async fn auth_rejection_does_not_trigger_restart() {
        assert_eq!(
            supervised("echo 'Permission denied (publickey)' >&2; exit 1").await,
            Err(("ssh_auth_or_host_key_rejected", false))
        );
    }
    #[tokio::test]
    async fn unexpected_output_is_bounded() {
        assert_eq!(
            supervised("/bin/dd if=/dev/zero bs=17000 count=1 2>/dev/null; exec /bin/sleep 5")
                .await,
            Err(("unexpected_tunnel_output", false))
        );
    }
    #[test]
    fn state_file_is_private_and_replaced() {
        let (d, a) = fixture("fixture");
        record(&a.home, json!({"state":"connected"})).unwrap();
        record(&a.home, json!({"state":"disconnected"})).unwrap();
        let p = d.path().join("pigeonmq-cloud-state.json");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let v: Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
        assert_eq!(v["state"], "disconnected");
    }
    #[test]
    fn disconnected_record_retains_origin_and_failure_is_not_success() {
        let (d, a) = fixture("fixture");
        assert_eq!(
            finish(
                &a,
                "transient_host_start",
                Some("https://fixture.tunnl.gg".into()),
                true
            ),
            1
        );
        let value: Value =
            serde_json::from_slice(&fs::read(d.path().join("pigeonmq-cloud-state.json")).unwrap())
                .unwrap();
        assert_eq!(value["public_origin"], "https://fixture.tunnl.gg");
        assert_eq!(value["reason"], "transient_host_start");
        assert!(value.get("health_public_paths").is_none());
        fs::remove_file(d.path().join("pigeonmq-cloud-state.json")).unwrap();
        fs::create_dir(d.path().join("pigeonmq-cloud-state.json")).unwrap();
        assert_eq!(finish(&a, "stopped", None, false), 0);
    }
    #[test]
    fn host_io_failures_retry_but_invalid_data_does_not() {
        assert_eq!(
            host_read_error(std::io::ErrorKind::ConnectionReset.into()),
            "transient_host_start"
        );
        assert_eq!(
            host_read_error(std::io::ErrorKind::InvalidData.into()),
            "unexpected_host_profile"
        );
    }
    #[tokio::test]
    async fn connected_fixture_records_only_origin_and_detects_host_exit() {
        let (d, mut a) = fixture("fixture");
        a.health_public_paths = json!({"muse":"/muse/mcp"});
        let mut host = Command::new("/bin/sleep")
            .arg("0.3")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut tunnel=Command::new("/bin/sh").args(["-c", "printf '%s\n' '{\"event\":\"tunnel\",\"stable\":true,\"url\":\"https://fixture.tunnl.gg\"}' '{\"event\":\"request\",\"body\":\"PRIVATE_FIXTURE\"}'; exec /bin/sleep 5"])
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        let mut managed = ManagedTunnel::new();
        let result = timeout(
            Duration::from_secs(2),
            supervise(
                &a,
                a.transport.managed_provider().unwrap(),
                &mut host,
                &mut tunnel,
                &mut managed,
                &mut term,
                &mut interrupt,
            ),
        )
        .await
        .unwrap();
        terminate(&mut tunnel).await;
        terminate(&mut host).await;
        assert_eq!(result, Err(("transient_connected", true)));
        assert_eq!(
            managed.public_origin.as_deref(),
            Some("https://fixture.tunnl.gg")
        );
        let state = fs::read_to_string(d.path().join("pigeonmq-cloud-state.json")).unwrap();
        assert!(!state.contains("PRIVATE_FIXTURE"));
        let value: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(value["state"], "connected");
        assert_eq!(value["tunnel_provider"], "tunnl");
        assert_eq!(
            cloud_transport::handoff_origin(&value),
            managed.public_origin
        );
        assert_eq!(value["muse_endpoint"], "https://fixture.tunnl.gg/muse/mcp");
    }
}
