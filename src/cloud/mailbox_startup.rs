//! Resolve configured shared mailbox profiles without granting authority.
use crate::codex_mcp::Profile;
use pigeonmq::mailbox_catalog as catalog;
use pigeonmq::private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file};
use std::{
    fs,
    io::{self, Read},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

pub(crate) const QUEUE: &str = "pigeonmq-cloud-queue.sqlite3";
pub(crate) const REGISTRATION: &str = "pigeonmq-cloud-registration.json";

pub fn explicit(
    definition: &catalog::Profile,
    queue: &Path,
    policy: &Path,
    registration: &Path,
) -> Profile {
    Profile {
        identity: crate::codex_mcp::Identity::BuiltIn(definition.mailbox),
        db: queue.into(),
        policy: policy.into(),
        registration: registration.into(),
    }
}

pub(crate) fn selected_policy(
    home: &Path,
    manifest: Option<&Manifest>,
    profile: &catalog::HttpProfile,
) -> Option<std::path::PathBuf> {
    if let Some(manifest) = manifest {
        return manifest.policy(profile).map(Path::to_path_buf);
    }
    if profile.manifest_only {
        return None;
    }
    // Only absence disables a conventional profile. Other errors reach validation.
    let path = home.join(profile.policy_file);
    (!matches!(fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound))
        .then_some(path)
}

#[derive(Clone)]
pub(crate) struct Binding {
    pub(crate) descriptor: &'static catalog::HttpProfile,
    pub(crate) route: String,
    pub(crate) policy: PathBuf,
    pub(crate) registration: Option<PathBuf>,
    pub(crate) wake_files_dir: Option<PathBuf>,
}

impl Binding {
    pub(crate) fn bundled(
        descriptor: &'static catalog::HttpProfile,
        policy: PathBuf,
        home: &Path,
    ) -> Self {
        Self {
            descriptor,
            route: descriptor.route.into(),
            policy,
            registration: descriptor
                .adapter
                .requires_registration()
                .then(|| home.join(REGISTRATION)),
            wake_files_dir: None,
        }
    }

    pub(crate) fn mailbox(&self, queue: &Path) -> Option<Profile> {
        Some(explicit(
            self.descriptor.mailbox.as_ref()?,
            queue,
            &self.policy,
            self.registration.as_deref()?,
        ))
    }
}

pub(crate) fn bindings(home: &Path, manifest: Option<&Manifest>) -> Vec<Binding> {
    if let Some(manifest) = manifest {
        return manifest.profiles.clone();
    }
    catalog::http_profiles()
        .iter()
        .filter_map(|profile| {
            Some(Binding::bundled(
                profile,
                selected_policy(home, None, profile)?,
                home,
            ))
        })
        .collect()
}

/// Explicit operational mounts for the known built-in adapters. No credentials, mailbox
/// identities, peers, or grants can be expressed in this file.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    #[serde(default)]
    registration: Option<String>,
    profiles: Vec<ManifestEntry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    adapter: String,
    route: String,
    policy: String,
    #[serde(default, deserialize_with = "deserialize_wake_files_dir")]
    wake_files_dir: Option<String>,
}

fn deserialize_wake_files_dir<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    <String as serde::Deserialize>::deserialize(deserializer).map(Some)
}

pub(crate) struct Manifest {
    source: std::path::PathBuf,
    fingerprint: String,
    profiles: Vec<Binding>,
}

impl Manifest {
    pub(crate) fn load(path: &Path, home: &Path) -> io::Result<Self> {
        if !path.is_absolute() || !home.is_absolute() {
            return Err(invalid_manifest());
        }
        Self::from_bytes(path, home, &read_manifest(&open_manifest(path)?)?)
    }

    fn from_bytes(path: &Path, home: &Path, bytes: &[u8]) -> io::Result<Self> {
        use sha2::{Digest, Sha256};

        let invalid = invalid_manifest;
        let parsed = parse_manifest(bytes)?;
        let input: ManifestFile = serde_json::from_value(parsed).map_err(|_| invalid())?;
        if input.profiles.is_empty() {
            return Err(invalid());
        }
        let registration = match input.registration {
            Some(path) => resolve_json(home, &path).ok_or_else(invalid)?,
            None => home.join(REGISTRATION),
        };
        let mut profiles: Vec<Binding> = Vec::with_capacity(input.profiles.len());
        let mut policies = std::collections::HashSet::new();
        for entry in input.profiles {
            let descriptor = catalog::http_by_id(&entry.adapter).ok_or_else(invalid)?;
            if !valid_route(descriptor, &entry.route)
                || profiles.iter().any(|previous| {
                    previous.descriptor == descriptor || previous.route == entry.route
                })
            {
                return Err(invalid());
            }
            let policy = resolve_policy(home, &entry.policy).ok_or_else(invalid)?;
            let metadata = open_private_file(&policy)
                .map_err(|_| io::Error::other("selected hub profile policy unavailable"))?
                .metadata()?;
            if !policies.insert((metadata.dev(), metadata.ino())) {
                return Err(invalid());
            }
            let wake_files_dir = entry
                .wake_files_dir
                .as_deref()
                .map(|dir| {
                    if descriptor.adapter != catalog::Adapter::Grok || !clean_absolute(dir) {
                        return Err(invalid());
                    }
                    let dir = Path::new(dir);
                    pigeonmq::grok_wake::validate_files_dir(home, dir)?;
                    Ok(dir.to_path_buf())
                })
                .transpose()?;
            profiles.push(Binding {
                descriptor,
                route: entry.route,
                policy,
                registration: descriptor
                    .adapter
                    .requires_registration()
                    .then(|| registration.clone()),
                wake_files_dir,
            });
        }
        if profiles
            .iter()
            .any(|binding| binding.registration.is_some())
        {
            let metadata = open_private_file(&registration)
                .map_err(|_| io::Error::other("selected Cloud registration unavailable"))?
                .metadata()?;
            if policies.contains(&(metadata.dev(), metadata.ino())) {
                return Err(invalid());
            }
        }
        let fingerprint = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            source: path.into(),
            fingerprint,
            profiles,
        })
    }

    pub(crate) fn source(&self) -> &Path {
        &self.source
    }
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn route(&self, profile: &catalog::HttpProfile) -> Option<&str> {
        self.profiles
            .iter()
            .find(|entry| entry.descriptor == profile)
            .map(|entry| entry.route.as_str())
    }

    #[cfg(test)]
    pub(crate) fn grok_wake_files_dir(&self) -> Option<&Path> {
        self.profiles
            .iter()
            .find(|entry| entry.descriptor.adapter == catalog::Adapter::Grok)
            .and_then(|entry| entry.wake_files_dir.as_deref())
    }

    pub(crate) fn policy(&self, profile: &catalog::HttpProfile) -> Option<&Path> {
        self.profiles
            .iter()
            .find(|entry| entry.descriptor == profile)
            .map(|entry| entry.policy.as_path())
    }
}

fn invalid_manifest() -> io::Error {
    io::Error::other("invalid hub profile manifest")
}

fn open_manifest(path: &Path) -> io::Result<fs::File> {
    open_private_file(path).map_err(|_| invalid_manifest())
}

fn read_manifest(file: &fs::File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_manifest(bytes: &[u8]) -> io::Result<serde_json::Value> {
    if bytes.len() as u64 > MAX_PRIVATE_CONFIG_BYTES {
        return Err(invalid_manifest());
    }
    pigeonmq::mcp_request::strict_json(bytes).map_err(|_| invalid_manifest())
}

pub(crate) fn prepare_dot_binding(queue: &Path, policy: &Path, path: &Path) -> io::Result<bool> {
    let absolute_paths =
        || io::Error::other("Dot binding requires absolute queue, policy, and manifest paths");
    let home = queue
        .parent()
        .filter(|home| home.is_absolute())
        .ok_or_else(absolute_paths)?;
    if !path.is_absolute() {
        return Err(absolute_paths());
    }
    let policy = policy
        .to_str()
        .filter(|path| clean_absolute(path))
        .ok_or_else(absolute_paths)?;
    let resolved = resolve_policy(home, policy).ok_or_else(invalid_manifest)?;
    let file = open_manifest(path)?;
    // SAFETY: the open manifest descriptor stays live through validation and replacement.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("Hub profile manifest is being edited"));
    }
    let metadata = file.metadata()?;
    let before = read_manifest(&file)?;
    let current = Manifest::from_bytes(path, home, &before)?;
    let dot = catalog::by_adapter(catalog::Adapter::Dot);
    if let Some(binding) = current
        .profiles
        .iter()
        .find(|entry| entry.descriptor == dot)
    {
        if binding.route != dot.route || binding.policy != resolved {
            return Err(io::Error::other(
                "existing Dot binding uses a different policy or route",
            ));
        }
        pigeonmq::dot_profile::check(queue, &resolved, true)?;
        return Ok(false);
    }
    if metadata.mode() & 0o200 == 0 {
        return Err(io::Error::other("Hub profile manifest is read-only"));
    }

    let mut input = parse_manifest(&before)?;
    input["profiles"]
        .as_array_mut()
        .ok_or_else(invalid_manifest)?
        .push(serde_json::json!({
            "adapter":dot.id,"route":dot.route,"policy":policy
        }));
    let after = serde_json::to_vec_pretty(&input)?;
    let updated = Manifest::from_bytes(path, home, &after)?;
    // A manifest cannot be rewritten if another binding treats it as a policy or registration.
    for binding in &updated.profiles {
        for input in
            std::iter::once(binding.policy.as_path()).chain(binding.registration.as_deref())
        {
            let config = open_private_file(input)
                .map_err(|_| invalid_manifest())?
                .metadata()?;
            if (metadata.dev(), metadata.ino()) == (config.dev(), config.ino()) {
                return Err(invalid_manifest());
            }
        }
    }
    pigeonmq::dot_profile::check(queue, &resolved, true)?;

    let latest = open_manifest(path)?;
    let latest_metadata = latest.metadata()?;
    if (metadata.dev(), metadata.ino(), metadata.mode())
        != (
            latest_metadata.dev(),
            latest_metadata.ino(),
            latest_metadata.mode(),
        )
        || before != read_manifest(&latest)?
    {
        return Err(io::Error::other(
            "Hub profile manifest changed during Dot preparation",
        ));
    }
    pigeonmq::private_file::replace_private(path, &after)?;
    Ok(true)
}

pub(crate) fn valid_route(profile: &catalog::HttpProfile, route: &str) -> bool {
    if route == profile.route {
        return true;
    }
    if profile.adapter == catalog::Adapter::Receiver || catalog::http_by_route(route).is_some() {
        return false;
    }
    catalog::valid_mcp_route(route)
}

fn resolve_policy(home: &Path, configured: &str) -> Option<std::path::PathBuf> {
    let path = resolve_json(home, configured)?;
    (path.file_name()?.to_str()? != REGISTRATION).then_some(path)
}

fn resolve_json(home: &Path, configured: &str) -> Option<PathBuf> {
    let path = Path::new(configured);
    let filename = path.file_name()?.to_str()?;
    if !valid_policy_filename(filename) {
        return None;
    }
    if path.is_absolute() {
        clean_absolute(configured).then(|| path.to_path_buf())
    } else {
        (configured == filename).then(|| home.join(filename))
    }
}

// Reject dot segments and alternate spellings before opening a configured path.
fn clean_absolute(configured: &str) -> bool {
    configured.strip_prefix('/').is_some_and(|absolute| {
        absolute
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
    })
}

fn valid_policy_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    !stem.is_empty()
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pigeonmq::{grok_wake, private_file::replace_private};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn wake_files_dir_requires_explicit_grok_manifest_and_a_nonconflicting_path() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::set_permissions(home, fs::Permissions::from_mode(0o700)).unwrap();
        let selected = tempfile::tempdir().unwrap();
        let selected = selected.path();
        fs::set_permissions(selected, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = home.join("profiles.json");
        let mount = |profile: &catalog::HttpProfile, dir: Option<serde_json::Value>| {
            replace_private(&home.join(profile.policy_file), b"{}").unwrap();
            let mut entry =
                json!({"adapter":profile.id,"route":profile.route,"policy":profile.policy_file});
            if let Some(dir) = dir {
                entry["wake_files_dir"] = dir;
            }
            replace_private(
                &manifest,
                &serde_json::to_vec(&json!({"profiles":[entry]})).unwrap(),
            )
            .unwrap();
            Manifest::load(&manifest, home)
        };
        for profile in catalog::http_profiles()
            .iter()
            .filter(|p| p.adapter != catalog::Adapter::Grok)
        {
            assert!(mount(profile, Some(json!(selected))).is_err());
            assert!(mount(profile, Some(serde_json::Value::Null)).is_err());
        }
        for dir in [
            json!("relative"),
            json!(""),
            json!("/some/../dir"),
            serde_json::Value::Null,
        ] {
            assert!(mount(catalog::by_adapter(catalog::Adapter::Grok), Some(dir)).is_err());
        }
        for file in [grok_wake::URL_FILE, grok_wake::AUTH_FILE] {
            replace_private(&home.join(file), b"default").unwrap();
        }
        assert!(
            mount(
                catalog::by_adapter(catalog::Adapter::Grok),
                Some(json!(selected))
            )
            .is_err()
        );
        for file in [grok_wake::URL_FILE, grok_wake::AUTH_FILE] {
            replace_private(&selected.join(file), b"explicit").unwrap();
        }
        assert_eq!(
            mount(
                catalog::by_adapter(catalog::Adapter::Grok),
                Some(json!(selected))
            )
            .unwrap()
            .grok_wake_files_dir(),
            Some(selected)
        );
        let marker = home.join(grok_wake::KEYCHAIN_MARKER);
        std::os::unix::fs::symlink(home.join("missing-marker-target"), &marker).unwrap();
        assert!(
            mount(
                catalog::by_adapter(catalog::Adapter::Grok),
                Some(json!(selected))
            )
            .is_err()
        );
        assert!(
            mount(catalog::by_adapter(catalog::Adapter::Grok), None)
                .unwrap()
                .grok_wake_files_dir()
                .is_none()
        );
    }

    #[test]
    fn resolution_preserves_absence_symlinks_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(
            bindings(home, None)
                .iter()
                .find(|binding| binding.descriptor == catalog::codex_http())
                .is_none()
        );
        let policy = home.join(catalog::codex_http().policy_file);
        std::os::unix::fs::symlink(home.join("missing"), &policy).unwrap();
        let profiles = bindings(home, None);
        assert!(
            profiles
                .iter()
                .find_map(|binding| binding.mailbox(&home.join(QUEUE)))
                .is_some()
        );
        let profile = profiles
            .iter()
            .find_map(|binding| binding.mailbox(&home.join(QUEUE)))
            .unwrap();
        assert_eq!(profile.policy, policy);
        assert_eq!(profile.registration, home.join(REGISTRATION));
        assert_eq!(profile.db, home.join(QUEUE));
        fs::remove_file(&policy).unwrap();
        assert!(
            bindings(home, None)
                .iter()
                .find(|binding| binding.descriptor == catalog::codex_http())
                .is_none()
        );
        fs::write(&policy, "invalid policy").unwrap();
        assert!(
            bindings(home, None)
                .iter()
                .find(|binding| binding.descriptor == catalog::codex_http())
                .is_some()
        );
    }
}
