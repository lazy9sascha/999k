//! Exercise the actual runner with local stand-ins, never launchd or a remote service.
#![cfg(target_os = "macos")]
use radio999k::{Envelope, Queue, hub_http, mailbox_catalog as catalog};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_999k");
const DOT: &str = "dot_tunnel";
const CODEX: &str = "codex_tunnel";
const EMAIL: &str = "email_relay";
const BODY: &str = "fixture-message-private-marker";
const TOKEN: &str = "fixture-bearer-private-marker";
const CHILD_TEXT: &str = "fixture-companion-private-output";

fn write(path: &Path, bytes: impl AsRef<[u8]>, mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn wait<T>(label: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < until, "{label} timed out");
        thread::sleep(Duration::from_millis(30));
    }
}

fn pid(value: &Value) -> u32 {
    u32::try_from(value.as_u64().expect("process PID")).unwrap()
}

fn signal(pid: u32, signal: i32) -> bool {
    // SAFETY: All process IDs in this test come from the isolated runner.
    unsafe { libc::kill(pid as i32, signal) == 0 }
}

fn gone(pid: u32) {
    wait("isolated process cleanup", || {
        (!signal(pid, 0)).then_some(())
    });
}

struct Runner(Child);
impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Runner {
    fn stop(&mut self, kill: bool) {
        assert!(signal(
            self.0.id(),
            if kill { libc::SIGKILL } else { libc::SIGTERM }
        ));
        let status = wait("runner stop", || self.0.try_wait().unwrap());
        assert_eq!(status.success(), !kill);
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    queue: PathBuf,
    port: u16,
    id: String,
}
impl Fixture {
    fn new(port: u16) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let queue_path = home.join("pigeonmq-cloud-queue.sqlite3");
        let registration = home.join("pigeonmq-cloud-registration.json");
        let dot_policy = home.join(catalog::by_adapter(catalog::Adapter::Dot).policy_file);
        let codex_policy = home.join(catalog::codex_http().policy_file);
        let mut queue = Queue::open(&queue_path).unwrap();
        fs::set_permissions(&queue_path, fs::Permissions::from_mode(0o600)).unwrap();
        let id = queue
            .send(Envelope {
                sender: "codex-cloud".into(),
                recipient: "dot".into(),
                body: BODY.into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        drop(queue);
        let digest = Sha256::digest(TOKEN.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for (path, value) in [
            (
                &registration,
                json!({
                    "codex-cloud":{"mailbox":"codex-cloud","revoked":false},
                    "dot":{"mailbox":"dot","revoked":false},
                    "cue":{"mailbox":"cue","revoked":false},
                    "instinct":{"mailbox":"instinct","revoked":false}
                }),
            ),
            (
                &dot_policy,
                json!({"principal":"pigeonmq-cloud-dot","mailbox":"dot","enabled":true,
                "token_sha256":digest,"registration":registration}),
            ),
            (
                &codex_policy,
                json!({"principals":[{"id":"codex-cloud","token_sha256":digest,
                "operations":["read","wait","reply","send"],"mailboxes":["codex-cloud"],"send_to":[],"revoked":false}]}),
            ),
        ] {
            write(path, value.to_string(), 0o600);
        }
        let prep = Command::new(BIN)
            .args(["dot-prepare", "--queue"])
            .arg(&queue_path)
            .arg("--auth-policy")
            .arg(&dot_policy)
            .output()
            .unwrap();
        assert!(
            prep.status.success(),
            "isolated Dot storage did not prepare"
        );
        for role in [DOT, CODEX] {
            for suffix in [".cwd", ".home"] {
                let dir = home.join(format!("{role}{suffix}"));
                fs::create_dir(&dir).unwrap();
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
            }
            write(
                &home.join(role),
                format!(
                    r#"#!/bin/sh
[ "$1" = run ] && [ "$2" = --mcp.server-url ] || exit 16
[ "$PWD" = "$0.cwd" ] && [ "$HOME" = "$0.home" ] && [ "$PATH" = "/usr/bin:/bin" ] || exit 18
status=$(/usr/bin/curl --noproxy '*' -sS -m 2 -o /dev/null -w '%{{http_code}}' -H 'Content-Type: application/json' -d '{{}}' "$3")
[ "$status" = 401 ] || exit 17
printf '%s\n' '{CHILD_TEXT}'
printf '%s\n' '{CHILD_TEXT}' >&2
/bin/sleep 90 &
printf '%s %s\n' "$$" "$!" > "$0.pid"
wait "$!"
"#
                ),
                0o700,
            );
        }
        write(
            &home.join("send-mail"),
            br#"#!/bin/sh
printf '%s %s\n' "$1" "$2" >> "$0.out"
"#,
            0o700,
        );
        let queue_meta = fs::metadata(&queue_path).unwrap();
        let tunnel = |role, path| {
            json!({"executable":home.join(role),
                "args":["run","--mcp.server-url",format!("http://127.0.0.1:{port}{path}")],
                "working_directory":home.join(format!("{role}.cwd")),
                "home":home.join(format!("{role}.home")),
                "path":["/usr/bin","/bin"]
            })
        };
        write(&home.join("companions.json"), json!({
            "version":1,
            "queue_identity":{"device":queue_meta.dev(),"inode":queue_meta.ino()},
            "dot_tunnel":tunnel(DOT, "/dot/mcp"),
            "codex_tunnel":tunnel(CODEX, "/codex/mcp"),
            "email_relay":{
                "registration":registration,"state":home.join("email.cursor"),
                "command":home.join("send-mail"),"mailboxes":["cue","instinct"],"interval_secs":1
            }
        }).to_string(), 0o600);
        Self {
            _tmp: tmp,
            home,
            queue: queue_path,
            port,
            id,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["run", "--home"])
            .arg(&self.home)
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--companions-file")
            .arg(self.home.join("companions.json"))
            .stdin(Stdio::null());
        cmd
    }

    fn direct_command(&self, port: u16) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("serve-hub")
            .arg("--queue")
            .arg(
                self.home
                    .join("..")
                    .join(self.home.file_name().unwrap())
                    .join(self.queue.file_name().unwrap()),
            )
            .args([
                "--bundled-profiles",
                "--port",
                &port.to_string(),
                "--seconds",
                "1",
            ]);
        cmd
    }

    fn managed_command(&self, port: u16) -> Command {
        let registry_dir = self.home.join(".pigeonmq-lifecycle");
        if !registry_dir.exists() {
            fs::create_dir(&registry_dir).unwrap();
            fs::set_permissions(&registry_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        write(
            &registry_dir.join("profiles.json"),
            json!({
                "port":port,
                "registration":self.home.join("pigeonmq-cloud-registration.json"),
                "relay_command":null,"profiles":{}
            })
            .to_string(),
            0o600,
        );
        let mut cmd = Command::new(BIN);
        cmd.args(["serve-managed", "--home"]).arg(&self.home);
        cmd
    }

    fn runner(&self) -> Runner {
        Runner(
            self.command()
                .stdout(fs::File::create(self.home.join("stdout")).unwrap())
                .stderr(fs::File::create(self.home.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        )
    }

    fn state(&self) -> Value {
        fs::read(self.home.join("pigeonmq-cloud-state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    }

    fn ready(&self, condition: impl Fn(&Value) -> bool) -> Value {
        wait("roles running behind the hub", || {
            let state = self.state();
            ([DOT, CODEX, EMAIL]
                .into_iter()
                .all(|role| state["companions"][role]["state"] == "running")
                && condition(&state))
            .then_some(state)
        })
    }

    fn descendant(&self, role: &str, expected: u32) -> u32 {
        wait("owned companion saw required hub route", || {
            let text = fs::read_to_string(self.home.join(format!("{role}.pid"))).ok()?;
            let mut values = text
                .split_whitespace()
                .filter_map(|value| value.parse().ok());
            (values.next()? == expected)
                .then(|| values.next())
                .flatten()
        })
    }

    fn health(&self, expected: bool) -> Value {
        let output = Command::new(BIN)
            .args(["health", "--home"])
            .arg(&self.home)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.success(), expected, "{report}");
        assert_eq!(report["local_ready"], expected);
        assert_eq!(report["remote_verified"], false);
        if expected {
            assert_eq!(report["tunnel"]["public_reachability"], "skipped");
            for role in [DOT, CODEX, EMAIL] {
                assert_eq!(report["companions"][role]["state"], "running");
            }
        }
        report
    }

    fn assert_marker(&self) {
        let (status, json) = hub_http::post(
            self.port,
            "/dot/mcp",
            TOKEN,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"p-999k_read","arguments":{"message_id":self.id}
            }}),
        )
        .unwrap();
        assert_eq!(status, 200);
        let body = json["result"]["content"][0]["text"].as_str().unwrap();
        let message: Value = serde_json::from_str(body).unwrap();
        assert_eq!(message["id"], self.id);
        assert_eq!(message["body"], BODY);
        assert_eq!(message["state"], "pending");
    }

    fn assert_redaction(&self) {
        for name in ["stdout", "stderr", "pigeonmq-cloud-state.json"] {
            let content = fs::read_to_string(self.home.join(name)).unwrap();
            for marker in [BODY, TOKEN, CHILD_TEXT, &self.id] {
                assert!(!content.contains(marker));
            }
        }
    }
}

fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn rejected_owner(command: &mut Command) {
    let mut child = Runner(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let result = wait("competing hub refusal", || child.0.try_wait().unwrap());
    assert!(!result.success());
    let mut error = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert_eq!(error.trim(), "hub_queue_already_owned");
}

#[test]
fn hub_modes_exclude_the_same_queue_on_other_ports_and_allow_distinct_queues() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let mut primary = fixture.runner();
    fixture.ready(|_| true);
    let other_port = listener().local_addr().unwrap().port();
    assert_ne!(other_port, fixture.port);
    rejected_owner(&mut fixture.direct_command(other_port));
    rejected_owner(&mut fixture.managed_command(other_port));
    let independent = Fixture::new(other_port);
    let mut other = independent.runner();
    independent.ready(|_| true);
    fixture.assert_marker();
    independent.assert_marker();
    other.stop(false);
    primary.stop(false);
    let lock = fixture.home.join("pigeonmq-cloud-queue.sqlite3.hub.lock");
    let metadata = fs::metadata(&lock).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let mut managed = Runner(
        fixture
            .managed_command(other_port)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait("managed hub owns queue", || {
        TcpStream::connect(("127.0.0.1", other_port)).ok()
    });
    rejected_owner(&mut fixture.direct_command(fixture.port));
    managed.stop(true);
    let direct = fixture.direct_command(fixture.port).output().unwrap();
    assert!(
        direct.status.success(),
        "direct hub did not take the released lock"
    );
    assert_eq!(fs::metadata(lock).unwrap().ino(), metadata.ino());
}

#[test]
fn unsafe_hub_lock_is_not_followed_or_replaced() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let lock = fixture.home.join("pigeonmq-cloud-queue.sqlite3.hub.lock");
    let victim = fixture.home.join("unrelated-file");
    write(&victim, "keep this file", 0o600);
    std::os::unix::fs::symlink(&victim, &lock).unwrap();
    let output = fixture.direct_command(fixture.port).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "hub_queue_lock_unavailable"
    );
    assert!(
        fs::symlink_metadata(&lock)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(victim).unwrap(), "keep this file");
}

#[test]
fn tunnel_restarts_do_not_replace_hub_queue_or_siblings() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let inode = fs::metadata(&fixture.queue).unwrap().ino();
    let mut runner = fixture.runner();
    let mut state = fixture.ready(|_| true);
    let hub = pid(&state["host_pid"]);
    for (failed, sibling) in [(DOT, CODEX), (CODEX, DOT)] {
        let stopped = pid(&state["companions"][failed]["pid"]);
        let descendant = fixture.descendant(failed, stopped);
        let sibling_pid = pid(&state["companions"][sibling]["pid"]);
        let relay_pid = pid(&state["companions"][EMAIL]["pid"]);
        assert!(signal(stopped, libc::SIGKILL));
        wait("independent retry", || {
            (fixture.state()["companions"][failed]["state"] == "retrying").then_some(())
        });
        fixture.assert_marker();
        state = fixture.ready(|state| pid(&state["companions"][failed]["pid"]) != stopped);
        gone(descendant);
        assert_eq!(pid(&state["host_pid"]), hub);
        assert_eq!(pid(&state["companions"][sibling]["pid"]), sibling_pid);
        assert_eq!(pid(&state["companions"][EMAIL]["pid"]), relay_pid);
        assert_eq!(fs::metadata(&fixture.queue).unwrap().ino(), inode);
        fixture.health(true);
    }
    wait("email cursor", || {
        fixture.home.join("email.cursor").exists().then_some(())
    });
    let mut queue = Queue::open_existing(&fixture.queue).unwrap();
    let outgoing = queue
        .send(Envelope {
            sender: "dot".into(),
            recipient: "cue".into(),
            body: BODY.into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        })
        .unwrap()
        .message
        .id;
    wait(
        "existing relay command receives mailbox alias and ID",
        || {
            (fs::read_to_string(fixture.home.join("send-mail.out"))
                .ok()?
                .trim()
                == format!("cue {outgoing}"))
            .then_some(())
        },
    );
    let descendants =
        [DOT, CODEX].map(|role| fixture.descendant(role, pid(&state["companions"][role]["pid"])));
    runner.stop(false);
    for role in [DOT, CODEX, EMAIL] {
        gone(pid(&state["companions"][role]["pid"]));
    }
    for child in descendants {
        gone(child);
    }
    gone(hub);
    fixture.health(false);
    fixture.assert_redaction();
}

#[test]
fn a_hard_runner_exit_releases_hub_and_owned_groups_before_restart() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let mut runner = fixture.runner();
    let first = fixture.ready(|_| true);
    let descendants =
        [DOT, CODEX].map(|role| fixture.descendant(role, pid(&first["companions"][role]["pid"])));
    runner.stop(true);
    gone(pid(&first["host_pid"]));
    for role in [DOT, CODEX, EMAIL] {
        gone(pid(&first["companions"][role]["pid"]));
    }
    for child in descendants {
        gone(child);
    }
    wait("stale state unavailable after lock release", || {
        let output = Command::new(BIN)
            .args(["health", "--home"])
            .arg(&fixture.home)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).ok()?;
        (report["reason"] == "runner_inactive").then_some(())
    });
    let mut runner = fixture.runner();
    let second = fixture.ready(|state| state["host_pid"] != first["host_pid"]);
    for role in [DOT, CODEX, EMAIL] {
        assert_ne!(
            second["companions"][role]["pid"],
            first["companions"][role]["pid"]
        );
    }
    fixture.assert_marker();
    runner.stop(false);
}

#[test]
fn a_foreign_local_http_listener_and_stale_state_cannot_start_companions() {
    let foreign = listener();
    let fixture = Fixture::new(foreign.local_addr().unwrap().port());
    let state = fixture.home.join("pigeonmq-cloud-state.json");
    let mut runner = fixture.runner();
    wait("blocked hub startup", || {
        (fixture.state()["state"] == "recovering").then_some(())
    });
    write(&state, json!({"state":"local_ready","local_hub":"running","tunnel_mode":"private_companions",
        "local_port":fixture.port,"host_pid":runner.0.id(),"companions":{"dot_tunnel":{"state":"running"}}}).to_string(), 0o600);
    let fake = thread::spawn(move || {
        foreign.set_nonblocking(true).unwrap();
        let limit = Instant::now() + Duration::from_millis(500);
        while Instant::now() < limit {
            if let Ok((mut stream, _)) = foreign.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let _ = stream.read(&mut [0; 1024]);
                let body = r#"{"error":{"code":"route_not_found"}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    assert!(fixture.health(false)["companions"].is_null());
    assert!(!fixture.home.join(format!("{DOT}.pid")).exists());
    assert!(!fixture.home.join("email.cursor").exists());
    fake.join().unwrap();
    fixture.ready(|_| true);
    runner.stop(false);
}

#[test]
fn hub_failure_restarts_companions_only_after_owned_hub_recovers() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let inode = fs::metadata(&fixture.queue).unwrap().ino();
    let mut runner = fixture.runner();
    let first = fixture.ready(|_| true);
    let hub = pid(&first["host_pid"]);
    let descendants =
        [DOT, CODEX].map(|role| fixture.descendant(role, pid(&first["companions"][role]["pid"])));
    assert!(signal(hub, libc::SIGKILL));
    let second = fixture.ready(|state| state["host_pid"] != first["host_pid"]);
    gone(hub);
    for child in descendants {
        gone(child);
    }
    for role in [DOT, CODEX, EMAIL] {
        let previous = pid(&first["companions"][role]["pid"]);
        let current = pid(&second["companions"][role]["pid"]);
        assert_ne!(previous, current);
        gone(previous);
        if role != EMAIL {
            fixture.descendant(role, current);
        }
    }
    assert_eq!(fs::metadata(&fixture.queue).unwrap().ino(), inode);
    fixture.assert_marker();
    runner.stop(false);
}

#[test]
fn launchd_relaunch_cannot_adopt_a_replacement_queue() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let mut runner = fixture.runner();
    let first = fixture.ready(|_| true);
    let next = fixture.home.join("different-queue.sqlite3");
    drop(Queue::open(&next).unwrap());
    fs::set_permissions(&next, fs::Permissions::from_mode(0o600)).unwrap();
    let old = fixture.home.join("existing-queue.sqlite3");
    fs::rename(&fixture.queue, &old).unwrap();
    fs::rename(next, &fixture.queue).unwrap();
    assert_ne!(
        fs::metadata(&fixture.queue).unwrap().ino(),
        fs::metadata(old).unwrap().ino()
    );
    let status = wait("runner refuses replaced queue", || {
        runner.0.try_wait().unwrap()
    });
    assert!(!status.success());
    for role in [DOT, CODEX, EMAIL] {
        gone(pid(&first["companions"][role]["pid"]));
    }
    gone(pid(&first["host_pid"]));
    let output = fixture.command().output().unwrap();
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["reason"], "queue_identity_changed_restart_required");
    assert!(!fixture.health(false)["local_ready"].as_bool().unwrap());
}

#[test]
fn guard_death_reaps_its_group_before_only_that_role_retries() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let mut runner = fixture.runner();
    let first = fixture.ready(|_| true);
    let target = pid(&first["companions"][DOT]["pid"]);
    let descendant = fixture.descendant(DOT, target);
    // SAFETY: target belongs to this fixture, and getpgid only queries its group.
    let group = unsafe { libc::getpgid(target as i32) };
    assert!(group > 0 && group as u32 != target);
    assert!(signal(group as u32, libc::SIGKILL));
    let second = fixture.ready(|state| pid(&state["companions"][DOT]["pid"]) != target);
    gone(target);
    gone(descendant);
    gone(group as u32);
    assert_eq!(second["host_pid"], first["host_pid"]);
    for role in [CODEX, EMAIL] {
        assert_eq!(
            second["companions"][role]["pid"],
            first["companions"][role]["pid"]
        );
    }
    fixture.assert_marker();
    runner.stop(false);
}

#[test]
fn inline_credentials_and_mismatched_routes_are_rejected_without_echo() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let path = fixture.home.join("companions.json");
    let valid: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for extra in [
        vec!["--control-plane.api-key".into(), TOKEN.into()],
        vec![format!("--control-plane.api-key={TOKEN}")],
        vec![
            "--mcp.extra-headers".into(),
            format!("Authorization: Bearer {TOKEN}"),
        ],
        vec![format!(
            "--mcp.discovery-extra-headers=Authorization: Bearer {TOKEN}"
        )],
        vec![format!(
            "--mcp.server-url=http://127.0.0.1:{}/codex/mcp",
            fixture.port
        )],
    ] {
        let mut invalid = valid.clone();
        invalid["dot_tunnel"]["args"]
            .as_array_mut()
            .unwrap()
            .extend(extra.into_iter().map(Value::from));
        write(&path, invalid.to_string(), 0o600);
        let output = fixture.command().output().unwrap();
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["reason"], "companion_tunnel_args_invalid");
        assert!(!report.to_string().contains(TOKEN));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
        assert!(!fixture.home.join(format!("{DOT}.pid")).exists());
        assert!(!fixture.home.join("email.cursor").exists());
    }
}

#[test]
fn malformed_and_non_private_manifests_fail_without_starting_children() {
    let fixture = Fixture::new(listener().local_addr().unwrap().port());
    let path = fixture.home.join("companions.json");
    let valid = fs::read(&path).unwrap();
    for (bytes, mode) in [
        (valid.as_slice(), 0o644),
        (
            br#"{"version":1,"version":1,"dot_tunnel":null}"#.as_slice(),
            0o600,
        ),
        (
            br#"{"version":1,"unexpected":"fixture-companion-private-output"}"#,
            0o600,
        ),
    ] {
        write(&path, bytes, mode);
        let output = fixture.command().output().unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CHILD_TEXT));
        assert!(!fixture.home.join("pigeonmq-cloud-run.lock").exists());
    }
}
