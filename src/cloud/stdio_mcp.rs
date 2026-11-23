//! Local MCP client adapter. The hub alone authorizes and accesses the queue.
use pigeonmq::{
    arrival_notify::lock_state, mailbox_catalog, mcp_request::strict_json, private_file,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, Metadata},
    io::{self, BufRead, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    time::{Duration, Instant},
};

const SOCKET: &str = "mcp.sock";
const OWNER: &str = "stdio-mcp.json";
pub(crate) const AUTHORITY: &str = "127.0.0.1";
const REQUEST_LIMIT: usize = 128 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const PROTOCOL: &str = "2025-03-26";
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    pid: u32,
    device: u64,
    inode: u64,
    routes: Vec<String>,
}

pub(crate) struct LocalSocket {
    pub(crate) listener: UnixListener,
    _lock: File,
}

fn fail(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments or preconditions.
    unsafe { libc::geteuid() }
}

fn metadata(path: &Path) -> io::Result<Metadata> {
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_socket() && meta.uid() == uid() && meta.nlink() == 1 {
        Ok(meta)
    } else {
        Err(fail("unsafe local MCP socket"))
    }
}

pub(crate) fn bind(home: &Path, routes: &[String]) -> io::Result<LocalSocket> {
    let home = crate::cloud_setup::require_home(home)?;
    let socket = home.join(SOCKET);
    if socket.as_os_str().as_encoded_bytes().len() > 103 {
        return Err(fail(
            "local MCP socket path is too long; use a shorter private hub home",
        ));
    }
    if routes.is_empty() {
        return Err(fail("local MCP socket needs a configured mailbox route"));
    }
    let lock =
        lock_state(&home.join(OWNER)).map_err(|_| fail("local MCP socket is already owned"))?;
    match metadata(&socket) {
        Ok(_) => match UnixStream::connect(&socket) {
            Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(&socket)?,
            _ => return Err(fail("local MCP socket is already serving")),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(&socket)?;
    let meta = metadata(&socket)?;
    let owner = Owner {
        pid: std::process::id(),
        device: meta.dev(),
        inode: meta.ino(),
        routes: routes.to_vec(),
    };
    let bytes = serde_json::to_vec(&owner)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(fail("local MCP socket owner record is too large"));
    }
    private_file::replace_private(&home.join(OWNER), &bytes)?;
    Ok(LocalSocket {
        listener,
        _lock: lock,
    })
}

fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let cap = private_file::MAX_PRIVATE_CONFIG_BYTES;
    private_file::open_private_file(path)
        .map_err(|_| fail("owner-private MCP file required"))?
        .take(cap + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(fail("local MCP file is too large"));
    }
    Ok(bytes)
}

fn peer_matches(stream: &UnixStream, expected: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut peer_uid = 0;
        let mut gid = 0;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: Kernel writes only into the live objects and declared buffer length.
        let ok = unsafe {
            libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut gid) == 0
                && libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut len,
                ) == 0
        };
        ok && peer_uid == uid()
            && pid > 0
            && pid as u32 == expected
            && len as usize == std::mem::size_of_val(&pid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: Kernel writes into the live ucred buffer with the declared length.
        let ok = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            ) == 0
        };
        ok && credentials.uid == uid()
            && credentials.pid > 0
            && credentials.pid as u32 == expected
            && len as usize == std::mem::size_of_val(&credentials)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (stream, expected);
        false
    }
}

fn connect(home: &Path, route: &str) -> io::Result<UnixStream> {
    let owner: Owner = serde_json::from_slice(&read_private(&home.join(OWNER))?)
        .map_err(|_| fail("invalid local MCP socket owner"))?;
    if !owner.routes.iter().any(|candidate| candidate == route) {
        return Err(fail(
            "selected mailbox is not mounted on the local MCP socket",
        ));
    }
    let socket = home.join(SOCKET);
    let pinned = (owner.device, owner.inode);
    let checked = metadata(&socket)?;
    if (checked.dev(), checked.ino()) != pinned {
        return Err(fail("local MCP socket identity changed"));
    }
    let stream = UnixStream::connect(&socket)?;
    let checked = metadata(&socket)?;
    if (checked.dev(), checked.ino()) != pinned || !peer_matches(&stream, owner.pid) {
        return Err(fail("local MCP socket peer does not match the hub"));
    }
    Ok(stream)
}

pub(crate) fn named_route(route: &str) -> bool {
    mailbox_catalog::valid_mcp_route(route)
        && !matches!(route, "/receiver/mcp" | "/admin/mcp" | "/rpc/mcp")
}

fn route_for(mailbox: &str) -> io::Result<String> {
    let route = mailbox_catalog::http_by_id(mailbox)
        .map_or_else(|| format!("/{mailbox}/mcp"), |profile| profile.route.into());
    if named_route(&route) {
        Ok(route)
    } else {
        Err(fail("invalid local MCP mailbox"))
    }
}

fn exchange(home: &Path, route: &str, token: &str, request: &Value) -> io::Result<Value> {
    let mut stream = connect(home, route)?;
    let body = serde_json::to_vec(request)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    // Set this before publishing the request. Darwin can reject changing a
    // socket's timeout after the server has already closed its end.
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    write!(
        stream,
        "POST {route} HTTP/1.1\r\nHost: {AUTHORITY}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMcp-Protocol-Version: {PROTOCOL}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len(),
    )?;
    stream.write_all(&body)?;
    let end = Instant::now() + TIMEOUT;
    let mut response = Vec::new();
    loop {
        if Instant::now() >= end {
            return Err(fail("local MCP hub response timed out"));
        }
        let mut chunk = [0; 4096];
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if n == 0 {
            break;
        }
        if response.len() + n > RESPONSE_LIMIT {
            return Err(fail("local MCP response is too large"));
        }
        response.extend_from_slice(&chunk[..n]);
    }
    let mut headers = [httparse::EMPTY_HEADER; 24];
    let mut http = httparse::Response::new(&mut headers);
    let offset = http
        .parse(&response)
        .ok()
        .and_then(|result| match result {
            httparse::Status::Complete(offset) => Some(offset),
            httparse::Status::Partial => None,
        })
        .ok_or_else(|| fail("invalid local MCP response"))?;
    if http.code != Some(200) {
        return Err(io::Error::other(format!(
            "local MCP hub returned HTTP {}",
            http.code.unwrap_or(0)
        )));
    }
    let reply = strict_json(&response[offset..]).map_err(|_| fail("invalid local MCP response"))?;
    if reply.get("id") != request.get("id")
        || reply["jsonrpc"] != "2.0"
        || (reply.get("result").is_some() == reply.get("error").is_some())
    {
        return Err(fail("invalid local MCP response"));
    }
    Ok(reply)
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

#[derive(PartialEq, Eq)]
enum Session {
    AwaitInitialize,
    AwaitInitialized,
    Ready,
}

fn serve(
    home: &Path,
    route: &str,
    token: &str,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut line = Vec::new();
    let mut session = Session::AwaitInitialize;
    loop {
        line.clear();
        if input
            .by_ref()
            .take((REQUEST_LIMIT + 1) as u64)
            .read_until(b'\n', &mut line)?
            == 0
        {
            return Ok(());
        }
        if line.len() > REQUEST_LIMIT {
            return Err(fail("local MCP request is too large"));
        }
        if line.last() != Some(&b'\n') {
            return Err(fail("incomplete local MCP request"));
        }
        let reply = match strict_json(&line) {
            Ok(request) if request.is_object() && request["jsonrpc"] == "2.0" => {
                let method = request["method"].as_str();
                let id = request.get("id");
                if id.is_none() && method.is_some_and(|method| method.starts_with("notifications/"))
                {
                    if method == Some("notifications/initialized")
                        && session == Session::AwaitInitialized
                    {
                        session = Session::Ready;
                    }
                    continue;
                }
                if !id.is_some_and(|id| id.is_i64() || id.is_string()) || method.is_none() {
                    error(Value::Null, -32600, "Invalid Request")
                } else {
                    let id = id.unwrap().clone();
                    match method.unwrap() {
                        "initialize" if session == Session::AwaitInitialize => {
                            let mut init = request;
                            if let Some(params) =
                                init.get_mut("params").and_then(Value::as_object_mut)
                                && params.get("protocolVersion").is_some_and(Value::is_string)
                            {
                                // Offer the handshake version this stdio adapter implements.
                                params.insert("protocolVersion".into(), json!(PROTOCOL));
                            }
                            match exchange(home, route, token, &init) {
                                Ok(mut result) if result.get("result").is_some() => {
                                    let value = &mut result["result"];
                                    if value["protocolVersion"] != PROTOCOL {
                                        error(id, -32000, "Unsupported local MCP protocol")
                                    } else {
                                        session = Session::AwaitInitialized;
                                        if let Some(capabilities) = value
                                            .get_mut("capabilities")
                                            .and_then(Value::as_object_mut)
                                        {
                                            capabilities.retain(|key, _| key == "tools");
                                        }
                                        result
                                    }
                                }
                                Ok(result) => result,
                                Err(err) => {
                                    eprintln!("Local MCP request failed: {err}");
                                    error(id, -32000, "Local MCP hub unavailable or unauthorized")
                                }
                            }
                        }
                        "ping" | "tools/list" | "tools/call" if session == Session::Ready => {
                            exchange(home, route, token, &request).unwrap_or_else(|err| {
                                eprintln!("Local MCP request failed: {err}");

                                error(id, -32000, "Local MCP hub unavailable or unauthorized")
                            })
                        }
                        "ping" | "tools/list" | "tools/call" | "initialize" => {
                            error(id, -32000, "Invalid initialization state")
                        }
                        _ => error(id, -32601, "Method not found"),
                    }
                }
            }
            Ok(_) => error(Value::Null, -32600, "Invalid Request"),
            Err(_) => error(Value::Null, -32700, "Parse error"),
        };
        // Response serialization stays on the protocol stream; diagnostics do not.
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

pub(crate) fn run(home: &Path, mailbox: &str, bearer_file: &Path) -> io::Result<()> {
    let home = crate::cloud_setup::require_home(home)?;
    let route = route_for(mailbox)?;
    let bytes = read_private(bearer_file)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| fail("invalid MCP bearer file"))?
        .trim();
    let token = text.strip_prefix("Bearer ").unwrap_or(text);
    if token.is_empty() || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(fail("invalid MCP bearer file"));
    }
    serve(
        &home,
        &route,
        token,
        io::stdin().lock(),
        io::stdout().lock(),
    )
}
