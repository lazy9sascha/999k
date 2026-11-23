//! Launch one cloudflared connector with a private token file.
#![allow(dead_code)]
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

pub(crate) const TOKEN_FILE: &str = "hosted-connector.token";

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
    let path = home.join(TOKEN_FILE);
    pigeonmq::private_file::replace_private(&path, token.as_bytes())
        .map_err(|_| "hosted_config_write_failed".to_string())?;
    Ok(path)
}

/// Load the hosted connection metadata, if present.
pub fn load_connection(home: &Path) -> Result<Option<(String, String)>, String> {
    let path = home.join("hosted-connection.json");
    let file = match pigeonmq::private_file::open_private_file(&path) {
        Ok(f) => f,
        Err(e) => {
            if path
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(None);
            }
            return Err(format!("open_private_failed: {} {}", e.0, e.1));
        }
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(pigeonmq::private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "hosted_config_unreadable".to_string())?;
    if bytes.len() as u64 > pigeonmq::private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err("hosted_config_too_large".to_string());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "hosted_config_invalid".to_string())?;
    let origin = value["origin"]
        .as_str()
        .ok_or("hosted_config_invalid")?
        .to_string();
    let registration = value["registration"]
        .as_str()
        .ok_or("hosted_config_invalid")?
        .to_string();
    Ok(Some((origin, registration)))
}

pub(crate) fn check_binding(home: &Path, url: &str, id: &str) -> Result<(), String> {
    if load_connection(home)?.is_some_and(|(old_url, old_id)| old_url != url || old_id != id) {
        return Err("existing hosted connection belongs to another registration".into());
    }
    Ok(())
}

pub(crate) fn stage(home: &Path, url: &str, id: &str, token: &str) -> Result<(), String> {
    check_binding(home, url, id)?;
    save_token(home, token)?;
    save_connection(home, url, id)
}

/// Report hosted connection health without claiming unverified public access.
pub fn hosted_health(home: &Path) -> serde_json::Value {
    let conn = match load_connection(home) {
        Ok(Some(c)) => c,
        _ => return serde_json::json!({ "hosted": "not_configured" }),
    };
    let token_path = home.join(TOKEN_FILE);
    let token_present = pigeonmq::private_file::open_private_file(&token_path).is_ok();
    let public_reachable = probe_public(&conn.0);
    serde_json::json!({
        "hosted": "configured",
        "public_url": conn.0,
        "registration": conn.1,
        "token_present": token_present,
        "public_reachable": public_reachable,
        "public_verified": false,
    })
}

fn probe_public(url: &str) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()
        .and_then(|c| c.get(url).send().ok())
        .map(|r| r.status().as_u16() < 500)
        .unwrap_or(false)
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn saves_and_loads_private_connection() {
        let dir = tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        save_connection(dir.path(), "https://example.com", "reg-123").unwrap();
        let token_path = save_token(dir.path(), "secret-token").unwrap();
        assert_eq!(token_path.file_name().unwrap(), "hosted-connector.token");
        // file modes
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
    fn stages_credentials_without_queue_plist_or_process() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let url = "https://u.example.test";
        let id = "11111111-1111-4111-8111-111111111111";
        stage(home, url, id, "fixture-token").unwrap();
        let files = std::fs::read_dir(home)
            .unwrap()
            .map(|file| file.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            files,
            ["hosted-connection.json".into(), TOKEN_FILE.into()].into()
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("hosted-connection.json")).unwrap())
                .unwrap();
        assert!(metadata.get("connector_token").is_none());
        assert_eq!(
            load_connection(home).unwrap(),
            Some((url.into(), id.into()))
        );
        assert_eq!(
            home.join(TOKEN_FILE)
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(stage(home, "https://other.example.test", id, "different").is_err());
        assert_eq!(
            std::fs::read(home.join(TOKEN_FILE)).unwrap(),
            b"fixture-token"
        );
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
    fn debug_open_private() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hosted-connection.json");
        pigeonmq::private_file::replace_private(&path, b"{}").unwrap();
        let parent = path.parent().unwrap();
        let pm = std::fs::metadata(parent).unwrap();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        eprintln!(
            "parent mode: {:o} uid: {}",
            pm.permissions().mode(),
            pm.uid()
        );
        let fm = std::fs::metadata(&path).unwrap();
        eprintln!(
            "file mode: {:o} uid: {} nlink: {}",
            fm.permissions().mode(),
            fm.uid(),
            fm.nlink()
        );
        let res = pigeonmq::private_file::open_private_file(&path);
        eprintln!("open result: {:?}", res.map(|_| "ok"));
    }

    #[test]
    fn hosted_health_reports_unconfigured() {
        let dir = tempdir().unwrap();
        let h = hosted_health(dir.path());
        assert_eq!(h["hosted"], "not_configured");
    }

    #[test]
    fn hosted_health_reports_configured_and_probe() {
        // Start a mock HTTP server returning 200.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                let _ = s.flush();
            }
        });
        let dir = tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        save_connection(dir.path(), &format!("http://{}", addr), "reg-1").unwrap();
        save_token(dir.path(), "tok").unwrap();
        let h = hosted_health(dir.path());
        assert_eq!(h["hosted"], "configured");
        assert_eq!(h["public_reachable"], true);
        assert_eq!(h["public_verified"], false);
    }

    #[test]
    fn preflight_rejects_non_absolute() {
        let result = preflight(Some(Path::new("cloudflared")));
        assert!(result.is_err());
    }
}
