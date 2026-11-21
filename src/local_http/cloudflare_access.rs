//! Verifies the identity-based Cloudflare Access application token at the origin.
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, PublicKeyUse},
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::Deserialize;
use std::{
    io::Read,
    sync::Mutex,
    time::{Duration, Instant},
};

const JWKS_PATH: &str = "/cdn-cgi/access/certs";
const MAX_JWKS_BYTES: u64 = 64 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const JWKS_TTL: Duration = Duration::from_secs(60);
const FAILED_FETCH_TTL: Duration = Duration::from_secs(5);
const INVALID_ASSERTION: &str = "cloudflare_access_invalid_assertion";
pub(super) const KEYS_UNAVAILABLE: &str = "cloudflare_access_keys_unavailable";

pub(super) struct CloudflareAccessVerifier {
    allowed_email: String,
    validation: Validation,
    certs_url: Url,
    client: Client,
    cache: Mutex<Option<KeyCache>>,
}

struct KeyCache {
    refreshed: Instant,
    keys: Result<JwkSet, &'static str>,
}

#[derive(Deserialize)]
struct AccessIdentity {
    email: String,
    #[serde(rename = "type")]
    token_type: String,
    sub: String,
}

impl CloudflareAccessVerifier {
    pub(super) fn new(
        issuer: &str,
        audience: &str,
        allowed_email: &str,
    ) -> Result<Self, &'static str> {
        const INVALID_CONFIG: &str = "cloudflare_access_invalid_config";
        let mut certs_url = Url::parse(issuer).map_err(|_| INVALID_CONFIG)?;
        let team = certs_url
            .host_str()
            .and_then(|host| host.strip_suffix(".cloudflareaccess.com"))
            .filter(|team| !team.is_empty() && !team.contains('.'))
            .ok_or(INVALID_CONFIG)?;
        if issuer != format!("https://{team}.cloudflareaccess.com")
            || audience.len() != 64
            || !audience.bytes().all(|byte| byte.is_ascii_hexdigit())
            || allowed_email.chars().any(char::is_whitespace)
            || !allowed_email
                .split_once('@')
                .is_some_and(|(local, domain)| {
                    !local.is_empty() && !domain.is_empty() && !domain.contains('@')
                })
        {
            return Err(INVALID_CONFIG);
        }
        certs_url.set_path(JWKS_PATH);
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(Policy::none())
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|_| "cloudflare_access_http_client_unavailable")?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.reject_tokens_expiring_in_less_than = 1;
        validation.validate_nbf = true;
        validation.set_required_spec_claims(&["iss", "aud", "exp", "nbf", "sub"]);
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        Ok(Self {
            allowed_email: allowed_email.to_owned(),
            validation,
            certs_url,
            client,
            cache: Mutex::new(None),
        })
    }

    pub(super) fn verify(&self, assertions: &[Vec<u8>]) -> Result<(), &'static str> {
        let [assertion] = assertions else {
            return Err(INVALID_ASSERTION);
        };
        let header = decode_header(assertion).map_err(|_| INVALID_ASSERTION)?;
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or(INVALID_ASSERTION)?;
        if header.alg != Algorithm::RS256 {
            return Err(INVALID_ASSERTION);
        }
        let key = self.key(&kid)?;
        let identity = decode::<AccessIdentity>(assertion, &key, &self.validation)
            .map_err(|_| INVALID_ASSERTION)?
            .claims;
        if identity.token_type != "app"
            || identity.sub.is_empty()
            || identity.email != self.allowed_email
        {
            return Err(INVALID_ASSERTION);
        }
        Ok(())
    }

    fn key(&self, kid: &str) -> Result<DecodingKey, &'static str> {
        let mut cache = self.cache.lock().map_err(|_| KEYS_UNAVAILABLE)?;
        let cache_is_fresh = cache.as_ref().is_some_and(|entry| {
            let ttl = if entry.keys.is_ok() {
                JWKS_TTL
            } else {
                FAILED_FETCH_TTL
            };
            entry.refreshed.elapsed() < ttl
        });
        if !cache_is_fresh {
            let keys = self.fetch_keys();
            *cache = Some(KeyCache {
                refreshed: Instant::now(),
                keys,
            });
        }
        let key_set = cache
            .as_ref()
            .ok_or(KEYS_UNAVAILABLE)?
            .keys
            .as_ref()
            .map_err(|error| *error)?;
        let mut matches = key_set
            .keys
            .iter()
            .filter(|key| key.common.key_id.as_deref() == Some(kid));
        let key = matches.next().ok_or(INVALID_ASSERTION)?;
        if matches.next().is_some()
            || !matches!(key.algorithm, AlgorithmParameters::RSA(_))
            || key
                .common
                .key_algorithm
                .is_some_and(|alg| alg != KeyAlgorithm::RS256)
            || key
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| usage != &PublicKeyUse::Signature)
        {
            return Err(INVALID_ASSERTION);
        }
        DecodingKey::from_jwk(key).map_err(|_| INVALID_ASSERTION)
    }

    fn fetch_keys(&self) -> Result<JwkSet, &'static str> {
        let response = self
            .client
            .get(self.certs_url.clone())
            .send()
            .map_err(|_| KEYS_UNAVAILABLE)?;
        if !response.status().is_success() {
            return Err(KEYS_UNAVAILABLE);
        }
        let mut body = Vec::new();
        response
            .take(MAX_JWKS_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| KEYS_UNAVAILABLE)?;
        if body.len() as u64 > MAX_JWKS_BYTES {
            return Err(KEYS_UNAVAILABLE);
        }
        serde_json::from_slice(&body).map_err(|_| KEYS_UNAVAILABLE)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use jsonwebtoken::{EncodingKey, Header, encode, get_current_timestamp};
    use serde_json::{Value, json};

    const ISSUER: &str = "https://synthetic-team.cloudflareaccess.com";
    const AUDIENCE: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const EMAIL: &str = "dot-user@example.invalid";
    const KID: &str = "test-key";
    // Generated solely for this fixture. Never use this key for authentication.
    const TEST_PRIVATE_KEY: &str = concat!(
        "MIIEowIBAAKCAQEAyo+h5IXAlxBDOWsXxfsOjTZw6bJOz3GPYWTT6zOp9fX0eMILQ2dI2Emv+yma0x0H9ZXH3OlzmsW5",
        "tjz+ODr1grPkxURQVuyT6ssLQk5izHpuwnmeIwc8V10ATy3EZftayd6G34whdAHQQ4tUFbLBCaQ2PotnNRxFjwFSnFb3",
        "qn2TuB3vW9edrmeBZ8ZKk8zf99q5SCKcicmDS0w3VLUeh1Uzjzey0Ut/pT+cM5CJVbqPe3O4dSwVbOaJn2emoyDfIn7z",
        "mmaFbggvidNBe1EtnKArpaJYu0vlo2P9QIzk28hw9QHg2BepWykF0wjv+kKz1xavyOmE2QjrVDJ5efXfAQIDAQABAoIB",
        "AA0xOBG+TUn5XutzKFf2Tx8Oy+FIgd38TyKActXXZksU0vB3cMzZwfKGMA4rSkBUHHPX5q2iTatnk74E8GignGNADJb1",
        "aUcT/tk0kp3lpYUcNzD2w0AURU+KSM4fVLJwoZvSoJWYFoir4pC3lwkXvH9AgODnatgYaIZVB3iTqGsjx2V3YZxWcHEX",
        "GIxMM0vL2nWxQNR4yHK/0dPKNhGhnSCfsrTCrsPhtmC8sGAWMIp06bHGWGvGVJasK6jVuUQXOMWMgLPSjQ+FeR/QQqlT",
        "eUXeh51S7cu9viLN4bOeGdsJEw3ehBm1KblJx78+YnKG+s3hKEpELwxP/y9oRRJFCvECgYEA7g0SVQwU5cRxCh5B1veE",
        "9wEOHpCe3ZwRugCfioEl4TkmFv5QB2qZgpFNME29gUtn5mD5wuMNKl6lccr+ymFnu8/WCbITUvihnlCDtiuv/STwy+bT",
        "YYFfAZ9FBBbDsWepIqcrid70FWn7sjeQo+e5TC2tcvlRqdrGx7dUBdQGV9kCgYEA2dWF3N+smZA72heGqkfHPg4DSaiO",
        "85J18O4vv/vWr/vM0Xn0hKqqmINlhuqLL3eHt4B3VqcNjD7lSs97wyb1eAZ05d6X6JtN97lACpvO6ppVg2kwCFMg8Zkm",
        "TiMBW1SjXxnoRmA9atw6xkSwcJqpHP93aX+4KiJROUFn1QEzL2kCgYA12zk5iokzznSwkFXe95Laex+hpKB8jLGp/jga",
        "EsvqH47Rhum+M8zVMGBhPEVSeifghDIdeFbjLhSKIR3x1YWthQDsoqAilmOiOJ8D2PBw+imMSAWsuqMu78Sify/48/Xd",
        "BCsifxGDwr2R/WY43tUaOf9S0rs6DWgJiR12UBi7SQKBgQC/9vUislKOeZblGPppsRuaSgsHrG1dpOIe0ihuTWbia0oS",
        "CCE9kJEKGjEsdiD2UaXeRm3khQ7hWsQB61qrnfeQMFBwCntgVwyclDolnyz3fc6CIf5cN7W+czJYhWy6qZnXQyN4cXNu",
        "amm5M+sxMHp7vX3kENOg/eEKTeNMyOPskQKBgHJGc9yKdh2epNg+n2o77aZeBkz1AoVUjTjfXWXHYXTosm9H6lYF1SIQ",
        "mF5mQKaLO9iEA69DMyVg0TFIyA3JeUM87d5rDDnKdL/O/qyannRAVSk/lmMCFwsJoR4PhDfsmVeJiTDYLaqAhlg5t/yn",
        "n9zHJGV1fK/ReCWVZq213WJz",
    );
    const TEST_MODULUS: &str = "yo-h5IXAlxBDOWsXxfsOjTZw6bJOz3GPYWTT6zOp9fX0eMILQ2dI2Emv-yma0x0H9ZXH3OlzmsW5tjz-ODr1grPkxURQVuyT6ssLQk5izHpuwnmeIwc8V10ATy3EZftayd6G34whdAHQQ4tUFbLBCaQ2PotnNRxFjwFSnFb3qn2TuB3vW9edrmeBZ8ZKk8zf99q5SCKcicmDS0w3VLUeh1Uzjzey0Ut_pT-cM5CJVbqPe3O4dSwVbOaJn2emoyDfIn7zmmaFbggvidNBe1EtnKArpaJYu0vlo2P9QIzk28hw9QHg2BepWykF0wjv-kKz1xavyOmE2QjrVDJ5efXfAQ";

    fn fixture_key() -> JwkSet {
        serde_json::from_value(json!({"keys": [{
            "kty": "RSA", "alg": "RS256", "use": "sig", "kid": KID,
            "n": TEST_MODULUS, "e": "AQAB"
        }]}))
        .unwrap()
    }

    fn assertion(kid: &str, claims: &Value) -> Vec<u8> {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        let key = EncodingKey::from_rsa_der(&STANDARD.decode(TEST_PRIVATE_KEY).unwrap());
        encode(&header, claims, &key).unwrap().into_bytes()
    }

    pub(in crate::local_http) fn verifier_with_claims() -> (CloudflareAccessVerifier, Value) {
        let verifier = CloudflareAccessVerifier::new(ISSUER, AUDIENCE, EMAIL).unwrap();
        *verifier.cache.lock().unwrap() = Some(KeyCache {
            refreshed: Instant::now(),
            keys: Ok(fixture_key()),
        });
        let now = get_current_timestamp();
        let valid = json!({
            "iss": ISSUER, "aud": [AUDIENCE], "exp": now + 60, "nbf": now - 60,
            "type": "app", "sub": "subject-fixture", "email": EMAIL
        });
        (verifier, valid)
    }

    pub(in crate::local_http) fn signed(claims: &Value) -> Vec<u8> {
        assertion(KID, claims)
    }

    #[test]
    fn verifies_only_the_configured_access_app_identity() {
        let (verifier, valid) = verifier_with_claims();
        let now = get_current_timestamp();
        let signed = assertion(KID, &valid);
        assert_eq!(verifier.verify(std::slice::from_ref(&signed)), Ok(()));
        assert!(verifier.verify(&[]).is_err());
        assert!(verifier.verify(&[Vec::new()]).is_err());
        assert!(verifier.verify(&[signed.clone(), signed.clone()]).is_err());
        assert!(verifier.verify(&[assertion("unknown", &valid)]).is_err());
        let mut tampered = signed;
        let signature_start = tampered.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        tampered[signature_start] = if tampered[signature_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(verifier.verify(&[tampered]).is_err());
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(KID.into());
        let hmac = encode(
            &header,
            &valid,
            &EncodingKey::from_secret(b"synthetic-fixture").unwrap(),
        )
        .unwrap();
        assert!(verifier.verify(&[hmac.into_bytes()]).is_err());
        for (field, wrong) in [
            ("iss", json!("https://other-team.cloudflareaccess.com")),
            ("aud", json!(["another-app"])),
            ("exp", json!(now)),
            ("nbf", json!(now + 60)),
            ("nbf", json!("future")),
            ("email", json!("other@example.invalid")),
            ("type", json!("org")),
            ("sub", json!("")),
        ] {
            let mut claims = valid.clone();
            claims[field] = wrong;
            assert!(
                verifier.verify(&[assertion(KID, &claims)]).is_err(),
                "{field}"
            );
        }
        let mut no_nbf = valid;
        no_nbf.as_object_mut().unwrap().remove("nbf");
        assert!(verifier.verify(&[assertion(KID, &no_nbf)]).is_err());
    }

    #[test]
    fn restricts_cert_fetch_to_the_configured_https_team() {
        let verifier = CloudflareAccessVerifier::new(ISSUER, AUDIENCE, EMAIL).unwrap();
        assert_eq!(verifier.certs_url.as_str(), format!("{ISSUER}{JWKS_PATH}"));
        for issuer in [
            "http://synthetic-team.cloudflareaccess.com",
            "https://synthetic-team.cloudflareaccess.com:443",
            "https://synthetic-team.cloudflareaccess.com/path",
            "https://synthetic-team.cloudflareaccess.com@attacker.invalid",
            "https://attacker.invalid/?team=synthetic-team.cloudflareaccess.com",
        ] {
            assert!(CloudflareAccessVerifier::new(issuer, AUDIENCE, EMAIL).is_err());
        }
    }
}
