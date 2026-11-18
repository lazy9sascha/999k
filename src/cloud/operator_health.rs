//! Opt-in HTTP diagnostics over an owner-private Unix socket, not the tunneled listener.
use serde_json::{Value, json};
use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::timeout,
};

const SOCKET: &str = "pigeonmq-cloud-health.sock";
const IO_BUDGET: Duration = Duration::from_secs(2);
const MAX_HEADERS: u64 = 8192;
type Fault = (u16, &'static str);

pub(super) struct Server {
    task: JoinHandle<()>,
    path: PathBuf,
    identity: (u64, u64),
}

impl Server {
    // The supervisor holds the exclusive run lock and has checked the private home.
    pub(super) async fn start(home: &Path) -> io::Result<Self> {
        let home = home.canonicalize()?;
        let path = home.join(SOCKET);
        let owner = fs::metadata(&home)?.uid();
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == owner => {
                // Clean up a stale socket after a crash, not a live listener.
                match timeout(IO_BUDGET, UnixStream::connect(&path)).await {
                    Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        let current = fs::symlink_metadata(&path)?;
                        if !current.file_type().is_socket()
                            || current.uid() != owner
                            || (current.dev(), current.ino()) != (metadata.dev(), metadata.ino())
                        {
                            return Err(io::ErrorKind::AlreadyExists.into());
                        }
                        fs::remove_file(&path)?;
                    }
                    _ => return Err(io::ErrorKind::AddrInUse.into()),
                }
            }
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let identity = (metadata.dev(), metadata.ino());
        let task = tokio::spawn(async move {
            // Serve one request at a time to bound concurrent queue scans.
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => respond(stream, &home).await,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        tracing::warn!(
                            component = "operator_health",
                            error_code = "socket_accept_failed"
                        );
                        break;
                    }
                }
            }
        });
        let server = Self {
            task,
            path,
            identity,
        };
        fs::set_permissions(&server.path, fs::Permissions::from_mode(0o600))?;
        Ok(server)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn read_request(stream: &mut UnixStream) -> Result<(), Fault> {
    let mut reader = BufReader::new(stream.take(MAX_HEADERS));
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        match reader.read_until(b'\n', &mut bytes).await {
            Ok(0) if bytes.len() as u64 == MAX_HEADERS => {
                return Err((431, "headers_too_large"));
            }
            Ok(0) | Err(_) => return Err((400, "invalid_http")),
            Ok(_) => {}
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if request.parse(&bytes) != Ok(httparse::Status::Complete(bytes.len()))
        || request.version != Some(1)
    {
        return Err((400, "invalid_http"));
    }
    let mut hosts = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("host"));
    if hosts
        .next()
        .is_none_or(|header| header.value != b"localhost")
        || hosts.next().is_some()
    {
        return Err((403, "localhost_host_required"));
    }
    let mut length = false;
    for header in request.headers {
        match header.name.to_ascii_lowercase().as_str() {
            "origin" | "sec-fetch-site" => return Err((403, "browser_requests_forbidden")),
            "transfer-encoding" | "expect" => return Err((400, "unsupported_framing")),
            "content-length" if length || header.value != b"0" => {
                return Err((400, "unsupported_framing"));
            }
            "content-length" => length = true,
            _ => {}
        }
    }
    if request.path != Some("/health") {
        return Err((404, "route_not_found"));
    }
    if request.method != Some("GET") {
        return Err((405, "get_required"));
    }
    Ok(())
}

async fn respond(mut stream: UnixStream, home: &Path) {
    let request = timeout(IO_BUDGET, read_request(&mut stream))
        .await
        .unwrap_or(Err((408, "request_timeout")));
    let (code, value): (u16, Value) = match request {
        Ok(()) => {
            let (value, ready) = super::diagnostics::report(home, false).await;
            (if ready { 200 } else { 503 }, value)
        }
        Err((code, error)) => (code, json!({"error":{"code":error}})),
    };
    let body = value.to_string();
    let allow = if code == 405 { "Allow: GET\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{allow}\r\n",
        body.len()
    );
    let _ = timeout(IO_BUDGET, async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await
    })
    .await;
}
