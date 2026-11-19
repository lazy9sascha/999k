//! Real-binary comparison of built-in defaults and explicit synthetic route/policy mounts.
use pigeonmq::{Queue, mailbox_catalog as catalog};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::LazyLock,
    time::Duration,
};

const BIN: &str = env!("CARGO_BIN_EXE_pigeonmq");
const DB: &str = "pigeonmq-cloud-queue.sqlite3";
const REG: &str = "pigeonmq-cloud-registration.json";
static ROLES: LazyLock<[(&catalog::HttpProfile, &str, &str); 5]> = LazyLock::new(|| {
    [
        (
            catalog::by_adapter(catalog::Adapter::Muse),
            "/fixture-muse/mcp",
            "test-muse",
        ),
        (
            catalog::by_adapter(catalog::Adapter::Dot),
            "/fixture-dot/mcp",
            "test-dot",
        ),
        (
            catalog::by_adapter(catalog::Adapter::Grok),
            "/fixture-grok-extraordinary-long-but-valid-lowercase-slug/mcp",
            "test-grok",
        ),
        (
            catalog::codex_http(),
            "/fixture-codex/mcp",
            "test-codex-cloud",
        ),
        (
            catalog::by_adapter(catalog::Adapter::Receiver),
            "/receiver",
            "test-codex-local",
        ),
    ]
});

fn private(path: &Path, payload: &Value) {
    fs::write(path, payload.to_string()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn role_file(role: &catalog::HttpProfile) -> String {
    if role == catalog::by_adapter(catalog::Adapter::Grok) {
        format!("fixture-{}-{}.json", role.id, "section".repeat(15))
    } else {
        format!("fixture-{}.json", role.id)
    }
}

struct Fixture {
    home: tempfile::TempDir,
    db: PathBuf,
    manifest: PathBuf,
    _dot_home: tempfile::TempDir,
    dot_policy: PathBuf,
    muse_policy: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let dot_home = tempfile::tempdir().unwrap();
        fs::set_permissions(dot_home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let muse_home = home.path().join("muse-private");
        fs::create_dir(&muse_home).unwrap();
        fs::set_permissions(&muse_home, fs::Permissions::from_mode(0o700)).unwrap();
        let dot_policy = dot_home
            .path()
            .join(role_file(catalog::by_adapter(catalog::Adapter::Dot)));
        let muse_policy = muse_home.join(role_file(catalog::by_adapter(catalog::Adapter::Muse)));
        let db = home.path().join(DB);
        let registry = home.path().join(REG);
        let manifest = home.path().join("profiles.json");
        drop(Queue::open(&db).unwrap());
        fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        let registrations: serde_json::Map<String, Value> = ROLES
            .iter()
            .map(|(role, _, _)| (role.id.into(), json!({"mailbox":role.id,"revoked":false})))
            .collect();
        private(&registry, &Value::Object(registrations));
        let mut entries = Vec::new();
        for (role, route, token) in &*ROLES {
            let hash: String = Sha256::digest(token)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let payload = match role.adapter {
                catalog::Adapter::Receiver | catalog::Adapter::SharedMailbox => {
                    json!({"principals":[{"id":role.id,"token_sha256":hash,
                        "operations":if role.adapter == catalog::Adapter::Receiver {
                            vec!["receive", "reply"]
                        } else { vec!["read", "wait", "reply", "send"] },
                        "send_to":[],"mailboxes":[role.id],"revoked":false}]})
                }
                catalog::Adapter::Dot => json!({"principal":"pigeonmq-cloud-dot",
                    "mailbox":"dot","token_sha256":hash,"enabled":true,"registration":registry}),
                _ => json!({"principal":role.id,"mailbox":role.id,"token_sha256":hash,
                    "operations":["read","wait","reply","send"],"enabled":true,
                    "expires_at_ms":u64::MAX,"registration":registry}),
            };
            let selected = match role.adapter {
                catalog::Adapter::Dot => dot_policy.clone(),
                catalog::Adapter::Muse => muse_policy.clone(),
                _ => PathBuf::from(role_file(role)),
            };
            private(&home.path().join(&selected), &payload);
            private(&home.path().join(role.policy_file), &payload);
            entries.push(json!({"adapter":role.id,"route":route,"policy":selected}));
        }
        private(&manifest, &json!({"profiles":entries}));
        pigeonmq::muse_mailbox::prepare(&db).unwrap();
        pigeonmq::grok_mailbox::prepare(&db).unwrap();
        pigeonmq::dot_profile::check(&db, &dot_policy, true).unwrap();
        Self {
            home,
            db,
            manifest,
            _dot_home: dot_home,
            dot_policy,
            muse_policy,
        }
    }
    fn command(&self, port: u16) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .args(["serve-hub", "--queue"])
            .arg(&self.db)
            .args(["--port", &port.to_string(), "--seconds", "20"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn start(&self, explicit: bool) -> Hub {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = self.command(port);
        if explicit {
            command.arg("--profiles-file").arg(&self.manifest);
        } else {
            command.arg("--bundled-profiles");
        }
        let mut hub = Hub {
            child: command.spawn().unwrap(),
            port,
        };
        let mut line = String::new();
        BufReader::new(hub.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        if line.is_empty() {
            let mut diagnostic = String::new();
            hub.child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut diagnostic)
                .unwrap();
            panic!("synthetic hub failed: {diagnostic}");
        }
        let ready: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ready["profiles_fingerprint"].is_string(), explicit);
        assert_eq!(ready["generic_enabled"], false);
        for (role, _, _) in &*ROLES {
            assert_eq!(ready[role.enabled_key], true);
        }
        hub
    }
}

struct Hub {
    child: Child,
    port: u16,
}
impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Hub {
    fn request(&self, route: &str, token: &str, body: &Value) -> (u16, Value) {
        let body = body.to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        write!(stream, "POST {route} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", self.port, body.len()).unwrap();
        let mut bytes = String::new();
        stream.read_to_string(&mut bytes).unwrap();
        let (hdr, body) = bytes.split_once("\r\n\r\n").unwrap();
        (
            hdr.split_whitespace().nth(1).unwrap().parse().unwrap(),
            serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.into())),
        )
    }
}

#[test]
fn renamed_policy_and_route_preserve_each_real_adapters_authority_and_tools() {
    let fixture = Fixture::new();
    OpenOptions::new()
        .append(true)
        .open(&fixture.manifest)
        .unwrap()
        .write_all(&[b' '; 8192])
        .unwrap();
    fs::set_permissions(&fixture.manifest, fs::Permissions::from_mode(0o400)).unwrap();
    let stock = fixture.start(false);
    let remapped = fixture.start(true);
    let tools = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let receiver = json!({"recipient":"codex-local","session_id":"fixture","profile_id":"fixture",
        "action":{"operation":"authorize","message_id":null}});
    for (role, route, token) in &*ROLES {
        let body = if role.adapter == catalog::Adapter::Receiver {
            &receiver
        } else {
            &tools
        };
        let old = stock.request(role.route, token, body);
        let new = remapped.request(route, token, body);
        assert_eq!(old.0, 200, "{}: {:?}", role.id, old);
        assert_eq!(old, new, "{}", role.id);
        if role.adapter != catalog::Adapter::Receiver {
            assert!(!old.1["result"]["tools"].as_array().unwrap().is_empty());
            assert_eq!(remapped.request(role.route, token, body).0, 404);
        }
        let old_denial = stock.request(role.route, "test-foreign", body);
        let new_denial = remapped.request(route, "test-foreign", body);
        assert!(old_denial.0 >= 400 || old_denial.1.get("error").is_some());
        assert_eq!(old_denial, new_denial, "{}", role.id);
    }
    assert_eq!(remapped.request("/mcp", "test-muse", &tools).0, 404);
    // The mounted external policy is still reopened and checked on each request.
    fs::set_permissions(&fixture.dot_policy, fs::Permissions::from_mode(0o644)).unwrap();
    let blocked = remapped.request("/fixture-dot/mcp", "test-dot", &tools);
    assert_eq!(
        blocked,
        (503, json!({"error":{"code":"policy_unavailable"}}))
    );
    assert_eq!(
        stock
            .request(
                catalog::by_adapter(catalog::Adapter::Dot).route,
                "test-dot",
                &tools
            )
            .0,
        200
    );
    let help = Command::new(BIN).args(["run", "--help"]).output().unwrap();
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("[default: 54843]")
    );
}

#[test]
fn explicit_manifest_rejects_collisions_paths_and_pinned_content_drift() {
    let fixture = Fixture::new();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let linked_policy = fixture.home.path().join("linked-policy.json");
    symlink(&fixture.muse_policy, &linked_policy).unwrap();
    let linked_parent = fixture.home.path().join("linked-parent");
    symlink(fixture.muse_policy.parent().unwrap(), &linked_parent).unwrap();
    let filename = fixture.muse_policy.file_name().unwrap();
    let bare = role_file(catalog::by_adapter(catalog::Adapter::Grok));
    for entries in [
        json!([{"adapter":"vooy","route":"/vooy/mcp","policy":fixture.muse_policy}]),
        json!([{"adapter":"muse","route":"/dot/mcp","policy":fixture.muse_policy}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":"../fixture-muse.json"}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":"muse-private/fixture-muse.json"}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":fixture.home.path().join("muse-private/../muse-private").join(filename)}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":fixture.home.path().join("muse-private/.").join(filename)}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":linked_policy}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":linked_parent.join(filename)}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":"missing.json"}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":fixture.muse_policy},
               {"adapter":"dot","route":"/fixture/mcp","policy":fixture.dot_policy}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":fixture.muse_policy},
               {"adapter":"muse","route":"/other/mcp","policy":fixture.dot_policy}]),
        json!([{"adapter":"muse","route":"/fixture/mcp","policy":bare},
               {"adapter":"dot","route":"/other/mcp","policy":fixture.home.path().join(&bare)}]),
        json!([{"adapter":"codex-local","route":"/fixture-receiver/mcp","policy":"fixture-codex-local.json"}]),
    ] {
        private(&fixture.manifest, &json!({"profiles":entries}));
        let mut command = fixture.command(port);
        let result = command
            .arg("--profiles-file")
            .arg(&fixture.manifest)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
    private(
        &fixture.manifest,
        &json!({"profiles":[{"adapter":"muse","route":"/fixture/mcp",
        "policy":fixture.muse_policy}]}),
    );
    let original = fs::read(&fixture.manifest).unwrap();
    for (contents, mode) in [
        (original.clone(), 0o644),
        (
            [
                original.clone(),
                vec![b' '; pigeonmq::private_file::MAX_PRIVATE_CONFIG_BYTES as usize],
            ]
            .concat(),
            0o600,
        ),
    ] {
        fs::write(&fixture.manifest, contents).unwrap();
        fs::set_permissions(&fixture.manifest, fs::Permissions::from_mode(mode)).unwrap();
        let result = fixture
            .command(port)
            .arg("--profiles-file")
            .arg(&fixture.manifest)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
    }
    fs::write(&fixture.manifest, original).unwrap();
    let result = fixture
        .command(port)
        .arg("--profiles-file")
        .arg(&fixture.manifest)
        .args(["--profiles-fingerprint", &"0".repeat(64)])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("manifest changed before hub startup")
    );
}

#[test]
fn registered_send_admits_only_readable_idempotent_ordinary_requests() {
    let fixture = Fixture::new();
    let hub = fixture.start(true);
    let (muse, dot, grok, codex) = (ROLES[0], ROLES[1], ROLES[2], ROLES[3]);
    let request = |role: (&catalog::HttpProfile, &str, &str), method: &str, params: Value| {
        let (status, value) = hub.request(
            role.1,
            role.2,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        );
        assert_eq!(status, 200);
        value
    };
    let tool = |role, name, arguments| {
        request(
            role,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
    };
    let content = |response: &Value| -> Value {
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let accepted = |response: Value| {
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        content(&response)
    };
    let rejected = |response: Value, reason| {
        if reason == "invalid_fields" {
            assert_eq!(response["error"]["code"], -32602);
            assert_eq!(response["error"]["message"], reason);
        } else {
            assert_eq!(response["result"]["isError"], true);
            assert_eq!(content(&response), json!({"error":reason}));
        }
    };
    let count = || {
        rusqlite::Connection::open(&fixture.db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let common = pigeonmq::MAX_ORDINARY_REQUEST_BYTES;
    let deferred = pigeonmq::muse_mailbox::MAX_REQUEST_BYTES;
    for (sender, deferred_recipient) in [(muse, grok), (grok, muse)] {
        let tools = request(sender, "tools/list", json!({}));
        let send = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "pigeonmq_send")
            .unwrap();
        let schema = &send["inputSchema"];
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("idempotency_key"))
        );
        assert!(schema["properties"].get("in_reply_to").is_none());

        let recipient = deferred_recipient.0.id;
        let valid = json!({"recipient":recipient,"body":"test","idempotency_key":"test"});
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("idempotency_key");
        rejected(tool(sender, "pigeonmq_send", missing), "invalid_fields");
        for (field, value, reason) in [
            ("idempotency_key", Value::Null, "invalid_fields"),
            (
                "idempotency_key",
                json!(" \n\u{1c}"),
                "invalid_idempotency_key",
            ),
            (
                "idempotency_key",
                json!("é".repeat(65)),
                "invalid_idempotency_key",
            ),
            ("in_reply_to", json!("unbound"), "invalid_fields"),
            ("body", json!("\t \u{1c}"), "invalid_body"),
            (
                "body",
                json!("é".repeat(deferred / 2) + "x"),
                "invalid_body",
            ),
            ("correlation_id", json!("café"), "invalid_identifier"),
            (
                "correlation_id",
                json!("x".repeat(129)),
                "invalid_identifier",
            ),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            rejected(tool(sender, "pigeonmq_send", invalid), reason);
        }
        rejected(
            tool(
                sender,
                "pigeonmq_send",
                json!({"recipient":dot.0.id,"body":"é".repeat(common / 2) + "x","idempotency_key":"test"}),
            ),
            "invalid_body",
        );
    }
    rejected(
        tool(
            codex,
            "pigeonmq_send",
            json!({
                "recipient":muse.0.id,"body":"test","idempotency_key":"test","correlation_id":"café"
            }),
        ),
        "invalid_fields",
    );
    rejected(
        tool(
            codex,
            "pigeonmq_send",
            json!({
                "recipient":dot.0.id,"body":" \u{1c}","idempotency_key":"test"
            }),
        ),
        "invalid_fields",
    );
    let denied = tool(
        dot,
        "pigeonmq_send",
        json!({
            "recipient":grok.0.id,"body":"test","idempotency_key":"test",
            "conversation":{"conversation_id":"café","participants":[{"mailbox":grok.0.id,"role":"to"}]}
        }),
    );
    assert_eq!(
        denied["error"]["message"],
        "invalid_conversation_identifier"
    );
    assert_eq!(count(), 0);

    for (sender, recipient, limit) in [
        (muse, grok, deferred),
        (grok, muse, deferred),
        (muse, dot, common),
        (grok, codex, common),
    ] {
        let body = "é".repeat(limit / 2);
        let key = "é".repeat(64);
        let correlation = "c".repeat(128);
        let send = json!({"recipient":recipient.0.id,"body":body,"idempotency_key":key,"correlation_id":correlation});
        let first = accepted(tool(sender, "pigeonmq_send", send.clone()));
        assert_eq!(first["duplicate"], false);
        let id = &first["message"]["id"];
        let replay = accepted(tool(sender, "pigeonmq_send", send.clone()));
        assert_eq!(replay["duplicate"], true);
        assert_eq!(&replay["message"]["id"], id);
        let mut changed = send;
        changed["body"] = json!("different request");
        rejected(tool(sender, "pigeonmq_send", changed), "conflict");

        let read = accepted(tool(recipient, "pigeonmq_read", json!({"message_id":id})));
        let observed = read.get("envelope").unwrap_or(&read);
        assert_eq!(observed["body"], body);
        assert_eq!(observed["correlation_id"], correlation);

        let answer = if recipient.0.adapter == catalog::Adapter::Dot {
            "answer".to_owned()
        } else {
            "é".repeat(pigeonmq::muse_mailbox::MAX_REPLY_BYTES / 2)
        };
        let complete = accepted(tool(
            recipient,
            "pigeonmq_reply",
            json!({"message_id":id,"body":answer}),
        ));
        let reply = accepted(tool(
            sender,
            "pigeonmq_read",
            json!({"message_id":complete["reply_id"]}),
        ));
        assert_eq!(reply["body"], answer);
        assert_eq!(reply["correlation_id"], correlation);
    }
    assert_eq!(count(), 8);

    // Mixed recipients keep existing per-copy results. Rejected means no row was inserted.
    let subset = accepted(tool(
        muse,
        "pigeonmq_send",
        json!({
            "recipient":format!("{},{}", grok.0.id, dot.0.id),
            "body":"x".repeat(common + 1),
            "idempotency_key":"subset-test"
        }),
    ));
    assert_eq!(subset["accepted"], 1);
    assert_eq!(subset["recipients"][0]["state"], "accepted");
    assert_eq!(subset["recipients"][1]["state"], "rejected");
    assert_eq!(subset["recipients"][1]["error"], "invalid_body");
    assert_eq!(count(), 9);
}

#[test]
fn muse_and_grok_continuations_require_both_read_and_send() {
    let fixture = Fixture::new();
    let hub = fixture.start(true);
    let mut queue = Queue::open_existing(&fixture.db).unwrap();
    for index in [0, 2] {
        let (role, route, token) = ROLES[index];
        let policy_path = if index == 0 {
            fixture.muse_policy.clone()
        } else {
            fixture.home.path().join(role_file(role))
        };
        let mut policy: Value = serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        let parent = queue
            .send(pigeonmq::Envelope {
                sender: "dot".into(),
                recipient: role.id.into(),
                body: "fixture parent".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message;
        let args = json!({
            "recipient":"dot", "body":"fixture continuation", "idempotency_key":"continued",
            "continue_from_message_id":parent.id
        });
        let call = |arguments| {
            let (status, response) = hub.request(
                route,
                token,
                &json!({
                    "jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"pigeonmq_send","arguments":arguments}
                }),
            );
            assert_eq!(status, 200, "{response}");
            let result = &response["result"];
            let detail: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            (result["isError"].clone(), detail)
        };
        for grant in ["send", "read"] {
            policy["operations"] = json!([grant]);
            private(&policy_path, &policy);
            assert_eq!(
                call(args.clone()),
                (json!(true), json!({"error":"forbidden"}))
            );
        }
        policy["operations"] = json!(["send", "read"]);
        private(&policy_path, &policy);
        let (error, receipt) = call(args.clone());
        assert_eq!(error, false);
        assert_eq!(
            receipt["message"]["conversation_id"],
            parent.conversation_id
        );
        assert_eq!(receipt["duplicate"], false);
        assert_eq!(call(args).1["duplicate"], true);
    }
}

#[test]
fn thread_reads_are_per_mailbox_even_after_an_authorized_continuation() {
    let fixture = Fixture::new();
    let hub = fixture.start(true);
    let tool = |role: usize, name: &str, args: Value| {
        let (_, route, token) = ROLES[role];
        let (status, value) = hub.request(
            route,
            token,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
        );
        (status, value)
    };
    let call = |role, name, args| {
        let (status, value) = tool(role, name, args);
        assert_eq!(status, 200, "{value}");
        let result = &value["result"];
        assert_ne!(result["isError"], true, "role {role}, {name}: {value}");
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let thread = |role, conversation: &str, cursor: Option<&str>| {
        let mut args = json!({"conversation_id":conversation});
        if let Some(cursor) = cursor {
            args["cursor"] = json!(cursor);
        }
        call(role, "pigeonmq_read_thread", args)
    };
    let (a, b, c, outsider) = (1, 0, 2, 3); // Dot, Muse, Grok and Codex fixtures.
    let root = call(
        a,
        "pigeonmq_send",
        json!({"recipient":"muse","body":"A to B","idempotency_key":"root"}),
    );
    let root_id = root["message_id"].as_str().unwrap();
    assert_eq!(root["conversation_id"], root_id);
    assert_eq!(thread(c, root_id, None), thread(c, "unknown-fixture", None));
    assert_eq!(
        thread(outsider, root_id, None),
        thread(c, "unknown-fixture", None)
    );
    let observed = call(b, "pigeonmq_read", json!({"message_id":root_id}));
    assert_eq!(observed["message_id"], root_id);
    assert_eq!(observed["conversation_id"], root_id);
    let reply = call(
        b,
        "pigeonmq_reply",
        json!({"message_id":root_id,"body":"B to A terminal"}),
    );
    let reply_id = reply["reply_id"].as_str().unwrap();
    let continued = call(
        a,
        "pigeonmq_send",
        json!({"recipient":"grok-bot","body":"A to C, no old bodies","idempotency_key":"new","continue_from_message_id":reply_id}),
    );
    let continued_id = continued["message_id"].as_str().unwrap();
    assert_eq!(continued["conversation_id"], root_id);
    let visible = thread(c, root_id, None);
    assert_eq!(visible["messages"].as_array().unwrap().len(), 1);
    assert_eq!(visible["messages"][0]["message_id"], continued_id);
    assert_eq!(
        thread(b, root_id, None)["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let denied = |parent| {
        tool(
            b,
            "pigeonmq_send",
            json!({
                "recipient":"dot","body":"denied","idempotency_key":"deny","continue_from_message_id":parent
            }),
        )
    };
    assert_eq!(denied(continued_id), denied("missing-fixture"));
    let new_root = call(
        b,
        "pigeonmq_send",
        json!({"recipient":"dot","body":"raw collision",
        "idempotency_key":"collision","correlation_id":root_id}),
    );
    assert_eq!(
        new_root["message"]["conversation_id"],
        new_root["message"]["id"]
    );
    assert_ne!(new_root["message"]["conversation_id"], root_id);

    let mut expected = vec![
        root_id.to_owned(),
        reply_id.to_owned(),
        continued_id.to_owned(),
    ];
    for i in 0..20 {
        let result = call(
            a,
            "pigeonmq_send",
            json!({"recipient":"muse","body":"later A to B",
            "idempotency_key":format!("later-{i}"),"continue_from_message_id":root_id}),
        );
        expected.push(result["message_id"].as_str().unwrap().into());
        assert_eq!(result["conversation_id"], root_id);
    }
    let before = fs::read(&fixture.db).unwrap();
    let ids = |page: &Value| {
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["message_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let latest = thread(a, root_id, None);
    assert_eq!(ids(&latest), expected[3..]);
    let older = thread(a, root_id, latest["next_cursor"].as_str());
    assert_eq!(ids(&older), expected[..3]);
    assert!(older["next_cursor"].is_null());
    assert_eq!(older["messages"][1]["in_reply_to"], root_id);
    assert!(older["messages"][2]["in_reply_to"].is_null());
    let cursor_error = |cursor| {
        tool(
            c,
            "pigeonmq_read_thread",
            json!({
                "conversation_id":root_id,"cursor":cursor
            }),
        )
    };
    assert_eq!(cursor_error(root_id), cursor_error("missing-fixture"));
    assert_eq!(fs::read(&fixture.db).unwrap(), before);
}

#[test]
fn standalone_has_one_profile_manifest_option_and_no_per_app_flags() {
    let help = Command::new(BIN)
        .args(["serve-hub", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--profiles-file"));
    assert!(!help.contains("--bundled-profiles"));
    for removed in [
        "--muse-auth-policy",
        "--dot-auth-policy",
        "--grok-auth-policy",
        "--codex-auth-policy",
        "--codex-registration",
        "--receiver-auth-policy",
        "--receiver-registration",
    ] {
        assert!(!help.contains(removed), "{removed}");
        let result = Command::new(BIN)
            .arg("serve-hub")
            .arg(removed)
            .arg("policy.json")
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(
            String::from_utf8(result.stderr)
                .unwrap()
                .contains("unexpected argument")
        );
    }
}

#[test]
fn terminal_confirmation_reports_only_in_scope_domain_errors_over_rmcp() {
    use pigeonmq::Envelope;
    let fixture = Fixture::new();
    let hub = fixture.start(true);
    let mut queue = Queue::open_existing(&fixture.db).unwrap();
    let conn = rusqlite::Connection::open(&fixture.db).unwrap();
    let ordinary = |sender: &str, recipient: &str| Envelope {
        sender: sender.into(),
        recipient: recipient.into(),
        body: "synthetic message".into(),
        correlation_id: None,
        in_reply_to: None,
        idempotency_key: None,
    };

    for role in [0, 2, 3] {
        let (profile, route, token) = ROLES[role];
        let hidden = if profile.adapter == catalog::Adapter::SharedMailbox {
            "message_not_available"
        } else {
            "not_found"
        };
        let call = |id: &str| {
            let (status, response) = hub.request(
                route,
                token,
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name":"pigeonmq_confirm_terminal","arguments":{"message_id":id}}}),
            );
            assert_eq!(status, 200, "{response}");
            let result = &response["result"];
            let text = result["content"][0]["text"].as_str().unwrap();
            (
                result["isError"].as_bool().unwrap(),
                serde_json::from_str::<Value>(text).unwrap(),
            )
        };
        let state = |id: &str| -> (i64, Option<i64>) {
            conn.query_row(
                "SELECT attempts,acknowledged_at_ms FROM messages WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let incoming = queue.send(ordinary("dot", profile.id)).unwrap().message.id;
        let outgoing = queue.send(ordinary(profile.id, "dot")).unwrap().message.id;
        let foreign = queue
            .send(ordinary("dot", "codex-local"))
            .unwrap()
            .message
            .id;
        let unregistered = queue
            .send(ordinary("not-registered", profile.id))
            .unwrap()
            .message
            .id;
        let rpc = queue
            .request(ordinary("dot", profile.id))
            .unwrap()
            .message
            .id;
        let terminal = |parent: &str, correlation: &str| Envelope {
            correlation_id: Some(correlation.into()),
            in_reply_to: Some(parent.into()),
            ..ordinary("dot", profile.id)
        };
        let invalid = queue
            .send(terminal(&outgoing, "wrong-link"))
            .unwrap()
            .message
            .id;
        let answer = queue
            .send(terminal(&outgoing, &outgoing))
            .unwrap()
            .message
            .id;
        let claimed = queue
            .send(terminal(&outgoing, &outgoing))
            .unwrap()
            .message
            .id;
        let previous = queue
            .send(terminal(&outgoing, &outgoing))
            .unwrap()
            .message
            .id;
        conn.execute(
            "UPDATE messages SET attempts=1,lease_until_ms=?1,receipt='synthetic' WHERE id=?2",
            rusqlite::params![i64::MAX, claimed],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET attempts=1,lease_until_ms=0 WHERE id=?1",
            [&previous],
        )
        .unwrap();

        assert_eq!(
            call(&incoming),
            (true, json!({"error":"terminal_answer_required"})),
            "{}",
            profile.id
        );
        if profile.adapter != catalog::Adapter::SharedMailbox {
            let wide = queue
                .send(Envelope {
                    body: "x".repeat(9000),
                    ..ordinary("dot", profile.id)
                })
                .unwrap()
                .message
                .id;
            assert_eq!(
                call(&wide),
                (true, json!({"error":"terminal_answer_required"})),
                "{}",
                profile.id
            );
            assert_eq!(state(&wide), (0, None));
        }
        for id in [
            &outgoing,
            &foreign,
            &unregistered,
            &rpc,
            &invalid,
            "nonexistent",
        ] {
            assert_eq!(
                call(id),
                (true, json!({"error":hidden})),
                "{}: {id}",
                profile.id
            );
        }
        for id in [&claimed, &previous] {
            assert_eq!(
                call(id),
                (
                    true,
                    json!({"error":"existing_claim_requires_reconciliation"})
                ),
                "{}",
                profile.id
            );
            assert_eq!(state(id), (1, None));
        }
        assert_eq!(state(&incoming), (0, None));
        assert_eq!(state(&answer), (0, None));
        assert_eq!(
            call(&answer),
            (
                false,
                json!({"duplicate":false,"message_id":answer,"state":"acknowledged"})
            )
        );
        let acknowledged = state(&answer);
        assert!(acknowledged.1.is_some());
        assert_eq!(
            call(&answer),
            (
                false,
                json!({"duplicate":true,"message_id":answer,"state":"acknowledged"})
            )
        );
        assert_eq!(state(&answer), acknowledged);
        assert_eq!(state(&outgoing), (0, None));

        let (status, rejected) = hub.request(
            route,
            "wrong-test-token",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"pigeonmq_confirm_terminal","arguments":{"message_id":answer}}}),
        );
        assert_eq!(status, 401, "{rejected}");

        // Rotate the same registry while the hub is running. The next
        // confirmation must not reuse the earlier authorized snapshot.
        let registry = fixture.home.path().join(REG);
        let mut entries: Value = serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        entries["dot"]["revoked"] = json!(true);
        private(&registry, &entries);
        let unread = queue
            .send(terminal(&outgoing, &outgoing))
            .unwrap()
            .message
            .id;
        assert_eq!(call(&unread), (true, json!({"error":hidden})));
        assert_eq!(state(&unread), (0, None));
        entries["dot"]["revoked"] = json!(false);
        private(&registry, &entries);
        assert!(!call(&unread).0);

        // A reply-only credential cannot probe for any inbox IDs.
        let policy_path = if profile.adapter == catalog::Adapter::Muse {
            fixture.muse_policy.clone()
        } else {
            fixture.home.path().join(role_file(profile))
        };
        let mut policy: Value = serde_json::from_slice(&fs::read(&policy_path).unwrap()).unwrap();
        if profile.adapter == catalog::Adapter::SharedMailbox {
            policy["principals"][0]["operations"] = json!(["reply"]);
        } else {
            policy["operations"] = json!(["reply"]);
        }
        private(&policy_path, &policy);
        for id in [&incoming, &answer, "nonexistent"] {
            assert_eq!(
                call(id),
                (true, json!({"error":"forbidden"})),
                "{}",
                profile.id
            );
        }
    }
}
