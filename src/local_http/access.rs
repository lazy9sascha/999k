use super::{
    HubRoutes, RouteHandler,
    cloudflare_access::{CloudflareAccessVerifier, KEYS_UNAVAILABLE},
    transport::Fault,
};
use pigeonmq::private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    issuer: String,
    email: String,
    dot: Option<Binding>,
    codex: Option<Binding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    audience: String,
    bearer_file: PathBuf,
}

pub(super) struct MailboxAccess {
    pub(super) verifier: CloudflareAccessVerifier,
    pub(super) bearer_file: PathBuf,
}

pub(super) fn mount(db: &Path, routes: &mut HubRoutes) -> io::Result<()> {
    let home = db.parent().ok_or_else(invalid)?;
    let path = home.join("cloudflare-access.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(invalid()),
        Ok(_) => {}
    }
    let bytes = private_bytes(&path, MAX_PRIVATE_CONFIG_BYTES).map_err(|_| invalid())?;
    let settings: Settings = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if settings
        .dot
        .as_ref()
        .zip(settings.codex.as_ref())
        .is_some_and(|(dot, codex)| dot.audience == codex.audience)
    {
        return Err(invalid());
    }
    for (path, binding) in [("/dot/mcp", settings.dot), ("/codex/mcp", settings.codex)] {
        let active = routes.0.get_mut(path).filter(|route| {
            matches!(
                route.handler,
                RouteHandler::Dot(_) | RouteHandler::Mailbox(_, _)
            )
        });
        if let (Some(binding), Some(route)) = (binding, active) {
            let verifier =
                CloudflareAccessVerifier::new(&settings.issuer, &binding.audience, &settings.email)
                    .map_err(|_| invalid())?;
            route.access = Some(MailboxAccess {
                verifier,
                bearer_file: home.join(binding.bearer_file),
            });
        }
    }
    Ok(())
}

impl MailboxAccess {
    pub(super) fn authorization(&self, assertions: &[Vec<u8>]) -> Result<Vec<u8>, Fault> {
        self.verifier
            .verify(assertions)
            .map_err(|error| match error {
                KEYS_UNAVAILABLE => (503, error),
                _ => (403, error),
            })?;
        let token = private_bytes(&self.bearer_file, 258)?;
        let token = token.trim_ascii_end();
        if token.is_empty()
            || token.len() > 256
            || token.iter().any(|byte| !byte.is_ascii_graphic())
        {
            return Err((503, "auth_unavailable"));
        }
        let mut header = b"Bearer ".to_vec();
        header.extend_from_slice(token);
        Ok(header)
    }
}

fn private_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, Fault> {
    let mut bytes = Vec::new();
    open_private_file(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (503, "auth_unavailable"))?;
    if bytes.len() as u64 > limit {
        return Err((503, "auth_unavailable"));
    }
    Ok(bytes)
}

fn invalid() -> io::Error {
    io::Error::other("invalid Cloudflare Access mailbox configuration")
}
