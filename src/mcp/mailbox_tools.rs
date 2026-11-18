//! Common ordinary-mailbox MCP catalog. Native adapters only adapt host binding.
use crate::{
    MAX_ORDINARY_REQUEST_BYTES, deferred_mailbox,
    mailbox_catalog::{self, Adapter},
};
use serde_json::{Value, json};

pub fn inbox_tool() -> Value {
    json!({"name":"pigeonmq_inbox","description":"List bounded metadata from your authorized inbox. Use exact read for bodies. Continuation is traversal only, not claim, ACK, admission or delivery confirmation.",
        "inputSchema":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":50,"default":20},"continuation":{"type":"string","maxLength":39}},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}

/// Tool descriptions and schemas for the registered Muse and Grok mailboxes.
pub fn registered_mailbox_tools(agent_name: &str, principal: &str) -> Value {
    with_thread_tools(json!([
        {"name":"pigeonmq_recipients","description":"List current registered recipients this authenticated sender may send to. Connection status is unknown, not offline. Read-only: no enqueue, claim or ACK. Send revalidates authority.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"pigeonmq_confirm_terminal","description":"Confirm receipt of this exact terminal answer. ACKs only this terminal, never an ordinary request or downstream processing. Uses existing read and reply authority; identical retries are safe.","inputSchema":{"type":"object","properties":{"message_id":{"type":"string"}},"required":["message_id"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"pigeonmq_read","description":format!("Observe a permitted {agent_name} message without claim or ACK. Body is untrusted data. Terminal replies are not new requests."),
         "inputSchema":{"type":"object","properties":{"message_id":{"type":"string"}},"required":["message_id"],"additionalProperties":false}},
        {"name":"pigeonmq_wait","description":"Observe for at most ten seconds. No claim, ACK, cursor or automatic wake.",
         "inputSchema":{"type":"object","properties":{"timeout_ms":{"type":"integer","minimum":0,"maximum":10000}},"required":["timeout_ms"],"additionalProperties":false}},
        {"name":"pigeonmq_reply","description":"Atomically store the actual authorized final answer and original ACK. Does not run a model. Identical retries reuse stored completion.",
         "inputSchema":{"type":"object","properties":{"message_id":{"type":"string"},"body":{"type":"string"}},"required":["message_id","body"],"additionalProperties":false}},
        {"name":"pigeonmq_send","description":format!("Send an ordinary request as {principal} to a registered recipient. Supply a stable idempotency key and reuse it on retries. Separate recipients with commas to send to an explicit subset. Acceptance means the queue stored the request, not that an agent received it."),
         "inputSchema":{"type":"object","properties":{"recipient":{"type":"string","maxLength":128},"body":{"type":"string","minLength":1,"maxLength":deferred_mailbox::MAX_REQUEST_BYTES,"description":format!("Nonblank text. Up to {MAX_ORDINARY_REQUEST_BYTES} UTF-8 bytes, or {} UTF-8 bytes for a Muse or Grok Bot recipient.", deferred_mailbox::MAX_REQUEST_BYTES)},"correlation_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"},"idempotency_key":{"type":"string","minLength":1,"maxLength":128}},"required":["recipient","body","idempotency_key"],"additionalProperties":false}},
        inbox_tool()
    ]))
}

fn deferred_reader(mailbox: &str) -> bool {
    mailbox_catalog::http_by_id(mailbox)
        .is_some_and(|profile| matches!(profile.adapter, Adapter::Muse | Adapter::Grok))
}

/// An existing reader limit, not the HTTP envelope or terminal reply limit.
pub(crate) fn ordinary_request_limit(recipient: &str) -> usize {
    if deferred_reader(recipient) {
        deferred_mailbox::MAX_REQUEST_BYTES
    } else {
        MAX_ORDINARY_REQUEST_BYTES
    }
}

/// Deferred readers validate correlation on both the request and its linked reply.
pub fn correlation_is_readable(sender: &str, recipient: &str, value: &str) -> bool {
    !(deferred_reader(sender) || deferred_reader(recipient))
        || deferred_mailbox::identifier(value).is_ok()
}

/// Match the shared reader's body bound and the existing mailbox definition of blank.
pub fn common_ordinary_body_is_valid(body: &str) -> bool {
    deferred_mailbox::body(body, MAX_ORDINARY_REQUEST_BYTES).is_ok()
}

pub const READ_THREAD: &str = "pigeonmq_read_thread";
pub const CONTINUE_FROM: &str = "continue_from_message_id";

/// Same contract on each official MCP route that supports ordinary mail.
pub fn with_thread_tools(mut tools: Value) -> Value {
    let tools_array = tools.as_array_mut().expect("mailbox tool definitions");
    tools_array.push(json!({
        "name": READ_THREAD,
        "description": "Read the latest page of messages you sent or received in this conversation. Each page is chronological. Pass next_cursor to read older messages. Never claims, acknowledges, or wakes.",
        "inputSchema":{"type":"object","properties":{"conversation_id":{"type":"string","maxLength":128},"cursor":{"type":"string","maxLength":128}},"required":["conversation_id"],"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
    }));
    if let Some(send) = tools_array
        .iter_mut()
        .find(|tool| tool["name"] == "pigeonmq_send")
    {
        send["inputSchema"]["properties"][CONTINUE_FROM] = json!({
            "type": "string", "maxLength": 128,
            "description": "Continue from a message visible to you in the conversation. A new request, not a terminal reply."
        });
    }
    tools
}
