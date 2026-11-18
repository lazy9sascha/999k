//! Shared loopback HTTP client with a total deadline, not a response-size limit.
//! One connection, no request replay.
use serde_json::Value;
use std::{
    io, thread,
    time::{Duration, Instant},
};

pub const POST_TIMEOUT: Duration = Duration::from_secs(15);
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "hub_deadline_exceeded"))
}

fn endpoint(port: u16, path: &str) -> io::Result<reqwest::Url> {
    // Build authority independently from the origin-form request target.
    if port == 0
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '#'])
        || path.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid_hub_path",
        ));
    }
    let mut url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid_hub_path"))?;
    let (path, query) = path
        .split_once('?')
        .map_or((path, None), |(path, query)| (path, Some(query)));
    url.set_path(path);
    url.set_query(query);
    Ok(url)
}

fn transport_error(error: reqwest::Error) -> io::Error {
    if error.is_timeout() {
        return io::Error::new(io::ErrorKind::TimedOut, "hub_deadline_exceeded");
    }
    if error.is_connect() {
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            if let Some(cause) = cause.downcast_ref::<io::Error>() {
                return io::Error::new(cause.kind(), "hub_connection_failed");
            }
            source = cause.source();
        }
        return io::Error::other("hub_connection_failed");
    }
    // Never expose request URLs, tokens or dependency diagnostic text.
    io::Error::other("bad_http_response")
}

pub fn post(port: u16, path: &str, token: &str, body: &Value) -> io::Result<(u16, Value)> {
    post_until(port, path, token, body, Instant::now() + POST_TIMEOUT)
}

pub fn post_until(
    port: u16,
    path: &str,
    token: &str,
    body: &Value,
    deadline: Instant,
) -> io::Result<(u16, Value)> {
    let deadline = deadline.min(Instant::now() + POST_TIMEOUT);
    remaining(deadline)?;
    let url = endpoint(port, path)?;
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid_hub_authorization"))?;
    authorization.set_sensitive(true);
    let body = serde_json::to_vec(body)?;
    remaining(deadline)?;
    // Separate runtime for synchronous callers that themselves run in Tokio.
    // The deadline includes setup time; OS scheduling and join are not a hard
    // wall-clock guarantee. Each request has its own client and no replay.
    thread::scope(|scope| {
        thread::Builder::new()
            .spawn_scoped(scope, move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async move {
                    let client = reqwest::Client::builder()
                        .no_proxy()
                        .redirect(reqwest::redirect::Policy::none())
                        .retry(reqwest::retry::never())
                        .http1_only()
                        .connect_timeout(Duration::from_secs(2))
                        .build()
                        .map_err(|_| io::Error::other("hub_client_unavailable"))?;
                    remaining(deadline)?;
                    tokio::time::timeout_at(deadline.into(), async {
                        let response = client
                            .post(url)
                            .header(reqwest::header::AUTHORIZATION, authorization)
                            .header(reqwest::header::CONTENT_TYPE, "application/json")
                            .header(
                                reqwest::header::ACCEPT,
                                "application/json, text/event-stream",
                            )
                            .header(reqwest::header::CONNECTION, "close")
                            .body(body)
                            .send()
                            .await
                            .map_err(transport_error)?;
                        let status = response.status().as_u16();
                        let bytes = response.bytes().await.map_err(transport_error)?;
                        remaining(deadline)?;
                        let json = serde_json::from_slice(&bytes)
                            .map_err(|_| io::Error::other("bad_http_json"))?;
                        remaining(deadline)?;
                        Ok((status, json))
                    })
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "hub_deadline_exceeded"))?
                })
            })?
            .join()
            .map_err(|_| io::Error::other("hub_http_worker_panicked"))?
    })
}
