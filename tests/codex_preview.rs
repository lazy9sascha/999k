//! Verify that a preview invocation cannot bootstrap without its private opt-in.
#[cfg(target_os = "macos")]
#[test]
fn preview_without_marker_does_not_create_state() {
    use std::{
        fs::{self, Permissions},
        os::unix::fs::PermissionsExt,
        process::Command,
    };

    let home = tempfile::tempdir().unwrap();
    let prefix = home.path().join(".pmq-codex-preview");
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_pigeonmq"))
            .env_clear()
            .env("HOME", home.path())
            .arg("codex-preview-stdio")
            .output()
            .unwrap()
    };

    let missing = invoke();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(!prefix.exists());

    fs::create_dir(&prefix).unwrap();
    fs::set_permissions(&prefix, Permissions::from_mode(0o700)).unwrap();
    let marker = prefix.join("enable");
    fs::write(&marker, b"not the preview marker\n").unwrap();
    fs::set_permissions(&marker, Permissions::from_mode(0o600)).unwrap();
    let malformed = invoke();
    assert!(!malformed.status.success());
    assert!(malformed.stdout.is_empty());
    assert_eq!(fs::read_dir(prefix).unwrap().count(), 1);
}
