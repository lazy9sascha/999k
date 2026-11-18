//! Shared owner-private descriptor checks.
use std::{fs::OpenOptions, path::Path};
type Fault = (u16, &'static str);

/// Bound the allocation before parsing private policy, registration, and profile JSON.
pub const MAX_PRIVATE_CONFIG_BYTES: u64 = 16 * 1024;

#[cfg(unix)]
pub fn open_private_file(path: &Path) -> Result<std::fs::File, Fault> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    };
    let fail = (503, "auth_unavailable");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or(fail)?;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| fail)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent)
        .map_err(|_| fail)?;
    let metadata = directory.metadata().map_err(|_| fail)?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(fail);
    }
    // SAFETY: the directory descriptor is live and name is a valid NUL-terminated component.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(fail);
    }
    // SAFETY: openat returned a new descriptor owned exclusively by this File.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata().map_err(|_| fail)?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(fail);
    }
    Ok(file)
}

#[cfg(not(unix))]
pub fn open_private_file(_path: &Path) -> Result<std::fs::File, Fault> {
    // No permissive fallback where owner-private descriptor checks are unavailable.
    Err((503, "auth_unavailable"))
}

/// Replace through an exclusively created 0600 sibling, then sync the directory.
/// The caller must supply a trusted, stable parent directory.
/// A directory-sync error can occur after replacement; it does not undo the rename.
pub fn replace_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = std::fs::File::open(parent)?;
    let temp = parent.join(format!(".pigeonmq-{}.tmp", uuid::Uuid::new_v4()));
    // Never truncate or follow an existing entry, even if a name collides.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    // Clean only before a successful rename. Afterwards this name is no longer ours.
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    directory.sync_all()
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn replaces_with_private_file_without_following_destination_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "unchanged").unwrap();
        let path = dir.path().join("state.json");
        symlink(&victim, &path).unwrap();
        replace_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        replace_private(&path, b"again").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"again");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn failed_rename_leaves_no_temporary_file_and_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::create_dir(&path).unwrap();
        assert!(replace_private(&path, b"new").is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        fs::remove_dir(&path).unwrap();
        replace_private(&path, b"retry").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"retry");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
