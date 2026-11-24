use super::{
    Fault, auth, inbox_page, mapped, terminal_confirmation_fault, terminal_observation_fault,
    transport::Input,
};
use crate::mcp_sdk;
use radio999k::{Error, Queue};
use radio999k::{
    mailbox_tools::registered_mailbox_tools,
    mcp_request::{
        accepted_cancellation, check_streamable_mailbox_body, registered_mailbox_fields,
    },
};
use rmcp::{
    ErrorData,
    model::{ErrorCode, RequestId},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

mod policy;
pub(crate) use policy::validate_grok_setup_policy;

// Grok-bot is a separate authenticated profile of the existing loopback transport.
// This path never creates a queue, emits Events, starts a model, or uses dot ACLs.
// FIXME(yagni): 8-worker cap for a single Grok client - per-key dedupe already blocks repeats
const GROK_MAX_WORKERS: usize = 8;
const GROK_PROCESSING_BUDGET: Duration = Duration::from_secs(15);

pub(super) struct GrokState {
    db: PathBuf,
    policy: PathBuf,
    deadline: Option<Instant>,
    waiting: AtomicBool,
    active: Mutex<HashMap<RequestId, Arc<AtomicBool>>>,
}
struct ActiveGrokRequest<'a> {
    state: &'a GrokState,
    key: RequestId,
    cancelled: Arc<AtomicBool>,
}
impl Drop for ActiveGrokRequest<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.state.active.lock() {
            active.remove(&self.key);
        }
    }
}
struct GrokWaitGuard<'a>(&'a AtomicBool);
impl Drop for GrokWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl GrokState {
    pub(super) fn new(db: PathBuf, policy: PathBuf, deadline: Option<Instant>) -> Self {
        Self {
            db,
            policy,
            deadline,
            waiting: AtomicBool::new(false),
            active: Mutex::new(HashMap::new()),
        }
    }

    fn begin(&self, key: RequestId) -> Result<ActiveGrokRequest<'_>, Fault> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| (503, "request_state_unavailable"))?;
        if active.contains_key(&key) || active.len() >= GROK_MAX_WORKERS {
            return Err((409, "request_already_active"));
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        active.insert(key.clone(), cancelled.clone());
        Ok(ActiveGrokRequest {
            state: self,
            key,
            cancelled,
        })
    }
    fn check_open(&self, active: &ActiveGrokRequest<'_>, deadline: Instant) -> Result<(), Fault> {
        if active.cancelled.load(Ordering::Acquire)
            || Instant::now() >= deadline
            || self.deadline.is_some_and(|end| Instant::now() >= end)
        {
            return Err((499, "request_cancelled"));
        }
        Ok(())
    }
    fn scope(
        &self,
        authorization: &[Vec<u8>],
        operation: policy::GrokOperation,
        active: &ActiveGrokRequest<'_>,
        deadline: Instant,
    ) -> Result<radio999k::grok_mailbox::GrokScope, Fault> {
        self.check_open(active, deadline)?;
        policy::authenticate_grok(&self.policy, authorization)?.scope(operation)
    }
    fn send_checkpoint(
        &self,
        authorization: &[Vec<u8>],
        active: &ActiveGrokRequest<'_>,
        deadline: Instant,
        recipient: &str,
        thread_peers: Option<&[String]>,
    ) -> Result<(), Fault> {
        self.check_open(active, deadline)?;
        let principal = policy::authenticate_grok(&self.policy, authorization)?;
        let scope = principal.scope(policy::GrokOperation::Send)?;
        auth::check_registered_send(&scope.allowed_senders, recipient, thread_peers, || {
            self.check_open(active, deadline)?;
            principal.require_operation(policy::GrokOperation::Read)
        })
    }
    fn confirm_scope(
        &self,
        authorization: &[Vec<u8>],
        active: &ActiveGrokRequest<'_>,
        deadline: Instant,
    ) -> Result<radio999k::grok_mailbox::GrokScope, Fault> {
        // Reload at each checkpoint, not once per request or per individual grant.
        self.check_open(active, deadline)?;
        let principal = policy::authenticate_grok(&self.policy, authorization)?;
        let scope = principal.scope(policy::GrokOperation::Reply)?;
        self.check_open(active, deadline)?;
        principal.require_operation(policy::GrokOperation::Read)?;
        Ok(scope)
    }
}

fn grok_fault(error: radio999k::grok_mailbox::MailboxError) -> Fault {
    let status = match error.reason {
        "forbidden" => 403,
        "unauthorized" | "credential_expired" => 401,
        "request_cancelled" => 499,
        "message_not_available" => 404,
        "wait_busy"
        | "reply_conflict"
        | "reconciliation_required"
        | "existing_claim_requires_reconciliation" => 409,
        "storage_unavailable" | "auth_unavailable" => 503,
        _ => 400,
    };
    (status, error.reason)
}

pub(super) fn dispatch_grok(
    state: &GrokState,
    authority: &str,
    input: &Input,
    body: &[u8],
    max_body: usize,
) -> Result<http::Response<Vec<u8>>, Fault> {
    let authorization = &input.authorization;
    policy::authenticate_grok(&state.policy, authorization)?;
    let now = Instant::now();
    let budget = now + GROK_PROCESSING_BUDGET;
    let deadline = state.deadline.map_or(budget, |end| end.min(budget));
    if now >= deadline {
        return Err((499, "request_cancelled"));
    }
    check_streamable_mailbox_body(body)?;
    let request = mcp_sdk::streamable_request(&input.path, &input.sdk_headers, body)?;
    let mut active = None;
    let response = mcp_sdk::dispatch_streamable(
        "999k-grok-bot",
        registered_mailbox_tools("Grok Bot", "grok-bot"),
        request,
        authority,
        (max_body, deadline),
        |id, name, arguments| {
            // Retain the same guard through all preflight and fan-out calls.
            let active = match &active {
                Some(active) => active,
                None => active.insert(state.begin(id.clone())?),
            };
            call_grok_tool(state, authorization, active, deadline, name, &arguments)
        },
        |_, _| {
            Ok(Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "method_not_found",
                None,
            )))
        },
    )?;
    if let Some(key) = accepted_cancellation(response.status(), body)? {
        let active = state
            .active
            .lock()
            .map_err(|_| (503, "request_state_unavailable"))?;
        if let Some(cancelled) = active.get(&key) {
            cancelled.store(true, Ordering::Release);
        }
        // Best effort only. A completion that already committed is not rolled back.
    }
    Ok(response)
}

fn call_grok_tool(
    state: &GrokState,
    authorization: &[Vec<u8>],
    active: &ActiveGrokRequest<'_>,
    deadline: Instant,
    name: &str,
    arguments: &Value,
) -> Result<Value, Fault> {
    Ok(match name {
        "radio999k_inbox" => inbox_page::list_with(
            &state.db,
            "grok-bot",
            arguments,
            deadline,
            || {
                state
                    .scope(authorization, policy::GrokOperation::Read, active, deadline)
                    .map(|scope| scope.allowed_senders)
            },
            || {
                if radio999k::grok_mailbox::is_prepared(&state.db).map_err(|e| (503, e.reason))? {
                    Ok(())
                } else {
                    Err((503, "mailbox_not_prepared"))
                }
            },
            |id, allowed| {
                use radio999k::grok_mailbox::{GrokScope, read};
                match read(
                    &state.db,
                    id,
                    &GrokScope {
                        allowed_senders: allowed.to_vec(),
                    },
                ) {
                    Ok(m) => Ok(Some(json!({
                        "id": m.id, "sender": m.sender, "recipient": m.recipient,
                        "conversation_id": m.conversation_id, "correlation_id": m.correlation_id,
                        "in_reply_to": m.in_reply_to, "state": m.state, "reply_id": m.reply_id,
                    }))),
                    Err(e) => match e.reason {
                        "message_not_available" | "invalid_body" | "invalid_identifier" => Ok(None),
                        "forbidden" => Err((403, "forbidden")),
                        reason => Err((503, reason)),
                    },
                }
            },
        )?,
        "radio999k_recipients" => {
            registered_mailbox_fields(arguments, &[], &[])?;
            let scope =
                state.scope(authorization, policy::GrokOperation::Send, active, deadline)?;
            json!({"recipients":scope.allowed_senders,"connection_status":"unknown"})
        }
        "radio999k_confirm_terminal" => {
            let args = registered_mailbox_fields(arguments, &["message_id"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            if message_id.trim().is_empty() || message_id.len() > 128 {
                return Err((400, "invalid_fields"));
            }
            let confirm_scope = || state.confirm_scope(authorization, active, deadline);
            let visible = |sender: &str| {
                if confirm_scope()?
                    .allowed_senders
                    .iter()
                    .any(|peer| peer == sender)
                {
                    Ok(())
                } else {
                    Err((404, "not_found"))
                }
            };
            let observed = radio999k::grok_mailbox::read(&state.db, message_id, &confirm_scope()?)
                .map_err(|error| terminal_observation_fault(grok_fault(error)))?;
            if observed.in_reply_to.is_none() {
                visible(&observed.sender)?;
                return Err((400, "terminal_answer_required"));
            }
            let mut queue = Queue::open_existing(&state.db).map_err(mapped)?;
            let mut fault = None;
            let result = queue.confirm_terminal_checked("grok-bot", message_id, |sender| {
                visible(sender).map_err(|error| {
                    fault = Some(error);
                    Error::Invalid("authorization_changed")
                })
            });
            let duplicate = result.map_err(|error| {
                fault.unwrap_or_else(|| terminal_confirmation_fault(error, mapped))
            })?;
            json!({"state":"acknowledged","message_id":message_id,"duplicate":duplicate})
        }
        radio999k::mailbox_tools::READ_THREAD => radio999k::conversation::read_thread_tool(
            &state.db,
            "grok-bot",
            radio999k::grok_mailbox::MAX_TEXT as i64,
            arguments,
            || {
                state
                    .scope(authorization, policy::GrokOperation::Read, active, deadline)
                    .map(|s| s.allowed_senders)
            },
        )?,
        "radio999k_read" => {
            let args = registered_mailbox_fields(arguments, &["message_id"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            let scope =
                state.scope(authorization, policy::GrokOperation::Read, active, deadline)?;
            let observed =
                radio999k::grok_mailbox::read(&state.db, message_id, &scope).map_err(grok_fault)?;
            let current =
                state.scope(authorization, policy::GrokOperation::Read, active, deadline)?;
            if !current.allowed_senders.contains(&observed.sender) {
                return Err((403, "forbidden"));
            }
            serde_json::to_value(observed).map_err(|_| (500, "serialization_failed"))?
        }
        "radio999k_wait" => {
            let args = registered_mailbox_fields(arguments, &["timeout_ms"], &[])?;
            let timeout = args["timeout_ms"]
                .as_u64()
                .filter(|value| *value <= 10000)
                .ok_or((400, "invalid_timeout"))?;
            state.scope(authorization, policy::GrokOperation::Wait, active, deadline)?;
            if state
                .waiting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err((409, "wait_busy"));
            }
            let _waiting = GrokWaitGuard(&state.waiting);
            let until = deadline.min(Instant::now() + Duration::from_millis(timeout));
            // Internal raw-page position only. It is never accepted from
            // clients, persisted, returned as a cursor, or treated as ACK.
            // Requests take priority so a repeated terminal answer cannot
            // hide a pending request. Terminal observation still does not ACK.
            let mut after_seq = 0;
            let mut terminal = false;
            loop {
                let scope =
                    state.scope(authorization, policy::GrokOperation::Wait, active, deadline)?;
                if timeout != 0 && Instant::now() >= until {
                    break json!({"state":"timeout","message":null});
                }
                let page =
                    radio999k::grok_mailbox::scan_page(&state.db, &scope, after_seq, terminal)
                        .map_err(grok_fault)?;
                let current =
                    state.scope(authorization, policy::GrokOperation::Wait, active, deadline)?;
                if timeout != 0 && Instant::now() >= until {
                    break json!({"state":"timeout","message":null});
                }
                if let Some(observed) = page.message {
                    if !current.allowed_senders.contains(&observed.sender) {
                        return Err((403, "forbidden"));
                    }
                    break json!({"state":"message","message":observed});
                }
                if page.scanned == radio999k::grok_mailbox::SCAN_PAGE_SIZE {
                    if timeout == 0 {
                        // Do not misreport an incomplete zero-wait scan as
                        // an empty mailbox. Use a bounded wait or read by ID.
                        return Err((503, "observation_budget_exhausted"));
                    }
                    after_seq = page.next_seq;
                    continue;
                }
                if !terminal {
                    terminal = true;
                    after_seq = 0;
                    continue;
                }
                let now = Instant::now();
                if timeout == 0 || now >= until {
                    break json!({"state":"timeout","message":null});
                }
                after_seq = 0;
                terminal = false;
                thread::sleep((until - now).min(Duration::from_millis(100)));
            }
        }
        "radio999k_reply" => {
            let args = registered_mailbox_fields(arguments, &["message_id", "body"], &[])?;
            let message_id = args["message_id"].as_str().ok_or((400, "invalid_fields"))?;
            let body = args["body"].as_str().ok_or((400, "invalid_fields"))?;
            let result = radio999k::grok_mailbox::reply(&state.db, message_id, body, || {
                state
                    .scope(
                        authorization,
                        policy::GrokOperation::Reply,
                        active,
                        deadline,
                    )
                    .map_err(|(_, reason)| radio999k::grok_mailbox::MailboxError { reason })
            })
            .map_err(grok_fault)?;
            serde_json::to_value(result).map_err(|_| (500, "serialization_failed"))?
        }
        "radio999k_send" => super::send_registered(
            &state.db,
            "grok-bot",
            radio999k::grok_mailbox::MAX_TEXT as i64,
            arguments,
            || {
                state
                    .scope(authorization, policy::GrokOperation::Read, active, deadline)
                    .map(|scope| scope.allowed_senders)
            },
            |recipient, peers| {
                state.send_checkpoint(authorization, active, deadline, recipient, peers)
            },
        )?,
        _ => return Err((404, "unknown_tool")),
    })
}

/// Read-only readiness. A policy file is configuration, not permission to create credentials.
pub fn check_grok(db: &std::path::Path, policy: &std::path::Path) -> io::Result<bool> {
    auth::require_private_file(db).map_err(|_| io::Error::other("private database required"))?;
    policy::validate_grok(policy)
        .map_err(|(_, reason)| auth::local_policy_error(reason, "invalid Cloud Grok Bot policy"))?;
    radio999k::grok_mailbox::is_prepared(db)
        .map_err(|_| io::Error::other("Cloud Grok Bot storage unavailable"))
}

/// Read scope for the wake notifier, with Wait as the fallback.
pub(super) fn observation_scope(
    path: &std::path::Path,
) -> Result<radio999k::grok_mailbox::GrokScope, Fault> {
    match policy::local_grok_scope(path, policy::GrokOperation::Read) {
        Err((403, "forbidden")) => policy::local_grok_scope(path, policy::GrokOperation::Wait),
        result => result,
    }
}

/// Explicit approved installation. Never called implicitly by check or serve.
pub fn prepare_grok(db: &std::path::Path, policy: &std::path::Path) -> io::Result<()> {
    auth::require_private_file(db).map_err(|_| io::Error::other("private database required"))?;
    policy::local_grok_scope(policy, policy::GrokOperation::Reply).map_err(|(_, reason)| {
        auth::local_policy_error(reason, "Cloud Grok Bot preparation forbidden")
    })?;
    radio999k::grok_mailbox::prepare(db)
        .map_err(|_| io::Error::other("Cloud Grok Bot preparation failed"))
}
#[cfg(all(test, unix))]
mod grok_authority_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    fn private(path: &std::path::Path, value: &Value) {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    const ACCEPT: &str = "Accept: application/json, text/event-stream\r\n";
    fn post(
        state: &GrokState,
        token: &str,
        headers: &str,
        body: &[u8],
    ) -> Result<http::Response<Vec<u8>>, Fault> {
        let authority = "127.0.0.1:1234";
        let head = format!(
            "POST /grok/mcp HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        );
        let input = super::super::parse_head(head.as_bytes(), authority)?;
        dispatch_grok(
            state,
            authority,
            &input,
            body,
            radio999k::mailbox_catalog::by_adapter(radio999k::mailbox_catalog::Adapter::Grok)
                .body_limit(super::super::MAX_HEADERS),
        )
    }

    #[test]
    fn grok_wait_cancels_only_after_authenticated_sdk_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        drop(Queue::open(&db).unwrap());
        radio999k::grok_mailbox::prepare(&db).unwrap();
        let policy = dir.path().join("policy.json");
        let registry = dir.path().join("registration.json");
        private(
            &registry,
            &json!({
                "grok-bot":{"mailbox":"grok-bot","revoked":false},
                "hermes":{"mailbox":"hermes","revoked":false}
            }),
        );
        let hash = Sha256::digest(b"public-fixture")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        private(
            &policy,
            &json!({"principal":"grok-bot","mailbox":"grok-bot","token_sha256":hash,
            "operations":["wait"],"enabled":true,"expires_at_ms":u64::MAX,"registration":registry}),
        );
        let state = GrokState::new(db, policy, None);
        let cancel =
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"42"}}"#;
        let wait = br#"{"jsonrpc":"2.0","id":"42","method":"tools/call","params":{"name":"radio999k_wait","arguments":{"timeout_ms":10000}}}"#;
        let send = |token, headers, body| post(&state, token, headers, body);
        thread::scope(|scope| {
            let in_flight = scope.spawn(|| send("public-fixture", ACCEPT, wait));
            let until = Instant::now() + Duration::from_secs(3);
            while !state.waiting.load(Ordering::Acquire) {
                assert!(Instant::now() < until && !in_flight.is_finished());
                thread::sleep(Duration::from_millis(10));
            }

            assert_eq!(
                send("wrong", ACCEPT, cancel).unwrap_err(),
                (401, "unauthorized")
            );
            assert_eq!(
                send("public-fixture", "", cancel).unwrap().status(),
                http::StatusCode::NOT_ACCEPTABLE
            );
            let numeric = br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42}}"#;
            assert_eq!(
                send("public-fixture", ACCEPT, numeric).unwrap().status(),
                http::StatusCode::ACCEPTED
            );
            assert_eq!(
                send("public-fixture", ACCEPT, wait).unwrap_err(),
                (409, "request_already_active")
            );

            let accepted = send("public-fixture", ACCEPT, cancel).unwrap();
            assert_eq!(accepted.status(), http::StatusCode::ACCEPTED);
            assert!(accepted.body().is_empty());
            assert_eq!(
                in_flight.join().unwrap().unwrap_err(),
                (499, "request_cancelled")
            );
            assert!(!state.waiting.load(Ordering::Acquire));
        });
    }

    #[test]
    fn central_authority_send_read_wait_and_revocation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        let mut queue = Queue::open(&db).unwrap();
        radio999k::grok_mailbox::prepare(&db).unwrap();
        let registry = dir.path().join("registration.json");
        let mut entries = json!({"grok-bot":{"mailbox":"grok-bot","revoked":false},"hermes":{"mailbox":"hermes","revoked":false}});
        private(&registry, &entries);
        let policy_path = dir.path().join("policy.json");
        let mut policy = json!({"principal":"grok-bot","mailbox":"grok-bot","token_sha256":Sha256::digest(b"public-fixture").iter().map(|b| format!("{b:02x}")).collect::<String>(),"operations":["read","wait","reply"],"enabled":true,"expires_at_ms":u64::MAX,"registration":registry});
        private(&policy_path, &policy);
        let state = GrokState {
            db: db.clone(),
            policy: policy_path.clone(),
            deadline: None,
            waiting: AtomicBool::new(false),
            active: Mutex::new(HashMap::new()),
        };
        let active = state.begin(RequestId::String("test".into())).unwrap();
        let auth = vec![b"Bearer public-fixture".to_vec()];
        let call = |name: &str, args: Value| {
            call_grok_tool(
                &state,
                &auth,
                &active,
                Instant::now() + Duration::from_secs(10),
                name,
                &args,
            )
        };
        let rpc = |id: Value, method: &str, params: Value| {
            let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            let response = post(
                &state,
                "public-fixture",
                ACCEPT,
                request.to_string().as_bytes(),
            )
            .unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            let response: Value = serde_json::from_slice(response.body()).unwrap();
            assert_eq!(response["id"], id);
            response
        };
        let tool = |name: &str, arguments: Value| {
            rpc(
                json!(i64::MAX),
                "tools/call",
                json!({"name":name,"arguments":arguments}),
            )
        };
        let failed = |value: Value, reason: &str| {
            assert_eq!(value["result"]["isError"], true);
            assert!(value.get("error").is_none());
            let details: Value =
                serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(details["error"], reason);
        };
        assert_eq!(rpc(json!("ping"), "ping", json!({}))["result"], json!({}));
        assert_eq!(
            rpc(json!("unknown"), "unknown_method", json!({}))["error"]["code"],
            -32601
        );
        assert_eq!(tool("unknown_tool", json!({}))["error"]["code"], -32602);
        assert_eq!(
            tool("radio999k_read", json!({"message_id":8}))["error"]["code"],
            -32602
        );
        failed(
            tool("radio999k_read", json!({"message_id":"missing-fixture"})),
            "message_not_available",
        );
        let send = json!({"recipient":"hermes","body":"fixture","idempotency_key":"fixture-send"});
        failed(tool("radio999k_send", send.clone()), "forbidden");
        assert_eq!(
            call("radio999k_send", send.clone()),
            Err((403, "forbidden"))
        );
        assert!(
            queue
                .receive("hermes", Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
        policy["operations"] = json!(["wait"]);
        private(&policy_path, &policy);
        assert!(
            super::policy::local_grok_scope(&policy_path, super::policy::GrokOperation::Read)
                .is_err()
        );
        policy["operations"] = json!(["send", "read", "wait", "reply"]);
        private(&policy_path, &policy);
        assert!(call("radio999k_send", send.clone()).is_ok());
        failed(
            tool(
                "radio999k_send",
                json!({"recipient":"hermes,unknown,hermes","body":"fixture","idempotency_key":"multi"}),
            ),
            "subset_recipient_not_authorized",
        );
        active.cancelled.store(true, Ordering::Release);
        assert_eq!(
            call("radio999k_send", send.clone()),
            Err((499, "request_cancelled"))
        );
        active.cancelled.store(false, Ordering::Release);

        assert_eq!(
            call(
                "radio999k_send",
                json!({"recipient":"hermes","body":"x","idempotency_key":"fixture-invalid","correlation_id":3})
            ),
            Err((400, "invalid_fields"))
        );
        let envelope = |sender: &str, recipient: &str| radio999k::Envelope {
            sender: sender.into(),
            recipient: recipient.into(),
            body: "fixture".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        };
        let id = queue
            .send(envelope("hermes", "grok-bot"))
            .unwrap()
            .message
            .id;
        assert!(call("radio999k_read", json!({"message_id":id})).is_ok());
        assert_eq!(
            call("radio999k_wait", json!({"timeout_ms":0})).unwrap()["message"]["id"],
            id
        );
        let unknown = queue
            .send(envelope("unknown", "grok-bot"))
            .unwrap()
            .message
            .id;
        assert!(call("radio999k_read", json!({"message_id":unknown})).is_err());
        let other = queue.send(envelope("hermes", "dot")).unwrap().message.id;
        assert!(call("radio999k_read", json!({"message_id":other})).is_err());
        let inbox = call("radio999k_inbox", json!({})).unwrap();
        let messages = inbox["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], id);
        assert_eq!(messages[0]["recipient"], "grok-bot");
        assert!(messages[0].get("body").is_none());
        assert_eq!(inbox["metadata_only"], true);
        policy["operations"] = json!(["wait"]);
        private(&policy_path, &policy);
        assert_eq!(call("radio999k_inbox", json!({})), Err((403, "forbidden")));
        policy["operations"] = json!(["send", "read", "wait", "reply"]);
        private(&policy_path, &policy);
        let completion = call("radio999k_reply", json!({"message_id":id,"body":"reply"})).unwrap();
        assert_eq!(completion["state"], "replied");
        assert_eq!(completion["duplicate"], false);
        let replay = call("radio999k_reply", json!({"message_id":id,"body":"reply"})).unwrap();
        assert_eq!(replay["duplicate"], true);
        assert_eq!(replay["reply_id"], completion["reply_id"]);
        failed(
            tool(
                "radio999k_reply",
                json!({"message_id":id,"body":"different reply"}),
            ),
            "reply_conflict",
        );
        let replay = call("radio999k_reply", json!({"message_id":id,"body":"reply"})).unwrap();
        assert_eq!(replay["reply_id"], completion["reply_id"]);
        entries["hermes"]["revoked"] = json!(true);
        private(&registry, &entries);
        assert!(call("radio999k_read", json!({"message_id":id})).is_err());
        assert!(call("radio999k_send", send).is_err());
        assert!(call("radio999k_reply", json!({"message_id":id,"body":"reply"})).is_err());
        assert_eq!(
            call("radio999k_wait", json!({"timeout_ms":0})).unwrap()["state"],
            "timeout"
        );
        policy["enabled"] = json!(false);
        private(&policy_path, &policy);
        assert_eq!(
            call("radio999k_read", json!({"message_id":id})),
            Err((401, "unauthorized"))
        );
        policy["enabled"] = json!(true);
        policy["expires_at_ms"] = json!(0);
        private(&policy_path, &policy);
        assert_eq!(
            call(
                "radio999k_send",
                json!({"recipient":"hermes","body":"x","idempotency_key":"fixture-expired"})
            ),
            Err((401, "unauthorized"))
        );
        policy["expires_at_ms"] = json!(u64::MAX);
        private(&policy_path, &policy);
        std::fs::write(&registry, "invalid").unwrap();
        assert_eq!(
            call("radio999k_read", json!({"message_id":id})),
            Err((503, "auth_unavailable"))
        );
        std::fs::remove_file(&registry).unwrap();
        assert!(call("radio999k_wait", json!({"timeout_ms":0})).is_err());
    }
}
