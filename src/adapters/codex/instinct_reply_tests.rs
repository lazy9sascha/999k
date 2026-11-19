use super::{Identity, Profile};
use pigeonmq::{Envelope, Queue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt};

const INSTINCT_TOKEN: &str = "PUBLIC_INSTINCT_TEST_NOT_A_CREDENTIAL";
const CUE_TOKEN: &str = "PUBLIC_CUE_TEST_NOT_A_CREDENTIAL";

#[test]
fn instinct_discovery_and_atomic_reply_are_bound_to_its_mailbox() {
    check_scoped_mailbox("instinct", INSTINCT_TOKEN, "cue", CUE_TOKEN);
}

#[test]
fn cue_discovery_and_atomic_reply_are_bound_to_its_mailbox() {
    check_scoped_mailbox("cue", CUE_TOKEN, "instinct", INSTINCT_TOKEN);
}

fn check_scoped_mailbox(
    mailbox: &'static str,
    token: &str,
    other_mailbox: &str,
    other_token: &str,
) {
    let home = tempfile::tempdir().unwrap();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let profile = Profile {
        identity: Identity::BuiltIn(mailbox),
        db: home.path().join("queue.db"),
        policy: home.path().join("policy.json"),
        registration: home.path().join("registration.json"),
    };
    let mut queue = Queue::open(&profile.db).unwrap();
    fs::set_permissions(&profile.db, fs::Permissions::from_mode(0o600)).unwrap();
    let principal = |id: &str, bearer: &str| {
        let digest = Sha256::digest(bearer)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        json!({"id":id,"token_sha256":digest,"operations":["read","wait","reply","send"],
            "send_to":[],"mailboxes":[id],"revoked":false})
    };
    let put_private = |path, value: Value| {
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    };
    put_private(
        &profile.policy,
        json!({"principals":[principal(mailbox, token), principal(other_mailbox, other_token)]}),
    );
    put_private(
        &profile.registration,
        json!({(mailbox):{"mailbox":mailbox,"revoked":false}, "dot":{"mailbox":"dot","revoked":false},
            (other_mailbox):{"mailbox":other_mailbox,"revoked":false}, "revoked":{"mailbox":"revoked","revoked":true}}),
    );

    assert_eq!(profile.route().unwrap(), format!("/{mailbox}/mcp"));
    profile.validate().unwrap();
    let headers = [format!("Bearer {token}").into_bytes()];
    let other_headers = [format!("Bearer {other_token}").into_bytes()];
    let list = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    assert!(profile.dispatch(&[], list).is_err());
    assert!(profile.dispatch(&other_headers, list).is_err());
    let listed = profile.dispatch(&headers, list).unwrap().unwrap();
    let mut names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "pigeonmq_confirm_terminal",
            "pigeonmq_inbox",
            "pigeonmq_read",
            "pigeonmq_read_thread",
            "pigeonmq_recipients",
            "pigeonmq_reply",
            "pigeonmq_send",
            "pigeonmq_wait"
        ]
    );

    let mut send = |sender: &str, recipient: &str| {
        queue
            .send(Envelope {
                sender: sender.into(),
                recipient: recipient.into(),
                body: "synthetic request".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id
    };
    let original = send("dot", mailbox);
    let unrelated = send("dot", other_mailbox);
    let revoked = send("revoked", mailbox);
    assert!(
        profile
            .call(&headers, "pigeonmq_read", json!({"message_id":original}))
            .is_ok()
    );
    assert_eq!(
        profile
            .call(&headers, "pigeonmq_read", json!({"message_id":unrelated}))
            .unwrap_err()
            .0,
        404
    );
    assert!(
        profile
            .call(&headers, "pigeonmq_read", json!({"message_id":revoked}))
            .is_err()
    );
    assert!(
        profile
            .call(&headers, "pigeonmq_wait", json!({"timeout_ms":0}))
            .unwrap()["message"]
            .is_object()
    );
    let inbox = profile.call(&headers, "pigeonmq_inbox", json!({})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert!(
        profile
            .call(
                &headers,
                "pigeonmq_read_thread",
                json!({"conversation_id":original})
            )
            .is_ok()
    );
    let unrelated_thread = profile
        .call(
            &headers,
            "pigeonmq_read_thread",
            json!({"conversation_id":unrelated}),
        )
        .unwrap();
    assert!(unrelated_thread["messages"].as_array().unwrap().is_empty());
    assert!(unrelated_thread["next_cursor"].is_null());
    let recipients = profile
        .call(&headers, "pigeonmq_recipients", json!({}))
        .unwrap();
    assert!(
        recipients["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "dot")
    );
    assert!(
        profile
            .call(
                &headers,
                "pigeonmq_send",
                json!({"recipient":"revoked","body":"must not deliver","idempotency_key":"invalid"})
            )
            .is_err()
    );
    let reply = |id: &str, body: &str| {
        profile.call(
            &headers,
            "pigeonmq_reply",
            json!({"message_id":id,"body":body}),
        )
    };
    assert_eq!(reply(&unrelated, "not authorized").unwrap_err().0, 404);
    assert_eq!(reply(&revoked, "not authorized").unwrap_err().0, 403);
    assert!(
        profile
            .call(
                &other_headers,
                "pigeonmq_reply",
                json!({"message_id":original,"body":"wrong principal"})
            )
            .is_err()
    );
    let answer = "synthetic answer";
    let result = reply(&original, answer).unwrap();
    assert_eq!(result["state"], "replied");
    let reply_id = result["reply_id"].as_str().unwrap();
    assert_eq!(reply(&original, answer).unwrap()["reply_id"], reply_id);
    assert_eq!(reply(&original, "different answer").unwrap_err().0, 409);

    let stored = queue.observe_ordinary(reply_id, "dot").unwrap();
    assert_eq!(stored.envelope.sender, mailbox);
    assert_eq!(stored.envelope.recipient, "dot");
    assert_eq!(stored.envelope.body, answer);
    assert_eq!(
        stored.envelope.in_reply_to.as_deref(),
        Some(original.as_str())
    );
    let metadata = queue.ordinary_metadata(&original, mailbox).unwrap();
    assert_eq!(metadata["state"], "replied");
    assert_eq!(metadata["reply_id"], reply_id);
    let connection = rusqlite::Connection::open(&profile.db).unwrap();
    let linked: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE in_reply_to IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(linked, 1);
    assert_eq!(
        queue.ordinary_metadata(&unrelated, other_mailbox).unwrap()["state"],
        "pending"
    );
    assert_eq!(
        queue.ordinary_metadata(&revoked, mailbox).unwrap()["state"],
        "pending"
    );
    let outgoing = profile.call(&headers, "pigeonmq_send", json!({
        "recipient":"dot","body":"Synthetic request from own mailbox","idempotency_key":"outgoing-fixture"
    })).unwrap();
    assert_eq!(outgoing["message"]["envelope"]["sender"], mailbox);
    let outid = outgoing["message"]["id"].as_str().unwrap();
    let terminal = queue
        .reply_unclaimed_checked(
            "dot",
            outid,
            Envelope {
                sender: "dot".into(),
                recipient: mailbox.into(),
                body: "Synthetic terminal answer".into(),
                correlation_id: Some(outid.into()),
                in_reply_to: Some(outid.into()),
                idempotency_key: Some("terminal-fixture".into()),
            },
            || Ok(()),
        )
        .unwrap();
    let done = profile
        .call(
            &headers,
            "pigeonmq_confirm_terminal",
            json!({"message_id":terminal.id}),
        )
        .unwrap();
    assert_eq!(done["state"], "acknowledged");
}
