//! Owner-private queue fixtures and fake launchctl only.
use super::*;
use pigeonmq::{Envelope, Queue, dot_profile, grok_mailbox, muse_mailbox};
use std::{
    cell::{Cell, RefCell},
    os::unix::fs::PermissionsExt,
};

const NAMES: [&str; 6] = ["dot", "codex-cloud", "muse", "grok-bot", "cue", "instinct"];

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Bootstrap(String),
    Bootout(String),
}

#[derive(Default)]
struct FakeLaunchctl {
    actions: RefCell<Vec<Action>>,
    loaded: RefCell<BTreeMap<String, PathBuf>>,
    fail_next_readiness: Cell<bool>,
    ready_routes: RefCell<Vec<Vec<String>>>,
}

impl LaunchControl for FakeLaunchctl {
    fn loaded(&self, label: &str, expected_plist: &Path) -> io::Result<bool> {
        match self.loaded.borrow().get(label) {
            Some(path) if path == expected_plist => Ok(true),
            Some(_) => Err(error("different fake launchd plist")),
            None => Ok(false),
        }
    }

    fn hub_ready(&self, plist: &Path, _: u16, routes: &[String]) -> io::Result<()> {
        if !self.loaded(HUB_LABEL, plist)? {
            return Err(error("fake hub was not loaded"));
        }
        self.ready_routes.borrow_mut().push(routes.to_vec());
        if self.fail_next_readiness.replace(false) {
            return Err(error("fake loopback did not bind"));
        }
        Ok(())
    }

    fn bootstrap(&self, path: &Path) -> io::Result<()> {
        let job = plist::Value::from_file(path).map_err(io::Error::other)?;
        let label = job
            .as_dictionary()
            .and_then(|values| values.get("Label"))
            .and_then(plist::Value::as_string)
            .ok_or_else(|| error("invalid fake launchd plist"))?;
        if self.loaded.borrow().contains_key(label) {
            return Err(error("fake launchd job already loaded"));
        }
        self.loaded.borrow_mut().insert(label.into(), path.into());
        self.actions
            .borrow_mut()
            .push(Action::Bootstrap(label.into()));
        Ok(())
    }

    fn bootout(&self, label: &str) -> io::Result<()> {
        if self.loaded.borrow_mut().remove(label).is_none() {
            return Err(error("fake launchd job not loaded"));
        }
        self.actions
            .borrow_mut()
            .push(Action::Bootout(label.into()));
        Ok(())
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    agents: PathBuf,
    fake: FakeLaunchctl,
    executable: PathBuf,
    relay_command: PathBuf,
}

impl Fixture {
    fn new(legacy: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let home = root_path.join("hub");
        mkdir_private(&home).unwrap();
        let registry = home.join(REGISTRATION);
        let mut registrations = serde_json::Map::new();
        for name in NAMES.into_iter().chain(["synthetic-peer"]) {
            registrations.insert(name.into(), json!({"mailbox":name,"revoked":false}));
        }
        write_json(&registry, &Value::Object(registrations)).unwrap();

        for (index, name) in NAMES.into_iter().enumerate() {
            let digest = format!("{:064x}", index + 1);
            let operations = ["read", "wait", "reply", "send"];
            let policy = match name {
                "dot" => json!({
                    "principal":"synthetic-dot","mailbox":"dot","token_sha256":digest,
                    "enabled":true,"registration":registry,
                }),
                "muse" | "grok-bot" => json!({
                    "principal":name,"mailbox":name,"token_sha256":digest,"operations":operations,
                    "enabled":true,"expires_at_ms":u64::MAX,"registration":registry,
                }),
                _ => json!({"principals":[{
                    "id":name,"mailboxes":[name],"token_sha256":digest,"operations":operations,
                    "send_to":[],"revoked":false,
                }]}),
            };
            let path = home.join(catalog::http_by_id(name).unwrap().policy_file);
            write_json(&path, &policy).unwrap();
        }
        let db = home.join(QUEUE);
        let mut queue = Queue::open(&db).unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        for name in NAMES {
            queue
                .send(Envelope {
                    sender: "synthetic-peer".into(),
                    recipient: name.into(),
                    body: "synthetic pending request".into(),
                    correlation_id: None,
                    in_reply_to: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        drop(queue);
        dot_profile::check(
            &db,
            &home.join(catalog::http_by_id("dot").unwrap().policy_file),
            true,
        )
        .unwrap();
        muse_mailbox::prepare(&db).unwrap();
        grok_mailbox::prepare(&db).unwrap();
        if legacy {
            Connection::open(&db)
                .unwrap()
                .execute_batch("DROP TABLE pigeonmq_mailbox_generations")
                .unwrap();
        }

        let relay_command = home.join("synthetic-relay");
        fs::write(&relay_command, b"#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&relay_command, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _root: root,
            home,
            agents: root_path.join("Library/LaunchAgents"),
            fake: FakeLaunchctl::default(),
            executable: root_path.join("bin/pigeonmq"),
            relay_command,
        }
    }

    fn jobs(&self) -> Jobs<'_> {
        Jobs {
            service: &self.fake,
            directory: &self.agents,
            executable: &self.executable,
            installation: None,
        }
    }

    fn protected_bytes(&self) -> Vec<Vec<u8>> {
        [QUEUE, REGISTRATION]
            .into_iter()
            .chain(NAMES.map(|name| catalog::http_by_id(name).unwrap().policy_file))
            .map(|file| fs::read(self.home.join(file)).unwrap())
            .collect()
    }

    fn messages(&self) -> Vec<(String, String)> {
        let connection =
            Connection::open_with_flags(self.home.join(QUEUE), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mut select = connection
            .prepare("SELECT recipient,body FROM main.messages ORDER BY seq")
            .unwrap();
        select
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn options<'a>(&'a self, apply: bool, email_recipient: Option<&'a str>) -> Options<'a> {
        Options {
            profiles_file: None,
            email_recipient,
            relay_command: email_recipient.map(|_| self.relay_command.as_path()),
            port: None,
            apply,
        }
    }
}

#[test]
fn adopts_all_six_profiles_idempotently_and_changes_relay_plist_only_at_edges() {
    let fixture = Fixture::new(false);
    let jobs = fixture.jobs();
    let before = fixture.protected_bytes();
    for name in NAMES {
        let email = match name {
            "cue" => Some("cue@example.invalid"),
            "instinct" => Some("instinct@example.invalid"),
            _ => None,
        };
        let staged = init_with(&fixture.home, name, &fixture.options(false, email), None).unwrap();
        assert_eq!(staged["state"], "staged");
        assert!(staged.get("token_file").is_none());
    }
    assert!(!fixture.agents.exists());
    assert!(fixture.fake.actions.borrow().is_empty());
    assert_eq!(fixture.protected_bytes(), before);

    let hub_plist = fixture.agents.join(format!("{HUB_LABEL}.plist"));
    let relay_plist = fixture.agents.join(format!("{RELAY_LABEL}.plist"));
    let mut relay_bytes = None;
    for name in NAMES {
        let active = init_with(
            &fixture.home,
            name,
            &fixture.options(true, None),
            Some(&jobs),
        )
        .unwrap();
        assert_eq!(active["state"], "active");
        assert!(active.get("token_file").is_none());
        assert!(hub_plist.is_file());
        if name == "cue" {
            relay_bytes = Some(fs::read(&relay_plist).unwrap());
        } else if name == "instinct" {
            assert_eq!(Some(fs::read(&relay_plist).unwrap()), relay_bytes);
        } else {
            assert!(!relay_plist.exists());
        }
    }

    let active = fixture.home.join(DIRECTORY).join(STATE);
    let active_config = fs::read(active).unwrap();
    let hub_bytes = fs::read(&hub_plist).unwrap();
    let actions_before = fixture.fake.actions.borrow().len();
    assert_eq!(
        fixture
            .fake
            .actions
            .borrow()
            .iter()
            .filter(|action| { matches!(action, Action::Bootstrap(label) if label == RELAY_LABEL) })
            .count(),
        1
    );
    assert!(
        !fixture
            .fake
            .actions
            .borrow()
            .contains(&Action::Bootout(RELAY_LABEL.into()))
    );
    for name in NAMES {
        assert_eq!(
            init_with(&fixture.home, name, &fixture.options(false, None), None).unwrap()["state"],
            "active"
        );
        assert_eq!(
            init_with(
                &fixture.home,
                name,
                &fixture.options(true, None),
                Some(&jobs)
            )
            .unwrap()["state"],
            "active"
        );
    }
    assert_eq!(fixture.fake.actions.borrow().len(), actions_before);
    assert_eq!(fixture.protected_bytes(), before);
    assert_eq!(
        fs::read(fixture.home.join(DIRECTORY).join(STATE)).unwrap(),
        active_config
    );
    assert_eq!(fs::read(&hub_plist).unwrap(), hub_bytes);
    assert_eq!(Some(fs::read(&relay_plist).unwrap()), relay_bytes);
    assert!(
        init_with(
            &fixture.home,
            "cue",
            &fixture.options(true, Some("different@example.invalid")),
            Some(&jobs),
        )
        .is_err()
    );
    assert_eq!(fixture.fake.actions.borrow().len(), actions_before);

    let backlog = fixture.messages();
    let action_start = fixture.fake.actions.borrow().len();
    for name in ["cue", "instinct", "dot", "codex-cloud", "muse", "grok-bot"] {
        assert_eq!(
            remove_with(&fixture.home, name, Some(&jobs)).unwrap()["state"],
            "retired"
        );
        assert_eq!(fixture.messages(), backlog);
        assert_eq!(fs::read(&hub_plist).unwrap(), hub_bytes);
        if name == "cue" {
            assert_eq!(Some(fs::read(&relay_plist).unwrap()), relay_bytes);
            assert!(
                !fixture.fake.actions.borrow()[action_start..]
                    .contains(&Action::Bootout(RELAY_LABEL.into()))
            );
        } else {
            assert!(!relay_plist.exists());
        }
    }
    assert_eq!(
        fixture.fake.actions.borrow()[action_start..]
            .iter()
            .filter(|action| **action == Action::Bootout(RELAY_LABEL.into()))
            .count(),
        1
    );
    let registrations: Value = read_json(&fixture.home.join(REGISTRATION)).unwrap();
    for name in NAMES {
        assert_eq!(registrations[name]["revoked"], true);
    }
}

#[test]
fn recognizes_all_six_legacy_profiles_and_refuses_writes_or_launchd_changes() {
    let fixture = Fixture::new(true);
    let jobs = fixture.jobs();
    let before = fixture.protected_bytes();
    for name in NAMES {
        let existing = init_with(&fixture.home, name, &fixture.options(false, None), None).unwrap();
        assert_eq!(existing["state"], "existing");
        assert_eq!(existing["lifecycle"], "read_only");
        let apply = init_with(
            &fixture.home,
            name,
            &fixture.options(true, None),
            Some(&jobs),
        );
        assert!(
            apply
                .unwrap_err()
                .to_string()
                .contains("generation catalog")
        );
        let remove = remove_with(&fixture.home, name, Some(&jobs));
        assert!(
            remove
                .unwrap_err()
                .to_string()
                .contains("generation catalog")
        );
    }
    assert!(!fixture.home.join(DIRECTORY).exists());
    assert!(!fixture.agents.exists());
    assert_eq!(fixture.protected_bytes(), before);
    assert!(fixture.fake.actions.borrow().is_empty());
    assert!(fixture.fake.loaded.borrow().is_empty());
}

#[test]
fn failed_hub_readiness_keeps_the_new_profile_staged_and_restores_ready_routes() {
    let fixture = Fixture::new(false);
    let jobs = fixture.jobs();
    init_with(
        &fixture.home,
        "muse",
        &fixture.options(true, None),
        Some(&jobs),
    )
    .unwrap();
    init_with(
        &fixture.home,
        "cue",
        &fixture.options(false, Some("cue@example.invalid")),
        None,
    )
    .unwrap();
    let before = fixture.protected_bytes();
    let hub_plist = fixture.agents.join(format!("{HUB_LABEL}.plist"));
    let original_plist = fs::read(&hub_plist).unwrap();

    fixture.fake.fail_next_readiness.set(true);
    assert!(
        init_with(
            &fixture.home,
            "cue",
            &fixture.options(true, None),
            Some(&jobs)
        )
        .is_err()
    );
    let staged = init_with(&fixture.home, "cue", &fixture.options(false, None), None).unwrap();
    assert_eq!(staged["state"], "staged");
    assert_eq!(staged["runtime_verified"], false);
    assert_eq!(
        fixture.fake.ready_routes.borrow().last().unwrap(),
        &["/muse/mcp"]
    );
    assert_eq!(fixture.protected_bytes(), before);
    assert_eq!(fs::read(&hub_plist).unwrap(), original_plist);
    assert!(!fixture.agents.join(format!("{RELAY_LABEL}.plist")).exists());

    let activated = init_with(
        &fixture.home,
        "cue",
        &fixture.options(true, None),
        Some(&jobs),
    )
    .unwrap();
    assert_eq!(activated["state"], "active");
    assert_eq!(activated["runtime_verified"], true);
    assert_eq!(
        fixture.fake.ready_routes.borrow().last().unwrap(),
        &["/cue/mcp", "/muse/mcp"]
    );
    assert!(
        fixture
            .agents
            .join(format!("{RELAY_LABEL}.plist"))
            .is_file()
    );
}

#[test]
fn managed_executable_checks_allow_installed_paths_and_refuse_disposable_binaries() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let volatile = root.join("volatile");
    let installation = InstallPaths {
        home: home.clone(),
        temporary: vec![volatile.clone()],
    };
    let executable = |path: &Path| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    };

    let installed = home.join(".local/libexec/pigeonmq/pigeonmq");
    executable(&installed);
    fs::write(home.join(".git"), "dotfiles worktree fixture").unwrap();
    assert_eq!(
        installation.require_executable(&installed).unwrap(),
        installed
    );

    let prefix = root.join("package-prefix");
    let package = prefix.join("installed/pigeonmq/0.1.3/bin/pigeonmq");
    executable(&package);
    fs::create_dir(prefix.join(".git")).unwrap();
    fs::write(
        package
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("INSTALL_RECEIPT.json"),
        b"{}",
    )
    .unwrap();
    assert_eq!(installation.require_executable(&package).unwrap(), package);

    let worktree = root.join("checkout");
    let cargo_dir = root.join("alternate-cargo-profile");
    let outside = volatile.join("bin/pigeonmq");
    for rejected in [
        worktree.join("tools/pigeonmq"),
        cargo_dir.join("pigeonmq"),
        home.join("scratch-build/relay"),
        home.join("tmp/relay"),
        home.join("target/release/pigeonmq"),
        outside.clone(),
    ] {
        executable(&rejected);
    }
    fs::write(worktree.join(".git"), "gitdir: synthetic").unwrap();
    fs::create_dir(cargo_dir.join(".fingerprint")).unwrap();
    let alias = home.join(".local/alias");
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    let temporary_alias = volatile.join("installed-alias");
    std::os::unix::fs::symlink(&installed, &temporary_alias).unwrap();
    for rejected in [
        worktree.join("tools/pigeonmq"),
        cargo_dir.join("pigeonmq"),
        home.join("scratch-build/relay"),
        home.join("tmp/relay"),
        home.join("target/release/pigeonmq"),
        outside,
        alias,
        temporary_alias,
    ] {
        assert!(
            installation.require_executable(&rejected).is_err(),
            "{rejected:?}"
        );
    }
}

#[test]
fn managed_apply_refuses_a_persisted_disposable_relay_before_launchd_changes() {
    let fixture = Fixture::new(false);
    let staged = init_with(
        &fixture.home,
        "cue",
        &fixture.options(false, Some("cue@example.invalid")),
        None,
    )
    .unwrap();
    assert_eq!(staged["state"], "staged");
    let installation = InstallPaths {
        home: fixture.home.clone(),
        temporary: vec![fixture.home.clone()],
    };
    let jobs = Jobs {
        installation: Some(&installation),
        ..fixture.jobs()
    };
    let result = init_with(
        &fixture.home,
        "cue",
        &fixture.options(true, None),
        Some(&jobs),
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("durable installed executable")
    );
    assert!(!fixture.agents.exists());
    assert!(fixture.fake.actions.borrow().is_empty());
    assert_eq!(
        init_with(&fixture.home, "cue", &fixture.options(false, None), None).unwrap(),
        staged
    );
}

#[test]
fn hosted_unix_opt_in_stages_until_match_then_schedules_and_retires_only_owned_job() {
    let fixture = Fixture::new(false);
    let jobs = fixture.jobs();
    init_with(
        &fixture.home,
        "cue",
        &fixture.options(true, None),
        Some(&jobs),
    )
    .unwrap();
    init_with(
        &fixture.home,
        "instinct",
        &fixture.options(true, None),
        Some(&jobs),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4();
    let cloudflared = fixture.home.join("synthetic-cloudflared");
    let self_managed = fixture.agents.join("test.self-managed.plist");
    fs::write(&self_managed, b"unchanged").unwrap();
    fixture
        .fake
        .loaded
        .borrow_mut()
        .insert("test.self-managed".into(), self_managed.clone());
    let baseline = fixture.fake.actions.borrow().len();
    crate::hosted_enrollment::stage_unix_fixture(
        &fixture.home,
        id,
        DEFAULT_PORT + 1,
        &["/cue/mcp"],
    );
    assert!(!schedule_hosted_with(&jobs, &fixture.home, &cloudflared).unwrap());
    assert_eq!(fixture.fake.actions.borrow().len(), baseline);
    assert!(read_hosted_owner(&fixture.home).unwrap().is_none());

    crate::hosted_enrollment::stage_unix_fixture(&fixture.home, id, DEFAULT_PORT, &["/cue/mcp"]);
    assert!(schedule_hosted_with(&jobs, &fixture.home, &cloudflared).unwrap());
    let plist = fixture.agents.join(format!("{HOSTED_LABEL}.plist"));
    let data = plist::Value::from_file(&plist).unwrap();
    let dict = data.as_dictionary().unwrap();
    let args = dict["ProgramArguments"].as_array().unwrap();
    assert_eq!(
        args.iter()
            .map(|value| value.as_string().unwrap())
            .collect::<Vec<_>>(),
        [
            fixture.executable.to_str().unwrap(),
            "serve-hosted-connector",
            "--home",
            fixture.home.to_str().unwrap()
        ]
    );
    assert_eq!(dict["KeepAlive"].as_boolean(), Some(true));
    #[cfg(target_os = "macos")]
    assert!(
        std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&plist)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        &fixture.fake.actions.borrow()[baseline..],
        [
            Action::Bootout(HUB_LABEL.into()),
            Action::Bootstrap(HUB_LABEL.into()),
            Action::Bootstrap(HOSTED_LABEL.into()),
        ]
    );
    let count = fixture.fake.actions.borrow().len();
    assert!(schedule_hosted_with(&jobs, &fixture.home, &cloudflared).unwrap());
    assert_eq!(fixture.fake.actions.borrow().len(), count);

    let queue = fixture.home.join(QUEUE);
    let inode = queue.metadata().unwrap().ino();
    remove_with(&fixture.home, "cue", Some(&jobs)).unwrap();
    assert!(!fixture.fake.loaded.borrow().contains_key(HOSTED_LABEL));
    assert!(!plist.exists());
    assert!(read_hosted_owner(&fixture.home).unwrap().is_none());
    assert!(
        crate::hosted_enrollment::staged_unix(&fixture.home)
            .unwrap()
            .is_some()
    );
    assert_eq!(queue.metadata().unwrap().ino(), inode);
    assert_eq!(
        fixture.fake.loaded.borrow().get("test.self-managed"),
        Some(&self_managed)
    );
    assert_eq!(fs::read(self_managed).unwrap(), b"unchanged");
    assert!(fixture.fake.loaded.borrow().contains_key(HUB_LABEL));
}

#[test]
fn hosted_unix_refuses_foreign_job_before_touching_hub_and_recovers_after_interruption() {
    let fixture = Fixture::new(false);
    let jobs = fixture.jobs();
    init_with(
        &fixture.home,
        "cue",
        &fixture.options(true, None),
        Some(&jobs),
    )
    .unwrap();
    crate::hosted_enrollment::stage_unix_fixture(
        &fixture.home,
        uuid::Uuid::new_v4(),
        DEFAULT_PORT,
        &["/cue/mcp"],
    );
    let cloudflared = fixture.home.join("synthetic-cloudflared");
    let foreign = fixture.home.join("unrelated.plist");
    fixture
        .fake
        .loaded
        .borrow_mut()
        .insert(HOSTED_LABEL.into(), foreign.clone());
    let baseline = fixture.fake.actions.borrow().len();
    assert!(schedule_hosted_with(&jobs, &fixture.home, &cloudflared).is_err());
    assert_eq!(fixture.fake.actions.borrow().len(), baseline);
    assert!(read_hosted_owner(&fixture.home).unwrap().is_none());
    assert_eq!(
        fixture.fake.loaded.borrow().get(HOSTED_LABEL),
        Some(&foreign)
    );

    fixture.fake.loaded.borrow_mut().remove(HOSTED_LABEL);
    fixture.fake.fail_next_readiness.set(true);
    assert!(schedule_hosted_with(&jobs, &fixture.home, &cloudflared).is_err());
    assert!(read_hosted_owner(&fixture.home).unwrap().is_some());
    assert!(!fixture.fake.loaded.borrow().contains_key(HOSTED_LABEL));
    assert!(schedule_hosted_with(&jobs, &fixture.home, &cloudflared).unwrap());
    assert!(fixture.fake.loaded.borrow().contains_key(HOSTED_LABEL));
}
