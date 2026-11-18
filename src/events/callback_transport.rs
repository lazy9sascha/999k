//! Source: PigeonMQ e399161, src/dot_callback_transport.rs (MIT).
//! Signed, DNS-pinned HTTPS callbacks. No queue mutation or automatic retries.
//! The resolver executable must implement `--dot-resolve HOST` by calling
//! `resolver_child`; it runs as an owned, kill-on-drop child, never a detached
//! blocking resolver thread. Errors deliberately exclude destinations and keys.
use super::{delivery::DeliveryAttempt, policy::EventsPolicy};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use sha2::Sha256;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use url::Url;

const BODY_LIMIT: usize = 262144;
const RESPONSE_LIMIT: usize = 4096;
const DNS_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CallbackError(pub &'static str);
type Result<T> = std::result::Result<T, CallbackError>;

/// This contains signing secrets and intentionally has no Debug implementation.
pub struct SignedCallback<'a> {
    pub url: &'a str,
    pub subscription_id: &'a str,
    pub event_id: &'a str,
    pub body: &'a [u8],
    pub current_secret: &'a str,
    /// Caller supplies this only during the approved rotation window.
    pub previous_secret: Option<&'a str>,
    pub signed_at: f64,
}

pub struct CallbackResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct CallbackTransport {
    resolver_executable: PathBuf,
}
impl CallbackTransport {
    pub fn new(resolver_executable: impl AsRef<Path>) -> Self {
        Self {
            resolver_executable: resolver_executable.as_ref().to_owned(),
        }
    }

    /// A body must be the final encoded event bytes; these exact bytes are signed
    /// and sent. The delivery ledger remains responsible for outcome fencing.
    pub async fn deliver(
        &self,
        policy: &EventsPolicy,
        attempt: &DeliveryAttempt,
        body: &[u8],
        signed_at: f64,
    ) -> Result<CallbackResponse> {
        self.post(
            policy,
            SignedCallback {
                url: attempt.callback_url(),
                subscription_id: attempt.subscription_id(),
                event_id: attempt.event_id(),
                body,
                current_secret: attempt.signing_secret(),
                previous_secret: attempt.previous_signing_secret(signed_at),
                signed_at,
            },
        )
        .await
    }

    pub async fn post(
        &self,
        policy: &EventsPolicy,
        request: SignedCallback<'_>,
    ) -> Result<CallbackResponse> {
        // One absolute budget includes DNS, connect, TLS, upload and response.
        callback_deadline(self.post_inner(policy, request)).await
    }

    async fn post_inner(
        &self,
        policy: &EventsPolicy,
        request: SignedCallback<'_>,
    ) -> Result<CallbackResponse> {
        let url = callback_url(policy, request.url)?;
        let headers = signed_headers(&request)?;
        let host = url.host_str().ok_or(CallbackError("invalid_callback"))?;
        let addresses = self.resolve(host).await?;
        let client = pinned_client(host, SocketAddr::new(addresses[0], 443))
            .build()
            .map_err(|_| CallbackError("callback_transport"))?;
        send_request(client, url, headers, request.body).await
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>> {
        let mut command = Command::new(&self.resolver_executable);
        command.arg("--dot-resolve").arg(host);
        resolve_command(command).await
    }
}

async fn callback_deadline<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    timeout(Duration::from_secs(10), future)
        .await
        .map_err(|_| CallbackError("callback_timeout"))?
}

fn pinned_client(host: &str, address: SocketAddr) -> reqwest::ClientBuilder {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .resolve(host, address)
}

async fn send_request(
    client: Client,
    url: Url,
    headers: HeaderMap,
    request_body: &[u8],
) -> Result<CallbackResponse> {
    // The original URL preserves Host and TLS SNI/certificate validation.
    // A fresh client prevents connection reuse across policy/DNS snapshots.
    let mut response = client
        .post(url)
        .headers(headers)
        .body(request_body.to_vec())
        .send()
        .await
        .map_err(|_| CallbackError("callback_transport"))?;
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|n| n > RESPONSE_LIMIT as u64)
    {
        return Err(CallbackError("callback_response_limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CallbackError("callback_transport"))?
    {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err(CallbackError("callback_response_limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(CallbackResponse { status, body })
}

async fn resolve_command(mut command: Command) -> Result<Vec<IpAddr>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| CallbackError("callback_dns"))?;
    let result = timeout(Duration::from_secs(3), async {
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .ok_or(CallbackError("callback_dns"))?
            .take((DNS_LIMIT + 1) as u64)
            .read_to_end(&mut output)
            .await
            .map_err(|_| CallbackError("callback_dns"))?;
        if output.len() > DNS_LIMIT {
            return Err(CallbackError("callback_dns"));
        }
        if !child
            .wait()
            .await
            .map_err(|_| CallbackError("callback_dns"))?
            .success()
        {
            return Err(CallbackError("callback_dns"));
        }
        let addresses: Vec<IpAddr> =
            serde_json::from_slice(&output).map_err(|_| CallbackError("callback_dns"))?;
        validate_public_addresses(&addresses)?;
        Ok(addresses)
    })
    .await;
    // Explicitly reap on errors; outer cancellation also kills on drop.
    match result {
        Ok(Ok(addresses)) => Ok(addresses),
        failure => {
            let _ = child.kill().await;
            Err(match failure {
                Ok(Err(error)) => error,
                _ => CallbackError("callback_dns_timeout"),
            })
        }
    }
}

#[cfg(test)]
#[path = "callback_transport_tests.rs"]
mod transport_tests;

pub fn validate_callback_url(policy: &EventsPolicy, raw: &str) -> Result<()> {
    callback_url(policy, raw).map(|_| ())
}

fn callback_url(policy: &EventsPolicy, raw: &str) -> Result<Url> {
    if raw.chars().count() > 2048 || !policy.callback_allowed(raw) {
        return Err(CallbackError("invalid_callback"));
    }
    let mut url = Url::parse(raw).map_err(|_| CallbackError("invalid_callback"))?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
        || !matches!(url.host(), Some(url::Host::Domain(_)))
    {
        return Err(CallbackError("invalid_callback"));
    }
    url.set_fragment(None);
    // Empty userinfo is accepted by the legacy parser but is not HTTP Basic auth.
    url.set_username("")
        .map_err(|_| CallbackError("invalid_callback"))?;
    url.set_password(None)
        .map_err(|_| CallbackError("invalid_callback"))?;
    Ok(url)
}

fn signature(secret: &str, event: &str, stamp: &str, body: &[u8]) -> Result<String> {
    let key = signing_key(secret)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key)
        .map_err(|_| CallbackError("invalid_signing_secret"))?;
    mac.update(event.as_bytes());
    mac.update(b".");
    mac.update(stamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

/// Validates subscription key syntax and length without returning secret bytes.
pub fn validate_signing_secret(secret: &str) -> Result<()> {
    signing_key(secret).map(|_| ())
}

fn signing_key(secret: &str) -> Result<Vec<u8>> {
    if secret.len() > 94 {
        return Err(CallbackError("invalid_signing_secret"));
    }
    let encoded = secret
        .strip_prefix("whsec_")
        .ok_or(CallbackError("invalid_signing_secret"))?;
    let key = STANDARD
        .decode(encoded)
        .map_err(|_| CallbackError("invalid_signing_secret"))?;
    if !(24..=64).contains(&key.len()) {
        return Err(CallbackError("invalid_signing_secret"));
    }
    Ok(key)
}

pub fn signed_headers(request: &SignedCallback<'_>) -> Result<HeaderMap> {
    if request.body.len() > BODY_LIMIT {
        return Err(CallbackError("callback_body_limit"));
    }
    if !request.signed_at.is_finite()
        || request.signed_at < 0.0
        || request.signed_at >= i64::MAX as f64
    {
        return Err(CallbackError("invalid_time"));
    }
    let stamp = (request.signed_at as i64).to_string();
    let mut sig = signature(
        request.current_secret,
        request.event_id,
        &stamp,
        request.body,
    )?;
    if let Some(previous) = request.previous_secret {
        sig.push(' ');
        sig.push_str(&signature(
            previous,
            request.event_id,
            &stamp,
            request.body,
        )?);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/json"),
        ("webhook-id", request.event_id),
        ("webhook-timestamp", &stamp),
        ("webhook-signature", &sig),
        ("x-mcp-subscription-id", request.subscription_id),
    ] {
        if value.is_empty() || value.len() > 1024 {
            return Err(CallbackError("invalid_callback_header"));
        }
        headers.insert(
            name,
            HeaderValue::from_str(value).map_err(|_| CallbackError("invalid_callback_header"))?,
        );
    }
    Ok(headers)
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    // IANA special-purpose ranges, including shared, documentation, benchmarking,
    // link-local, private, reserved, multicast and limited broadcast.
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 168 || (b == 0 && (c == 0 || c == 2))))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Only global-unicast space; reject transition, special-purpose and
            // documentation blocks conservatively (including mapped/NAT64).
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

/// Reject empty, oversized, or non-public DNS answer sets.
pub fn validate_public_addresses(addresses: &[IpAddr]) -> Result<()> {
    if addresses.is_empty() || addresses.len() > 32 {
        return Err(CallbackError("callback_dns"));
    }
    if addresses.iter().any(|ip| !public_ip(*ip)) {
        return Err(CallbackError("callback_non_public"));
    }
    Ok(())
}

/// Native helper entrypoint. Its parent owns the three-second resolver deadline.
/// Return bytes rather than printing so the host controls stdout and exit status.
pub fn resolver_child(host: &str) -> Result<Vec<u8>> {
    if host.is_empty()
        || host.len() > 253
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(CallbackError("callback_dns"));
    }
    let mut addresses = Vec::new();
    for address in (host, 443)
        .to_socket_addrs()
        .map_err(|_| CallbackError("callback_dns"))?
    {
        if !addresses.contains(&address.ip()) {
            addresses.push(address.ip());
        }
        if addresses.len() > 32 {
            return Err(CallbackError("callback_dns"));
        }
    }
    validate_public_addresses(&addresses)?;
    serde_json::to_vec(&addresses).map_err(|_| CallbackError("callback_dns"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_special_and_mixed_dns_answers() {
        for raw in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001::1",
            "2001:db8::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "3fff::1",
        ] {
            assert!(!public_ip(raw.parse().unwrap()), "{raw}");
            assert!(
                validate_public_addresses(&["8.8.8.8".parse().unwrap(), raw.parse().unwrap()])
                    .is_err()
            );
        }
        for raw in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(raw.parse().unwrap()));
        }
        assert!(validate_public_addresses(&[]).is_err());
        assert!(validate_public_addresses(&vec!["8.8.8.8".parse().unwrap(); 33]).is_err());
    }

    #[test]
    fn signing_key_bounds() {
        for n in [0, 23, 65] {
            assert!(
                signature(
                    &format!("whsec_{}", STANDARD.encode(vec![0; n])),
                    "evt",
                    "1",
                    b"{}"
                )
                .is_err()
            );
        }
        for n in [24, 32, 64] {
            assert!(
                signature(
                    &format!("whsec_{}", STANDARD.encode(vec![0; n])),
                    "evt",
                    "1",
                    b"{}"
                )
                .is_ok()
            );
        }
        assert!(signature("whsec_!!!", "evt", "1", b"{}").is_err());
    }

    #[test]
    fn dual_signatures_and_exact_body() {
        let key = format!("whsec_{}", STANDARD.encode([0x0b; 24]));
        let old = format!("whsec_{}", STANDARD.encode([0x0c; 24]));
        let request = SignedCallback {
            url: "https://example.com",
            subscription_id: "sub",
            event_id: "evt",
            body: b"{}",
            current_secret: &key,
            previous_secret: Some(&old),
            signed_at: 1234.99,
        };
        let headers = signed_headers(&request).unwrap();
        // Independently computed with OpenSSL HMAC-SHA256 over evt.1234.{}.
        assert_eq!(
            signature(&key, "evt", "1234", b"{}").unwrap(),
            "v1,sZt81mWasuZLcvLAqi/IR6YcEru+DjwcndFZZADOY3A="
        );
        let expected = format!(
            "{} {}",
            signature(&key, "evt", "1234", b"{}").unwrap(),
            signature(&old, "evt", "1234", b"{}").unwrap()
        );
        assert_eq!(headers["webhook-signature"], expected);
        assert_eq!(headers["webhook-timestamp"], "1234");
        assert_ne!(
            signature(&key, "evt", "1234", b"{}").unwrap(),
            signature(&key, "evt", "1234", b"{ }").unwrap()
        );
        assert!(
            signed_headers(&SignedCallback {
                event_id: "x\r\nBad: yes",
                ..request
            })
            .is_err()
        );
    }

    #[test]
    fn strict_callback_url_gate() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("policy.json");
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"principal":"dot-owner", "mailbox":"dot", "token_sha256":"a".repeat(64),"senders":["alice"],"callback_hosts":["example.com"],"enabled":true})).unwrap()).unwrap();
        let policy = EventsPolicy::load(&path).unwrap();
        for raw in [
            "http://example.com/x",
            "https://example.com:444/x",
            "https://evil.example/x",
            "https://example.com@evil.example/x",
            "https://user@example.com/x",
            "https://example.com/#fragment",
            "https://example.com/a b",
            "https://127.0.0.1",
            "https://example.com\\evil/x",
        ] {
            assert!(callback_url(&policy, raw).is_err(), "{raw}");
        }
        assert!(callback_url(&policy, "https://example.com:443/path?q=1").is_ok());
        for raw in [
            "https://example.com/a@b?q=a@b",
            "https://example.com/#",
            "https://@example.com/x",
            "https://:@example.com/x",
        ] {
            assert!(callback_url(&policy, raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn body_and_clock_bounds() {
        let key = format!("whsec_{}", STANDARD.encode([0x0b; 24]));
        let oversized = vec![0; BODY_LIMIT + 1];
        let request = SignedCallback {
            url: "https://example.com",
            subscription_id: "sub",
            event_id: "evt",
            body: &oversized,
            current_secret: &key,
            previous_secret: None,
            signed_at: 1.0,
        };
        assert!(signed_headers(&request).is_err());
        for signed_at in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(
                signed_headers(&SignedCallback {
                    body: b"{}",
                    signed_at,
                    ..request
                })
                .is_err()
            );
        }
        assert!(resolver_child("x\ninvalid").is_err());
    }
}
