//! Owner-private queue fixtures and fake launchctl only.
use super::*;
use pigeonmq::{Envelope, Queue, dot_profile, grok_mailbox, muse_mailbox};
use std::{cell::RefCell, os::unix::fs::PermissionsExt};

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
}

impl LaunchControl for FakeLaunchctl {
    fn loaded(&self, label: &str, expected_plist: &Path) -> io::Result<bool> {
        match self.loaded.borrow().get(label) {
            Some(path) if path == expected_plist => Ok(true),
            Some(_) => Err(error("different fake launchd plist")),
            None => Ok(false),
        }
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
