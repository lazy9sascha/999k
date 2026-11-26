//! One-time delivery of the selected mailbox's existing credential.
use crate::{auth, cloud_setup, mailbox_startup, stdio_mcp};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use radio999k::{Queue, mcp_request::strict_json, private_file};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

type Fault = (u16, &'static str);
const DENIED: Fault = (401, "pairing_denied");
const UNAVAILABLE: Fault = (503, "pairing_unavailable");
const TTL_MS: i64 = 10 * 60 * 1000;
pub(crate) const BODY_LIMIT: usize = 256;

pub(crate) use radio999k::mailbox_catalog::{pairing_paths as route, pairing_peer as by_path};

fn unavailable() -> io::Error {
    io::Error::other("pairing state unavailable")
}

fn time() -> io::Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(unavailable)
}

fn database(path: &Path) -> io::Result<(Connection, String)> {
    let file = private_file::open_private_file(path).map_err(|_| unavailable())?;
    let metadata = file.metadata()?;
    drop(Queue::open_existing(path).map_err(|_| unavailable())?);
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(path, flags).map_err(|_| unavailable())?;
    connection
        .busy_timeout(Duration::from_millis(250))
        .map_err(|_| unavailable())?;
    let current = private_file::open_private_file(path)
        .map_err(|_| unavailable())?
        .metadata()?;
    if (metadata.dev(), metadata.ino()) != (current.dev(), current.ino()) {
        return Err(unavailable());
    }
    Ok((connection, format!("{}:{}", metadata.dev(), metadata.ino())))
}

fn credential(path: &Path) -> io::Result<(String, [u8; 32])> {
    if !path.is_absolute() {
        return Err(io::Error::other(
            "pairing requires an absolute owner-private bearer file",
        ));
    }
    let bearer = stdio_mcp::bearer(path)?;
    let digest = auth::presented_token_digest(&[format!("Bearer {bearer}").into_bytes()])
        .map_err(|_| io::Error::other("invalid private bearer file"))?;
    Ok((bearer, digest))
}

pub(crate) fn issue(home: &Path, peer: &str, bearer_file: &Path) -> io::Result<String> {
    let (mcp, _) =
        route(peer).ok_or_else(|| io::Error::other("this peer does not support pairing"))?;
    let home = cloud_setup::require_home(home)?;
    let (bearer, fingerprint) = credential(bearer_file)?;
    let hosted = if peer == "cue" {
        crate::lifecycle::verify_finite_cue_pairing_bearer(&home, &bearer)?
    } else {
        false
    };
    if !hosted {
        stdio_mcp::verify_pairing_bearer(&home, mcp, &bearer)?;
    }
    store(
        &home.join(mailbox_startup::QUEUE),
        peer,
        bearer_file,
        fingerprint,
        time()?,
    )
}

fn store(
    db: &Path,
    peer: &str,
    path: &Path,
    fingerprint: [u8; 32],
    now: i64,
) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let code = URL_SAFE_NO_PAD.encode(bytes);
    let digest: [u8; 32] = Sha256::digest(code.as_bytes()).into();
    let expires = now.checked_add(TTL_MS).ok_or_else(unavailable)?;
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("pairing requires a UTF-8 bearer path"))?;
    let (mut connection, queue_identity) = database(db)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| unavailable())?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS orca_mailbox_pairing (
        mailbox TEXT PRIMARY KEY,
        digest BLOB NOT NULL,
        bearer_path TEXT NOT NULL,
        fingerprint BLOB NOT NULL,
        queue_identity TEXT NOT NULL,
        issued_ms INTEGER NOT NULL,
        expires_ms INTEGER NOT NULL
    );",
    )
    .map_err(|_| unavailable())?;
    tx.execute(
        "REPLACE INTO orca_mailbox_pairing
         (mailbox, digest, bearer_path, fingerprint, queue_identity, issued_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            peer,
            digest.as_slice(),
            path,
            fingerprint.as_slice(),
            queue_identity,
            now,
            expires
        ],
    )
    .map_err(|_| unavailable())?;
    tx.commit().map_err(|_| unavailable())?;
    Ok(code)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    code: String,
}

pub(crate) fn redeem(
    db: &Path,
    peer: &str,
    mcp: &str,
    body: &[u8],
    active: impl FnOnce(&str) -> bool,
) -> Result<Value, Fault> {
    if !route(peer).is_some_and(|(path, _)| path == mcp) {
        return Err(DENIED);
    }
    let request: Request = strict_json(body)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or(DENIED)?;
    if request.code.len() != 43
        || !URL_SAFE_NO_PAD
            .decode(&request.code)
            .is_ok_and(|b| b.len() == 32 && URL_SAFE_NO_PAD.encode(b) == request.code)
    {
        return Err(DENIED);
    }
    redeem_at(
        db,
        peer,
        mcp,
        &request.code,
        time().map_err(|_| UNAVAILABLE)?,
        active,
    )
}

fn redeem_at(
    db: &Path,
    peer: &str,
    mcp: &str,
    code: &str,
    now: i64,
    active: impl FnOnce(&str) -> bool,
) -> Result<Value, Fault> {
    let code_digest: [u8; 32] = Sha256::digest(code.as_bytes()).into();
    let (mut connection, queue_identity) = database(db).map_err(|_| UNAVAILABLE)?;
    let present = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='orca_mailbox_pairing')",
        [], |row| row.get::<_, bool>(0),
    ).map_err(|_| UNAVAILABLE)?;
    if !present {
        return Err(DENIED);
    }
    type Issuance = (PathBuf, Vec<u8>);
    let find = |connection: &Connection| -> Result<Option<Issuance>, Fault> {
        connection
            .query_row(
                "SELECT bearer_path, fingerprint FROM orca_mailbox_pairing
             WHERE mailbox=?1 AND digest=?2 AND queue_identity=?3
             AND issued_ms<=?4 AND expires_ms>?4",
                params![peer, code_digest.as_slice(), queue_identity, now],
                |row| Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?)),
            )
            .optional()
            .map_err(|_| UNAVAILABLE)
    };
    // Guessed and expired codes never acquire the queue's write reservation.
    if find(&connection)?.is_none() {
        return Err(DENIED);
    }
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| UNAVAILABLE)?;
    let (path, fingerprint) = find(&tx)?.ok_or(DENIED)?;
    let (bearer, actual) = credential(&path).map_err(|_| DENIED)?;
    if fingerprint.ct_eq(&actual).unwrap_u8() != 1 || !active(&bearer) {
        return Err(DENIED);
    }
    tx.execute(
        "DELETE FROM orca_mailbox_pairing WHERE mailbox=?1 AND digest=?2",
        params![peer, code_digest.as_slice()],
    )
    .map_err(|_| UNAVAILABLE)?;
    tx.commit().map_err(|_| UNAVAILABLE)?;
    Ok(json!({"mailbox":peer, "mcp_path":mcp, "token_type":"Bearer", "access_token":bearer}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, sync::Barrier};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, [u8; 32]) {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let queue = home
            .path()
            .canonicalize()
            .unwrap()
            .join(mailbox_startup::QUEUE);
        drop(Queue::open(&queue).unwrap());
        fs::set_permissions(&queue, fs::Permissions::from_mode(0o600)).unwrap();
        let file = home.path().join("fixture-bearer");
        private_file::replace_private(&file, b"Bearer fixture-pairing-bearer\n").unwrap();
        let (_, digest) = credential(&file).unwrap();
        (home, queue, file, digest)
    }

    #[test]
    fn pairing_rejects_wrong_scope_expiry_and_rotated_or_revoked_credentials() {
        let (_home, queue, file, digest) = fixture();
        let code = store(&queue, "muse", &file, digest, 10).unwrap();
        assert_eq!(code.len(), 43);
        let queue_bytes = fs::read(&queue).unwrap();
        assert!(
            !queue_bytes
                .windows(code.len())
                .any(|bytes| bytes == code.as_bytes())
        );
        assert!(
            !queue_bytes
                .windows(22)
                .any(|bytes| bytes == b"fixture-pairing-bearer")
        );
        for (peer, now) in [("grok-bot", 11), ("muse", 9), ("muse", 10 + TTL_MS)] {
            assert_eq!(
                redeem_at(&queue, peer, "/muse/mcp", &code, now, |_| panic!(
                    "unauthorized code reached policy"
                )),
                Err(DENIED)
            );
        }
        let replacement = store(&queue, "muse", &file, digest, 12).unwrap();
        assert_ne!(replacement, code);
        assert_eq!(
            redeem_at(&queue, "muse", "/muse/mcp", &code, 13, |_| true),
            Err(DENIED)
        );
        assert_eq!(
            redeem_at(&queue, "muse", "/muse/mcp", &replacement, 13, |_| false),
            Err(DENIED)
        );
        private_file::replace_private(&file, b"fixture-rotated-bearer\n").unwrap();
        assert_eq!(
            redeem_at(&queue, "muse", "/muse/mcp", &replacement, 13, |_| panic!(
                "changed bearer reached policy"
            )),
            Err(DENIED)
        );
    }

    #[test]
    fn concurrent_pairing_consumes_before_returning_the_only_credential() {
        let (_home, queue, file, digest) = fixture();
        let code = store(&queue, "cue", &file, digest, 10).unwrap();
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        redeem_at(&queue, "cue", "/cue/mcp", &code, 11, |bearer| {
                            bearer == "fixture-pairing-bearer"
                        })
                    })
                })
                .collect();
            let results: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(
                results
                    .iter()
                    .filter(|result| *result == &Err(DENIED))
                    .count(),
                1
            );
            let won = results.into_iter().find_map(Result::ok).unwrap();
            assert_eq!(
                won,
                json!({"mailbox":"cue", "mcp_path":"/cue/mcp", "token_type":"Bearer", "access_token":"fixture-pairing-bearer"})
            );
        });
        assert_eq!(
            redeem_at(&queue, "cue", "/cue/mcp", &code, 12, |_| true),
            Err(DENIED)
        );
    }
}
