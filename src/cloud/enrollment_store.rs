//! Private operator state for hosted registration.
use super::hosted_origin::UnixOrigin;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const APPLICATION_ID: i64 = 0x504D_5145; // PMQE
const SCHEMA_VERSION: i64 = 6;
pub(crate) const PENDING_SECONDS: i64 = 15 * 60;
const BROWSER_SECONDS: i64 = 5 * 60;
pub(crate) const UNOPENED_SECONDS: i64 = 12 * 60 * 60;
const MAX_PENDING: i64 = 32;
pub(crate) const DEFAULT_HOSTED_CAPACITY: u32 = 32;
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

const ADD_LIFECYCLE: &str = "
ALTER TABLE enrollments ADD COLUMN removal TEXT CHECK (
    removal IS NULL OR (state = 'ready' AND removal = 'expiring')
        OR (state = 'revoked' AND removal IN ('revoking', 'expired', 'revoked'))
);
ALTER TABLE enrollments ADD COLUMN ever_connected INTEGER NOT NULL DEFAULT 0
    CHECK (ever_connected IN (0, 1));
UPDATE enrollments SET removal = 'revoking' WHERE state = 'revoked';
";

const ADD_ORIGIN: &str = "
CREATE TABLE enrollment_origins (
    installation_id TEXT PRIMARY KEY REFERENCES enrollments(id),
    origin TEXT NOT NULL
);
CREATE UNIQUE INDEX enrollment_unix_origin ON enrollment_origins(origin) WHERE origin <> 'tcp';";

const ADD_EMAIL_VERIFICATION: &str = "
ALTER TABLE enrollments ADD COLUMN email_verified INTEGER NOT NULL DEFAULT 0
    CHECK (email_verified IN (0, 1));
UPDATE enrollments SET email_verified = 1 WHERE account_id IS NOT NULL;
ALTER TABLE enrollments DROP COLUMN account_id;
DROP TABLE browser_attempts;
CREATE TABLE browser_attempts (
    id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL UNIQUE REFERENCES enrollments(id),
    email_digest BLOB NOT NULL CHECK (length(email_digest) = 32),
    code_digest BLOB CHECK (code_digest IS NULL OR length(code_digest) = 32),
    sent INTEGER NOT NULL DEFAULT 0 CHECK (sent IN (0, 1)),
    expires_at INTEGER NOT NULL
);
";

const ADD_ISSUANCE_NOTICE: &str = "
ALTER TABLE enrollments ADD COLUMN first_issue_seen INTEGER NOT NULL DEFAULT 0
    CHECK (first_issue_seen IN (0, 1));
ALTER TABLE enrollments ADD COLUMN issuance_notice_at INTEGER
    CHECK (issuance_notice_at IS NULL OR first_issue_seen = 1);
UPDATE enrollments SET first_issue_seen = 1 WHERE issued_at IS NOT NULL OR ever_connected = 1;
";

const ADD_HOSTED_ADMISSION: &str = "
ALTER TABLE enrollments ADD COLUMN active_email_digest BLOB
    CHECK (active_email_digest IS NULL OR length(active_email_digest) = 32);
CREATE UNIQUE INDEX enrollment_active_email ON enrollments(active_email_digest)
    WHERE active_email_digest IS NOT NULL;
ALTER TABLE browser_attempts ADD COLUMN blocked TEXT
    CHECK (blocked IS NULL OR (blocked IN ('active_address', 'hosted_capacity') AND code_digest IS NULL));
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryError {
    InvalidRequest,
    NotFound,
    Expired,
    Pending,
    Revoked,
    RateLimited,
    ActiveAddress,
    HostedCapacity,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecyclePhase {
    Pending,
    Approved,
    Ready,
    Expiring,
    Revoking,
    Expired,
    Revoked,
}

impl LifecyclePhase {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Ready => "ready",
            Self::Expiring => "expiring",
            Self::Revoking => "revoking",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

pub(crate) struct LifecycleRecord {
    pub id: Uuid,
    pub phase: LifecyclePhase,
    pub resources: Option<Vec<u8>>,
    pub ever_connected: bool,
}

pub(crate) struct Enrollment {
    pub id: Uuid,
    pub state: EnrollmentState,
    pub expires_at: i64,
    pub issued_at: Option<i64>,
    pub resources: Option<Vec<u8>>,
    pub phase: LifecyclePhase,
    has_email_claim: bool,
}

pub(crate) struct LoginRecord {
    pub id: Uuid,
    pub expires_at: i64,
}

pub(crate) struct BrowserAttempt {
    pub id: Uuid,
    pub code: String,
}

pub(crate) struct Registry {
    connection: Connection,
}

impl Registry {
    fn private_path(path: &Path) -> Result<PathBuf> {
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
        Ok(parent
            .canonicalize()
            .map_err(|_| RegistryError::Unavailable)?
            .join(path.file_name().ok_or(RegistryError::Unavailable)?))
    }

    fn verify_header(path: &Path) -> Result<()> {
        let mut file = radio999k::private_file::open_private_file(path)
            .map_err(|_| RegistryError::Unavailable)?;
        let mut header = [0; 72];
        file.read_exact(&mut header)
            .map_err(|_| RegistryError::Unavailable)?;
        let version = u32::from_be_bytes([header[60], header[61], header[62], header[63]]);
        if &header[..16] != b"SQLite format 3\0"
            || !(1..=SCHEMA_VERSION as u32).contains(&version)
            || header[68..72] != (APPLICATION_ID as u32).to_be_bytes()
        {
            return Err(RegistryError::Unavailable);
        }
        Ok(())
    }

    pub(crate) fn open_for_inspection(path: &Path) -> Result<Self> {
        let path = Self::private_path(path)?;
        Self::verify_header(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let application: i64 =
            connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != APPLICATION_ID || !(2..=SCHEMA_VERSION).contains(&version) {
            return Err(RegistryError::Unavailable);
        }
        Ok(Self { connection })
    }

    /// Create only an absent file in an existing owner-private, trusted directory.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let path = Self::private_path(path)?;
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
        if created {
            radio999k::private_file::open_private_file(&path)
                .map_err(|_| RegistryError::Unavailable)?;
        } else {
            // Check the SQLite file header before SQLite can open or recover another database.
            Self::verify_header(&path)?;
        }

        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "secure_delete", "ON")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if created {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(ADD_LIFECYCLE)?;
            tx.execute_batch(ADD_ORIGIN)?;
            tx.execute_batch(ADD_EMAIL_VERIFICATION)?;
            tx.execute_batch(ADD_ISSUANCE_NOTICE)?;
            tx.execute_batch(ADD_HOSTED_ADMISSION)?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else {
            let application: i64 =
                tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if application != APPLICATION_ID || !(1..=SCHEMA_VERSION).contains(&version) {
                return Err(RegistryError::Unavailable);
            }
            if version == 1 {
                tx.execute_batch(ADD_LIFECYCLE)?;
            }
            if version < 3 {
                tx.execute_batch(ADD_ORIGIN)?;
            }
            if version < 4 {
                tx.execute_batch(ADD_EMAIL_VERIFICATION)?;
            }
            if version < 5 {
                tx.execute_batch(ADD_ISSUANCE_NOTICE)?;
            }
            if version < 6 {
                tx.execute_batch(ADD_HOSTED_ADMISSION)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
        }
        tx.commit()?;
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
            phase: LifecyclePhase::Pending,
            has_email_claim: false,
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
        let enrollment = unexpired(load(&self.connection, &id)?, now)?;
        if enrollment.state == EnrollmentState::Pending {
            let blocked: Option<String> = self.connection.query_row(
                "SELECT blocked FROM browser_attempts WHERE installation_id = ?1 AND expires_at > ?2 AND blocked IS NOT NULL",
                params![id, now],
                |row| row.get(0),
            ).optional()?;
            match blocked.as_deref() {
                Some("active_address") => return Err(RegistryError::ActiveAddress),
                Some("hosted_capacity") => return Err(RegistryError::HostedCapacity),
                _ => {}
            }
        }
        Ok(enrollment)
    }

    /// Pin one origin under the verified installation before any provider writes.
    pub(crate) fn bind_origin(
        &mut self,
        id: Uuid,
        proof: &str,
        now: i64,
        unix: Option<&UnixOrigin>,
    ) -> Result<()> {
        let digest = proof_digest(proof).ok_or(RegistryError::NotFound)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = id.to_string();
        let expected: Vec<u8> = tx
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
        let record = provisionable(load(&tx, &id)?, now)?;
        let origin = unix.map_or("tcp", UnixOrigin::nonce);
        let prior: Option<String> = tx
            .query_row(
                "SELECT origin FROM enrollment_origins WHERE installation_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            return if prior == origin {
                Ok(())
            } else {
                Err(RegistryError::InvalidRequest)
            };
        }
        if unix.is_some()
            && (record.resources.is_some() || record.state != EnrollmentState::Approved)
        {
            return Err(RegistryError::InvalidRequest);
        }
        if unix.is_some()
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM enrollment_origins WHERE origin = ?1)",
                [origin],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(RegistryError::InvalidRequest);
        }
        tx.execute(
            "INSERT INTO enrollment_origins (installation_id, origin) VALUES (?1, ?2)",
            params![id, origin],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Public browser data. No proof, resources, or account data.
    pub(crate) fn login(&mut self, id: Uuid, now: i64) -> Result<LoginRecord> {
        let enrollment = pending(load(&self.connection, &id.to_string())?, now)?;
        Ok(LoginRecord {
            id,
            expires_at: enrollment.expires_at,
        })
    }

    /// An authenticated installer retry can reopen only its own inactive pending enrollment.
    pub(crate) fn handoff_url_secret(
        &mut self,
        id: Uuid,
        proof: &str,
        now: i64,
    ) -> Result<Option<String>> {
        let digest = proof_digest(proof).ok_or(RegistryError::NotFound)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let text = id.to_string();
        let (expected, generation): (Vec<u8>, Option<Vec<u8>>) = tx
            .query_row(
                "SELECT proof_digest, code_digest FROM enrollments WHERE id = ?1",
                [&text],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(RegistryError::NotFound)?;
        if !bool::from(expected.as_slice().ct_eq(&digest)) {
            return Err(RegistryError::NotFound);
        }
        let record = unexpired(load(&tx, &text)?, now)?;
        if record.state != EnrollmentState::Pending {
            return Ok(None);
        }
        let generation = match generation {
            Some(value) => value,
            None => {
                let active: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM browser_attempts WHERE installation_id = ?1 AND expires_at > ?2)",
                    params![text, now],
                    |row| row.get(0),
                )?;
                if active {
                    return Ok(None);
                }
                let value = Uuid::new_v4().as_bytes().to_vec();
                tx.execute(
                    "UPDATE enrollments SET code_digest = ?2 WHERE id = ?1",
                    params![text, value],
                )?;
                value
            }
        };
        let handoff =
            browser_handoff(id, &digest, &generation).ok_or(RegistryError::Unavailable)?;
        tx.commit()?;
        Ok(Some(handoff))
    }

    pub(crate) fn start_handoff_attempt(
        &mut self,
        id: Uuid,
        handoff: &str,
        email: &str,
        key: &[u8; 32],
        now: i64,
    ) -> Result<BrowserAttempt> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id_text = id.to_string();
        let enrollment = pending(load(&tx, &id_text)?, now)?;
        let (generation, proof): (Option<Vec<u8>>, Vec<u8>) = tx.query_row(
            "SELECT code_digest, proof_digest FROM enrollments WHERE id = ?1",
            [&id_text],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let generation = generation.ok_or(RegistryError::NotFound)?;
        if !browser_handoff(id, &proof, &generation)
            .is_some_and(|expected| bool::from(expected.as_bytes().ct_eq(handoff.as_bytes())))
        {
            return Err(RegistryError::NotFound);
        }

        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 32-byte key");
        mac.update(b"999k-email-throttle-v1\0");
        mac.update(email.as_bytes());
        let email_digest = mac.finalize().into_bytes();
        let active: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM browser_attempts
                            WHERE (installation_id = ?1 OR email_digest = ?2) AND expires_at > ?3)",
            params![id_text, &email_digest[..], now],
            |row| row.get(0),
        )?;
        if active {
            return Err(RegistryError::RateLimited);
        }

        let attempt_id = Uuid::new_v4();
        let code = new_otp();
        let digest = otp_digest(key, id, attempt_id, &code);
        tx.execute("DELETE FROM browser_attempts WHERE expires_at <= ?1", [now])?;
        tx.execute(
            "INSERT INTO browser_attempts (id, installation_id, email_digest, code_digest, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                attempt_id.to_string(),
                id_text,
                &email_digest[..],
                &digest[..],
                enrollment.expires_at.min(now.saturating_add(BROWSER_SECONDS)),
            ],
        )?;
        tx.execute(
            "UPDATE enrollments SET code_digest = NULL WHERE id = ?1",
            [&id_text],
        )?;
        tx.commit()?;
        Ok(BrowserAttempt {
            id: attempt_id,
            code,
        })
    }

    pub(crate) fn mark_email_sent(
        &mut self,
        attempt: Uuid,
        installation: Uuid,
        now: i64,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE browser_attempts SET sent = 1
             WHERE id = ?1 AND installation_id = ?2 AND expires_at > ?3 AND sent = 0",
            params![attempt.to_string(), installation.to_string(), now],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(RegistryError::NotFound)
        }
    }

    pub(crate) fn expire_email_attempts(&mut self, now: i64) -> Result<()> {
        self.connection
            .execute("DELETE FROM browser_attempts WHERE expires_at <= ?1", [now])?;
        Ok(())
    }

    pub(crate) fn expire_unstarted_approvals(&mut self, now: i64) -> Result<()> {
        expire_unstarted_approvals(&self.connection, now)
    }

    pub(crate) fn verify_email_code(
        &mut self,
        id: Uuid,
        attempt: Uuid,
        code: &str,
        key: &[u8; 32],
        now: i64,
        capacity: u32,
    ) -> Result<Enrollment> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id_text = id.to_string();
        let attempt_text = attempt.to_string();
        let (expected, expires_at, email_digest): (Vec<u8>, i64, Vec<u8>) = tx
            .query_row(
                "SELECT code_digest, expires_at, email_digest FROM browser_attempts
                 WHERE id = ?1 AND installation_id = ?2 AND sent = 1 AND code_digest IS NOT NULL",
                params![attempt_text, id_text],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(RegistryError::NotFound)?;
        if expires_at <= now {
            return Err(RegistryError::Expired);
        }
        pending(load(&tx, &id_text)?, now)?;
        let digest = otp_digest(key, id, attempt, code);
        if !bool::from(expected.as_slice().ct_eq(&digest)) {
            tx.execute(
                "UPDATE browser_attempts SET code_digest = NULL WHERE id = ?1",
                [&attempt_text],
            )?;
            tx.commit()?;
            return Err(RegistryError::NotFound);
        }
        expire_unstarted_approvals(&tx, now)?;
        let address_exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM enrollments WHERE active_email_digest = ?1)",
            [&email_digest],
            |row| row.get(0),
        )?;
        let allocated: u32 = tx.query_row(
            "SELECT count(*) FROM enrollments WHERE state IN ('approved', 'ready') OR removal = 'revoking'",
            [],
            |row| row.get(0),
        )?;
        let rejection = if address_exists {
            Some(("active_address", RegistryError::ActiveAddress))
        } else if allocated >= capacity {
            Some(("hosted_capacity", RegistryError::HostedCapacity))
        } else {
            None
        };
        if let Some((reason, error)) = rejection {
            tx.execute(
                "UPDATE browser_attempts SET code_digest = NULL, blocked = ?2 WHERE id = ?1",
                params![attempt_text, reason],
            )?;
            tx.commit()?;
            return Err(error);
        }
        tx.execute(
            "UPDATE enrollments SET state = 'approved', email_verified = 1, code_digest = NULL,
                    active_email_digest = ?2
             WHERE id = ?1",
            params![id_text, email_digest],
        )?;
        tx.execute(
            "DELETE FROM browser_attempts WHERE id = ?1",
            [&attempt_text],
        )?;
        let enrollment = load(&tx, &id_text)?;
        tx.commit()?;
        Ok(enrollment)
    }

    #[cfg(test)]
    pub(crate) fn approve_for_test(
        &mut self,
        id: Uuid,
        proof: &str,
        now: i64,
    ) -> Result<Enrollment> {
        let handoff = self
            .handoff_url_secret(id, proof, now)?
            .ok_or(RegistryError::NotFound)?;
        let key = [42; 32];
        let email = format!("fixture+{id}@example.test");
        let attempt = self.start_handoff_attempt(id, &handoff, &email, &key, now)?;
        self.mark_email_sent(attempt.id, id, now)?;
        self.verify_email_code(
            id,
            attempt.id,
            &attempt.code,
            &key,
            now,
            DEFAULT_HOSTED_CAPACITY,
        )
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

    pub(crate) fn update_ready_resources(
        &mut self,
        id: Uuid,
        previous: &[u8],
        resources: &[u8],
    ) -> Result<()> {
        if resources.is_empty() || resources.len() > MAX_RESOURCES {
            return Err(RegistryError::InvalidRequest);
        }
        let changed = self.connection.execute(
            "UPDATE enrollments SET resources = ?3 WHERE id = ?1 AND state = 'ready' AND removal IS NULL AND resources = ?2",
            params![id.to_string(), previous, resources],
        )?;
        if changed != 1 {
            return Err(RegistryError::Unavailable);
        }
        Ok(())
    }

    pub(crate) fn mark_ready(&mut self, id: Uuid, now: i64, notify: bool) -> Result<Enrollment> {
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
            "UPDATE enrollments SET state = 'ready', issued_at = ?2,
                 issuance_notice_at = CASE WHEN first_issue_seen = 0 AND ?3 THEN ?2 ELSE issuance_notice_at END,
                 first_issue_seen = 1 WHERE id = ?1",
            params![id, now, notify],
        )?;
        let enrollment = load(&tx, &id)?;
        tx.commit()?;
        Ok(enrollment)
    }

    pub(crate) fn issuance_notices(&self) -> Result<Vec<(Uuid, i64)>> {
        let mut query = self.connection.prepare(
            "SELECT id, issuance_notice_at FROM enrollments WHERE issuance_notice_at IS NOT NULL
             ORDER BY issuance_notice_at, id",
        )?;
        let mut rows = query.query([])?;
        let mut notices = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let id = Uuid::parse_str(&id).map_err(|_| RegistryError::Unavailable)?;
            notices.push((id, row.get(1)?));
        }
        Ok(notices)
    }

    pub(crate) fn take_issuance_notice(&mut self, id: Uuid, issued_at: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE enrollments SET issuance_notice_at = NULL WHERE id = ?1 AND issuance_notice_at = ?2",
            params![id.to_string(), issued_at],
        )? == 1)
    }

    pub(crate) fn retry_issuance_notice(&mut self, id: Uuid, issued_at: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE enrollments SET issuance_notice_at = ?2 WHERE id = ?1 AND issuance_notice_at IS NULL",
            params![id.to_string(), issued_at],
        )?;
        require_change(changed)
    }

    pub(crate) fn lifecycle(&self, id: Uuid) -> Result<LifecycleRecord> {
        load_lifecycle(&self.connection, &id.to_string())
    }

    pub(crate) fn list_lifecycle(&self) -> Result<Vec<LifecycleRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT id, state, removal, resources, ever_connected FROM enrollments ORDER BY id",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(lifecycle_row(row)?);
        }
        Ok(records)
    }

    pub(crate) fn lifecycle_candidates(&self, now: i64) -> Result<Vec<LifecycleRecord>> {
        let mut candidates = self.connection.prepare(
            "SELECT id, state, removal, resources, ever_connected FROM enrollments
             WHERE removal IN ('revoking', 'expiring')
                OR (state = 'ready' AND ever_connected = 0 AND issued_at <= ?1)
             ORDER BY issued_at, id",
        )?;
        let mut rows = candidates.query([now.checked_sub(UNOPENED_SECONDS)])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(lifecycle_row(row)?);
        }
        Ok(records)
    }

    pub(crate) fn request_hosted_reset(&mut self, cutoff: i64) -> Result<usize> {
        Ok(self.connection.execute(
            "UPDATE enrollments SET state = 'revoked', removal = 'revoking'
             WHERE state = 'ready' AND resources IS NOT NULL AND issued_at < ?1",
            [cutoff],
        )?)
    }

    pub(crate) fn request_revoke(&mut self, id: Uuid) -> Result<LifecycleRecord> {
        self.change_lifecycle(id, |tx, id, record| {
            if !matches!(
                record.phase,
                LifecyclePhase::Revoking | LifecyclePhase::Expired | LifecyclePhase::Revoked
            ) {
                // Provisioning persists its first create attempt before changing the provider.
                let removal = if matches!(
                    record.phase,
                    LifecyclePhase::Pending | LifecyclePhase::Approved
                ) && record.resources.is_none()
                {
                    "revoked"
                } else {
                    "revoking"
                };
                tx.execute(
                    "UPDATE enrollments SET state = 'revoked', removal = ?2,
                         active_email_digest = CASE WHEN ?2 = 'revoking' THEN active_email_digest END
                     WHERE id = ?1",
                    params![id, removal],
                )?;
            }
            Ok(())
        })
    }

    /// Call after a current provider check reports the tunnel has never connected.
    pub(crate) fn request_expiry(&mut self, id: Uuid, now: i64) -> Result<LifecycleRecord> {
        self.change_lifecycle(id, |tx, id, record| {
            if record.phase == LifecyclePhase::Expiring {
                return Ok(());
            }
            let changed = tx.execute(
                "UPDATE enrollments SET removal = 'expiring'
                 WHERE id = ?1 AND state = 'ready' AND resources IS NOT NULL
                   AND ever_connected = 0 AND issued_at <= ?2",
                params![id, now.checked_sub(UNOPENED_SECONDS)],
            )?;
            require_change(changed)
        })
    }

    /// Call only when the provider confirms all resources for this installation are gone.
    pub(crate) fn complete_removal(
        &mut self,
        id: Uuid,
        expected: LifecyclePhase,
    ) -> Result<LifecycleRecord> {
        let removal = match expected {
            LifecyclePhase::Expiring => "expired",
            LifecyclePhase::Revoking => "revoked",
            _ => return Err(RegistryError::InvalidRequest),
        };
        self.change_lifecycle(id, |tx, id, record| {
            if record.phase != expected {
                return Err(RegistryError::Unavailable);
            }
            tx.execute(
                "UPDATE enrollments SET state = 'revoked', removal = ?2, active_email_digest = NULL WHERE id = ?1",
                params![id, removal],
            )?;
            Ok(())
        })
    }

    /// Also clears a pending expiry if the provider still reports a prior connection.
    pub(crate) fn retain_connected(
        &mut self,
        id: Uuid,
        expected: LifecyclePhase,
    ) -> Result<LifecycleRecord> {
        if !matches!(expected, LifecyclePhase::Ready | LifecyclePhase::Expiring) {
            return Err(RegistryError::InvalidRequest);
        }
        self.change_lifecycle(id, |tx, id, record| {
            if record.phase != expected {
                return Err(RegistryError::Unavailable);
            }
            tx.execute(
                "UPDATE enrollments SET state = 'ready', removal = NULL, ever_connected = 1
                 WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
    }

    pub(crate) fn request_reissue(&mut self, id: Uuid) -> Result<LifecycleRecord> {
        self.change_lifecycle(id, |tx, id, _| {
            let changed = tx.execute(
                "UPDATE enrollments SET state = 'approved', removal = NULL,
                     resources = NULL, issued_at = NULL
                 WHERE id = ?1 AND email_verified = 1 AND removal IN ('revoked', 'expired')",
                [id],
            )?;
            require_change(changed)
        })
    }

    fn change_lifecycle(
        &mut self,
        id: Uuid,
        change: impl FnOnce(&Connection, &str, &LifecycleRecord) -> Result<()>,
    ) -> Result<LifecycleRecord> {
        let id = id.to_string();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = load_lifecycle(&tx, &id)?;
        change(&tx, &id, &old)?;
        let new = load_lifecycle(&tx, &id)?;
        tx.commit()?;
        Ok(new)
    }
}

fn new_otp() -> String {
    const SPACE: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - u32::MAX % SPACE;
    loop {
        let random = Uuid::new_v4();
        let bytes = random.as_bytes();
        let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if value < LIMIT {
            return format!("{:06}", value % SPACE);
        }
    }
}

fn otp_digest(key: &[u8; 32], id: Uuid, attempt: Uuid, code: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a 32-byte key");
    mac.update(b"999k-email-code-v1\0");
    mac.update(id.as_bytes());
    mac.update(attempt.as_bytes());
    mac.update(code.as_bytes());
    mac.finalize().into_bytes().into()
}

fn require_change(changed: usize) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(RegistryError::Unavailable)
    }
}

fn load_lifecycle(connection: &Connection, id: &str) -> Result<LifecycleRecord> {
    let mut statement = connection.prepare(
        "SELECT id, state, removal, resources, ever_connected FROM enrollments WHERE id = ?1",
    )?;
    let mut rows = statement.query([id])?;
    lifecycle_row(rows.next()?.ok_or(RegistryError::NotFound)?)
}

fn lifecycle_row(row: &rusqlite::Row<'_>) -> Result<LifecycleRecord> {
    let id: String = row.get(0)?;
    let state: String = row.get(1)?;
    let removal: Option<String> = row.get(2)?;
    Ok(LifecycleRecord {
        id: Uuid::parse_str(&id).map_err(|_| RegistryError::Unavailable)?,
        phase: lifecycle_phase(&state, removal.as_deref())?,
        resources: row.get(3)?,
        ever_connected: row.get(4)?,
    })
}

fn lifecycle_phase(state: &str, removal: Option<&str>) -> Result<LifecyclePhase> {
    let phase = match (state, removal) {
        ("pending", None) => LifecyclePhase::Pending,
        ("approved", None) => LifecyclePhase::Approved,
        ("ready", None) => LifecyclePhase::Ready,
        ("ready", Some("expiring")) => LifecyclePhase::Expiring,
        ("revoked", Some("revoking")) => LifecyclePhase::Revoking,
        ("revoked", Some("expired")) => LifecyclePhase::Expired,
        ("revoked", Some("revoked")) => LifecyclePhase::Revoked,
        _ => return Err(RegistryError::Unavailable),
    };
    Ok(phase)
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

// Domain separation keeps this capability distinct from the installer proof.
fn browser_handoff(id: Uuid, digest: &[u8], generation: &[u8]) -> Option<String> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(digest).ok()?;
    mac.update(b"pigeonmq/install-browser-handoff/v1");
    mac.update(id.as_bytes());
    mac.update(generation);
    Some(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn code_digest(id: Uuid, code: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(id.as_bytes());
    digest.update(code.as_bytes());
    digest.finalize().into()
}

fn load(connection: &Connection, id: &str) -> Result<Enrollment> {
    let (state, expires_at, issued_at, resources, removal, has_email_claim) = connection
        .query_row(
            "SELECT state, expires_at, issued_at, resources, removal, active_email_digest IS NOT NULL
             FROM enrollments WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(RegistryError::NotFound)?;
    let phase = lifecycle_phase(&state, removal.as_deref())?;
    let state = match phase {
        LifecyclePhase::Pending => EnrollmentState::Pending,
        LifecyclePhase::Approved => EnrollmentState::Approved,
        LifecyclePhase::Ready | LifecyclePhase::Expiring => EnrollmentState::Ready,
        LifecyclePhase::Expired | LifecyclePhase::Revoking | LifecyclePhase::Revoked => {
            EnrollmentState::Revoked
        }
    };
    Ok(Enrollment {
        id: Uuid::parse_str(id).map_err(|_| RegistryError::Unavailable)?,
        state,
        expires_at,
        issued_at,
        resources,
        phase,
        has_email_claim,
    })
}

fn expire_unstarted_approvals(connection: &Connection, now: i64) -> Result<()> {
    connection.execute(
        "UPDATE enrollments SET state = 'revoked', removal = 'expired', active_email_digest = NULL
         WHERE state = 'approved' AND resources IS NULL AND active_email_digest IS NOT NULL AND expires_at <= ?1",
        [now],
    )?;
    Ok(())
}

fn unexpired(enrollment: Enrollment, now: i64) -> Result<Enrollment> {
    let waiting_for_resources = enrollment.resources.is_none()
        && (enrollment.has_email_claim || enrollment.phase == LifecyclePhase::Expired);
    if (enrollment.state == EnrollmentState::Pending || waiting_for_resources)
        && enrollment.expires_at <= now
    {
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
    if enrollment.phase == LifecyclePhase::Expiring {
        return Err(RegistryError::Unavailable);
    }
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
    const OTP_KEY: [u8; 32] = [42; 32];

    fn private_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    fn proof(byte: u8) -> String {
        URL_SAFE_NO_PAD.encode([byte; 32])
    }

    fn approved(registry: &mut Registry) -> Uuid {
        let enrollment = registry.begin(Uuid::new_v4(), &proof(7), CODE, 0).unwrap();
        registry
            .approve_for_test(enrollment.id, &proof(7), 1)
            .unwrap();
        enrollment.id
    }

    fn verify_address(
        registry: &mut Registry,
        email: &str,
        now: i64,
        capacity: u32,
    ) -> (Uuid, Result<Enrollment>) {
        let proof = proof(7);
        let id = registry
            .begin(Uuid::new_v4(), &proof, CODE, now)
            .unwrap()
            .id;
        let handoff = registry
            .handoff_url_secret(id, &proof, now)
            .unwrap()
            .unwrap();
        let attempt = registry
            .start_handoff_attempt(id, &handoff, email, &OTP_KEY, now)
            .unwrap();
        registry.mark_email_sent(attempt.id, id, now).unwrap();
        (
            id,
            registry.verify_email_code(id, attempt.id, &attempt.code, &OTP_KEY, now, capacity),
        )
    }

    #[test]
    fn verified_email_and_capacity_stay_claimed_until_provider_removal() {
        let home = private_home();
        let path = home.path().join("registry.db");
        let mut registry = Registry::open(&path).unwrap();
        let email = "pilot@example.test";
        let (first, admitted) = verify_address(&mut registry, email, 0, 1);
        assert!(admitted.is_ok());
        registry
            .save_resources(first, b"fixture resource", 0)
            .unwrap();
        registry.mark_ready(first, 0, false).unwrap();
        drop(registry);

        let mut registry = Registry::open(&path).unwrap();
        let (duplicate, denied) = verify_address(&mut registry, email, 1, 1);
        assert_eq!(denied.err(), Some(RegistryError::ActiveAddress));
        assert_eq!(
            registry.get(duplicate, &proof(8), 1).err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry.get(duplicate, &proof(7), 1).err(),
            Some(RegistryError::ActiveAddress)
        );
        assert_eq!(
            verify_address(&mut registry, "other@example.test", 1, 1)
                .1
                .err(),
            Some(RegistryError::HostedCapacity)
        );

        assert_eq!(registry.request_hosted_reset(1).unwrap(), 1);
        assert_eq!(
            verify_address(&mut registry, email, BROWSER_SECONDS + 1, 1)
                .1
                .err(),
            Some(RegistryError::ActiveAddress)
        );
        registry
            .complete_removal(first, LifecyclePhase::Revoking)
            .unwrap();
        assert!(
            verify_address(&mut registry, email, 2 * BROWSER_SECONDS + 1, 1)
                .1
                .is_ok()
        );
    }

    #[test]
    fn an_abandoned_approval_releases_its_claim_without_allowing_a_late_finish() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("registry.db")).unwrap();
        let email = "pilot@example.test";
        let (first, admitted) = verify_address(&mut registry, email, 0, 1);
        assert!(admitted.is_ok());
        assert_eq!(
            registry
                .save_resources(first, b"too late", PENDING_SECONDS)
                .err(),
            Some(RegistryError::Expired)
        );
        assert!(
            verify_address(&mut registry, email, PENDING_SECONDS, 1)
                .1
                .is_ok()
        );
        assert_eq!(
            registry.get(first, &proof(7), PENDING_SECONDS).err(),
            Some(RegistryError::Expired)
        );
    }

    #[test]
    fn address_reissue_does_not_queue_a_second_first_issuance_notice() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("registry.db")).unwrap();
        let id = approved(&mut registry);
        registry.save_resources(id, b"old resource", 80).unwrap();
        registry.mark_ready(id, 80, true).unwrap();
        registry.mark_ready(id, 81, true).unwrap();
        assert_eq!(registry.issuance_notices().unwrap(), vec![(id, 80)]);
        assert!(registry.take_issuance_notice(id, 80).unwrap());
        assert!(!registry.take_issuance_notice(id, 80).unwrap());
        registry.request_revoke(id).unwrap();
        registry
            .complete_removal(id, LifecyclePhase::Revoking)
            .unwrap();
        registry.request_reissue(id).unwrap();
        registry.save_resources(id, b"new resource", 90).unwrap();
        registry.mark_ready(id, 90, true).unwrap();
        assert!(registry.issuance_notices().unwrap().is_empty());
    }

    #[test]
    fn inspection_lists_existing_records_without_creating_or_changing_the_database() {
        let home = private_home();
        let path = home.path().join("registry.db");
        assert!(Registry::open_for_inspection(&path).is_err());
        assert!(!path.exists());
        let mut registry = Registry::open(&path).unwrap();
        let id = approved(&mut registry);
        let before = fs::read(&path).unwrap();
        let mut inspection = Registry::open_for_inspection(&path).unwrap();
        assert_eq!(inspection.list_lifecycle().unwrap()[0].id, id);
        assert_eq!(
            inspection.lifecycle(id).unwrap().phase,
            LifecyclePhase::Approved
        );
        assert!(inspection.request_revoke(id).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn migrates_schema_two_and_preserves_ready_and_pending_registrations() {
        let home = private_home();
        let path = home.path().join("registry.db");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection.execute_batch(ADD_LIFECYCLE).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        let id = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO enrollments
             (id, request_id, proof_digest, state, expires_at, account_id, issued_at, resources)
             VALUES (?1, ?2, ?3, 'ready', 901, ?4, 3, ?5)",
                params![
                    id.to_string(),
                    Uuid::new_v4().to_string(),
                    proof_digest(&proof(7)).unwrap().as_slice(),
                    Uuid::new_v4().to_string(),
                    b"original-resources".as_slice(),
                ],
            )
            .unwrap();
        let pending = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO enrollments (id, request_id, proof_digest, state, expires_at)
                 VALUES (?1, ?2, ?3, 'pending', 901)",
                params![
                    pending.to_string(),
                    Uuid::new_v4().to_string(),
                    proof_digest(&proof(8)).unwrap().as_slice(),
                ],
            )
            .unwrap();
        let old_attempt = Uuid::new_v4();
        connection
            .execute(
                "INSERT INTO browser_attempts (id, installation_id, verifier, expires_at)
             VALUES (?1, ?2, 'email:old@example.test', 300)",
                params![old_attempt.to_string(), pending.to_string()],
            )
            .unwrap();
        drop(connection);

        let mut registry = Registry::open(&path).unwrap();
        let record = registry.get(id, &proof(7), 4).unwrap();
        assert_eq!(record.state, EnrollmentState::Ready);
        assert_eq!(
            record.resources.as_deref(),
            Some(b"original-resources".as_slice())
        );
        assert_eq!(record.issued_at, Some(3));
        let nonce = UnixOrigin::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            registry.bind_origin(id, &proof(7), 4, Some(&nonce)),
            Err(RegistryError::InvalidRequest)
        );
        registry.bind_origin(id, &proof(7), 4, None).unwrap();
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(b"old@example.test".len())
                .any(|bytes| bytes == b"old@example.test")
        );
        assert_eq!(
            registry
                .connection
                .query_row(
                    "SELECT email_verified FROM enrollments WHERE id = ?1",
                    [id.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            registry
                .verify_email_code(
                    pending,
                    old_attempt,
                    "123456",
                    &OTP_KEY,
                    4,
                    DEFAULT_HOSTED_CAPACITY
                )
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry.approve_for_test(pending, &proof(8), 4).unwrap().id,
            pending
        );
        registry.request_revoke(id).unwrap();
        registry
            .complete_removal(id, LifecyclePhase::Revoking)
            .unwrap();
        assert_eq!(
            registry.request_reissue(id).unwrap().phase,
            LifecyclePhase::Approved
        );
    }

    #[test]
    fn protects_immutable_unix_origins_across_restart_and_legacy_provisioning() {
        let home = private_home();
        let path = home.path().join("registry.db");
        let mut registry = Registry::open(&path).unwrap();
        let unix_id = approved(&mut registry);
        let legacy_id = approved(&mut registry);
        let partial_legacy = approved(&mut registry);
        registry
            .save_resources(partial_legacy, b"partial", 2)
            .unwrap();
        let nonce = UnixOrigin::parse("0123456789abcdef0123456789abcdef").unwrap();
        let other = UnixOrigin::parse("0123456789abcdef0123456789abcdee").unwrap();
        assert_eq!(
            registry.bind_origin(unix_id, &proof(6), 2, Some(&nonce)),
            Err(RegistryError::NotFound)
        );
        registry
            .bind_origin(unix_id, &proof(7), 2, Some(&nonce))
            .unwrap();
        registry.bind_origin(legacy_id, &proof(7), 2, None).unwrap();
        assert_eq!(
            registry.bind_origin(partial_legacy, &proof(7), 2, Some(&other)),
            Err(RegistryError::InvalidRequest)
        );
        let another_id = approved(&mut registry);
        assert_eq!(
            registry.bind_origin(another_id, &proof(7), 2, Some(&nonce)),
            Err(RegistryError::InvalidRequest)
        );
        drop(registry);

        let mut registry = Registry::open(&path).unwrap();
        registry
            .bind_origin(unix_id, &proof(7), 3, Some(&nonce))
            .unwrap();
        registry.bind_origin(legacy_id, &proof(7), 3, None).unwrap();
        assert_eq!(
            registry.bind_origin(unix_id, &proof(7), 3, Some(&other)),
            Err(RegistryError::InvalidRequest)
        );
        assert_eq!(
            registry.bind_origin(unix_id, &proof(7), 3, None),
            Err(RegistryError::InvalidRequest)
        );
        assert_eq!(
            registry.bind_origin(legacy_id, &proof(7), 3, Some(&other)),
            Err(RegistryError::InvalidRequest)
        );
    }

    #[test]
    fn expiry_uses_issue_cutoff_and_resumes_after_reopen() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&path).unwrap();
        let id = approved(&mut registry);
        let retained = approved(&mut registry);
        let unknown = approved(&mut registry);
        let issued_at = 80;
        for id in [id, retained, unknown] {
            registry
                .save_resources(id, b"synthetic", issued_at)
                .unwrap();
            registry.mark_ready(id, issued_at, false).unwrap();
        }

        let cutoff = issued_at + UNOPENED_SECONDS;
        assert!(
            registry
                .lifecycle_candidates(cutoff - 1)
                .unwrap()
                .is_empty()
        );
        assert!(registry.request_expiry(id, cutoff - 1).is_err());
        assert_eq!(registry.lifecycle_candidates(cutoff).unwrap().len(), 3);
        registry
            .retain_connected(retained, LifecyclePhase::Ready)
            .unwrap();
        assert!(registry.request_expiry(retained, cutoff).is_err());
        assert_eq!(
            registry.request_expiry(id, cutoff).unwrap().phase,
            LifecyclePhase::Expiring
        );
        assert_eq!(
            registry.mark_ready(id, cutoff, false).err(),
            Some(RegistryError::Unavailable)
        );
        assert_eq!(
            registry.save_resources(id, b"changed", cutoff).err(),
            Some(RegistryError::Unavailable)
        );
        assert_eq!(
            registry.get(id, &proof(7), cutoff).unwrap().state,
            EnrollmentState::Ready
        );
        drop(registry);

        let mut registry = Registry::open(&path).unwrap();
        let candidates = registry.lifecycle_candidates(cutoff).unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .any(|r| r.id == id && r.phase == LifecyclePhase::Expiring)
        );
        assert!(
            candidates
                .iter()
                .any(|r| r.id == unknown && r.phase == LifecyclePhase::Ready)
        );
        assert_eq!(
            registry
                .complete_removal(id, LifecyclePhase::Expiring)
                .unwrap()
                .phase,
            LifecyclePhase::Expired
        );
        assert_eq!(
            registry.lifecycle_candidates(cutoff).unwrap()[0].id,
            unknown
        );
        assert!(registry.lifecycle(retained).unwrap().ever_connected);
    }

    #[test]
    fn revoke_and_reissue_keep_identity_and_require_confirmed_removal() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&path).unwrap();
        let id = approved(&mut registry);
        registry.save_resources(id, b"old resource", 80).unwrap();
        registry.mark_ready(id, 80, false).unwrap();
        registry
            .retain_connected(id, LifecyclePhase::Ready)
            .unwrap();
        assert_eq!(
            registry.request_revoke(id).unwrap().phase,
            LifecyclePhase::Revoking
        );
        assert!(registry.request_reissue(id).is_err());
        assert!(
            registry
                .complete_removal(id, LifecyclePhase::Expiring)
                .is_err()
        );
        drop(registry);

        let mut registry = Registry::open(&path).unwrap();
        let revoking = registry.request_revoke(id).unwrap();
        assert_eq!(revoking.phase, LifecyclePhase::Revoking);
        assert_eq!(
            revoking.resources.as_deref(),
            Some(b"old resource".as_slice())
        );
        assert_eq!(
            registry
                .complete_removal(id, LifecyclePhase::Revoking)
                .unwrap()
                .phase,
            LifecyclePhase::Revoked
        );
        let reissued = registry.request_reissue(id).unwrap();
        assert_eq!(reissued.id, id);
        assert_eq!(reissued.phase, LifecyclePhase::Approved);
        assert!(reissued.resources.is_none());
        assert!(reissued.ever_connected);
        let enrollment = registry.get(id, &proof(7), 100).unwrap();
        assert_eq!(enrollment.state, EnrollmentState::Approved);
        assert_eq!(enrollment.issued_at, None);
        registry.save_resources(id, b"new resource", 100).unwrap();
        assert_eq!(
            registry.mark_ready(id, 100, false).unwrap().issued_at,
            Some(100)
        );
        assert!(
            registry
                .lifecycle_candidates(100 + UNOPENED_SECONDS)
                .unwrap()
                .is_empty()
        );

        let pending = registry
            .begin(Uuid::new_v4(), &proof(7), CODE, 100)
            .unwrap();
        assert_eq!(
            registry.request_revoke(pending.id).unwrap().phase,
            LifecyclePhase::Revoked
        );
        assert!(registry.request_reissue(pending.id).is_err());
        assert_eq!(
            registry.login(pending.id, 100).err(),
            Some(RegistryError::Revoked)
        );
    }

    #[test]
    fn migrates_version_one_enrollment_in_place() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        let id = Uuid::new_v4();
        let digest = proof_digest(&proof(7)).unwrap();
        connection.execute(
            "INSERT INTO enrollments (id, request_id, proof_digest, state, expires_at, issued_at, resources)
             VALUES (?1, ?2, ?3, 'ready', 60, 80, ?4)",
            params![id.to_string(), Uuid::new_v4().to_string(), digest.as_slice(), b"synthetic"],
        ).unwrap();
        drop(connection);

        let mut registry = Registry::open(&path).unwrap();
        let enrollment = registry.get(id, &proof(7), 100).unwrap();
        assert_eq!(enrollment.state, EnrollmentState::Ready);
        assert_eq!(enrollment.issued_at, Some(80));
        assert_eq!(
            enrollment.resources.as_deref(),
            Some(b"synthetic".as_slice())
        );
        assert_eq!(registry.lifecycle(id).unwrap().phase, LifecyclePhase::Ready);
        assert_eq!(
            registry
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        drop(registry);
        assert_eq!(Registry::open(&path).unwrap().lifecycle(id).unwrap().id, id);
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
    }

    #[test]
    fn browser_handoff_is_idempotent_bound_single_use_and_rotatable() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("enrollments.sqlite")).unwrap();
        let a = registry
            .begin(Uuid::new_v4(), &proof(1), CODE, 0)
            .unwrap()
            .id;
        let b = registry
            .begin(Uuid::new_v4(), &proof(2), CODE, 0)
            .unwrap()
            .id;
        let key = registry
            .handoff_url_secret(a, &proof(1), 0)
            .unwrap()
            .unwrap();
        assert_ne!(key, proof(1));
        assert_eq!(
            registry
                .handoff_url_secret(a, &proof(1), 1)
                .unwrap()
                .as_deref(),
            Some(key.as_str())
        );
        assert_eq!(
            registry.handoff_url_secret(a, &proof(2), 1).err(),
            Some(RegistryError::NotFound)
        );
        let email = "pilot@example.test";
        let mut tampered = key.as_bytes().to_vec();
        tampered[0] = if tampered[0] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            registry
                .start_handoff_attempt(
                    a,
                    std::str::from_utf8(&tampered).unwrap(),
                    email,
                    &OTP_KEY,
                    1
                )
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry
                .start_handoff_attempt(b, &key, email, &OTP_KEY, 1)
                .err(),
            Some(RegistryError::NotFound)
        );
        registry
            .start_handoff_attempt(a, &key, email, &OTP_KEY, 1)
            .unwrap();
        assert_eq!(
            registry
                .start_handoff_attempt(a, &key, email, &OTP_KEY, 2)
                .err(),
            Some(RegistryError::NotFound)
        );
        assert!(
            registry
                .handoff_url_secret(a, &proof(1), 2)
                .unwrap()
                .is_none()
        );
        let time = 1 + BROWSER_SECONDS;
        let new_key = registry
            .handoff_url_secret(a, &proof(1), time)
            .unwrap()
            .unwrap();
        assert_ne!(new_key, key);
        assert_eq!(
            registry
                .start_handoff_attempt(a, &key, email, &OTP_KEY, time)
                .err(),
            Some(RegistryError::NotFound)
        );
        assert_eq!(
            registry
                .start_handoff_attempt(a, &new_key, email, &OTP_KEY, PENDING_SECONDS)
                .err(),
            Some(RegistryError::Expired)
        );
    }

    #[test]
    fn email_code_requires_delivery_and_expires() {
        let home = private_home();
        let mut registry = Registry::open(&home.path().join("enrollments.sqlite")).unwrap();
        let record = registry.begin(Uuid::new_v4(), &proof(7), CODE, 0).unwrap();
        let handoff = registry
            .handoff_url_secret(record.id, &proof(7), 0)
            .unwrap()
            .unwrap();
        let attempt = registry
            .start_handoff_attempt(record.id, &handoff, "pilot@example.test", &OTP_KEY, 0)
            .unwrap();
        assert_eq!(
            registry
                .verify_email_code(
                    record.id,
                    attempt.id,
                    &attempt.code,
                    &OTP_KEY,
                    1,
                    DEFAULT_HOSTED_CAPACITY
                )
                .err(),
            Some(RegistryError::NotFound)
        );
        registry.mark_email_sent(attempt.id, record.id, 1).unwrap();
        assert_eq!(
            registry
                .verify_email_code(
                    record.id,
                    attempt.id,
                    &attempt.code,
                    &OTP_KEY,
                    BROWSER_SECONDS,
                    DEFAULT_HOSTED_CAPACITY
                )
                .err(),
            Some(RegistryError::Expired)
        );
        registry.expire_email_attempts(BROWSER_SECONDS).unwrap();
        assert!(
            registry
                .handoff_url_secret(record.id, &proof(7), BROWSER_SECONDS)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_wrong_email_code_consumes_the_guess_but_keeps_the_throttle() {
        let home = private_home();
        let db = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&db).unwrap();
        let id = registry
            .begin(Uuid::new_v4(), &proof(7), CODE, 0)
            .unwrap()
            .id;
        let handoff = registry
            .handoff_url_secret(id, &proof(7), 0)
            .unwrap()
            .unwrap();
        let attempt = registry
            .start_handoff_attempt(id, &handoff, "pilot@example.test", &OTP_KEY, 0)
            .unwrap();
        registry.mark_email_sent(attempt.id, id, 0).unwrap();
        let wrong = if attempt.code == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert_eq!(
            registry
                .verify_email_code(id, attempt.id, wrong, &OTP_KEY, 1, DEFAULT_HOSTED_CAPACITY)
                .err(),
            Some(RegistryError::NotFound)
        );
        drop(registry);
        let mut registry = Registry::open(&db).unwrap();
        assert_eq!(
            registry
                .verify_email_code(
                    id,
                    attempt.id,
                    &attempt.code,
                    &OTP_KEY,
                    2,
                    DEFAULT_HOSTED_CAPACITY
                )
                .err(),
            Some(RegistryError::NotFound)
        );
        let other = registry
            .begin(Uuid::new_v4(), &proof(8), CODE, 2)
            .unwrap()
            .id;
        let other_handoff = registry
            .handoff_url_secret(other, &proof(8), 2)
            .unwrap()
            .unwrap();
        assert_eq!(
            registry
                .start_handoff_attempt(other, &other_handoff, "pilot@example.test", &OTP_KEY, 2)
                .err(),
            Some(RegistryError::RateLimited)
        );
        let next = registry
            .handoff_url_secret(id, &proof(7), BROWSER_SECONDS)
            .unwrap()
            .unwrap();
        assert_ne!(next, handoff);
        assert!(
            registry
                .start_handoff_attempt(id, &next, "pilot@example.test", &OTP_KEY, BROWSER_SECONDS)
                .is_ok()
        );
    }

    #[test]
    fn approved_record_resumes_and_first_issue_timestamp_persists() {
        let home = private_home();
        let path = home.path().join("enrollments.sqlite");
        let mut registry = Registry::open(&path).unwrap();
        let request = Uuid::new_v4();
        let original = registry.begin(request, &proof(7), CODE, 0).unwrap();
        let approved = registry
            .approve_for_test(original.id, &proof(7), 1)
            .unwrap();
        assert_eq!(approved.state, EnrollmentState::Approved);
        let later = original.expires_at + 1;
        let resources = br#"{"tunnel_id":"synthetic"}"#;
        registry.save_resources(original.id, resources, 2).unwrap();
        drop(registry);
        let mut registry = Registry::open(&path).unwrap();
        let retried = registry.begin(request, &proof(7), CODE, later).unwrap();
        assert_eq!(retried.id, original.id);
        assert_eq!(retried.state, EnrollmentState::Approved);
        assert_eq!(retried.resources.as_deref(), Some(resources.as_slice()));
        let ready = registry.mark_ready(original.id, later, false).unwrap();
        assert_eq!(ready.issued_at, Some(later));
        drop(registry);
        let mut registry = Registry::open(&path).unwrap();
        let ready = registry.mark_ready(original.id, later + 1, false).unwrap();
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
        drop(radio999k::Queue::open(&path).unwrap());
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
