//! Lease-first Codex receiver. No native owner creation or automatic resubmission.
//! Any registered Pigeon sender may send; Pigeon registration decides who can send.
use crate::{
    Delivery, Message,
    adapters::codex::owner::SubmissionScan,
    receiver_api::{Action, Request},
    receiver_client::Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub thread_id: String,
    pub endpoint: PathBuf,
    pub cwd: PathBuf,
    // FIXME(yagni): dead approved_inputs field - rewrite old bindings once and drop it
    /// Ignored. Kept only so existing binding files still load.
    #[serde(default)]
    pub approved_inputs: Vec<String>,
    pub registration: PathBuf,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        if self.thread_id.trim().is_empty()
            || self.thread_id.len() > 1024
            || !self.endpoint.is_absolute()
            || !self.cwd.is_absolute()
            || !self.registration.is_absolute()
        {
            return Err("invalid_codex_binding");
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid_codex_binding")?
            .len()
            > 65536
        {
            return Err("binding_too_large");
        }
        Ok(())
    }
}
fn now() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock_failed")
        .map(|d| d.as_millis() as i64)
}
fn digest(v: &impl Serialize) -> Result<String> {
    Ok(
        Sha256::digest(serde_json::to_vec(v).map_err(|_| "serialize_failed")?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn private(path: &Path) -> Result<File> {
    crate::private_file::open_private_file(path).map_err(|_| "private_file_required")
}

pub fn load_binding(path: &Path) -> Result<Binding> {
    let mut bytes = Vec::new();
    private(path)?
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "binding_unreadable")?;
    // FIXME(yagni): 64 KiB cap on an owner-private binding file - no stated basis
    if bytes.len() > 65536 {
        return Err("binding_too_large");
    }
    let b: Binding = serde_json::from_slice(&bytes).map_err(|_| "invalid_codex_binding")?;
    b.validate()?;
    Ok(b)
}
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    binding: Binding,
    delivery: Delivery,
    prompt: String,
    admission: Option<String>,
    turn: Option<String>,
    input_seen: bool,
    foreign_input_turns: Vec<String>,
    final_id: Option<String>,
    final_text: Option<String>,
    completed: bool,
    blocked: Option<String>,
    #[serde(default)]
    reconciliation_scan: SubmissionScan,
}
fn read_pending(path: &Path) -> Result<Pending> {
    let mut bytes = Vec::new();
    private(path)?
        .take(1048577)
        .read_to_end(&mut bytes)
        .map_err(|_| "journal_unreadable")?;
    if bytes.len() > 1048576 {
        return Err("journal_too_large");
    }
    serde_json::from_slice(&bytes).map_err(|_| "journal_invalid")
}
fn write_pending(path: &Path, pending: &Pending) -> Result<()> {
    let bytes = serde_json::to_vec(pending).map_err(|_| "journal_invalid")?;
    if bytes.len() > 1048576 {
        return Err("journal_too_large");
    }
    crate::private_file::replace_private(path, &bytes).map_err(|_| "journal_write_failed")
}

fn failure_body(reason: &str) -> String {
    format!(
        "[codex-local receiver] Not completed: {reason}. It was not resubmitted to Codex; send it again if still needed."
    )
}
/// What the bound thread shows for `pigeon-<message>` (looked up, never resubmitted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The turn completed with this final answer.
    Final(String),
    /// Codex ended the turn as failed or interrupted.
    Failed(String),
    /// Neither in the thread's turns nor in its queue.
    NotThere,
    /// Still queued or running, or the lookup was incomplete.
    Unknown,
}
/// Local native proof and one process owner; the hub owns all queue operations.
pub struct Receiver {
    transport: Box<dyn Transport>,
    journal: PathBuf,
    lock: File,
    pub binding: Binding,
    pending: Option<Pending>,
    /// Claim lease. Long turns keep it alive with `renew_lease`.
    pub lease: Duration,
}
impl Drop for Receiver {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Receiver {
    pub fn open(state: &Path, binding: Binding, transport: Box<dyn Transport>) -> Result<Self> {
        binding.validate()?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state)
            .map_err(|_| "receiver_state_unavailable")?;
        let metadata =
            std::fs::symlink_metadata(state).map_err(|_| "receiver_state_unavailable")?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("private_receiver_state_required");
        }
        let lock_path = state.join("codex-local-receiver.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|_| "receiver_lock_failed")?;
        let lm = lock.metadata().map_err(|_| "receiver_lock_failed")?;
        // FIXME(yagni): nlink/uid/0600 checks on an empty receiver lock - flock alone is enough
        if !lm.is_file()
            || lm.nlink() != 1
            || lm.uid() != unsafe { libc::geteuid() }
            || lm.mode() & 0o077 != 0
        {
            return Err("unsafe_receiver_lock");
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("codex_receiver_already_running");
        }
        let journal = state.join("codex-local-receiver.json");
        let pending = if journal.exists() {
            let p = read_pending(&journal)?;
            // Same thread and workspace; an endpoint path change does not matter.
            if p.binding.thread_id != binding.thread_id || p.binding.cwd != binding.cwd {
                return Err("pending_binding_changed");
            }
            Some(p)
        } else {
            None
        };
        Ok(Self {
            transport,
            journal,
            lock,
            binding,
            pending,
            lease: Duration::from_secs(180),
        })
    }
    fn call<T: serde::de::DeserializeOwned>(&self, action: Action) -> Result<T> {
        let value = self.transport.call(Request {
            recipient: "codex-local".into(),
            session_id: self.binding.thread_id.clone(),
            profile_id: "codex-native".into(),
            action,
        })?;
        serde_json::from_value(value).map_err(|_| "invalid_receiver_response")
    }
    fn authorize(&self) -> Result<()> {
        self.call(Action::Authorize {
            message_id: self.pending.as_ref().map(|p| p.delivery.message.id.clone()),
        })
    }
    fn save(&self) -> Result<()> {
        write_pending(&self.journal, self.pending.as_ref().ok_or("no_pending")?)
    }
    pub fn authorize_before_native(&self) -> Result<()> {
        self.authorize()?;
        let p = self.pending.as_ref().ok_or("no_pending")?;
        if p.blocked.is_some() || now()? >= p.delivery.lease_until_ms {
            return Err("pending_requires_reconciliation");
        };
        Ok(())
    }
    /// Native I/O never outlives the currently held queue lease.
    pub fn native_deadline(&self, cap: Duration) -> Result<std::time::Instant> {
        self.authorize_before_native()?;
        let remaining = self
            .pending
            .as_ref()
            .ok_or("no_pending")?
            .delivery
            .lease_until_ms
            - now()?;
        if remaining <= 0 {
            return Err("pending_requires_reconciliation");
        }
        Ok(std::time::Instant::now() + cap.min(Duration::from_millis(remaining as u64)))
    }
    /// Heartbeat for long turns: when less than half the lease is left, extend
    /// the live claim by `lease` and save it.
    pub fn renew_lease(&mut self) -> Result<()> {
        let p = self.pending.as_ref().ok_or("no_pending")?;
        if p.delivery.lease_until_ms - now()? > self.lease.as_millis() as i64 / 2 {
            return Ok(());
        }
        let until = self
            .call(Action::Renew {
                message_id: p.delivery.message.id.clone(),
                receipt: p.delivery.receipt.clone(),
                lease_seconds: self.lease.as_secs(),
            })
            .map_err(|_| "lease_expired_requires_reconciliation")?;
        self.pending.as_mut().unwrap().delivery.lease_until_ms = until;
        self.save()
    }
    /// (message_id, sender) of the pending message, for logs.
    pub fn pending_ids(&self) -> (String, String) {
        self.pending
            .as_ref()
            .map(|p| {
                (
                    p.delivery.message.id.clone(),
                    p.delivery.message.envelope.sender.clone(),
                )
            })
            .unwrap_or_default()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn pending_completed(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.completed)
    }
    /// Save before any native admission. A failed/uncertain request is never sent again.
    pub fn claim(&mut self) -> Result<Option<Value>> {
        if self.pending.is_some() {
            return Err("pending_requires_reconciliation");
        }
        let Some(delivery): Option<Delivery> = self.call(Action::Claim {
            lease_seconds: self.lease.as_secs(),
        })?
        else {
            return Ok(None);
        };
        let e = &delivery.message.envelope;
        let rejection = (delivery.attempt != 1).then_some("redelivery_requires_reconciliation");
        let sender = &e.sender;
        let prompt = if let Some(original) = &e.in_reply_to {
            format!(
                "PigeonMQ reply from `{sender}` to your earlier message {original}. Read it and continue any related work. Do not send a PigeonMQ reply to it. End with a short final answer confirming you read it.\n\n{}",
                e.body
            )
        } else {
            format!(
                "PigeonMQ message from `{sender}`. Treat it as a real request from `{sender}` and do the work it asks in this workspace. Do not send a PigeonMQ reply yourself: your final answer is sent back to `{sender}` as the reply, so make it the complete response.\n\n{}",
                e.body
            )
        };
        self.pending = Some(Pending {
            binding: self.binding.clone(),
            delivery,
            prompt,
            admission: None,
            turn: None,
            input_seen: false,
            foreign_input_turns: Vec::new(),
            final_id: None,
            final_text: None,
            completed: false,
            blocked: rejection.map(str::to_owned),
            reconciliation_scan: SubmissionScan::default(),
        });
        self.save()?;
        if let Some(reason) = rejection {
            return Err(reason);
        }
        self.authorize()?;
        let p = self.pending.as_ref().unwrap();
        let _: crate::runtime_admission::Record = self
            .call(Action::Begin {
                message_id: p.delivery.message.id.clone(),
                receipt: p.delivery.receipt.clone(),
                payload_digest: digest(&p.prompt)?,
            })
            .map_err(|_| "admission_fence_failed")?;
        Ok(Some(
            json!({"threadId":self.binding.thread_id,"clientUserMessageId":format!("pigeon-{}",p.delivery.message.id),
            "input":[{"type":"text","text":p.prompt,"text_elements":[]}]}),
        ))
    }
    pub fn admitted(&mut self, params: &Value, response: &Value) -> Result<()> {
        self.authorize()?;
        let q = &response["queuedSubmission"];
        let id = q["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("invalid_native_admission")?;
        if q["clientUserMessageId"] != params["clientUserMessageId"]
            || q["input"] != params["input"]
        {
            return Err("native_admission_mismatch");
        }
        let p = self.pending.as_mut().ok_or("no_pending")?;
        if p.admission.is_some() || p.blocked.is_some() {
            return Err("admission_already_or_blocked");
        }
        p.admission = Some(id.into());
        self.save()?;
        let p = self.pending.as_ref().unwrap();
        let mut r: crate::runtime_admission::Record = self
            .call(Action::Load {
                message_id: p.delivery.message.id.clone(),
                receipt: p.delivery.receipt.clone(),
            })
            .map_err(|_| "admission_fence_failed")?;
        r.admission_id = Some(id.into());
        r.stage = "admitted".into();
        let _: crate::runtime_admission::Record = self
            .call(Action::Update {
                receipt: p.delivery.receipt.clone(),
                record: r.into(),
            })
            .map_err(|_| "admission_fence_failed")?;
        tracing::info!(
            component = "codex_receiver",
            event = "native_submission_accepted",
            outcome = "accepted",
            message_id = p.delivery.message.id.as_str()
        );
        Ok(())
    }
    fn block(&mut self, reason: &'static str) -> Result<bool> {
        if let Some(p) = self.pending.as_mut() {
            p.blocked = Some(reason.into())
        }
        self.save()?;
        Err(reason)
    }
    /// Ordered notifications from this one trusted native connection only.
    pub fn notification(&mut self, n: &Value) -> Result<bool> {
        self.authorize()?;
        if n.get("id").is_some() {
            // Server requests are answered by the caller (declined); they never block.
            return Ok(false);
        }
        let Some(p) = self.pending.as_ref() else {
            return Err("no_pending");
        };
        if p.blocked.is_some() {
            return Err("pending_requires_reconciliation");
        }
        if p.admission.is_none() {
            return Err("admission_unproven");
        }
        if now()? >= p.delivery.lease_until_ms {
            return self.block("lease_expired_requires_reconciliation");
        }
        let params = &n["params"];
        if params["threadId"] != self.binding.thread_id {
            return Ok(false);
        }
        let method = n["method"].as_str().unwrap_or("");
        let turn = if method == "turn/completed" {
            params["turn"]["id"].as_str()
        } else {
            params["turnId"].as_str()
        };
        let item = &params["item"];
        let input =
            matches!(method, "item/started" | "item/completed") && item["type"] == "userMessage";
        let matching = input && item["clientId"] == format!("pigeon-{}", p.delivery.message.id);
        if input && !matching {
            if let Some(t) = turn {
                if p.turn.as_deref() == Some(t) {
                    return self.block("mixed_user_input");
                }
                if p.foreign_input_turns.len() >= 128 {
                    return self.block("native_event_backlog");
                }
                self.pending
                    .as_mut()
                    .unwrap()
                    .foreign_input_turns
                    .push(t.to_owned());
                self.save()?;
            }
            return Ok(false);
        }
        let p = self.pending.as_ref().unwrap();
        if matching {
            if turn.is_some_and(|t| p.foreign_input_turns.iter().any(|old| old == t)) {
                return self.block("mixed_user_input");
            }
            if turn.is_none_or(str::is_empty) {
                return self.block("invalid_input_turn");
            }
            if p.turn.as_deref().is_some_and(|id| Some(id) != turn) {
                return self.block("multiple_matching_turns");
            }
            self.pending.as_mut().unwrap().turn = turn.map(str::to_owned);
        }
        let p = self.pending.as_mut().unwrap();
        if p.turn.is_none() || p.turn.as_deref() != turn {
            return Ok(false);
        }
        if input {
            if !matching {
                return self.block("mixed_user_input");
            }
            p.input_seen = true;
        } else if method == "item/completed"
            && item["type"] == "agentMessage"
            && item["phase"] == "final_answer"
        {
            let (Some(id), Some(text)) = (item["id"].as_str(), item["text"].as_str()) else {
                return self.block("invalid_final");
            };
            if id.is_empty() || text.trim().is_empty() || text.len() > 16384 {
                return self.block("invalid_final");
            }
            if p.final_id.as_deref().is_some_and(|x| x != id)
                || p.final_text.as_deref().is_some_and(|x| x != text)
            {
                return self.block("ambiguous_final");
            }
            p.final_id = Some(id.into());
            p.final_text = Some(text.into());
        } else if method == "turn/completed" {
            if matches!(
                params["turn"]["status"].as_str(),
                Some("failed" | "interrupted")
            ) {
                tracing::warn!(
                    component = "codex_receiver",
                    event = "native_processing_failed",
                    outcome = if params["turn"]["status"] == "interrupted" {
                        "interrupted"
                    } else {
                        "failed"
                    },
                    message_id = p.delivery.message.id.as_str()
                );
                return self.block("native_turn_failed");
            }
            if params["turn"]["status"] != "completed"
                || !params["turn"]["error"].is_null()
                || !p.input_seen
                || p.final_text.is_none()
            {
                return self.block("native_completion_unproven");
            }
            p.completed = true;
        }
        self.save()?;
        if self.pending.as_ref().unwrap().completed {
            let p = self.pending.as_ref().unwrap();
            let mut r: crate::runtime_admission::Record = self
                .call(Action::Load {
                    message_id: p.delivery.message.id.clone(),
                    receipt: p.delivery.receipt.clone(),
                })
                .map_err(|_| "admission_fence_failed")?;
            let already_recorded = r.stage == "model_completed";
            r.stage = "model_completed".into();
            r.answer_digest = Some(digest(p.final_text.as_ref().ok_or("missing_final")?)?);
            let _: crate::runtime_admission::Record = self
                .call(Action::Update {
                    receipt: p.delivery.receipt.clone(),
                    record: r.into(),
                })
                .map_err(|_| "admission_fence_failed")?;
            tracing::info!(
                component = "codex_receiver",
                event = "native_final_verified",
                outcome = if already_recorded {
                    "already_verified"
                } else {
                    "verified"
                },
                message_id = p.delivery.message.id.as_str()
            );
        }
        Ok(self.pending.as_ref().unwrap().completed)
    }
    /// Unknown native outcome: never ACK, never resubmit. The journal is kept
    /// (renamed to `.unresolved-<message>-<attempt>`) for reconciliation by
    /// clientUserMessageId `pigeon-<message>` in the bound thread, and the
    /// claim is parked for 7 days so it is not redelivered meanwhile. The
    /// receiver can then take other messages.
    pub fn set_aside(&mut self, reason: &str) -> Result<PathBuf> {
        let p = self.pending.as_ref().ok_or("no_pending")?;
        let (id, receipt) = (p.delivery.message.id.clone(), p.delivery.receipt.clone());
        let parked = self
            .call::<i64>(Action::Renew {
                message_id: id.clone(),
                receipt,
                lease_seconds: 7 * 86400,
            })
            .ok();
        let p = self.pending.as_mut().unwrap();
        p.blocked = Some(reason.into());
        if let Some(until) = parked {
            p.delivery.lease_until_ms = until;
        }
        let aside = PathBuf::from(format!(
            "{}.unresolved-{id}-{}",
            self.journal.display(),
            p.delivery.attempt
        ));
        self.save()?;
        std::fs::rename(&self.journal, &aside).map_err(|_| "journal_set_aside_failed")?;
        self.pending = None;
        Ok(aside)
    }
    /// Confirmed failure only (rejected before native admission, or Codex
    /// ended the turn as failed): a request gets a failure reply to its sender
    /// and is ACKed; a terminal is only ACKed. If the queue refuses, the work
    /// is set aside instead and the journal is kept.
    /// Returns (message_id, sender, failure reply stored).
    pub fn fail(&mut self, reason: &str) -> Result<(String, String, bool)> {
        let p = self.pending.as_ref().ok_or("no_pending")?;
        let (id, e) = (&p.delivery.message.id, &p.delivery.message.envelope);
        let stored = self
            .call::<Message>(Action::Complete {
                message_id: id.clone(),
                receipt: p.delivery.receipt.clone(),
                body: e.in_reply_to.is_none().then(|| failure_body(reason)),
                failure: true,
            })
            .is_ok();
        let (id, sender) = (id.clone(), e.sender.clone());
        if !stored {
            self.set_aside(reason)?;
            return Ok((id, sender, false));
        }
        self.pending = None;
        match std::fs::remove_file(&self.journal) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err("failed_journal_cleanup_failed");
            }
            _ => {}
        }
        Ok((id, sender, true))
    }
    /// Set-aside journals waiting for reconciliation:
    /// (journal, clientUserMessageId, native admission was confirmed).
    pub fn unresolved(&self) -> Vec<(PathBuf, String, bool)> {
        let (Some(dir), Some(name)) = (self.journal.parent(), self.journal.file_name()) else {
            return Vec::new();
        };
        let prefix = format!("{}.unresolved-", name.to_string_lossy());
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .filter_map(|path| {
                let p = read_pending(&path).ok()?;
                let client = format!("pigeon-{}", p.delivery.message.id);
                Some((path, client, p.admission.is_some()))
            })
            .collect();
        found.sort();
        found
    }
    fn check_unresolved(&self, pending: &Pending) -> Result<()> {
        if pending.binding.thread_id != self.binding.thread_id
            || pending.binding.cwd != self.binding.cwd
        {
            return Err("pending_binding_changed");
        }
        let _: Value = self.call(Action::State {
            message_id: pending.delivery.message.id.clone(),
            receipt: pending.delivery.receipt.clone(),
        })?;
        Ok(())
    }
    pub fn reconciliation_scan(&self, path: &Path) -> Result<SubmissionScan> {
        let pending = read_pending(path)?;
        self.check_unresolved(&pending)?;
        Ok(pending.reconciliation_scan)
    }
    pub fn save_reconciliation_scan(&self, path: &Path, scan: &SubmissionScan) -> Result<()> {
        let mut pending = read_pending(path)?;
        self.check_unresolved(&pending)?;
        pending.reconciliation_scan = scan.clone();
        write_pending(path, &pending)
    }
    /// Settles one set-aside journal from what the bound thread shows. Never
    /// resubmits. A final answer is sent as the reply and ACKed; a failed turn,
    /// or input that was never admitted and is not in the thread, gets a
    /// failure reply and ACK. A message already ACKed in the queue gets nothing
    /// more. Anything else stays parked: `Ok(None)`. A settled journal is
    /// renamed `.resolved-*`, never deleted.
    pub fn reconcile(&self, path: &Path, found: Found) -> Result<Option<&'static str>> {
        let p = read_pending(path)?;
        if p.binding.thread_id != self.binding.thread_id {
            return Ok(None);
        }
        let (id, e) = (&p.delivery.message.id, &p.delivery.message.envelope);
        let status: Value = self.call(Action::State {
            message_id: id.clone(),
            receipt: p.delivery.receipt.clone(),
        })?;
        let acked = status["acknowledged"]
            .as_bool()
            .ok_or("invalid_receiver_response")?;
        let failed = matches!(&found, Found::Failed(_))
            || (found == Found::NotThere && p.admission.is_none());
        let (outcome, body) = match found {
            _ if acked => ("already_acked", None),
            Found::Final(text) => ("replied", Some(text)),
            Found::Failed(why) => ("failed", Some(failure_body(&why))),
            Found::NotThere if p.admission.is_none() => (
                "not_received",
                Some(failure_body("Codex never received it")),
            ),
            _ => return Ok(None),
        };
        if let Some(body) = body {
            if !failed {
                let _: crate::runtime_admission::Record = self.call(Action::Reconcile {
                    message_id: id.clone(),
                    receipt: p.delivery.receipt.clone(),
                    answer_digest: digest(&body)?,
                })?;
            }
            let _: Message = self
                .call(Action::Complete {
                    message_id: id.clone(),
                    receipt: p.delivery.receipt.clone(),
                    body: e.in_reply_to.is_none().then_some(body),
                    failure: failed,
                })
                .map_err(|_| "reconcile_completion_failed")?;
        }
        let resolved = path
            .to_string_lossy()
            .replacen(".unresolved-", ".resolved-", 1);
        std::fs::rename(path, resolved).map_err(|_| "journal_resolve_failed")?;
        Ok(Some(outcome))
    }
    /// Recovery can retry only a persisted final's atomic queue completion, never native inference.
    pub fn complete(&mut self) -> Result<Message> {
        let p = self.pending.as_ref().ok_or("no_pending")?;
        if !p.completed || p.blocked.is_some() {
            return Err("pending_requires_reconciliation");
        }
        let body = if p.delivery.message.envelope.in_reply_to.is_some() {
            None
        } else {
            Some(p.final_text.clone().ok_or("missing_final")?)
        };
        let message = self
            .call(Action::Complete {
                message_id: p.delivery.message.id.clone(),
                receipt: p.delivery.receipt.clone(),
                body,
                failure: false,
            })
            .map_err(|_| "atomic_completion_requires_reconciliation")?;
        std::fs::remove_file(&self.journal).map_err(|_| "completed_journal_cleanup_failed")?;
        self.pending = None;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate as pigeonmq;
    use crate::{Envelope, Queue};
    include!("../../../tests/receiver_support/mod.rs");
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (tempfile::TempDir, PathBuf, Binding) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let db = dir.path().join("queue");
        Queue::open(&db).unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        let b = Binding {
            thread_id: "thread".into(),
            endpoint: dir.path().join("native.sock"),
            cwd: dir.path().into(),
            approved_inputs: vec!["question".into()],
            registration: dir.path().join("registration.json"),
        };
        std::fs::write(
            &b.registration,
            r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":false}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&b.registration, std::fs::Permissions::from_mode(0o600)).unwrap();
        (dir, db, b)
    }
    fn send(db: &Path, terminal: bool) -> String {
        Queue::open_existing(db)
            .unwrap()
            .send(Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body: "question".into(),
                correlation_id: Some("corr".into()),
                in_reply_to: terminal.then(|| "original".into()),
                idempotency_key: None,
            })
            .unwrap()
            .message
            .id
    }
    fn accept(r: &mut Receiver) -> String {
        let params = r.claim().unwrap().unwrap();
        r.admitted(&params,&json!({"queuedSubmission":{"id":"admission","clientUserMessageId":params["clientUserMessageId"],"input":params["input"]}})).unwrap();
        params["clientUserMessageId"].as_str().unwrap().into()
    }
    fn input(client: &str) -> Value {
        json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"type":"userMessage","clientId":client}}})
    }
    fn answer() -> Value {
        json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"type":"agentMessage","phase":"final_answer","id":"answer","text":"final"}}})
    }
    fn completed() -> Value {
        json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"completed","error":null}}})
    }
    fn counts(db: &Path) -> (i64, i64) {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT count(*),count(acknowledged_at_ms) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
    #[test]
    fn ordinary_final_restarts_without_new_admission() {
        let (_d, db, b) = fixture();
        send(&db, false);
        {
            let mut r = open(&db, b.clone()).unwrap();
            let client = accept(&mut r);
            assert!(!r.notification(&input(&client)).unwrap());
            assert!(!r.notification(&answer()).unwrap());
            assert_eq!(counts(&db), (1, 0));
            assert!(r.notification(&completed()).unwrap());
        }
        let mut r = open(&db, b).unwrap();
        assert!(r.pending_completed());
        assert!(r.claim().is_err());
        let reply = r.complete().unwrap();
        assert_eq!(reply.envelope.body, "final");
        assert!(
            reply
                .envelope
                .idempotency_key
                .unwrap()
                .starts_with("gw-reply-")
        );
        assert_eq!(counts(&db), (2, 1));
        assert!(!r.has_pending());
    }
    #[test]
    fn terminal_requires_native_final_then_ack_only_after_restart() {
        let (_d, db, b) = fixture();
        let mut q = Queue::open_existing(&db).unwrap();
        let original = q
            .send(Envelope {
                sender: "codex-local".into(),
                recipient: "dot".into(),
                body: "question".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap();
        let terminal = q
            .send(Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body: "answer".into(),
                correlation_id: Some(original.message.id.clone()),
                in_reply_to: Some(original.message.id),
                idempotency_key: None,
            })
            .unwrap();
        {
            let mut r = open(&db, b.clone()).unwrap();
            let client = accept(&mut r);
            assert!(r.complete().is_err());
            assert_eq!(counts(&db), (2, 0));
            r.notification(&input(&client)).unwrap();
            r.notification(&answer()).unwrap();
            assert!(r.complete().is_err());
            assert!(r.notification(&completed()).unwrap());
        }
        let mut r = open(&db, b).unwrap();
        assert!(r.claim().is_err());
        assert_eq!(r.complete().unwrap().id, terminal.message.id);
        assert_eq!(counts(&db), (2, 1));
        assert!(!r.has_pending());
    }
    #[test]
    fn uncertain_admission_is_never_requeued() {
        let (_d, db, b) = fixture();
        send(&db, false);
        {
            let mut r = open(&db, b.clone()).unwrap();
            assert!(r.claim().unwrap().is_some());
        }
        let mut r = open(&db, b).unwrap();
        assert_eq!(r.claim().unwrap_err(), "pending_requires_reconciliation");
        assert!(r.complete().is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn invalid_terminal_never_reaches_native() {
        let (_d, db, b) = fixture();
        send(&db, true);
        let mut r = open(&db, b).unwrap();
        assert!(r.claim().is_err());
        assert!(r.complete().is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn any_registered_sender_and_body_reach_native() {
        let (_d, db, mut b) = fixture();
        b.approved_inputs.clear();
        std::fs::write(&b.registration, r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"hermes":{"mailbox":"hermes","revoked":false}}"#).unwrap();
        Queue::open_existing(&db)
            .unwrap()
            .send(Envelope {
                sender: "hermes".into(),
                recipient: "codex-local".into(),
                body: "run git rev-parse --short HEAD".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap();
        let mut r = open(&db, b).unwrap();
        let prompt = r.claim().unwrap().unwrap()["input"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            prompt.contains("real request from `hermes`")
                && prompt.ends_with("run git rev-parse --short HEAD")
        );
    }
    #[test]
    fn mixed_input_never_completes_and_approval_does_not_block() {
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b.clone()).unwrap();
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        assert!(r.notification(&input("other")).is_err());
        assert!(r.complete().is_err());
        assert_eq!(counts(&db), (1, 0));
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        let approval =
            json!({"id":99,"method":"item/commandExecution/requestApproval","params":{}});
        assert!(!r.notification(&approval).unwrap());
        r.notification(&answer()).unwrap();
        assert!(r.notification(&completed()).unwrap());
    }
    #[test]
    fn one_owner_and_changed_binding_fail_closed() {
        let (_d, db, mut b) = fixture();
        send(&db, false);
        {
            let mut r = open(&db, b.clone()).unwrap();
            assert!(open(&db, b.clone()).is_err());
            r.claim().unwrap();
        }
        b.thread_id = "replacement".into();
        assert!(open(&db, b).is_err());
    }
    #[test]
    fn replaced_queue_is_not_created_or_mutated() {
        let (d, db, b) = fixture();
        let r = open(&db, b).unwrap();
        std::fs::rename(&db, d.path().join("old")).unwrap();
        assert!(r.authorize().is_err());
        assert!(!db.exists());
    }
    #[test]
    fn foreign_input_before_matching_is_not_ignored() {
        for same_turn in [true, false] {
            let (_d, db, b) = fixture();
            send(&db, false);
            let mut r = open(&db, b).unwrap();
            let client = accept(&mut r);
            let mut foreign = input("human");
            if !same_turn {
                foreign["params"]["turnId"] = json!("older-turn");
            }
            assert!(!r.notification(&foreign).unwrap());
            let result = r.notification(&input(&client));
            if same_turn {
                assert!(result.is_err());
                assert!(r.complete().is_err());
                assert_eq!(counts(&db), (1, 0));
            } else {
                assert!(!result.unwrap());
                r.notification(&answer()).unwrap();
                assert!(r.notification(&completed()).unwrap());
                r.complete().unwrap();
                assert_eq!(counts(&db), (2, 1));
            }
        }
    }
    #[test]
    fn escaped_input_journal_reopens_with_same_identity() {
        let (_d, db, mut b) = fixture();
        let body = "\0".repeat(8191) + "x";
        b.approved_inputs = vec![body.clone()];
        Queue::open_existing(&db)
            .unwrap()
            .send(Envelope {
                sender: "dot".into(),
                recipient: "codex-local".into(),
                body,
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap();
        {
            let mut r = open(&db, b.clone()).unwrap();
            assert!(r.claim().unwrap().is_some());
        }
        let mut r = open(&db, b).unwrap();
        assert!(r.has_pending());
        assert!(r.claim().is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn revoked_owner_stops_admission_and_saved_final() {
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b.clone()).unwrap();
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        r.notification(&answer()).unwrap();
        r.notification(&completed()).unwrap();
        std::fs::write(
            &b.registration,
            r#"{"codex-local":{"mailbox":"codex-local","revoked":true}}"#,
        )
        .unwrap();
        assert!(r.authorize_before_native().is_err());
        assert!(r.complete().is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn expired_final_lease_stays_pending_without_new_work() {
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        r.notification(&answer()).unwrap();
        r.notification(&completed()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute("UPDATE messages SET lease_until_ms=0", [])
            .unwrap();
        assert!(r.complete().is_err());
        assert!(r.has_pending());
        assert!(r.claim().is_err());
        assert_eq!(counts(&db), (1, 0));
        let stage: String = c
            .query_row("SELECT stage FROM runtime_admissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stage, "model_completed");
    }
    #[test]
    fn long_turn_renews_lease_beyond_its_length() {
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        r.lease = Duration::from_millis(1000);
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(300));
            r.renew_lease().unwrap();
        }
        // 1.8 s elapsed, beyond the 1 s lease: the turn still completes.
        r.notification(&answer()).unwrap();
        assert!(r.notification(&completed()).unwrap());
        r.complete().unwrap();
        assert_eq!(counts(&db), (2, 1));
    }
    #[test]
    fn failed_message_gets_failure_reply_and_next_message_is_claimed() {
        let (_d, db, b) = fixture();
        send(&db, false);
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        let client = accept(&mut r);
        r.notification(&input(&client)).unwrap();
        let failed = json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn","status":"failed","error":{"message":"boom"}}}});
        let reason = r.notification(&failed).unwrap_err();
        assert_eq!(reason, "native_turn_failed");
        let (_, sender, stored) = r.fail(reason).unwrap();
        assert_eq!((sender.as_str(), stored), ("dot", true));
        assert_eq!(counts(&db), (3, 1));
        assert!(r.claim().unwrap().is_some());
    }
    #[test]
    fn unregistered_sender_is_rejected_by_hub_without_claim_or_native() {
        let (_d, db, b) = fixture();
        Queue::open_existing(&db)
            .unwrap()
            .send(Envelope {
                sender: "stranger".into(),
                recipient: "codex-local".into(),
                body: "hi".into(),
                correlation_id: None,
                in_reply_to: None,
                idempotency_key: None,
            })
            .unwrap();
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        assert!(r.claim().is_err());
        assert!(!r.has_pending());
        assert_eq!(counts(&db), (2, 0));
        let c = rusqlite::Connection::open(&db).unwrap();
        let untouched: bool = c
            .query_row(
                "SELECT count(*)=2 FROM messages WHERE attempts=0 AND receipt IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(untouched);
        // Remove only the injected invalid fixture to exercise the next permitted item.
        c.execute("DELETE FROM messages WHERE sender='stranger'", [])
            .unwrap();
        assert!(r.claim().unwrap().is_some());
    }
    #[test]
    fn reconciliation_settles_only_proven_outcomes_and_never_resubmits() {
        let (_d, db, b) = fixture();
        let [a, c, n, u, k] = [(); 5].map(|_| send(&db, false));
        let mut r = open(&db, b).unwrap();
        // a, c, u: admitted then lost; n: claimed but never admitted; k: acked elsewhere.
        for admitted in [true, true, false, true, false] {
            if admitted {
                accept(&mut r);
            } else {
                r.claim().unwrap().unwrap();
            }
            r.set_aside("native_transport_disconnected").unwrap();
        }
        let parked = r.unresolved();
        assert_eq!(parked.len(), 5);
        let path = |id: &str| {
            parked
                .iter()
                .find(|p| p.1 == format!("pigeon-{id}"))
                .unwrap()
                .0
                .clone()
        };
        assert!(parked.iter().any(|p| p.1 == format!("pigeon-{a}") && p.2));
        assert!(parked.iter().any(|p| p.1 == format!("pigeon-{n}") && !p.2));
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("UPDATE messages SET acknowledged_at_ms=1 WHERE id=?1", [&k])
            .unwrap();
        // Admitted but not found is still unknown; so is a running turn.
        assert_eq!(r.reconcile(&path(&u), Found::NotThere).unwrap(), None);
        assert_eq!(r.reconcile(&path(&u), Found::Unknown).unwrap(), None);
        assert_eq!(
            r.reconcile(&path(&a), Found::Final("done".into())).unwrap(),
            Some("replied")
        );
        assert_eq!(
            r.reconcile(&path(&c), Found::Failed("failed".into()))
                .unwrap(),
            Some("failed")
        );
        assert_eq!(
            r.reconcile(&path(&n), Found::NotThere).unwrap(),
            Some("not_received")
        );
        assert_eq!(
            r.reconcile(&path(&k), Found::NotThere).unwrap(),
            Some("already_acked")
        );
        // 5 originals + 3 replies; a, c, n, k ACKed; only u still parked.
        assert_eq!(counts(&db), (8, 4));
        let left = r.unresolved();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1, format!("pigeon-{u}"));
        let body: String = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT body FROM messages WHERE in_reply_to=?1",
                [&a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body, "done");
        // Nothing was handed to Codex again: no new claim is pending.
        assert!(!r.has_pending());
    }
    #[test]
    fn trusted_final_recovers_lost_admission_response_or_record_without_resubmission() {
        for local_admission_saved in [false, true] {
            let (_dir, db, binding) = fixture();
            let id = send(&db, false);
            let mut receiver = open(&db, binding.clone()).unwrap();
            let params = receiver.claim().unwrap().unwrap();
            assert_eq!(params["clientUserMessageId"], format!("pigeon-{id}"));
            if local_admission_saved {
                // Native admission was saved locally, but the hub update did not succeed.
                receiver.pending.as_mut().unwrap().admission = Some("native-admission".into());
                receiver.save().unwrap();
            }
            let path = receiver.set_aside("native_transport_disconnected").unwrap();
            assert_eq!(receiver.reconcile(&path, Found::Unknown).unwrap(), None);
            assert_eq!(counts(&db), (1, 0));
            assert_eq!(
                receiver
                    .reconcile(&path, Found::Final("recovered final".into()))
                    .unwrap(),
                Some("replied")
            );
            let connection = rusqlite::Connection::open(&db).unwrap();
            let (stage, admission, attempts): (String, Option<String>, i64) = connection.query_row(
                "SELECT r.stage,r.admission_id,m.attempts FROM runtime_admissions r JOIN messages m ON m.id=r.message_id WHERE m.id=?1",
                [&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(stage, "reconciled_completed");
            assert_eq!(admission, None); // Never invent a missing native admission ID.
            assert_eq!(attempts, 1);
            let reply: (String, String, String) = connection
                .query_row(
                    "SELECT id,idempotency_key,body FROM messages WHERE in_reply_to=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(reply.1, format!("gw-reply-{id}"));
            assert_eq!(reply.2, "recovered final");
            assert_eq!(counts(&db), (2, 1));
            assert!(!receiver.has_pending());
            drop(receiver);
            let mut reopened = open(&db, binding).unwrap();
            assert!(reopened.claim().unwrap().is_none());
            assert_eq!(counts(&db), (2, 1));
            let retained: String = connection
                .query_row(
                    "SELECT id FROM messages WHERE in_reply_to=?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, reply.0);
        }
    }

    #[test]
    fn unknown_outcome_is_kept_unacked_and_others_flow() {
        let (d, db, b) = fixture();
        let first = send(&db, false);
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        accept(&mut r);
        let aside = r.set_aside("native_transport_disconnected").unwrap();
        assert!(aside.exists() && !r.has_pending());
        let kept: Pending = serde_json::from_slice(&std::fs::read(&aside).unwrap()).unwrap();
        assert_eq!(kept.delivery.message.id, first);
        // Not ACKed, not answered, and parked so it is not redelivered.
        assert_eq!(counts(&db), (2, 0));
        let lease: i64 = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT lease_until_ms FROM messages WHERE id=?1",
                [&first],
                |r| r.get(0),
            )
            .unwrap();
        assert!(lease > now().unwrap() + 86_400_000);
        // The next message is still taken.
        let params = r.claim().unwrap().unwrap();
        assert_ne!(params["clientUserMessageId"], format!("pigeon-{first}"));
        drop(d);
    }
    #[test]
    fn native_deadline_is_clamped_to_live_lease() {
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b).unwrap();
        r.claim().unwrap();
        r.pending.as_mut().unwrap().delivery.lease_until_ms = now().unwrap() + 50;
        let start = std::time::Instant::now();
        let deadline = r.native_deadline(Duration::from_secs(10)).unwrap();
        assert!(deadline.duration_since(start) <= Duration::from_millis(51));
        r.pending.as_mut().unwrap().delivery.lease_until_ms = now().unwrap();
        assert!(r.native_deadline(Duration::from_secs(10)).is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn unregistered_or_revoked_peer_cannot_enter_native() {
        for registration in [
            r#"{"codex-local":{"mailbox":"codex-local","revoked":false}}"#,
            r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":true}}"#,
        ] {
            let (_d, db, b) = fixture();
            send(&db, false);
            let mut r = open(&db, b.clone()).unwrap();
            std::fs::write(&b.registration, registration).unwrap();
            assert!(r.claim().is_err());
            assert!(r.authorize_before_native().is_err());
            assert!(r.complete().is_err());
            assert_eq!(counts(&db), (1, 0));
        }
        let (_d, db, b) = fixture();
        send(&db, false);
        let mut r = open(&db, b.clone()).unwrap();
        r.claim().unwrap();
        std::fs::write(&b.registration, r#"{"codex-local":{"mailbox":"codex-local","revoked":false},"dot":{"mailbox":"dot","revoked":true}}"#).unwrap();
        assert!(r.authorize_before_native().is_err());
        assert_eq!(counts(&db), (1, 0));
    }
    #[test]
    fn revocation_during_database_lock_wait_prevents_atomic_completion() {
        for revoked in ["codex-local", "dot"] {
            let (_d, db, b) = fixture();
            send(&db, false);
            let mut r = open(&db, b.clone()).unwrap();
            let client = accept(&mut r);
            r.notification(&input(&client)).unwrap();
            r.notification(&answer()).unwrap();
            r.notification(&completed()).unwrap();
            let c = rusqlite::Connection::open(&db).unwrap();
            c.execute_batch("BEGIN IMMEDIATE").unwrap();
            let worker = std::thread::spawn(move || r.complete());
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(
            &b.registration,
            serde_json::to_vec(&json!({"codex-local":{"mailbox":"codex-local","revoked":revoked == "codex-local"},"dot":{"mailbox":"dot","revoked":revoked == "dot"}})).unwrap(),
        )
        .unwrap();
            c.execute_batch("COMMIT").unwrap();
            assert!(worker.join().unwrap().is_err());
            assert_eq!(counts(&db), (1, 0));
        }
    }
}
