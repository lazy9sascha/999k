//! Finite Cue Bearer custody for the opt-in managed hosted Unix origin only.
use crate::auth;
use pigeonmq::private_file;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

const GRANT: &str = "hosted-cue-bearer.json";
const LOCK: &str = "hosted-cue-bearer.lock";
pub(crate) const MAX_SECONDS: u64 = 30 * 24 * 60 * 60;
const DENY: (u16, &str) = (401, "unauthorized");

#[derive(Clone)]
pub(crate) struct Scope {
    home: PathBuf,
    installation: Uuid,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    installation: String,
    mailbox: String,
    route: String,
    digest: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
    revoked: bool,
}

fn invalid() -> io::Error {
    io::Error::other("hosted Cue Bearer state is unavailable")
}

fn now_ms() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(invalid)
}

impl Scope {
    pub(crate) fn new(home: &Path, installation: Uuid) -> Self {
        Self {
            home: home.into(),
            installation,
        }
    }

    pub(crate) fn lock(&self, write: bool) -> io::Result<File> {
        let path = self.home.join(LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        private_file::open_private_file(&path).map_err(|_| invalid())?;
        let operation = if write { libc::LOCK_EX } else { libc::LOCK_SH };
        // SAFETY: this file descriptor remains live for the lock's lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other(
                "hosted Cue Bearer state is busy; retry after active requests",
            ));
        }
        Ok(file)
    }

    fn verify_at(&self, headers: &[Vec<u8>], now: u64) -> Result<(), (u16, &'static str)> {
        let received = auth::presented_token_digest(headers)?;
        let grant = self.load().map_err(|_| DENY)?;
        let expected = auth::digest(&grant.digest).ok_or(DENY)?;
        if grant.revoked
            || now < grant.issued_at_ms
            || now >= grant.expires_at_ms
            || expected.ct_eq(&received).unwrap_u8() != 1
        {
            return Err(DENY);
        }
        Ok(())
    }

    pub(crate) fn authorize(&self, headers: &[Vec<u8>]) -> Result<(), (u16, &'static str)> {
        self.verify_at(headers, now_ms().map_err(|_| DENY)?)
    }

    fn load(&self) -> io::Result<Grant> {
        let bytes = auth::read_private_policy(&self.home.join(GRANT)).map_err(|_| invalid())?;
        let value = pigeonmq::mcp_request::strict_json(&bytes).map_err(|_| invalid())?;
        let grant: Grant = serde_json::from_value(value).map_err(|_| invalid())?;
        if self.installation.is_nil()
            || grant.installation != self.installation.to_string()
            || grant.mailbox != "cue"
            || grant.route != "/cue/mcp"
            || !grant
                .expires_at_ms
                .checked_sub(grant.issued_at_ms)
                .is_some_and(|life| (1..=MAX_SECONDS * 1000).contains(&life))
        {
            return Err(invalid());
        }
        Ok(grant)
    }

    fn save(&self, grant: &Grant) -> io::Result<()> {
        private_file::replace_private(&self.home.join(GRANT), &serde_json::to_vec(grant)?)
    }

    pub(crate) fn rotate_locked(
        &self,
        seconds: u64,
        output: &Path,
        enable_mode: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        if self.installation.is_nil() || !(1..=MAX_SECONDS).contains(&seconds) {
            return Err(io::Error::other(
                "select a hosted Cue Bearer lifetime of 1 to 2592000 seconds",
            ));
        }
        let parent = output
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or_else(invalid)?;
        let parent = crate::cloud_setup::require_home(parent)?;
        let output = parent.join(output.file_name().ok_or_else(invalid)?);
        if output.starts_with(&self.home) {
            return Err(io::Error::other(
                "select an export directory outside the hub home",
            ));
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let issued = now_ms()?;
        let grant = Grant {
            installation: self.installation.to_string(),
            mailbox: "cue".into(),
            route: "/cue/mcp".into(),
            digest: Sha256::digest(token.as_bytes())
                .iter()
                .map(|value| format!("{value:02x}"))
                .collect(),
            issued_at_ms: issued,
            expires_at_ms: issued.checked_add(seconds * 1000).ok_or_else(invalid)?,
            revoked: false,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&output)?;
        let stage = (|| {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            File::open(&parent)?.sync_all()
        })();
        if let Err(err) = stage {
            let _ = fs::remove_file(&output);
            return Err(err);
        }
        // The export is durable before the grant changes. If replace returns
        // an error after rename, retain the export: it may hold the active token.
        enable_mode()?;
        self.save(&grant)
    }

    pub(crate) fn revoke_locked(&self) -> io::Result<()> {
        let mut grant = self.load()?;
        grant.revoked = true;
        self.save(&grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn headers(token: &str) -> Vec<Vec<u8>> {
        vec![format!("Bearer {token}").into_bytes()]
    }

    #[test]
    fn expiry_rotation_and_revocation_are_private_and_installation_bound() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::new_v4();
        let scope = Scope::new(dir.path(), id);
        let transfer = tempfile::tempdir().unwrap();
        fs::set_permissions(transfer.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let first = transfer.path().join("first");
        let second = transfer.path().join("second");
        let lock = scope.lock(true).unwrap();
        assert!(scope.lock(false).is_err());
        assert!(
            scope
                .rotate_locked(30, &dir.path().join(GRANT), || panic!("no mode change"))
                .is_err()
        );
        assert!(
            scope
                .rotate_locked(0, &first, || panic!("no mode change"))
                .is_err()
        );
        assert!(
            scope
                .rotate_locked(MAX_SECONDS + 1, &first, || panic!("no mode change"))
                .is_err()
        );
        scope.rotate_locked(30, &first, || Ok(())).unwrap();
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let raw = fs::read_to_string(&first).unwrap();
        assert!(
            !fs::read_to_string(dir.path().join(GRANT))
                .unwrap()
                .contains(&raw)
        );
        let old = headers(&raw);
        let grant = scope.load().unwrap();
        assert!(scope.verify_at(&old, grant.issued_at_ms - 1).is_err());
        assert!(scope.verify_at(&old, grant.issued_at_ms).is_ok());
        assert!(scope.verify_at(&old, grant.expires_at_ms - 1).is_ok());
        assert!(scope.verify_at(&old, grant.expires_at_ms).is_err());
        let other = Scope::new(dir.path(), Uuid::new_v4());
        assert!(other.verify_at(&old, grant.issued_at_ms).is_err());
        let before = fs::read(dir.path().join(GRANT)).unwrap();
        assert!(
            scope
                .rotate_locked(30, &first, || panic!("no mode change"))
                .is_err()
        );
        assert_eq!(fs::read(dir.path().join(GRANT)).unwrap(), before);
        scope.rotate_locked(30, &second, || Ok(())).unwrap();
        drop(lock);
        let reopened = Scope::new(dir.path(), id);
        let new = headers(&fs::read_to_string(second).unwrap());
        assert!(reopened.authorize(&old).is_err());
        assert!(reopened.authorize(&new).is_ok());
        let _lock = reopened.lock(true).unwrap();
        reopened.revoke_locked().unwrap();
        assert!(reopened.authorize(&new).is_err());
    }
}
