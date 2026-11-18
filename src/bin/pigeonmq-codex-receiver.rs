//! Explicit foreground receiver for one approved dedicated Codex owner. No installation.
#[cfg(test)]
#[path = "../../tests/receiver_support/mod.rs"]
mod receiver_support;
use clap::Parser;
#[cfg(test)]
use pigeonmq::adapters::codex::owner::thread_matches;
use pigeonmq::adapters::codex::owner::{connect_native, preflight};
use pigeonmq::{
    adapters::codex::native_transport::{NativeTransport, TransportError},
    adapters::codex::receiver::{Binding, Receiver, load_binding},
    receiver_client::{Client, Transport},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    state: Option<PathBuf>,
    #[arg(long, default_value_t = 54843)]
    hub_port: u16,
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[arg(long)]
    binding: PathBuf,
    /// Execute one request. Without this flag, only validate protected configuration.
    #[arg(long, requires_all = ["state", "token_file"])]
    run: bool,
    /// Keep the same native transport open and process successive approved requests.
    #[arg(long, requires = "run")]
    serve: bool,
    /// Explicit setup attachment to the SAME saved ID, only when no pending work exists.
    #[arg(long, requires = "run")]
    attach_saved: bool,
    /// codex CLI for `app-server daemon start|restart` when the socket is gone.
    /// Default: ~/.local/bin/codex.
    #[arg(long)]
    codex_bin: Option<PathBuf>,
    /// One-time setup, then exit: resume the bound thread and set it to
    /// approvalPolicy "never" with a workspace-write sandbox in the bound cwd.
    /// The receiver itself never changes these settings.
    #[arg(long, conflicts_with = "run")]
    set_unattended: bool,
}
fn unchanged(args: &Args, binding: &Binding) -> Result<(), String> {
    if load_binding(&args.binding).map_err(str::to_owned)? != *binding {
        return Err("codex_binding_changed".into());
    }
    Ok(())
}
/// Connects to the daemon. In serve mode it retries with backoff. Only a
/// missing or dead socket runs `codex app-server daemon start` (or `restart`
/// if a start did not bring the socket back), at most every 30 s. An
/// untrusted endpoint is permanent: it stops the receiver with a plain log.
async fn wait_for_native(args: &Args, binding: &Binding) -> Result<NativeTransport, String> {
    let codex = args.codex_bin.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin/codex")
    });
    let mut delay = Duration::from_millis(200);
    let mut last_action: Option<(Instant, &str)> = None;
    loop {
        unchanged(args, binding)?;
        let reason = match connect_native(binding).await {
            Ok(native) => {
                unchanged(args, binding)?;
                return Ok(native);
            }
            Err(reason) if !args.serve || reason == "native_endpoint_untrusted" => {
                return Err(reason);
            }
            Err(reason) => reason,
        };
        if reason != "native_endpoint_unavailable" {
            println!(
                "{}",
                json!({"state":"waiting_native_endpoint","reason":reason})
            );
        } else if last_action.is_none_or(|(t, _)| t.elapsed() >= Duration::from_secs(30)) {
            let action = match last_action {
                Some((_, "start")) if std::fs::metadata(&binding.endpoint).is_err() => "restart",
                _ => "start",
            };
            let command = tokio::process::Command::new(&codex)
                .args(["app-server", "daemon", action])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status();
            let status = tokio::time::timeout(Duration::from_secs(60), command)
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|s| s.code());
            println!(
                "{}",
                json!({"state":"waiting_native_endpoint","reason":reason,"daemon_action":action,"daemon_exit":status})
            );
            last_action = Some((Instant::now(), action));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}
fn error_report(reason: &str) -> Value {
    json!({"state":"blocked_or_uncertain","reason":reason,"automatic_resubmit":false})
}
async fn execute(args: Args) -> Result<(), String> {
    execute_with_transport(args, None).await
}
async fn execute_with_transport(
    args: Args,
    transport: Option<Box<dyn Transport>>,
) -> Result<(), String> {
    let binding = load_binding(&args.binding).map_err(str::to_owned)?;
    if args.set_unattended {
        let mut native = connect_native(&binding).await?;
        pigeonmq::adapters::codex::owner::attach_saved(&mut native, &binding).await?;
        pigeonmq::adapters::codex::owner::set_unattended(&mut native, &binding).await?;
        println!(
            "{}",
            json!({"state":"unattended_settings_applied","thread_id":binding.thread_id,
                "approvalPolicy":"never","sandbox":"workspaceWrite","writableRoots":[binding.cwd]})
        );
        return Ok(());
    }
    if !args.run {
        println!(
            "{}",
            json!({"state":"configuration_valid","receiver_started":false})
        );
        return Ok(());
    }
    let state = args.state.as_deref().ok_or("receiver_state_required")?;
    let token_file = args
        .token_file
        .clone()
        .ok_or("receiver_token_file_required")?;
    let transport = match transport {
        Some(transport) => transport,
        None => Box::new(Client::new(args.hub_port, token_file).map_err(str::to_owned)?),
    };
    let mut receiver = Receiver::open(state, binding.clone(), transport).map_err(str::to_owned)?;
    if receiver.has_pending() {
        // A turn interrupted by a stop has an unknown outcome: set it aside.
        match receiver.pending_completed() {
            true => match receiver.complete() {
                Ok(_) => println!("{}", json!({"state":"recovered_atomic_completion"})),
                Err(reason) => end_pending(&mut receiver, reason, false)?,
            },
            false => end_pending(&mut receiver, "receiver_restarted_during_turn", false)?,
        }
        if !args.serve {
            return Ok(());
        }
    }
    let mut native = wait_for_native(&args, &binding).await?;
    if args.attach_saved {
        pigeonmq::adapters::codex::owner::attach_saved(&mut native, &binding).await?;
    }
    let mut reconciled: Option<Instant> = None;
    loop {
        unchanged(&args, &binding)?;
        if let Err(reason) = preflight(&mut native, &binding).await {
            // A thread that is not ours is permanent, not a reason to reconnect.
            if !args.serve || receiver.has_pending() || reason == "codex_native_binding_mismatch" {
                return Err(reason);
            }
            // Idle connection lost (daemon restarted or socket wiped): reconnect.
            println!("{}", json!({"state":"reconnecting","reason":reason}));
            native = wait_for_native(&args, &binding).await?;
            if args.attach_saved {
                pigeonmq::adapters::codex::owner::attach_saved(&mut native, &binding).await?;
            }
            continue;
        }
        // Parked unknown outcomes: look them up in the thread every 5 minutes.
        if reconciled.is_none_or(|t| t.elapsed() >= Duration::from_secs(300)) {
            reconcile_parked(&args, &mut receiver, &mut native, &binding).await;
            reconciled = Some(Instant::now());
        }
        // Native inspection awaits I/O; recheck the approved binding before claim.
        unchanged(&args, &binding)?;
        let params = match receiver.claim() {
            Ok(Some(params)) => params,
            Ok(None) if !args.serve => {
                println!("{}", json!({"state":"empty"}));
                return Ok(());
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(reason) if receiver.has_pending() => {
                // A redelivery means an earlier attempt's outcome is unknown.
                let confirmed = reason != "redelivery_requires_reconciliation";
                end_pending(&mut receiver, reason, confirmed)?;
                continue;
            }
            Err("receiver_api_outcome_uncertain") if args.serve => {
                // No native input exists without a saved pending journal.
                // A lost claim response may leave a lease for reconciliation.
                println!(
                    "{}",
                    json!({"state":"waiting_hub","reason":"receiver_api_outcome_uncertain"})
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(reason) => return Err(reason.into()),
        };
        if let Err((reason, confirmed)) =
            handle(&args, &binding, &mut native, &mut receiver, params).await
        {
            if !receiver.has_pending() {
                return Err(reason);
            }
            // One message never stops the receiver. A dropped connection fails
            // the next preflight and is reconnected.
            end_pending(&mut receiver, &reason, confirmed)?;
            if reason == "codex_binding_changed" {
                return Err(reason);
            }
        }
        if !args.serve {
            return Ok(());
        }
    }
}
/// One message as one turn in the bound thread, then the reply.
/// `Err((reason, confirmed))`: `confirmed` only when Codex never received the
/// input or itself reported the turn as failed; otherwise the outcome is unknown.
async fn handle(
    args: &Args,
    binding: &Binding,
    native: &mut NativeTransport,
    receiver: &mut Receiver,
    params: Value,
) -> Result<(), (String, bool)> {
    let unknown = |e: String| (e, false);
    unchanged(args, binding).map_err(|e| (e, true))?;
    let admission_deadline = receiver
        .native_deadline(Duration::from_secs(10))
        .map_err(|e| (e.to_owned(), true))?;
    let response = native
        .request("thread/queue/add", params.clone(), admission_deadline)
        .await
        .map_err(|e| {
            let confirmed = matches!(e, TransportError::Remote { .. });
            (e.to_string(), confirmed)
        })?;
    receiver
        .admitted(&params, &response)
        .map_err(|e| unknown(e.into()))?;
    // A turn may run for hours: wait in 30 s slices and renew the claim lease.
    loop {
        receiver.renew_lease().map_err(|e| unknown(e.into()))?;
        let wait = Instant::now() + Duration::from_secs(30);
        let notice = match native.next_notification(wait).await {
            Err(TransportError::Timeout) => continue,
            other => other.map_err(|e| unknown(e.to_string()))?,
        };
        if let Some(id) = notice.get("id") {
            // approvalPolicy is "never"; anything still asking is declined.
            let answer = match notice["method"].as_str() {
                Some("mcpServer/elicitation/request") => json!({"action":"decline"}),
                _ => json!({"decision":"decline"}),
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            native
                .respond(id.clone(), answer, deadline)
                .await
                .map_err(|e| unknown(e.to_string()))?;
        }
        unchanged(args, binding).map_err(unknown)?;
        match receiver.notification(&notice) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => return Err((e.into(), e == "native_turn_failed")),
        }
    }
    let completion = receiver.complete().map_err(|e| unknown(e.into()))?;
    println!(
        "{}",
        json!({"state":if completion.envelope.recipient == "codex-local" { "terminal_consumed" } else { "replied" },"native_final_verified":true,"tunnel_verified":false})
    );
    Ok(())
}
/// Settles `.unresolved-*` journals from the bound thread. Never resubmits.
async fn reconcile_parked(
    args: &Args,
    receiver: &mut Receiver,
    native: &mut NativeTransport,
    binding: &Binding,
) {
    for (path, client, _) in receiver.unresolved() {
        let mut scan = match receiver.reconciliation_scan(&path) {
            Ok(scan) => scan,
            Err(reason) => {
                println!(
                    "{}",
                    json!({"state":"reconcile_failed","client_id":client,"reason":reason})
                );
                continue;
            }
        };
        let result = pigeonmq::adapters::codex::owner::find_submission(
            native,
            binding,
            &client,
            &mut scan,
            || {
                unchanged(args, binding)?;
                receiver
                    .reconciliation_scan(&path)
                    .map(|_| ())
                    .map_err(str::to_owned)
            },
        )
        .await;
        if let Err(reason) = unchanged(args, binding).and_then(|_| {
            receiver
                .save_reconciliation_scan(&path, &scan)
                .map_err(str::to_owned)
        }) {
            println!(
                "{}",
                json!({"state":"reconcile_failed","client_id":client,"reason":reason})
            );
            return;
        }
        let found = match result {
            Ok(found) => found,
            Err(reason) => {
                println!(
                    "{}",
                    json!({"state":"reconcile_lookup_failed","client_id":client,"reason":reason})
                );
                return;
            }
        };
        let state = match receiver.reconcile(&path, found) {
            Ok(Some(outcome)) => {
                tracing::info!(target: "pigeonmq", component = "codex_receiver",
                    event = "reconciliation", outcome = "settled");
                json!({"state":"reconciled","client_id":client,"outcome":outcome})
            }
            Ok(None) => json!({"state":"still_unresolved","client_id":client,"journal":path}),
            Err(reason) => json!({"state":"reconcile_failed","client_id":client,"reason":reason}),
        };
        println!("{state}");
    }
}
/// Confirmed failures get a failure reply; unknown outcomes are set aside
/// unACKed for reconciliation. Either way the receiver goes on.
fn end_pending(receiver: &mut Receiver, reason: &str, confirmed: bool) -> Result<(), String> {
    if !confirmed {
        let (id, sender) = receiver.pending_ids();
        let kept = receiver.set_aside(reason).map_err(str::to_owned)?;
        tracing::warn!(target: "pigeonmq", component = "codex_receiver",
            event = "message_processing", outcome = "deferred",
            reason = "native_outcome_unconfirmed", original_acknowledged = false,
            automatic_resubmit = false);
        println!(
            "{}",
            json!({"state":"message_unresolved","message_id":id,"sender":sender,"reason":reason,"journal":kept,"acked":false,"automatic_resubmit":false})
        );
        return Ok(());
    }
    fail_pending(receiver, reason)
}
fn fail_pending(receiver: &mut Receiver, reason: &str) -> Result<(), String> {
    let (id, sender, stored) = receiver.fail(reason).map_err(str::to_owned)?;
    tracing::warn!(target: "pigeonmq", component = "codex_receiver",
        event = "message_processing", outcome = "failed", reason = "processing_failed",
        failure_reply_stored = stored, automatic_resubmit = false);
    println!(
        "{}",
        json!({"state":"message_failed","message_id":id,"sender":sender,"reason":reason,"failure_reply_stored":stored,"automatic_resubmit":false})
    );
    Ok(())
}
fn main() {
    pigeonmq::logging::init();
    tracing::info!(target: "pigeonmq", component = "codex_receiver",
        event = "process_started", outcome = "running");
    receiver_main(Args::parse());
}
#[tokio::main(flavor = "current_thread")]
async fn receiver_main(args: Args) {
    let result = tokio::select! {r=execute(args)=>r,_=tokio::signal::ctrl_c()=>Err("stopped_pending_work_requires_reconciliation".into())};
    if let Err(reason) = result {
        tracing::error!(target: "pigeonmq", component = "codex_receiver",
            event = "process_stopped", outcome = "failed", reason = "runtime_check_failed");
        println!("{}", error_report(&reason));
        std::process::exit(2)
    }
    tracing::info!(target: "pigeonmq", component = "codex_receiver",
        event = "process_stopped", outcome = "completed");
}
#[cfg(test)]
mod tests {
    use super::*;
    struct ClaimFailure {
        inner: receiver_support::LocalTransport,
        failure: std::sync::Mutex<Option<&'static str>>,
    }
    impl Transport for ClaimFailure {
        fn call(&self, request: pigeonmq::receiver_api::Request) -> Result<Value, &'static str> {
            if matches!(request.action, pigeonmq::receiver_api::Action::Claim { .. })
                && let Some(reason) = self.failure.lock().unwrap().take()
            {
                return Err(reason);
            }
            self.inner.call(request)
        }
    }
    async fn execute(args: Args) -> Result<(), String> {
        execute_with_claim_failure(args, None).await
    }
    async fn execute_with_claim_failure(
        args: Args,
        failure: Option<&'static str>,
    ) -> Result<(), String> {
        let binding = load_binding(&args.binding).map_err(str::to_owned)?;
        let db = args
            .state
            .as_deref()
            .unwrap()
            .parent()
            .unwrap()
            .join("queue.db");
        execute_with_transport(
            args,
            Some(Box::new(ClaimFailure {
                inner: receiver_support::LocalTransport {
                    db,
                    registration: binding.registration,
                },
                failure: std::sync::Mutex::new(failure),
            })),
        )
        .await
    }
    #[test]
    fn default_hub_port_matches_cloud_runner() {
        let args = Args::parse_from([
            "pigeonmq-codex-receiver",
            "--state",
            "/state",
            "--token-file",
            "/token",
            "--binding",
            "/binding",
        ]);
        assert_eq!(args.hub_port, 54843);
    }
    #[tokio::test]
    async fn synthetic_native_owner_roundtrip_uses_existing_queue_atomically() {
        roundtrip(false, false).await;
    }
    #[tokio::test]
    async fn setup_attachment_precedes_claim_and_uses_same_owner() {
        roundtrip(true, false).await;
    }
    #[tokio::test]
    async fn terminal_native_processing_acks_without_another_reply() {
        roundtrip(false, true).await;
    }
    async fn roundtrip(attach_saved: bool, terminal: bool) {
        use futures_util::{SinkExt, StreamExt};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("codex-e2e-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        let original = pigeonmq::Queue::open(&db)
            .unwrap()
            .send(pigeonmq::Envelope {
                sender: if terminal { "codex-local" } else { "dot" }.into(),
                recipient: if terminal { "dot" } else { "codex-local" }.into(),
                body: "question".into(),
                correlation_id: Some("correlation".into()),
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        if terminal {
            pigeonmq::Queue::open_existing(&db)
                .unwrap()
                .send(pigeonmq::Envelope {
                    sender: "dot".into(),
                    recipient: "codex-local".into(),
                    body: "answer".into(),
                    correlation_id: Some("correlation".into()),
                    in_reply_to: Some(original.clone()),
                    idempotency_key: None,
                })
                .unwrap();
        }
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        let registration = dir.path().join("registration.json");
        std::fs::write(
            &registration,
            r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&registration, std::fs::Permissions::from_mode(0o600)).unwrap();
        let endpoint = dir.path().join("native.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        let binding = Binding {
            thread_id: "approved-owner".into(),
            endpoint,
            cwd: dir.path().into(),
            approved_inputs: vec!["question".into()],
            registration,
        };
        let binding_path = dir.path().join("binding.json");
        std::fs::write(&binding_path, serde_json::to_vec(&binding).unwrap()).unwrap();
        std::fs::set_permissions(&binding_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cwd = binding.cwd.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut methods = vec![
                "initialize",
                "initialized",
                "thread/loaded/list",
                "thread/read",
                "thread/queue/add",
            ];
            if attach_saved {
                methods.insert(2, "thread/resume");
            }
            for method in methods {
                let frame = ws.next().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                assert_eq!(req["method"], method);
                let result = match method {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/loaded/list" => json!({"data":["approved-owner"],"nextCursor":null}),
                    "thread/read" | "thread/resume" => {
                        assert_eq!(req["params"]["threadId"], "approved-owner");
                        json!({"thread":{"id":"approved-owner","cwd":cwd,"ephemeral":false,"status":{"type":"idle"}}})
                    }
                    _ => {
                        assert_eq!(req["params"]["threadId"], "approved-owner");
                        // Native events may arrive before admission's response.
                        for notice in [
                            json!({"method":"item/completed","params":{"threadId":"approved-owner","turnId":"turn","item":{"type":"userMessage","clientId":req["params"]["clientUserMessageId"]}}}),
                            json!({"id":"srv-1","method":"item/commandExecution/requestApproval","params":{"threadId":"approved-owner","turnId":"turn"}}),
                            json!({"method":"item/completed","params":{"threadId":"approved-owner","turnId":"turn","item":{"type":"agentMessage","phase":"final_answer","id":"answer","text":"final"}}}),
                            json!({"method":"turn/completed","params":{"threadId":"approved-owner","turn":{"id":"turn","status":"completed","error":null}}}),
                        ] {
                            ws.send(notice.to_string().into()).await.unwrap();
                        }
                        json!({"queuedSubmission":{"id":"admission","clientUserMessageId":req["params"]["clientUserMessageId"],"input":req["params"]["input"]}})
                    }
                };
                ws.send(json!({"id":req["id"],"result":result}).to_string().into())
                    .await
                    .unwrap();
            }
            // The approval request was declined, not left waiting for a human.
            let answer: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(
                answer,
                json!({"id":"srv-1","result":{"decision":"decline"}})
            );
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            execute(Args {
                state: Some(db.with_extension("receiver-state")),
                hub_port: 54743,
                token_file: Some(db.with_extension("token")),
                binding: binding_path,
                run: true,
                serve: false,
                attach_saved,
                codex_bin: Some("/usr/bin/false".into()),
                set_unattended: false,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        let connection = rusqlite::Connection::open(&db).unwrap();
        let counts: (i64, i64) = connection
            .query_row(
                "SELECT count(*), count(acknowledged_at_ms) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (2, 1));
        if terminal {
            let consumed: bool = connection
                .query_row(
                    "SELECT acknowledged_at_ms IS NOT NULL FROM messages WHERE recipient='codex-local'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(consumed);
            return;
        }
        let mut queue = pigeonmq::Queue::open_existing(&db).unwrap();
        let reply = queue
            .receive("dot", Duration::from_secs(30))
            .unwrap()
            .unwrap();
        assert_eq!(
            reply.message.envelope.in_reply_to.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(
            reply.message.envelope.correlation_id.as_deref(),
            Some("correlation")
        );
        assert_eq!(reply.message.envelope.body, "final");
    }
    fn isolated_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Binding) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("codex-idle-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue.db");
        drop(pigeonmq::Queue::open(&db).unwrap());
        let registration = dir.path().join("registration.json");
        std::fs::write(&registration, r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#).unwrap();
        let binding = Binding {
            thread_id: "selected-owner".into(),
            endpoint: dir.path().join("native.sock"),
            cwd: dir.path().into(),
            approved_inputs: vec!["late question".into()],
            registration,
        };
        let path = dir.path().join("binding.json");
        std::fs::write(&path, serde_json::to_vec(&binding).unwrap()).unwrap();
        for p in [&db, &path, &binding.registration] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (dir, db, path, binding)
    }

    #[tokio::test]
    async fn no_run_validates_configuration_without_native_or_queue_access() {
        let (_dir, db, path, _binding) = isolated_fixture();
        let before = std::fs::read(&db).unwrap();
        execute(Args {
            state: Some(db.with_extension("receiver-state")),
            hub_port: 54743,
            token_file: Some(db.with_extension("token")),
            binding: path,
            run: false,
            serve: false,
            attach_saved: false,
            codex_bin: Some("/usr/bin/false".into()),
            set_unattended: false,
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert!(
            !db.with_extension("receiver-state")
                .join("codex-local-receiver.lock")
                .exists()
        );
        assert!(
            !db.with_extension("receiver-state")
                .join("codex-local-receiver.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn binding_changed_during_native_preflight_stops_before_claim() {
        native_driver(false, true, None).await;
    }

    #[tokio::test]
    async fn native_check_does_not_open_the_queue_or_admit_input() {
        native_driver(true, false, None).await;
    }

    #[tokio::test]
    async fn serving_receiver_admits_late_arrival_and_completes_without_manual_read() {
        native_driver(false, false, None).await;
    }

    #[tokio::test]
    async fn serving_receiver_recovers_transient_hub_failure_without_duplicate_admission() {
        native_driver(false, false, Some("receiver_api_outcome_uncertain")).await;
    }

    #[tokio::test]
    async fn serving_receiver_stops_on_hub_auth_failure_without_native_admission() {
        for reason in ["receiver_api_unauthorized", "receiver_api_forbidden"] {
            native_driver(false, false, Some(reason)).await;
        }
    }

    async fn native_driver(
        check_only: bool,
        change_binding: bool,
        claim_failure: Option<&'static str>,
    ) {
        use futures_util::{SinkExt, StreamExt};
        let (_dir, db, path, binding) = isolated_fixture();
        let before = std::fs::read(&db).unwrap();
        let listener = tokio::net::UnixListener::bind(&binding.endpoint).unwrap();
        let server_db = db.clone();
        let server_binding = path.clone();
        let inspected_binding = binding.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut reads = 0;
            let mut admissions = 0;
            let mut request_id = None;
            loop {
                let Some(Ok(frame)) = ws.next().await else {
                    break;
                };
                let req: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/loaded/list" if admissions == 1 => {
                        // Stop the synthetic endpoint only after the real receiver loop
                        // has completed and returned for its next queue pass.
                        let _ = ws.send(json!({"id":req["id"],"error":{"code":-32000,"message":"fixture finished"}}).to_string().into()).await;
                        break;
                    }
                    "thread/loaded/list" => json!({"data":["selected-owner"],"nextCursor":null}),
                    "thread/read" => {
                        assert_eq!(req["params"]["threadId"], "selected-owner");
                        reads += 1;
                        if reads == 2 || (change_binding && reads == 1) {
                            assert!(!check_only);
                            // The first queue poll was empty. A later producer writes
                            // through the existing queue, with no read/wait forwarding.
                            request_id = Some(
                                pigeonmq::Queue::open_existing(&server_db)
                                    .unwrap()
                                    .send(pigeonmq::Envelope {
                                        sender: "dot".into(),
                                        recipient: "codex-local".into(),
                                        body: "late question".into(),
                                        correlation_id: Some("idle-test".into()),
                                        in_reply_to: None,
                                        idempotency_key: Some("idle-once".into()),
                                    })
                                    .unwrap()
                                    .message
                                    .id,
                            );
                        }
                        if change_binding {
                            let mut changed = binding.clone();
                            changed.thread_id = "another-owner".into();
                            std::fs::write(&server_binding, serde_json::to_vec(&changed).unwrap())
                                .unwrap();
                        }
                        json!({"thread":{"id":"selected-owner","cwd":binding.cwd,"ephemeral":false,"status":{"type":"idle"}}})
                    }
                    "thread/queue/add" => {
                        assert!(!check_only);
                        assert_eq!(reads, 2);
                        assert_eq!(req["params"]["threadId"], "selected-owner");
                        admissions += 1;
                        assert_eq!(admissions, 1);
                        for notice in [
                            json!({"method":"item/completed","params":{"threadId":"selected-owner","turnId":"late-turn","item":{"type":"userMessage","clientId":req["params"]["clientUserMessageId"]}}}),
                            json!({"method":"item/completed","params":{"threadId":"selected-owner","turnId":"late-turn","item":{"type":"agentMessage","phase":"final_answer","id":"late-answer","text":"synthetic final"}}}),
                            json!({"method":"turn/completed","params":{"threadId":"selected-owner","turn":{"id":"late-turn","status":"completed","error":null}}}),
                        ] {
                            ws.send(notice.to_string().into()).await.unwrap();
                        }
                        json!({"queuedSubmission":{"id":"late-admission","clientUserMessageId":req["params"]["clientUserMessageId"],"input":req["params"]["input"]}})
                    }
                    method => panic!("unexpected method: {method}"),
                };
                ws.send(json!({"id":req["id"],"result":result}).to_string().into())
                    .await
                    .unwrap();
                if (check_only || change_binding) && reads == 1 {
                    break;
                }
            }
            (request_id, admissions)
        });
        let serve_db = db.clone();
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            if check_only {
                let mut native = connect_native(&inspected_binding).await?;
                preflight(&mut native, &inspected_binding).await
            } else if !change_binding {
                // A serving receiver no longer exits when the daemon goes away;
                // stop it once the reply is stored.
                tokio::select! {
                    r = execute_with_claim_failure(Args {
                        state: Some(serve_db.with_extension("receiver-state")),
                        hub_port: 54743,
                        token_file: Some(serve_db.with_extension("token")),
                        binding: path.clone(),
                        run: true,
                        serve: true,
                        attach_saved: false,
                        codex_bin: Some("/usr/bin/false".into()),
                set_unattended: false,
                    }, claim_failure) => r,
                    _ = async {
                        loop {
                            let n: i64 = rusqlite::Connection::open(&serve_db).unwrap()
                                .query_row("SELECT count(*) FROM messages WHERE sender='codex-local'", [], |r| r.get(0)).unwrap();
                            if n == 1 { break; }
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                    } => Err("stopped_after_reply".into()),
                }
            } else {
                execute(Args {
                    state: Some(db.with_extension("receiver-state")),
            hub_port: 54743,
            token_file: Some(db.with_extension("token")),
                    binding: path,
                    run: true,
                    serve: true,
                    attach_saved: false,
                    codex_bin: Some("/usr/bin/false".into()),
                set_unattended: false,
                })
                .await
            }
        })
        .await
        .unwrap();
        let (request_id, admissions) = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        if check_only {
            result.unwrap();
            assert_eq!(admissions, 0);
            assert_eq!(std::fs::read(&db).unwrap(), before);
            assert!(
                !db.with_extension("receiver-state")
                    .join("codex-local-receiver.lock")
                    .exists()
            );
            return;
        }
        if change_binding {
            assert_eq!(result.unwrap_err(), "codex_binding_changed");
            assert_eq!(admissions, 0);
            let conn = rusqlite::Connection::open(&db).unwrap();
            let untouched:bool = conn.query_row("SELECT attempts=0 AND receipt IS NULL AND lease_until_ms IS NULL AND acknowledged_at_ms IS NULL FROM messages",[],|r|r.get(0)).unwrap();
            assert!(untouched);
            assert!(
                !db.with_extension("receiver-state")
                    .join("codex-local-receiver.json")
                    .exists()
            );
            return;
        }
        if let Some(reason @ ("receiver_api_unauthorized" | "receiver_api_forbidden")) =
            claim_failure
        {
            assert_eq!(result.unwrap_err(), reason);
            assert_eq!(admissions, 0);
            let count: i64 = rusqlite::Connection::open(&db)
                .unwrap()
                .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0);
            return;
        }
        assert_eq!(result.unwrap_err(), "stopped_after_reply");
        assert_eq!(admissions, 1);
        let conn = rusqlite::Connection::open(&db).unwrap();
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT count(*), count(acknowledged_at_ms) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (2, 1));
        let reply: (String, String, String) = conn
            .query_row(
                "SELECT in_reply_to,correlation_id,body FROM messages WHERE sender='codex-local'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            reply,
            (
                request_id.unwrap(),
                "idle-test".into(),
                "synthetic final".into()
            )
        );
        assert!(
            !db.with_extension("receiver-state")
                .join("codex-local-receiver.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn endpoint_missing_then_recovered_after_daemon_start() {
        use futures_util::{SinkExt, StreamExt};
        use std::os::unix::fs::PermissionsExt;
        let (dir, db, path, binding) = isolated_fixture();
        // Stable symlink whose /tmp target was wiped (reboot or tmp cleanup).
        let real = dir.path().join("real.sock");
        std::os::unix::fs::symlink(&real, &binding.endpoint).unwrap();
        let marker = dir.path().join("daemon-actions");
        let fake = dir.path().join("fake-codex");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = pigeonmq::Queue::open_existing(&db)
            .unwrap()
            .send(pigeonmq::Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body: "after reboot".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        let (marker2, cwd) = (marker.clone(), binding.cwd.clone());
        let server = tokio::spawn(async move {
            while !marker2.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let listener = tokio::net::UnixListener::bind(&real).unwrap();
            let mut ws = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let req: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "initialized" => continue,
                    "thread/loaded/list" => json!({"data":["selected-owner"],"nextCursor":null}),
                    "thread/read" => {
                        json!({"thread":{"id":"selected-owner","cwd":cwd,"ephemeral":false,"status":{"type":"idle"}}})
                    }
                    "thread/queue/add" => {
                        let c = req["params"]["clientUserMessageId"].clone();
                        for n in [
                            json!({"method":"item/completed","params":{"threadId":"selected-owner","turnId":"t","item":{"type":"userMessage","clientId":c}}}),
                            json!({"method":"item/completed","params":{"threadId":"selected-owner","turnId":"t","item":{"type":"agentMessage","phase":"final_answer","id":"a","text":"recovered"}}}),
                            json!({"method":"turn/completed","params":{"threadId":"selected-owner","turn":{"id":"t","status":"completed","error":null}}}),
                        ] {
                            ws.send(n.to_string().into()).await.unwrap();
                        }
                        json!({"queuedSubmission":{"id":"adm","clientUserMessageId":c,"input":req["params"]["input"]}})
                    }
                    _ => json!({}),
                };
                if ws
                    .send(json!({"id":req["id"],"result":result}).to_string().into())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let args = Args {
            state: Some(db.with_extension("receiver-state")),
            hub_port: 54743,
            token_file: Some(db.with_extension("token")),
            binding: path,
            run: true,
            serve: true,
            attach_saved: false,
            codex_bin: Some(fake),
            set_unattended: false,
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                r = execute(args) => panic!("serve must keep running: {r:?}"),
                _ = async {
                    loop {
                        let n: i64 = rusqlite::Connection::open(&db).unwrap()
                            .query_row("SELECT count(*) FROM messages WHERE sender='codex-local' AND in_reply_to=?1 AND body='recovered'", [&id], |r| r.get(0)).unwrap();
                        if n == 1 { break; }
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                } => {}
            }
        }).await.unwrap();
        server.abort();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().lines().next(),
            Some("app-server daemon start")
        );
    }

    #[tokio::test]
    async fn set_unattended_is_a_separate_one_time_step() {
        use futures_util::{SinkExt, StreamExt};
        let (_dir, db, path, binding) = isolated_fixture();
        let listener = tokio::net::UnixListener::bind(&binding.endpoint).unwrap();
        let cwd = binding.cwd.clone();
        let server = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            let mut seen = vec![];
            while let Some(Ok(frame)) = ws.next().await {
                let req: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                seen.push(req.clone());
                let result = match req["method"].as_str().unwrap() {
                    "initialized" => continue,
                    "thread/resume" => {
                        json!({"thread":{"id":"selected-owner","cwd":cwd,"ephemeral":false,"status":{"type":"idle"}}})
                    }
                    _ => json!({}),
                };
                let _ = ws
                    .send(json!({"id":req["id"],"result":result}).to_string().into())
                    .await;
            }
            seen
        });
        let args = Args {
            state: Some(db.with_extension("receiver-state")),
            hub_port: 54743,
            token_file: Some(db.with_extension("token")),
            binding: path,
            run: false,
            serve: false,
            attach_saved: false,
            codex_bin: Some("/usr/bin/false".into()),
            set_unattended: true,
        };
        execute(args).await.unwrap();
        let seen = server.await.unwrap();
        let methods: Vec<_> = seen.iter().filter_map(|v| v["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "thread/resume",
                "thread/settings/update"
            ]
        );
        let p = &seen[3]["params"];
        assert_eq!(p["approvalPolicy"], "never");
        assert_eq!(p["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(p["sandboxPolicy"]["writableRoots"], json!([binding.cwd]));
        assert!(
            !db.with_extension("receiver-state")
                .join("codex-local-receiver.lock")
                .exists()
        );
    }
    #[tokio::test]
    async fn reconciliation_resumes_past_two_thousand_turns_after_restart_without_admission() {
        use futures_util::{SinkExt, StreamExt};
        use std::os::unix::fs::PermissionsExt;
        let (_dir, db, path, binding) = isolated_fixture();
        let id = pigeonmq::Queue::open_existing(&db)
            .unwrap()
            .send(pigeonmq::Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body: "ordinary work".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        let journal = {
            let mut receiver = receiver_support::open(&db, binding.clone()).unwrap();
            let params = receiver.claim().unwrap().unwrap();
            receiver.admitted(&params, &json!({"queuedSubmission": {
                "id":"original-admission", "clientUserMessageId":params["clientUserMessageId"],
                "input":params["input"]
            }})).unwrap();
            receiver.set_aside("native_transport_disconnected").unwrap()
        };
        let listener = tokio::net::UnixListener::bind(&binding.endpoint).unwrap();
        let client = format!("pigeon-{id}");
        let server = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap();
            let (mut queue_pages, mut history_pages) = (0, 0);
            while let Some(Ok(frame)) = ws.next().await {
                let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/queue/list" => {
                        assert_eq!(request["params"]["threadId"], "selected-owner");
                        queue_pages += 1;
                        json!({"data":[],"nextCursor":null})
                    }
                    "thread/turns/list" => {
                        assert_eq!(request["params"]["threadId"], "selected-owner");
                        assert_eq!(request["params"]["limit"], 50);
                        history_pages += 1;
                        let start = request["params"]["cursor"]
                            .as_str()
                            .map(|cursor| cursor.parse::<usize>().unwrap())
                            .unwrap_or(0);
                        let end = (start + 50).min(2101);
                        let data: Vec<_> = (start..end).map(|index| {
                            let matching = index == 2100;
                            json!({"id":format!("turn-{index}"),"status":"completed","items":[
                                {"type":"userMessage","clientId":if matching {client.clone()} else {format!("other-{index}")}},
                                {"type":"agentMessage","phase":"final_answer","text":if matching {"recovered ancient answer"} else {"unrelated"}}
                            ]})
                        }).collect();
                        json!({"data":data,"nextCursor":(end < 2101).then(|| end.to_string())})
                    }
                    method => panic!("reconciliation must not admit native input: {method}"),
                };
                ws.send(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                )
                .await
                .unwrap();
            }
            (queue_pages, history_pages)
        });
        let mut native = connect_native(&binding).await.unwrap();
        let args = Args {
            state: Some(db.with_extension("receiver-state")),
            hub_port: 54843,
            token_file: Some(db.with_extension("token")),
            binding: path,
            run: true,
            serve: true,
            attach_saved: false,
            codex_bin: None,
            set_unattended: false,
        };
        {
            let mut receiver = receiver_support::open(&db, binding.clone()).unwrap();
            reconcile_parked(&args, &mut receiver, &mut native, &binding).await;
            assert_eq!(receiver.unresolved().len(), 1);
            let saved: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            assert!(saved["reconciliation_scan"]["cursor"].as_str().is_some());
            assert_eq!(
                std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut receiver = receiver_support::open(&db, binding.clone()).unwrap();
        reconcile_parked(&args, &mut receiver, &mut native, &binding).await;
        assert!(receiver.unresolved().is_empty());
        reconcile_parked(&args, &mut receiver, &mut native, &binding).await;
        let connection = rusqlite::Connection::open(&db).unwrap();
        let state: (i64, bool, i64, String) = connection.query_row(
            "SELECT m.attempts,m.acknowledged_at_ms IS NOT NULL,(SELECT count(*) FROM messages r WHERE r.in_reply_to=m.id),(SELECT body FROM messages r WHERE r.in_reply_to=m.id) FROM messages m WHERE m.id=?1",
            [&id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap();
        assert_eq!(state, (1, true, 1, "recovered ancient answer".into()));
        drop(native);
        assert_eq!(server.await.unwrap(), (1, 43));
    }

    #[tokio::test]
    async fn interrupted_turn_is_kept_unacked_on_restart() {
        let (_dir, db, path, binding) = isolated_fixture();
        let id = pigeonmq::Queue::open_existing(&db)
            .unwrap()
            .send(pigeonmq::Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body: "late question".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id;
        drop({
            let mut r = receiver_support::open(&db, binding).unwrap();
            r.claim().unwrap().unwrap();
            r
        });
        let args = Args {
            state: Some(db.with_extension("receiver-state")),
            hub_port: 54743,
            token_file: Some(db.with_extension("token")),
            binding: path,
            run: true,
            serve: false,
            attach_saved: false,
            codex_bin: Some("/usr/bin/false".into()),
            set_unattended: false,
        };
        execute(args).await.unwrap();
        // Unknown outcome: no reply, no ACK, journal kept for reconciliation.
        let (replies, acked): (i64, bool) = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT (SELECT count(*) FROM messages WHERE in_reply_to=?1), acknowledged_at_ms IS NOT NULL FROM messages WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((replies, acked), (0, false));
        let journal = db
            .with_extension("receiver-state")
            .join(format!("codex-local-receiver.json.unresolved-{id}-1"));
        assert!(journal.exists());
    }
    #[test]
    fn owner_metadata_cannot_select_another_session() {
        let b = Binding {
            thread_id: "saved".into(),
            endpoint: "/native".into(),
            cwd: "/work".into(),
            approved_inputs: vec!["question".into()],
            registration: "/registration".into(),
        };
        let good = json!({"id":"saved","cwd":"/work","ephemeral":false,"status":{"type":"idle"}});
        assert!(thread_matches(&b, &good));
        for (key, value) in [
            ("id", json!("other")),
            ("cwd", json!("/else")),
            ("ephemeral", json!(true)),
            ("status", json!({"type":"notLoaded"})),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(!thread_matches(&b, &bad));
        }
    }
}
