//! Local HTTP client for the hosted registration service.
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone)]
pub struct Registration {
    pub installation_id: String,
    pub public_url: String,
    pub login_url: String,
    pub expires_at: String,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Registration")
            .field("installation_id", &self.installation_id)
            .field("login_url", &"[private handoff]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Deserialize)]
struct BeginResponse {
    installation_id: String,
    public_url: String,
    login_url: String,
    expires_at: String,
}

#[derive(Deserialize)]
pub(crate) struct StatusResponse {
    pub(crate) state: String,
    pub(crate) public_url: String,
}

#[derive(Deserialize)]
pub(crate) struct FinishResponse {
    state: String,
    pub(crate) public_url: String,
    pub(crate) connector_token: String,
    pub(crate) hub_target: Option<HubTarget>,
}

// This scope comes from the same operator settings used to build Cloudflare ingress.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HubTarget {
    pub(crate) port: u16,
    pub(crate) mcp_paths: Vec<String>,
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
}

fn map_error(status: reqwest::StatusCode, body: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(body).ok();
    match (
        status.as_u16(),
        value.as_ref().and_then(|v| v["error"].as_str()),
    ) {
        (410, Some("registration_expired")) => return "registration_expired".into(),
        (410, Some("registration_revoked")) => return "registration_revoked".into(),
        (409, Some("login_pending")) => return "login_pending".into(),
        _ => {}
    }
    format!("http_{}", status.as_u16())
}

pub fn begin(
    base_url: &str,
    request_id: &str,
    user_code: &str,
    proof: &str,
) -> Result<Registration, String> {
    let url = format!("{}/v1/enrollments", base_url.trim_end_matches('/'));
    let resp = client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", proof))
        .json(&json!({ "request_id": request_id, "user_code": user_code }))
        .send()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: BeginResponse =
        serde_json::from_str(&body).map_err(|_| "registration_service_unavailable".to_owned())?;
    Ok(Registration {
        installation_id: parsed.installation_id,
        public_url: parsed.public_url,
        login_url: parsed.login_url,
        expires_at: parsed.expires_at,
    })
}

pub fn status(
    base_url: &str,
    installation_id: &str,
    proof: &str,
) -> Result<StatusResponse, String> {
    // Validate installation_id to prevent path injection.
    if uuid::Uuid::parse_str(installation_id).is_err() {
        return Err("invalid_installation_id".to_string());
    }
    let url = format!(
        "{}/v1/enrollments/{}",
        base_url.trim_end_matches('/'),
        installation_id
    );
    let resp = client()
        .get(&url)
        .header("Authorization", format!("Bearer {}", proof))
        .send()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: StatusResponse =
        serde_json::from_str(&body).map_err(|_| "registration_service_unavailable".to_owned())?;
    Ok(parsed)
}

pub fn finish(
    base_url: &str,
    installation_id: &str,
    proof: &str,
) -> Result<FinishResponse, String> {
    // Validate installation_id to prevent path injection.
    if uuid::Uuid::parse_str(installation_id).is_err() {
        return Err("invalid_installation_id".to_string());
    }
    let url = format!(
        "{}/v1/enrollments/{}/finish",
        base_url.trim_end_matches('/'),
        installation_id
    );
    let resp = client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", proof))
        .send()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|_| "registration_service_unavailable".to_owned())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: FinishResponse =
        serde_json::from_str(&body).map_err(|_| "registration_service_unavailable".to_owned())?;
    if parsed.state != "ready" {
        return Err("unexpected_state".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn mock_server(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{}", addr)
    }

    #[test]
    fn begin_parses_fixture_shape() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\r\n{\"installation_id\":\"11111111-1111-4111-8111-111111111111\",\"public_url\":\"https://u-fixture.example.com\",\"login_url\":\"https://init.example.com/register/11111111-1111-4111-8111-111111111111\",\"expires_at\":\"2030-01-01T00:15:00Z\"}";
        let base = mock_server(resp);
        let reg = begin(&base, "req-1", "CODE", "proof").unwrap();
        assert_eq!(reg.installation_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(reg.public_url, "https://u-fixture.example.com");
    }

    #[test]
    fn status_returns_state() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"approved\",\"public_url\":\"https://u-fixture.example.com\"}";
        let base = mock_server(resp);
        // Need installation_id in URL; mock ignores path.
        let state = status(&base, "11111111-1111-4111-8111-111111111111", "proof").unwrap();
        assert_eq!(state.state, "approved");
    }

    #[test]
    fn finish_returns_token() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"ready\",\"public_url\":\"https://u-fixture.example.com\",\"connector_token\":\"tok-123\"}";
        let base = mock_server(resp);
        let result = finish(&base, "11111111-1111-4111-8111-111111111111", "proof").unwrap();
        assert_eq!(result.public_url, "https://u-fixture.example.com");
        assert_eq!(result.connector_token, "tok-123");
        assert!(result.hub_target.is_none());
    }

    #[test]
    fn maps_login_pending() {
        let resp = "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\n\r\n{\"error\":\"login_pending\"}";
        let base = mock_server(resp);
        let err = begin(&base, "req-1", "CODE", "proof").unwrap_err();
        assert_eq!(err, "login_pending");
    }
}
