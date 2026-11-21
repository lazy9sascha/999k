//! Launch one cloudflared connector with a private token file.
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Save the hosted connection metadata in an owner-private JSON file.
pub fn save_connection(home: &Path, origin: &str, registration: &str) -> Result<(), String> {
    let path = home.join("hosted-connection.json");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "origin": origin,
        "registration": registration,
    }))
    .map_err(|_| "hosted_config_invalid".to_string())?;
    write_private(&path, &bytes)
}

/// Save the connector token in a separate owner-only file.
pub fn save_token(home: &Path, token: &str) -> Result<PathBuf, String> {
    let path = home.join("hosted-connector.token");
    write_private(&path, token.as_bytes())?;
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

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(|_| "hosted_config_write_failed".to_string())?;
    file.write_all(bytes)
        .map_err(|_| "hosted_config_write_failed".to_string())?;
    file.sync_all()
        .map_err(|_| "hosted_config_write_failed".to_string())?;
    std::fs::rename(&tmp, path).map_err(|_| "hosted_config_write_failed".to_string())?;
    Ok(())
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
    fn missing_executable_errors() {
        let dir = tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "secret").unwrap();
        let result = launch(&dir.path().join("nope"), &token);
        assert!(result.is_err());
    }
}