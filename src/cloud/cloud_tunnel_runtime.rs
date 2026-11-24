//! Foreground OpenAI Secure MCP Tunnel for the isolated Cloud Dot.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const READY_LIMIT: Duration = Duration::from_secs(75);
const REQUEST_LIMIT: Duration = Duration::from_millis(500);
const HEALTH_FILE: &str = "health.url";
const LOCK_FILE: &str = "foreground.lock";
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn lock_file(dir: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(LOCK_FILE))?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("Unsafe Cloud Dot tunnel lock"));
    }
    Ok(file)
}

fn try_lock(file: &File) -> io::Result<bool> {
    // SAFETY: The descriptor is valid for the duration of this call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

fn health_address(input: &str) -> Option<SocketAddrV4> {
    let address: SocketAddrV4 = input.trim().strip_prefix("http://")?.parse().ok()?;
    (address.ip() == &Ipv4Addr::LOCALHOST && address.port() != 0).then_some(address)
}

fn ready(dir: &Path) -> io::Result<bool> {
    let mut value = String::new();
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(HEALTH_FILE))
    {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("Unsafe tunnel health URL file"));
            }
            file.take(128).read_to_string(&mut value)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let Some(addr) = health_address(&value) else {
        return Ok(false);
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr.into(), REQUEST_LIMIT) else {
        return Ok(false);
    };
    stream.set_read_timeout(Some(REQUEST_LIMIT))?;
    stream.set_write_timeout(Some(REQUEST_LIMIT))?;
    // FIXME(lib: reqwest): hand-rolled health GET; reqwest is already a direct dependency.
    if write!(
        stream,
        "GET /readyz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return Ok(false);
    }
    let mut line = String::new();
    if BufReader::new(stream.take(128))
        .read_line(&mut line)
        .is_err()
    {
        return Ok(false);
    }
    let mut fields = line.split_ascii_whitespace();
    Ok(matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1")) && fields.next() == Some("200"))
}

pub(crate) fn status(dir: &Path) -> io::Result<&'static str> {
    let file = match lock_file(dir, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("not-running"),
        Err(error) => return Err(error),
    };
    if try_lock(&file)? {
        return Ok("not-running");
    }
    if ready(dir)? {
        Ok("ready")
    } else {
        Ok("not-ready")
    }
}

struct Client(Child);
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

extern "C" fn interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

#[expect(
    dead_code,
    reason = "dot-tunnel-setup must verify authenticated workspace admission before running its foreground tunnel"
)]
pub(crate) fn run(
    executable: &Path,
    dir: &Path,
    tunnel_id: &str,
    bearer: &str,
    queue: &Path,
    policy: &Path,
) -> io::Result<()> {
    let file = lock_file(dir, true)?;
    if !try_lock(&file)? {
        return Err(io::Error::other(
            "Cloud Dot tunnel already runs. Use dot-tunnel-status.",
        ));
    }
    let health = dir.join(HEALTH_FILE);
    match fs::remove_file(&health) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let config = dir.join("foreground.yaml");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)
    {
        Ok(mut file) => file.write_all(b"config_version: 1\n")?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = String::new();
            radio999k::private_file::open_private_file(&config)
                .map_err(|_| io::Error::other("Unsafe Cloud Dot foreground config"))?
                .take(64)
                .read_to_string(&mut existing)?;
            if existing != "config_version: 1\n" {
                return Err(io::Error::other("Unexpected Cloud Dot foreground config"));
            }
        }
        Err(error) => return Err(error),
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let origin = format!(
        "http://{}{}",
        listener.local_addr()?,
        radio999k::mailbox_catalog::by_adapter(radio999k::mailbox_catalog::Adapter::Dot).route
    );
    let (hub_tx, hub_rx) = std::sync::mpsc::channel();
    let (queue, policy) = (queue.to_path_buf(), policy.to_path_buf());
    thread::Builder::new()
        .name("cloud-dot-hub".into())
        .spawn(move || {
            let _ = hub_tx.send(crate::local_http::serve_dot_on(listener, queue, policy));
        })?;
    INTERRUPTED.store(false, Ordering::Relaxed);
    // SAFETY: This command runs only once per process. The handler sets an atomic flag.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, interrupt as *const () as libc::sighandler_t);
    }
    let key = dir.join("runtime-key");
    let key = key
        .to_str()
        .ok_or_else(|| io::Error::other("Cloud path is not UTF-8"))?;
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("HOME", dir)
        .current_dir(dir)
        .env("PIGEON_DOT_AUTHORIZATION", format!("Bearer {bearer}"));
    let lock_fd = file.as_raw_fd();
    // SAFETY: pre_exec performs only an async-signal-safe fcntl. The duplicate
    // is intentionally inherited, so the child keeps the lock if the supervisor
    // unexpectedly dies. The original descriptor is still close-on-exec.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(lock_fd, libc::F_DUPFD, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut client = Client(
        command
            .arg("run")
            .arg("--config")
            .arg(config)
            .args([
                "--control-plane.tunnel-id",
                tunnel_id,
                "--control-plane.api-key",
            ])
            .arg(format!("file:{key}"))
            .args(["--mcp.server-url", &origin])
            .args([
                "--mcp.extra-headers",
                "Authorization: env:PIGEON_DOT_AUTHORIZATION",
                "--mcp.discovery-extra-headers",
                "Authorization: env:PIGEON_DOT_AUTHORIZATION",
                "--health.listen-addr",
                "127.0.0.1:0",
                "--health.url-file",
            ])
            .arg(&health)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    io::Error::other(
                        "Install the official OpenAI tunnel-client and run this command again",
                    )
                } else {
                    e
                }
            })?,
    );
    let deadline = Instant::now() + READY_LIMIT;
    let mut announced = false;
    loop {
        if INTERRUPTED.load(Ordering::Relaxed) {
            return Ok(());
        }
        if client.0.try_wait()?.is_some() {
            return Err(io::Error::other(
                "OpenAI tunnel-client stopped. Check the tunnel runtime key and tunnel ID.",
            ));
        }
        if !matches!(hub_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
            return Err(io::Error::other("Cloud Dot MCP listener stopped"));
        }
        if !announced && ready(dir)? {
            println!(
                "Cloud Dot tunnel ready (/readyz). Keep this terminal open. ChatGPT app registration was not attempted."
            );
            announced = true;
        }
        if !announced && Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OpenAI tunnel-client did not become ready. Check tunnel scope, runtime key, and network.",
            ));
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_exact_loopback_health_url() {
        assert!(health_address("http://127.0.0.1:5001\n").is_some());
        for bad in [
            "https://127.0.0.1:5001",
            "http://127.0.0.1:5001/path",
            "http://127.0.0.2:5001",
            "http://localhost:5001",
            "http://127.0.0.1:0",
            "http://169.254.169.254:80",
        ] {
            assert!(health_address(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn status_requires_active_supervisor_and_ready_response() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        fs::write(
            dir.path().join(HEALTH_FILE),
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .unwrap();
        assert_eq!(status(dir.path()).unwrap(), "not-running");
        let file = lock_file(dir.path(), true).unwrap();
        assert!(try_lock(&file).unwrap());
        let server = thread::spawn(move || {
            for status in [503, 200] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut first = String::new();
                let mut reader = BufReader::new(&mut stream);
                reader.read_line(&mut first).unwrap();
                assert_eq!(first, "GET /readyz HTTP/1.1\r\n");
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\n\r\n"
                )
                .unwrap();
            }
        });
        assert_eq!(status(dir.path()).unwrap(), "not-ready");
        assert_eq!(status(dir.path()).unwrap(), "ready");
        server.join().unwrap();
        drop(file);
        assert_eq!(status(dir.path()).unwrap(), "not-running");
    }
}
