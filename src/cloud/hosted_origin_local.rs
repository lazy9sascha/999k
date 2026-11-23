//! Local custody for a new hosted Unix origin. It never forwards to TCP.
use crate::hosted_origin::UnixOrigin;
use pigeonmq::{arrival_notify, private_file};
use std::{
    fs::{self, DirBuilder, File},
    io::{self, Read},
    os::unix::{
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

fn bind_in(
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
    let created = match DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
        Err(err) => return Err(err),
    };
    let state = dir.symlink_metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    if !state.is_dir() || state.uid() != uid || state.mode() & 0o777 != 0o700 {
        return Err(failure("hosted origin directory is not private"));
    }
    let marker = dir.join(CUSTODY);
    let expected = installation.to_string();
    if created {
        private_file::replace_private(&marker, expected.as_bytes())?;
    }
    let mut existing = String::new();
    private_file::open_private_file(&marker)
        .map_err(|_| failure("hosted origin is not owned by this installation"))?
        .take(37)
        .read_to_string(&mut existing)?;
    if existing != expected {
        return Err(failure("hosted origin is not owned by this installation"));
    }
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
    fn nonce_path_fits_darwin_without_username_or_hub_home() {
        let origin = new_origin().unwrap();
        let path = origin.socket_path();
        assert!(path.starts_with("/private/tmp/pmq-"));
        assert!(path.ends_with("/hub.sock"));
        assert!(path.len() <= MAC_SOCKET_MAX);
        let _api_checked_without_touching_system_path = bind_system;
    }
}
