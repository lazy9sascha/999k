use super::*;
use std::{cell::RefCell, collections::BTreeSet, os::unix::fs::PermissionsExt};

#[derive(Default)]
struct FakeLaunchctl {
    loaded: RefCell<BTreeSet<String>>,
    changes: RefCell<Vec<(String, bool)>>,
}

impl LaunchControl for FakeLaunchctl {
    fn loaded(&self, label: &str, _expected_plist: &Path) -> io::Result<bool> {
        Ok(self.loaded.borrow().contains(label))
    }

    fn bootstrap(&self, path: &Path) -> io::Result<()> {
        let label = path.file_stem().unwrap().to_str().unwrap();
        assert!(path.is_file());
        assert!(self.loaded.borrow_mut().insert(label.into()));
        self.changes.borrow_mut().push((label.into(), true));
        Ok(())
    }

    fn bootout(&self, label: &str) -> io::Result<()> {
        assert!(self.loaded.borrow_mut().remove(label));
        self.changes.borrow_mut().push((label.into(), false));
        Ok(())
    }
}

fn relay_changes(launch: &FakeLaunchctl) -> Vec<bool> {
    launch
        .changes
        .borrow()
        .iter()
        .filter_map(|(label, start)| (label == RELAY_LABEL).then_some(*start))
        .collect()
}

fn fresh_options<'a>(
    email_recipient: Option<&'a str>,
    relay_command: Option<&'a Path>,
) -> Options<'a> {
    Options {
        profiles_file: None,
        email_recipient,
        relay_command,
        port: None,
        apply: false,
    }
}

fn staged_token(result: &Value) -> (PathBuf, String) {
    let path = PathBuf::from(result["token_file"].as_str().unwrap());
    let token = fs::read_to_string(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(token.len(), 64);
    assert!(!result.to_string().contains(&token));
    (path, token)
}

fn state(home: &Path) -> Registry {
    read_json(&home.join(DIRECTORY).join(STATE)).unwrap()
}

fn mailbox(home: &Path, name: &str) -> crate::codex_mcp::Profile {
    let registry = state(home);
    let profile = &registry.profiles[name];
    assert_eq!(profile.phase, Phase::Active);
    let binding = profile.binding(name, &registry.registration).unwrap();
    binding.mailbox(&home.join(QUEUE)).unwrap()
}

fn tool(
    profile: &crate::codex_mcp::Profile,
    token: &str,
    name: &str,
    arguments: Value,
) -> Result<Value, u16> {
    let request = json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":"tools/call",
        "params":{"name":name,"arguments":arguments}
    });
    let response = profile
        .dispatch(
            &[format!("Bearer {token}").into_bytes()],
            &serde_json::to_vec(&request).unwrap(),
        )
        .map_err(|(status, _)| status)?
        .unwrap();
    if response["result"]["isError"] == true || response["error"].is_object() {
        return Err(200);
    }
    Ok(serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap())
}

#[test]
fn fresh_install_keeps_one_relay_and_reinit_fences_old_mail_in_real_mcp_handlers() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("home");
    mkdir_private(&home).unwrap();
    let location = root.join("LaunchAgents");
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let launch = FakeLaunchctl::default();
    let jobs = Jobs {
        service: &launch,
        directory: &location,
        executable: &executable,
        installation: None,
    };
    let command = root.join("relay-command");
    fs::write(&command, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();

    let queue = home.join(QUEUE);
    let hub_plist = location.join(format!("{HUB_LABEL}.plist"));
    let relay_plist = location.join(format!("{RELAY_LABEL}.plist"));
    let relay_config_path = home.join(DIRECTORY).join(RELAY_CONFIG);
    let cue_destination = "cue-fixture@example.invalid";
    let instinct_destination = "instinct-fixture@example.invalid";
    assert!(!queue.exists());

    let cue_options = fresh_options(Some(cue_destination), Some(&command));
    let cue = init_with(&home, "cue", &cue_options, None).unwrap();
    assert_eq!(cue["state"], "staged");
    let (cue_token_path, cue_token) = staged_token(&cue);
    assert_eq!(
        staged_token(&init_with(&home, "cue", &cue_options, None).unwrap()),
        (cue_token_path.clone(), cue_token.clone())
    );
    assert!(launch.changes.borrow().is_empty());
    assert!(!hub_plist.exists() && !relay_plist.exists() && !relay_config_path.exists());

    let mut cue_apply = cue_options;
    cue_apply.apply = true;
    assert_eq!(
        init_with(&home, "cue", &cue_apply, Some(&jobs)).unwrap()["state"],
        "active"
    );
    let saved_hub = fs::read(&hub_plist).unwrap();
    let saved_relay = fs::read(&relay_plist).unwrap();
    assert_eq!(relay_changes(&launch), vec![true]);
    let original_config: RelayConfig = read_json(&relay_config_path).unwrap();
    assert_eq!(original_config.len(), 1);
    assert_eq!(original_config["cue"].recipient, cue_destination);
    assert!(!String::from_utf8_lossy(&saved_relay).contains(cue_destination));
    assert_eq!(
        fs::metadata(&relay_config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let first_active = state(&home);
    let initial_registration = fs::read(&first_active.registration).unwrap();
    let initial_state = fs::read(home.join(DIRECTORY).join(STATE)).unwrap();
    let changes = launch.changes.borrow().len();
    assert_eq!(
        staged_token(&init_with(&home, "cue", &cue_apply, Some(&jobs)).unwrap()),
        (cue_token_path, cue_token.clone())
    );
    assert_eq!(launch.changes.borrow().len(), changes);
    assert_eq!(
        fs::read(&first_active.registration).unwrap(),
        initial_registration
    );
    assert_eq!(
        fs::read(home.join(DIRECTORY).join(STATE)).unwrap(),
        initial_state
    );
    assert!(
        init_with(
            &home,
            "cue",
            &fresh_options(Some("different@example.invalid"), None),
            None,
        )
        .is_err()
    );
    assert_eq!(fs::read(&relay_plist).unwrap(), saved_relay);

    let mut instinct_options = fresh_options(Some(instinct_destination), None);
    let instinct = init_with(&home, "instinct", &instinct_options, None).unwrap();
    assert_eq!(instinct["state"], "staged");
    assert_eq!(launch.changes.borrow().len(), changes);
    let (_, instinct_token) = staged_token(&instinct);
    instinct_options.apply = true;
    assert_eq!(
        init_with(&home, "instinct", &instinct_options, Some(&jobs)).unwrap()["state"],
        "active"
    );
    let both: RelayConfig = read_json(&relay_config_path).unwrap();
    assert_eq!(both["cue"], original_config["cue"]);
    assert_eq!(both["instinct"].recipient, instinct_destination);
    assert!(!String::from_utf8_lossy(&saved_relay).contains(instinct_destination));
    assert_eq!(fs::read(&relay_plist).unwrap(), saved_relay);
    assert_eq!(fs::read(&hub_plist).unwrap(), saved_hub);
    assert_eq!(relay_changes(&launch), vec![true]);

    let cue_profile = mailbox(&home, "cue");
    let instinct_profile = mailbox(&home, "instinct");
    let send = json!({
        "recipient":"cue",
        "body":"synthetic original",
        "idempotency_key":"same-key-across-generations"
    });
    let original = tool(
        &instinct_profile,
        &instinct_token,
        "pigeonmq_send",
        send.clone(),
    )
    .unwrap();
    let original_id = original["message"]["id"].as_str().unwrap();
    let observed = tool(
        &cue_profile,
        &cue_token,
        "pigeonmq_read",
        json!({"message_id":original_id}),
    )
    .unwrap();
    assert_eq!(observed["envelope"]["body"], "synthetic original");
    let answer = tool(
        &cue_profile,
        &cue_token,
        "pigeonmq_reply",
        json!({"message_id":original_id,"body":"synthetic terminal"}),
    )
    .unwrap();
    let answer_id = answer["reply_id"].as_str().unwrap();
    let terminal = tool(
        &instinct_profile,
        &instinct_token,
        "pigeonmq_read",
        json!({"message_id":answer_id}),
    )
    .unwrap();
    assert_eq!(terminal["envelope"]["body"], "synthetic terminal");
    assert_eq!(
        tool(
            &instinct_profile,
            &instinct_token,
            "pigeonmq_confirm_terminal",
            json!({"message_id":answer_id}),
        )
        .unwrap()["state"],
        "acknowledged"
    );

    assert_eq!(
        remove_with(&home, "cue", None).unwrap()["state"],
        "remove_pending"
    );
    assert_eq!(relay_changes(&launch), vec![true]);
    assert_eq!(
        remove_with(&home, "cue", Some(&jobs)).unwrap()["state"],
        "retired"
    );
    let one: RelayConfig = read_json(&relay_config_path).unwrap();
    assert_eq!(
        one.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["instinct"]
    );
    assert_eq!(one["instinct"], both["instinct"]);
    assert_eq!(fs::read(&relay_plist).unwrap(), saved_relay);
    assert_eq!(relay_changes(&launch), vec![true]);
    assert!(
        tool(
            &cue_profile,
            &cue_token,
            "pigeonmq_read",
            json!({"message_id":original_id}),
        )
        .is_err()
    );

    let replacement = init_with(&home, "cue", &fresh_options(None, None), None).unwrap();
    let (_, new_cue_token) = staged_token(&replacement);
    assert_ne!(cue_token, new_cue_token);
    let mut new_apply = fresh_options(None, None);
    new_apply.apply = true;
    assert_eq!(
        init_with(&home, "cue", &new_apply, Some(&jobs)).unwrap()["state"],
        "active"
    );
    let current_cue = mailbox(&home, "cue");
    assert_eq!(
        tool(
            &current_cue,
            &cue_token,
            "pigeonmq_read",
            json!({"message_id":original_id}),
        ),
        Err(401)
    );
    for name in [
        "pigeonmq_read",
        "pigeonmq_reply",
        "pigeonmq_confirm_terminal",
    ] {
        let args = if name == "pigeonmq_reply" {
            json!({"message_id":original_id,"body":"must be rejected"})
        } else {
            json!({"message_id":original_id})
        };
        assert!(tool(&current_cue, &new_cue_token, name, args).is_err());
    }
    assert_eq!(
        tool(
            &current_cue,
            &new_cue_token,
            "pigeonmq_inbox",
            json!({"scope":"all","query":"synthetic"}),
        )
        .unwrap()["messages"],
        json!([])
    );
    assert_eq!(
        tool(
            &current_cue,
            &new_cue_token,
            "pigeonmq_read_thread",
            json!({"conversation_id":original_id}),
        )
        .unwrap()["messages"],
        json!([])
    );
    assert_eq!(
        tool(
            &current_cue,
            &new_cue_token,
            "pigeonmq_wait",
            json!({"timeout_ms":0})
        )
        .unwrap()["message"],
        Value::Null
    );
    let next = tool(&instinct_profile, &instinct_token, "pigeonmq_send", send).unwrap();
    let next_id = next["message"]["id"].as_str().unwrap();
    assert_ne!(next_id, original_id);
    assert_eq!(
        tool(
            &current_cue,
            &new_cue_token,
            "pigeonmq_read",
            json!({"message_id":next_id})
        )
        .unwrap()["id"],
        next_id
    );
    let retained: i64 = Connection::open(&queue)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM messages WHERE id IN (?1,?2) AND acknowledged_at_ms IS NOT NULL",
            [original_id, answer_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 2);

    assert_eq!(
        remove_with(&home, "instinct", Some(&jobs)).unwrap()["state"],
        "retired"
    );
    assert_eq!(relay_changes(&launch), vec![true, false]);
    assert_eq!(fs::read(&hub_plist).unwrap(), saved_hub);
    assert!(launch.loaded.borrow().contains(HUB_LABEL));
    assert!(!relay_plist.exists());
    assert!(
        read_json::<RelayConfig>(&relay_config_path)
            .unwrap()
            .is_empty()
    );
}
