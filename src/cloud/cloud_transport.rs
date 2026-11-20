//! Transport selection and public-origin metadata. This module never probes an origin.
use serde_json::Value;
use std::{ffi::OsStr, io::Read, path::Path};
#[path = "cloud_tunnel_providers.rs"]
mod providers;
pub(crate) use providers::Provider;

const TUNNEL_ENV: &str = "PIGEONMQ_TUNNEL";
const PUBLIC_URL_ENV: &str = "PIGEONMQ_PUBLIC_URL";
const MAX_ENV_FILE_BYTES: usize = 4096;
pub(crate) const TUNNEL_MODE_REQUIRED: &str = "tunnel_mode_required. Set PIGEONMQ_TUNNEL=external and PIGEONMQ_PUBLIC_URL for a reverse tunnel on your own domain, for example Cloudflare Tunnel. This is the recommended option. For testing without a domain, set PIGEONMQ_TUNNEL=tunnl. tunnl.gg can read your traffic. Its free URLs expire. For loopback testing only, pass --local-only.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Managed(Provider),
    LocalOnly,
    PrivateCompanions,
    External(String),
}

impl Transport {
    pub(crate) fn select(local_only: bool, env_file: Option<&Path>) -> Result<Self, &'static str> {
        if local_only {
            return Ok(Self::LocalOnly);
        }
        if let Some(path) = env_file {
            return Self::from_env_file(path);
        }
        Self::from_config(
            std::env::var_os(TUNNEL_ENV).as_deref(),
            std::env::var_os(PUBLIC_URL_ENV).as_deref(),
        )
    }

    fn from_env_file(path: &Path) -> Result<Self, &'static str> {
        let mut contents = String::new();
        pigeonmq::private_file::open_private_file(path)
            .map_err(|_| "tunnel_env_file_unavailable")?
            .take(MAX_ENV_FILE_BYTES as u64 + 1)
            .read_to_string(&mut contents)
            .map_err(|_| "tunnel_env_file_unavailable")?;
        if contents.len() > MAX_ENV_FILE_BYTES {
            return Err("invalid_tunnel_env_file");
        }
        let mut mode = None;
        let mut public_url = None;
        for line in contents.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').ok_or("invalid_tunnel_env_file")?;
            let slot = match key.trim() {
                TUNNEL_ENV => &mut mode,
                PUBLIC_URL_ENV => &mut public_url,
                _ => return Err("invalid_tunnel_env_file"),
            };
            let value = value.trim();
            if value.contains(['\'', '"', '$', '\x60']) || slot.replace(OsStr::new(value)).is_some()
            {
                return Err("invalid_tunnel_env_file");
            }
        }
        Self::from_config(
            Some(mode.ok_or("tunnel_env_file_requires_mode")?),
            public_url,
        )
    }

    fn from_config(mode: Option<&OsStr>, public_url: Option<&OsStr>) -> Result<Self, &'static str> {
        let Some(mode) = mode else {
            return match public_url {
                Some(_) => Err("public_url_requires_external_tunnel"),
                None => Err(TUNNEL_MODE_REQUIRED),
            };
        };
        let mode = mode.to_str().ok_or("invalid_tunnel_mode")?;
        if mode == "external" {
            let value = public_url.ok_or("external_tunnel_requires_public_url")?;
            return public_origin(value.to_str().ok_or("invalid_public_origin")?)
                .map(Self::External);
        }
        let provider = Provider::named(mode).ok_or("invalid_tunnel_mode")?;
        if public_url.is_some() {
            return Err("public_url_requires_external_tunnel");
        }
        Ok(Self::Managed(provider))
    }

    pub(crate) fn managed_provider(&self) -> Option<Provider> {
        match self {
            Self::Managed(provider) => Some(*provider),
            _ => None,
        }
    }

    pub(crate) fn mode(&self) -> &'static str {
        match self {
            Self::Managed(_) => "ssh",
            Self::LocalOnly => "local_only",
            Self::PrivateCompanions => "private_companions",
            Self::External(_) => "external",
        }
    }

    pub(crate) fn configured_origin(&self) -> Option<&str> {
        match self {
            Self::External(origin) => Some(origin),
            _ => None,
        }
    }
}

/// HTTPS origin, not an endpoint or request destination. Uses the default HTTPS port.
pub(crate) fn public_origin(raw: &str) -> Result<String, &'static str> {
    // Check the raw authority before URL normalization can strip a dot segment,
    // an empty userinfo section, or a backslash from malformed input.
    let (scheme, tail) = raw.split_once("://").ok_or("invalid_public_origin")?;
    let authority = tail.strip_suffix('/').unwrap_or(tail);
    if !scheme.eq_ignore_ascii_case("https")
        || authority.is_empty()
        || authority.ends_with(':')
        || authority
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || authority.contains(['/', '\\', '@', '?', '#'])
    {
        return Err("invalid_public_origin");
    }
    let url = url::Url::parse(raw).map_err(|_| "invalid_public_origin")?;
    if url.host_str().is_none() || url.port().is_some() {
        return Err("invalid_public_origin");
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) fn handoff_origin(state: &Value) -> Option<String> {
    // A user-configured origin is metadata, not authority for credential handoff.
    if state["state"] != "connected" || state["public_origin_source"] == "configured" {
        return None;
    }
    let provider = match state.get("tunnel_provider") {
        // Older connected state files could only come from tunnl.
        None => Provider::TUNNL,
        Some(name) => Provider::named(name.as_str()?)?,
    };
    let url = provider.parse_origin(state["public_origin"].as_str()?)?;
    Some(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_origin_requires_an_explicit_mode() {
        let tunnl = Some(OsStr::new("tunnl"));
        let external = Some(OsStr::new("external"));
        let url = Some(OsStr::new("https://Pigeon.Example:443/"));
        assert_eq!(
            Transport::from_config(None, None),
            Err(TUNNEL_MODE_REQUIRED)
        );
        assert_eq!(
            Transport::from_config(tunnl, None),
            Ok(Transport::Managed(Provider::TUNNL))
        );
        for mode in [None, tunnl] {
            assert_eq!(
                Transport::from_config(mode, url),
                Err("public_url_requires_external_tunnel")
            );
        }
        assert_eq!(
            Transport::from_config(external, url),
            Ok(Transport::External("https://pigeon.example".into()))
        );
        assert_eq!(
            Transport::from_config(external, None),
            Err("external_tunnel_requires_public_url")
        );
        assert_eq!(
            Transport::from_config(external, Some(OsStr::new("http://pigeon.example"))),
            Err("invalid_public_origin")
        );
        let invalid = Some(OsStr::new("invalid"));
        assert_eq!(
            Transport::from_config(invalid, None),
            Err("invalid_tunnel_mode")
        );
    }

    #[test]
    fn explicit_env_file_accepts_tunnel_settings_only() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let file = dir.path().join(".env");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        for (input, expected) in [
            (
                "# External proxy\r\n\r\nPIGEONMQ_PUBLIC_URL=https://Pigeon.Example/\r\nPIGEONMQ_TUNNEL = external\r\n",
                Ok(Transport::External("https://pigeon.example".into())),
            ),
            (
                "PIGEONMQ_TUNNEL=tunnl\n",
                Ok(Transport::Managed(Provider::TUNNL)),
            ),
            (
                "PIGEONMQ_PUBLIC_URL=https://pigeon.example\n",
                Err("tunnel_env_file_requires_mode"),
            ),
            (
                "PIGEONMQ_TUNNEL=external\n",
                Err("external_tunnel_requires_public_url"),
            ),
            (
                "PIGEONMQ_TUNNEL=external\nPIGEONMQ_PUBLIC_URL=https://example.test/path\n",
                Err("invalid_public_origin"),
            ),
            (
                "PIGEONMQ_TUNNEL=tunnl\nPIGEONMQ_TUNNEL=external\n",
                Err("invalid_tunnel_env_file"),
            ),
            (
                "PIGEONMQ_TUNNEL=tunnl\nTEST_OTHER=fixture-only\n",
                Err("invalid_tunnel_env_file"),
            ),
            (
                "export PIGEONMQ_TUNNEL=tunnl\n",
                Err("invalid_tunnel_env_file"),
            ),
            ("PIGEONMQ_TUNNEL='tunnl'\n", Err("invalid_tunnel_env_file")),
            (
                "PIGEONMQ_TUNNEL=external\nPIGEONMQ_PUBLIC_URL=https://$FIXTURE_HOST\n",
                Err("invalid_tunnel_env_file"),
            ),
        ] {
            fs::write(&file, input).unwrap();
            assert_eq!(Transport::select(false, Some(&file)), expected);
        }
        fs::write(&file, "#".repeat(MAX_ENV_FILE_BYTES + 1)).unwrap();
        assert_eq!(
            Transport::select(false, Some(&file)),
            Err("invalid_tunnel_env_file")
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            Transport::select(false, Some(&file)),
            Err("tunnel_env_file_unavailable")
        );
        assert_eq!(
            Transport::select(true, Some(&file)),
            Ok(Transport::LocalOnly)
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let symlink = dir.path().join("link.env");
        std::os::unix::fs::symlink(&file, &symlink).unwrap();
        assert_eq!(
            Transport::select(false, Some(&symlink)),
            Err("tunnel_env_file_unavailable")
        );
    }

    #[test]
    fn configured_origin_rejects_endpoint_or_credential_components() {
        for value in [
            "",
            "http://pigeon.example",
            "https://user@pigeon.example",
            "https://@pigeon.example",
            "https://pigeon.example/path",
            "https://pigeon.example/.",
            "https://pigeon.example/%2e%2e",
            "https://pigeon.example//",
            "https://pigeon.example\\",
            "https:////pigeon.example",
            "https://pigeon.example?key=x",
            "https://pigeon.example#x",
            "https://pigeon.example:444",
            " https://pigeon.example",
            "https://pigeon.example\n",
        ] {
            assert!(public_origin(value).is_err());
        }
    }
}
