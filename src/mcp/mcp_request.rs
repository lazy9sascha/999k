//! Shared MCP request context, faults, and strict JSON parsing.
//! Source: PigeonMQ e399161, src/dot_mcp_host.rs (MIT).
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub code: i32,
    pub reason: &'static str,
}
impl Fault {
    pub fn new(code: i32, reason: &'static str) -> Self {
        Self { code, reason }
    }
}
type Result<T> = std::result::Result<T, Fault>;
fn invalid(reason: &'static str) -> Fault {
    Fault::new(-32602, reason)
}

// Context follows PigeonMQ e399161, src/dot_mcp_host.rs (MIT).
pub struct Context<'a> {
    pub deadline: Instant,
    enforce_deadline: bool,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}
impl<'a> Context<'a> {
    pub fn new(deadline: Instant, cancelled: &'a (dyn Fn() -> bool + Sync)) -> Self {
        Self {
            deadline,
            enforce_deadline: true,
            cancelled,
        }
    }
    /// Event ticker context only; callback transport retains its own ten-second budget.
    pub fn unbounded(cancelled: &'a (dyn Fn() -> bool + Sync)) -> Self {
        Self {
            deadline: Instant::now(),
            enforce_deadline: false,
            cancelled,
        }
    }
    pub fn cancelled(&self) -> bool {
        (self.enforce_deadline && Instant::now() >= self.deadline) || (self.cancelled)()
    }
}
pub fn strict_json(raw: &[u8]) -> Result<Value> {
    serde_json::from_slice::<Unique>(raw)
        .map(|value| value.0)
        .map_err(|error| {
            invalid(if error.to_string().contains("duplicate_json_key") {
                "duplicate_json_key"
            } else {
                "parse_error"
            })
        })
}

// Recursive visitor prevents duplicate keys disappearing in serde_json::Value.
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("unique JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element::<Unique>()? {
                    v.push(x.0);
                }
                Ok(Unique(Value::Array(v)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut v = serde_json::Map::new();
                while let Some((k, x)) = a.next_entry::<String, Unique>()? {
                    if v.insert(k, x.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate_json_key"));
                    }
                }
                Ok(Unique(Value::Object(v)))
            }
        }
        d.deserialize_any(Visitor)
    }
}

type HttpFault = (u16, &'static str);

/// Preserve registered mailbox field and duplicate-key bounds. rmcp handles wire parsing.
pub fn check_streamable_mailbox_body(body: &[u8]) -> std::result::Result<(), HttpFault> {
    match strict_json(body) {
        Ok(value)
            if value.as_object().is_some_and(|object| {
                object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params"))
                    || object
                        .get("params")
                        .is_some_and(|params| !params.is_object())
                    || object
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.len() > 128)
            }) =>
        {
            Err((400, "invalid_fields"))
        }
        Err(fault) if fault.reason == "duplicate_json_key" => Err((400, "invalid_fields")),
        _ => Ok(()),
    }
}

/// Stateless rmcp validates notifications but does not deliver them to a handler.
/// An authenticated profile may cancel an in-flight request only after SDK acceptance.
pub fn accepted_cancellation(
    status: http::StatusCode,
    body: &[u8],
) -> std::result::Result<Option<rmcp::model::RequestId>, HttpFault> {
    use rmcp::model::{ClientJsonRpcMessage, ClientNotification, RequestId};
    if status != http::StatusCode::ACCEPTED {
        return Ok(None);
    }
    let Ok(ClientJsonRpcMessage::Notification(notification)) = serde_json::from_slice(body) else {
        return Ok(None);
    };
    let ClientNotification::CancelledNotification(cancellation) = notification.notification else {
        return Ok(None);
    };
    let params = cancellation.params;
    if params.reason.is_some_and(|reason| reason.len() > 512)
        || matches!(&params.request_id, Some(RequestId::String(id)) if id.len() > 128)
    {
        return Err((400, "invalid_fields"));
    }
    Ok(params.request_id)
}

pub fn registered_mailbox_fields<'a>(
    value: &'a Value,
    required: &[&str],
    optional: &[&str],
) -> std::result::Result<&'a Map<String, Value>, HttpFault> {
    let object = value.as_object().ok_or((400, "invalid_fields"))?;
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
        || object.get("_meta").is_some_and(|value| !value.is_object())
    {
        return Err((400, "invalid_fields"));
    }
    Ok(object)
}

/// Parse the ordinary send contract shared by the registered Muse and Grok routes.
pub fn registered_send<'a>(
    sender: &str,
    arguments: &'a Value,
) -> std::result::Result<(crate::Envelope, Option<&'a str>), HttpFault> {
    let args = registered_mailbox_fields(
        arguments,
        &["recipient", "body", "idempotency_key"],
        &["correlation_id", crate::mailbox_tools::CONTINUE_FROM],
    )?;
    let required = |name| args[name].as_str().ok_or((400, "invalid_fields"));
    let optional = |name| {
        args.get(name)
            .map(|value| value.as_str().ok_or((400, "invalid_fields")))
            .transpose()
    };
    let recipient = required("recipient")?;
    let body = required("body")?;
    let key = required("idempotency_key")?;
    let correlation = optional("correlation_id")?;
    let parent = optional(crate::mailbox_tools::CONTINUE_FROM)?;
    crate::deferred_mailbox::body(
        body,
        crate::mailbox_tools::ordinary_request_limit(recipient),
    )
    .map_err(|e| (400, e.reason))?;
    if crate::py_strip_empty(key) || key.len() > 128 || key.bytes().any(|b| b < b' ') {
        return Err((400, "invalid_idempotency_key"));
    }
    if correlation.is_some_and(|value| {
        !crate::mailbox_tools::correlation_is_readable(sender, recipient, value)
    }) {
        return Err((400, "invalid_identifier"));
    }
    Ok((
        crate::Envelope {
            sender: sender.into(),
            recipient: recipient.into(),
            body: body.into(),
            correlation_id: correlation.map(str::to_owned),
            in_reply_to: None,
            idempotency_key: Some(key.into()),
        },
        parent,
    ))
}
