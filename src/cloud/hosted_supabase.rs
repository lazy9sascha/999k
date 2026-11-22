//! Supabase email OTP for the operator registration service.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

const RESPONSE_LIMIT: usize = 256 * 1024;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum SupabaseAuthError {
    #[error("invalid Supabase Auth settings")]
    Settings,
    #[error("invalid email OTP")]
    InvalidInput,
    #[error("Supabase Auth rejected sign-in")]
    Rejected,
    #[error("Supabase Auth unavailable")]
    Unavailable,
}

pub(crate) struct SupabaseAuth {
    client: Client,
    project: Url,
}

#[derive(Deserialize)]
struct Access {
    access_token: String,
}

#[derive(Deserialize)]
struct User {
    id: String,
    role: String,
    is_anonymous: bool,
}

impl SupabaseAuth {
    pub(crate) fn new(project_url: &str, publishable_key: &str) -> Result<Self, SupabaseAuthError> {
        let project = https_url(project_url)?;
        let key = publishable_key
            .strip_prefix("sb_publishable_")
            .filter(|key| !key.is_empty() && unreserved(key))
            .ok_or(SupabaseAuthError::Settings)?;
        if project.path() != "/" || key.len() > 256 {
            return Err(SupabaseAuthError::Settings);
        }
        let mut headers = HeaderMap::new();
        let mut api_key =
            HeaderValue::from_str(publishable_key).map_err(|_| SupabaseAuthError::Settings)?;
        api_key.set_sensitive(true);
        headers.insert("apikey", api_key);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .default_headers(headers)
            .build()
            .map_err(|_| SupabaseAuthError::Settings)?;
        Ok(Self { client, project })
    }

    #[cfg(test)]
    pub(crate) fn use_loopback_fixture(&mut self, port: u16) {
        self.project = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    }

    pub(crate) async fn request_otp(&self, email: &str) -> Result<(), SupabaseAuthError> {
        let request = self
            .client
            .post(self.endpoint("auth/v1/otp")?)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::json!({"email": email, "create_user": false}).to_string());
        let _: serde_json::Value = json_response(request).await?;
        Ok(())
    }

    pub(crate) async fn verify_otp(
        &self,
        email: &str,
        otp: &str,
    ) -> Result<Uuid, SupabaseAuthError> {
        if !valid_otp(otp) {
            return Err(SupabaseAuthError::InvalidInput);
        }
        let exchange = self
            .client
            .post(self.endpoint("auth/v1/verify")?)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::json!({"email": email, "token": otp, "type": "email"}).to_string());
        let access: Access = json_response(exchange).await?;

        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", access.access_token))
            .map_err(|_| SupabaseAuthError::Unavailable)?;
        bearer.set_sensitive(true);
        let verify = self
            .client
            .get(self.endpoint("auth/v1/user")?)
            .header(AUTHORIZATION, bearer);
        let user: User = json_response(verify).await?;
        if user.is_anonymous || user.role != "authenticated" {
            return Err(SupabaseAuthError::Rejected);
        }
        user.id
            .parse::<Uuid>()
            .ok()
            .filter(|id| !id.is_nil())
            .ok_or(SupabaseAuthError::Unavailable)
    }

    fn endpoint(&self, path: &str) -> Result<Url, SupabaseAuthError> {
        self.project
            .join(path)
            .map_err(|_| SupabaseAuthError::Settings)
    }
}

fn https_url(value: &str) -> Result<Url, SupabaseAuthError> {
    let url = Url::parse(value).map_err(|_| SupabaseAuthError::Settings)?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SupabaseAuthError::Settings);
    }
    Ok(url)
}

fn unreserved(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

pub(crate) fn valid_otp(value: &str) -> bool {
    (6..=8).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn normalized_email(value: &str) -> Option<String> {
    let value = value.trim();
    let (local, domain) = value.split_once('@')?;
    if value.len() > 254
        || local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&*+-/=?^_`{|}~".contains(&b))
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

async fn json_response<T: DeserializeOwned>(
    request: RequestBuilder,
) -> Result<T, SupabaseAuthError> {
    let mut response = request
        .send()
        .await
        .map_err(|_| SupabaseAuthError::Unavailable)?;
    match response.status().as_u16() {
        200 => {}
        400 | 401 | 403 | 422 => return Err(SupabaseAuthError::Rejected),
        _ => return Err(SupabaseAuthError::Unavailable),
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| SupabaseAuthError::Unavailable)?
    {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err(SupabaseAuthError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| SupabaseAuthError::Unavailable)
}

#[cfg(test)]
#[path = "hosted_supabase_tests.rs"]
mod tests;
