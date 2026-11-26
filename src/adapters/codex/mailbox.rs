//! Source: PigeonMQ e399161, src/codex_mcp.rs (MIT).
//! Shared ordinary mailbox tools. Deferred reply reuses the common atomic completion.
use crate::{
    auth::{self, Operation, Principal},
    local_http::terminal_confirmation_fault,
    runtime_profiles::LocalIdentity,
};
use radio999k::{
    Envelope, Error, Message, Queue,
    conversation::{self, ThreadScope},
    inbox_page, mailbox_catalog as catalog,
    mailbox_tools::{
        READ_THREAD, common_ordinary_body_is_valid, correlation_is_readable, inbox_tool,
    },
    registration,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
type Fault = (u16, &'static str);
pub(crate) enum Identity {
    BuiltIn(&'static str),
    Local(LocalIdentity),
}
pub struct Profile {
    /// Fixed by validated host configuration, never request data.
    pub(crate) identity: Identity,
    pub db: PathBuf,
    pub policy: PathBuf,
    pub registration: PathBuf,
    pub(crate) hosted: Option<crate::hosted_mailbox_bearer::Scope>,
}
impl Profile {
    pub(crate) fn hosted_cue(
        &self,
        scope: crate::hosted_mailbox_bearer::Scope,
    ) -> Result<Self, Fault> {
        if !matches!(self.identity, Identity::BuiltIn("cue")) {
            return Err((401, "unauthorized"));
        }
        Ok(Self {
            identity: Identity::BuiltIn("cue"),
            db: self.db.clone(),
            policy: self.policy.clone(),
            registration: self.registration.clone(),
            hosted: Some(scope),
        })
    }

    pub(crate) fn mailbox(&self) -> &str {
        match &self.identity {
            Identity::BuiltIn(name) => name,
            Identity::Local(local) => local.mailbox(),
        }
    }
    pub(crate) fn definition(&self) -> Result<&'static catalog::Profile, Fault> {
        if let Identity::BuiltIn(name) = self.identity {
            return catalog::by_mailbox(name).ok_or((403, "identity_not_allowed"));
        }
        Err((403, "identity_not_allowed"))
    }
    pub(crate) fn route(&self) -> Result<&str, Fault> {
        match &self.identity {
            Identity::BuiltIn(name) => {
                self.definition()?;
                catalog::http_by_id(name)
                    .map(|profile| profile.route)
                    .ok_or((403, "identity_not_allowed"))
            }
            Identity::Local(local) => Ok(local.route()),
        }
    }
    pub(crate) fn local(&self) -> bool {
        matches!(self.identity, Identity::Local(_))
    }
    fn tool_enabled(&self, name: &str) -> bool {
        match &self.identity {
            Identity::BuiltIn(_) => true,
            Identity::Local(local) => local.tool_enabled(name),
        }
    }
    fn registered_sender(&self, sender: &str) -> Result<(), Fault> {
        if self.local() && sender != self.mailbox() {
            return Err((403, "sender_not_registered"));
        }
        registration::validate_registration(&self.registration, sender, self.mailbox())
    }
    fn owner(&self, authorization: &[Vec<u8>]) -> Result<Principal, Fault> {
        self.route()?;
        self.check_generation()?;
        let p = if let Some(hosted) = &self.hosted {
            hosted.authorize(authorization)?;
            auth::active_hosted_cue_policy(&self.policy)?
        } else {
            auth::authenticate_private(&self.policy, authorization)?
        };
        p.identity(self.mailbox())?;
        self.check_generation()?;
        registration::validate_registration(&self.registration, self.mailbox(), self.mailbox())?;
        Ok(p)
    }
    fn check_generation(&self) -> Result<(), Fault> {
        if let Identity::Local(local) = &self.identity {
            let storage = radio999k::mailbox_generation::active_storage(&self.db, self.mailbox())
                .map_err(|_| (503, "mailbox_generation_unavailable"))?;
            if storage.as_deref() != Some(local.generation()) {
                return Err((403, "mailbox_generation_retired"));
            }
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), Fault> {
        self.route()?;
        auth::require_private_file(&self.db)?;
        auth::validate_private(&self.policy)?;
        registration::validate_registration(&self.registration, self.mailbox(), self.mailbox())?;
        Queue::open_existing(&self.db).map_err(mapped)?;
        Ok(())
    }
    fn send_authority(&self, headers: &[Vec<u8>], recipient: &str) -> Result<(), Fault> {
        let p = self.owner(headers)?;
        p.operation(Operation::Send)?;
        registration::validate_registration(&self.registration, self.mailbox(), recipient)
    }
    fn read_peers(&self, headers: &[Vec<u8>]) -> Result<Vec<String>, Fault> {
        self.mailbox_authority(headers, Operation::Read)?;
        let mut peers = if self.local() {
            Vec::new()
        } else {
            registration::active_peers_matching(&self.registration, self.mailbox(), |_| true)?
        };
        peers.push(self.mailbox().into());
        Ok(peers)
    }
    fn read_authority(&self, headers: &[Vec<u8>], sender: &str) -> Result<(), Fault> {
        let p = self.owner(headers)?;
        p.operation(Operation::Read)?;
        p.mailbox(self.mailbox())?;
        self.registered_sender(sender)
    }
    fn mailbox_authority(&self, headers: &[Vec<u8>], operation: Operation) -> Result<(), Fault> {
        let p = self.owner(headers)?;
        p.operation(operation)?;
        p.mailbox(self.mailbox())
    }
    fn observed(&self, queue: &Queue, message: &Message) -> Result<(), Fault> {
        if let Some(parent_id) = &message.envelope.in_reply_to {
            let parent = queue
                .observe_ordinary(parent_id, &message.envelope.sender)
                .map_err(|error| match error {
                    Error::Invalid(_) => mapped(Error::NotFound),
                    error => mapped(error),
                })?;
            validate_terminal(message, &parent, self.mailbox())?;
        }
        self.registered_sender(&message.envelope.sender)
    }
    #[cfg(test)]
    fn call(&self, headers: &[Vec<u8>], name: &str, args: Value) -> Result<Value, Fault> {
        self.call_while_open(
            headers,
            name,
            args,
            Instant::now() + Duration::from_secs(15),
            &|| Ok(()),
        )
    }

    fn call_while_open(
        &self,
        headers: &[Vec<u8>],
        name: &str,
        args: Value,
        deadline: Instant,
        check_open: &impl Fn() -> Result<(), Fault>,
    ) -> Result<Value, Fault> {
        check_open()?;
        self.owner(headers)?;
        if !self.tool_enabled(name) {
            return Err((404, "tool_not_found"));
        }
        if !args.is_object() {
            return Err((400, "invalid_fields"));
        }
        auth::require_private_file(&self.db)?;
        match name {
            "p-999k_inbox" => {
                self.mailbox_authority(headers, Operation::Read)?;
                let queue = Queue::open_existing(&self.db).map_err(mapped)?;
                inbox_page::list_with(
                    &self.db,
                    self.mailbox(),
                    &args,
                    deadline,
                    || {
                        check_open()?;
                        self.read_peers(headers)
                    },
                    || Ok(()),
                    |id, _| {
                        let message = match queue.observe_ordinary(id, self.mailbox()) {
                            Ok(message) => message,
                            Err(Error::NotFound) => return Ok(None),
                            Err(error) => return Err(mapped(error)),
                        };
                        match self.observed(&queue, &message) {
                            Ok(()) => queue
                                .ordinary_metadata(id, self.mailbox())
                                .map(|metadata| {
                                    (metadata["state"] != "dead_lettered")
                                        .then(|| inbox_page::metadata_fields(&metadata))
                                })
                                .map_err(mapped),
                            Err((403 | 404, _)) => Ok(None),
                            Err(fault) => Err(fault),
                        }
                    },
                )
            }
            "p-999k_recipients" => {
                if !args.as_object().is_some_and(|args| args.is_empty()) {
                    return Err((400, "invalid_fields"));
                }
                self.owner(headers)?.operation(Operation::Send)?;
                let recipients = registration::active_peers_matching(
                    &self.registration,
                    self.mailbox(),
                    // A comma is the MCP send tool's subset separator, not a literal target.
                    |recipient| send_recipient(recipient) && !recipient.contains(','),
                )?;
                Ok(json!({"recipients":recipients,"connection_status":"unknown"}))
            }
            READ_THREAD => {
                conversation::read_thread_tool(&self.db, self.mailbox(), 8192, &args, || {
                    check_open()?;
                    self.read_peers(headers)
                })
            }
            "p-999k_wait" => {
                let b: Wait = serde_json::from_value(args).map_err(|_| (400, "invalid_fields"))?;
                if b.in_reply_to
                    .as_ref()
                    .is_some_and(|s| s.is_empty() || s.len() > 128)
                {
                    return Err((400, "invalid_fields"));
                }
                let deadline = Instant::now() + Duration::from_millis(b.timeout_ms);
                loop {
                    check_open()?;
                    self.mailbox_authority(headers, Operation::Wait)?;
                    let queue = Queue::open_existing(&self.db).map_err(mapped)?;
                    let mut after = 0;
                    loop {
                        check_open()?;
                        let page = queue
                            .scan_ordinary(self.mailbox(), after, b.in_reply_to.as_deref())
                            .map_err(mapped)?;
                        let count = page.len();
                        for (seq, message) in page {
                            after = seq;
                            let Some(message) = message else { continue };
                            match self.observed(&queue, &message) {
                                Ok(()) => {}
                                Err((403 | 404, _)) => continue,
                                Err(fault) => return Err(fault),
                            }
                            check_open()?;
                            self.mailbox_authority(headers, Operation::Wait)?;
                            return Ok(json!({"message":observe_json(message)?}));
                        }
                        if count < Queue::ORDINARY_SCAN_PAGE_SIZE {
                            break;
                        }
                        if b.timeout_ms == 0 {
                            return Err((503, "observation_budget_exhausted"));
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                    }
                    if Instant::now() >= deadline {
                        return Ok(json!({"message":null}));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            "p-999k_confirm_terminal" => {
                let b: Read = serde_json::from_value(args).map_err(|_| (400, "invalid_fields"))?;
                if b.message_id.trim().is_empty() || b.message_id.len() > 128 {
                    return Err((400, "invalid_fields"));
                }
                let confirm_grants = || {
                    check_open()?;
                    let principal = self.owner(headers)?;
                    principal.operation(Operation::Reply)?;
                    principal.operation(Operation::Read)?;
                    principal.mailbox(self.mailbox())
                };
                confirm_grants()?;
                let mut queue = Queue::open_existing(&self.db).map_err(mapped)?;
                let mut fault = None;
                let result =
                    queue.confirm_terminal_checked(self.mailbox(), &b.message_id, |sender| {
                        confirm_grants()
                            .and_then(|_| {
                                self.registered_sender(sender).map_err(|error| {
                                    if error.0 == 403 {
                                        mapped(Error::NotFound)
                                    } else {
                                        error
                                    }
                                })
                            })
                            .map_err(|e| {
                                fault = Some(e);
                                Error::Invalid("authorization_changed")
                            })
                    });
                let duplicate = result
                    .map_err(|e| fault.unwrap_or_else(|| terminal_confirmation_fault(e, mapped)))?;
                Ok(json!({"state":"acknowledged","message_id":b.message_id,"duplicate":duplicate}))
            }
            "p-999k_reply" => {
                let b: Reply = serde_json::from_value(args).map_err(|_| (400, "invalid_fields"))?;
                if b.message_id.is_empty()
                    || b.message_id.len() > 128
                    || b.body.trim().is_empty()
                    || b.body.len() > 16384
                {
                    return Err((400, "invalid_fields"));
                }
                self.mailbox_authority(headers, Operation::Reply)?;
                let mut queue = Queue::open_existing(&self.db).map_err(mapped)?;
                let original = queue
                    .observe_ordinary(&b.message_id, self.mailbox())
                    .map_err(mapped)?;
                if original.envelope.in_reply_to.is_some() {
                    return Err((409, "reply_is_terminal"));
                }
                let sender = original.envelope.sender.clone();
                registration::validate_registration(&self.registration, &sender, self.mailbox())?;
                let mut fault = None;
                let result = queue.reply_unclaimed_checked(
                    self.mailbox(),
                    &b.message_id,
                    Envelope {
                        sender: self.mailbox().into(),
                        recipient: sender.clone(),
                        body: b.body,
                        correlation_id: Some(
                            original
                                .envelope
                                .correlation_id
                                .unwrap_or_else(|| b.message_id.clone()),
                        ),
                        in_reply_to: Some(b.message_id.clone()),
                        idempotency_key: Some(format!("gw-reply-{}", b.message_id)),
                    },
                    || {
                        check_open()
                            .and_then(|_| self.mailbox_authority(headers, Operation::Reply))
                            .and_then(|_| {
                                registration::validate_registration(
                                    &self.registration,
                                    &sender,
                                    self.mailbox(),
                                )
                            })
                            .map_err(|e| {
                                fault = Some(e);
                                Error::Invalid("authorization_changed")
                            })
                    },
                );
                let reply = result.map_err(|e| fault.unwrap_or_else(|| mapped(e)))?;
                Ok(json!({"state":"replied","reply_id":reply.id}))
            }
            "p-999k_send" => {
                let b: Send = serde_json::from_value(args).map_err(|_| (400, "invalid_fields"))?;
                if !send_recipient(&b.recipient)
                    || b.correlation_id.as_ref().is_some_and(|v| {
                        v.trim().is_empty()
                            || v.len() > 128
                            || !correlation_is_readable(self.mailbox(), &b.recipient, v)
                    })
                {
                    return Err((400, "invalid_fields"));
                }
                if !common_ordinary_body_is_valid(&b.body)
                    || b.idempotency_key.trim().is_empty()
                    || b.idempotency_key.len() > 128
                {
                    return Err((400, "invalid_fields"));
                }
                self.send_authority(headers, &b.recipient)?;
                let mut queue = Queue::open_existing(&self.db).map_err(mapped)?;
                let recipient = b.recipient.clone();
                let mut fault = None;
                let parent = b.continue_from_message_id;
                let peers = parent
                    .as_ref()
                    .map(|_| self.read_peers(headers))
                    .transpose()?;
                let envelope = Envelope {
                    sender: self.mailbox().into(),
                    recipient: b.recipient,
                    body: b.body,
                    correlation_id: b.correlation_id,
                    in_reply_to: None,
                    idempotency_key: Some(b.idempotency_key),
                };
                let authorize = || {
                    check_open()
                        .and_then(|_| self.send_authority(headers, &recipient))
                        .and_then(|_| {
                            if let Some(peers) = &peers
                                && self.read_peers(headers)? != *peers
                            {
                                return Err((403, "thread_scope_changed"));
                            }
                            Ok(())
                        })
                        .map_err(|e| {
                            fault = Some(e);
                            Error::Invalid("authorization_changed")
                        })
                };
                let result = match (parent.as_deref(), peers.as_ref()) {
                    (Some(parent), Some(peers)) => queue.continue_checked(
                        envelope,
                        parent,
                        ThreadScope {
                            mailbox: self.mailbox(),
                            peers,
                            request_bytes: 8192,
                        },
                        authorize,
                    ),
                    _ => queue.send_checked(envelope, authorize),
                };
                serde_json::to_value(result.map_err(|e| fault.unwrap_or_else(|| mapped(e)))?)
                    .map_err(|_| (500, "internal_error"))
            }
            "p-999k_read" => {
                let b: Read = serde_json::from_value(args).map_err(|_| (400, "invalid_fields"))?;
                if b.message_id.trim().is_empty() || b.message_id.len() > 128 {
                    return Err((400, "invalid_fields"));
                }
                let p = self.owner(headers)?;
                p.operation(Operation::Read)?;
                p.mailbox(self.mailbox())?;
                let queue = Queue::open_existing(&self.db).map_err(mapped)?;
                let message = queue
                    .observe_ordinary(&b.message_id, self.mailbox())
                    .map_err(mapped)?;
                if let Some(parent_id) = &message.envelope.in_reply_to {
                    let parent = queue
                        .observe_ordinary(parent_id, &message.envelope.sender)
                        .map_err(|_| (404, "message_not_available"))?;
                    validate_terminal(&message, &parent, self.mailbox())?;
                }
                self.read_authority(headers, &message.envelope.sender)?;
                observe_json(message)
            }
            _ => Err((404, "tool_not_found")),
        }
    }
    #[cfg(test)]
    pub fn dispatch(&self, headers: &[Vec<u8>], body: &[u8]) -> Result<Option<Value>, Fault> {
        let context =
            radio999k::mcp_request::Context::new(Instant::now() + Duration::from_secs(15), &|| {
                false
            });
        self.dispatch_with_events(headers, body, None, &context)
    }
    pub(super) fn dispatch_streamable_http(
        &self,
        (authority, route): (&str, &str),
        sdk_headers: &[(String, Vec<u8>)],
        authorization: &[Vec<u8>],
        body: &[u8],
        events: Option<&crate::codex_events::Runtime>,
        (max_body, context): (usize, &radio999k::mcp_request::Context<'_>),
    ) -> Result<http::Response<Vec<u8>>, Fault> {
        self.owner(authorization)?;
        let check_open = || {
            if context.cancelled() {
                Err((499, "request_cancelled"))
            } else {
                Ok(())
            }
        };
        check_open()?;
        let server_name = if self.local() {
            "999k-local-mailbox"
        } else {
            self.definition()?.server_name
        };
        // Keep the duplicate-key policy. Forward the original body without rewriting it.
        let value =
            radio999k::mcp_request::strict_json(body).map_err(|_| (400, "invalid_fields"))?;
        if !value.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params"))
        }) || value
            .get("params")
            .is_some_and(|params| !params.is_object())
            || (value["method"] == "tools/call" && !value["params"]["arguments"].is_object())
        {
            return Err((400, "invalid_fields"));
        }
        let advertise = matches!(
            value["method"].as_str(),
            Some("initialize" | "server/discover")
        );
        let request = crate::mcp_sdk::streamable_request(route, sdk_headers, body)?;
        let mut definitions = tools();
        if let Some(definitions) = definitions.as_array_mut() {
            definitions.retain(|tool| {
                tool["name"]
                    .as_str()
                    .is_some_and(|name| self.tool_enabled(name))
            });
        }
        let mut response = crate::mcp_sdk::dispatch_streamable(
            server_name,
            definitions,
            request,
            authority,
            (max_body, context.deadline),
            |_, name, args| {
                self.call_while_open(authorization, name, args, context.deadline, &check_open)
            },
            |method, params| {
                let runtime = events.ok_or((404, "events_disabled"))?;
                let token = std::str::from_utf8(&authorization[0])
                    .map_err(|_| (401, "unauthorized"))?
                    .strip_prefix("Bearer ")
                    .ok_or((401, "unauthorized"))?;
                Ok(runtime
                    .event(token, method, &params, context)
                    .map_err(|fault| {
                        rmcp::ErrorData::new(
                            rmcp::model::ErrorCode(fault.code),
                            fault.reason,
                            Some(json!({"reason":fault.reason})),
                        )
                    }))
            },
        )?;
        if advertise && let Some(runtime) = events {
            crate::mcp_sdk::advertise_events(&mut response, || {
                runtime.authorize().map_err(|fault| (503, fault.reason))?;
                Ok(true)
            })?;
        }
        Ok(response)
    }

    #[cfg(test)]
    pub fn dispatch_with_events(
        &self,
        authorization: &[Vec<u8>],
        body: &[u8],
        events: Option<&crate::codex_events::Runtime>,
        context: &radio999k::mcp_request::Context<'_>,
    ) -> Result<Option<Value>, Fault> {
        const HOST: &str = "127.0.0.1:1234";
        let mut headers = vec![
            ("Host".into(), HOST.as_bytes().to_vec()),
            ("Content-Type".into(), b"application/json".to_vec()),
            (
                "Accept".into(),
                b"application/json, text/event-stream".to_vec(),
            ),
        ];
        if let Ok(request) = serde_json::from_slice::<Value>(body)
            && let Some(version) =
                request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str()
            && let Some(method) = request["method"].as_str()
        {
            headers.push(("MCP-Protocol-Version".into(), version.as_bytes().to_vec()));
            headers.push(("Mcp-Method".into(), method.as_bytes().to_vec()));
            if method == "tools/call"
                && let Some(name) = request["params"]["name"].as_str()
            {
                headers.push(("Mcp-Name".into(), name.as_bytes().to_vec()));
            }
        }
        let reply = self.dispatch_streamable_http(
            (HOST, self.route()?),
            &headers,
            authorization,
            body,
            events,
            (106496, context),
        )?;
        if reply.status() == http::StatusCode::ACCEPTED {
            return Ok(None);
        }
        serde_json::from_slice(reply.body())
            .map(Some)
            .map_err(|_| (reply.status().as_u16(), "invalid_response"))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    recipient: String,
    continue_from_message_id: Option<String>,
    body: String,
    idempotency_key: String,
    correlation_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    message_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    #[serde(default, deserialize_with = "wait_timeout_ms")]
    timeout_ms: u64,
    in_reply_to: Option<String>,
}
fn wait_timeout_ms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let ms = f64::deserialize(d)?;
    if !(0.0..=10000.0).contains(&ms) || ms.fract() != 0.0 {
        return Err(serde::de::Error::custom("invalid wait timeout"));
    }
    Ok(ms as u64)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    message_id: String,
    body: String,
}
fn send_recipient(recipient: &str) -> bool {
    !recipient.trim().is_empty() && recipient.len() <= 128
}
fn mapped(e: Error) -> Fault {
    match e {
        Error::NotFound => (404, "message_not_available"),
        Error::Conflict => (409, "conflict"),
        Error::Invalid(_) => (400, "invalid_input"),
        _ => (503, "storage_unavailable"),
    }
}
fn validate_terminal(reply: &Message, parent: &Message, mailbox: &str) -> Result<(), Fault> {
    let r = &reply.envelope;
    let p = &parent.envelope;
    if p.in_reply_to.is_some()
        || p.sender != mailbox
        || p.recipient != r.sender
        || r.recipient != mailbox
        || r.correlation_id.as_deref() != Some(p.correlation_id.as_deref().unwrap_or(&parent.id))
    {
        return Err((404, "message_not_available"));
    }
    Ok(())
}
fn tools() -> Value {
    radio999k::mailbox_tools::with_thread_tools(json!([
        inbox_tool(),
        {"name":"p-999k_recipients","description":"List current registered recipients this authenticated sender may send to. Connection status is unknown, not offline. Read-only: no enqueue, claim or ACK. Send revalidates authority.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"p-999k_confirm_terminal","description":"Confirm that this exact terminal answer body reached the authenticated mailbox consumer. Call only after receiving the answer. ACKs only that terminal, never an ordinary request or model processing; creates no reply. Identical retries are safe.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string","maxLength":128}},"required":["message_id"],"additionalProperties":false}},
        {"name":"p-999k_wait","description":"Observe an ordinary request without claiming or acknowledging. With in_reply_to, observe only answers to that original request. Terminal answers never block request discovery. Bounded to 10 seconds.","inputSchema":{"type":"object","properties":{"timeout_ms":{"type":"integer","minimum":0,"maximum":10000},"in_reply_to":{"type":"string","maxLength":128}},"additionalProperties":false}},
        {"name":"p-999k_reply","description":"Submit the final answer to an ordinary request, storing reply and original ACK atomically. Cannot answer a terminal reply or take over a native receiver claim.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string","maxLength":128},"body":{"type":"string","maxLength":16384}},"required":["message_id","body"],"additionalProperties":false}},
        {"name":"p-999k_send","description":"Send an ordinary mailbox request. Keep the same idempotency key on retries. Does not claim, reply, or acknowledge. Recipient accepts a comma-separated explicit subset; preserve the idempotency key on retry. Subset results describe each submission, not agent completion.","inputSchema":{"type":"object","properties":{"recipient":{"type":"string","maxLength":128},"body":{"type":"string","maxLength":8192},"correlation_id":{"type":"string","maxLength":128},"idempotency_key":{"type":"string","maxLength":128}},"required":["recipient","body","idempotency_key"],"additionalProperties":false}},
        {"name":"p-999k_read","description":"Observe a known inbound message ID for this mailbox. Does not discover replies, claim, acknowledge, or prove native consumption.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string","maxLength":128}},"required":["message_id"],"additionalProperties":false}}
    ]))
}

#[cfg(test)]
#[path = "instinct_reply_tests.rs"]
mod instinct_reply_tests;

fn observe_json(message: Message) -> Result<Value, Fault> {
    let mut value = serde_json::to_value(&message).map_err(|_| (500, "internal_error"))?;
    value["message_id"] = json!(message.id);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    const TOKEN: &str = "PUBLIC_CODEX_TEST_NOT_A_CREDENTIAL";
    #[test]
    fn unknown_profile_identity_is_rejected_before_file_access() {
        let temp = tempfile::tempdir().unwrap();
        let profile = Profile {
            hosted: None,
            identity: Identity::BuiltIn("not-a-catalog-profile"),
            db: temp.path().join("absent.db"),
            policy: temp.path().join("absent-policy.json"),
            registration: temp.path().join("absent-registration.json"),
        };
        assert_eq!(profile.validate(), Err((403, "identity_not_allowed")));
        assert_eq!(profile.definition(), Err((403, "identity_not_allowed")));
    }

    fn headers() -> Vec<Vec<u8>> {
        vec![format!("Bearer {TOKEN}").into_bytes()]
    }
    fn put(path: &std::path::Path, value: Value) {
        std::fs::write(path, value.to_string()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn fixture() -> (tempfile::TempDir, Profile) {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = Profile {
            hosted: None,
            identity: Identity::BuiltIn(catalog::codex().mailbox),
            db: d.path().join("q.db"),
            policy: d.path().join("policy.json"),
            registration: d.path().join("registration.json"),
        };
        Queue::open(&p.db).unwrap();
        std::fs::set_permissions(&p.db, std::fs::Permissions::from_mode(0o600)).unwrap();
        put(
            &p.policy,
            json!({"principals":[{"id":"codex-cloud","token_sha256":Sha256::digest(TOKEN).iter().map(|b|format!("{b:02x}")).collect::<String>(),"operations":["read","wait","reply","send"],"send_to":["dot"],"mailboxes":["codex-cloud"],"revoked":false}]}),
        );
        put(
            &p.registration,
            json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":false}}),
        );
        (d, p)
    }
    fn envelope(sender: &str, recipient: &str) -> Envelope {
        Envelope {
            sender: sender.into(),
            recipient: recipient.into(),
            body: "fixture".into(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        }
    }
    fn args() -> Value {
        json!({"recipient":"dot","body":"question","idempotency_key":"work-one"})
    }
    fn count(p: &Profile) -> (i64, i64, i64) {
        rusqlite::Connection::open(&p.db)
            .unwrap()
            .query_row(
                "SELECT count(*),count(receipt),count(acknowledged_at_ms) FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }
    #[test]
    fn mcp_subset_preserves_central_authority_and_replay() {
        let (_d, p) = fixture();
        let request = json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"p-999k_send","arguments":{"recipient":"dot,unknown,dot","body":"fixture","idempotency_key":"subset"}}});
        let rejected = p
            .dispatch(&headers(), &serde_json::to_vec(&request).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(rejected["result"]["isError"], true);
        assert_eq!(
            rejected["result"]["content"][0]["text"],
            r#"{"error":"subset_recipient_not_authorized"}"#
        );
        assert_eq!(count(&p), (0, 0, 0));
        let discovery = p.call(&headers(), "p-999k_recipients", json!({})).unwrap();
        assert_eq!(discovery["connection_status"], "unknown");
        assert!(
            discovery["recipients"]
                .as_array()
                .unwrap()
                .contains(&json!("dot"))
        );
        let mut request = request;
        request["params"]["arguments"]["recipient"] = json!("dot,dot");
        let send = || {
            let response = p
                .dispatch(&headers(), &serde_json::to_vec(&request).unwrap())
                .unwrap()
                .unwrap();
            serde_json::from_str::<Value>(
                response["result"]["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap()
        };
        let first = send();
        assert_eq!(first["accepted"], 1);
        assert_eq!(first["recipients"].as_array().unwrap().len(), 1);
        let replay = send();
        assert_eq!(replay["recipients"][0]["result"]["duplicate"], true);
        assert_eq!(count(&p), (1, 0, 0));
    }
    #[test]
    fn send_is_fixed_identity_idempotent_and_never_completes() {
        let (_d, p) = fixture();
        p.validate().unwrap();
        let a = p.call(&headers(), "p-999k_send", args()).unwrap();
        let b = p.call(&headers(), "p-999k_send", args()).unwrap();
        assert_eq!(a["message"]["id"], b["message"]["id"]);
        assert_eq!(b["duplicate"], true);
        assert_eq!(a["message"]["envelope"]["sender"], "codex-cloud");
        let mut changed = args();
        changed["body"] = json!("changed");
        assert_eq!(
            p.call(&headers(), "p-999k_send", changed).unwrap_err().0,
            409
        );
        for field in ["sender", "in_reply_to", "receipt"] {
            let mut bad = args();
            bad[field] = json!("unexpected");
            assert!(p.call(&headers(), "p-999k_send", bad).is_err());
        }
        assert_eq!(count(&p), (1, 0, 0));
    }
    #[test]
    fn reads_only_ordinary_codex_mailbox_without_claims() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let good = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        let other = q.send(envelope("dot", "elsewhere")).unwrap().message.id;
        let rpc = q
            .request(envelope("dot", "codex-cloud"))
            .unwrap()
            .message
            .id;
        let result = p
            .call(&headers(), "p-999k_read", json!({"message_id":good}))
            .unwrap();
        assert_eq!(result["envelope"]["body"], "fixture");
        assert!(result.get("receipt").is_none());
        for id in [other, rpc] {
            assert_eq!(
                p.call(&headers(), "p-999k_read", json!({"message_id":id}))
                    .unwrap_err()
                    .0,
                404
            );
        }
        assert_eq!(count(&p), (3, 0, 0));
    }
    #[test]
    fn inbox_uses_exact_read_authority_and_returns_only_scoped_metadata() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let first = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        q.send(envelope("dot", "another-mailbox")).unwrap();
        q.send(envelope("unregistered", "codex-cloud")).unwrap();
        q.request(envelope("dot", "codex-cloud")).unwrap();
        let parent = q.send(envelope("codex-cloud", "dot")).unwrap().message.id;
        let mut terminal = envelope("dot", "codex-cloud");
        terminal.in_reply_to = Some(parent.clone());
        terminal.correlation_id = Some("wrong-correlation".into());
        q.send(terminal.clone()).unwrap();
        terminal.correlation_id = Some(parent);
        let terminal = q.send(terminal).unwrap().message.id;
        let own = q
            .send(envelope("codex-cloud", "codex-cloud"))
            .unwrap()
            .message
            .id;
        let quarantined = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        rusqlite::Connection::open(&p.db)
            .unwrap()
            .execute(
                "UPDATE messages SET dead_lettered_at_ms=1 WHERE id=?1",
                [&quarantined],
            )
            .unwrap();
        let before = count(&p);

        let page = |args| p.call(&headers(), "p-999k_inbox", args);
        let first_page = page(json!({"limit":1,"order":"oldest"})).unwrap();
        assert_eq!(first_page["messages"][0]["id"], first);
        assert_eq!(first_page["messages"][0]["state"], "pending");
        assert_eq!(first_page["metadata_only"], true);
        let next =
            page(json!({"continuation":first_page["next_continuation"],"order":"oldest"})).unwrap();
        assert_eq!(next["messages"][0]["id"], terminal);
        assert_eq!(next["messages"][0]["state"], "reply_received");
        assert_eq!(next["messages"][1]["id"], own);
        assert_eq!(next["messages"].as_array().unwrap().len(), 2);
        assert!(next["next_continuation"].is_null());
        for message in next["messages"].as_array().unwrap() {
            assert!(message.get("body").is_none());
            assert!(message.get("receipt").is_none());
            assert_eq!(message["recipient"], "codex-cloud");
        }
        assert_eq!(count(&p), before);
        assert_eq!(
            page(json!({"recipient":"another-mailbox"})),
            Err((400, "invalid_fields"))
        );
        assert_eq!(
            p.call(&[], "p-999k_inbox", json!({})),
            Err((401, "unauthorized"))
        );

        let mut policy: Value = serde_json::from_slice(&std::fs::read(&p.policy).unwrap()).unwrap();
        policy["principals"][0]["operations"] = json!(["send", "reply", "wait"]);
        put(&p.policy, policy.clone());
        assert_eq!(page(json!({})), Err((403, "forbidden")));
        policy["principals"][0]["operations"] = json!(["read"]);
        policy["principals"][0]["mailboxes"] = json!(["another-mailbox"]);
        put(&p.policy, policy);
        assert_eq!(page(json!({})), Err((403, "forbidden")));
    }
    #[test]
    fn terminal_read_requires_original_reverse_correlation_without_chain() {
        for variant in [
            "valid",
            "correlation",
            "chain",
            "wrong_sender",
            "missing",
            "rpc",
        ] {
            let (_d, p) = fixture();
            let mut q = Queue::open_existing(&p.db).unwrap();
            let mut parent = envelope(
                if variant == "wrong_sender" {
                    "elsewhere"
                } else {
                    "codex-cloud"
                },
                "dot",
            );
            if variant == "chain" {
                parent.in_reply_to = Some("older".into());
            }
            let original = if variant == "rpc" {
                q.request(parent)
            } else {
                q.send(parent)
            }
            .unwrap()
            .message
            .id;
            let mut reply = envelope("dot", "codex-cloud");
            reply.in_reply_to = Some(if variant == "missing" {
                "missing".into()
            } else {
                original.clone()
            });
            reply.correlation_id = Some(if variant == "correlation" {
                "bad".into()
            } else {
                original
            });
            let id = q.send(reply).unwrap().message.id;
            let result = p.call(&headers(), "p-999k_read", json!({"message_id":id}));
            assert_eq!(result.is_ok(), variant == "valid", "{variant}");
            assert_eq!(count(&p), (2, 0, 0));
        }
    }
    #[test]
    fn owner_and_peer_revocation_and_scopes_are_enforced() {
        for mode in ["owner", "peer", "scope", "identity"] {
            let (_d, p) = fixture();
            match mode {
                "peer" => put(
                    &p.registration,
                    json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":true}}),
                ),
                _ => {
                    let mut policy: Value =
                        serde_json::from_slice(&std::fs::read(&p.policy).unwrap()).unwrap();
                    match mode {
                        "owner" => policy["principals"][0]["revoked"] = json!(true),
                        "scope" => policy["principals"][0]["operations"] = json!([]),
                        _ => policy["principals"][0]["id"] = json!("dot"),
                    };
                    put(&p.policy, policy);
                }
            }
            assert!(p.call(&headers(), "p-999k_send", args()).is_err());
            let discovery = p.call(&headers(), "p-999k_recipients", json!({}));
            if mode == "peer" {
                assert_eq!(discovery.unwrap()["recipients"], json!([]));
            } else {
                assert!(discovery.is_err());
            }
            assert_eq!(count(&p), (0, 0, 0));
        }
    }
    #[test]
    fn transactional_send_rechecks_peer_after_lock_wait() {
        let (_d, p) = fixture();
        let registration = p.registration.clone();
        let db = p.db.clone();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let worker = std::thread::spawn(move || p.call(&headers(), "p-999k_send", args()));
        std::thread::sleep(Duration::from_millis(100));
        put(
            &registration,
            json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"dot":{"mailbox":"dot","revoked":true}}),
        );
        c.execute_batch("COMMIT").unwrap();
        assert!(worker.join().unwrap().is_err());
        let n: i64 = c
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
    #[test]
    fn central_registration_not_legacy_peer_list_selects_recipients() {
        let (_d, p) = fixture();
        put(
            &p.registration,
            json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},"hermes":{"mailbox":"hermes","revoked":false}}),
        );
        let mut input = args();
        input["recipient"] = json!("hermes");
        assert!(p.call(&headers(), "p-999k_send", input).is_ok());
        assert_eq!(count(&p), (1, 0, 0));
    }
    #[test]
    fn recipient_discovery_includes_addressable_registered_utf8_peers() {
        let (_d, p) = fixture();
        let mut registry: Value =
            serde_json::from_slice(&std::fs::read(&p.registration).unwrap()).unwrap();
        for peer in ["é-peer", "comma,peer", " ", &"x".repeat(129)] {
            registry[peer] = json!({"mailbox":peer,"revoked":false});
        }
        registry["revoked"] = json!({"mailbox":"revoked","revoked":true});
        registry["alias"] = json!({"mailbox":"other","revoked":false});
        put(&p.registration, registry);
        let discovery = p.call(&headers(), "p-999k_recipients", json!({})).unwrap();
        assert_eq!(discovery["recipients"], json!(["dot", "é-peer"]));
        let request = json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
            "name":"p-999k_send","arguments":{"recipient":"dot,é-peer","body":"fixture","idempotency_key":"subset"}}});
        let reply = p
            .dispatch(&headers(), &serde_json::to_vec(&request).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(reply["result"]["isError"], false);
        let result: Value =
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(result["accepted"], 2);
        assert_eq!(count(&p), (2, 0, 0));
    }
    #[test]
    fn send_metadata_uses_matching_utf8_byte_bounds() {
        for length in [128, 129] {
            let (_d, p) = fixture();
            let recipient = "é".repeat(64) + if length == 129 { "x" } else { "" };
            let correlation = recipient.clone();
            put(
                &p.registration,
                json!({"codex-cloud":{"mailbox":"codex-cloud","revoked":false},recipient.clone():{"mailbox":recipient,"revoked":false}}),
            );
            let result=p.call(&headers(),"p-999k_send",json!({"recipient":recipient,"body":"q","correlation_id":correlation,"idempotency_key":"key"}));
            assert_eq!(result.is_ok(), length == 128);
        }
        let (_d, p) = fixture();
        let mut input = args();
        input["correlation_id"] = json!("é".repeat(64) + "x");
        assert!(p.call(&headers(), "p-999k_send", input).is_err());
        assert_eq!(count(&p), (0, 0, 0));
    }
    #[test]
    fn objects_are_required_and_duplicate_fields_rejected() {
        let (_d, p) = fixture();
        for raw in [
            r#"["2.0",1,"tools/list",{}]"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":[]}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"p-999k_send","arguments":["dot","body","key",null]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"p-999k_read","arguments":["id"]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"p-999k_send","arguments":{"recipient":"dot","recipient":"hermes","body":"b","idempotency_key":"k"}}}"#,
        ] {
            assert!(p.dispatch(&headers(), raw.as_bytes()).is_err(), "{raw}");
        }
        assert!(p.call(&headers(), "p-999k_read", json!(["id"])).is_err());
        assert_eq!(count(&p), (0, 0, 0));
    }
    #[test]
    fn oversized_stored_message_fields_never_become_tool_output() {
        for (terminal, size) in [(false, 8193), (true, 16385)] {
            let (_d, p) = fixture();
            let mut q = Queue::open_existing(&p.db).unwrap();
            let mut e = envelope("dot", "codex-cloud");
            e.body = "x".repeat(size);
            e.in_reply_to = terminal.then(|| "parent".into());
            let id = q.send(e).unwrap().message.id;
            assert!(
                p.call(&headers(), "p-999k_read", json!({"message_id":id}))
                    .is_err()
            );
            assert_eq!(count(&p), (1, 0, 0));
        }
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let mut e = envelope("dot", "codex-cloud");
        e.correlation_id = Some("x".repeat(129));
        let id = q.send(e).unwrap().message.id;
        assert!(
            p.call(&headers(), "p-999k_read", json!({"message_id":id}))
                .is_err()
        );
    }
    #[test]
    fn mcp_surface_has_mailbox_tools_and_terminal_confirmation_with_auth() {
        let (_d, p) = fixture();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}).to_string();
        assert!(p.dispatch(&[], request.as_bytes()).is_err());
        let result = p.dispatch(&headers(), request.as_bytes()).unwrap().unwrap();
        let names: Vec<_> = result["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "p-999k_inbox",
                "p-999k_recipients",
                "p-999k_confirm_terminal",
                "p-999k_wait",
                "p-999k_reply",
                "p-999k_send",
                "p-999k_read",
                "p-999k_read_thread"
            ]
        );
        for name in ["p-999k_receive", "p-999k_ack"] {
            assert!(p.call(&headers(), name, json!({})).is_err());
        }
    }
    #[test]
    fn wait_reports_registration_failure_during_observation() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        q.send(envelope("dot", "codex-cloud")).unwrap();
        let checks = std::cell::Cell::new(0);
        let check_open = || {
            // Fail registration after the wait grant but before checking the sender.
            if checks.replace(checks.get() + 1) == 2 {
                put(&p.registration, json!("invalid fixture registry"));
            }
            Ok(())
        };
        assert_eq!(
            p.call_while_open(
                &headers(),
                "p-999k_wait",
                json!({"timeout_ms":0}),
                Instant::now() + Duration::from_secs(2),
                &check_open,
            ),
            Err((503, "auth_unavailable"))
        );
        assert_eq!(count(&p), (1, 0, 0));
    }
    #[test]
    fn wait_hides_missing_terminal_parent_but_reports_storage_failure() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let mut terminal = envelope("dot", "codex-cloud");
        terminal.in_reply_to = Some("missing-parent".into());
        terminal.correlation_id = terminal.in_reply_to.clone();
        q.send(terminal.clone()).unwrap();
        let wait = |parent: &str| {
            p.call(
                &headers(),
                "p-999k_wait",
                json!({"timeout_ms":0,"in_reply_to":parent}),
            )
        };
        assert_eq!(wait("missing-parent").unwrap(), json!({"message":null}));

        let parent = q.send(envelope("codex-cloud", "dot")).unwrap().message.id;
        terminal.in_reply_to = Some(parent.clone());
        terminal.correlation_id = terminal.in_reply_to.clone();
        q.send(terminal).unwrap();
        rusqlite::Connection::open(&p.db)
            .unwrap()
            .execute(
                "UPDATE messages SET created_at_ms='invalid fixture timestamp' WHERE id=?",
                [&parent],
            )
            .unwrap();
        assert_eq!(wait(&parent), Err((503, "storage_unavailable")));
        assert_eq!(count(&p), (3, 0, 0));
    }
    #[test]
    fn wait_distinguishes_unreadable_partial_and_full_pages() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let mut unreadable = envelope("dot", "codex-cloud");
        unreadable.body = "x".repeat(8193);
        q.send(unreadable).unwrap();
        let wait = |timeout_ms| p.call(&headers(), "p-999k_wait", json!({"timeout_ms":timeout_ms}));
        assert_eq!(wait(0).unwrap(), json!({"message":null}));

        for _ in 1..Queue::ORDINARY_SCAN_PAGE_SIZE {
            q.send(envelope("unregistered", "codex-cloud")).unwrap();
        }
        let visible = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        assert_eq!(wait(0), Err((503, "observation_budget_exhausted")));
        assert_eq!(wait(2_000).unwrap()["message"]["id"], visible);
        assert_eq!(count(&p), (Queue::ORDINARY_SCAN_PAGE_SIZE as i64 + 1, 0, 0));
    }
    #[test]
    fn deferred_reply_and_wait_preserve_atomicity_and_native_claims() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let incoming = q.send(envelope("dot", "codex-cloud")).unwrap().message;
        let waiting = p
            .call(&headers(), "p-999k_wait", json!({"timeout_ms":0}))
            .unwrap();
        assert_eq!(waiting["message"]["id"], incoming.id);
        assert_eq!(count(&p), (1, 0, 0));
        let db = rusqlite::Connection::open(&p.db).unwrap();
        db.execute_batch("CREATE TRIGGER fail_reply BEFORE INSERT ON messages WHEN NEW.in_reply_to IS NOT NULL BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        let reply_args = json!({"message_id":incoming.id,"body":"actual final answer"});
        assert!(
            p.call(&headers(), "p-999k_reply", reply_args.clone())
                .is_err()
        );
        assert_eq!(count(&p), (1, 0, 0));
        db.execute_batch("DROP TRIGGER fail_reply").unwrap();
        let final_reply = p
            .call(&headers(), "p-999k_reply", reply_args.clone())
            .unwrap();
        assert_eq!(
            final_reply,
            p.call(&headers(), "p-999k_reply", reply_args).unwrap()
        );
        assert_eq!(count(&p), (2, 1, 1));
        assert!(
            p.call(
                &headers(),
                "p-999k_reply",
                json!({"message_id":incoming.id,"body":"changed answer"})
            )
            .is_err()
        );
        let next = q.send(envelope("dot", "codex-cloud")).unwrap().message;
        let lease = q
            .receive("codex-cloud", Duration::from_secs(60))
            .unwrap()
            .unwrap();
        assert_eq!(lease.message.id, next.id);
        assert!(
            p.call(
                &headers(),
                "p-999k_reply",
                json!({"message_id":next.id,"body":"must not steal native work"})
            )
            .is_err()
        );
        assert_eq!(count(&p), (3, 2, 1));
        let outgoing = q.send(envelope("codex-cloud", "dot")).unwrap().message;
        let wait_args = json!({"timeout_ms":0,"in_reply_to":outgoing.id});
        let pending = p
            .call(&headers(), "p-999k_wait", wait_args.clone())
            .unwrap();
        assert!(pending["message"].is_null());
        let mut terminal = envelope("dot", "codex-cloud");
        terminal.in_reply_to = Some(outgoing.id.clone());
        terminal.correlation_id = Some(outgoing.id.clone());
        let terminal = q.send(terminal).unwrap().message;
        let observed = p.call(&headers(), "p-999k_wait", wait_args).unwrap();
        assert_eq!(observed["message"]["id"], terminal.id);
        assert!(
            p.call(
                &headers(),
                "p-999k_reply",
                json!({"message_id":terminal.id,"body":"no reply loop"})
            )
            .is_err()
        );
        assert_eq!(count(&p), (5, 2, 1));
    }
    #[test]
    fn old_terminals_do_not_hide_requests_or_targeted_answers() {
        let (_d, p) = fixture();
        let mut q = Queue::open_existing(&p.db).unwrap();
        let outgoing = q.send(envelope("codex-cloud", "dot")).unwrap().message.id;
        let mut oldest = String::new();
        for _ in 0..105 {
            let mut e = envelope("dot", "codex-cloud");
            e.in_reply_to = Some(outgoing.clone());
            e.correlation_id = Some(outgoing.clone());
            let id = q.send(e).unwrap().message.id;
            if oldest.is_empty() {
                oldest = id;
            }
        }
        let incoming = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        let second_parent = q.send(envelope("codex-cloud", "dot")).unwrap().message.id;
        let mut e = envelope("dot", "codex-cloud");
        e.in_reply_to = Some(second_parent.clone());
        e.correlation_id = Some(second_parent.clone());
        let second_reply = q.send(e).unwrap().message.id;
        let wait = |args: Value| {
            let raw=json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"p-999k_wait","arguments":args}}).to_string();
            let response = p.dispatch(&headers(), raw.as_bytes()).unwrap().unwrap();
            serde_json::from_str::<Value>(
                response["result"]["content"][0]["text"].as_str().unwrap(),
            )
            .unwrap()
        };
        let before = std::fs::read(&p.db).unwrap();
        for _ in 0..2 {
            assert_eq!(wait(json!({"timeout_ms":0}))["message"]["id"], incoming);
        }
        assert_eq!(
            wait(json!({"timeout_ms":0,"in_reply_to":outgoing}))["message"]["id"],
            oldest
        );
        for timeout in [json!(0), json!(10000.0)] {
            assert_eq!(
                wait(json!({"timeout_ms":timeout,"in_reply_to":second_parent}))["message"]["id"],
                second_reply
            );
        }
        assert_eq!(
            p.call(&headers(), "p-999k_read", json!({"message_id":oldest}))
                .unwrap()["id"],
            oldest
        );
        assert_eq!(std::fs::read(&p.db).unwrap(), before);
        p.call(
            &headers(),
            "p-999k_reply",
            json!({"message_id":incoming,"body":"completed once"}),
        )
        .unwrap();
        assert!(wait(json!({"timeout_ms":0}))["message"].is_null());
        let newer = q.send(envelope("dot", "codex-cloud")).unwrap().message.id;
        assert_eq!(wait(json!({"timeout_ms":0}))["message"]["id"], newer);
        assert_eq!(count(&p).2, 1);
    }

    #[test]
    fn wait_rejects_fractional_out_of_range_and_nonnumeric_timeouts() {
        let (_d, p) = fixture();
        for timeout in [
            json!(1.5),
            json!(-1),
            json!(10001),
            json!(10000.5),
            json!("10000"),
            json!(true),
            Value::Null,
        ] {
            assert_eq!(
                p.call(&headers(), "p-999k_wait", json!({"timeout_ms":timeout})),
                Err((400, "invalid_fields")),
                "{timeout}"
            );
        }
    }
}

#[cfg(test)]
#[path = "mailbox_parity_tests.rs"]
mod mailbox_parity_tests;
