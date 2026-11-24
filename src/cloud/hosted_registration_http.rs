use std::time::Duration;

use super::hosted_origin::UnixOrigin;
use super::hosted_supabase::{normalized_email, valid_otp};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Body;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const BODY_LIMIT: usize = 4096;
const BODY_DEADLINE: Duration = Duration::from_secs(5);
const ATTEMPT_COOKIE: &str = "__Host-pigeonmq-registration";

pub(crate) type RegistrationResponse = Response<Full<Bytes>>;

// Request fields can contain secrets. Do not derive Debug.
pub(crate) enum RequestKind {
    Begin {
        request_id: Uuid,
        user_code: String,
        proof: String,
    },
    Status {
        installation_id: Uuid,
        proof: String,
    },
    Finish {
        installation_id: Uuid,
        proof: String,
    },
    FinishUnix {
        installation_id: Uuid,
        proof: String,
        origin: UnixOrigin,
    },
    RegisterPage {
        installation_id: Uuid,
    },
    RegisterSubmit {
        installation_id: Uuid,
        handoff: String,
        email: String,
    },
    VerifyEmail {
        installation_id: Uuid,
        otp: String,
        attempt: Uuid,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegistrationHttpError {
    InvalidRequest,
    RegistrationNotFound,
    LoginPending,
    RegistrationExpired,
    RegistrationRevoked,
    RateLimited,
    TemporarilyUnavailable,
}

impl RegistrationHttpError {
    pub(crate) fn response(self) -> RegistrationResponse {
        let (status, code) = match self {
            Self::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::RegistrationNotFound => (StatusCode::NOT_FOUND, "registration_not_found"),
            Self::LoginPending => (StatusCode::CONFLICT, "login_pending"),
            Self::RegistrationExpired => (StatusCode::GONE, "registration_expired"),
            Self::RegistrationRevoked => (StatusCode::GONE, "registration_revoked"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::TemporarilyUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
            }
        };
        json_response(status, json!({ "error": code }))
    }
}

use RegistrationHttpError::{InvalidRequest, RegistrationNotFound, TemporarilyUnavailable};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginInput {
    request_id: String,
    user_code: String,
}

pub(crate) async fn parse_request<B>(
    request: Request<B>,
) -> Result<RequestKind, RegistrationHttpError>
where
    B: Body<Data = Bytes>,
{
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let query = parts.uri.query();
    if query.is_some() {
        return Err(InvalidRequest);
    }

    // Match exactly. A path identifier never doubles as an installation proof.
    let segments: Vec<_> = path.split('/').collect();
    match (parts.method, segments.as_slice()) {
        (Method::POST, ["", "v1", "enrollments"]) => {
            let proof = installation_proof(&parts.headers).ok_or(InvalidRequest)?;
            let input: BeginInput =
                serde_json::from_slice(&bounded_body(body).await?).map_err(|_| InvalidRequest)?;
            let request_id = path_uuid(&input.request_id)?;
            let user_code = valid_user_code(input.user_code)?;
            Ok(RequestKind::Begin {
                request_id,
                user_code,
                proof,
            })
        }
        (Method::GET, ["", "v1", "enrollments", installation_id]) => {
            let installation_id = path_uuid(installation_id)?;
            let proof = installation_proof(&parts.headers).ok_or(RegistrationNotFound)?;
            require_empty_body(body).await?;
            Ok(RequestKind::Status {
                installation_id,
                proof,
            })
        }
        (Method::POST, ["", "v1", "enrollments", installation_id, "finish"]) => {
            let installation_id = path_uuid(installation_id)?;
            let proof = installation_proof(&parts.headers).ok_or(RegistrationNotFound)?;
            require_empty_body(body).await?;
            Ok(RequestKind::Finish {
                installation_id,
                proof,
            })
        }
        (Method::POST, ["", "v1", "enrollments", installation_id, "finish-unix"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct UnixInput {
                kind: String,
                nonce: String,
            }
            let installation_id = path_uuid(installation_id)?;
            let proof = installation_proof(&parts.headers).ok_or(RegistrationNotFound)?;
            let payload: UnixInput =
                serde_json::from_slice(&bounded_body(body).await?).map_err(|_| InvalidRequest)?;
            if payload.kind != "unix_v1" {
                return Err(InvalidRequest);
            }
            let origin = UnixOrigin::parse(&payload.nonce).ok_or(InvalidRequest)?;
            Ok(RequestKind::FinishUnix {
                installation_id,
                proof,
                origin,
            })
        }
        (Method::GET, ["", "register", installation_id]) => {
            let installation_id = path_uuid(installation_id)?;
            require_empty_body(body).await?;
            Ok(RequestKind::RegisterPage { installation_id })
        }
        (Method::POST, ["", "register", installation_id]) => {
            let installation_id = path_uuid(installation_id)?;
            require_form(&parts.headers)?;
            let input = bounded_body(body).await?;
            let mut handoff = None;
            let mut email = None;
            for (key, value) in url::form_urlencoded::parse(&input) {
                match key.as_ref() {
                    "handoff" if handoff.is_none() => {
                        let bytes = URL_SAFE_NO_PAD
                            .decode(&*value)
                            .map_err(|_| InvalidRequest)?;
                        if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != value {
                            return Err(InvalidRequest);
                        }
                        handoff = Some(value.into_owned());
                    }
                    "email" if email.is_none() => {
                        email = Some(normalized_email(&value).ok_or(InvalidRequest)?)
                    }
                    _ => return Err(InvalidRequest),
                }
            }
            Ok(RequestKind::RegisterSubmit {
                installation_id,
                handoff: handoff.ok_or(InvalidRequest)?,
                email: email.ok_or(InvalidRequest)?,
            })
        }
        (Method::POST, ["", "register", installation_id, "verify"]) => {
            require_form(&parts.headers)?;
            let installation_id = path_uuid(installation_id)?;
            let attempt = path_uuid(&browser_attempt(&parts.headers)?)?;
            let otp = single_url_field(&bounded_body(body).await?, "otp")?;
            if !valid_otp(&otp) {
                return Err(InvalidRequest);
            }
            Ok(RequestKind::VerifyEmail {
                installation_id,
                otp,
                attempt,
            })
        }
        _ => Err(RegistrationNotFound),
    }
}

fn require_form(headers: &HeaderMap) -> Result<(), RegistrationHttpError> {
    if single_header(headers, header::CONTENT_TYPE)
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.eq_ignore_ascii_case("application/x-www-form-urlencoded"))
    {
        Ok(())
    } else {
        Err(InvalidRequest)
    }
}

fn path_uuid(input: &str) -> Result<Uuid, RegistrationHttpError> {
    let uuid = Uuid::try_parse(input).map_err(|_| InvalidRequest)?;
    if input != uuid.hyphenated().to_string() {
        return Err(InvalidRequest);
    }
    Ok(uuid)
}

fn valid_user_code(code: String) -> Result<String, RegistrationHttpError> {
    if code.len() == 8
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        Ok(code)
    } else {
        Err(InvalidRequest)
    }
}

fn single_header(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn installation_proof(headers: &HeaderMap) -> Option<String> {
    let auth = single_header(headers, header::AUTHORIZATION)?;
    let (scheme, proof) = auth.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") || proof.len() != 43 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(proof).ok()?;
    (bytes.len() == 32).then(|| proof.to_owned())
}

fn single_url_field(input: &[u8], field: &str) -> Result<String, RegistrationHttpError> {
    let mut pairs = url::form_urlencoded::parse(input);
    let (name, value) = pairs.next().ok_or(InvalidRequest)?;
    if name != field || pairs.next().is_some() {
        return Err(InvalidRequest);
    }
    Ok(value.into_owned())
}

fn browser_attempt(headers: &HeaderMap) -> Result<String, RegistrationHttpError> {
    let mut attempt = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().map_err(|_| InvalidRequest)?;
        for (name, value) in value
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
        {
            if name == ATTEMPT_COOKIE {
                if attempt.is_some() || !valid_attempt_cookie(value) {
                    return Err(InvalidRequest);
                }
                attempt = Some(value);
            }
        }
    }
    attempt.map(str::to_owned).ok_or(InvalidRequest)
}

fn valid_attempt_cookie(input: &str) -> bool {
    !input.is_empty()
        && input.len() <= 128
        && input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn bounded_body<B: Body<Data = Bytes>>(body: B) -> Result<Bytes, RegistrationHttpError> {
    let body = body.map_err(|_| std::io::Error::other("invalid registration body"));
    let body = tokio::time::timeout(BODY_DEADLINE, Limited::new(body, BODY_LIMIT).collect())
        .await
        .map_err(|_| InvalidRequest)?
        .map_err(|_| InvalidRequest)?;
    if body.trailers().is_some() {
        return Err(InvalidRequest);
    }
    Ok(body.to_bytes())
}

async fn require_empty_body<B: Body<Data = Bytes>>(body: B) -> Result<(), RegistrationHttpError> {
    if !bounded_body(body).await?.is_empty() {
        return Err(InvalidRequest);
    }
    Ok(())
}

pub(crate) fn json_response(status: StatusCode, payload: Value) -> RegistrationResponse {
    response(status, "application/json", payload.to_string())
}

fn response(status: StatusCode, content_type: &'static str, body: String) -> RegistrationResponse {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn html_response(body: String) -> RegistrationResponse {
    let mut response = response(StatusCode::OK, "text/html; charset=utf-8", body);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

// The fragment stays in the browser. Only the user's form submission sends it.
const HANDOFF_SCRIPT: &str = r#"(() => {
const value = location.hash.slice(1);
if (location.hash) history.replaceState(null, "", location.pathname);
if (!/^[A-Za-z0-9_-]{43}$/.test(value)) return;
const form = document.getElementById("request");
form.addEventListener("submit", () => { document.getElementById("handoff").value = value; });
document.getElementById("send").disabled = false;
document.getElementById("missing").hidden = true;
})();"#;

pub(crate) fn login_form(
    installation_id: Uuid,
    hostname: &str,
) -> Result<RegistrationResponse, RegistrationHttpError> {
    if hostname.is_empty()
        || !hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(TemporarilyUnavailable);
    }
    let mut response = html_response(format!(
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Register 999K</title>
<h1>Register 999K</h1>
<p>Proposed hostname: <strong>{hostname}</strong></p>
<p id="missing">Open the installation link from 999K on your Mac.</p>
<form id="request" method="post" action="/register/{installation_id}">
<input id="handoff" name="handoff" type="hidden" value="">
<label for="email">Email address</label>
<input id="email" name="email" type="email" autocomplete="email" maxlength="254" required>
<button id="send" type="submit" disabled>Send email code</button>
</form>
<script>{HANDOFF_SCRIPT}</script>
</html>"#
    ));
    let hash = STANDARD.encode(Sha256::digest(HANDOFF_SCRIPT.as_bytes()));
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(
        &format!("default-src 'none'; script-src 'sha256-{hash}'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'")
    ).map_err(|_| TemporarilyUnavailable)?);
    // Same-origin form navigation must send its Origin for the CSRF check.
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    Ok(response)
}

pub(crate) fn otp_form(installation_id: Uuid, attempt: Uuid) -> RegistrationResponse {
    let mut response = html_response(format!(
        r#"<!doctype html><html lang="en"><meta charset="utf-8">
<title>Verify 999K registration</title><h1>Verify 999K registration</h1>
<p>Check your inbox for a code. If it does not arrive, try the registration link again after the current attempt expires.</p>
<form method="post" action="/register/{installation_id}/verify">
<label for="otp">Email code</label>
<input id="otp" name="otp" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]{{6,8}}" maxlength="8" required>
<button type="submit">Verify email code</button></form></html>"#
    ));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    set_attempt_cookie(&mut response, &attempt.to_string(), 300);
    response
}

pub(crate) fn approval_result() -> RegistrationResponse {
    let mut response = html_response(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
         <title>999K registration approved</title><h1>Registration approved</h1>\
         <p>Return to 999K on your Mac to finish setup.</p></html>"
            .to_owned(),
    );
    set_attempt_cookie(&mut response, "", 0);
    response
}

fn set_attempt_cookie(response: &mut RegistrationResponse, value: &str, ttl_seconds: u64) {
    let mut cookie = HeaderValue::from_str(&format!(
        "{ATTEMPT_COOKIE}={value}; Path=/; Max-Age={ttl_seconds}; Secure; HttpOnly; SameSite=Strict"
    ))
    .expect("validated registration cookie");
    cookie.set_sensitive(true);
    response.headers_mut().insert(header::SET_COOKIE, cookie);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "11111111-1111-4111-8111-111111111111";
    const REQUEST_ID: &str = "22222222-2222-4222-8222-222222222222";
    const PROOF: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    const USER_CODE: &str = "FR6K3P8X";

    fn installer_request(
        method: Method,
        path: &str,
        proof: &str,
        body: impl Into<Bytes>,
    ) -> Request<Full<Bytes>> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {proof}"))
            .body(Full::new(body.into()))
            .unwrap()
    }

    #[tokio::test]
    async fn accepts_only_opaque_unix_v1_nonce_on_separate_finish_route() {
        let path = format!("/v1/enrollments/{ID}/finish-unix");
        let good = r#"{"kind":"unix_v1","nonce":"0123456789abcdef0123456789abcdef"}"#;
        assert!(matches!(
            parse_request(installer_request(Method::POST, &path, PROOF, good)).await,
            Ok(RequestKind::FinishUnix { installation_id, proof, origin })
                if installation_id.to_string() == ID && proof == PROOF
                    && origin.socket_path() == "/private/tmp/pmq-0123456789abcdef0123456789abcdef/hub.sock"
        ));
        for bad in [
            r#"{"kind":"unix_v1","nonce":"/private/tmp/other"}"#,
            r#"{"kind":"unix_v1","nonce":"0123456789ABCDEF0123456789ABCDEF"}"#,
            r#"{"kind":"tcp","nonce":"0123456789abcdef0123456789abcdef"}"#,
            r#"{"kind":"unix_v1","nonce":"0123456789abcdef0123456789abcdef","path":"/Users/somebody"}"#,
        ] {
            assert!(matches!(
                parse_request(installer_request(Method::POST, &path, PROOF, bad)).await,
                Err(InvalidRequest)
            ));
        }
        assert!(matches!(
            parse_request(installer_request(Method::POST, &path, "wrong", good)).await,
            Err(RegistrationNotFound)
        ));
    }

    #[tokio::test]
    async fn parses_installer_contract_and_redacts_invalid_proof() {
        let id = Uuid::parse_str(ID).unwrap();
        let begin = json!({"request_id": REQUEST_ID, "user_code": USER_CODE}).to_string();
        let request = installer_request(Method::POST, "/v1/enrollments", PROOF, begin);
        assert!(matches!(
            parse_request(request).await,
            Ok(RequestKind::Begin { request_id, user_code, proof })
                if request_id.to_string() == REQUEST_ID && user_code == USER_CODE && proof == PROOF
        ));

        let status_path = format!("/v1/enrollments/{ID}");
        let request = installer_request(Method::GET, &status_path, PROOF, "");
        assert!(matches!(
            parse_request(request).await,
            Ok(RequestKind::Status { installation_id, proof })
                if installation_id == id && proof == PROOF
        ));

        let finish_path = format!("{status_path}/finish");
        let request = installer_request(Method::POST, &finish_path, PROOF, "");
        assert!(matches!(
            parse_request(request).await,
            Ok(RequestKind::Finish { installation_id, proof })
                if installation_id == id && proof == PROOF
        ));

        let bad_proof = "synthetic-invalid-proof";
        let request = installer_request(Method::POST, &finish_path, bad_proof, "");
        let Err(error) = parse_request(request).await else {
            panic!("accepted invalid proof");
        };
        let response = error.response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes, r#"{"error":"registration_not_found"}"#);
    }

    #[tokio::test]
    async fn rejects_unexpected_body_query_and_oversized_input() {
        let finish_path = format!("/v1/enrollments/{ID}/finish");
        let invalid_requests = [
            installer_request(Method::POST, &finish_path, PROOF, "{}"),
            installer_request(Method::POST, "/v1/enrollments?proof=ignored", PROOF, ""),
            installer_request(
                Method::POST,
                "/v1/enrollments",
                PROOF,
                "x".repeat(BODY_LIMIT + 1),
            ),
            installer_request(
                Method::POST,
                "/v1/enrollments",
                PROOF,
                json!({"request_id": REQUEST_ID, "user_code": "bad-code"}).to_string(),
            ),
        ];
        for request in invalid_requests {
            assert!(matches!(parse_request(request).await, Err(InvalidRequest)));
        }
    }

    #[tokio::test]
    async fn validates_browser_input_and_secures_callback_cookie() {
        let id = Uuid::parse_str(ID).unwrap();
        let hostname = "u-fixture.example.com";
        let request = Request::builder()
            .uri(format!("/register/{ID}"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(matches!(
            parse_request(request).await,
            Ok(RequestKind::RegisterPage { installation_id }) if installation_id == id
        ));
        let page = login_form(id, hostname).unwrap();
        assert_eq!(page.headers()[header::REFERRER_POLICY], "same-origin");
        let html = page.into_body().collect().await.unwrap().to_bytes();
        let html = std::str::from_utf8(&html).unwrap();
        assert!(html.contains(hostname) && html.contains(ID));
        assert!(!html.contains(USER_CODE));
        assert!(!html.contains("name=\"user_code\""));
        assert!(html.contains("history.replaceState"));
        assert!(login_form(id, "<script>").is_err());

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/register/{ID}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Full::new(Bytes::from(format!(
                "handoff={PROOF}&email=Pilot%40example.test"
            ))))
            .unwrap();
        assert!(matches!(
            parse_request(request).await,
            Ok(RequestKind::RegisterSubmit { installation_id, handoff, email })
                if installation_id == id && handoff == PROOF && email == "pilot@example.test"
        ));

        let verify = otp_form(id, id);
        let cookie = &verify.headers()[header::SET_COOKIE];
        assert!(cookie.is_sensitive());
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.ends_with("; Secure; HttpOnly; SameSite=Strict"));
        assert!(verify.headers().get(header::LOCATION).is_none());
        let cookie_pair = cookie.split(';').next().unwrap();
        let submit = |cookie: &str, body: &str| {
            Request::builder()
                .method(Method::POST)
                .uri(format!("/register/{ID}/verify"))
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
                .unwrap()
        };
        assert!(matches!(
            parse_request(submit(cookie_pair, "otp=123456")).await,
            Ok(RequestKind::VerifyEmail { installation_id, otp, attempt })
                if installation_id == id && otp == "123456" && attempt == id
        ));
        for body in ["otp=abcdef", "otp=123456&email=other%40example.test"] {
            assert!(matches!(
                parse_request(submit(cookie_pair, body)).await,
                Err(InvalidRequest)
            ));
        }
        assert!(matches!(
            parse_request(submit("", "otp=123456")).await,
            Err(InvalidRequest)
        ));

        let approved = approval_result();
        assert!(
            approved.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        let body = approved.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains(ID) && !body.contains("synthetic-callback"));
    }
}
