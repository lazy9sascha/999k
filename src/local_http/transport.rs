//! Internal single-request loopback HTTP transport. Application dispatch may have side effects.
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
pub(super) const MAX_HEADERS: usize = 8192;
const IO_BUDGET: Duration = Duration::from_secs(2);
pub(super) type Fault = (u16, &'static str);
pub(super) type Response = (Result<Option<Value>, Fault>, u16);
pub(super) enum WireResponse {
    Json(Response),
    Sdk(http::Response<Vec<u8>>),
}
impl From<Response> for WireResponse {
    fn from(response: Response) -> Self {
        Self::Json(response)
    }
}
pub(super) enum ProfileListener {
    Tcp(TcpListener),
    Unix(UnixListener, String),
}

impl ProfileListener {
    fn prepare(&self) -> io::Result<String> {
        match self {
            Self::Tcp(listener) => {
                listener.set_nonblocking(true)?;
                Ok(listener.local_addr()?.to_string())
            }
            Self::Unix(listener, authority) => {
                listener.set_nonblocking(true)?;
                Ok(authority.clone())
            }
        }
    }

    fn accept(&self) -> io::Result<ProfileStream> {
        match self {
            Self::Tcp(listener) => listener.accept().map(|(stream, _)| stream.into()),
            Self::Unix(listener, _) => listener
                .accept()
                .map(|(stream, _)| ProfileStream::Unix(stream)),
        }
    }
}

pub(super) enum ProfileStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl From<TcpStream> for ProfileStream {
    fn from(stream: TcpStream) -> Self {
        Self::Tcp(stream)
    }
}

macro_rules! socket_call {
    ($self:expr, $operation:ident($($argument:expr),*)) => {
        match $self {
            ProfileStream::Tcp(stream) => stream.$operation($($argument),*),
            ProfileStream::Unix(stream) => stream.$operation($($argument),*),
        }
    };
}

impl ProfileStream {
    fn set_nonblocking(&self, value: bool) -> io::Result<()> {
        socket_call!(self, set_nonblocking(value))
    }
    fn set_read_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        socket_call!(self, set_read_timeout(value))
    }
    fn set_write_timeout(&self, value: Option<Duration>) -> io::Result<()> {
        socket_call!(self, set_write_timeout(value))
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        socket_call!(self, shutdown(how))
    }
}

impl AsRawFd for ProfileStream {
    fn as_raw_fd(&self) -> RawFd {
        socket_call!(self, as_raw_fd())
    }
}
impl Read for ProfileStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        socket_call!(self, read(bytes))
    }
}
impl Write for ProfileStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        socket_call!(self, write(bytes))
    }
    fn flush(&mut self) -> io::Result<()> {
        socket_call!(self, flush())
    }
}

pub(super) type ProfileHandler = dyn Fn(ProfileStream, &str) + Send + Sync;
pub(super) struct Input {
    pub(super) path: String,
    length: usize,
    pub(super) authorization: Vec<Vec<u8>>,
    pub(super) access_assertions: Vec<Vec<u8>>,
    pub(super) sdk_headers: Vec<(String, Vec<u8>)>,
}
pub(super) fn mcp_probe_input(authority: &str, path: &str, bearer: &str) -> Input {
    Input {
        path: path.into(),
        length: 0,
        authorization: vec![format!("Bearer {bearer}").into_bytes()],
        access_assertions: Vec::new(),
        sdk_headers: [
            ("Host", authority),
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.as_bytes().to_vec()))
        .collect(),
    }
}

// FIXME(lib: hyper): hand-rolled HTTP/1.1 server on httparse+TcpStream; hyper is already in the lockfile via reqwest.
pub(super) fn parse_head(
    bytes: &[u8],
    authority: &str,
    hosted_authority: Option<&str>,
    route_body_limit: impl Fn(&str) -> Result<usize, Fault>,
) -> Result<Input, Fault> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if !request
        .parse(bytes)
        .map_err(|_| (400, "invalid_http"))?
        .is_complete()
        || request.version != Some(1)
    {
        return Err((400, "invalid_http"));
    }
    let values = |name: &str| -> Vec<&[u8]> {
        request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value)
            .collect()
    };
    let hosts = values("host");
    if !matches!(hosts.as_slice(), [host] if *host == authority.as_bytes()
        || hosted_authority.is_some_and(|public| *host == public.as_bytes()))
    {
        return Err((403, "loopback_host_required"));
    }
    // No browser origin is trusted, including same-origin/null. No CORS headers.
    if !values("origin").is_empty() || !values("sec-fetch-site").is_empty() {
        return Err((403, "browser_requests_forbidden"));
    }
    // This bearer-authenticated server does not publish OAuth metadata. Return
    // absence to discovery clients; a 405 JSON error is not OAuth metadata.
    if request.method == Some("GET")
        && request
            .path
            .is_some_and(|path| path.starts_with("/.well-known/"))
    {
        return Err((404, "route_not_found"));
    }
    if request.method != Some("POST") {
        return Err((405, "post_required"));
    }
    let path = request.path.ok_or((400, "invalid_http"))?;
    let limit = route_body_limit(path)?;
    if !values("transfer-encoding").is_empty() || !values("expect").is_empty() {
        return Err((400, "unsupported_framing"));
    }
    let lengths = values("content-length");
    if lengths.len() != 1 || lengths[0].is_empty() || !lengths[0].iter().all(u8::is_ascii_digit) {
        return Err((400, "content_length_required"));
    }
    let length = std::str::from_utf8(lengths[0])
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or((413, "body_too_large"))?;
    if length > limit {
        return Err((413, "body_too_large"));
    }
    let types = values("content-type");
    if types.len() != 1
        || !std::str::from_utf8(types[0]).is_ok_and(|v| {
            let mut parts = v.split(';');
            parts
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
                && parts.all(|v| v.trim().eq_ignore_ascii_case("charset=utf-8"))
        })
    {
        return Err((415, "application_json_required"));
    }
    // Capture the same safe inputs for any mounted SDK profile. Routing is a hub decision.
    let sdk_headers = request
        .headers
        .iter()
        .filter(|h| {
            let name = h.name.to_ascii_lowercase();
            matches!(
                name.as_str(),
                "host" | "accept" | "content-type" | "last-event-id"
            ) || name.starts_with("mcp-")
        })
        .map(|h| {
            // SDK profiles receive the same authority as the local dispatch after Host validation.
            let value = if h.name.eq_ignore_ascii_case("host") {
                authority.as_bytes()
            } else {
                h.value
            };
            (h.name.to_owned(), value.to_vec())
        })
        .collect();
    Ok(Input {
        sdk_headers,
        path: path.to_owned(),
        length,
        authorization: values("authorization")
            .into_iter()
            .map(|v| v.to_vec())
            .collect(),
        access_assertions: values("cf-access-jwt-assertion")
            .into_iter()
            .map(|v| v.to_vec())
            .collect(),
    })
}
fn read_part(
    stream: &mut ProfileStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, Fault> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or((408, "request_timeout"))?;
    stream
        .set_read_timeout(Some(left))
        .map_err(|_| (500, "transport_error"))?;
    match stream.read(buffer) {
        Ok(0) => Err((400, "truncated_request")),
        Ok(n) => Ok(n),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Err((408, "request_timeout"))
        }
        Err(_) => Err((400, "invalid_http")),
    }
}
fn read_input(
    stream: &mut ProfileStream,
    authority: &str,
    hosted_authority: Option<&str>,
    route_body_limit: impl Fn(&str) -> Result<usize, Fault>,
) -> Result<(Input, Vec<u8>), Fault> {
    let deadline = Instant::now() + IO_BUDGET;
    let mut bytes = Vec::new();
    let offset = loop {
        if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err((431, "headers_too_large"));
        }
        let mut chunk = [0; 512];
        let capacity = chunk.len().min(MAX_HEADERS - bytes.len());
        let size = read_part(stream, &mut chunk[..capacity], deadline)?;
        bytes.extend_from_slice(&chunk[..size]);
    };
    let input = parse_head(
        &bytes[..offset],
        authority,
        hosted_authority,
        route_body_limit,
    )?;
    if bytes.len() > offset + input.length {
        return Err((400, "extra_request_bytes"));
    }
    while bytes.len() < offset + input.length {
        let mut chunk = [0; 1024];
        let capacity = chunk.len().min(offset + input.length - bytes.len());
        let size = read_part(stream, &mut chunk[..capacity], deadline)?;
        bytes.extend_from_slice(&chunk[..size]);
    }
    Ok((input, bytes[offset..].to_vec()))
}
fn reply(stream: &mut ProfileStream, status: u16, value: Value) -> io::Result<()> {
    let body = serde_json::to_vec(&value)?;
    stream.set_write_timeout(Some(IO_BUDGET))?;
    let allow = if status == 405 { "Allow: POST\r\n" } else { "" };
    write!(
        stream,
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{allow}\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}
fn finish_profile_connection_with_status(
    mut stream: ProfileStream,
    result: Result<Option<Value>, Fault>,
    status: u16,
) {
    match result {
        Ok(Some(value)) => {
            let _ = reply(&mut stream, status, value);
        }
        Ok(None) => {
            let _ = stream.set_write_timeout(Some(IO_BUDGET));
            let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n");
        }
        Err((status, code)) => {
            let _ = reply(&mut stream, status, json!({"error":{"code":code}}));
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

pub(super) fn serve_profile_listeners(
    listeners: Vec<(ProfileListener, Arc<ProfileHandler>)>,
    deadline: Option<Instant>,
    max_workers: usize,
) -> io::Result<()> {
    let listeners = listeners
        .into_iter()
        .map(|(listener, handler)| {
            let authority = listener.prepare()?;
            Ok((listener, authority, handler))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut failure = None;
    'accept: while deadline.is_none_or(|end| Instant::now() < end) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let mut accepted = false;
        for (listener, authority, handler) in &listeners {
            match listener.accept() {
                Ok(mut stream) => {
                    accepted = true;
                    // BSD/macOS may inherit O_NONBLOCK from the listener.
                    // Per-request read/write timeouts require a blocking stream.
                    if stream.set_nonblocking(false).is_err() {
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    if workers.len() >= max_workers {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                        let _ = stream.write_all(
                            b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    let handler = handler.clone();
                    let authority = authority.clone();
                    match thread::Builder::new()
                        .name("pigeon-hub-http".into())
                        .spawn(move || handler(stream, &authority))
                    {
                        Ok(worker) => workers.push(worker),
                        Err(error) => {
                            failure = Some(error);
                            break 'accept;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    failure = Some(error);
                    break 'accept;
                }
            }
        }
        if !accepted {
            thread::sleep(Duration::from_millis(10));
        }
    }
    // Deadlines and cancellation are cooperative between operations.
    // SQL/filesystem calls are not interrupted; joining can outlast the limit.
    for worker in workers {
        let _ = worker.join();
    }
    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(())
    }
}

pub(super) fn handle_connection(
    mut stream: ProfileStream,
    authority: &str,
    hosted_authority: Option<&str>,
    route_body_limit: impl Fn(&str) -> Result<usize, Fault>,
    dispatch: impl FnOnce(&Input, &[u8], &(dyn Fn() -> bool + Sync)) -> WireResponse,
) {
    let (input, body) = match read_input(&mut stream, authority, hosted_authority, route_body_limit)
    {
        Ok(request) => request,
        Err(error) => return finish_profile_connection_with_status(stream, Err(error), 200),
    };
    let peer_closed = || radio999k::dot_mcp_host::peer_closed(&stream);
    match dispatch(&input, &body, &peer_closed) {
        WireResponse::Json((result, status)) => {
            finish_profile_connection_with_status(stream, result, status);
        }
        WireResponse::Sdk(response) => {
            let _ = write_sdk_response(&mut stream, response);
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn write_sdk_response(
    stream: &mut ProfileStream,
    response: http::Response<Vec<u8>>,
) -> io::Result<()> {
    let (parts, body) = response.into_parts();
    stream.set_write_timeout(Some(IO_BUDGET))?;
    write!(
        stream,
        "HTTP/1.1 {} Response\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n",
        parts.status.as_u16(),
        body.len()
    )?;
    for name in ["content-type", "allow", "mcp-protocol-version"] {
        for value in parts.headers.get_all(name) {
            write!(stream, "{name}: ")?;
            stream.write_all(value.as_bytes())?;
            stream.write_all(b"\r\n")?;
        }
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(&body)
}
