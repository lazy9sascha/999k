//! Launch one cloudflared connector with a private token file.
use std::path::Path;
use std::process::{Child, Command, Stdio};

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
    fn missing_executable_errors() {
        let dir = tempdir().unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "secret").unwrap();
        let result = launch(&dir.path().join("nope"), &token);
        assert!(result.is_err());
    }
}