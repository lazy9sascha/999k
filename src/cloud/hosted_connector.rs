//! Launch one cloudflared connector with a private token file.
#![allow(dead_code)]
use base64::Engine;
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
/// Report hosted connection health without claiming unverified public access.
pub fn hosted_health(home: &Path) -> serde_json::Value {
    let conn = match load_connection(home) {
        Ok(Some(c)) => c,
        _ => return serde_json::json!({ "hosted": "not_configured" }),
    };
    let token_path = home.join("hosted-connector.token");
    let token_present = token_path.exists();
    let public_reachable = probe_public(&conn.0);
    serde_json::json!({
        "hosted": "configured",
        "public_url": conn.0,
        "registration": conn.1,
        "token_file": token_path,
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
/// Run the hosted registration flow using a mock/real operator base URL.
pub fn hosted_flow(home: &Path, base_url: &str, cloudflared: &Path) -> Result<String, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut proof_bytes = [0u8; 32];
    let u1 = uuid::Uuid::new_v4();
    let u2 = uuid::Uuid::new_v4();
    proof_bytes[..16].copy_from_slice(u1.as_bytes());
    proof_bytes[16..].copy_from_slice(u2.as_bytes());
    let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(proof_bytes);
    let user_code = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_uppercase();
    hosted_resume(home, base_url, cloudflared, &request_id, &user_code, &proof)
}

/// Resume a hosted registration using an existing request ID and proof.
pub fn hosted_resume(
    home: &Path,
    base_url: &str,
    cloudflared: &Path,
    _request_id: &str,
    _user_code: &str,
    proof: &str,
) -> Result<String, String> {
    let exe = preflight(Some(cloudflared))?;
    // Re-begin (idempotent: same request_id returns existing reservation).
    // For resume we still call begin; the operator returns the existing reservation.
    // We don't have request_id/user_code stored, so begin is called by hosted_flow only.
    // This function assumes the caller passes them; here we accept proof only for simplicity.
    // In practice, hosted_flow generates them and calls this path.
    let reg = crate::hosted_registration::begin(base_url, _request_id, _user_code, proof)?;
    let mut state = String::new();
    for _ in 0..10 {
        state = crate::hosted_registration::status(base_url, &reg.installation_id, proof)?;
        if state == "ready" || state == "approved" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if state != "ready" && state != "approved" {
        return Err("registration_not_approved".to_string());
    }
    let (public_url, token) = crate::hosted_registration::finish(base_url, &reg.installation_id, proof)?;
    finalize_hosted(home, &public_url, &reg.installation_id, &exe, &token)?;
    Ok(public_url)
}
/// Map a registration status to a local action.
pub fn registration_action(state: &str) -> &'static str {
    match state {
        "expired" => "begin_new",
        "revoked" => "close_connector",
        "pending" | "approved" | "ready" => "continue",
        _ => "unknown",
    }
}
/// Generate a launchd plist for the hosted cloudflared connector.
pub fn launchd_plist(cloudflared: &Path, token_file: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.pigeonmq.hosted-connector</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>tunnel</string>
        <string>run</string>
        <string>--token-file</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#,
        cloudflared.display(),
        token_file.display()
    )
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
        save_connection(dir.path(), &format!("http://{}", addr), "reg-1").unwrap();
        save_token(dir.path(), "tok").unwrap();
        let h = hosted_health(dir.path());
        assert_eq!(h["hosted"], "configured");
        assert_eq!(h["public_reachable"], true);
        assert_eq!(h["public_verified"], false);
    }

    #[test]
    fn hosted_flow_completes_with_mock_operator() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);
        thread::spawn(move || {
            for _ in 0..3 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first = req.lines().next().unwrap_or("");
                    let resp = if first.starts_with("POST /v1/enrollments ") {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"installation_id\":\"inst-1\",\"public_url\":\"https://u.example.com\",\"login_url\":\"https://login.example.com\",\"expires_at\":\"2030-01-01T00:15:00Z\"}"
                    } else if first.starts_with("GET /v1/enrollments/") {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"ready\",\"public_url\":\"https://u.example.com\"}"
                    } else if first.starts_with("POST /v1/enrollments/") && first.contains("/finish") {
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"ready\",\"public_url\":\"https://u.example.com\",\"connector_token\":\"tok-xyz\"}"
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"error\":\"registration_not_found\"}"
                    };
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            }
        });

        // Fake cloudflared executable.
        let dir = tempdir().unwrap();
        let fake_cf = dir.path().join("cloudflared");
        let script = "#!/bin/sh\n# token-file arg present; exit 0\nexit 0\n";
        std::fs::write(&fake_cf, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_cf, std::fs::Permissions::from_mode(0o755)).unwrap();

        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let url = hosted_flow(&home, &base, &fake_cf).unwrap();
        assert_eq!(url, "https://u.example.com");
        // Config saved.
        let conn = load_connection(&home).unwrap().unwrap();
        assert_eq!(conn.0, "https://u.example.com");
        assert!(home.join("hosted-connector.token").exists());
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
    fn registration_action_maps_states() {
        assert_eq!(registration_action("expired"), "begin_new");
        assert_eq!(registration_action("revoked"), "close_connector");
        assert_eq!(registration_action("pending"), "continue");
        assert_eq!(registration_action("ready"), "continue");
        assert_eq!(registration_action("garbage"), "unknown");
    }

    #[test]
    fn launchd_plist_passes_plutil() {
        let plist = launchd_plist(Path::new("/opt/homebrew/bin/cloudflared"), Path::new("/tmp/token"));
        let dir = tempdir().unwrap();
        let path = dir.path().join("hosted.plist");
        std::fs::write(&path, &plist).unwrap();
        // plutil may not be available in all environments; skip if missing.
        let plutil = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output();
        match plutil {
            Ok(out) => assert!(out.status.success(), "plutil failed: {}", String::from_utf8_lossy(&out.stderr)),
            Err(_) => {} // skip
        }
    }
    #[test]
    fn preflight_rejects_non_absolute() {
        let result = preflight(Some(Path::new("cloudflared")));
        assert!(result.is_err());
    }
}