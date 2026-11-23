//! Local custody for a new hosted Unix origin. It never forwards to TCP.
#![allow(dead_code)] // The connector activation path is not exposed yet.
use crate::hosted_origin::UnixOrigin;
use pigeonmq::{arrival_notify, private_file};
use std::{
    fs::{self, DirBuilder, File},
    io::{self, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
};

const CUSTODY: &str = "installation";
const MAC_SOCKET_MAX: usize = 103;

fn failure(message: &'static str) -> io::Error {
    io::Error::other(message)
}

pub(super) fn new_origin() -> io::Result<UnixOrigin> {
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    use std::fmt::Write;
    let mut value = String::with_capacity(32);
    for byte in random {
        write!(&mut value, "{byte:02x}").map_err(|_| failure("invalid hosted origin"))?;
    }
    UnixOrigin::parse(&value).ok_or_else(|| failure("invalid hosted origin"))
}

/// Keep custody for the full listener lifetime. A second hub cannot unlink its socket.
pub(super) struct OriginListener {
    pub(super) listener: UnixListener,
    _guard: File,
}

pub(super) fn bind_system(
    origin: &UnixOrigin,
    installation: uuid::Uuid,
) -> io::Result<OriginListener> {
    let root = Path::new("/private/tmp");
    let state = root.symlink_metadata()?;
    if !state.is_dir()
        || state.uid() != 0
        || state.mode() & 0o1000 == 0
        || root.canonicalize()? != root
    {
        return Err(failure("unsafe system root for hosted origin"));
    }
    let expected = origin.socket_path();
    if root.join(origin.directory_name()).join("hub.sock") != Path::new(&expected) {
        return Err(failure("invalid hosted origin path"));
    }
    bind_in(root, origin, installation)
}

#[cfg(target_os = "macos")]
pub(super) fn attest_system_path(
    origin: &UnixOrigin,
    installation: uuid::Uuid,
) -> io::Result<std::path::PathBuf> {
    use std::path::PathBuf;
    let root = Path::new("/private/tmp");
    let state = root.symlink_metadata()?;
    if !state.is_dir()
        || state.uid() != 0
        || state.mode() & 0o1000 == 0
        || root.canonicalize()? != root
    {
        return Err(failure("unsafe system root for hosted origin"));
    }
    let dir = root.join(origin.directory_name());
    verify_custody(&dir, &installation.to_string())?;
    let path = PathBuf::from(origin.socket_path());
    let state = path.symlink_metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    if !state.file_type().is_socket()
        || state.uid() != unsafe { libc::geteuid() }
        || state.nlink() != 1
    {
        return Err(failure("unexpected file at hosted origin"));
    }
    Ok(path)
}

fn verify_custody(dir: &Path, expected: &str) -> io::Result<()> {
    let state = dir.symlink_metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    if !state.is_dir() || state.uid() != uid || state.mode() & 0o777 != 0o700 {
        return Err(failure("hosted origin directory is not private"));
    }
    let mut existing = String::new();
    private_file::open_private_file(&dir.join(CUSTODY))
        .map_err(|_| failure("hosted origin is not owned by this installation"))?
        .take(37)
        .read_to_string(&mut existing)?;
    if existing != expected {
        return Err(failure("hosted origin is not owned by this installation"));
    }
    Ok(())
}

fn publish_new(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: Paths are NUL-terminated and remain valid for the call.
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn publish_custody(root: &Path, target: &Path, expected: &str) -> io::Result<()> {
    // Prepare below an independent name. A crash cannot leave an unmarked
    // directory at the reserved socket path, and an existing entry is never replaced.
    let random = new_origin()?;
    let stage = root.join(format!(".pigeonmq-origin-{}", random.nonce()));
    DirBuilder::new().mode(0o700).create(&stage)?;
    let outcome = private_file::replace_private(&stage.join(CUSTODY), expected.as_bytes())
        .and_then(|()| publish_new(&stage, target));
    match outcome {
        // After rename, the old staging pathname is no longer ours.
        Ok(()) => File::open(root)?.sync_all(),
        Err(err) => {
            let _ = fs::remove_file(stage.join(CUSTODY));
            let _ = fs::remove_dir(&stage);
            if err.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

pub(crate) fn bind_in(
    root: &Path,
    origin: &UnixOrigin,
    installation: uuid::Uuid,
) -> io::Result<OriginListener> {
    if installation.is_nil() {
        return Err(failure("invalid hosted installation"));
    }
    let dir = root.join(origin.directory_name());
    let path = dir.join("hub.sock");
    if path.as_os_str().as_encoded_bytes().len() > MAC_SOCKET_MAX {
        return Err(failure("hosted origin path exceeds the macOS socket limit"));
    }
    let expected = installation.to_string();
    match dir.symlink_metadata() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            publish_custody(root, &dir, &expected)?
        }
        Err(err) => return Err(err),
        Ok(_) => {}
    }
    verify_custody(&dir, &expected)?;
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    let guard = arrival_notify::lock_state(&dir.join("socket.state"))
        .map_err(|_| failure("hosted origin is already in use"))?;
    match path.symlink_metadata() {
        Ok(state) => {
            if !state.file_type().is_socket() || state.uid() != uid || state.nlink() != 1 {
                return Err(failure("unexpected file at hosted origin"));
            }
            match UnixStream::connect(&path) {
                Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&path)?
                }
                _ => return Err(failure("hosted origin socket is already in use")),
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(&path)?;
    Ok(OriginListener {
        listener,
        _guard: guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("po")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn restart_recreates_only_its_stale_socket_without_tcp_fallback() {
        let root = fixture();
        let origin = new_origin().unwrap();
        assert!(origin != new_origin().unwrap());
        let id = uuid::Uuid::new_v4();
        let dir = root.path().join(origin.directory_name());
        let path = dir.join("hub.sock");
        let holder = bind_in(root.path(), &origin, id).unwrap();
        assert_eq!(dir.symlink_metadata().unwrap().mode() & 0o777, 0o700);
        let peer = UnixStream::connect(&path).unwrap();
        assert!(holder.listener.accept().is_ok());
        drop(peer);
        assert!(bind_in(root.path(), &origin, id).is_err());
        drop(holder);
        assert_eq!(
            UnixStream::connect(&path).err().unwrap().kind(),
            io::ErrorKind::ConnectionRefused
        );
        let holder = bind_in(root.path(), &origin, id).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(holder);
        assert!(bind_in(root.path(), &origin, uuid::Uuid::new_v4()).is_err());
        fs::remove_dir_all(&dir).unwrap();
        assert!(bind_in(root.path(), &origin, id).is_ok());
    }

    #[test]
    fn refuses_foreign_dirs_and_socket_symlinks_without_removing_them() {
        let root = fixture();
        let origin = new_origin().unwrap();
        let id = uuid::Uuid::new_v4();
        let dir = root.path().join(origin.directory_name());
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bind_in(root.path(), &origin, id).is_err());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(bind_in(root.path(), &origin, id).is_err());
        fs::remove_dir(&dir).unwrap();
        let listener = bind_in(root.path(), &origin, id).unwrap();
        drop(listener);
        let path = dir.join("hub.sock");
        fs::remove_file(&path).unwrap();
        let foreign = root.path().join("keep");
        fs::write(&foreign, "unrelated").unwrap();
        symlink(&foreign, &path).unwrap();
        assert!(bind_in(root.path(), &origin, id).is_err());
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&foreign).unwrap(), b"unrelated");
    }

    #[test]
    fn cut_before_publication_keeps_reserved_name_available() {
        let root = fixture();
        let origin = new_origin().unwrap();
        let id = uuid::Uuid::new_v4();
        let stage = root.path().join(".pigeonmq-origin-mock-crash");
        fs::create_dir(&stage).unwrap(); // Process died before writing the marker.
        let target = root.path().join(origin.directory_name());
        assert!(!target.exists());
        let holder = bind_in(root.path(), &origin, id).unwrap();
        assert!(stage.is_dir());
        assert!(
            holder
                .listener
                .local_addr()
                .unwrap()
                .as_pathname()
                .is_some()
        );
    }

    #[test]
    fn nonce_path_fits_darwin_without_username_or_hub_home() {
        let origin = new_origin().unwrap();
        let path = origin.socket_path();
        assert!(path.starts_with("/private/tmp/pmq-"));
        assert!(path.ends_with("/hub.sock"));
        assert!(path.len() <= MAC_SOCKET_MAX);
        let _api_checked_without_touching_system_path = bind_system;
    }
}
