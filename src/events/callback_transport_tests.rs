//! Source: PigeonMQ e399161, src/dot_callback_transport_tests.rs (MIT).
//! Loopback-only TLS and owned-process cancellation evidence. These private test
//! hooks never bypass the public DNS/URL gate in the production entrypoint.
use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, OnceLock},
    thread,
    time::Instant,
};

struct Fixture {
    cert: Vec<u8>,
    key: Vec<u8>,
}
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let temp = tempfile::tempdir().unwrap();
        for args in [
            vec![
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "key.pem",
                "-out",
                "cert.pem",
                "-days",
                "1",
                "-subj",
                "/CN=callback.test",
                "-addext",
                "subjectAltName=DNS:callback.test",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
            ],
            vec![
                "x509", "-in", "cert.pem", "-outform", "DER", "-out", "cert.der",
            ],
            vec![
                "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "DER", "-out",
                "key.der",
            ],
        ] {
            let result = std::process::Command::new("openssl")
                .args(args)
                .current_dir(temp.path())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "ephemeral certificate generation failed"
            );
        }
        Fixture {
            cert: std::fs::read(temp.path().join("cert.der")).unwrap(),
            key: std::fs::read(temp.path().join("key.der")).unwrap(),
        }
    })
}

struct ServerObservation {
    sni: Option<String>,
    request: Vec<u8>,
    peer_closed: bool,
}
fn tls_server(response: Option<Vec<u8>>) -> (SocketAddr, thread::JoinHandle<ServerObservation>) {
    let fixture = fixture();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                fixture.cert.clone(),
            )],
            rustls::pki_types::PrivatePkcs8KeyDer::from(fixture.key.clone()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(13)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut tls = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
            socket,
        );
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        loop {
            match tls.read(&mut buf) {
                Ok(0) | Err(_) => {
                    return ServerObservation {
                        sni: tls.conn.server_name().map(str::to_owned),
                        request,
                        peer_closed: true,
                    };
                }
                Ok(count) => request.extend_from_slice(&buf[..count]),
            }
            if request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .is_some_and(|end| request.len() >= end + 6)
            {
                break;
            }
            assert!(request.len() < 8192);
        }
        let sni = tls.conn.server_name().map(str::to_owned);
        if let Some(response) = response {
            tls.write_all(&response).unwrap();
            tls.flush().unwrap();
            ServerObservation {
                sni,
                request,
                peer_closed: false,
            }
        } else {
            let peer_closed = match tls.read(&mut buf) {
                Ok(0) => true,
                Err(error) => !matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ),
                _ => false,
            };
            ServerObservation {
                sni,
                request,
                peer_closed,
            }
        }
    });
    (address, worker)
}

fn client(host: &str, address: SocketAddr, trust: bool) -> Client {
    let mut builder = pinned_client(host, address);
    if trust {
        builder =
            builder.add_root_certificate(reqwest::Certificate::from_der(&fixture().cert).unwrap());
    }
    builder.build().unwrap()
}
fn headers() -> HeaderMap {
    let key = format!("whsec_{}", STANDARD.encode([0x0b; 24]));
    signed_headers(&SignedCallback {
        url: "https://callback.test",
        subscription_id: "sub",
        event_id: "evt",
        body: b"{}",
        current_secret: &key,
        previous_secret: None,
        signed_at: 1234.0,
    })
    .unwrap()
}
fn local_url(host: &str, address: SocketAddr) -> Url {
    Url::parse(&format!(
        "https://{host}:{}/callback?q=test",
        address.port()
    ))
    .unwrap()
}

#[tokio::test]
async fn pinned_tls_preserves_sni_host_signature_and_exact_body() {
    let (address, worker) = tls_server(Some(
        b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
    ));
    let response = callback_deadline(send_request(
        client("callback.test", address, true),
        local_url("callback.test", address),
        headers(),
        b"{}",
    ))
    .await
    .unwrap();
    assert_eq!(response.status, 202);
    assert_eq!(response.body, b"ok");
    let observed = worker.join().unwrap();
    assert_eq!(observed.sni.as_deref(), Some("callback.test"));
    let request = String::from_utf8(observed.request).unwrap();
    assert!(request.starts_with("POST /callback?q=test HTTP/1.1\r\n"));
    assert!(request.contains(&format!("host: callback.test:{}\r\n", address.port())));
    assert!(
        request.contains("webhook-signature: v1,sZt81mWasuZLcvLAqi/IR6YcEru+DjwcndFZZADOY3A=\r\n")
    );
    assert!(request.ends_with("\r\n\r\n{}"));
}

#[tokio::test]
async fn tls_rejects_untrusted_certificate_and_wrong_hostname_before_http() {
    for (host, trust) in [("callback.test", false), ("wrong.test", true)] {
        let (address, worker) = tls_server(None);
        let result = send_request(
            client(host, address, trust),
            local_url(host, address),
            headers(),
            b"{}",
        )
        .await;
        assert!(matches!(result, Err(CallbackError("callback_transport"))));
        let observed = worker.join().unwrap();
        assert!(observed.request.is_empty());
    }
}

#[tokio::test]
async fn redirect_is_returned_without_following_and_response_limit_is_enforced() {
    let forbidden = TcpListener::bind("127.0.0.1:0").unwrap();
    forbidden.set_nonblocking(true).unwrap();
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1:{}/forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        forbidden.local_addr().unwrap().port()
    );
    let (address, worker) = tls_server(Some(redirect.into_bytes()));
    let response = send_request(
        client("callback.test", address, true),
        local_url("callback.test", address),
        headers(),
        b"{}",
    )
    .await
    .unwrap();
    assert_eq!(response.status, 302);
    assert!(
        matches!(forbidden.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
    );
    worker.join().unwrap();
    for chunked in [false, true] {
        let response = if chunked {
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1001\r\n{}\r\n0\r\n\r\n",
                "x".repeat(4097)
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 4097\r\nConnection: close\r\n\r\n{}",
                "x".repeat(4097)
            )
        };
        let (address, worker) = tls_server(Some(response.into_bytes()));
        assert!(matches!(
            send_request(
                client("callback.test", address, true),
                local_url("callback.test", address),
                headers(),
                b"{}"
            )
            .await,
            Err(CallbackError("callback_response_limit"))
        ));
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn stalled_https_is_cancelled_within_absolute_ten_second_budget() {
    let (address, worker) = tls_server(None);
    let started = Instant::now();
    let result = callback_deadline(async {
        // Stand in for time already consumed by DNS. The HTTP client's own
        // ten-second timeout must not reset the callback's absolute budget.
        tokio::time::sleep(Duration::from_secs(1)).await;
        send_request(
            client("callback.test", address, true),
            local_url("callback.test", address),
            headers(),
            b"{}",
        )
        .await
    })
    .await;
    assert!(matches!(result, Err(CallbackError("callback_timeout"))));
    assert!(started.elapsed() >= Duration::from_secs(9));
    assert!(started.elapsed() < Duration::from_secs(12));
    // Keep polling the current-thread runtime while Hyper processes cancellation
    // and closes its owned socket; a blocking join here would starve that cleanup.
    let observed = tokio::task::spawn_blocking(move || worker.join().unwrap())
        .await
        .unwrap();
    assert!(
        observed.peer_closed,
        "timed-out request must release its TLS socket"
    );
}

// Reexecute this Rust test runner as the fake resolver. No production test mode,
// shell helper, unbounded orphan, external DNS or Python harness is introduced.
#[test]
fn owned_resolver_stall_child() {
    if let Some(path) = std::env::var_os("PIGEONMQ_SYNTHETIC_DNS_PID") {
        std::fs::write(path, std::process::id().to_string()).unwrap();
        thread::sleep(Duration::from_secs(60));
    }
}
fn stalled_resolver(path: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "events::callback_transport::transport_tests::owned_resolver_stall_child",
            "--nocapture",
        ])
        .env("PIGEONMQ_SYNTHETIC_DNS_PID", path);
    command
}
async fn assert_child_gone(path: &Path) {
    let pid: i32 = std::fs::read_to_string(path).unwrap().parse().unwrap();
    for _ in 0..100 {
        // Signal zero only checks existence; it never signals another process.
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("owned resolver child remains after cancellation");
}

#[tokio::test]
async fn dns_deadline_kills_and_reaps_owned_child() {
    let temp = tempfile::tempdir().unwrap();
    let pid = temp.path().join("pid");
    let started = Instant::now();
    assert!(matches!(
        resolve_command(stalled_resolver(&pid)).await,
        Err(CallbackError("callback_dns_timeout"))
    ));
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_child_gone(&pid).await;
}

#[tokio::test]
async fn caller_cancellation_also_kills_resolver_child() {
    let temp = tempfile::tempdir().unwrap();
    let pid = temp.path().join("pid");
    assert!(
        timeout(
            Duration::from_millis(500),
            resolve_command(stalled_resolver(&pid))
        )
        .await
        .is_err()
    );
    assert_child_gone(&pid).await;
}
