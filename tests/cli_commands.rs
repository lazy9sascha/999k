use std::process::Command;

#[test]
fn disabled_message_commands_do_not_open_a_queue() {
    let dir = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_pigeonmq");
    for name in ["send", "reply"] {
        let result = Command::new(binary)
            .current_dir(dir.path())
            .args([name, "--help"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(error.contains("unrecognized subcommand"), "{error}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn help_keeps_install_and_operations_commands() {
    let dir = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_pigeonmq");
    let result = Command::new(binary)
        .current_dir(dir.path())
        .arg("--help")
        .output()
        .unwrap();
    assert!(result.status.success());
    let help = String::from_utf8(result.stdout).unwrap();
    assert!(
        !help
            .lines()
            .any(|line| matches!(line.split_whitespace().next(), Some("send" | "reply")))
    );
    for name in [
        "init",
        "discover",
        "setup-muse",
        "run",
        "serve-hub",
        "dot-tunnel-status",
    ] {
        assert!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some(name))
        );
        assert!(
            Command::new(binary)
                .current_dir(dir.path())
                .args([name, "--help"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn run_loads_only_the_explicit_tunnel_env_file() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join(".env");
    fs::write(&path, "PIGEONMQ_TUNNEL=bad\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let missing_home = dir.path().join("missing-home");
    let run = |args: &[&str], process_mode: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        command.env_clear().env("PIGEONMQ_TUNNEL", process_mode);
        if process_mode == "external" {
            command.env("PIGEONMQ_PUBLIC_URL", "https://process.example");
        }
        let output = command
            .current_dir(dir.path())
            .args(["run", "--home"])
            .arg(&missing_home)
            .args(args)
            .output()
            .unwrap();
        let diagnostic = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
        assert_eq!(output.status.code(), Some(1), "{diagnostic}");
        diagnostic
    };
    let home_error = "cloud_runner_owned_or_home_unavailable";
    let unset = |extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_pigeonmq"))
            .env_clear()
            .env_remove("PIGEONMQ_TUNNEL")
            .env_remove("PIGEONMQ_PUBLIC_URL")
            .current_dir(dir.path())
            .args(["run", "--home"])
            .arg(&missing_home)
            .args(extra)
            .output()
            .unwrap();
        let text = String::from_utf8([output.stdout, output.stderr].concat()).unwrap();
        assert_eq!(output.status.code(), Some(1), "{text}");
        text
    };
    let default_text = unset(&[]);
    assert!(
        default_text.contains("tunnel_mode_required"),
        "{default_text}"
    );
    assert!(
        default_text.contains("PIGEONMQ_TUNNEL=external"),
        "{default_text}"
    );
    assert!(!default_text.contains(home_error), "{default_text}");
    assert!(unset(&["--local-only"]).contains(home_error));
    assert!(run(&[], "tunnl").contains(home_error));
    assert!(run(&["--env-file", ".env"], "tunnl").contains("invalid_tunnel_mode"));
    fs::write(&path, "PIGEONMQ_TUNNEL=external\n").unwrap();
    assert!(
        run(&["--env-file", ".env"], "external").contains("external_tunnel_requires_public_url")
    );

    fs::write(
        &path,
        "PIGEONMQ_TUNNEL=external\nPIGEONMQ_PUBLIC_URL=https://pigeon.example\n",
    )
    .unwrap();
    assert!(run(&["--env-file", ".env"], "bad").contains(home_error));
    assert!(run(&["--env-file", "nonexistent", "--local-only"], "bad").contains(home_error));
    assert!(!missing_home.exists());
}

#[test]
fn queue_file_flag_is_reverse_scoped_to_codex_queue() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fixture.sqlite3");
    drop(pigeonmq::Queue::open(&db).unwrap());
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        c.current_dir(dir.path());
        c
    };

    let claim = command()
        .args(["codex-queue", "--db"])
        .arg(&db)
        .args(["--id", "absent", "--sender", "fixture", "claim"])
        .output()
        .unwrap();
    assert!(
        claim.status.success(),
        "{}",
        String::from_utf8_lossy(&claim.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&claim.stdout).trim(), "null");

    for (action, args) in [
        ("init", &["--home", "unused"][..]),
        ("codex-queue", &["--help"][..]),
    ] {
        let rejected = command()
            .arg("--db")
            .arg(&db)
            .arg(action)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(rejected.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("unexpected argument '--db'"));
    }
    assert!(!dir.path().join("unused").exists());
}

#[test]
fn hidden_dot_setup_still_denies_without_writing_and_has_no_home_argument() {
    let dir = tempfile::tempdir().unwrap();
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        c.current_dir(dir.path()).arg("dot-tunnel-setup");
        c
    };
    let gate = command().output().unwrap();
    assert_eq!(gate.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&gate.stderr).contains("cannot verify Dot access"));
    let rejected = command().args(["--home", "unused"]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unexpected argument '--home'"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
