//! Authenticated client of the common hub APIs. Never opens a queue.
use crate::{hub_http, receiver_api::Request};
use serde_json::Value;
use std::{io::Read, path::PathBuf};

pub trait Transport: Send + Sync {
    fn call(&self, request: Request) -> Result<Value, &'static str>;
}

pub struct Client {
    port: u16,
    token_file: PathBuf,
}
impl Client {
    pub fn new(port: u16, token_file: PathBuf) -> Result<Self, &'static str> {
        if port == 0 || !token_file.is_absolute() {
            return Err("invalid_receiver_api_config");
        }
        Ok(Self { port, token_file })
    }
    fn post(&self, body: &Value) -> Result<Value, &'static str> {
        let mut token = String::new();
        crate::private_file::open_private_file(&self.token_file)
            .map_err(|_| "receiver_credential_unavailable")?
            .take(257)
            .read_to_string(&mut token)
            .map_err(|_| "receiver_credential_unavailable")?;
        let token = token.trim();
        // FIXME(yagni): length/charset check on our own token file - the hub rejects a bad token anyway
        if token.is_empty() || token.len() > 256 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("invalid_receiver_credential");
        }
        // An I/O failure can follow a committed claim or completion. Do not replay.
        let (status, value) = hub_http::post(
            self.port,
            crate::mailbox_catalog::by_adapter(crate::mailbox_catalog::Adapter::Receiver).route,
            token,
            body,
        )
        .map_err(|_| "receiver_api_outcome_uncertain")?;
        match status {
            200 => Ok(value),
            401 => Err("receiver_api_unauthorized"),
            403 => Err("receiver_api_forbidden"),
            404 => Err("receiver_api_unavailable_or_not_found"),
            409 if value["error"]["code"] == "existing_claim_requires_reconciliation" => {
                Err("existing_claim_requires_reconciliation")
            }
            409 => Err("receiver_api_conflict"),
            _ => Err("receiver_api_rejected"),
        }
    }
}
impl Transport for Client {
    fn call(&self, request: Request) -> Result<Value, &'static str> {
        let body = serde_json::to_value(request).map_err(|_| "invalid_receiver_request")?;
        self.post(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver_api::Action;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        thread,
    };

    fn request() -> Request {
        Request {
            recipient: "fixture-agent".into(),
            session_id: "fixture-session".into(),
            profile_id: "fixture-profile".into(),
            action: Action::Claim { lease_seconds: 180 },
        }
    }

    fn exchange(response: &'static str) -> (Result<Value, &'static str>, Value) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "fixture-token\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = None;
            let mut first = true;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if first {
                    assert_eq!(line, "POST /receiver HTTP/1.1\r\n");
                    first = false;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(":")
                    && name.eq_ignore_ascii_case("content-length")
                {
                    assert!(length.is_none());
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            reader.get_mut().write_all(response.as_bytes()).unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        });
        let result = Client::new(port, token).unwrap().call(request());
        (result, server.join().unwrap())
    }

    #[test]
    fn sends_common_scoped_request_and_accepts_empty_inbox() {
        let (result, sent) =
            exchange("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull");
        assert_eq!(result.unwrap(), Value::Null);
        assert_eq!(
            sent,
            json!({"recipient":"fixture-agent","session_id":"fixture-session", "profile_id":"fixture-profile", "action":{"operation":"claim","lease_seconds":180}})
        );
    }

    #[test]
    fn malformed_success_and_refusal_are_not_empty_inbox_success() {
        let (result, _) = exchange("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nnot-json");
        assert_eq!(result, Err("receiver_api_outcome_uncertain"));
        let (result, _) = exchange(
            "HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"private fixture details\"}}",
        );
        assert_eq!(result, Err("receiver_api_forbidden"));
        let (result, _) = exchange(
            "HTTP/1.1 409 Conflict\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"existing_claim_requires_reconciliation\"}}",
        );
        assert_eq!(result, Err("existing_claim_requires_reconciliation"));
        let (result, _) = exchange(
            "HTTP/1.1 409 Conflict\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"conflict\"}}",
        );
        assert_eq!(result, Err("receiver_api_conflict"));
    }
}
