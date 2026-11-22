use super::*;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const PROJECT: &str = "https://project.example.test";
const KEY: &str = "sb_publishable_fixture";
const CODE: &str = "123456";
const EMAIL: &str = "invited@example.test";
const USER_ID: &str = "fa8554e2-d187-4b31-b6f5-976ccb92a198";

fn auth() -> SupabaseAuth {
    SupabaseAuth::new(PROJECT, KEY).unwrap()
}

#[test]
fn validates_email_and_numeric_code() {
    assert_eq!(
        normalized_email(" Invited@EXAMPLE.test ").as_deref(),
        Some(EMAIL)
    );
    assert!(normalized_email("two@@example.test").is_none());
    assert!(valid_otp(CODE));
    assert!(!valid_otp("123a56"));
}

#[test]
fn reject_insecure_or_ambiguous_configuration() {
    for base in [
        "http://project.example.test",
        "https://project.example.test/path",
        "https://user@project.example.test",
        "https://project.example.test?x=1",
    ] {
        assert!(matches!(
            SupabaseAuth::new(base, KEY),
            Err(SupabaseAuthError::Settings)
        ));
    }
    assert!(matches!(
        SupabaseAuth::new(PROJECT, "sb_secret_fixture"),
        Err(SupabaseAuthError::Settings)
    ));
}

async fn request(listener: &TcpListener) -> (TcpStream, String, HashMap<String, String>, Vec<u8>) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(stream);
    let mut start = String::new();
    stream.read_line(&mut start).await.unwrap();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        assert!(stream.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .map_or(0, |length| length.parse().unwrap());
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (
        stream.into_inner(),
        start.trim_end().to_owned(),
        headers,
        body,
    )
}

async fn respond(mut stream: TcpStream, status: &str, extra: &str, body: &str) {
    let message = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(message.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn exchange(
    status: &'static str,
    extra: &'static str,
    user: String,
) -> Result<Uuid, SupabaseAuthError> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut auth = auth();
    auth.project = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let (stream, start, headers, body) = request(&listener).await;
        assert_eq!(start, "POST /auth/v1/verify HTTP/1.1");
        assert_eq!(headers["apikey"], KEY);
        assert!(!headers.contains_key("authorization"));
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "email": EMAIL, "token": CODE, "type": "email" })
        );
        respond(
            stream,
            "200 OK",
            "",
            r#"{"access_token":"fixture-access","user":{"id":"ignored"}}"#,
        )
        .await;
        let (stream, start, headers, _) = request(&listener).await;
        assert_eq!(start, "GET /auth/v1/user HTTP/1.1");
        assert_eq!(headers["apikey"], KEY);
        assert_eq!(headers["authorization"], "Bearer fixture-access");
        respond(stream, status, extra, &user).await;
        if status.starts_with("30") {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "followed a redirect"
            );
        }
    });
    let result = auth.verify_otp(EMAIL, CODE).await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn exchange_returns_only_verified_non_anonymous_uuid() {
    let user = format!(
        r#"{{"id":"{USER_ID}","role":"authenticated","is_anonymous":false,"email":"ignored@example.test"}}"#
    );
    assert_eq!(
        exchange("200 OK", "", user).await,
        Ok(USER_ID.parse().unwrap())
    );

    let anonymous = format!(r#"{{"id":"{USER_ID}","role":"authenticated","is_anonymous":true}}"#);
    assert_eq!(
        exchange("200 OK", "", anonymous).await,
        Err(SupabaseAuthError::Rejected)
    );
}

#[tokio::test]
async fn upstream_rejection_and_redirect_fail_closed() {
    assert_eq!(
        exchange("401 Unauthorized", "", "upstream details".into()).await,
        Err(SupabaseAuthError::Rejected)
    );
    assert_eq!(
        exchange("302 Found", "Location: /untrusted\r\n", String::new()).await,
        Err(SupabaseAuthError::Unavailable)
    );
}

#[tokio::test]
async fn otp_request_disables_user_creation_and_uses_no_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut auth = auth();
    auth.project = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let (stream, start, headers, body) = request(&listener).await;
        assert_eq!(start, "POST /auth/v1/otp HTTP/1.1");
        assert_eq!(headers["apikey"], KEY);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"email": EMAIL, "create_user": false})
        );
        respond(stream, "200 OK", "", "{}").await;
    });
    assert_eq!(auth.request_otp(EMAIL).await, Ok(()));
    server.await.unwrap();
}
