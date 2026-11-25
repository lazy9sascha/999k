//! Connect the Dot MCP host to the standalone durable queue.
pub mod events;

use crate::{
    Queue,
    dot_mailbox::Mailbox,
    dot_mcp_host::{Backend, Context, Fault, Host},
    dot_service_preflight::DotPolicy,
    private_file::open_private_file,
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub type DotHost = Host<DotBackend>;
pub struct DotBackend {
    policy: PathBuf,
    mailbox: Mailbox,
    events: Option<Arc<events::Runtime>>,
}
impl DotBackend {
    pub fn host(db: PathBuf, policy: PathBuf) -> DotHost {
        Self::host_with_events(db, policy, None)
    }
    pub fn host_with_events(
        db: PathBuf,
        policy: PathBuf,
        events: Option<Arc<events::Runtime>>,
    ) -> DotHost {
        Host::new(Self {
            mailbox: Mailbox::new(db, policy.clone()),
            policy,
            events,
        })
    }
}

pub fn check(db: &Path, policy: &Path, prepare: bool) -> std::io::Result<()> {
    check_with_generation(db, policy, prepare, None).map(|_| ())
}

pub fn check_with_generation(
    db: &Path,
    policy: &Path,
    prepare: bool,
    generation: Option<&str>,
) -> std::io::Result<Option<events::Runtime>> {
    DotPolicy::load(policy).map_err(|error| std::io::Error::other(error.reason))?;
    open_private_file(db).map_err(|_| std::io::Error::other("private database required"))?;
    let queue = Queue::open_existing(db).map_err(std::io::Error::other)?;
    // Validate optional settings before any schema write.
    let events = events::Runtime::new_with_generation(db, policy, generation)
        .map_err(|e| std::io::Error::other(e.reason))?;
    if prepare {
        if let Some(events) = &events {
            events
                .prepare()
                .map_err(|e| std::io::Error::other(e.reason))?;
        } else {
            queue.connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS dot_event_replies(message_id TEXT PRIMARY KEY,reply_id TEXT NOT NULL UNIQUE);",
            ).map_err(std::io::Error::other)?;
        }
    }
    queue
        .connection
        .prepare("SELECT message_id,reply_id FROM dot_event_replies LIMIT 0")
        .map_err(|_| std::io::Error::other("Dot storage unprepared"))?;
    if let Some(events) = &events {
        events
            .check()
            .map_err(|e| std::io::Error::other(e.reason))?;
    }
    Ok(events)
}

impl Backend for DotBackend {
    fn events_enabled(&self) -> Result<bool, Fault> {
        if let Some(events) = &self.events {
            events.authorize()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn event(
        &self,
        bearer: &str,
        method: &str,
        params: &Value,
        context: &Context<'_>,
    ) -> Result<Value, Fault> {
        self.events
            .as_ref()
            .ok_or_else(|| Fault::new(-32601, "events_disabled"))?
            .event(bearer, method, params, context)
    }

    fn authorize(&self, bearer: &str) -> Result<String, Fault> {
        Ok(DotPolicy::load(&self.policy)?.authorize(bearer)?.to_owned())
    }
    fn validate_owner(&self, owner: &str) -> Result<(), Fault> {
        if DotPolicy::load(&self.policy)?.principal() != owner {
            return Err(Fault::new(-32001, "unauthorized"));
        }
        Ok(())
    }
    fn call_tool(
        &self,
        owner: &str,
        bearer: &str,
        name: &str,
        args: &Value,
        context: &Context<'_>,
    ) -> Result<Value, Fault> {
        let string = |key: &str| {
            args[key]
                .as_str()
                .ok_or(Fault::new(-32602, "invalid_string"))
        };
        match name {
            "p-999k_recipients" => self.mailbox.recipients(owner, bearer, context),
            "p-999k_inbox" => self.mailbox.inbox(owner, bearer, args, context),
            "p-999k_send" => self.mailbox.send_with_conversation(
                owner,
                bearer,
                string("recipient")?,
                string("body")?,
                string("idempotency_key")?,
                args.get("conversation")
                    .map(|value| {
                        serde_json::from_value(value.clone())
                            .map_err(|_| Fault::new(-32602, "invalid_conversation_context"))
                    })
                    .transpose()?,
                args.get(crate::mailbox_tools::CONTINUE_FROM)
                    .and_then(Value::as_str),
                context,
            ),
            crate::mailbox_tools::READ_THREAD => {
                self.mailbox.read_thread(owner, bearer, args, context)
            }
            "p-999k_read" => self
                .mailbox
                .read(owner, bearer, string("message_id")?, context),
            "p-999k_confirm_terminal" => {
                self.mailbox
                    .confirm_terminal(owner, bearer, string("message_id")?, context)
            }
            "p-999k_reply" => self.mailbox.reply(
                owner,
                bearer,
                string("message_id")?,
                string("body")?,
                context,
            ),
            "p-999k_wait" => self.mailbox.wait_filtered(
                owner,
                bearer,
                args["timeout_ms"]
                    .as_u64()
                    .ok_or(Fault::new(-32602, "invalid_wait_timeout"))?,
                (
                    args.get("cursor").and_then(Value::as_str),
                    args.get("in_reply_to").and_then(Value::as_str),
                ),
                context,
            ),
            _ => Err(Fault::new(-32601, "unknown_tool")),
        }
    }
}
