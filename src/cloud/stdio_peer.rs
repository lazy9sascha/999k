use super::{error, fail, named_route, strict_json};
use serde_json::{Value, json};
use std::io;

const RECIPIENTS: &str = "p-999k_recipients";
const SEND: &str = "p-999k_send";
const WAIT: &str = "p-999k_wait";
const CONTINUE: &str = "continue_from_message_id";

#[derive(Clone, Copy)]
pub(super) struct Scope<'a> {
    sender: &'a str,
    peer: &'a str,
}

impl<'a> Scope<'a> {
    pub(super) fn new(sender: &'a str, peer: &'a str) -> io::Result<Self> {
        if peer == sender || peer.len() > 128 || !named_route(&format!("/{peer}/mcp")) {
            return Err(fail("invalid local MCP peer"));
        }
        Ok(Self { sender, peer })
    }

    pub(super) fn exchange(
        self,
        method: &str,
        request: &Value,
        forward: impl FnOnce(&Value) -> io::Result<Value>,
    ) -> io::Result<Value> {
        if method == "tools/list" {
            let mut reply = forward(request)?;
            if reply.get("error").is_some() {
                return Ok(reply);
            }
            let tools = reply["result"]["tools"]
                .as_array_mut()
                .ok_or_else(|| fail("invalid local MCP tool list"))?;
            tools.retain(|tool| matches!(tool["name"].as_str(), Some(RECIPIENTS | SEND | WAIT)));
            for tool in tools {
                match tool["name"].as_str() {
                    Some(SEND) => {
                        let schema = &mut tool["inputSchema"];
                        schema["properties"]["recipient"]["const"] = json!(self.peer);
                        if let Some(properties) = schema["properties"].as_object_mut() {
                            properties.remove(CONTINUE);
                        }
                    }
                    Some(WAIT) => {
                        let schema = &mut tool["inputSchema"];
                        let mut required =
                            schema["required"].as_array().cloned().unwrap_or_default();
                        if !required.iter().any(|field| field == "in_reply_to") {
                            required.push(json!("in_reply_to"));
                        }
                        schema["required"] = json!(required);
                        schema["properties"]["in_reply_to"] =
                            json!({"type":"string","minLength":1,"maxLength":128});
                    }
                    _ => {}
                }
            }
            return Ok(reply);
        }

        let id = &request["id"];
        let canonical = request["params"]["name"]
            .as_str()
            .map(radio999k::mailbox_tools::canonical_tool_name);
        let name = canonical.as_deref();
        let args = &request["params"]["arguments"];
        match name {
            Some(RECIPIENTS) => {}
            Some(SEND)
                if args["recipient"].as_str() == Some(self.peer)
                    && args.get(CONTINUE).is_none() => {}
            Some(WAIT)
                if args["in_reply_to"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty() && id.len() <= 128) => {}
            Some(SEND | WAIT) => {
                return Ok(error(
                    id.clone(),
                    -32602,
                    "Arguments exceed this bot's peer scope",
                ));
            }
            _ => {
                return Ok(error(
                    id.clone(),
                    -32601,
                    "Tool is outside this bot's peer scope",
                ));
            }
        }

        let reply = forward(request)?;
        if name == Some(SEND) || reply.get("error").is_some() || reply["result"]["isError"] == true
        {
            return Ok(reply);
        }

        let value = tool_value(&reply)?;
        let filtered = match name {
            Some(RECIPIENTS) => {
                let recipients = value["recipients"]
                    .as_array()
                    .ok_or_else(|| fail("invalid local MCP recipient list"))?;
                let recipients: Vec<_> = recipients
                    .iter()
                    .filter(|value| value.as_str() == Some(self.peer))
                    .take(1)
                    .collect();
                json!({"recipients":recipients, "connection_status":"unknown"})
            }
            Some(WAIT) => {
                let message = &value["message"];
                let envelope = &message["envelope"];
                let message = if envelope["sender"] == self.peer
                    && envelope["recipient"] == self.sender
                    && envelope["in_reply_to"] == args["in_reply_to"]
                {
                    message
                } else {
                    &Value::Null
                };
                json!({"message":message})
            }
            _ => unreachable!(),
        };
        // Rebuild the content from the filtered value. Alternate result fields
        // must never carry an unfiltered copy of the hub response.
        Ok(json!({"jsonrpc":"2.0", "id":id, "result":{
            "content":[{"type":"text", "text":filtered.to_string()}], "isError":false
        }}))
    }
}

pub(super) fn tool_value(reply: &Value) -> io::Result<Value> {
    let result = &reply["result"];
    if reply.get("error").is_some() || result["isError"] == true {
        return Err(fail("local MCP tool call failed"));
    }
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    let text = result["content"][0]["text"]
        .as_str()
        .ok_or_else(|| fail("invalid local MCP tool response"))?;
    strict_json(text.as_bytes()).map_err(|_| fail("invalid local MCP tool response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, args: Value) -> Value {
        json!({"id":7, "params":{"name":name, "arguments":args}})
    }

    fn response(value: Value) -> Value {
        json!({"jsonrpc":"2.0","id":7,"result":{
            "content":[{"type":"text","text":value.to_string()}],
            "structuredContent":value,
            "isError":false
        }})
    }

    #[test]
    fn scoped_listing_and_recipient_discovery_hide_other_tools_and_peers() {
        let scope = Scope::new("codex-cloud", "dot").unwrap();
        let tools = json!([
            {"name":RECIPIENTS},
            {"name":SEND,"inputSchema":{"properties":{"recipient":{},(CONTINUE):{}}}},
            {"name":WAIT,"inputSchema":{"properties":{},"required":["timeout_ms"]}},
            {"name":"p-999k_read_thread"},
        ]);
        let list = scope
            .exchange("tools/list", &json!({"id":7}), |_| {
                Ok(json!({"result":{"tools":tools}}))
            })
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["recipient"]["const"],
            "dot"
        );
        assert!(
            tools[1]["inputSchema"]["properties"]
                .get(CONTINUE)
                .is_none()
        );
        assert_eq!(
            tools[2]["inputSchema"]["required"],
            json!(["timeout_ms", "in_reply_to"])
        );

        let listed = scope
            .exchange("tools/call", &request(RECIPIENTS, json!({})), |_| {
                Ok(response(json!({"recipients":["dot","cue","muse"]})))
            })
            .unwrap();
        assert_eq!(tool_value(&listed).unwrap()["recipients"], json!(["dot"]));
        assert!(listed["result"].get("structuredContent").is_none());
        assert!(!listed.to_string().contains("cue"));
    }

    #[test]
    fn scoped_requests_reject_multiple_or_wrong_peers_and_unscoped_tools() {
        let scope = Scope::new("codex-cloud", "dot").unwrap();
        assert!(Scope::new("codex-cloud", "dot,muse").is_err());
        assert!(Scope::new("codex-cloud", "codex-cloud").is_err());

        for (tool, args, code) in [
            (SEND, json!({"recipient":"dot,muse"}), -32602),
            (SEND, json!({"recipient":"cue"}), -32602),
            (
                SEND,
                json!({"recipient":"dot",(CONTINUE):"previous"}),
                -32602,
            ),
            (WAIT, json!({"timeout_ms":0}), -32602),
            (WAIT, json!({"in_reply_to":" "}), -32602),
            ("p-999k_read_thread", json!({}), -32601),
        ] {
            let denied = scope
                .exchange("tools/call", &request(tool, args), |_| {
                    panic!("out-of-scope call was forwarded")
                })
                .unwrap();
            assert_eq!(denied["error"]["code"], code);
        }

        let send = request(
            SEND,
            json!({"recipient":"dot","body":"fixture","idempotency_key":"test"}),
        );
        let sent = scope
            .exchange("tools/call", &send, |forwarded| {
                assert_eq!(forwarded, &send);
                Ok(response(json!({"state":"accepted"})))
            })
            .unwrap();
        assert_eq!(tool_value(&sent).unwrap()["state"], "accepted");
    }

    #[test]
    fn scoped_wait_reveals_only_the_matching_peer_and_original() {
        let scope = Scope::new("codex-cloud", "dot").unwrap();
        let wait = request(WAIT, json!({"in_reply_to":"request-a","timeout_ms":0}));
        for (sender, recipient, original, visible) in [
            ("dot", "codex-cloud", "request-a", true),
            ("dot", "codex-cloud", "request-b", false),
            ("muse", "codex-cloud", "request-a", false),
            ("dot", "another-sender", "request-a", false),
        ] {
            let waited = scope.exchange("tools/call", &wait, |_| {
                Ok(response(json!({"message":{"envelope":{
                    "sender":sender,"recipient":recipient,"in_reply_to":original,"body":"fixture reply"
                }}})))
            }).unwrap();
            assert_eq!(!tool_value(&waited).unwrap()["message"].is_null(), visible);
            assert_eq!(waited.to_string().contains("fixture reply"), visible);
        }
    }
}
