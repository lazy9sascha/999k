//! Launch one cloudflared connector with a private token file.
#![allow(dead_code)]
use std::path::Path;
use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Resolve and validate a compatible cloudflared executable.
pub fn preflight(executable: Option<&Path>) -> Result<PathBuf, String> {
    let path = match executable {
        Some(p) => {
            if !p.is_absolute() {
                return Err("absolute cloudflared path required".into());
            }
            p.to_path_buf()
        }
        None => {
            // Search PATH.
            let path_var = env::var("PATH").map_err(|_| "cloudflared_unavailable".to_string())?;
            let mut found = None;
            for dir in path_var.split(':') {
                let candidate = Path::new(dir).join("cloudflared");
                if candidate.is_file() {
                    found = Some(candidate);
                    break;
                }
            }
            found.ok_or_else(|| "cloudflared_unavailable".to_string())?
        }
    };
    if !path.is_file() {
        return Err("cloudflared_unavailable".to_string());
    }
    // Verify it runs.
    let output = Command::new(&path)
        .arg("--version")
        .output()
        .map_err(|_| "cloudflared_unavailable".to_string())?;
    if !output.status.success() {
        return Err("cloudflared_incompatible".to_string());
    }
    Ok(path)
}

/// Save the hosted connection metadata in an owner-private JSON file.
pub fn save_connection(home: &Path, origin: &str, registration: &str) -> Result<(), String> {
    let path = home.join("hosted-connection.json");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "origin": origin,
        "registration": registration,
    }))
    .map_err(|_| "hosted_config_invalid".to_string())?;
    pigeonmq::private_file::replace_private(&path, &bytes)
        .map_err(|_| "hosted_config_write_failed".to_string())
}

/// Save the connector token in a separate owner-only file.
pub fn save_token(home: &Path, token: &str) -> Result<PathBuf, String> {
    let path = home.join("hosted-connector.token");
    pigeonmq::private_file::replace_private(&path, token.as_bytes())
        .map_err(|_| "hosted_config_write_failed".to_string())?;
    Ok(path)
}

/// Load the hosted connection metadata, if present.
pub fn load_connection(home: &Path) -> Result<Option<(String, String)>, String> {
    let path = home.join("hosted-connection.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|_| "hosted_config_unreadable".to_string())?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "hosted_config_invalid".to_string())?;
    let origin = value["origin"].as_str().ok_or("hosted_config_invalid")?.to_string();
    let registration = value["registration"]
        .as_str()
        .ok_or("hosted_config_invalid")?
        .to_string();
    Ok(Some((origin, registration)))
}

/// Wire a ready registration into local config and launch the connector.
pub fn finalize_hosted(
    home: &Path,
    public_url: &str,
    installation_id: &str,
    cloudflared: &Path,
    token: &str,
) -> Result<PathBuf, String> {
    save_connection(home, public_url, installation_id)?;
    let token_path = save_token(home, token)?;
    let _child = launch(cloudflared, &token_path)?;
    Ok(token_path)
}
pub fn launch(executable: &Path, token_file: &Path) -> Result<Child, String> {
    if !executable.is_absolute() {
        return Err("absolute cloudflared path required".into());
    }
    if !token_file.is_absolute() {
        return Err("absolute token file path required".into());
    }
    Command::new(executable)
        .arg("tunnel")
        .arg("run")
        .arg("--token-file")
        .arg(token_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cloudflared_unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn records_expected_command_shape() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("cloudflared");
        let token = dir.path().join("token");
        fs::write(&token, "secret").unwrap();
        // Write a fake executable that records argv to a file.
        let recorder = dir.path().join("argv.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nsleep 1\n",
            recorder.display()
        );
        fs::write(&fake, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let mut child = launch(&fake, &token).unwrap();
        // Give the fake time to write argv.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(recorder.exists(), "recorder not created");
        let recorded = fs::read_to_string(&recorder).unwrap();
        assert!(recorded.contains("tunnel"));
        assert!(recorded.contains("run"));
        assert!(recorded.contains("--token-file"));
        assert!(recorded.contains(token.to_str().unwrap()));
        // Secret must not appear in argv.
        assert!(!recorded.contains("secret"));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn saves_and_loads_private_connection() {
        let dir = tempdir().unwrap();
        save_connection(dir.path(), "https://example.com", "reg-123").unwrap();
        let token_path = save_token(dir.path(), "secret-token").unwrap();
        assert_eq!(token_path.file_name().unwrap(), "hosted-connector.token");
        // file modes
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("hosted-connection.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let loaded = load_connection(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.0, "https://example.com");
        assert_eq!(loaded.1, "reg-123");
        // token not in connection file
        let conn = std::fs::read_to_string(dir.path().join("hosted-connection.json")).unwrap();
        assert!(!conn.contains("secret-token"));
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempdir().unwrap();
        assert!(load_connection(dir.path()).unwrap().is_none());
    }

    #[test]
    fn preflight_finds_fake_executable() {
        let dir = tempdir().unwrap();
        let fake = dir.path().join("cloudflared");
        let script = "#!/bin/sh\necho 'cloudflared version 2026.10.7'\nexit 0\n";
        std::fs::write(&fake, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = preflight(Some(&fake)).unwrap();
        assert_eq!(resolved, fake);
    }

    #[test]
    fn preflight_rejects_missing() {
        let dir = tempdir().unwrap();
        let result = preflight(Some(&dir.path().join("nope")));
        assert!(result.is_err());
    }

    #[test]
    fn missing_executable_errors() {
        let dir = tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "secret").unwrap();
        let result = launch(&dir.path().join("nope"), &token);
        assert!(result.is_err());
    }

    #[test]
    fn preflight_rejects_non_absolute() {
        let result = preflight(Some(Path::new("cloudflared")));
        assert!(result.is_err());
    }
}