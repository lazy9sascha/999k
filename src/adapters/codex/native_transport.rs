//! Bounded JSON-RPC over the existing Codex daemon's Unix WebSocket endpoint.
//! The caller supplies an endpoint from its approved protected binding. A
//! symlink (Codex's stable control socket) is resolved again on every connect.
//! All errors are redacted; remote message/data and local paths are not exposed.
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    time::Instant,
};
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
// FIXME(yagni): 128-message/4 MiB backlog caps with no measured basis
pub const MAX_PENDING_MESSAGES: usize = 128;
pub const MAX_PENDING_BYTES: usize = 4 * MAX_MESSAGE_BYTES;
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The socket is missing or dead (ENOENT, ECONNREFUSED): the daemon may be restarted.
    #[error("native_endpoint_unavailable")]
    EndpointUnavailable,
    /// The endpoint exists but is not this user's socket, or the peer is
    /// another user. Permanent: never a reason to restart the daemon.
    #[error("native_endpoint_untrusted")]
    EndpointUntrusted,
    #[error("native_transport_timeout")]
    Timeout,
    #[error("native_transport_disconnected")]
    Disconnected,
    #[error("native_protocol_error")]
    Protocol,
    #[error("native_message_limit")]
    MessageLimit,
    #[error("native_notification_backlog")]
    BacklogLimit,
    #[error("native_not_initialized")]
    NotInitialized,
    #[error("native_already_initialized")]
    AlreadyInitialized,
    #[error("native_remote_error")]
    Remote { code: Option<i64> },
}
pub type Result<T> = std::result::Result<T, TransportError>;
type Socket = WebSocketStream<UnixStream>;
/// One request at a time. Early inbound method messages retain wire ordering.
/// Server requests are retained with their `id`, never implicitly approved.
/// If an operation times out or its future is dropped, its owned socket is
/// dropped and cannot be reused: the remote outcome remains uncertain.
pub struct NativeTransport {
    socket: Option<Socket>,
    initialized: bool,
    next_id: u64,
    pending: VecDeque<(Value, usize)>,
    pending_bytes: usize,
}
impl NativeTransport {
    pub async fn connect(endpoint: &Path, deadline: Instant) -> Result<Self> {
        use std::io::ErrorKind::{ConnectionRefused, NotFound};
        let io = |e: std::io::Error| match e.kind() {
            NotFound | ConnectionRefused => TransportError::EndpointUnavailable,
            _ => TransportError::EndpointUntrusted,
        };
        if !endpoint.is_absolute() {
            return Err(TransportError::EndpointUntrusted);
        }
        let endpoint = std::fs::canonicalize(endpoint).map_err(io)?;
        let metadata = std::fs::metadata(&endpoint).map_err(io)?;
        // The resolved target must be the current user's socket.
        // FIXME(yagni): socket owner check after canonicalize - the endpoint comes from the owner's private binding
        // SAFETY: getuid has no preconditions.
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::getuid() } {
            return Err(TransportError::EndpointUntrusted);
        }
        let stream = bounded(deadline, UnixStream::connect(&endpoint))
            .await?
            .map_err(|e| match e.kind() {
                NotFound | ConnectionRefused => TransportError::EndpointUnavailable,
                _ => TransportError::Disconnected,
            })?;
        check_peer(&stream)?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let (socket, _) = bounded(
            deadline,
            client_async_with_config("ws://localhost/rpc", stream, Some(config)),
        )
        .await?
        .map_err(ws_error)?;
        Ok(Self {
            socket: Some(socket),
            initialized: false,
            next_id: 1,
            pending: VecDeque::new(),
            pending_bytes: 0,
        })
    }
    /// Official app-server initialization, followed by its initialized notification.
    /// Client metadata/capabilities are explicit caller input, not inferred grants.
    pub async fn initialize(&mut self, params: Value, deadline: Instant) -> Result<Value> {
        if self.initialized {
            return Err(TransportError::AlreadyInitialized);
        }
        let result = self.exchange("initialize", params, deadline).await?;
        let mut socket = self.socket.take().ok_or(TransportError::Disconnected)?;
        send_json(
            &mut socket,
            json!({"method":"initialized","params":{}}),
            deadline,
        )
        .await?;
        self.socket = Some(socket);
        self.initialized = true;
        Ok(result)
    }
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        if !self.initialized {
            return Err(TransportError::NotInitialized);
        }
        if matches!(method, "initialize" | "initialized") {
            return Err(TransportError::AlreadyInitialized);
        }
        self.exchange(method, params, deadline).await
    }
    async fn exchange(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        if method.is_empty()
            || method.len() > 128
            || method.chars().any(char::is_control)
            || !params.is_object()
        {
            return Err(TransportError::Protocol);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(TransportError::Protocol)?;
        // Taking before the first await ensures cancellation cannot accidentally
        // reuse a partially written request or correlate a late response.
        let mut socket = self.socket.take().ok_or(TransportError::Disconnected)?;
        send_json(
            &mut socket,
            json!({"id":id,"method":method,"params":params}),
            deadline,
        )
        .await?;
        loop {
            let (message, size) = read_json(&mut socket, deadline).await?;
            if message.get("method").is_some() {
                self.enqueue(message, size)?;
                continue;
            }
            if message.get("id") != Some(&json!(id)) {
                return Err(TransportError::Protocol);
            }
            let outcome = response(message)?;
            self.socket = Some(socket);
            return outcome;
        }
    }
    /// Returns the next inbound method envelope. Notifications have no id;
    /// server requests retain their id for explicit handling by the caller.
    pub async fn next_notification(&mut self, deadline: Instant) -> Result<Value> {
        if !self.initialized {
            return Err(TransportError::NotInitialized);
        }
        if let Some((message, size)) = self.pending.pop_front() {
            self.pending_bytes -= size;
            return Ok(message);
        }
        let mut socket = self.socket.take().ok_or(TransportError::Disconnected)?;
        // An idle timeout keeps the connection: reading a frame is cancel-safe.
        let (message, _) = match read_json(&mut socket, deadline).await {
            Err(TransportError::Timeout) => {
                self.socket = Some(socket);
                return Err(TransportError::Timeout);
            }
            other => other?,
        };
        if message.get("method").is_none() {
            return Err(TransportError::Protocol);
        }
        self.socket = Some(socket);
        Ok(message)
    }
    /// Answers one server request (a method message with an `id`).
    pub async fn respond(&mut self, id: Value, result: Value, deadline: Instant) -> Result<()> {
        let mut socket = self.socket.take().ok_or(TransportError::Disconnected)?;
        send_json(&mut socket, json!({"id":id,"result":result}), deadline).await?;
        self.socket = Some(socket);
        Ok(())
    }
    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }
    fn enqueue(&mut self, message: Value, size: usize) -> Result<()> {
        if self.pending.len() >= MAX_PENDING_MESSAGES
            || self.pending_bytes.saturating_add(size) > MAX_PENDING_BYTES
        {
            return Err(TransportError::BacklogLimit);
        }
        self.pending_bytes += size;
        self.pending.push_back((message, size));
        Ok(())
    }
}
async fn bounded<T>(deadline: Instant, future: impl std::future::Future<Output = T>) -> Result<T> {
    if Instant::now() >= deadline {
        return Err(TransportError::Timeout);
    }
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .map_err(|_| TransportError::Timeout)
}
async fn send_json(socket: &mut Socket, value: Value, deadline: Instant) -> Result<()> {
    let text = serde_json::to_string(&value).map_err(|_| TransportError::Protocol)?;
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(TransportError::MessageLimit);
    }
    bounded(deadline, socket.send(Message::Text(text.into())))
        .await?
        .map_err(ws_error)
}
async fn read_json(socket: &mut Socket, deadline: Instant) -> Result<(Value, usize)> {
    loop {
        match bounded(deadline, socket.next())
            .await?
            .ok_or(TransportError::Disconnected)?
            .map_err(ws_error)?
        {
            Message::Text(text) => {
                if text.len() > MAX_MESSAGE_BYTES {
                    return Err(TransportError::MessageLimit);
                }
                let value: Value = crate::dot_mcp_host::strict_json(text.as_bytes())
                    .map_err(|_| TransportError::Protocol)?;
                validate_envelope(&value)?;
                return Ok((value, text.len()));
            }
            Message::Ping(_) => {
                bounded(deadline, socket.flush()).await?.map_err(ws_error)?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(TransportError::Disconnected),
            _ => return Err(TransportError::Protocol),
        }
    }
}
fn validate_envelope(value: &Value) -> Result<()> {
    let object = value.as_object().ok_or(TransportError::Protocol)?;
    if object.get("jsonrpc").is_some_and(|v| v != "2.0") {
        return Err(TransportError::Protocol);
    }
    if let Some(method) = object.get("method") {
        if !method
            .as_str()
            .is_some_and(|m| !m.is_empty() && m.len() <= 128 && !m.chars().any(char::is_control))
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return Err(TransportError::Protocol);
        }
        if object
            .get("params")
            .is_some_and(|v| !v.is_object() && !v.is_null())
        {
            return Err(TransportError::Protocol);
        }
        if object
            .get("id")
            .is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
        {
            return Err(TransportError::Protocol);
        }
    } else if !object.contains_key("id")
        || object.contains_key("result") == object.contains_key("error")
    {
        return Err(TransportError::Protocol);
    }
    Ok(())
}
fn response(value: Value) -> Result<Result<Value>> {
    if let Some(error) = value.get("error") {
        let object = error.as_object().ok_or(TransportError::Protocol)?;
        let code = object
            .get("code")
            .and_then(Value::as_i64)
            .ok_or(TransportError::Protocol)?;
        return Ok(Err(TransportError::Remote { code: Some(code) }));
    }
    Ok(Ok(value
        .get("result")
        .cloned()
        .ok_or(TransportError::Protocol)?))
}
fn ws_error(error: tokio_tungstenite::tungstenite::Error) -> TransportError {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Capacity(_) => TransportError::MessageLimit,
        Error::ConnectionClosed | Error::AlreadyClosed | Error::Io(_) => {
            TransportError::Disconnected
        }
        _ => TransportError::Protocol,
    }
}
fn check_peer(stream: &UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: stream descriptor is valid for this call; getpeereid writes into provided out-params.
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result != 0 || uid != unsafe { libc::getuid() } { // SAFETY: getuid has no preconditions.
            return Err(TransportError::EndpointUntrusted);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() }; // SAFETY: zeroed is valid for POD ucred.
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: stream descriptor valid; getsockopt writes into provided buffer.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 || credentials.uid != unsafe { libc::getuid() } { // SAFETY: getuid has no preconditions.
            return Err(TransportError::EndpointUntrusted);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UnixListener;
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (dir, path, listener)
    }
    #[expect(
        clippy::result_large_err,
        reason = "The tungstenite handshake callback requires this response error type"
    )]
    async fn accept(listener: &UnixListener) -> Socket {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        tokio_tungstenite::accept_hdr_async(
            listener.accept().await.unwrap().0,
            |request: &Request, response: Response| {
                assert_eq!(request.uri().path(), "/rpc");
                assert_eq!(request.headers().get("host").unwrap(), "localhost");
                Ok(response)
            },
        )
        .await
        .unwrap()
    }
    async fn recv(socket: &mut Socket) -> Value {
        let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
            panic!("text required")
        };
        serde_json::from_str(&text).unwrap()
    }
    async fn send(socket: &mut Socket, value: Value) {
        socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    async fn initialize(socket: &mut Socket) {
        let request = recv(socket).await;
        assert_eq!(request["method"], "initialize");
        send(
            socket,
            json!({"id":request["id"],"result":{"server":"synthetic"}}),
        )
        .await;
        assert_eq!(recv(socket).await["method"], "initialized");
    }
    #[tokio::test]
    async fn initialize_and_early_notifications_preserve_order() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            let init = recv(&mut socket).await;
            send(&mut socket, json!({"method":"first","params":{"n":1}})).await;
            send(&mut socket, json!({"id":init["id"],"result":{}})).await;
            assert_eq!(recv(&mut socket).await["method"], "initialized");
            let req = recv(&mut socket).await;
            send(&mut socket, json!({"method":"second","params":{"n":2}})).await;
            send(
                &mut socket,
                json!({"id":99,"method":"approval/request","params":{}}),
            )
            .await;
            send(&mut socket, json!({"id":req["id"],"result":{"ok":true}})).await;
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport
            .initialize(
                json!({"clientInfo":{"name":"synthetic","version":"1"}}),
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(
            transport
                .request("thread/read", json!({"threadId":"synthetic"}), deadline())
                .await
                .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(
            transport.next_notification(deadline()).await.unwrap()["method"],
            "first"
        );
        assert_eq!(
            transport.next_notification(deadline()).await.unwrap()["method"],
            "second"
        );
        assert_eq!(
            transport.next_notification(deadline()).await.unwrap()["id"],
            99
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn request_timeout_disconnects_without_retry() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            initialize(&mut socket).await;
            let req = recv(&mut socket).await;
            assert_eq!(req["method"], "turn/start");
            assert!(
                tokio::time::timeout(Duration::from_secs(1), socket.next())
                    .await
                    .unwrap()
                    .is_none_or(|v| v.is_err() || matches!(v, Ok(Message::Close(_))))
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport.initialize(json!({}), deadline()).await.unwrap();
        assert_eq!(
            transport
                .request(
                    "turn/start",
                    json!({}),
                    Instant::now() + Duration::from_millis(30)
                )
                .await
                .unwrap_err(),
            TransportError::Timeout
        );
        assert!(!transport.is_connected());
        assert_eq!(
            transport
                .request("turn/start", json!({}), deadline())
                .await
                .unwrap_err(),
            TransportError::Disconnected
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn clean_disconnect_is_redacted() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            initialize(&mut socket).await;
            socket.close(None).await.unwrap();
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport.initialize(json!({}), deadline()).await.unwrap();
        assert_eq!(
            transport.next_notification(deadline()).await.unwrap_err(),
            TransportError::Disconnected
        );
        assert!(!transport.is_connected());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn remote_errors_do_not_expose_text_or_poison_connection() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            initialize(&mut socket).await;
            let req = recv(&mut socket).await;
            send(&mut socket,json!({"id":req["id"],"error":{"code":-32000,"message":"private token and path","data":{"secret":"not exposed"}}})).await;
            let req = recv(&mut socket).await;
            send(&mut socket, json!({"id":req["id"],"result":{}})).await;
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport.initialize(json!({}), deadline()).await.unwrap();
        let error = transport
            .request("thread/read", json!({}), deadline())
            .await
            .unwrap_err();
        assert_eq!(error, TransportError::Remote { code: Some(-32000) });
        assert_eq!(error.to_string(), "native_remote_error");
        assert!(transport.is_connected());
        transport
            .request("thread/read", json!({}), deadline())
            .await
            .unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn handshake_timeout_and_missing_endpoint() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let _stream = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
        });
        assert!(matches!(
            NativeTransport::connect(&path, Instant::now() + Duration::from_millis(20)).await,
            Err(TransportError::Timeout)
        ));
        server.await.unwrap();
        assert!(matches!(
            NativeTransport::connect(Path::new("/nonexistent/synthetic-codex.sock"), deadline())
                .await,
            Err(TransportError::EndpointUnavailable)
        ));
    }
    #[tokio::test]
    async fn only_missing_or_dead_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("dead.sock");
        drop(UnixListener::bind(&dead).unwrap());
        assert!(matches!(
            NativeTransport::connect(&dead, deadline()).await,
            Err(TransportError::EndpointUnavailable)
        ));
        let file = dir.path().join("not-a-socket");
        std::fs::write(&file, b"").unwrap();
        assert!(matches!(
            NativeTransport::connect(&file, deadline()).await,
            Err(TransportError::EndpointUntrusted)
        ));
    }
    #[tokio::test]
    async fn symlink_is_resolved_on_every_connect() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("control.sock");
        let real = dir.path().join("daemon.sock");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            NativeTransport::connect(&link, deadline()).await,
            Err(TransportError::EndpointUnavailable)
        ));
        let listener = UnixListener::bind(&real).unwrap();
        let server = tokio::spawn(async move { accept(&listener).await });
        NativeTransport::connect(&link, deadline()).await.unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn idle_notification_timeout_keeps_connection() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            initialize(&mut socket).await;
            tokio::time::sleep(Duration::from_millis(80)).await;
            send(&mut socket, json!({"method":"late","params":{}})).await;
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport.initialize(json!({}), deadline()).await.unwrap();
        let short = Instant::now() + Duration::from_millis(20);
        assert_eq!(
            transport.next_notification(short).await.unwrap_err(),
            TransportError::Timeout
        );
        assert!(transport.is_connected());
        assert_eq!(
            transport.next_notification(deadline()).await.unwrap()["method"],
            "late"
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn mismatched_response_id_poisoned() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            let _ = recv(&mut socket).await;
            send(&mut socket, json!({"id":999,"result":{}})).await;
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        assert_eq!(
            transport
                .initialize(json!({}), deadline())
                .await
                .unwrap_err(),
            TransportError::Protocol
        );
        assert!(!transport.is_connected());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn notification_backlog_is_bounded() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            let _ = recv(&mut socket).await;
            for n in 0..=MAX_PENDING_MESSAGES {
                send(&mut socket, json!({"method":"early","params":{"n":n}})).await;
            }
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        assert_eq!(
            transport
                .initialize(json!({}), deadline())
                .await
                .unwrap_err(),
            TransportError::BacklogLimit
        );
        assert!(!transport.is_connected());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn duplicate_json_and_oversized_input_rejected() {
        for oversized in [false, true] {
            let (_dir, path, listener) = fixture();
            let server = tokio::spawn(async move {
                let mut socket = accept(&listener).await;
                let _ = recv(&mut socket).await;
                let text = if oversized {
                    "x".repeat(MAX_MESSAGE_BYTES + 1)
                } else {
                    r#"{"id":1,"id":1,"result":{}}"#.to_owned()
                };
                let _ = socket.send(Message::Text(text.into())).await;
            });
            let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
            let error = transport
                .initialize(json!({}), deadline())
                .await
                .unwrap_err();
            assert_eq!(
                error,
                if oversized {
                    TransportError::MessageLimit
                } else {
                    TransportError::Protocol
                }
            );
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn cancelled_request_future_cannot_reuse_socket() {
        let (_dir, path, listener) = fixture();
        let server = tokio::spawn(async move {
            let mut socket = accept(&listener).await;
            initialize(&mut socket).await;
            assert_eq!(recv(&mut socket).await["method"], "turn/start");
            let _ = tokio::time::timeout(Duration::from_secs(1), socket.next())
                .await
                .unwrap();
        });
        let mut transport = NativeTransport::connect(&path, deadline()).await.unwrap();
        transport.initialize(json!({}), deadline()).await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                transport.request("turn/start", json!({}), deadline())
            )
            .await
            .is_err()
        );
        assert!(!transport.is_connected());
        assert_eq!(
            transport
                .request("turn/start", json!({}), deadline())
                .await
                .unwrap_err(),
            TransportError::Disconnected
        );
        server.await.unwrap();
    }
}
