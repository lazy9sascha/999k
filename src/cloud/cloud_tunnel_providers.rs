//! Registered SSH tunnel providers and their trusted hostnames.
use serde_json::Value;
use std::path::Path;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Provider {
    pub(crate) name: &'static str,
    domain_suffix: &'static str,
    destination: &'static str,
    identity_file: &'static str,
    known_hosts_file: &'static str,
}

impl Provider {
    pub(super) const TUNNL: Self = Self {
        name: "tunnl",
        domain_suffix: ".tunnl.gg",
        destination: "stable@proxy.tunnl.gg",
        identity_file: "pigeonmq-cloud-tunnel-ed25519",
        known_hosts_file: "pigeonmq-cloud-tunnel-known_hosts",
    };

    pub(crate) fn named(name: &str) -> Option<Self> {
        [Self::TUNNL]
            .into_iter()
            .find(|provider| provider.name == name)
    }

    pub(crate) fn check_local_files(self, home: &Path) -> Result<(), &'static str> {
        for file in [self.identity_file, self.known_hosts_file] {
            pigeonmq::private_file::open_private_file(&home.join(file))
                .map_err(|_| "unsafe_private_path")?;
        }
        Ok(())
    }

    pub(crate) fn stable_origin(self, value: &Value) -> Result<Option<String>, &'static str> {
        if value["event"] != "tunnel" {
            return Ok(None);
        }
        let raw = value["url"]
            .as_str()
            .ok_or("unexpected_stable_url")?
            .trim_end_matches('/');
        self.parse_origin(raw).ok_or("unexpected_stable_url")?;
        if value["stable"] != true {
            return Err("unexpected_stable_url");
        }
        Ok(Some(raw.to_owned()))
    }

    pub(super) fn parse_origin(self, raw: &str) -> Option<url::Url> {
        let url = url::Url::parse(raw).ok()?;
        (url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(self.domain_suffix))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/"
            && url.port().is_none())
        .then_some(url)
    }

    pub(crate) fn ssh_command(self, home: &Path, port: u16) -> Command {
        let mut command = Command::new("/usr/bin/ssh");
        command
            .args(["-F", "/dev/null", "-n", "-T", "-i"])
            .arg(home.join(self.identity_file));
        command.arg("-o").arg(format!(
            "UserKnownHostsFile={}",
            home.join(self.known_hosts_file).display()
        ));
        for option in [
            "GlobalKnownHostsFile=/dev/null",
            "StrictHostKeyChecking=yes",
            "HostKeyAlgorithms=ssh-ed25519",
            "IdentityAgent=none",
            "IdentitiesOnly=yes",
            "PubkeyAuthentication=yes",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "GSSAPIAuthentication=no",
            "HostbasedAuthentication=no",
            "PreferredAuthentications=publickey",
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "ConnectTimeout=15",
            "ServerAliveInterval=30",
            "ServerAliveCountMax=3",
            "LogLevel=ERROR",
        ] {
            command.args(["-o", option]);
        }
        command.args([
            "-R",
            &format!("80:127.0.0.1:{port}"),
            self.destination,
            &format!("host=127.0.0.1:{port}"),
            "output=json",
        ]);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::super::handoff_origin;
    use serde_json::{Value, json};

    #[test]
    fn handoff_requires_a_connected_registered_provider_and_its_origin() {
        let mut state = json!({"state":"local_ready","tunnel":"externally_managed","public_origin_source":"configured","public_origin":"https://fixture.tunnl.gg/","remote_verified":false});
        assert_eq!(handoff_origin(&state), None);

        state["state"] = json!("connected");
        state["tunnel"] = json!("connected");
        assert_eq!(handoff_origin(&state), None);

        state
            .as_object_mut()
            .unwrap()
            .remove("public_origin_source");
        let expected = Some("https://fixture.tunnl.gg".into());
        // Before provider identity was persisted, tunnl was the only built-in provider.
        assert_eq!(handoff_origin(&state), expected);

        for invalid in [json!("external"), json!("unregistered"), Value::Null] {
            state["tunnel_provider"] = invalid;
            assert_eq!(handoff_origin(&state), None);
        }
        state["tunnel_provider"] = json!("tunnl");
        assert_eq!(handoff_origin(&state), expected);
        state["public_origin"] = json!("https://pigeon.example/");
        assert_eq!(handoff_origin(&state), None);
    }
}
