//! Set up an independent checkout and detect installed app bundles without enabling access.
use crate::{
    cloud_credentials::{self, App, KEYCHAIN_SERVICE},
    mailbox_startup::{QUEUE, REGISTRATION as REGISTRY},
};
use pigeonmq::mailbox_catalog as catalog;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(crate) fn require_home(path: &Path) -> io::Result<PathBuf> {
    // Strip trailing separators and dot components before the no-follow open.
    let path: PathBuf = path.components().collect();
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Hub home must be an owner-private directory",
        ));
    }
    let canonical = path.canonicalize()?;
    let resolved = canonical.symlink_metadata()?;
    if (metadata.dev(), metadata.ino()) != (resolved.dev(), resolved.ino()) {
        return Err(io::Error::other("Hub home changed during validation"));
    }
    Ok(canonical)
}

fn create_private(path: &Path) -> io::Result<Option<fs::File>> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error),
    }
}

fn check_private(path: &Path) -> io::Result<()> {
    pigeonmq::private_file::open_private_file(path)
        .map(|_| ())
        .map_err(|_| io::Error::other("expected an owner-private hub file"))
}

pub(crate) fn create_settings(path: &Path, value: &Value) -> io::Result<bool> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let Some(mut file) = create_private(path)? else {
        check_private(path)?;
        return Ok(false);
    };
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(true)
}

#[cfg(target_os = "macos")]
const DOT_TUNNEL_DIR: &str = "pigeonmq-cloud-dot-secure-tunnel";

// Do not replace authenticated account admission with a plan, app, or user assertion.
// The public Dot and tunnel APIs reviewed for this command do not supply that signal.
// NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
pub(crate) fn dot_tunnel_setup() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Dot setup is unavailable: this CLI cannot verify Dot access and Secure MCP permission for the target ChatGPT workspace. No keys were read, files changed, or tunnels created. Use the official setup with an authorized workspace operator: https://platform.openai.com/settings/organization/tunnels. See https://learn.chatgpt.com/docs/dots for Dot access requirements. Rerunning this command cannot resolve the missing verification support.",
    ))
}

#[cfg(target_os = "macos")]
pub(crate) fn dot_tunnel_status(path: &Path) -> io::Result<()> {
    let home = require_home(path)?;
    let dir = home.join(DOT_TUNNEL_DIR);
    if !existing_dot_tunnel_inputs(&dir)? {
        println!("not-configured");
        return Ok(());
    }
    println!("{}", crate::cloud_tunnel_runtime::status(&dir)?);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn dot_tunnel_status(_: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Dot tunnel status needs macOS",
    ))
}

#[cfg(any(target_os = "macos", test))]
fn valid_tunnel_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(any(target_os = "macos", test))]
fn existing_dot_tunnel_inputs(dir: &Path) -> io::Result<bool> {
    use std::io::Read;
    match dir.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let invalid =
        || io::Error::other("Saved Dot tunnel inputs are incomplete or unsafe. No files changed.");
    let dir = require_home(dir).map_err(|_| invalid())?;
    let open =
        |name| pigeonmq::private_file::open_private_file(&dir.join(name)).map_err(|_| invalid());
    let settings: Value =
        serde_json::from_reader(open("settings.json")?.take(16385)).map_err(|_| invalid())?;
    if !settings["tunnel_id"].as_str().is_some_and(valid_tunnel_id)
        || settings["runtime_key_file"] != "runtime-key"
        || open("runtime-key")?.metadata()?.len() == 0
    {
        return Err(invalid());
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
#[expect(
    dead_code,
    reason = "dot-tunnel-setup needs authenticated workspace admission before preparing its mailbox"
)]
fn prepare_dot(home: &Path) -> io::Result<()> {
    let queue = home.join(QUEUE);
    check_private(&queue)?;
    let registration = home.join(REGISTRY);
    let registrations = pigeonmq::registration::load_registrations(&registration)
        .map_err(|(_, reason)| io::Error::other(reason))?;
    let policy = cloud_credentials::policy_path(home, App::Dot);

    if matches!(policy.symlink_metadata(), Err(ref error) if error.kind() == io::ErrorKind::NotFound)
    {
        let mut entries = serde_json::Map::new();
        for (id, entry) in registrations.0 {
            if (id == "dot" || entry.mailbox == "dot")
                && (id != "dot" || entry.mailbox != "dot" || entry.revoked)
            {
                return Err(io::Error::other(
                    "Existing Dot registration needs review. No changes made.",
                ));
            }
            entries.insert(
                id,
                json!({"mailbox": entry.mailbox, "revoked": entry.revoked}),
            );
        }
        if !entries.contains_key("dot") {
            entries.insert("dot".into(), json!({"mailbox": "dot", "revoked": false}));
            pigeonmq::private_file::replace_private(
                &registration,
                &serde_json::to_vec_pretty(&entries)?,
            )?;
        }
        create_settings(
            &policy,
            &json!({
                "principal": "pigeonmq-cloud-dot", "mailbox": "dot", "registration": registration,
                "token_sha256": "keychain", "enabled": true
            }),
        )?;
    }

    cloud_credentials::provision(&policy, App::Dot)?;
    pigeonmq::dot_profile::check(&queue, &policy, true)?;
    let bearer = cloud_credentials::read_keychain_bearer(home, App::Dot)?;
    let bearer =
        std::str::from_utf8(&bearer).map_err(|_| io::Error::other("Invalid Cloud bearer"))?;
    probe_dot(queue, policy, bearer)
}

#[cfg(any(target_os = "macos", test))]
// NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
fn probe_dot(queue: PathBuf, policy: PathBuf, bearer: &str) -> io::Result<()> {
    let host = pigeonmq::dot_profile::DotBackend::host(queue, policy);
    let context = pigeonmq::dot_mcp_host::Context::new(
        std::time::Instant::now() + std::time::Duration::from_secs(3),
        &|| false,
    );
    let authority = "127.0.0.1:54843";
    let route = App::Dot.http_profile().route;
    let headers = [
        ("Host".into(), authority.as_bytes().to_vec()),
        ("Content-Type".into(), b"application/json".to_vec()),
        (
            "Accept".into(),
            b"application/json, text/event-stream".to_vec(),
        ),
    ];
    let dispatch = |body: &[u8]| -> io::Result<Value> {
        let reply = host
            .dispatch_streamable_http((authority, route), &headers, bearer, body, &context)
            .map_err(|(_, reason)| io::Error::other(reason))?;
        if !reply.status().is_success() {
            return Err(io::Error::other("Local Dot MCP request failed"));
        }
        serde_json::from_slice(reply.body()).map_err(io::Error::other)
    };
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")}
        }
    });
    let initialize = dispatch(&serde_json::to_vec(&request)?)?;
    if initialize["result"]["serverInfo"]["name"] != "pigeonmq-cloud-dot" {
        return Err(io::Error::other("Local Dot MCP initialize failed"));
    }
    let listed = dispatch(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)?;
    if !listed["result"]["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        return Err(io::Error::other("Local Dot MCP tools/list failed"));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "dot-tunnel-setup may save verified tunnel inputs only after authenticated workspace admission"
    )
)]
fn save_dot_tunnel_inputs(dir: &Path, id: &str, key: &str) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(dir)?;
    let config = dir.join("settings.json");
    let credential = dir.join("runtime-key");
    let result = (|| {
        if !create_settings(
            &config,
            &json!({"tunnel_id": id, "runtime_key_file": "runtime-key"}),
        )? {
            return Err(io::Error::other("Dot tunnel settings already exist"));
        }
        let mut file = create_private(&credential)?
            .ok_or_else(|| io::Error::other("Dot runtime key already exists"))?;
        file.write_all(key.as_bytes())?;
        file.sync_all()
    })();
    if result.is_err() {
        // The directory was created exclusively by this command.
        let _ = fs::remove_file(&credential);
        let _ = fs::remove_file(&config);
        let _ = fs::remove_dir(dir);
    }
    result
}

fn app_bundle(roots: &[PathBuf], name: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        let bundle = root.join(format!("{name}.app"));
        let info = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
        (info.as_dictionary()?.get("CFBundleName")?.as_string()? == name).then_some(bundle)
    })
}

fn discover_at(home: &Path, app_roots: &[PathBuf]) -> io::Result<Vec<Value>> {
    let registry = home.join(REGISTRY);
    let mut apps = vec![];
    let chatgpt = app_bundle(app_roots, "ChatGPT");
    for app in catalog::http_profiles().iter().filter_map(App::for_profile) {
        let name = match app {
            App::Muse => "Muse",
            App::Dot => "Dot",
            App::GrokBot => "Grok Bot",
        };
        let bundle = if matches!(app, App::Dot) {
            chatgpt.clone()
        } else {
            app_bundle(app_roots, name)
        };
        let detection = match (&bundle, app) {
            (Some(_), App::Dot) => "candidate_found",
            (Some(_), _) => "found",
            (None, _) => "not_found",
        };
        let policy_path = cloud_credentials::policy_path(home, app);
        let policy_exists = match policy_path.symlink_metadata() {
            Ok(_) => {
                check_private(&policy_path)?;
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        let settings_path = home.join(format!("pigeonmq-cloud-{}-settings.json", app.slug()));
        let credential_field = match app {
            App::Muse => "Custom Connector: enter the bearer in the Secure Credentials Store.",
            App::Dot | App::GrokBot => {
                "Use an app-provided secure Bearer field if available. This setup does not verify one."
            }
        };
        let settings = json!({
            "connector": {
                "name": format!("pigeonmq-cloud-{}", app.slug()),
                "endpoint": {
                    "state_file": home.join("pigeonmq-cloud-state.json"),
                    "required_state": "connected",
                    "origin_field": "public_origin",
                    "route": app.http_profile().route,
                },
                "auth": {
                    "type": "bearer",
                    "keychain_service": KEYCHAIN_SERVICE,
                    "keychain_account": cloud_credentials::account(home, app)?,
                    "credential_field": credential_field,
                },
            },
            "server_policy": policy_path,
        });
        if !policy_exists {
            create_settings(&registry, &json!({}))?;
        }
        apps.push(settings_result(
            app.slug(),
            bundle,
            detection,
            &settings_path,
            &settings,
        )?);
    }
    let bundle = app_bundle(app_roots, "Codex").or_else(|| {
        let bundle = chatgpt?;
        let plist = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
        let id = plist
            .as_dictionary()?
            .get("CFBundleIdentifier")?
            .as_string()?;
        (id == "com.openai.codex").then_some(bundle)
    });
    let detection = if bundle.is_some() {
        "candidate_found"
    } else {
        "not_found"
    };
    let codex = json!({
        "connector": {
            "name": "pigeonmq-cloud-codex",
            "transport": "openai_secure_mcp_tunnel",
            "route": catalog::codex_http().route,
            "mailbox": catalog::codex().mailbox,
            "events": "pigeonmq.message.created",
        },
        "queue": home.join(QUEUE),
        "server_policy": home.join(catalog::codex_http().policy_file),
        "registration": registry,
        "events_prepare_command": "codex-prepare",
        "subscription_requirement": "Bind an authorized platform Events subscription to the selected Cloud conversation.",
    });
    apps.push(settings_result(
        catalog::codex().mailbox,
        bundle,
        detection,
        &home.join("pigeonmq-cloud-codex-settings.json"),
        &codex,
    )?);
    Ok(apps)
}

fn settings_result(
    app: &str,
    bundle: Option<PathBuf>,
    detection: &str,
    settings_path: &Path,
    settings: &Value,
) -> io::Result<Value> {
    let status = if create_settings(settings_path, settings)? {
        "created_settings"
    } else {
        "existing_settings"
    };
    Ok(
        json!({"app": app, "bundle": bundle, "detection": detection, "status": status, "settings": settings_path}),
    )
}

pub(crate) fn resolve_home(home: Option<PathBuf>) -> PathBuf {
    if let Some(h) = home {
        return h;
    }
    if let Ok(env_home) = std::env::var("PIGEONMQ_HOME") {
        let p = PathBuf::from(env_home);
        if p.is_absolute() {
            return p;
        }
    }
    let new_default = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".pigeonmq"))
        .unwrap_or_else(|| PathBuf::from(".pigeonmq"));
    if new_default.exists() {
        return new_default;
    }
    let legacy = PathBuf::from(".local");
    if legacy.exists() {
        return legacy;
    }
    new_default
}
pub(crate) fn quickstart(home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if home.exists() {
        return Err("quickstart_home_exists".into());
    }
    let (home, _) = initialize_home(home)?;
    let result = crate::reply_mailbox_setup::stage(&home, "instinct")?;
    println!("quickstart_home={}", home.display());
    println!("mcp_url=http://127.0.0.1:54843/instinct/mcp");
    println!("token_file={}", result["token_file"]);
    println!("next: pigeonmq run --home {} --local-only", home.display());
    Ok(())
}
pub(crate) fn initialize_home(path: &Path) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let home = require_home(path)?;
    let queue = home.join(QUEUE);
    let created = if create_private(&queue)?.is_some() {
        pigeonmq::Queue::open(&queue)?;
        true
    } else {
        check_private(&queue)?;
        pigeonmq::Queue::open_existing(&queue)?;
        false
    };
    Ok((home, created))
}

fn muse_settings(home: &Path, peer: &str) -> io::Result<PathBuf> {
    use std::io::Read;
    let registry = home.join(REGISTRY);
    let policy = cloud_credentials::policy_path(home, App::Muse);
    let policy_exists = match policy.symlink_metadata() {
        Ok(_) => {
            let file = pigeonmq::private_file::open_private_file(&policy)
                .map_err(|_| io::Error::other("Unsafe existing Muse policy"))?;
            let value: Value = serde_json::from_reader(file.take(16385))?;
            if value.get("senders").is_some() {
                return Err(io::Error::other(crate::auth::RetiredSenders));
            }
            if value["registration"] != json!(registry) {
                return Err(io::Error::other(
                    "Existing Muse policy uses a different registration file",
                ));
            }
            // Resume a policy left before Keychain provisioning completed.
            if value["token_sha256"] != "keychain" {
                crate::auth::validate_muse(&policy)
                    .map_err(|(_, reason)| crate::auth::local_policy_error(reason, reason))?;
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let entries = match registry.symlink_metadata() {
        Ok(_) => {
            pigeonmq::registration::load_registrations(&registry)
                .map_err(|(_, reason)| io::Error::other(reason))?
                .0
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
        Err(error) => return Err(error),
    };
    for name in ["muse", peer] {
        if entries.iter().any(|(id, entry)| {
            (id == name || entry.mailbox == name)
                && (id != name || entry.mailbox != name || entry.revoked)
        }) {
            return Err(io::Error::other(
                "Existing Muse or peer registration conflicts. No registration changed.",
            ));
        }
    }
    let missing = !entries.contains_key("muse") || !entries.contains_key(peer);
    if missing {
        let mut entries: serde_json::Map<String, Value> = entries
            .into_iter()
            .map(|(id, entry)| {
                (
                    id,
                    json!({"mailbox": entry.mailbox, "revoked": entry.revoked}),
                )
            })
            .collect();
        for name in ["muse", peer] {
            entries
                .entry(name.to_owned())
                .or_insert_with(|| json!({"mailbox": name, "revoked": false}));
        }
        pigeonmq::private_file::replace_private(&registry, &serde_json::to_vec_pretty(&entries)?)?;
    }
    if !policy_exists {
        create_settings(
            &policy,
            &json!({
                "principal": "muse", "mailbox": "muse", "token_sha256": "keychain",
                "operations": ["read", "send", "wait", "reply"],
                "enabled": true, "expires_at_ms": u64::MAX, "registration": registry,
            }),
        )?;
    }
    Ok(policy)
}

fn setup_muse_with(
    path: &Path,
    peer: &str,
    credential: impl FnOnce(&Path, &Path) -> io::Result<Value>,
) -> Result<Value, Box<dyn std::error::Error>> {
    if peer.is_empty()
        || peer.len() > 128
        || peer == "muse"
        || !peer
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err("Use a peer name of 1 to 128 letters, digits, hyphens, underscores, periods, or colons. It cannot be muse.".into());
    }
    let (home, queue_created) = initialize_home(path)?;
    let policy = muse_settings(&home, peer)?;
    let custody = credential(&home, &policy)?;
    let home_arg = format!("'{}'", home.to_string_lossy().replace('\'', "'\\''"));
    if custody["local_available"] != true {
        return Err(format!("Muse local_available=false. Import a surviving token:\n  pigeonmq credentials --home {home_arg} --app muse import --token-file /ABSOLUTE_PRIVATE_TOKEN_FILE\nIf no copy survives, explicitly reissue:\n  pigeonmq credentials --home {home_arg} --app muse reissue --token-file /ABSOLUTE_PRIVATE_NEW_TOKEN_FILE\nUpdate the connector after reissue.").into());
    }
    if custody["matches_policy"] != true {
        return Err(format!("Muse matches_policy=false. Check local custody:\n  pigeonmq credentials --home {home_arg} --app muse status\nIf Keychain is authoritative, explicitly select its existing token:\n  pigeonmq credentials --home {home_arg} --app muse reissue --token-file /ABSOLUTE_PRIVATE_TOKEN_FILE --use-stored-token\nUpdate the connector after repair.").into());
    }
    crate::local_http::prepare_muse(&home.join(QUEUE), &policy)?;
    if !crate::local_http::check_muse(&home.join(QUEUE), &policy)? {
        return Err("Muse storage remains unprepared".into());
    }
    let mut missing_tunnel_inputs = Vec::new();
    for name in [
        "pigeonmq-cloud-tunnel-ed25519",
        "pigeonmq-cloud-tunnel-known_hosts",
    ] {
        match home.join(name).symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing_tunnel_inputs.push(name)
            }
            Err(error) => return Err(error.into()),
            Ok(_) => check_private(&home.join(name))?,
        }
    }
    Ok(json!({"state": "ready_local_only", "home": home,
        "queue_created": queue_created, "policy": policy, "peer": peer,
        "local_credential_available": true, "matches_policy": true, "agent_use": "unverified",
        "runtime_verified": false, "missing_tunnel_inputs": missing_tunnel_inputs}))
}

pub(crate) fn setup_muse(path: &Path, peer: &str) -> Result<(), Box<dyn std::error::Error>> {
    let result = setup_muse_with(path, peer, |home, policy| {
        cloud_credentials::provision(policy, App::Muse)?;
        cloud_credentials::local_custody(home, App::Muse)
    })?;
    println!("{result}");
    Ok(())
}

fn grok_registration_snapshot(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use pigeonmq::private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file};
    use std::io::Read;

    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let file = open_private_file(path)
        .map_err(|_| io::Error::other("Grok setup requires owner-private registration"))?;
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PRIVATE_CONFIG_BYTES {
        return Err(io::Error::other("Cloud registration is too large"));
    }
    Ok(Some(bytes))
}

fn setup_grok_config(path: &Path, token_sha256: &str) -> io::Result<Value> {
    use pigeonmq::{private_file, registration::Registrations};

    if token_sha256.len() != 64 || !token_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "Use a 64-digit SHA-256 hex digest, not a Bearer token",
        ));
    }
    let home = require_home(path)?;
    let registration = home.join(REGISTRY);
    let registration_text = registration
        .to_str()
        .ok_or_else(|| io::Error::other("Hub home path must be UTF-8"))?;
    let policy = cloud_credentials::policy_path(&home, App::GrokBot);
    let grok = App::GrokBot.http_profile();
    let before = grok_registration_snapshot(&registration)?;
    let mut entries = match &before {
        Some(bytes) => {
            serde_json::from_slice::<Registrations>(bytes)
                .map_err(|_| io::Error::other("Invalid Cloud registration"))?
                .0
        }
        None => Default::default(),
    };
    if entries.iter().any(|(id, entry)| {
        (id == grok.id || entry.mailbox == grok.id)
            && (id != grok.id || entry.mailbox != grok.id || entry.revoked)
    }) {
        return Err(io::Error::other(
            "Existing Grok Bot registration conflicts. No registration changed.",
        ));
    }
    let registration_created = !entries.contains_key(grok.id);
    let update = if registration_created {
        entries.insert(
            grok.id.into(),
            pigeonmq::registration::RegistrationEntry {
                mailbox: grok.id.into(),
                revoked: false,
            },
        );
        let value = Value::Object(
            entries
                .into_iter()
                .map(|(id, entry)| (id, json!({"mailbox":entry.mailbox,"revoked":entry.revoked})))
                .collect(),
        );
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
            return Err(io::Error::other(
                "Adding Grok Bot would exceed the Cloud registration size limit",
            ));
        }
        Some((value, bytes))
    } else {
        None
    };
    let policy_created = create_settings(
        &policy,
        &json!({
            "principal":grok.id, "mailbox":grok.id, "token_sha256":token_sha256.to_ascii_lowercase(),
            "operations":["read","wait","reply","send"], "enabled":true,
            "expires_at_ms":u64::MAX, "registration":registration_text,
        }),
    )?;
    // Do not enable a central registration before checking the new or existing policy.
    crate::local_http::validate_grok_setup_policy(&policy, &registration, token_sha256).map_err(
        |(_, reason)| {
            crate::auth::local_policy_error(
                reason,
                "Existing Grok Bot policy is invalid or differs. No registration changed.",
            )
        },
    )?;
    if let Some((value, bytes)) = update {
        let changed = || {
            io::Error::other("Cloud registration changed during setup. Recheck it before retrying.")
        };
        if before.is_none() {
            if !create_settings(&registration, &value)? {
                return Err(changed());
            }
        } else {
            if grok_registration_snapshot(&registration)? != before {
                return Err(changed());
            }
            private_file::replace_private(&registration, &bytes)?;
        }
    }
    pigeonmq::registration::validate_registration(&registration, grok.id, grok.id)
        .map_err(|(_, reason)| io::Error::other(reason))?;
    Ok(
        json!({"state":"grok_configured_local_only", "policy":policy, "registration":registration,
        "policy_created":policy_created, "grok_registration_added":registration_created,
        "mailbox":grok.id, "route":grok.route, "runtime_verified":false}),
    )
}

pub(crate) fn setup_grok(path: &Path, token_sha256: &str) -> io::Result<()> {
    println!("{}", setup_grok_config(path, token_sha256)?);
    Ok(())
}

fn setup_at(
    path: &Path,
    init: bool,
    app_roots: &[PathBuf],
) -> Result<Value, Box<dyn std::error::Error>> {
    let (home, queue_created) = if init {
        initialize_home(path)?
    } else {
        (require_home(path)?, false)
    };
    let apps = discover_at(&home, app_roots)?;
    Ok(json!({"home": home, "queue_created": queue_created, "apps": apps}))
}

pub(crate) fn setup(path: &Path, init: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(user_home) = env::var_os("HOME") {
        roots.insert(0, PathBuf::from(user_home).join("Applications"));
    }
    println!("{}", setup_at(path, init, &roots)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_setup_and_named_lifecycle_share_the_prepared_home() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join(".local");
        let setup = setup_at(&home, true, &[]).unwrap();
        assert_eq!(setup["queue_created"], true);
        let options = crate::lifecycle::Options {
            profiles_file: None,
            email_recipient: None,
            relay_command: None,
            port: None,
            apply: false,
        };
        let staged = crate::lifecycle::init(&home, "cue", &options).unwrap();
        assert_eq!(staged["state"], "staged");
        assert_eq!(setup_at(&home, true, &[]).unwrap()["queue_created"], false);
        assert_eq!(
            crate::lifecycle::init(&home, "cue", &options).unwrap(),
            staged
        );
    }

    #[test]
    fn muse_setup_prepares_normal_permissions_and_preserves_existing_files() {
        use crate::auth::MuseOperation;
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        let (home, created) = initialize_home(&temp.path().join("cloud")).unwrap();
        assert!(created);
        create_settings(
            &home.join(REGISTRY),
            &json!({
                "other": {"mailbox": "other", "revoked": false}
            }),
        )
        .unwrap();
        let policy = muse_settings(&home, "cloud-test-peer").unwrap();
        assert_eq!(muse_settings(&home, "cloud-test-peer").unwrap(), policy);
        let mut value: Value = serde_json::from_reader(fs::File::open(&policy).unwrap()).unwrap();
        assert_eq!(value["token_sha256"], "keychain");
        assert!(value.get("senders").is_none());
        let hash: String = Sha256::digest("synthetic-muse-setup-only")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        value["token_sha256"] = json!(hash);
        pigeonmq::private_file::replace_private(&policy, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        crate::local_http::prepare_muse(&home.join(QUEUE), &policy).unwrap();
        assert!(crate::local_http::check_muse(&home.join(QUEUE), &policy).unwrap());
        for operation in [
            MuseOperation::Read,
            MuseOperation::Send,
            MuseOperation::Wait,
            MuseOperation::Reply,
        ] {
            let scope = crate::auth::local_muse_scope(&policy, operation).unwrap();
            assert!(scope.allowed_senders.contains(&"cloud-test-peer".into()));
            assert!(scope.allowed_senders.contains(&"other".into()));
        }
        let before_policy = fs::read(&policy).unwrap();
        let before_registry = fs::read(home.join(REGISTRY)).unwrap();
        assert!(!initialize_home(&home).unwrap().1);
        for _ in 0..2 {
            let result = setup_muse_with(&home, "cloud-test-peer", |_, _| {
                Ok(json!({"local_available": true, "matches_policy": true}))
            })
            .unwrap();
            assert_eq!(result["state"], "ready_local_only");
            assert_eq!(result["local_credential_available"], true);
            assert_eq!(result["matches_policy"], true);
            assert_eq!(result["agent_use"], "unverified");
        }
        assert_eq!(fs::read(&policy).unwrap(), before_policy);
        assert_eq!(fs::read(home.join(REGISTRY)).unwrap(), before_registry);
        for file in [QUEUE, REGISTRY, "pigeonmq-cloud-muse-policy.json"] {
            assert_eq!(fs::metadata(home.join(file)).unwrap().mode() & 0o777, 0o600);
        }
        value["operations"] = json!(["read", "reply"]);
        pigeonmq::private_file::replace_private(&policy, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        let restricted = fs::read(&policy).unwrap();
        muse_settings(&home, "cloud-test-peer").unwrap();
        assert_eq!(fs::read(&policy).unwrap(), restricted);
        assert!(matches!(
            crate::auth::local_muse_scope(&policy, MuseOperation::Send),
            Err((403, "forbidden"))
        ));

        value["senders"] = json!(["legacy-fixture-peer"]);
        value["token_sha256"] = json!("keychain");
        let obsolete = serde_json::to_vec(&value).unwrap();
        pigeonmq::private_file::replace_private(&policy, &obsolete).unwrap();
        let error = setup_muse_with(&home, "unregistered-fixture-peer", |_, _| {
            panic!("legacy policy must fail before credential provisioning")
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(crate::auth::RETIRED_SENDERS_HELP)
        );
        assert_eq!(fs::read(&policy).unwrap(), obsolete);
        assert_eq!(fs::read(home.join(REGISTRY)).unwrap(), before_registry);
    }

    #[test]
    fn muse_setup_requires_matching_local_custody_for_existing_policy() {
        let temp = tempfile::tempdir().unwrap();
        let (home, _) = initialize_home(&temp.path().join("cloud")).unwrap();
        let policy = muse_settings(&home, "cloud-test-peer").unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&policy).unwrap()).unwrap();
        value["token_sha256"] = json!("a".repeat(64));
        pigeonmq::private_file::replace_private(&policy, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        let before_policy = fs::read(&policy).unwrap();
        let before_registry = fs::read(home.join(REGISTRY)).unwrap();
        for available in [false, true] {
            let result = setup_muse_with(&home, "cloud-test-peer", |_, _| {
                Ok(json!({"local_available": available, "matches_policy": false}))
            });
            let error = result.unwrap_err().to_string();
            assert!(error.contains("credentials --home"));
            assert!(error.contains(if available {
                "matches_policy=false"
            } else {
                "local_available=false"
            }));
            assert_eq!(fs::read(&policy).unwrap(), before_policy);
            assert_eq!(fs::read(home.join(REGISTRY)).unwrap(), before_registry);
            assert!(!crate::local_http::check_muse(&home.join(QUEUE), &policy).unwrap());
        }
    }

    #[test]
    fn muse_setup_rejects_registration_conflicts_without_replacing_them() {
        let temp = tempfile::tempdir().unwrap();
        let (home, _) = initialize_home(&temp.path().join("cloud")).unwrap();
        for conflict in [
            json!({"muse": {"mailbox": "muse", "revoked": true}}),
            json!({"cloud-test-peer": {"mailbox": "other", "revoked": false}}),
            json!({"alias": {"mailbox": "muse", "revoked": false}}),
        ] {
            let bytes = serde_json::to_vec(&conflict).unwrap();
            pigeonmq::private_file::replace_private(&home.join(REGISTRY), &bytes).unwrap();
            assert!(muse_settings(&home, "cloud-test-peer").is_err());
            assert_eq!(fs::read(home.join(REGISTRY)).unwrap(), bytes);
            assert!(!cloud_credentials::policy_path(&home, App::Muse).exists());
        }
        let absent = temp.path().join("absent");
        for peer in ["", "muse", "bad/name"] {
            assert!(setup_muse(&absent, peer).is_err());
            assert!(!absent.exists());
        }
    }

    #[test]
    fn dot_setup_requires_authenticated_workspace_admission() {
        let error = dot_tunnel_setup().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("cannot verify Dot access"));
    }

    #[test]
    fn dot_tunnel_inputs_distinguish_missing_saved_and_incomplete() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("private-tunnel");
        assert!(!existing_dot_tunnel_inputs(&dir).unwrap());
        save_dot_tunnel_inputs(&dir, "tunnel_example", "example-only").unwrap();
        assert!(existing_dot_tunnel_inputs(&dir).unwrap());
        fs::remove_file(dir.join("runtime-key")).unwrap();
        assert!(existing_dot_tunnel_inputs(&dir).is_err());
        assert!(dir.join("settings.json").exists());
    }

    #[test]
    fn dot_preflight_checks_authenticated_tools_without_a_tunnel() {
        use sha2::{Digest, Sha256};
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap().join("runtime");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let queue = home.join(QUEUE);
        create_private(&queue).unwrap().unwrap();
        pigeonmq::Queue::open(&queue).unwrap();
        let registration = home.join(REGISTRY);
        create_settings(
            &registration,
            &json!({"dot":{"mailbox":"dot","revoked":false}}),
        )
        .unwrap();
        let policy = cloud_credentials::policy_path(&home, App::Dot);
        let bearer = "synthetic-dot-preflight-only";
        let digest: String = Sha256::digest(bearer)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        create_settings(
            &policy,
            &json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot",
            "registration":registration,"enabled":true,"token_sha256":digest}),
        )
        .unwrap();
        pigeonmq::dot_profile::check(&queue, &policy, true).unwrap();
        probe_dot(queue.clone(), policy.clone(), bearer).unwrap();
        assert!(probe_dot(queue, policy, "wrong").is_err());
    }

    #[test]
    fn bare_init_and_discover_reuse_one_dot_draft_with_duplicate_app_bundles() {
        let temp = tempfile::tempdir().unwrap();
        let temp = temp.path().canonicalize().unwrap();
        let home = temp.join("runtime");
        let roots = [
            temp.join("user/Applications"),
            temp.join("system/Applications"),
        ];
        let write_bundle = |root: &Path| {
            let contents = root.join("ChatGPT.app/Contents");
            fs::create_dir_all(&contents).unwrap();
            let mut bundle = plist::Dictionary::new();
            bundle.insert("CFBundleName".into(), "ChatGPT".into());
            bundle.insert(
                "CFBundleIdentifier".into(),
                "com.example.synthetic-chatgpt".into(),
            );
            plist::Value::Dictionary(bundle)
                .to_file_xml(contents.join("Info.plist"))
                .unwrap();
        };
        for root in &roots {
            write_bundle(root);
        }

        let assert_dot = |result: &Value, root: &Path, status: &str| {
            let mut dots = result["apps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|app| app["app"] == "dot");
            let dot = dots.next().unwrap();
            assert!(dots.next().is_none());
            assert_eq!(dot["detection"], "candidate_found");
            assert_eq!(dot["bundle"], json!(root.join("ChatGPT.app")));
            assert_eq!(dot["status"], status);
            assert_eq!(
                dot["settings"],
                json!(home.join("pigeonmq-cloud-dot-settings.json"))
            );
        };

        let initial = setup_at(&home, true, &roots).unwrap();
        assert_eq!(initial["queue_created"], true);
        assert_dot(&initial, &roots[0], "created_settings");

        let settings = home.join("pigeonmq-cloud-dot-settings.json");
        let dot_draft: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(dot_draft["connector"]["endpoint"]["route"], "/dot/mcp");
        assert_eq!(fs::metadata(&settings).unwrap().mode() & 0o777, 0o600);
        fs::write(&settings, b"operator's existing Dot connector draft\n").unwrap();
        let snapshot = || {
            fs::read_dir(&home)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();
        assert_eq!(before.len(), 6); // Queue, registration, and four drafts.

        for init in [false, true, false] {
            let repeated = setup_at(&home, init, &roots).unwrap();
            assert_eq!(repeated["queue_created"], false);
            assert_dot(&repeated, &roots[0], "existing_settings");
            assert!(
                repeated["apps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|app| app["status"] == "existing_settings")
            );
            assert_eq!(snapshot(), before);
        }

        fs::remove_dir_all(roots[0].join("ChatGPT.app")).unwrap();
        let fallback = setup_at(&home, false, &roots).unwrap();
        assert_dot(&fallback, &roots[1], "existing_settings");
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn discovery_only_creates_private_connector_settings() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("runtime");
        fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
        let applications = temp.path().join("Apps");
        let contents = applications.join("Muse.app/Contents");
        fs::create_dir_all(&contents).unwrap();
        let mut bundle = plist::Dictionary::new();
        bundle.insert("CFBundleName".into(), "Muse".into());
        plist::Value::Dictionary(bundle)
            .to_file_xml(contents.join("Info.plist"))
            .unwrap();

        let roots = &[applications];
        let result = discover_at(&home, roots).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["detection"], "found");
        assert_eq!(result[0]["status"], "created_settings");
        assert_eq!(result[1]["detection"], "not_found");
        assert_eq!(result[1]["status"], "created_settings");
        assert_eq!(result[2]["detection"], "not_found");
        assert_eq!(result[2]["status"], "created_settings");
        assert_eq!(result[3]["detection"], "not_found");
        assert_eq!(result[3]["status"], "created_settings");
        let codex: Value = serde_json::from_slice(
            &fs::read(home.join("pigeonmq-cloud-codex-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(codex["queue"], json!(home.join(QUEUE)));
        assert_eq!(codex["connector"]["route"], "/codex/mcp");
        assert_eq!(codex["connector"]["transport"], "openai_secure_mcp_tunnel");
        assert_eq!(codex["connector"]["events"], "pigeonmq.message.created");
        assert!(codex.get("run_command").is_none());
        assert!(codex.get("run_arguments").is_none());
        assert_eq!(codex["events_prepare_command"], "codex-prepare");
        assert!(codex.get("bridge").is_none());
        let settings = home.join("pigeonmq-cloud-muse-settings.json");
        let registry = home.join(REGISTRY);
        assert_eq!(fs::metadata(&settings).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&registry).unwrap().mode() & 0o777, 0o600);
        let draft: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(draft["connector"]["name"], "pigeonmq-cloud-muse");
        assert_eq!(draft["connector"]["endpoint"]["route"], "/muse/mcp");
        assert_eq!(
            draft["connector"]["auth"]["keychain_service"],
            KEYCHAIN_SERVICE
        );
        assert!(!home.join("pigeonmq-cloud-muse-policy.json").exists());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&registry).unwrap()).unwrap(),
            json!({})
        );

        let contents = roots[0].join("ChatGPT.app/Contents");
        fs::create_dir_all(&contents).unwrap();
        let mut bundle = plist::Dictionary::new();
        bundle.insert("CFBundleName".into(), "ChatGPT".into());
        bundle.insert("CFBundleIdentifier".into(), "com.openai.codex".into());
        plist::Value::Dictionary(bundle)
            .to_file_xml(contents.join("Info.plist"))
            .unwrap();

        fs::write(&settings, b"operator settings").unwrap();
        fs::write(&registry, b"operator registry").unwrap();
        let result = discover_at(&home, roots).unwrap();
        assert_eq!(result[0]["status"], "existing_settings");
        for index in [1, 3] {
            assert_eq!(result[index]["detection"], "candidate_found");
            assert_eq!(result[index]["bundle"], json!(roots[0].join("ChatGPT.app")));
            assert_eq!(result[index]["status"], "existing_settings");
        }
        assert_eq!(fs::read(&settings).unwrap(), b"operator settings");
        assert_eq!(fs::read(&registry).unwrap(), b"operator registry");
    }
}

pub(crate) fn reserve_address(home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let base_url = std::env::var("PIGEONMQ_REGISTRATION_URL")
        .map_err(|_| "registration_url_required".to_string())?;
    let cloudflared = crate::hosted_connector::preflight(None)
        .map_err(|e| e.to_string())?;
    let url = crate::hosted_connector::hosted_flow(home, &base_url, &cloudflared)
        .map_err(|e| e.to_string())?;
    println!("reserved_address={}", url);
    Ok(())
}
