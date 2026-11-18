//! Standalone Dot bearer, registration, and optional authenticated Events policy.
use crate::{dot_mcp_host::Fault, private_file::open_private_file};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;

pub struct DotPolicy {
    principal: String,
    digest: String,
    peers: Vec<String>,
    events: Option<DotEvents>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DotEvents {
    pub callback_hosts: Vec<String>,
    pub resolver: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    principal: String,
    mailbox: String,
    token_sha256: String,
    enabled: bool,
    registration: PathBuf,
    #[serde(default)]
    events: Option<DotEvents>,
}

impl DotPolicy {
    pub fn load(path: &Path) -> Result<Self, Fault> {
        let unavailable = || Fault::new(-32003, "policy_unavailable");
        let mut bytes = Vec::new();
        open_private_file(path)
            .map_err(|_| unavailable())?
            .take(16385)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > 16384 || bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            return Err(Fault::new(-32602, "invalid_policy"));
        }
        let p: Policy =
            serde_json::from_slice(&bytes).map_err(|_| Fault::new(-32602, "invalid_policy"))?;
        if !p.enabled || p.mailbox != "dot" {
            return Err(Fault::new(-32003, "policy_disabled"));
        }
        if p.principal.trim().is_empty()
            || p.principal.len() > 128
            || p.principal.chars().any(char::is_control)
            || p.token_sha256.len() != 64
            || !p
                .token_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Fault::new(-32602, "invalid_policy"));
        }
        let peers = crate::registration::active_peers(&p.registration, "dot")
            .map_err(|(_, reason)| Fault::new(-32003, reason))?;
        Ok(Self {
            principal: p.principal,
            digest: p.token_sha256,
            peers,
            events: p.events,
        })
    }

    pub fn events(
        &self,
    ) -> Result<Option<(DotEvents, crate::events::policy::EventsPolicy)>, Fault> {
        let Some(settings) = &self.events else {
            return Ok(None);
        };
        if !settings.resolver.is_absolute() {
            return Err(Fault::new(-32003, "invalid_events_resolver"));
        }
        let policy = crate::events::policy::EventsPolicy::for_dot_replies(
            self.principal.clone(),
            self.digest.clone(),
            self.peers.clone(),
            settings.callback_hosts.clone(),
        )
        .map_err(|e| Fault::new(e.code, e.reason))?;
        Ok(Some((settings.clone(), policy)))
    }
    pub fn principal(&self) -> &str {
        &self.principal
    }
    pub fn allowed_senders(&self) -> &[String] {
        &self.peers
    }
    pub fn authorize(&self, token: &str) -> Result<&str, Fault> {
        if token.is_empty()
            || token.len() > 256
            || !token.bytes().all(|c| (32..=126).contains(&c))
            || Sha256::digest(token)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                .as_bytes()
                .ct_eq(self.digest.as_bytes())
                .unwrap_u8()
                != 1
        {
            return Err(Fault::new(-32001, "unauthorized"));
        }
        Ok(&self.principal)
    }
}
