//! Install and inspect only the two owner-selected PigeonMQ LaunchAgents.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(crate) const HUB_LABEL: &str = "pigeonmq.hub";
pub(crate) const RELAY_LABEL: &str = "pigeonmq.email-relay";
const PLIST_LIMIT: u64 = 32 * 1024;

pub(crate) trait LaunchControl {
    fn loaded(&self, label: &str, expected_plist: &Path) -> io::Result<bool>;
    fn bootstrap(&self, path: &Path) -> io::Result<()>;
    fn bootout(&self, label: &str) -> io::Result<()>;
}

pub(crate) struct SystemLaunchctl;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Job {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: bool,
}

fn fail(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn check_label(label: &str) -> io::Result<()> {
    if matches!(label, HUB_LABEL | RELAY_LABEL) {
        return Ok(());
    }
    Err(fail("invalid PigeonMQ service label"))
}

fn expected_job(directory: &Path, label: &str, args: &[String]) -> io::Result<(PathBuf, Job)> {
    check_label(label)?;
    if !directory.is_absolute()
        || !args.first().is_some_and(|arg| Path::new(arg).is_absolute())
        || args.iter().any(|arg| arg.chars().any(char::is_control))
    {
        return Err(fail("invalid PigeonMQ LaunchAgent arguments"));
    }
    Ok((
        directory.join(format!("{label}.plist")),
        Job {
            label: label.to_owned(),
            program_arguments: args.to_vec(),
            run_at_load: true,
            keep_alive: true,
        },
    ))
}

fn euid() -> u32 {
    // SAFETY: geteuid has no arguments or preconditions.
    unsafe { libc::geteuid() }
}

fn check_directory(directory: &Path) -> io::Result<bool> {
    match directory.symlink_metadata() {
        Ok(metadata)
            if metadata.is_dir() && metadata.uid() == euid() && metadata.mode() & 0o022 == 0 =>
        {
            Ok(true)
        }
        Ok(_) => Err(fail("unsafe PigeonMQ LaunchAgents directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn verify_expected(path: &Path, expected: &Job) -> io::Result<bool> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != euid()
        || metadata.mode() & 0o022 != 0
        || metadata.nlink() != 1
        || metadata.len() > PLIST_LIMIT
    {
        return Err(fail("unsafe existing PigeonMQ LaunchAgent plist"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(PLIST_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > PLIST_LIMIT {
        return Err(fail("existing PigeonMQ LaunchAgent plist is too large"));
    }
    let actual: Job = plist::from_bytes(&bytes)
        .map_err(|_| fail("invalid existing PigeonMQ LaunchAgent plist"))?;
    if actual != *expected {
        return Err(fail("existing PigeonMQ LaunchAgent plist conflicts"));
    }
    Ok(true)
}

/// Check existing configuration without creating or rewriting anything.
pub(crate) fn verify_plist(
    directory: &Path,
    label: &str,
    args: &[String],
) -> io::Result<Option<PathBuf>> {
    let (path, expected) = expected_job(directory, label, args)?;
    if !check_directory(directory)? || !verify_expected(&path, &expected)? {
        return Ok(None);
    }
    Ok(Some(path))
}

/// Publish a complete new plist without overwriting another process's file.
pub(crate) fn ensure_plist(
    directory: &Path,
    label: &str,
    args: &[String],
) -> io::Result<(PathBuf, bool)> {
    let (path, expected) = expected_job(directory, label, args)?;
    if !check_directory(directory)? {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        if !check_directory(directory)? {
            return Err(fail("PigeonMQ LaunchAgents directory unavailable"));
        }
    }
    if verify_expected(&path, &expected)? {
        return Ok((path, false));
    }

    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &expected)
        .map_err(|_| fail("could not encode PigeonMQ LaunchAgent plist"))?;
    if bytes.len() as u64 > PLIST_LIMIT {
        return Err(fail("PigeonMQ LaunchAgent plist is too large"));
    }
    let temp = directory.join(format!(".pigeonmq-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let install = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        publish_new(&temp, &path)
    })();
    if install.is_err() {
        let _ = fs::remove_file(&temp);
    }
    match install {
        Ok(()) => {
            File::open(directory)?.sync_all()?;
            Ok((path, true))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if verify_expected(&path, &expected)? {
                Ok((path, false))
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn publish_new(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let temporary = CString::new(temporary.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both path strings are valid NUL-terminated pointers for the call.
    let result =
        unsafe { libc::renamex_np(temporary.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn publish_new(temporary: &Path, destination: &Path) -> io::Result<()> {
    // The local staging format can be verified without launchd on other Unix targets.
    fs::hard_link(temporary, destination)?;
    fs::remove_file(temporary)
}

#[cfg(any(target_os = "macos", test))]
struct Captured {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(any(target_os = "macos", test))]
fn loaded_from_print(output: Captured, expected_plist: &Path) -> io::Result<bool> {
    if output.exit_code != Some(0) {
        let missing = output.exit_code == Some(113)
            && String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.starts_with("Could not find service "));
        return if missing {
            Ok(false)
        } else {
            Err(fail("could not inspect PigeonMQ LaunchAgent"))
        };
    }
    let printed = std::str::from_utf8(&output.stdout)
        .map_err(|_| fail("invalid PigeonMQ LaunchAgent inspection output"))?;
    let mut paths = printed
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("path = "));
    let path = paths
        .next()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| fail("loaded PigeonMQ LaunchAgent path is unknown"))?;
    if paths.next().is_some()
        || path != expected_plist && expected_plist.canonicalize().ok().as_deref() != Some(path)
    {
        return Err(fail("loaded PigeonMQ LaunchAgent uses a different plist"));
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&std::ffi::OsStr]) -> io::Result<Captured> {
    use std::{process::Stdio, time::Duration};
    use tokio::io::AsyncReadExt;
    const MAX_OUTPUT: u64 = 128 * 1024;
    const MAX_ERROR: u64 = 4 * 1024;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut child = tokio::process::Command::new("/bin/launchctl")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| fail("could not start launchctl"))?;
            let mut stdout = child
                .stdout
                .take()
                .expect("piped stdout")
                .take(MAX_OUTPUT + 1);
            let mut stderr = child
                .stderr
                .take()
                .expect("piped stderr")
                .take(MAX_ERROR + 1);
            let captured = tokio::time::timeout(Duration::from_secs(20), async {
                let mut out = Vec::new();
                let mut err = Vec::new();
                tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
                if out.len() as u64 > MAX_OUTPUT || err.len() as u64 > MAX_ERROR {
                    return Err(fail("launchctl output limit exceeded"));
                }
                Ok(Captured {
                    exit_code: child.wait().await?.code(),
                    stdout: out,
                    stderr: err,
                })
            })
            .await;
            match captured {
                Ok(Ok(output)) => Ok(output),
                other => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    match other {
                        Ok(Err(error)) => Err(error),
                        _ => Err(fail("launchctl timed out")),
                    }
                }
            }
        })
}

#[cfg(target_os = "macos")]
impl LaunchControl for SystemLaunchctl {
    fn loaded(&self, label: &str, expected_plist: &Path) -> io::Result<bool> {
        check_label(label)?;
        if !expected_plist.is_absolute() {
            return Err(fail("expected PigeonMQ LaunchAgent path must be absolute"));
        }
        let target = format!("gui/{}/{label}", euid());
        loaded_from_print(
            launchctl(&["print".as_ref(), target.as_ref()])?,
            expected_plist,
        )
    }

    fn bootstrap(&self, path: &Path) -> io::Result<()> {
        if !path.is_absolute()
            || !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("pigeonmq.hub.plist" | "pigeonmq.email-relay.plist")
            )
        {
            return Err(fail("invalid PigeonMQ LaunchAgent path"));
        }
        let domain = format!("gui/{}", euid());
        let output = launchctl(&["bootstrap".as_ref(), domain.as_ref(), path.as_os_str()])?;
        if output.exit_code != Some(0) {
            return Err(fail("could not bootstrap PigeonMQ LaunchAgent"));
        }
        Ok(())
    }

    fn bootout(&self, label: &str) -> io::Result<()> {
        check_label(label)?;
        let target = format!("gui/{}/{label}", euid());
        let output = launchctl(&["bootout".as_ref(), target.as_ref()])?;
        if output.exit_code != Some(0) {
            return Err(fail("could not boot out PigeonMQ LaunchAgent"));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
impl LaunchControl for SystemLaunchctl {
    fn loaded(&self, _: &str, _: &Path) -> io::Result<bool> {
        Err(fail("PigeonMQ LaunchAgents require macOS"))
    }

    fn bootstrap(&self, _: &Path) -> io::Result<()> {
        Err(fail("PigeonMQ LaunchAgents require macOS"))
    }

    fn bootout(&self, _: &str) -> io::Result<()> {
        Err(fail("PigeonMQ LaunchAgents require macOS"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn args() -> Vec<String> {
        vec!["/tmp/a path & pigeonmq".into(), "serve-hub".into()]
    }

    #[test]
    fn stages_only_a_matching_plist_and_preserves_existing_bytes() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("Library/LaunchAgents");
        assert_eq!(verify_plist(&directory, HUB_LABEL, &args()).unwrap(), None);
        assert!(!directory.exists());

        let (path, created) = ensure_plist(&directory, HUB_LABEL, &args()).unwrap();
        assert!(created);
        assert_eq!(path.metadata().unwrap().mode() & 0o777, 0o600);
        let (_, expected) = expected_job(&directory, HUB_LABEL, &args()).unwrap();
        let mut binary = Vec::new();
        plist::to_writer_binary(&mut binary, &expected).unwrap();
        fs::write(&path, &binary).unwrap();

        assert_eq!(
            ensure_plist(&directory, HUB_LABEL, &args()).unwrap(),
            (path.clone(), false)
        );
        assert_eq!(
            verify_plist(&directory, HUB_LABEL, &args()).unwrap(),
            Some(path.clone())
        );
        assert_eq!(fs::read(path).unwrap(), binary);
    }

    #[test]
    fn refuses_conflicting_unsafe_and_ambiguous_plists_without_replacing_them() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().canonicalize().unwrap();
        let (path, _) = ensure_plist(&directory, HUB_LABEL, &args()).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(ensure_plist(&directory, HUB_LABEL, &["/tmp/foreign".into()]).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let duplicate = String::from_utf8(bytes).unwrap().replace(
            "<key>RunAtLoad</key>",
            "<key>RunAtLoad</key><false/><key>RunAtLoad</key>",
        );
        fs::write(&path, &duplicate).unwrap();
        assert!(verify_plist(&directory, HUB_LABEL, &args()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), duplicate);

        fs::remove_file(&path).unwrap();
        let victim = directory.join("unrelated");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, &path).unwrap();
        assert!(ensure_plist(&directory, HUB_LABEL, &args()).is_err());
        assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
        let linked_directory = directory.join("linked");
        symlink(&directory, &linked_directory).unwrap();
        assert!(ensure_plist(&linked_directory, RELAY_LABEL, &args()).is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify_plist(&directory, RELAY_LABEL, &args()).is_err());
    }

    fn captured(exit_code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            exit_code: Some(exit_code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn fake_launchctl_output_never_treats_conflict_or_access_denial_as_absence() {
        let expected = Path::new("/isolated/Launch Agents/pigeonmq.hub.plist");
        assert!(loaded_from_print(
            captured(0, "gui/12/pigeonmq.hub = {\n\tpath = /isolated/Launch Agents/pigeonmq.hub.plist\n}\n", ""),
            expected,
        ).unwrap());
        assert!(
            !loaded_from_print(
                captured(113, "", "Could not find service \"pigeonmq.hub\""),
                expected
            )
            .unwrap()
        );
        for result in [
            captured(113, "", "Permission denied"),
            captured(0, "state = running", ""),
            captured(0, "path = /somewhere/else.plist", ""),
            captured(
                0,
                "path = /isolated/Launch Agents/pigeonmq.hub.plist\npath = /second.plist",
                "",
            ),
        ] {
            assert!(loaded_from_print(result, expected).is_err());
        }
    }
}
