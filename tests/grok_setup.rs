use pigeonmq::private_file::replace_private;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

const POLICY: &str = "pigeonmq-cloud-grok-bot-policy.json";
const REGISTRATION: &str = "pigeonmq-cloud-registration.json";
const QUEUE: &str = "pigeonmq-cloud-queue.sqlite3";
const WAKE: &str = "grok-webhook-keychain";

fn run(home: &Path, digest: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pigeonmq-cloud"))
        .env_clear()
        .args(["setup-grok", "--home"])
        .arg(home)
        .args(["--token-sha256", digest])
        .output()
        .unwrap()
}

fn private_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn put(path: &Path, value: &Value) {
    replace_private(path, &serde_json::to_vec(value).unwrap()).unwrap();
}

fn get(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn result(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fail(output: Output) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn setup_writes_only_private_grok_configuration_and_repeat_preserves_existing_scope() {
    let temp = private_home();
    let home = temp.path().canonicalize().unwrap();
    let register = home.join(REGISTRATION);
    let policy = home.join(POLICY);
    let digest = "a1".repeat(32);
    let mut expected = json!({
        "muse":{"mailbox":"muse","revoked":false},
        "retired":{"mailbox":"retired","revoked":true}
    });
    put(&register, &expected);
    replace_private(&home.join(WAKE), b"synthetic wake marker\n").unwrap();
    let created = result(run(&home, &digest));
    assert_eq!(created["state"], "grok_configured_local_only");
    assert_eq!(created["route"], "/grok/mcp");
    assert_eq!(created["runtime_verified"], false);
    assert_eq!(created["grok_registration_added"], true);
    assert_eq!(created["policy_created"], true);
    assert!(!created.to_string().contains(&digest));
    for path in [&register, &policy] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    expected["grok-bot"] = json!({"mailbox":"grok-bot","revoked":false});
    assert_eq!(get(&register), expected);
    let registry_bytes = fs::read(&register).unwrap();
    let mut existing = get(&policy);
    assert_eq!(existing["registration"], json!(register));
    assert_eq!(existing["token_sha256"], digest);
    assert_eq!(
        existing["operations"],
        json!(["read", "wait", "reply", "send"])
    );
    assert_eq!(existing["principal"], "grok-bot");
    assert_eq!(existing["mailbox"], "grok-bot");
    assert_eq!(existing["enabled"], true);
    existing["operations"] = json!(["read"]);
    put(&policy, &existing);
    let policy_bytes = fs::read(&policy).unwrap();
    let rerun = result(run(&home, &digest.to_ascii_uppercase()));
    assert_eq!(rerun["grok_registration_added"], false);
    assert_eq!(rerun["policy_created"], false);
    assert_eq!(fs::read(&register).unwrap(), registry_bytes);
    assert_eq!(fs::read(&policy).unwrap(), policy_bytes);
    assert_eq!(
        fs::read(home.join(WAKE)).unwrap(),
        b"synthetic wake marker\n"
    );
    assert!(!home.join(QUEUE).exists());
}

#[test]
fn setup_fails_before_registration_for_incompatible_policy_and_resumes_matching_policy() {
    let temp = private_home();
    let home = temp.path().canonicalize().unwrap();
    let policy = home.join(POLICY);
    let register = home.join(REGISTRATION);
    let digest = "a1".repeat(32);
    let expected = json!({
        "principal":"grok-bot", "mailbox":"grok-bot", "token_sha256":digest,
        "operations":["read"], "enabled":true, "expires_at_ms":u64::MAX,
        "registration":register,
    });
    for (field, wrong) in [
        ("token_sha256", json!("b2".repeat(32))),
        ("token_sha256", json!("keychain")),
        ("registration", json!("registration.json")),
        ("operations", json!(["read", "read"])),
        ("expires_at_ms", json!(0)),
        ("enabled", json!(false)),
        ("mailbox", json!("foreign")),
    ] {
        let mut bad = expected.clone();
        bad[field] = wrong;
        put(&policy, &bad);
        fail(run(&home, &digest));
        assert!(!register.exists());
        assert_eq!(get(&policy), bad);
    }
    replace_private(&policy, b"{").unwrap();
    fail(run(&home, &digest));
    assert!(!register.exists());
    assert_eq!(fs::read(&policy).unwrap(), b"{");

    put(&policy, &expected);
    let prior = fs::read(&policy).unwrap();
    let repaired = result(run(&home, &digest));
    assert_eq!(repaired["policy_created"], false);
    assert_eq!(repaired["grok_registration_added"], true);
    assert_eq!(
        get(&register),
        json!({"grok-bot":{"mailbox":"grok-bot","revoked":false}})
    );
    assert_eq!(fs::read(&policy).unwrap(), prior);
}

#[test]
fn setup_does_not_overwrite_revoked_aliased_or_unsafe_registration() {
    let temp = private_home();
    let home = temp.path();
    let policy = home.join(POLICY);
    let registry = home.join(REGISTRATION);
    let digest = "a1".repeat(32);
    fail(run(home, "invalid-digest"));
    assert!(!registry.exists());
    for wrong in [
        json!({"grok-bot":{"mailbox":"grok-bot","revoked":true}}),
        json!({"grok-bot":{"mailbox":"other","revoked":false}}),
        json!({"alias":{"mailbox":"grok-bot","revoked":false}}),
    ] {
        put(&registry, &wrong);
        fail(run(home, &digest));
        assert_eq!(get(&registry), wrong);
        assert!(!policy.exists());
    }
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o644)).unwrap();
    fail(run(home, &digest));
    assert!(!policy.exists());
    fs::remove_file(&registry).unwrap();
    let target = home.join("registry-target.json");
    put(&target, &json!({}));
    std::os::unix::fs::symlink(&target, &registry).unwrap();
    fail(run(home, &digest));
    assert!(!policy.exists());
    assert_eq!(get(&target), json!({}));
}
