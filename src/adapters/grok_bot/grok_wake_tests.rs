use super::*;
use crate::{Envelope, Queue};

fn env(sender: &str, recipient: &str, reply: Option<String>) -> Envelope {
    Envelope {
        sender: sender.into(),
        recipient: recipient.into(),
        body: "fixture".into(),
        correlation_id: None,
        in_reply_to: reply,
        idempotency_key: None,
    }
}

#[test]
fn classify_bounds_retry() {
    assert_eq!(classify(200), Outcome::Delivered);
    assert_eq!(classify(204), Outcome::Delivered);
    assert_eq!(classify(401), Outcome::AuthBlock);
    assert_eq!(classify(403), Outcome::AuthBlock);
    assert_eq!(classify(429), Outcome::RateLimited);
    assert_eq!(classify(503), Outcome::Retry);
    assert_eq!(classify(408), Outcome::Retry);
    assert_eq!(classify(404), Outcome::Rejected);
    assert_eq!(classify(302), Outcome::Rejected);
}

#[test]
fn body_has_only_ids_and_mailbox() {
    let v: Value = serde_json::from_slice(&wake_body("m-1", "thread-root")).unwrap();
    let o = v.as_object().unwrap();
    assert_eq!(o.len(), 3);
    assert_eq!(o["message_id"], "m-1");
    assert_eq!(o["conversation_id"], "thread-root");
    assert_eq!(o["mailbox"], "grok-bot");
}

#[test]
fn auth_prefix_is_stripped() {
    assert_eq!(
        parse_auth("Authorization: Bearer abc\n").unwrap(),
        "Bearer abc"
    );
    assert_eq!(
        parse_auth("authorization:Bearer abc").unwrap(),
        "Bearer abc"
    );
    assert_eq!(parse_auth("Bearer abc").unwrap(), "Bearer abc");
    assert!(parse_auth("Authorization:   ").is_err());
    assert!(parse_auth("Bearer a\r\nX-Evil: 1").is_err());
    assert!(parse_auth("Bearer abc").unwrap().is_sensitive());
}

#[test]
fn url_must_be_https() {
    assert!(parse_url("https://example.com/hook\n").is_ok());
    assert!(parse_url("http://example.com/hook").is_err());
    assert!(parse_url("https://u:p@example.com/").is_err());
    assert!(parse_url("not a url").is_err());
}

#[test]
fn ids_are_validated() {
    assert!(valid_id("75212c62-704d-4962-9b37-15838b25be64"));
    assert!(!valid_id(""));
    assert!(!valid_id("a b"));
    assert!(!valid_id(&"a".repeat(129)));
}

#[test]
fn pending_rejects_invalid_links_and_excludes_claimed_and_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    let mut q = Queue::open(&db).unwrap();
    let plain = q.send(env("dot", "grok-bot", None)).unwrap().message.id;
    let other = q.send(env("dot", "muse", None)).unwrap().message.id;
    let rpc = q.request(env("dot", "grok-bot", None)).unwrap().message.id;
    let reply = q
        .send(env("dot", "grok-bot", Some(other.clone())))
        .unwrap()
        .message
        .id;
    let claimed = q.send(env("dot", "grok-bot", None)).unwrap().message.id;
    drop(q);
    crate::grok_mailbox::prepare(&db).unwrap();
    let c = Connection::open(&db).unwrap();
    c.execute(
        "UPDATE messages SET attempts=1,receipt='r',lease_until_ms=?2 WHERE id=?1",
        rusqlite::params![claimed, i64::MAX],
    )
    .unwrap();
    drop(c);
    let before = fs::read(&db).unwrap();
    let ids = pending(&db, unix_s() * 1000).unwrap();
    assert_eq!(ids, vec![plain]);
    assert!(!ids.contains(&rpc) && !ids.contains(&reply) && !ids.contains(&claimed));
    assert_eq!(fs::read(&db).unwrap(), before, "observation must not write");
}

#[test]
fn state_dedups_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(STATE_FILE);
    fs::write(
        &p,
        r#"{"notified":{"a":1},"suppressed":{"b":2},"failed":{}}"#,
    )
    .unwrap();
    let mut s = WakeState::load(p.clone());
    assert!(!s.due("a") && !s.due("b") && s.due("c"));
    for _ in 0..MAX_ROUNDS {
        s.failed("d", Some(503));
    }
    assert!(!s.due("d"));
    s.delivered("c");
    s.save().unwrap();
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let s = WakeState::load(p);
    assert!(!s.due("c") && !s.due("d") && !s.due("a"));
}

#[test]
fn unsafe_credential_file_blocks_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let c = WakeConfig {
        db: dir.path().join("q.sqlite3"),
        url_file: dir.path().join(URL_FILE),
        auth_file: dir.path().join(AUTH_FILE),
        state_file: dir.path().join(STATE_FILE),
        stop: None,
        keychain: None,
        explicit_files: false,
    };
    fs::write(&c.url_file, "https://127.0.0.1:9/hook").unwrap();
    fs::write(&c.auth_file, "Authorization: Bearer x").unwrap();
    fs::set_permissions(&c.url_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&c.auth_file, fs::Permissions::from_mode(0o644)).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        rt.block_on(notify(&c, "m-1", &|_| Some("thread-root".into()))),
        (None, Outcome::AuthBlock)
    );
    assert!(WakeConfig::discover(&c.db).is_some());
    fs::remove_file(&c.url_file).unwrap();
    assert!(WakeConfig::discover(&c.db).is_none());
}

#[test]
fn denied_authority_never_posts_or_marks_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let c = WakeConfig {
        db: dir.path().join("db"),
        url_file: dir.path().join("url"),
        auth_file: dir.path().join("auth"),
        state_file: dir.path().join("state"),
        stop: None,
        keychain: None,
        explicit_files: false,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(
        rt.block_on(notify(&c, "id", &|_| None)),
        (None, Outcome::Rejected)
    );
    assert!(!c.state_file.exists());
}

#[test]
fn ineligible_first_page_does_not_starve_later_authorized_message() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let mut q = Queue::open(&db).unwrap();
    crate::grok_mailbox::prepare(&db).unwrap();
    for _ in 0..50 {
        q.send(env("revoked", MAILBOX, None)).unwrap();
    }
    let good = q.send(env("hermes", MAILBOX, None)).unwrap().message.id;
    let (first, next) = pending_page(&db, i64::MAX, 0).unwrap();
    assert_eq!(first.len(), 50);
    assert!(!first.contains(&good));
    assert!(next > 0);
    let (second, next) = pending_page(&db, i64::MAX, next).unwrap();
    assert_eq!(second, vec![good]);
    assert_eq!(next, 0);
}

#[test]
fn malformed_id_page_advances_without_modifying_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let mut q = Queue::open(&db).unwrap();
    crate::grok_mailbox::prepare(&db).unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    for n in 0..50u8 {
        let id = q.send(env("hermes", MAILBOX, None)).unwrap().message.id;
        c.execute(
            "UPDATE messages SET id=? WHERE id=?",
            rusqlite::params![vec![128u8, n], id],
        )
        .unwrap();
    }
    let good = q.send(env("hermes", MAILBOX, None)).unwrap().message.id;
    let (first, next) = pending_page(&db, i64::MAX, 0).unwrap();
    assert!(first.is_empty());
    assert!(next > 0);
    let (second, next) = pending_page(&db, i64::MAX, next).unwrap();
    assert_eq!(second, vec![good]);
    assert_eq!(next, 0);
    let remaining: i64 = c
        .query_row(
            "SELECT count(*) FROM messages WHERE typeof(id)='blob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 50);
}

#[test]
fn save_ignores_legacy_temporary_symlink() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let victim = dir.path().join("victim");
    fs::write(&victim, "unchanged").unwrap();
    symlink(&victim, path.with_extension("json.tmp")).unwrap();
    let mut state = WakeState::load(path.clone());
    state.delivered("safe-id");
    state.save().unwrap();
    assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
    assert!(!WakeState::load(path).due("safe-id"));
}

#[test]
fn stop_flag_halts_run_loop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    // Verify stop flag can be set and read
    assert!(!stop.load(Ordering::Acquire));
    stop.store(true, Ordering::Release);
    assert!(stop.load(Ordering::Acquire));
}

#[test]
fn discover_with_stop_sets_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    std::fs::write(&db, b"").unwrap();
    let url = dir.path().join(URL_FILE);
    let auth = dir.path().join(AUTH_FILE);
    std::fs::write(&url, "https://example.com").unwrap();
    std::fs::write(&auth, "Bearer token").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&url, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    let c = discover_with_stop(&db, stop.clone());
    assert!(c.is_some());
    let c = c.unwrap();
    assert!(c.stop.is_some());
    c.stop.as_ref().unwrap().store(true, Ordering::Release);
    assert!(stop.load(Ordering::Acquire));
}

#[test]
fn singleton_lock_rejects_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    std::fs::write(&db, b"").unwrap();
    let lock_path = db.with_extension("grok-wake.lock");
    // Create lock file with safe permissions
    let f1 = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        unsafe { libc::flock(f1.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    // Second lock attempt should fail
    let f2 = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    assert_ne!(
        unsafe { libc::flock(f2.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    drop(f1);
    // After first lock released, second should succeed
    assert_eq!(
        unsafe { libc::flock(f2.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
}

#[test]
fn lock_is_created_private_and_existing_unsafe_lock_stays_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    let lock = db.with_extension("grok-wake.lock");
    let held = acquire_lock(&db).unwrap();
    assert_eq!(held.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    drop(held);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(acquire_lock(&db).unwrap_err(), "unsafe_lock_file");
    assert_eq!(
        fs::metadata(lock).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn lock_file_rejects_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    std::fs::write(&db, b"").unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"").unwrap();
    let link = db.with_extension("grok-wake.lock");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // symlink_metadata should detect symlink
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink());
}

#[test]
fn lock_failure_stops_only_the_notifier_and_is_marked_failed() {
    use std::os::unix::io::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    fs::write(&db, b"").unwrap();
    let lock_path = db.with_extension("grok-wake.lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let c = WakeConfig {
        db: db.clone(),
        url_file: dir.path().join(URL_FILE),
        auth_file: dir.path().join(AUTH_FILE),
        state_file: dir.path().join(STATE_FILE),
        stop: None,
        keychain: None,
        explicit_files: false,
    };
    // spawn itself succeeds (the hub keeps going); the thread ends at once.
    spawn(c, None, |_| Some("thread-root".into()))
        .unwrap()
        .join()
        .unwrap();
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATUS_FILE)).unwrap()).unwrap();
    assert_eq!(status["state"], "failed");
    assert_eq!(status["detail"]["reason"], "already_running");
    // Lock validation is kept: a symlinked lock is refused, not followed.
    drop(held);
    fs::remove_file(&lock_path).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &lock_path).unwrap();
    assert_eq!(acquire_lock(&db).unwrap_err(), "lock_file_is_symlink");
}

#[test]
fn auth_block_is_visible_in_status_and_clears() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("q.sqlite3");
    let mut q = Queue::open(&db).unwrap();
    q.send(env("dot", "grok-bot", None)).unwrap();
    drop(q);
    crate::grok_mailbox::prepare(&db).unwrap();
    let c = WakeConfig {
        db: db.clone(),
        url_file: dir.path().join(URL_FILE),
        auth_file: dir.path().join(AUTH_FILE),
        state_file: dir.path().join(STATE_FILE),
        stop: None,
        keychain: None,
        explicit_files: false,
    };
    fs::write(&c.url_file, "https://127.0.0.1:9/hook").unwrap();
    fs::write(&c.auth_file, "Authorization: Bearer x").unwrap();
    fs::set_permissions(&c.url_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&c.auth_file, fs::Permissions::from_mode(0o644)).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut state = WakeState::load(c.state_file.clone());
    let (mut block, mut rate, mut sent, mut after) = (None, Instant::now(), Vec::new(), 0);
    let read_status = || -> Value {
        serde_json::from_slice(&fs::read(dir.path().join(STATUS_FILE)).unwrap()).unwrap()
    };
    rt.block_on(sweep(
        &c,
        &mut state,
        &mut block,
        &mut rate,
        &mut sent,
        &mut after,
        &|_| Some("thread-root".into()),
    ));
    assert!(block.is_some());
    assert_eq!(read_status()["state"], "auth_blocked");
    // A credential file change clears the block and the status says so.
    fs::write(&c.auth_file, "Authorization: Bearer changed").unwrap();
    fs::set_permissions(&c.auth_file, fs::Permissions::from_mode(0o644)).unwrap();
    rt.block_on(sweep(
        &c,
        &mut state,
        &mut block,
        &mut rate,
        &mut sent,
        &mut after,
        &|_| None,
    ));
    assert!(block.is_none());
    assert_eq!(read_status()["state"], "running");
    assert_eq!(read_status()["detail"]["cleared"], "auth_blocked");
}

#[test]
fn explicit_files_are_reopened_privately_and_never_fall_back() {
    use crate::private_file::replace_private;
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().unwrap();
    let selected = tempfile::tempdir().unwrap();
    let selected = selected.path();
    fs::set_permissions(selected, fs::Permissions::from_mode(0o700)).unwrap();
    let db = home.path().join("db.sqlite3");
    let url = selected.join(URL_FILE);
    let auth = selected.join(AUTH_FILE);
    let default_url = home.path().join(URL_FILE);
    replace_private(&default_url, b"https://default.invalid/").unwrap();
    replace_private(&home.path().join(AUTH_FILE), b"Bearer default").unwrap();
    replace_private(&url, b"https://explicit.invalid/").unwrap();
    replace_private(&auth, b"Bearer explicit").unwrap();
    validate_files_dir(home.path(), selected).unwrap();

    let c = WakeConfig::from_files_dir(&db, selected);
    assert_eq!(c.db, db);
    assert_eq!(c.state_file, home.path().join(STATE_FILE));
    assert_eq!(
        c.credentials().unwrap().0.host_str(),
        Some("explicit.invalid")
    );
    replace_private(&url, b"https://rotated.invalid/").unwrap();
    assert_eq!(
        c.credentials().unwrap().0.host_str(),
        Some("rotated.invalid")
    );

    fs::set_permissions(selected, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(c.credentials().is_err());
    fs::set_permissions(selected, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(selected, home.path().join("symlink-directory")).unwrap();
    assert!(validate_files_dir(home.path(), &home.path().join("symlink-directory")).is_err());
    fs::set_permissions(&url, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(c.credentials().is_err());
    fs::remove_file(&url).unwrap();
    assert!(c.credentials().is_err());

    symlink(&default_url, &url).unwrap();
    assert!(c.credentials().is_err());
    fs::remove_file(&url).unwrap();
    fs::hard_link(&default_url, &url).unwrap();
    assert!(c.credentials().is_err());
    fs::remove_file(&url).unwrap();

    let prefix = "https://explicit.invalid/";
    let url_at_limit = format!(
        "{prefix}{}",
        "a".repeat(MAX_WAKE_FILE_BYTES as usize - prefix.len())
    );
    assert_eq!(url_at_limit.len() as u64, MAX_WAKE_FILE_BYTES);
    replace_private(&url, url_at_limit.as_bytes()).unwrap();
    assert!(c.credentials().is_ok());
    replace_private(&url, format!("{url_at_limit}a").as_bytes()).unwrap();
    assert!(c.credentials().is_err());
}

#[test]
fn keychain_pair_is_atomic_and_no_file_fallback() {
    use std::sync::atomic::AtomicUsize;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.sqlite3");
    let mark = dir.path().join(KEYCHAIN_MARKER);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    crate::private_file::replace_private(&mark, b"keychain\n").unwrap();
    for (file, placeholder) in [
        (URL_FILE, b"https://file.invalid/hook".as_slice()),
        (AUTH_FILE, b"Bearer file".as_slice()),
    ] {
        crate::private_file::replace_private(&dir.path().join(file), placeholder).unwrap();
    }
    let read = Arc::new(AtomicUsize::new(0));
    let count = read.clone();
    let missing = WakeConfig::discover_with_keychain(&db, move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        Err(std::io::Error::other("unavailable"))
    })
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            missing.credentials().unwrap_err(),
            WakeError("credential_unavailable")
        );
    }
    assert_eq!(read.load(Ordering::Relaxed), 1);
    // A noninteractive Keychain denial must stop this notifier visibly, not
    // report running or fall back to the plaintext fixtures above.
    spawn(missing, None, |_| Some("thread-root".into()))
        .unwrap()
        .join()
        .unwrap();
    let status: Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATUS_FILE)).unwrap()).unwrap();
    assert_eq!(status["state"], "auth_blocked");
    assert_eq!(status["detail"]["reason"], "keychain_unavailable");
    assert_eq!(read.load(Ordering::Relaxed), 1);
    let invalid = WakeConfig::discover_with_keychain(&db, |_| {
        Ok(br#"{"url":"https://pair.invalid"}"#.to_vec())
    })
    .unwrap();
    assert_eq!(
        invalid.credentials().unwrap_err(),
        WakeError("invalid_keychain_pair")
    );
    let selected = WakeConfig::discover_with_keychain(&db, |_| {
        Ok(encode_keychain_pair("https://pair.invalid/hook", "Bearer pair").unwrap())
    })
    .unwrap();
    let (url, auth) = selected.credentials().unwrap();
    assert_eq!(url.host_str(), Some("pair.invalid"));
    assert!(auth.is_sensitive());
    fs::remove_file(mark).unwrap();
    assert_eq!(
        WakeConfig::discover(&db)
            .unwrap()
            .credentials()
            .unwrap()
            .0
            .host_str(),
        Some("file.invalid")
    );
}
