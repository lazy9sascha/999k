//! Explicit Codex Cloud mailbox preparation. Transport and platform subscription stay separate.
use pigeonmq::mailbox_catalog as catalog;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const REGISTRY: &str = "pigeonmq-cloud-registration.json";
const QUEUE: &str = "pigeonmq-cloud-queue.sqlite3";

#[derive(clap::Args)]
pub(crate) struct SetupOptions {
    #[arg(long)]
    home: PathBuf,
    #[arg(long)]
    peer: String,
    #[arg(long, requires = "resolver")]
    callback_host: Vec<String>,
    #[arg(long, requires = "callback_host")]
    resolver: Option<PathBuf>,
}

fn validate(options: &SetupOptions) -> io::Result<()> {
    let peer = &options.peer;
    if peer.is_empty()
        || peer.len() > 128
        || peer == catalog::codex().mailbox
        || !peer
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return Err(io::Error::other(
            "Use a different peer name with 1 to 128 letters, digits, hyphens, underscores, periods, or colons.",
        ));
    }
    if options.callback_host.is_empty() != options.resolver.is_none() {
        return Err(io::Error::other(
            "Events require both --callback-host and --resolver.",
        ));
    }
    if let Some(path) = &options.resolver {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        let metadata = fs::symlink_metadata(path)?;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("invalid resolver path"))?;
        // FIXME(yagni): owner/0o022/X_OK checks on --resolver - the owner passes this path; spawn fails anyway if it is not executable
        // SAFETY: the path remains a live NUL-terminated string. geteuid has no preconditions.
        if !path.is_absolute()
            || !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
            || unsafe { libc::access(name.as_ptr(), libc::X_OK) } != 0
        {
            return Err(io::Error::other(
                "Use an absolute owned resolver executable without public write access.",
            ));
        }
        pigeonmq::events::policy::EventsPolicy::for_codex(
            "0".repeat(64),
            vec![peer.clone()],
            options.callback_host.clone(),
        )
        .map_err(|e| io::Error::other(e.reason))?;
    }
    Ok(())
}

fn existing_policy(home: &Path, options: &SetupOptions) -> io::Result<Option<Value>> {
    let path = home.join(catalog::codex_http().policy_file);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    crate::auth::validate_private(&path).map_err(|(_, reason)| io::Error::other(reason))?;
    let file = pigeonmq::private_file::open_private_file(&path)
        .map_err(|_| io::Error::other("private Codex policy required"))?;
    let value: Value = serde_json::from_reader(file.take(16385))?;
    let _ = principal(&value)?;
    if let Some(resolver) = &options.resolver {
        let settings: crate::auth::Events = serde_json::from_value(value["events"].clone())
            .map_err(|_| io::Error::other("Existing Codex policy has no matching Events configuration. No policy changed."))?;
        if settings.callback_hosts != options.callback_host || settings.resolver != *resolver {
            return Err(io::Error::other(
                "Existing Codex Events settings differ. No policy changed.",
            ));
        }
    }
    Ok(Some(value))
}

fn principal(value: &Value) -> io::Result<&Value> {
    value["principals"].as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == catalog::codex().mailbox))
        .filter(|entry| entry["revoked"] == false && entry["mailboxes"].as_array()
            .is_some_and(|mailboxes| mailboxes.iter().any(|mailbox| mailbox == catalog::codex().mailbox)))
        .ok_or_else(|| io::Error::other("Existing policy needs an active codex-cloud principal and mailbox. No policy changed."))
}

fn registration(home: &Path, peer: &str) -> io::Result<Option<Value>> {
    let path = home.join(REGISTRY);
    let entries = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
        Err(error) => return Err(error),
        Ok(_) => {
            pigeonmq::registration::load_registrations(&path)
                .map_err(|(_, reason)| io::Error::other(reason))?
                .0
        }
    };
    for name in [catalog::codex().mailbox, peer] {
        if entries.iter().any(|(id, entry)| {
            (id == name || entry.mailbox == name)
                && (id != name || entry.mailbox != name || entry.revoked)
        }) {
            return Err(io::Error::other(
                "Existing Codex or peer registration conflicts. No registration changed.",
            ));
        }
    }
    if entries.contains_key(catalog::codex().mailbox) && entries.contains_key(peer) {
        return Ok(None);
    }
    let mut entries: serde_json::Map<String, Value> = entries
        .into_iter()
        .map(|(id, entry)| (id, json!({"mailbox":entry.mailbox,"revoked":entry.revoked})))
        .collect();
    for name in [catalog::codex().mailbox, peer] {
        entries
            .entry(name.to_owned())
            .or_insert_with(|| json!({"mailbox":name,"revoked":false}));
    }
    Ok(Some(Value::Object(entries)))
}

fn snapshot(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let file = pigeonmq::private_file::open_private_file(path)
        .map_err(|_| io::Error::other("private Codex setup file required"))?;
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err(io::Error::other("Codex setup file is too large"));
    }
    Ok(Some(bytes))
}

fn setup_with(
    options: &SetupOptions,
    mut token: impl FnMut(&Path, bool) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Value> {
    validate(options)?;
    let policy_before = snapshot(&options.home.join(catalog::codex_http().policy_file))?;
    let existing = existing_policy(&options.home, options)?;
    let registry_before = snapshot(&options.home.join(REGISTRY))?;
    let registration = registration(&options.home, &options.peer)?;
    let (home, queue_created) = crate::cloud_setup::initialize_home(&options.home)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let fresh = existing.is_none();
    let bearer = token(&home, fresh)?.ok_or_else(|| io::Error::other(
        "Existing Codex token is absent locally. Use credentials --app codex-cloud import with its protected token file. No token was reissued."))?;
    // FIXME(yagni): bearer length/charset check on a token we generate or import ourselves - only the SHA-256 digest is stored
    if bearer.is_empty() || bearer.len() > 256 || !bearer.iter().all(|b| (33..=126).contains(b)) {
        return Err(io::Error::other("invalid Codex bearer"));
    }
    let digest: String = Sha256::digest(&bearer)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Some(value) = &existing {
        if snapshot(&home.join(catalog::codex_http().policy_file))? != policy_before {
            return Err(io::Error::other(
                "Codex policy changed during token lookup. Recheck it before retrying.",
            ));
        }
        if !principal(value)?["token_sha256"]
            .as_str()
            .is_some_and(|expected| expected.eq_ignore_ascii_case(&digest))
        {
            return Err(io::Error::other(
                "Existing Codex token does not match its policy. No token or policy changed.",
            ));
        }
    } else {
        let mut value = json!({"principals":[{"id":catalog::codex().mailbox,"token_sha256":digest,
            "operations":["read","wait","reply","send"],"send_to":[],"mailboxes":[catalog::codex().mailbox],"revoked":false}]});
        if let Some(resolver) = &options.resolver {
            value["events"] = json!({"callback_hosts":options.callback_host,"resolver":resolver});
        }
        if !crate::cloud_setup::create_settings(
            &home.join(catalog::codex_http().policy_file),
            &value,
        )? {
            return Err(io::Error::other(
                "Codex policy appeared during setup. Recheck it before retrying.",
            ));
        }
    }
    if let Some(value) = registration {
        if snapshot(&home.join(REGISTRY))? != registry_before {
            return Err(io::Error::other(
                "Registration changed during token lookup. No registration was replaced. Retry setup.",
            ));
        }
        pigeonmq::private_file::replace_private(
            &home.join(REGISTRY),
            &serde_json::to_vec_pretty(&value)?,
        )?;
    }
    let profile = crate::codex_mcp::Profile {
        identity: crate::codex_mcp::Identity::BuiltIn(pigeonmq::mailbox_catalog::codex().mailbox),
        db: home.join(QUEUE),
        policy: home.join(catalog::codex_http().policy_file),
        registration: home.join(REGISTRY),
    };
    profile
        .validate()
        .map_err(|(_, reason)| io::Error::other(reason))?;
    let events = crate::codex_events::Runtime::new(&profile)
        .map_err(|(_, reason)| io::Error::other(reason))?;
    if let Some(runtime) = &events {
        if runtime.check().is_err() {
            runtime.prepare().map_err(|e| io::Error::other(e.reason))?;
        }
        runtime.check().map_err(|e| io::Error::other(e.reason))?;
    }
    let current = existing_policy(&home, options)?
        .ok_or_else(|| io::Error::other("Codex policy disappeared during setup"))?;
    if !principal(&current)?["token_sha256"]
        .as_str()
        .is_some_and(|expected| expected.eq_ignore_ascii_case(&digest))
    {
        return Err(io::Error::other(
            "Codex policy changed. Local credential matching is unverified.",
        ));
    }
    pigeonmq::registration::validate_registration(
        &home.join(REGISTRY),
        &options.peer,
        catalog::codex().mailbox,
    )
    .map_err(|(_, reason)| io::Error::other(reason))?;
    Ok(
        json!({"state":"codex_prepared_local_only","home":home,"queue_created":queue_created,
        "policy_created":fresh,"mailbox":catalog::codex().mailbox,"route":catalog::codex_http().route,
        "events_enabled":events.is_some(),"local_credential_available":true,"matches_policy":true,
        "agent_use":"unverified","remote_verified":false,
        "next":"Use a hub that serves the Codex mailbox. Connect the authorized Secure MCP Tunnel to its loopback route."}),
    )
}

pub(crate) fn setup(options: &SetupOptions) -> io::Result<()> {
    println!(
        "{}",
        setup_with(options, crate::cloud_credentials::codex_setup_bearer)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const TOKEN: &[u8] = b"PUBLIC_CODEX_SETUP_FIXTURE";
    fn options(home: PathBuf) -> SetupOptions {
        SetupOptions {
            home,
            peer: "peer".into(),
            callback_host: vec![],
            resolver: None,
        }
    }
    fn write(path: &Path, value: &Value) {
        fs::write(path, value.to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn fresh_mailbox_setup_and_repeat_preserve_policy_registration_and_local_custody() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path().join("cloud"));
        let first = setup_with(&options, |_, create| {
            assert!(create);
            Ok(Some(TOKEN.into()))
        })
        .unwrap();
        assert_eq!(first["policy_created"], true);
        assert_eq!(first["events_enabled"], false);
        assert_eq!(first["agent_use"], "unverified");
        assert!(!first["next"].as_str().unwrap().contains("--local-only"));
        let policy = fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap();
        let registry = fs::read(options.home.join(REGISTRY)).unwrap();
        assert!(
            !String::from_utf8(policy.clone())
                .unwrap()
                .contains(std::str::from_utf8(TOKEN).unwrap())
        );
        let second = setup_with(&options, |_, create| {
            assert!(!create);
            Ok(Some(TOKEN.into()))
        })
        .unwrap();
        assert_eq!(second["policy_created"], false);
        assert_eq!(second["queue_created"], false);
        assert_eq!(
            fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap(),
            policy
        );
        assert_eq!(fs::read(options.home.join(REGISTRY)).unwrap(), registry);
        let value: Value = serde_json::from_slice(&policy).unwrap();
        assert_eq!(
            value["principals"][0]["operations"],
            json!(["read", "wait", "reply", "send"])
        );
        for name in [catalog::codex_http().policy_file, REGISTRY, QUEUE] {
            assert_eq!(
                fs::metadata(options.home.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn explicit_events_setup_prepares_schema_and_keeps_existing_policy_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(dir.path().join("cloud"));
        options.callback_host.push("callback.example.com".into());
        options.resolver = Some(std::env::current_exe().unwrap());
        assert_eq!(
            setup_with(&options, |_, _| Ok(Some(TOKEN.into()))).unwrap()["events_enabled"],
            true
        );
        let policy = fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap();
        options.callback_host.clear();
        options.resolver = None;
        assert_eq!(
            setup_with(&options, |_, create| {
                assert!(!create);
                Ok(Some(TOKEN.into()))
            })
            .unwrap()["events_enabled"],
            true
        );
        assert_eq!(
            fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap(),
            policy
        );
        let db = rusqlite::Connection::open(options.home.join(QUEUE)).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM dot_event_subscriptions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn invalid_inputs_and_existing_conflicts_do_not_replace_grants_or_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(dir.path().join("cloud"));
        options.peer = "codex-cloud".into();
        assert!(setup_with(&options, |_, _| panic!("must not access credentials")).is_err());
        assert!(!options.home.exists());
        options.peer = "peer".into();
        setup_with(&options, |_, _| Ok(Some(TOKEN.into()))).unwrap();
        let path = options.home.join(catalog::codex_http().policy_file);
        let policy = fs::read(&path).unwrap();
        let registry = fs::read(options.home.join(REGISTRY)).unwrap();
        assert!(
            setup_with(&options, |_, create| {
                assert!(!create);
                Ok(None)
            })
            .is_err()
        );
        assert!(setup_with(&options, |_, _| Ok(Some(b"WRONG_FIXTURE".to_vec()))).is_err());
        options.callback_host.push("callback.example.com".into());
        options.resolver = Some(std::env::current_exe().unwrap());
        assert!(
            setup_with(&options, |_, _| panic!(
                "configuration conflict precedes credentials"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), policy);
        assert_eq!(fs::read(options.home.join(REGISTRY)).unwrap(), registry);
        options.callback_host.clear();
        options.resolver = None;
        let mut changed: Value = serde_json::from_slice(&policy).unwrap();
        changed["principals"][0]["operations"] = json!(["read"]);
        write(&path, &changed);
        let retained = fs::read(&path).unwrap();
        setup_with(&options, |_, _| Ok(Some(TOKEN.into()))).unwrap();
        assert_eq!(fs::read(&path).unwrap(), retained);
        let mut revoked: Value = serde_json::from_slice(&registry).unwrap();
        revoked["peer"]["revoked"] = true.into();
        write(&options.home.join(REGISTRY), &revoked);
        assert!(
            setup_with(&options, |_, _| panic!(
                "revoked registration precedes credentials"
            ))
            .is_err()
        );
    }

    #[test]
    fn registry_change_during_token_lookup_is_preserved_without_stale_merge() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(dir.path().join("cloud"));
        setup_with(&options, |_, _| Ok(Some(TOKEN.into()))).unwrap();
        options.peer = "new-peer".into();
        let registry = options.home.join(REGISTRY);
        let policy = fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap();
        let mut changed: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        changed["concurrent-peer"] = json!({"mailbox":"concurrent-peer","revoked":false});
        let result = setup_with(&options, |_, create| {
            assert!(!create);
            write(&registry, &changed);
            Ok(Some(TOKEN.into()))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&registry).unwrap(), changed.to_string().as_bytes());
        assert_eq!(
            fs::read(options.home.join(catalog::codex_http().policy_file)).unwrap(),
            policy
        );
    }

    #[test]
    fn policy_rotation_during_token_lookup_cannot_report_matching_custody() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path().join("cloud"));
        setup_with(&options, |_, _| Ok(Some(TOKEN.into()))).unwrap();
        let path = options.home.join(catalog::codex_http().policy_file);
        let registry = fs::read(options.home.join(REGISTRY)).unwrap();
        let mut changed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        changed["principals"][0]["token_sha256"] = json!("f".repeat(64));
        let result = setup_with(&options, |_, create| {
            assert!(!create);
            write(&path, &changed);
            Ok(Some(TOKEN.into()))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), changed.to_string().as_bytes());
        assert_eq!(fs::read(options.home.join(REGISTRY)).unwrap(), registry);
    }
}
