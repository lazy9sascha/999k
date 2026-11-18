use pigeonmq::{Error, Queue};
use rusqlite::Connection;

#[test]
fn wal_is_rejected_by_both_open_paths_without_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    drop(Queue::open(&path).unwrap());
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    for existing in [false, true] {
        let result = if existing {
            Queue::open_existing(&path)
        } else {
            Queue::open(&path)
        };
        assert!(matches!(
            result,
            Err(Error::Invalid("unsupported_queue_journal_mode"))
        ));
        let mode: String = db
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, pigeonmq::SCHEMA_VERSION);
    }
}

#[test]
fn rollback_queue_reopens_and_ephemeral_memory_remains_available() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue.db");
    drop(Queue::open(&path).unwrap());
    drop(Queue::open_existing(&path).unwrap());
    drop(Queue::open(&path).unwrap());
    drop(Queue::open(":memory:").unwrap());
}

#[test]
fn old_schema_is_rejected_without_changing_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE messages(id TEXT PRIMARY KEY, body TEXT); INSERT INTO messages VALUES ('old', 'keep'); PRAGMA application_id=1346851153;").unwrap();
    for version in [2, 3] {
        db.pragma_update(None, "user_version", version).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(Queue::open(&path), Err(Error::Schema(v)) if v == version));
        assert!(matches!(Queue::open_existing(&path), Err(Error::Schema(v)) if v == version));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
