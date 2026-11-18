//! Official MCP service behind the existing bounded, stateless HTTP profile.
//!
//! HTTP authentication, framing and profile-specific cancellation belong to the ingress.
//! rmcp owns MCP request dispatch, version negotiation, tool result encoding,
//! and JSON-RPC response serialization. The
//! bounded channel adapts synchronous queue operations without moving their
//! transaction or authority checks into an async task or retaining credentials.
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CompleteRequestMethod,
        CompleteRequestParams, CompleteResult, ContentBlock, CustomRequest, CustomResult,
        Implementation, ListPromptsRequestMethod, ListPromptsResult,
        ListResourceTemplatesRequestMethod, ListResourceTemplatesResult,
        ListResourcesRequestMethod, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, RequestId, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use std::borrow::Cow;
use tokio::sync::{mpsc, oneshot};

type Fault = (u16, &'static str);
struct ToolCall {
    id: RequestId,
    params: CallToolRequestParams,
    reply: oneshot::Sender<Result<CallToolResult, ErrorData>>,
}
struct EventCall {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<CustomResult, ErrorData>>,
}
struct Profile {
    name: &'static str,
    tools: Vec<Tool>,
    calls: mpsc::Sender<ToolCall>,
    events: Option<mpsc::Sender<EventCall>>,
}
impl ServerHandler for Profile {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_03_26)
        .with_server_info(Implementation::new(self.name, env!("CARGO_PKG_VERSION")))
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_VERSIONS)
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }
    // Unadvertised capabilities must not appear to work via SDK defaults.
    async fn complete(
        &self,
        _: CompleteRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        Err(ErrorData::method_not_found::<CompleteRequestMethod>())
    }
    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Err(ErrorData::method_not_found::<ListPromptsRequestMethod>())
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Err(ErrorData::method_not_found::<ListResourcesRequestMethod>())
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Err(ErrorData::method_not_found::<
            ListResourceTemplatesRequestMethod,
        >())
    }
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        let Some(events) = self
            .events
            .as_ref()
            .filter(|_| request.method.starts_with("events/"))
        else {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                "method_not_found",
                None,
            ));
        };
        let (reply, result) = oneshot::channel();
        events
            .send(EventCall {
                method: request.method,
                params: request.params.unwrap_or_else(|| json!({})),
                reply,
            })
            .await
            .map_err(|_| internal())?;
        result.await.map_err(|_| internal())?
    }
    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if !self.tools.iter().any(|tool| tool.name == params.name) {
            return Err(ErrorData::invalid_params("unknown_tool", None));
        }
        let (reply, result) = oneshot::channel();
        self.calls
            .send(ToolCall {
                id: context.id,
                params,
                reply,
            })
            .await
            .map_err(|_| internal())?;
        Ok(result.await.map_err(|_| internal())??.into())
    }
}
fn internal() -> ErrorData {
    ErrorData::internal_error("operation_failed", None)
}

const SUPPORTED_VERSIONS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2025_03_26, ProtocolVersion::V_2026_07_28];

fn supports_version(version: &str) -> bool {
    SUPPORTED_VERSIONS
        .iter()
        .any(|supported| supported.as_str() == version)
}

fn failed_tool(reason: &'static str) -> CallToolResult {
    let text = json!({"error":reason}).to_string();
    CallToolResult::error(vec![ContentBlock::text(text)])
}

pub(crate) fn successful_tool(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

pub(crate) fn execute_tool(
    name: &str,
    arguments: Value,
    recipient_discovery: bool,
    call: &mut impl FnMut(&str, Value) -> Result<Value, Fault>,
) -> Result<Value, Fault> {
    // FIXME(yagni): comma-separated fan-out is existing tool behavior.
    let subset = (name == "pigeonmq_send")
        .then(|| arguments["recipient"].as_str())
        .flatten()
        .filter(|recipient| recipient.contains(','));
    let Some(subset) = subset else {
        return call(name, arguments);
    };
    let recipients: Vec<_> = subset.split(',').map(str::trim).collect();
    if subset.len() > 128
        || recipients.iter().any(|r| {
            r.len() > 128
                || r.chars()
                    .all(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
                || r.chars().any(|c| c < '\u{20}')
        })
        || !arguments["idempotency_key"]
            .as_str()
            .is_some_and(|key| !key.trim().is_empty())
    {
        return Err((400, "invalid_subset_or_idempotency_key"));
    }
    if !recipient_discovery {
        return Err((503, "recipient_preflight_unavailable"));
    }
    // Preflight before the first enqueue. The host rechecks authority on each send.
    let available = call("pigeonmq_recipients", json!({}))?;
    let available = available["recipients"]
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or((503, "recipient_preflight_unavailable"))?;
    if recipients
        .iter()
        .any(|recipient| !available.contains(recipient))
    {
        return Err((403, "subset_recipient_not_authorized"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut accepted = 0;
    let results: Vec<_> = recipients
        .into_iter()
        .filter(|r| seen.insert(*r))
        .map(|recipient| {
            let mut single = arguments.clone();
            single["recipient"] = json!(recipient);
            match call(name, single) {
                Ok(result) => {
                    accepted += 1;
                    json!({"recipient":recipient,"state":"accepted","result":result})
                }
                Err((code, reason)) => {
                    let state = match (code, reason) {
                        (401 | 403 | 409, _)
                        | (
                            400,
                            "send_conflict"
                            | "invalid_fields"
                            | "invalid_body"
                            | "invalid_identifier"
                            | "invalid_idempotency_key"
                            | "invalid_conversation_identifier",
                        ) => "rejected",
                        _ => "uncertain",
                    };
                    json!({"recipient":recipient,"state":state,"code":code,"error":reason})
                }
            }
        })
        .collect();
    Ok(json!({"state":"submission_results","accepted":accepted,"recipients":results}))
}

/// Build the SDK request from already-bounded ingress input without changing its headers or body.
pub fn streamable_request(
    path: &str,
    headers: &[(String, Vec<u8>)],
    body: &[u8],
) -> Result<http::Request<http_body_util::Full<bytes::Bytes>>, Fault> {
    let mut request = http::Request::post(path)
        .body(http_body_util::Full::new(bytes::Bytes::copy_from_slice(
            body,
        )))
        .map_err(|_| (500, "internal_error"))?;
    for (name, value) in headers {
        let name =
            http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| (400, "invalid_http"))?;
        let value = http::HeaderValue::from_bytes(value).map_err(|_| (400, "invalid_http"))?;
        request.headers_mut().append(name, value);
    }
    Ok(request)
}

/// OpenAI MCP Events requires capabilities.events at this exact path.
pub fn advertise_events(
    response: &mut http::Response<Vec<u8>>,
    enabled: impl FnOnce() -> Result<bool, Fault>,
) -> Result<(), Fault> {
    if response.status() == http::StatusCode::OK {
        let unavailable = (500, "events_capability_unavailable");
        let mut value: Value = serde_json::from_slice(response.body()).map_err(|_| unavailable)?;
        if let Some(result) = value.get_mut("result")
            && enabled()?
        {
            result
                .get_mut("capabilities")
                .and_then(Value::as_object_mut)
                .ok_or(unavailable)?
                .insert("events".into(), json!({}));
            *response.body_mut() = serde_json::to_vec(&value).map_err(|_| unavailable)?;
        }
    }
    Ok(())
}

pub fn dispatch_streamable(
    server_name: &'static str,
    tools: Value,
    request: http::Request<http_body_util::Full<bytes::Bytes>>,
    authority: &str,
    limits: (usize, std::time::Instant),
    mut call: impl FnMut(&RequestId, &str, Value) -> Result<Value, Fault>,
    event: impl FnMut(&str, Value) -> Result<Result<Value, ErrorData>, Fault>,
) -> Result<http::Response<Vec<u8>>, Fault> {
    let recipient_discovery = tools.as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["name"] == "pigeonmq_recipients")
    });
    dispatch_streamable_with_tools(
        server_name,
        tools,
        request,
        authority,
        limits,
        |id, name, args| {
            execute_tool(name, args, recipient_discovery, &mut |name, args| {
                call(id, name, args)
            })
            .map(|value| Ok(successful_tool(value)))
            .or_else(|(status, reason)| match (status, reason) {
                (401 | 499, _)
                | (
                    _,
                    "auth_unavailable"
                    | "registration_unavailable"
                    | "request_already_active"
                    | "request_state_unavailable"
                    | "transport_unavailable",
                ) => Err((status, reason)),
                (400, "invalid_fields") => Ok(Err(ErrorData::invalid_params(reason, None))),
                (500, _) => Ok(Err(internal())),
                _ => Ok(Ok(failed_tool(reason))),
            })
        },
        event,
    )
}

/// The opt-in generic profile keeps HTTP faults for its existing policy and RPC contracts.
pub fn dispatch_generic_streamable(
    tools: Value,
    request: http::Request<http_body_util::Full<bytes::Bytes>>,
    authority: &str,
    limits: (usize, std::time::Instant),
    mut call: impl FnMut(&str, Value) -> Result<Value, Fault>,
) -> Result<http::Response<Vec<u8>>, Fault> {
    dispatch_streamable_with_tools(
        "pigeonmq",
        tools,
        request,
        authority,
        limits,
        |_, name, args| {
            // This policy has no registered-recipient preflight. Do not fan out.
            execute_tool(name, args, false, &mut call)
                .map(|value| Ok(successful_tool(value)))
                .or_else(|fault| match fault {
                    (400, "invalid_params") => {
                        Ok(Err(ErrorData::invalid_params("invalid_params", None)))
                    }
                    other => Err(other),
                })
        },
        |_, _| {
            Ok(Err(ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                "method_not_found",
                None,
            )))
        },
    )
}

pub(crate) fn dispatch_streamable_with_tools(
    server_name: &'static str,
    tools: Value,
    request: http::Request<http_body_util::Full<bytes::Bytes>>,
    authority: &str,
    (max_body, deadline): (usize, std::time::Instant),
    mut call: impl FnMut(&RequestId, &str, Value) -> Result<Result<CallToolResult, ErrorData>, Fault>,
    mut event: impl FnMut(&str, Value) -> Result<Result<Value, ErrorData>, Fault>,
) -> Result<http::Response<Vec<u8>>, Fault> {
    use http_body_util::BodyExt;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    };
    use std::sync::Arc;
    // rmcp validates body/header agreement. Keep the application's version
    // allowlist and reject duplicate singleton headers even on older versions.
    for header in ["mcp-protocol-version", "mcp-method", "mcp-name"] {
        let all = request.headers().get_all(header);
        let mut values = all.iter();
        if let Some(first) = values.next() {
            if values.next().is_some() {
                return Err((400, "duplicate_mcp_header"));
            }
            let value = first.to_str().map_err(|_| (400, "invalid_mcp_header"))?;
            if header == "mcp-protocol-version" && !supports_version(value) {
                return Err((400, "unsupported_protocol_version"));
            }
        }
    }
    let tools: Vec<Tool> = serde_json::from_value(tools).map_err(|_| (500, "internal_error"))?;
    let (calls, mut operations) = mpsc::channel::<ToolCall>(1);
    let (events, mut event_operations) = mpsc::channel::<EventCall>(1);
    let service = StreamableHttpService::new(
        move || {
            Ok(Profile {
                name: server_name,
                tools: tools.clone(),
                calls: calls.clone(),
                events: Some(events.clone()),
            })
        },
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts([authority])
            .enforce_origin_validation()
            .with_max_request_body_bytes(max_body)
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| (503, "transport_unavailable"))?;
    let response = async {
        let exchange = async {
            let (parts, body) = service.handle(request).await.into_parts();
            let body = body
                .collect()
                .await
                .map_err(|_| (503, "transport_unavailable"))?;
            Ok(http::Response::from_parts(parts, body.to_bytes().to_vec()))
        };
        tokio::time::timeout_at(deadline.into(), exchange)
            .await
            .map_err(|_| (499, "request_cancelled"))?
    };
    tokio::pin!(response);
    enum Pending {
        Response(Result<http::Response<Vec<u8>>, Fault>),
        Tool(ToolCall),
        Event(EventCall),
    }
    loop {
        // Queue and callback work uses the connection worker, not the SDK runtime.
        // Callback verification already has its own bounded Tokio runtime.
        match runtime.block_on(async {
            tokio::select! {
                response = &mut response => Pending::Response(response),
                Some(operation) = operations.recv() => Pending::Tool(operation),
                Some(operation) = event_operations.recv() => Pending::Event(operation),
            }
        }) {
            Pending::Response(response) => return response,
            Pending::Tool(operation) => {
                let arguments = Value::Object(operation.params.arguments.unwrap_or_default());
                let result = call(&operation.id, &operation.params.name, arguments)?;
                let _ = operation.reply.send(result);
            }
            Pending::Event(operation) => {
                let result = event(&operation.method, operation.params)?.map(CustomResult::new);
                let _ = operation.reply.send(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_event_handler_runs_on_the_connection_worker() {
        let raw =
            br#"{"jsonrpc":"2.0","id":9,"method":"events/subscribe","params":{"name":"fixture"}}"#;
        let request = http::Request::post("/codex/mcp")
            .header("Host", "127.0.0.1:1234")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(http_body_util::Full::new(bytes::Bytes::from_static(raw)))
            .unwrap();
        let reply = dispatch_streamable(
            "fixture",
            json!([]),
            request,
            "127.0.0.1:1234",
            (
                1024,
                std::time::Instant::now() + std::time::Duration::from_secs(2),
            ),
            |_, _, _| panic!("Events must not execute a tool"),
            |method, params| {
                assert_eq!(method, "events/subscribe");
                assert_eq!(params, json!({"name":"fixture"}));
                assert!(tokio::runtime::Handle::try_current().is_err());
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                Ok(Ok(runtime.block_on(async { json!({"fixture":"verified"}) })))
            },
        )
        .unwrap();
        assert_eq!(reply.status(), 200);
        let value: Value = serde_json::from_slice(reply.body()).unwrap();
        assert_eq!(
            value,
            json!({"jsonrpc":"2.0","id":9,"result":{"fixture":"verified"}})
        );
    }
}
