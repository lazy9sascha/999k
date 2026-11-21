//! Private operator state for hosted registration.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const APPLICATION_ID: i64 = 0x504D_5145; // PMQE
const SCHEMA_VERSION: i64 = 1;
const PENDING_SECONDS: i64 = 15 * 60;
const BROWSER_SECONDS: i64 = 5 * 60;
const MAX_PENDING: i64 = 32;
const MAX_GUESSES: i64 = 5;
const MAX_RESOURCES: usize = 16 * 1024;

const SCHEMA: &str = "
CREATE TABLE enrollments (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    proof_digest BLOB NOT NULL,
    code_digest BLOB,
    failed_guesses INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'ready', 'revoked')),
    expires_at INTEGER NOT NULL,
    account_id TEXT,
    issued_at INTEGER,
    resources BLOB
);
CREATE INDEX enrollment_pending_expiry ON enrollments(expires_at) WHERE state = 'pending';
CREATE TABLE browser_attempts (
    id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL UNIQUE REFERENCES enrollments(id),
    verifier TEXT,
    expires_at INTEGER NOT NULL
);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryError {
    InvalidRequest,
    NotFound,
    Expired,
    Pending,
    Revoked,
    RateLimited,
    Forbidden,
    Unavailable,
}

impl From<rusqlite::Error> for RegistryError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Unavailable
    }
}

type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnrollmentState {
    Pending,
    Approved,
    Ready,
    Revoked,
}

pub(crate) struct Enrollment {
    pub id: Uuid,
    pub state: EnrollmentState,
    pub expires_at: i64,
    pub issued_at: Option<i64>,
    pub resources: Option<Vec<u8>>,
}

pub(crate) struct LoginRecord {
    pub id: Uuid,
    pub expires_at: i64,
}

pub(crate) struct BrowserAttempt {
    pub id: Uuid,
    pub installation_id: Uuid,
    pub verifier: String,
}

pub(crate) struct Registry {
    connection: Connection,
}

impl Registry {
    /// Create only an absent file in an existing owner-private, trusted directory.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or(RegistryError::Unavailable)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(parent)
            .map_err(|_| RegistryError::Unavailable)?;
        let metadata = directory
            .metadata()
            .map_err(|_| RegistryError::Unavailable)?;
        // SAFETY: geteuid takes no arguments and has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(RegistryError::Unavailable);
        }
        // Resolve system directory aliases such as macOS /var before using SQLite NOFOLLOW.
        let path = parent
            .canonicalize()
            .map_err(|_| RegistryError::Unavailable)?
            .join(path.file_name().ok_or(RegistryError::Unavailable)?);
        let created = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(_) => true,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
            Err(_) => return Err(RegistryError::Unavailable),
        };
        let mut file = pigeonmq::private_file::open_private_file(&path)
            .map_err(|_| RegistryError::Unavailable)?;
        if !created {
            // Check the SQLite file header before SQLite can open or recover another database.
            let mut header = [0; 72];
            file.read_exact(&mut header)
                .map_err(|_| RegistryError::Unavailable)?;
            if &header[..16] != b"SQLite format 3\0"
                || header[60..64] != (SCHEMA_VERSION as u32).to_be_bytes()
                || header[68..72] != (APPLICATION_ID as u32).to_be_bytes()
            {
                return Err(RegistryError::Unavailable);
            }
        }

        drop(file);
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        if created {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        } else {
            let application: i64 =
                connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let version: i64 =
                connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if application != APPLICATION_ID || version != SCHEMA_VERSION {
                return Err(RegistryError::Unavailable);
            }
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { connection })
    }

    pub(crate) fn begin(
        &mut self,
        request_id: Uuid,
        proof: &str,
        user_code: &str,
        now: i64,
    ) -> Result<Enrollment> {
        let digest = proof_digest(proof).ok_or(RegistryError::InvalidRequest)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT id, proof_digest FROM enrollments WHERE request_id = ?1",
                [request_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, expected)) = existing {
            if !bool::from(expected.as_slice().ct_eq(&digest)) {
                return Err(RegistryError::NotFound);
            }
            return unexpired(load(&tx, &id)?, now);
        }
        if request_id.get_version() != Some(uuid::Version::Random) || !valid_code(user_code) {
            return Err(RegistryError::InvalidRequest);
        }

        let pending: i64 = tx.query_row(
            "SELECT count(*) FROM enrollments WHERE state = 'pending' AND expires_at > ?1",
            [now],
            |row| row.get(0),
        )?;
        if pending >= MAX_PENDING {
            return Err(RegistryError::RateLimited);
        }
        let id = Uuid::new_v4();
        let expires_at = now
            .checked_add(PENDING_SECONDS)
            .ok_or(RegistryError::Unavailable)?;
        tx.execute(
            "INSERT INTO enrollments (id, request_id, proof_digest, code_digest, state, expires_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![
                id.to_string(),
                request_id.to_string(),
                digest.as_slice(),
                code_digest(id, user_code).as_slice(),
                expires_at
            ],
        )?;
        tx.commit()?;
        Ok(Enrollment {
            id,
            state: EnrollmentState::Pending,
            expires_at,
            issued_at: None,
            resources: None,
        })
    }

    pub(crate) fn get(&mut self, id: Uuid, proof: &str, now: i64) -> Result<Enrollment> {
        let digest = proof_digest(proof).ok_or(RegistryError::NotFound)?;
        let id = id.to_string();
        let expected: Vec<u8> = self
            .connection
            .query_row(
                "SELECT proof_digest FROM enrollments WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RegistryError::NotFound)?;
        if !bool::from(expected.as_slice().ct_eq(&digest)) {
            return Err(RegistryError::NotFound);
        }
        unexpired(load(&self.connection, &id)?, now)
    }

    /// Public browser data. No proof, resources, or account data.
    pub(crate) fn login(&mut self, id: Uuid, now: i64) -> Result<LoginRecord> {
        let enrollment = pending(load(&self.connection, &id.to_string())?, now)?;
        Ok(LoginRecord {
            id,
            expires_at: enrollment.expires_at,
        })
    }

    pub(crate) fn start_browser(
        &mut self,
        id: Uuid,
        user_code: &str,
        now: i64,
    ) -> Result<BrowserAttempt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id_text = id.to_string();
        let enrollment = pending(load(&tx, &id_text)?, now)?;
        let active: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM browser_attempts
                            WHERE installation_id = ?1 AND expires_at > ?2)",
            params![id_text, now],
            |row| row.get(0),
        )?;
        let (expected, failures): (Vec<u8>, i64) = tx.query_row(
            "SELECT code_digest, failed_guesses FROM enrollments WHERE id = ?1",
            [&id_text],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if active || failures >= MAX_GUESSES {
            return Err(RegistryError::RateLimited);
        }
        if !valid_code(user_code)
            || !bool::from(expected.as_slice().ct_eq(&code_digest(id, user_code)))
        {
            tx.execute(
                "UPDATE enrollments SET failed_guesses = failed_guesses + 1 WHERE id = ?1",
                [&id_text],
            )?;
            tx.commit()?;
            return Err(if failures + 1 >= MAX_GUESSES {
                RegistryError::RateLimited
            } else {
                RegistryError::Forbidden
            });
        }

        let attempt_id = Uuid::new_v4();
        let mut random = [0; 32];
        random[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        random[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let verifier = URL_SAFE_NO_PAD.encode(random);
        tx.execute("DELETE FROM browser_attempts WHERE expires_at <= ?1", [now])?;
        tx.execute(
            "INSERT INTO browser_attempts (id, installation_id, verifier, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                attempt_id.to_string(),
                id_text,
                verifier,
                enrollment
                    .expires_at
                    .min(now.saturating_add(BROWSER_SECONDS))
            ],
        )?;
        tx.commit()?;
        Ok(BrowserAttempt {
            id: attempt_id,
            installation_id: id,
            verifier,
        })
    }

    /// Atomically consume the verifier before attempting the provider exchange.
    pub(crate) fn take_browser(&mut self, attempt_id: Uuid, now: i64) -> Result<BrowserAttempt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt_text = attempt_id.to_string();
        let (id, verifier, expires_at): (String, Option<String>, i64) = tx
            .query_row(
                "SELECT installation_id, verifier, expires_at FROM browser_attempts WHERE id = ?1",
                [&attempt_text],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(RegistryError::NotFound)?;
        if expires_at <= now {
            return Err(RegistryError::Expired);
        }
        pending(load(&tx, &id)?, now)?;
        let verifier = verifier.ok_or(RegistryError::NotFound)?;
        let installation_id = Uuid::parse_str(&id).map_err(|_| RegistryError::Unavailable)?;
        tx.execute(
            "UPDATE browser_attempts SET verifier = NULL WHERE id = ?1",
            [&attempt_text],
        )?;
        tx.commit()?;
        Ok(BrowserAttempt {
            id: attempt_id,
            installation_id,
            verifier,
        })
    }

    /// The caller verifies the Supabase identity before passing its account ID.
    pub(crate) fn approve(
        &mut self,
        id: Uuid,
        verified_account: Uuid,
        allowed_accounts: &HashSet<Uuid>,
        now: i64,
    ) -> Result<Enrollment> {
        if !allowed_accounts.contains(&verified_account) {
            return Err(RegistryError::Forbidden);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = id.to_string();
        pending(load(&tx, &id)?, now)?;
        let consumed: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM browser_attempts
                            WHERE installation_id = ?1 AND verifier IS NULL AND expires_at > ?2)",
            params![id, now],
            |row| row.get(0),
        )?;
        if !consumed {
            return Err(RegistryError::Forbidden);
        }
        tx.execute(
            "UPDATE enrollments SET state = 'approved', account_id = ?2, code_digest = NULL
             WHERE id = ?1",
            params![id, verified_account.to_string()],
        )?;
        tx.execute(
            "DELETE FROM browser_attempts WHERE installation_id = ?1",
            [&id],
        )?;
        let enrollment = load(&tx, &id)?;
        tx.commit()?;
        Ok(enrollment)
    }

    /// Save caller-projected resource metadata. The caller must omit credentials.
    pub(crate) fn save_resources(
        &mut self,
        id: Uuid,
        resources: &[u8],
        now: i64,
    ) -> Result<Enrollment> {
        if resources.is_empty() || resources.len() > MAX_RESOURCES {
            return Err(RegistryError::InvalidRequest);
        }
        let id = id.to_string();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let enrollment = provisionable(load(&tx, &id)?, now)?;
        if enrollment.state == EnrollmentState::Ready {
            return if enrollment.resources.as_deref() == Some(resources) {
                Ok(enrollment)
            } else {
                Err(RegistryError::Unavailable)
            };
        }
        tx.execute(
            "UPDATE enrollments SET resources = ?2 WHERE id = ?1",
            params![id, resources],
        )?;
        let enrollment = load(&tx, &id)?;
        tx.commit()?;
        Ok(enrollment)
    }

    pub(crate) fn mark_ready(&mut self, id: Uuid, now: i64) -> Result<Enrollment> {
        let id = id.to_string();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let enrollment = provisionable(load(&tx, &id)?, now)?;
        if enrollment.state == EnrollmentState::Ready {
            return Ok(enrollment);
        }
        if enrollment.resources.is_none() {
            return Err(RegistryError::Unavailable);
        }
        tx.execute(
            "UPDATE enrollments SET state = 'ready', issued_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        let enrollment = load(&tx, &id)?;
        tx.commit()?;
        Ok(enrollment)
    }
}

fn valid_code(code: &str) -> bool {
    code.len() == 8
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn proof_digest(proof: &str) -> Option<[u8; 32]> {
    if proof.len() != 43 {
        return None;
    }
    let mut decoded = [0; 32];
    if URL_SAFE_NO_PAD.decode_slice(proof, &mut decoded).ok()? != decoded.len() {
        return None;
    }
    Some(Sha256::digest(decoded).into())
}

fn code_digest(id: Uuid, code: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(id.as_bytes());
    digest.update(code.as_bytes());
    digest.finalize().into()
}

fn load(connection: &Connection, id: &str) -> Result<Enrollment> {
    let (state, expires_at, issued_at, resources): (String, i64, Option<i64>, Option<Vec<u8>>) =
        connection
            .query_row(
                "SELECT state, expires_at, issued_at, resources FROM enrollments WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(RegistryError::NotFound)?;
    let state = match state.as_str() {
        "pending" => EnrollmentState::Pending,
        "approved" => EnrollmentState::Approved,
        "ready" => EnrollmentState::Ready,
        "revoked" => EnrollmentState::Revoked,
        _ => return Err(RegistryError::Unavailable),
    };
    Ok(Enrollment {
        id: Uuid::parse_str(id).map_err(|_| RegistryError::Unavailable)?,
        state,
        expires_at,
        issued_at,
        resources,
    })
}

fn unexpired(enrollment: Enrollment, now: i64) -> Result<Enrollment> {
    if enrollment.state == EnrollmentState::Pending && enrollment.expires_at <= now {
        return Err(RegistryError::Expired);
    }
    Ok(enrollment)
}

fn pending(enrollment: Enrollment, now: i64) -> Result<Enrollment> {
    match unexpired(enrollment, now)? {
        enrollment if enrollment.state == EnrollmentState::Pending => Ok(enrollment),
        enrollment if enrollment.state == EnrollmentState::Revoked => Err(RegistryError::Revoked),
        _ => Err(RegistryError::NotFound),
    }
}

fn provisionable(enrollment: Enrollment, now: i64) -> Result<Enrollment> {
    let enrollment = unexpired(enrollment, now)?;
    match enrollment.state {
        EnrollmentState::Pending => Err(RegistryError::Pending),
        EnrollmentState::Revoked => Err(RegistryError::Revoked),
        EnrollmentState::Approved | EnrollmentState::Ready => Ok(enrollment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    const CODE: &str = "ABCD1234";

    fn private_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    fn proof(byte: u8) -> String {
        URL_SAFE_NO_PAD.encode([byte; 32])
    }

    #[test]
    fn proof_mismatch_and_expired_replay_do_not_create_another_reservation() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("enrollments.sqlite")).unwrap();
        let request = Uuid::new_v4();
        let original = registry.begin(request, &proof(7), CODE, 0).unwrap();
        let retried = registry.begin(request, &proof(7), "ABCD5678", 1).unwrap();
        assert_eq!(retried.id, original.id);
        assert_eq!(retried.expires_at, original.expires_at);
        assert_eq!(
            registry.begin(request, &proof(8), CODE, 2).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry.get(original.id, &proof(8), 2).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry
                .begin(request, &proof(7), CODE, original.expires_at)
                .err(),
            Some(RegistryError::Expired)
        );
        assert_eq!(
            registry
                .get(original.id, &proof(8), original.expires_at)
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry.start_browser(original.id, "ABCD5678", 2).err(),
            Some(RegistryError::Forbidden)
        );
    }

    #[test]
    fn browser_attempt_is_single_use_and_limited_by_code_and_time() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&path).unwrap();
        let record = registry.begin(Uuid::new_v4(), &proof(7), CODE, 0).unwrap();
        let login = registry.login(record.id, 0).unwrap();
        assert_eq!(login.id, record.id);
        assert_eq!(login.expires_at, record.expires_at);
        let expired = registry.start_browser(record.id, CODE, 0).unwrap();
        assert_eq!(
            registry.start_browser(record.id, CODE, 1).err(),
            Some(RegistryError::RateLimited)
        );
        assert_eq!(
            registry.take_browser(expired.id, BROWSER_SECONDS).err(),
            Some(RegistryError::Expired)
        );
        let attempt = registry
            .start_browser(record.id, CODE, BROWSER_SECONDS)
            .unwrap();
        let taken = registry.take_browser(attempt.id, BROWSER_SECONDS).unwrap();
        assert_eq!(taken.installation_id, record.id);
        assert!(taken.verifier == attempt.verifier);
        assert_eq!(
            registry.take_browser(attempt.id, BROWSER_SECONDS).err(),
            Some(RegistryError::NotFound)
        );
        let wrong = registry.begin(Uuid::new_v4(), &proof(7), CODE, 0).unwrap();
        for _ in 0..MAX_GUESSES - 1 {
            assert_eq!(
                registry.start_browser(wrong.id, "ABCD5678", 0).err(),
                Some(RegistryError::Forbidden)
            );
        }
        drop(registry);
        let mut registry = Registry::open(&path).unwrap();
        assert_eq!(
            registry.start_browser(wrong.id, "", 1).err(),
            Some(RegistryError::RateLimited)
        );
        assert_eq!(
            registry.start_browser(wrong.id, CODE, 1).err(),
            Some(RegistryError::RateLimited)
        );
    }

    #[test]
    fn approved_record_resumes_and_first_issue_timestamp_persists() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&path).unwrap();
        let request = Uuid::new_v4();
        let account = Uuid::new_v4();
        let allowed = HashSet::from([account]);
        let original = registry.begin(request, &proof(7), CODE, 0).unwrap();
        let attempt = registry.start_browser(original.id, CODE, 0).unwrap();
        assert_eq!(
            registry.approve(original.id, account, &allowed, 1).err(),
            Some(RegistryError::Forbidden)
        );
        registry.take_browser(attempt.id, 1).unwrap();
        assert_eq!(
            registry
                .approve(original.id, Uuid::new_v4(), &allowed, 1)
                .err(),
            Some(RegistryError::Forbidden)
        );
        let approved = registry.approve(original.id, account, &allowed, 1).unwrap();
        assert_eq!(approved.state, EnrollmentState::Approved);
        let other_account = Uuid::new_v4();
        let either_account = HashSet::from([account, other_account]);
        assert_eq!(
            registry
                .approve(original.id, other_account, &either_account, 2)
                .err(),
            Some(RegistryError::NotFound)
        );
        let bound_account: String = registry
            .connection
            .query_row(
                "SELECT account_id FROM enrollments WHERE id = ?1",
                [original.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound_account, account.to_string());
        let later = original.expires_at + 1;
        let resources = br#"{"tunnel_id":"synthetic"}"#;
        registry
            .save_resources(original.id, resources, later)
            .unwrap();
        drop(registry);
        let mut registry = Registry::open(&path).unwrap();
        let retried = registry.begin(request, &proof(7), CODE, later).unwrap();
        assert_eq!(retried.id, original.id);
        assert_eq!(retried.state, EnrollmentState::Approved);
        assert_eq!(retried.resources.as_deref(), Some(resources.as_slice()));
        let ready = registry.mark_ready(original.id, later).unwrap();
        assert_eq!(ready.issued_at, Some(later));
        drop(registry);
        let mut registry = Registry::open(&path).unwrap();
        let ready = registry.mark_ready(original.id, later + 1).unwrap();
        assert_eq!(ready.state, EnrollmentState::Ready);
        assert_eq!(ready.issued_at, Some(later));
        assert_eq!(ready.resources.as_deref(), Some(resources.as_slice()));
    }

    #[test]
    fn unrelated_database_is_rejected_without_changes() {
        let home = private_home();
        let path = home.path().join("unrelated.sqlite");
        fs::File::create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Registry::open(&path).err(),
            Some(RegistryError::Unavailable)
        );
        assert!(fs::read(&path).unwrap().is_empty());
        drop(pigeonmq::Queue::open(&path).unwrap());
        let original = fs::read(&path).unwrap();
        assert_eq!(
            Registry::open(&path).err(),
            Some(RegistryError::Unavailable)
        );
        assert!(fs::read(&path).unwrap() == original);
    }

    #[test]
    fn pending_cap_does_not_change_replays_or_count_expired_reservations() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("enrollments.sqlite")).unwrap();
        let request = Uuid::new_v4();
        let original = registry.begin(request, &proof(7), CODE, 0).unwrap();
        for _ in 1..MAX_PENDING {
            registry.begin(Uuid::new_v4(), &proof(7), CODE, 0).unwrap();
        }
        assert_eq!(
            registry.begin(Uuid::new_v4(), &proof(7), CODE, 1).err(),
            Some(RegistryError::RateLimited)
        );
        assert_eq!(
            registry.begin(request, &proof(7), CODE, 1).unwrap().id,
            original.id
        );
        assert!(
            registry
                .begin(Uuid::new_v4(), &proof(7), CODE, original.expires_at)
                .is_ok()
        );
    }
}
