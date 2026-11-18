use pigeonmq::{Envelope, Queue};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Command, Output},
    time::Duration,
};

fn inspect(queue: &Path, id: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
        .env_clear()
        .args(["dlq-inspect", "--queue"])
        .arg(queue)
        .args(["--id", id])
        .output()
        .unwrap()
}

fn response(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rejected(output: Output, reason: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(reason), "{stderr}");
}

fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn inspect_only_quarantined_id_and_leave_queue_unchanged() {
    let directory = private_directory();
    let home = directory.path().canonicalize().unwrap();
    let path = home.join("queue.db");
    let mut queue = Queue::open(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let envelope = Envelope {
        sender: "sender_fixture".into(),
        recipient: "recipient_fixture".into(),
        body: "private body sentinel".into(),
        correlation_id: Some("private correlation sentinel".into()),
        in_reply_to: None,
        idempotency_key: None,
    };
    let id = queue.send(envelope.clone()).unwrap().message.id;
    let sql = Connection::open(&path).unwrap();
    for attempt in 1..=5 {
        let delivery = queue
            .receive("recipient_fixture", Duration::from_secs(60))
            .unwrap()
            .unwrap();
        assert_eq!(delivery.message.id, id);
        assert_eq!(delivery.attempt, attempt);
        sql.execute("UPDATE messages SET lease_until_ms=0 WHERE id=?1", [&id])
            .unwrap();
    }
    assert!(
        queue
            .receive("recipient_fixture", Duration::from_secs(60))
            .unwrap()
            .is_none()
    );
    let timestamp: i64 = sql
        .query_row(
            "SELECT dead_lettered_at_ms FROM messages WHERE id=?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    let pending = queue.send(envelope).unwrap().message.id;
    drop(sql);
    drop(queue);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    let before = fs::read(&path).unwrap();

    assert_eq!(
        response(inspect(&path, &id)),
        json!({"state":"dead_lettered","attempts":5,"dead_lettered_at_ms":timestamp})
    );
    for other in [pending.as_str(), "missing", "' OR 1=1 --"] {
        assert_eq!(
            response(inspect(&path, other)),
            json!({"state":"not_dead_lettered_or_missing"})
        );
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
}

#[test]
fn inspect_rejects_unsafe_paths_and_incompatible_queues_without_writes() {
    let directory = private_directory();
    let home = directory.path().canonicalize().unwrap();
    let missing = home.join("missing.db");
    rejected(inspect(&missing, "id"), "owner_private_queue_required");
    assert!(!missing.exists());

    let path = home.join("queue.db");
    drop(Queue::open(&path).unwrap());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    rejected(inspect(&path, "id"), "owner_private_queue_required");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = home.join("link.db");
    symlink(&path, &link).unwrap();
    rejected(inspect(&link, "id"), "owner_private_queue_required");

    let sql = Connection::open(&path).unwrap();
    sql.pragma_update(None, "user_version", 2).unwrap();
    drop(sql);
    let before = fs::read(&path).unwrap();
    rejected(
        inspect(&path, "id"),
        "unsupported database schema version: 2",
    );
    assert_eq!(fs::read(&path).unwrap(), before);

    let sql = Connection::open(&path).unwrap();
    sql.pragma_update(None, "user_version", pigeonmq::SCHEMA_VERSION)
        .unwrap();
    sql.pragma_update(None, "application_id", 0).unwrap();
    drop(sql);
    let before = fs::read(&path).unwrap();
    rejected(inspect(&path, "id"), "not a PigeonMQ database");
    assert_eq!(fs::read(&path).unwrap(), before);

    let wal = home.join("wal.db");
    drop(Queue::open(&wal).unwrap());
    fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).unwrap();
    let sql = Connection::open(&wal).unwrap();
    sql.pragma_update(None, "journal_mode", "WAL").unwrap();
    drop(sql);
    let before = fs::read(&wal).unwrap();
    assert_eq!(fs::read_dir(&home).unwrap().count(), 3);
    rejected(inspect(&wal, "id"), "unsupported_queue_journal_mode");
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert_eq!(fs::read_dir(&home).unwrap().count(), 3);
}
