//! Supabase browser sign-in for the operator registration service.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const RESPONSE_LIMIT: usize = 256 * 1024;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum SupabaseAuthError {
    #[error("invalid Supabase Auth settings")]
    Settings,
    #[error("invalid sign-in callback")]
    InvalidInput,
    #[error("Supabase Auth rejected sign-in")]
    Rejected,
    #[error("Supabase Auth unavailable")]
    Unavailable,
}

pub(crate) struct SupabaseAuth {
    client: Client,
    project: Url,
    provider: String,
    callback: Url,
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
    pub(crate) fn new(
        project_url: &str,
        publishable_key: &str,
        provider: &str,
        callback_url: &str,
    ) -> Result<Self, SupabaseAuthError> {
        let project = https_url(project_url)?;
        let callback = https_url(callback_url)?;
        let key = publishable_key
            .strip_prefix("sb_publishable_")
            .filter(|key| !key.is_empty() && unreserved(key))
            .ok_or(SupabaseAuthError::Settings)?;
        if project.path() != "/"
            || !(1..=64).contains(&provider.len())
            || !provider.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_:-".contains(&byte)
            })
            || key.len() > 256
        {
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
        Ok(Self {
            client,
            project,
            provider: provider.to_owned(),
            callback,
        })
    }

    pub(crate) fn authorize_url(&self, code_verifier: &str) -> Result<Url, SupabaseAuthError> {
        validate_verifier(code_verifier)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        let mut url = self.endpoint("auth/v1/authorize")?;
        url.query_pairs_mut().extend_pairs([
            ("provider", self.provider.as_str()),
            ("redirect_to", self.callback.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "s256"),
        ]);
        Ok(url)
    }

    /// Call only after consuming the matching browser attempt and its stored verifier.
    pub(crate) async fn exchange_user(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<Uuid, SupabaseAuthError> {
        validate_verifier(code_verifier)?;
        if !(1..=512).contains(&code.len()) || !unreserved(code) {
            return Err(SupabaseAuthError::InvalidInput);
        }

        let exchange = self
            .client
            .post(self.endpoint("auth/v1/token?grant_type=pkce")?)
            .header(CONTENT_TYPE, "application/json")
            .body(
                serde_json::json!({ "auth_code": code, "code_verifier": code_verifier })
                    .to_string(),
            );
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

fn validate_verifier(value: &str) -> Result<(), SupabaseAuthError> {
    if !(43..=128).contains(&value.len()) || !unreserved(value) {
        return Err(SupabaseAuthError::InvalidInput);
    }
    Ok(())
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
