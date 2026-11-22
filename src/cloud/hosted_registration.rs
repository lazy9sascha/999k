//! Local HTTP client for the hosted registration service.
#![allow(dead_code)]
use serde::Deserialize;
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
struct StatusResponse {
    state: String,
}

#[derive(Deserialize)]
struct FinishResponse {
    state: String,
    public_url: String,
    connector_token: String,
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("http client")
}

fn map_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(code) = v.get("error").and_then(|e| e.as_str()) {
            return code.to_string();
        }
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
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: BeginResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(Registration {
        installation_id: parsed.installation_id,
        public_url: parsed.public_url,
        login_url: parsed.login_url,
        expires_at: parsed.expires_at,
    })
}

pub fn status(base_url: &str, installation_id: &str, proof: &str) -> Result<String, String> {
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
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: StatusResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(parsed.state)
}

pub fn finish(
    base_url: &str,
    installation_id: &str,
    proof: &str,
) -> Result<(String, String), String> {
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
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(map_error(status, &body));
    }
    let parsed: FinishResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if parsed.state != "ready" {
        return Err("unexpected_state".to_string());
    }
    Ok((parsed.public_url, parsed.connector_token))
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
        assert_eq!(state, "approved");
    }

    #[test]
    fn finish_returns_token() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"ready\",\"public_url\":\"https://u-fixture.example.com\",\"connector_token\":\"tok-123\"}";
        let base = mock_server(resp);
        let (url, token) = finish(&base, "11111111-1111-4111-8111-111111111111", "proof").unwrap();
        assert_eq!(url, "https://u-fixture.example.com");
        assert_eq!(token, "tok-123");
    }

    #[test]
    fn maps_login_pending() {
        let resp = "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\n\r\n{\"error\":\"login_pending\"}";
        let base = mock_server(resp);
        let err = begin(&base, "req-1", "CODE", "proof").unwrap_err();
        assert_eq!(err, "login_pending");
    }
}
