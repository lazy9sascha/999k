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
    drop(queue);
    expected.push_str(&format!("instinct {next}\n"));
    assert_success(run());
    assert_eq!(fs::read_to_string(&records).unwrap(), expected);
}
