//! Launcher parity slice: explicit execution and read-only path checks at the Rust CLI boundary.
//! No real queue, agent process, or model provider is used.
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn receiver(state: &Path, binding: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_999k-codex-receiver"));
    command
        .arg("--state")
        .arg(state)
        .arg("--hub-port")
        .arg("54743")
        .arg("--token-file")
        .arg(state.with_extension("token"))
        .arg("--binding")
        .arg(binding);
    command
}

#[test]
fn default_preflight_preserves_spaced_paths_without_queue_or_native_access() {
    // Ports test_check_does_not_write_or_exec and the spaced-path assertion from
    // tools/test_agent_launchers.py to the actual Rust executable, not the Python launcher.
    let temp = tempfile::Builder::new()
        .prefix("pmq-launch-")
        .tempdir_in("/tmp")
        .unwrap();
    let work = temp.path().join("repo with spaces");
    fs::create_dir(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let db = work.join("queue with spaces.sqlite3");
    let path = work.join("binding with spaces.json");
    let endpoint = temp.path().join("native.sock");
    let listener = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
    listener.set_nonblocking(true).unwrap();
    let binding = radio999k::adapters::codex::receiver::Binding {
        thread_id: "synthetic-owner".into(),
        endpoint,
        cwd: work.clone(),
        approved_inputs: vec!["synthetic input".into()],
        registration: work.join("not-created-registration.json"),
    };
    let bytes = serde_json::to_vec(&binding).unwrap();
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let minimal = Command::new(env!("CARGO_BIN_EXE_999k-codex-receiver"))
        .arg("--binding")
        .arg(&path)
        .output()
        .unwrap();
    for output in [minimal, receiver(&db, &path).output().unwrap()] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"state":"configuration_valid","receiver_started":false})
        );
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    // No DB, registration, lock, or reconciliation journal was created.
    assert_eq!(fs::read_dir(&work).unwrap().count(), 1);
}

#[test]
fn execution_modes_require_run_before_configuration_or_queue_access() {
    // Native equivalents of the Python launcher's explicit-run confirmation gate.
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("queue.sqlite3");
    let binding = temp.path().join("missing-binding.json");
    for flag in ["--serve", "--attach-saved"] {
        let output = receiver(&db, &binding).arg(flag).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("--run"), "{error}");
        assert!(output.stdout.is_empty());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

#[test]
fn bad_explicit_binding_is_not_replaced_with_a_fallback() {
    // Native replacement for the explicit-path-no-fallback launcher assertions.
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("queue.sqlite3");
    let binding = temp.path().join("missing-binding.json");
    let output = receiver(&db, &binding).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["reason"], "private_file_required");
    assert_eq!(value["automatic_resubmit"], false);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn only_run_requires_state_and_token_file() {
    let temp = tempfile::tempdir().unwrap();
    let binding = temp.path().join("missing-binding.json");
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_999k-codex-receiver"));
        c.arg("--binding").arg(&binding);
        c
    };
    for (present, missing) in [("--state", "--token-file"), ("--token-file", "--state")] {
        let error = command()
            .arg("--run")
            .arg(present)
            .arg(temp.path().join("unused"))
            .output()
            .unwrap();
        assert_eq!(error.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&error.stderr).contains(missing));
        assert!(error.stdout.is_empty());
    }
    let setup = command().arg("--set-unattended").output().unwrap();
    assert_eq!(setup.status.code(), Some(2));
    let outcome: serde_json::Value = serde_json::from_slice(&setup.stdout).unwrap();
    assert_eq!(outcome["reason"], "private_file_required");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
