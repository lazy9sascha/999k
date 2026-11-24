use radio999k::{NeverClaimedBacklog, Queue};
use rusqlite::{Connection, params};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn aggregate_includes_only_never_claimed_ordinary_pending_requests() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("queue.db");
    drop(Queue::open(&db).unwrap());
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let inspect = || Queue::inspect_never_claimed_backlog(&db).unwrap();
    assert_eq!(
        inspect(),
        NeverClaimedBacklog {
            count: 0,
            oldest_age_ms: None
        }
    );

    let conn = Connection::open(&db).unwrap();
    let now = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let started = now();
    for (id, created) in [
        ("future", started + 60_000),
        ("newer", started - 10_000),
        ("oldest", started - 30_000),
        ("leased", started - 40_000),
        ("expired", started - 50_000),
        ("acknowledged", started - 60_000),
        ("dead_lettered", started - 70_000),
        ("terminal_reply", started - 80_000),
        ("rpc", started - 90_000),
    ] {
        conn.execute(
            "INSERT INTO messages (id, conversation_id, sender, recipient, body, created_at_ms)
             VALUES (?1, 'synthetic', 'a', 'b', 'fixture', ?2)",
            params![id, created],
        )
        .unwrap();
    }
    conn.execute_batch(&format!(
        "UPDATE messages SET receipt='synthetic', attempts=1, lease_until_ms={}
            WHERE id='leased';
         UPDATE messages SET receipt='synthetic', attempts=2, lease_until_ms={}
            WHERE id='expired';
         UPDATE messages SET acknowledged_at_ms=1 WHERE id='acknowledged';
         UPDATE messages SET receipt='synthetic', attempts=5, dead_lettered_at_ms=1
            WHERE id='dead_lettered';
         UPDATE messages SET in_reply_to='acknowledged' WHERE id='terminal_reply';
         INSERT INTO rpc_requests (message_id) VALUES ('rpc');",
        started + 60_000,
        started - 60_000
    ))
    .unwrap();
    let before = fs::read(&db).unwrap();
    let backlog = inspect();
    let ages = 30_000..=30_000 + (now() - started);
    assert_eq!(backlog.count, 3);
    assert!(ages.contains(&backlog.oldest_age_ms.unwrap()));
    assert_eq!(fs::read(&db).unwrap(), before);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

    conn.execute("DELETE FROM messages WHERE id IN ('newer', 'oldest')", [])
        .unwrap();
    assert_eq!(
        inspect(),
        NeverClaimedBacklog {
            count: 1,
            oldest_age_ms: Some(0)
        }
    );
}
