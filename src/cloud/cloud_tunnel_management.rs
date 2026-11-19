use serde::Deserialize;
use std::{
    env,
    fs::OpenOptions,
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const NAME: &str = "pigeonmq-cloud-dot";
const DESCRIPTION: &str = "PigeonMQ Dot MCP";
const PENDING: &str = "dot-tunnel-create-attempted";
const PLATFORM: &str = "Check the selected organization and workspace at https://platform.openai.com/settings/organization/tunnels.";

#[derive(Deserialize)]
struct Tunnel {
    id: String,
    name: String,
    #[serde(default)]
    organization_ids: Vec<String>,
    #[serde(default)]
    workspace_ids: Vec<String>,
    #[serde(default)]
    tenant_ids: Vec<String>,
}
#[derive(Deserialize)]
struct TunnelList {
    tunnels: Vec<Tunnel>,
}

#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub organization: &'a str,
    pub workspace: &'a str,
}
impl Scope<'_> {
    pub(crate) fn validate(&self) -> io::Result<()> {
        fn valid(s: &str) -> bool {
            !s.is_empty()
                && s.len() <= 128
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        if valid(self.organization) && valid(self.workspace) {
            Ok(())
        } else {
            Err(io::Error::other(
                "Enter one organization ID and one workspace ID",
            ))
        }
    }
    fn matches(&self, t: &Tunnel) -> bool {
        t.name == NAME
            && t.organization_ids.len() == 1
            && t.organization_ids[0] == self.organization
            && t.workspace_ids.len() == 1
            && t.workspace_ids[0] == self.workspace
            && t.tenant_ids.is_empty()
            && valid_id(&t.id)
    }
}

// Documented provider formats are tunnel_<32> or tunnel_<4>_<32>.
pub(crate) fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("tunnel_") else {
        return false;
    };
    let valid = |p: &str, len: usize| {
        p.len() == len
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    match rest.split_once('_') {
        Some((namespace, value)) => valid(namespace, 4) && valid(value, 32),
        None => valid(rest, 32),
    }
}

#[expect(
    dead_code,
    reason = "future dot-tunnel-setup client lookup; authenticated workspace admission is unavailable"
)]
pub(crate) fn discover_client() -> io::Result<PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("tunnel-client"))
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Install the official tunnel-client: https://github.com/openai/tunnel-client",
            )
        })
}

fn command(client: &Path, home: &Path) -> Command {
    let mut command = Command::new(client);
    command
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .current_dir(home)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn admin_json<T: serde::de::DeserializeOwned>(
    client: &Path,
    home: &Path,
    action: &str,
    args: &[&str],
    key_name: &str,
    key: &str,
) -> io::Result<T> {
    if key.trim().is_empty() || key.bytes().any(|b| b.is_ascii_control()) {
        return Err(io::Error::other("Invalid tunnel API key"));
    }
    let output = command(client, home)
        .args(["admin", "--json", "tunnels", action])
        .args(args)
        .env(key_name, key)
        .output()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    e.kind(),
                    "Install the official tunnel-client: https://github.com/openai/tunnel-client",
                )
            } else {
                io::Error::other(format!("Could not start tunnel-client for {action}"))
            }
        })?;
    if !output.status.success() {
        let status = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|json| json["error"]["status_code"].as_u64())
            .filter(|n| (400..=599).contains(n))
            .map_or(String::new(), |n| format!(" (HTTP {n})"));
        return Err(io::Error::other(format!(
            "Tunnel {action} failed{status}. {PLATFORM}"
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| io::Error::other(format!("Tunnel {action} returned unknown data. {PLATFORM}")))
}

#[expect(
    dead_code,
    reason = "future dot-tunnel-setup exact-scope verification; authenticated workspace admission is unavailable"
)]
pub(crate) fn verify_existing(
    client: &Path,
    home: &Path,
    scope: Scope<'_>,
    id: &str,
    runtime_key: &str,
) -> io::Result<()> {
    scope.validate()?;
    if !valid_id(id) {
        return Err(io::Error::other("Invalid tunnel ID"));
    }
    let tunnel: Tunnel = admin_json(
        client,
        home,
        "get",
        &[id],
        "CONTROL_PLANE_API_KEY",
        runtime_key,
    )?;
    if tunnel.id == id && scope.matches(&tunnel) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Runtime lookup did not verify the Cloud Dot tunnel and exact scope. {PLATFORM}"
        )))
    }
}

fn select(list: TunnelList, scope: &Scope<'_>) -> io::Result<Option<String>> {
    let mut matches = list.tunnels.into_iter().filter(|t| t.name == NAME);
    match (matches.next(), matches.next()) {
        (None, None) => Ok(None),
        (Some(t), None) if scope.matches(&t) => Ok(Some(t.id)),
        _ => Err(io::Error::other(format!(
            "Cannot select one Cloud Dot tunnel in the exact scope. No tunnel created. {PLATFORM}"
        ))),
    }
}

#[expect(
    dead_code,
    reason = "dot-tunnel-setup must verify authenticated workspace admission before creating or selecting a tunnel"
)]
pub(crate) fn create_or_reuse(
    client: &Path,
    home: &Path,
    scope: Scope<'_>,
    admin_key: &str,
) -> io::Result<(String, bool)> {
    scope.validate()?;
    let list = admin_json(
        client,
        home,
        "list",
        &["--workspace-id", scope.workspace],
        "OPENAI_ADMIN_KEY",
        admin_key,
    )?;
    if let Some(id) = select(list, &scope)? {
        return Ok((id, false));
    }
    let marker = OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create_new(true)
        .open(home.join(PENDING))
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                io::Error::other(format!(
                    "A prior creation attempt is recorded. No second tunnel was created. {PLATFORM}"
                ))
            } else {
                e
            }
        })?;
    marker.sync_all()?;
    std::fs::File::open(home)?.sync_all()?;
    let tunnel: Tunnel = admin_json(
        client,
        home,
        "create",
        &[
            "--name",
            NAME,
            "--description",
            DESCRIPTION,
            "--organization-id",
            scope.organization,
            "--workspace-id",
            scope.workspace,
        ],
        "OPENAI_ADMIN_KEY",
        admin_key,
    )
    .map_err(|e| {
        io::Error::other(format!(
            "Creation result not verified; no automatic retry. {e}"
        ))
    })?;
    if scope.matches(&tunnel) {
        Ok((tunnel.id, true))
    } else {
        Err(io::Error::other(format!(
            "Creation response did not verify the tunnel and exact scope. No automatic retry. {PLATFORM}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inconclusive_or_ambiguous_listing_cannot_allow_create() {
        let scope = Scope {
            organization: "org_cloud",
            workspace: "ws_cloud",
        };
        assert_eq!(
            select(serde_json::from_str(r#"{"tunnels":[]}"#).unwrap(), &scope).unwrap(),
            None
        );
        assert!(serde_json::from_str::<TunnelList>(r#"{"tunnels":null}"#).is_err());
        let one = r#"{"id":"tunnel_0123456789abcdef0123456789abcdef","name":"pigeonmq-cloud-dot","organization_ids":["org_cloud"],"workspace_ids":["ws_cloud"]}"#;
        let broad = one.replace("ws_cloud", "ws_cloud\",\"ws_other");
        for entries in [broad, format!("{one},{one}")] {
            assert!(
                select(
                    serde_json::from_str(&format!(r#"{{"tunnels":[{entries}]}}"#)).unwrap(),
                    &scope
                )
                .is_err()
            );
        }
        assert!(
            select(
                serde_json::from_str(&format!(r#"{{"tunnels":[{one}]}}"#)).unwrap(),
                &scope
            )
            .unwrap()
            .is_some()
        );
    }
}
