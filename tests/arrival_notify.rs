use pigeonmq::{Envelope, Queue};
use rusqlite::Connection;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

const MAILBOXES: [&str; 2] = ["cue", "instinct"];
const BODY: &str = "synthetic message body must stay in the queue";
const RECEIPT: &str = "synthetic-receipt-must-stay-in-the-queue";

fn write_private(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn envelope(sender: &str, recipient: &str) -> Envelope {
    Envelope {
        sender: sender.into(),
        recipient: recipient.into(),
        body: BODY.into(),
        correlation_id: None,
        in_reply_to: None,
        idempotency_key: None,
    }
}

fn send(queue: &mut Queue, sender: &str, recipient: &str) -> String {
    queue.send(envelope(sender, recipient)).unwrap().message.id
}

fn linked(queue: &mut Queue, mailbox: &str) -> String {
    let parent = send(queue, mailbox, "dot");
    let mut answer = envelope("dot", mailbox);
    answer.in_reply_to = Some(parent.clone());
    answer.correlation_id = Some(parent.clone());
    queue
        .reply_unclaimed_checked("dot", &parent, answer, || Ok(()))
        .unwrap()
        .id
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "notifier failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for stream in [output.stdout, output.stderr] {
        let stream = String::from_utf8_lossy(&stream);
        assert!(!stream.contains(BODY));
        assert!(!stream.contains(RECEIPT));
    }
}

#[test]
fn activation_skips_history_and_restart_delivers_only_new_eligible_ids() {
    let fixture = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(fixture.path()).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let database = dir.join("queue.sqlite3");
    let registration = dir.join("registration.json");
    let state = dir.join("arrival-state.json");
    let launcher = dir.join("record-arrival");
    let records = dir.join("record-arrival.log");
    write_private(
        &registration,
        br#"{
            "cue":{"mailbox":"cue","revoked":false},
            "instinct":{"mailbox":"instinct","revoked":false},
            "dot":{"mailbox":"dot","revoked":false},
            "other":{"mailbox":"other","revoked":false},
            "revoked":{"mailbox":"revoked","revoked":true}
        }"#,
        0o600,
    );
    write_private(
        &launcher,
        br#"#!/bin/sh
[ "$#" -eq 2 ] || exit 65
printf '%s %s\n' "$1" "$2" >> "$0.log"
"#,
        0o700,
    );
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_pigeonmq"))
            .env_clear()
            .current_dir(&dir)
            .arg("notify-arrivals")
            .arg("--queue")
            .arg(&database)
            .arg("--registration")
            .arg(&registration)
            .arg("--state")
            .arg(&state)
            .arg("--command")
            .arg(&launcher)
            .args(["--mailbox", "cue", "--mailbox", "instinct"])
            .output()
            .unwrap()
    };

    let mut queue = Queue::open(&database).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    for mailbox in MAILBOXES {
        send(&mut queue, "dot", mailbox);
        linked(&mut queue, mailbox);
    }
    drop(queue);
    let before = fs::read(&database).unwrap();
    assert_success(run());
    assert!(!records.exists(), "historical messages were notified");
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut queue = Queue::open_existing(&database).unwrap();
    let mut expected = String::new();
    for mailbox in MAILBOXES {
        let ordinary = send(&mut queue, "dot", mailbox);
        let terminal = linked(&mut queue, mailbox);
        expected.push_str(&format!("{mailbox} {ordinary}\n{mailbox} {terminal}\n"));
        send(&mut queue, "dot", "other");
        send(&mut queue, "unknown", mailbox);
        send(&mut queue, "revoked", mailbox);
        queue.request(envelope("dot", mailbox)).unwrap();

        let claimed = send(&mut queue, "dot", mailbox);
        let acknowledged = send(&mut queue, "dot", mailbox);
        let dead = send(&mut queue, "dot", mailbox);
        let bad_link = linked(&mut queue, mailbox);
        let conn = Connection::open(&database).unwrap();
        conn.execute(
            "UPDATE messages SET receipt=?1, attempts=1, lease_until_ms=1 WHERE id=?2",
            [RECEIPT, &claimed],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET acknowledged_at_ms=1 WHERE id=?1",
            [&acknowledged],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET dead_lettered_at_ms=1 WHERE id=?1",
            [&dead],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET correlation_id='wrong' WHERE id=?1",
            [&bad_link],
        )
        .unwrap();
    }
    drop(queue);

    let before = fs::read(&database).unwrap();
    assert_success(run());
    assert_eq!(fs::read_to_string(&records).unwrap(), expected);
    assert_eq!(fs::read(&database).unwrap(), before);

    assert_success(run());
    assert_eq!(fs::read_to_string(&records).unwrap(), expected);

    let mut queue = Queue::open_existing(&database).unwrap();
    let next = send(&mut queue, "dot", "instinct");
    expected.push_str(&format!("instinct {next}\n"));
    drop(queue);
    assert_success(run());
    assert_eq!(fs::read_to_string(&records).unwrap(), expected);

    let cursor = fs::read(&state).unwrap();
    pigeonmq::mailbox_generation::retire(&database, "cue").unwrap();
    assert!(!run().status.success());
    pigeonmq::mailbox_generation::reactivate(&database, "cue").unwrap();
    let mut queue = Queue::open_existing(&database).unwrap();
    send(&mut queue, "dot", "cue");
    linked(&mut queue, "cue");
    drop(queue);
    let rejected = run();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("mailbox generation requires managed relay --config")
    );
    assert_eq!(fs::read(&state).unwrap(), cursor);
    assert_eq!(fs::read_to_string(&records).unwrap(), expected);
}

#[test]
fn configured_relay_keeps_other_cursors_and_never_sends_for_a_retired_generation() {
    use pigeonmq::arrival_notify::{RelayConfig, RelayTarget, run_config};

    let fixture = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(fixture.path()).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let database = dir.join("queue.sqlite3");
    let registration = dir.join("registration.json");
    let state = dir.join("arrival-state.json");
    let config = dir.join("recipients.json");
    let launcher = dir.join("record-arrival");
    let records = dir.join("record-arrival.log");
    write_private(
        &registration,
        br#"{
            "cue":{"mailbox":"cue","revoked":false},
            "instinct":{"mailbox":"instinct","revoked":false},
            "dot":{"mailbox":"dot","revoked":false}
        }"#,
        0o600,
    );
    write_private(
        &launcher,
        br#"#!/bin/sh
[ "$#" -eq 2 ] || exit 65
printf '%s %s\n' "$1" "$2" >> "$0.log"
"#,
        0o700,
    );
    let floor = || {
        Connection::open(&database)
            .unwrap()
            .query_row("SELECT COALESCE(MAX(seq),0) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let run = || {
        run_config(&database, &registration, &state, &launcher, &config).unwrap()["delivered"]
            .as_u64()
            .unwrap()
    };
    let publish = |targets: &RelayConfig| {
        write_private(&config, &serde_json::to_vec(targets).unwrap(), 0o600);
    };

    let cue_target = "cue-test@example.invalid";
    let instinct_target = "instinct-test@example.invalid";
    let mut queue = Queue::open(&database).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    send(&mut queue, "dot", "cue");
    let mut targets = RelayConfig::from([(
        "cue".into(),
        RelayTarget {
            recipient: cue_target.into(),
            generation: "cue".into(),
            after: floor(),
        },
    )]);
    publish(&targets);
    let cue_first = send(&mut queue, "dot", "cue");
    send(&mut queue, "dot", "instinct");
    assert_eq!(run(), 1);

    targets.insert(
        "instinct".into(),
        RelayTarget {
            recipient: instinct_target.into(),
            generation: "instinct".into(),
            after: floor(),
        },
    );
    publish(&targets);
    let cue_second = linked(&mut queue, "cue");
    let instinct_first = send(&mut queue, "dot", "instinct");
    assert_eq!(run(), 2);
    assert_eq!(run(), 0);

    let retired_cue = send(&mut queue, "dot", "cue");
    drop(queue);
    pigeonmq::mailbox_generation::retire(&database, "cue").unwrap();
    pigeonmq::mailbox_generation::reactivate(&database, "cue").unwrap();
    let new_floor = floor();
    let mut queue = Queue::open_existing(&database).unwrap();
    let cue_next_generation = send(&mut queue, "dot", "cue");
    assert_eq!(run(), 0, "stale configuration must fail closed");

    let cue = targets.get_mut("cue").unwrap();
    cue.generation = pigeonmq::mailbox_generation::active_storage(&database, "cue")
        .unwrap()
        .unwrap();
    cue.after = new_floor;
    publish(&targets);
    assert_eq!(run(), 1);

    targets.remove("cue");
    publish(&targets);
    send(&mut queue, "dot", "cue");
    let instinct_second = send(&mut queue, "dot", "instinct");
    assert_eq!(run(), 1);
    assert_eq!(run(), 0);
    let observed = fs::read_to_string(&records).unwrap();
    assert_eq!(
        observed,
        format!(
            "{cue_target} {cue_first}\n{cue_target} {cue_second}\n{instinct_target} {instinct_first}\n{cue_target} {cue_next_generation}\n{instinct_target} {instinct_second}\n"
        )
    );
    assert!(!observed.contains(&retired_cue));
    assert!(!observed.contains(BODY));
}

#[test]
fn legacy_relay_still_reads_a_catalogless_queue() {
    use pigeonmq::arrival_notify::run;

    let fixture = tempfile::tempdir().unwrap();
    let dir = fixture.path().canonicalize().unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let database = dir.join("queue.sqlite3");
    drop(Queue::open(&database).unwrap());
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    Connection::open(&database)
        .unwrap()
        .execute_batch("DROP TABLE pigeonmq_mailbox_generations")
        .unwrap();

    let registration = dir.join("registration.json");
    let command = dir.join("record-arrival");
    let state = dir.join("state.json");
    write_private(
        &registration,
        br#"{"cue":{"mailbox":"cue","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#,
        0o600,
    );
    write_private(
        &command,
        b"#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$2\" > \"$0.log\"\n",
        0o700,
    );
    let run_legacy = || run(&database, &registration, &state, &command, &["cue".into()]);
    assert_eq!(run_legacy().unwrap()["state"], "initialized");
    let mut queue = Queue::open_existing(&database).unwrap();
    let id = send(&mut queue, "dot", "cue");
    assert_eq!(run_legacy().unwrap()["delivered"], 1);
    assert_eq!(
        fs::read_to_string(dir.join("record-arrival.log")).unwrap(),
        format!("cue {id}\n")
    );
    assert!(!pigeonmq::mailbox_generation::lock_path(&database).exists());
}

#[test]
fn managed_provider_uses_controlled_path_and_reports_no_private_command_output() {
    use pigeonmq::arrival_notify::{RelayConfig, RelayTarget};

    let fixture = tempfile::tempdir().unwrap();
    let dir = fixture.path().canonicalize().unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let database = dir.join("queue.sqlite3");
    let registration = dir.join("registration.json");
    let state = dir.join("arrival-state.json");
    let config = dir.join("recipients.json");
    let launcher = dir.join("record-arrival");
    let records = dir.join("record-arrival.log");
    write_private(
        &registration,
        br#"{"cue":{"mailbox":"cue","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#,
        0o600,
    );
    let recipient = "config-recipient@example.invalid";
    let targets = RelayConfig::from([(
        "cue".into(),
        RelayTarget {
            recipient: recipient.into(),
            generation: "cue".into(),
            after: 0,
        },
    )]);
    write_private(&config, &serde_json::to_vec(&targets).unwrap(), 0o600);
    write_private(
        &launcher,
        br#"#!/bin/sh
[ "$PATH" = "/usr/bin:/bin:/usr/sbin:/sbin" ] || exit 91
printf '%s %s\n' "$1" "$2" >> "$0.log"
"#,
        0o700,
    );
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_pigeonmq"))
            .env_clear()
            .env("PATH", dir.join("untrusted-path"))
            .current_dir(&dir)
            .arg("notify-arrivals")
            .arg("--queue")
            .arg(&database)
            .arg("--registration")
            .arg(&registration)
            .arg("--state")
            .arg(&state)
            .arg("--command")
            .arg(&launcher)
            .arg("--config")
            .arg(&config)
            .output()
            .unwrap()
    };

    let mut queue = Queue::open(&database).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    let accepted = send(&mut queue, "dot", "cue");
    assert_success(run());
    assert_eq!(
        fs::read_to_string(&records).unwrap(),
        format!("{recipient} {accepted}\n")
    );

    let pending = send(&mut queue, "dot", "cue");
    write_private(
        &launcher,
        br#"#!/bin/sh
printf 'synthetic-private-marker %s %s\n' "$1" "$2"
printf 'synthetic-private-marker %s %s\n' "$1" "$2" >&2
exit 4
"#,
        0o700,
    );
    let failed = run();
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("provider_exited_unsuccessfully"));
    for output in [&*stderr, &String::from_utf8_lossy(&failed.stdout)] {
        for private_text in ["synthetic-private-marker", recipient, pending.as_str()] {
            assert!(!output.contains(private_text));
        }
    }
}

#[test]
fn invalid_static_mailboxes_are_rejected_before_starting_any_relay() {
    use pigeonmq::arrival_notify::validate_static_mailboxes;
    use serde_json::json;
    use std::process::Stdio;
    use std::thread;
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = home.join("companions.json");
    let queue = home.join("missing.sqlite3");
    let registration = home.join("registration.json");
    let state = home.join("cursor.json");
    let provider = home.join("provider");
    let assert_exit = |mut command: Command, expected: &str| {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let limit = Instant::now() + Duration::from_secs(4);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Invalid relay configuration stayed running");
            }
            thread::sleep(Duration::from_millis(20));
        }
        let result = child.wait_with_output().unwrap();
        assert!(!result.status.success());
        assert_eq!(String::from_utf8_lossy(&result.stderr).trim(), expected);
        assert!(!state.exists());
        assert!(!home.join("pigeonmq-cloud-run.lock").exists());
    };
    let valid = vec![String::from("peer"), "-peer_0.x:y".into()];
    assert!(validate_static_mailboxes(&valid).is_ok());
    for mailboxes in [
        vec![String::from("duplicate"), "duplicate".into()],
        vec![String::from("bad/name")],
        vec!["a".repeat(129)],
        (0..17).map(|n| format!("fixture-{n}")).collect(),
    ] {
        assert!(validate_static_mailboxes(&mailboxes).is_err());
        write_private(
            &manifest,
            json!({"version":1, "queue_identity":{"device":1,"inode":1},
            "email_relay":{"registration":registration,"state":state,"command":provider,
                "interval_secs":1,"mailboxes":mailboxes}})
            .to_string()
            .as_bytes(),
            0o600,
        );
        let mut runner = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        runner
            .args(["run", "--home"])
            .arg(&home)
            .arg("--companions-file")
            .arg(&manifest);
        assert_exit(runner, "invalid_companions_file");

        let mut relay = Command::new(env!("CARGO_BIN_EXE_pigeonmq"));
        relay
            .arg("notify-arrivals")
            .arg("--queue")
            .arg(&queue)
            .arg("--registration")
            .arg(&registration)
            .arg("--state")
            .arg(&state)
            .arg("--command")
            .arg(&provider)
            .args(["--interval-secs", "1"]);
        for name in mailboxes {
            relay.arg(format!("--mailbox={name}"));
        }
        assert_exit(relay, "invalid arrival notifier configuration");
    }
}
