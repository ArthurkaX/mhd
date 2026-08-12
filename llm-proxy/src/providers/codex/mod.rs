//! Native Codex (ChatGPT OAuth) Responses passthrough.
//!
//! The HTTPS and native WebSocket paths can apply the same opt-in conservative
//! trim to client `response.create` messages. WebSocket framing and all other
//! event types remain unchanged.

mod responses;

/// Compatibility export for trace handlers and existing provider tests.
pub(crate) fn parse_responses_usage(line: &str) -> Option<(u64, u64, Option<u64>)> {
    responses::parse_usage(line)
}

use anyhow::Result;
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::state::AppState;
use serde_json::Value;

const CODEX_ORIGIN: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_WEBSOCKET_URL: &str = "wss://chatgpt.com/backend-api/codex/responses";

pub type CodexWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the native Codex Responses WebSocket with a fixed official endpoint.
/// The returned response headers are the small server capability surface that
/// Codex reads during the upgrade; request headers are rebuilt from the same
/// allowlist as the HTTPS path.
pub async fn connect_websocket(incoming: &HeaderMap) -> Result<(CodexWebSocket, HeaderMap)> {
    let mut request = CODEX_WEBSOCKET_URL.into_client_request()?;
    for name in allowed_request_headers() {
        if let Some(value) = incoming.get(*name) {
            request.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::HeaderName::from_static(name),
                value.clone(),
            );
        }
    }
    let (stream, response) = connect_async(request).await?;
    Ok((stream, response.headers().clone()))
}

/// Convert an upstream WebSocket frame into the local Axum frame type without
/// interpreting or logging its payload.
pub fn to_axum_message(message: Message) -> Option<axum::extract::ws::Message> {
    use axum::extract::ws::{CloseFrame, Message as AxumMessage};
    match message {
        Message::Text(text) => Some(AxumMessage::Text(text.to_string().into())),
        Message::Binary(bytes) => Some(AxumMessage::Binary(bytes.to_vec().into())),
        Message::Ping(bytes) => Some(AxumMessage::Ping(bytes.to_vec().into())),
        Message::Pong(bytes) => Some(AxumMessage::Pong(bytes.to_vec().into())),
        Message::Close(frame) => Some(AxumMessage::Close(frame.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.to_string().into(),
        }))),
        Message::Frame(_) => None,
    }
}

/// Convert a local Axum WebSocket frame to a tungstenite frame.
pub fn to_tungstenite_message(message: axum::extract::ws::Message) -> Option<Message> {
    use axum::extract::ws::Message as AxumMessage;
    match message {
        AxumMessage::Text(text) => Some(Message::Text(text.to_string().into())),
        AxumMessage::Binary(bytes) => Some(Message::Binary(bytes.to_vec().into())),
        AxumMessage::Ping(bytes) => Some(Message::Ping(bytes.to_vec().into())),
        AxumMessage::Pong(bytes) => Some(Message::Pong(bytes.to_vec().into())),
        AxumMessage::Close(frame) => Some(Message::Close(frame.map(|frame| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.to_string().into(),
            }
        }))),
    }
}

/// Decode a Codex JSON request for local corpus analysis while retaining the
/// original bytes for native forwarding.
pub fn decode_request(incoming: &HeaderMap, body: &[u8]) -> Result<Value> {
    let decoded = if incoming
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        == Some("zstd")
    {
        zstd::decode_all(body)?
    } else {
        body.to_vec()
    };
    Ok(serde_json::from_slice(&decoded)?)
}

/// Serialize a Responses request using the same content encoding as ingress.
/// This is used by the opt-in HTTPS trim path; callers can fall back to the
/// original bytes if serialization or compression fails.
pub fn encode_request(incoming: &HeaderMap, payload: &Value) -> Result<bytes::Bytes> {
    let json = serde_json::to_vec(payload)?;
    let encoded = if incoming
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        == Some("zstd")
    {
        zstd::encode_all(json.as_slice(), 0)?
    } else {
        json
    };
    Ok(encoded.into())
}

fn allowed_request_headers() -> &'static [&'static str] {
    &[
        "authorization",
        "accept",
        "content-type",
        "content-encoding",
        "openai-beta",
        "x-codex-beta-features",
        "x-codex-window-id",
        "x-codex-turn-metadata",
        "x-openai-internal-codex-responses-lite",
        "x-client-request-id",
    ]
}

fn copy_request_headers(
    builder: reqwest::RequestBuilder,
    incoming: &HeaderMap,
) -> reqwest::RequestBuilder {
    allowed_request_headers()
        .iter()
        .fold(builder, |builder, name| {
            if let Some(value) = incoming.get(*name) {
                builder.header(*name, value.clone())
            } else {
                builder
            }
        })
}

fn copy_response_headers(
    response: axum::http::response::Builder,
    headers: &reqwest::header::HeaderMap,
) -> axum::http::response::Builder {
    headers.iter().fold(response, |response, (name, value)| {
        let name = name.as_str();
        if matches!(
            name,
            "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "content-length"
        ) {
            response
        } else if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            response.header(name, value)
        } else {
            response
        }
    })
}

/// Forward one Codex HTTP request to the official ChatGPT Codex endpoint.
pub async fn forward(
    state: &Arc<AppState>,
    method: Method,
    path: &str,
    query: Option<&str>,
    incoming: &HeaderMap,
    body: bytes::Bytes,
) -> Result<Response> {
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    let mut url = format!(
        "{}{}",
        CODEX_ORIGIN,
        if suffix.is_empty() { "/" } else { suffix }
    );
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }

    let client = if method == Method::GET {
        &state.http
    } else {
        &state.http_stream
    };
    let mut request = client.request(method, &url);
    request = copy_request_headers(request, incoming);
    if !body.is_empty() {
        request = request.body(body);
    }
    let upstream = request.send().await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())?;
    let headers = upstream.headers().clone();
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    let mut response = Response::builder().status(status);
    response = copy_response_headers(response, &headers);
    Ok(response.body(Body::from_stream(stream))?)
}

/// Wrap a successful Codex Responses response so the terminal `response.completed`
/// SSE event's token counts land in the trace and proxy.db.
///
/// The upstream bytes are forwarded unchanged and incrementally: every chunk is
/// yielded downstream before it is scanned, so latency is unaffected and the
/// stream is never buffered whole. Only integer token counts are extracted —
/// never prompt text, tool output, OAuth headers or account ids. On stream end
/// the row is closed (or marked failed on a transport error), mirroring the
/// native Anthropic tap.
pub fn tap_response_usage(
    state: Arc<AppState>,
    req_id: u64,
    started_ms: u64,
    status: u16,
    resp: Response,
) -> Response {
    let (mut parts, body) = resp.into_parts();
    let status_opt = Some(status);
    let state_for_log = state.clone();
    // Keep the request open while the response body is being consumed. If the
    // Codex client drops the SSE body before EOF, the guard closes the DB row
    // as CANCELLED instead of leaving status/tokens NULL forever.
    let guard = crate::providers::InflightGuard::new(state, req_id);
    let mut data_stream = body.into_data_stream();
    let s = async_stream::stream! {
        let _guard = guard;
        // `response.completed` carries the whole assistant output on ONE `data:`
        // line, so the line buffer can grow to the size of the entire output.
        // Cap it: past this limit with no newline the usage event would be
        // unreasonably large, so we trade a missing usage number for a bounded
        // memory footprint and keep forwarding the stream untouched.
        const LINE_BUF_CAP: usize = 4 * 1024 * 1024;
        let mut scanning = true;
        let mut line_buf: Vec<u8> = Vec::new();
        let mut usage: Option<(u64, u64, Option<u64>)> = None;
        let mut had_error = false;
        while let Some(item) = data_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => { had_error = true; break },
            };
            // Forward the chunk before scanning so the client sees it with no
            // added latency; the scan only reads the same bytes from line_buf.
            // The clone is a cheap refcount bump — the underlying buffer is shared.
            yield Ok::<_, std::io::Error>(chunk.clone());
            if !scanning {
                continue;
            }
            line_buf.extend_from_slice(&chunk);
            // Accumulate lines and scan for the terminal usage event.
            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = line_buf.drain(..=pos).collect();
                if let Ok(line) = std::str::from_utf8(&line_bytes) {
                    if let Some(found) = responses::parse_usage(line.trim()) {
                        usage = Some(found);
                    }
                }
            }
            if line_buf.len() > LINE_BUF_CAP {
                line_buf.clear();
                scanning = false;
            }
        }
        // Close the row exactly like the Anthropic tap: token counts on success
        // (zeros when no usage was found — the row must never stay in flight),
        // a transport-error marker on a broken stream. Duration is measured to
        // the last byte, not to the response headers.
        let duration_ms = crate::providers::now_unix_ms().saturating_sub(started_ms);
        if had_error {
            state_for_log.mark_request_failed(
                req_id,
                Some(duration_ms),
                status_opt,
                "stream transport error",
                "STREAM_ERR",
            );
        } else {
            let (input, output, cache_read) = usage.unwrap_or((0, 0, None));
            state_for_log.update_trace_tokens(
                req_id,
                input,
                output,
                cache_read,
                None,
                Some(duration_ms),
                status_opt,
                None,
            );
        }
    };
    // The body is now re-chunked by `Body::from_stream`, so a stale
    // content-length would make the client stop reading early. Drop it; the
    // remaining headers (content-type, openai-beta, ...) pass through unchanged.
    parts.headers.remove("content-length");
    Response::from_parts(parts, Body::from_stream(s))
}

/// Build the Responses-API `usage` object for the side adapter's synthesized
/// `response.completed` event from the Chat Completions usage the gateway
/// reported via `stream_options.include_usage`. Returns `None` when the gateway
/// reported no usage at all, so the caller omits the `usage` key entirely rather
/// than emit zeros — a missing key keeps the response honest about what the
/// upstream actually reported.
fn responses_usage_from_chat(
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
) -> Option<Value> {
    let input = input?;
    let output = output?;
    let mut usage = serde_json::json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input.saturating_add(output),
    });
    if let Some(cached) = cached {
        usage["input_tokens_details"] = serde_json::json!({"cached_tokens": cached});
    }
    Some(usage)
}

/// Forward Codex Responses to the configured OpenAI-compatible gateway.
/// Native OAuth and all Codex account headers are deliberately excluded.
pub async fn forward_side(
    state: &Arc<AppState>,
    incoming: &HeaderMap,
    body: bytes::Bytes,
    model: &str,
) -> Result<Response> {
    let payload = decode_request(incoming, &body)?;
    return forward_side_chat_adapter(state, payload, model).await;
}

/// What the request direction rewrote, so the response direction can rewrite
/// the model's calls back into the shape Codex expects.
///
/// Chat Completions knows exactly one kind of tool: a named function taking
/// JSON arguments. Two of the tool shapes Codex sends do not fit that mould,
/// so they are translated on the way out and MUST be translated back on the
/// way in — a call returned in the wrong shape is a call Codex cannot execute.
#[derive(Debug, Default, Clone, PartialEq)]
struct ToolMap {
    /// Names of Responses `type: "custom"` tools. Their call carries raw text
    /// rather than JSON arguments (for `exec`, JavaScript source constrained by
    /// a lark grammar), which Chat Completions cannot express — so they go out
    /// as a function with a single `input` string property and come back
    /// unwrapped from it.
    custom: HashSet<String>,
    /// `namespace__child` -> `child`. Responses groups tools under a namespace
    /// item; Chat Completions has no grouping, so children are flattened and
    /// the original name is restored on the way back.
    flattened: HashMap<String, String>,
}

/// Chat Completions has no notion of a free-text tool, so a `custom` tool is
/// exposed as a function taking one string. The model must be told that the
/// string is verbatim payload rather than a description of one, or it writes
/// prose where `exec` expects source.
const CUSTOM_TOOL_NOTE: &str = "\n\nThis tool takes raw text, not structured fields. Put the entire tool input verbatim in the `input` string property — do not summarize it, wrap it in extra quotes, or fence it as markdown.";

/// Extract the text of a Responses content value. Codex only ever sends
/// `input_text` / `output_text` parts (verified across the captured corpus);
/// anything else is refused rather than silently flattened.
fn text_content(content: &Value) -> Result<String> {
    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                if !matches!(kind, "input_text" | "output_text") {
                    anyhow::bail!("Codex side adapter supports text content only (got '{kind}')");
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            Ok(out)
        }
        _ => anyhow::bail!("Codex message content is not text"),
    }
}

/// Build a Chat Completions function tool. `strict` rides along when the
/// Responses tool declared it, since gateways that honour it reject a schema
/// that silently lost the flag.
fn cc_function(name: &str, description: &str, parameters: Value, strict: Option<&Value>) -> Value {
    let mut function =
        serde_json::json!({"name": name, "description": description, "parameters": parameters});
    if let Some(strict) = strict {
        function["strict"] = strict.clone();
    }
    serde_json::json!({"type": "function", "function": function})
}

/// The one-string schema standing in for a Responses `custom` tool.
fn custom_tool_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": {"type": "string", "description": "The complete raw tool input, verbatim."}
        },
        "required": ["input"],
        "additionalProperties": false
    })
}

/// Translate one Responses tool into Chat Completions tools, recursing once
/// into a namespace. Unknown tool types are refused: shipping a tool list the
/// model cannot use correctly is worse than failing the request outright.
fn push_tool(
    tool: &Value,
    namespace: Option<&str>,
    out: &mut Vec<Value>,
    map: &mut ToolMap,
) -> Result<()> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Codex tool has no name"))?;
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    match tool_type {
        "function" => {
            let exposed = match namespace {
                Some(ns) => {
                    let flat = format!("{ns}__{name}");
                    map.flattened.insert(flat.clone(), name.to_string());
                    flat
                }
                None => name.to_string(),
            };
            let parameters = tool.get("parameters").cloned().unwrap_or_else(
                || serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
            );
            out.push(cc_function(
                &exposed,
                description,
                parameters,
                tool.get("strict"),
            ));
        }
        "custom" => {
            if namespace.is_some() {
                anyhow::bail!(
                    "Codex side adapter cannot flatten custom tool '{name}' nested in a namespace"
                );
            }
            map.custom.insert(name.to_string());
            let mut description = format!("{description}{CUSTOM_TOOL_NOTE}");
            // The grammar is the only statement of what `exec` will accept, so
            // it travels with the description — the gateway has nowhere else to
            // put it.
            if let Some(format) = tool.get("format")
                && format.get("type").and_then(Value::as_str) == Some("grammar")
                && let Some(definition) = format.get("definition").and_then(Value::as_str)
            {
                let syntax = format
                    .get("syntax")
                    .and_then(Value::as_str)
                    .unwrap_or("the following");
                description.push_str(&format!(
                    "\n\nThe input must match this {syntax} grammar:\n{definition}"
                ));
            }
            out.push(cc_function(
                name,
                &description,
                custom_tool_parameters(),
                None,
            ));
        }
        "namespace" => {
            if namespace.is_some() {
                anyhow::bail!("Codex side adapter does not support nested namespaces ('{name}')");
            }
            let children = tool
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("Codex namespace tool '{name}' has no tools"))?;
            for child in children {
                push_tool(child, Some(name), out, map)?;
            }
        }
        other => {
            anyhow::bail!("Codex side adapter does not support tool type '{other}' (tool '{name}')")
        }
    }
    Ok(())
}

/// Turn a Responses tool call into a Chat Completions `tool_calls` entry. A
/// custom call's raw text is wrapped into the one-string schema `push_tool`
/// advertised, so both directions agree on where the payload lives.
fn adapt_tool_call(item: &Value, is_custom: bool) -> Result<Value> {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Codex tool call has no call_id"))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Codex tool call has no name"))?;
    let arguments = if is_custom {
        let input = item
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or_default();
        serde_json::json!({ "input": input }).to_string()
    } else {
        item.get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string()
    };
    Ok(serde_json::json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    }))
}

/// Close the run of tool calls that has been accumulating, if any.
///
/// Responses lists every call as its own item; Chat Completions expects one
/// assistant message carrying the whole `tool_calls` array, with the matching
/// `role: "tool"` results after it. Buffering keeps parallel calls — which
/// Codex requests via `parallel_tool_calls` — in the order a gateway accepts.
fn flush_tool_calls(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": Value::Array(std::mem::take(pending))
    }));
}

/// Translate a Codex Responses request into a Chat Completions request.
///
/// Returns the request body, the map the response direction needs to undo the
/// tool rewrites, and a count of the items dropped as non-portable.
fn adapt_request(
    payload: &Value,
    model: &str,
) -> Result<(Value, ToolMap, BTreeMap<String, usize>)> {
    let mut messages = Vec::<Value>::new();
    let mut tools = Vec::<Value>::new();
    let mut map = ToolMap::default();
    let mut dropped = BTreeMap::<String, usize>::new();
    let mut pending = Vec::<Value>::new();

    match payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(serde_json::json!({"role":"user","content":text}))
        }
        Some(Value::Array(items)) => {
            for item in items {
                let item_type = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                match item_type {
                    "additional_tools" => {
                        let declared =
                            item.get("tools").and_then(Value::as_array).ok_or_else(|| {
                                anyhow::anyhow!("Codex additional_tools item has no tools array")
                            })?;
                        for tool in declared {
                            push_tool(tool, None, &mut tools, &mut map)?;
                        }
                    }
                    "message" => {
                        flush_tool_calls(&mut messages, &mut pending);
                        let role = match item.get("role").and_then(Value::as_str).unwrap_or("user")
                        {
                            "developer" => "system",
                            role => role,
                        };
                        let content = text_content(item.get("content").ok_or_else(|| {
                            anyhow::anyhow!("Codex message item has no content")
                        })?)?;
                        messages.push(serde_json::json!({"role": role, "content": content}));
                    }
                    "custom_tool_call" | "function_call" => {
                        pending.push(adapt_tool_call(item, item_type == "custom_tool_call")?);
                    }
                    "custom_tool_call_output" | "function_call_output" => {
                        flush_tool_calls(&mut messages, &mut pending);
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("Codex {item_type} has no call_id"))?;
                        // A custom output is an array of text parts, a function
                        // output a bare string; `text_content` reads both.
                        let output = text_content(item.get("output").unwrap_or(&Value::Null))?;
                        messages.push(serde_json::json!({
                            "role": "tool", "tool_call_id": call_id, "content": output
                        }));
                    }
                    // Backend-owned state that cannot cross to another
                    // provider: `reasoning` carries `encrypted_content` sealed
                    // by the ChatGPT backend, and the compaction markers are
                    // Codex's own transcript bookkeeping. These are dropped —
                    // but counted and reported by the caller, never in silence.
                    "reasoning" | "compaction" | "compaction_trigger" => {
                        *dropped.entry(item_type.to_string()).or_default() += 1;
                    }
                    other => anyhow::bail!(
                        "Codex side adapter does not support Responses item type '{other}'"
                    ),
                }
            }
        }
        _ => anyhow::bail!("Codex Responses input is missing or unsupported"),
    }
    flush_tool_calls(&mut messages, &mut pending);

    let mut request_body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    if !tools.is_empty() {
        request_body["tools"] = Value::Array(tools);
        if let Some(choice) = payload.get("tool_choice") {
            request_body["tool_choice"] = choice.clone();
        }
        if let Some(parallel) = payload.get("parallel_tool_calls") {
            request_body["parallel_tool_calls"] = parallel.clone();
        }
    }
    Ok((request_body, map, dropped))
}

/// One tool call assembled from the gateway's streamed `tool_calls` deltas.
#[derive(Debug, Default, Clone, PartialEq)]
struct ChatToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Rebuild the Responses item for a completed tool call, undoing the rewrites
/// `push_tool` applied so Codex sees the tool it actually declared.
fn responses_call_item(item_id: &str, call: &ChatToolCall, map: &ToolMap) -> Value {
    let name = map
        .flattened
        .get(&call.name)
        .cloned()
        .unwrap_or_else(|| call.name.clone());
    if map.custom.contains(&name) {
        // Unwrap the one-string schema. If the model ignored it and emitted raw
        // text, that text IS the input — passing it through keeps the call
        // executable instead of failing on a technicality.
        let input = serde_json::from_str::<Value>(&call.arguments)
            .ok()
            .and_then(|args| {
                args.get("input")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| call.arguments.clone());
        serde_json::json!({
            "id": item_id, "type": "custom_tool_call", "status": "completed",
            "call_id": call.id, "name": name, "input": input
        })
    } else {
        serde_json::json!({
            "id": item_id, "type": "function_call", "status": "completed",
            "call_id": call.id, "name": name, "arguments": call.arguments
        })
    }
}

/// Fold one streamed `tool_calls` delta array into the calls assembled so far.
fn absorb_tool_call_deltas(deltas: &[Value], calls: &mut BTreeMap<u64, ChatToolCall>) {
    for delta in deltas {
        let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0);
        let call = calls.entry(index).or_default();
        if let Some(id) = delta.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            call.id = id.to_string();
        }
        if let Some(name) = delta.pointer("/function/name").and_then(Value::as_str) {
            call.name.push_str(name);
        }
        if let Some(arguments) = delta.pointer("/function/arguments").and_then(Value::as_str) {
            call.arguments.push_str(arguments);
        }
    }
}

/// P5 compatibility adapter for gateways whose provider backend exposes only
/// Chat Completions. Text and tool round-trips are translated in both
/// directions; provider-owned continuation items are dropped with a report, and
/// anything genuinely unknown fails explicitly instead of passing silently.
async fn forward_side_chat_adapter(
    state: &Arc<AppState>,
    payload: Value,
    model: &str,
) -> Result<Response> {
    // Keep the model selected by Codex in the synthesized Responses objects.
    // `model` is the replacement provider model sent to Chat Completions; it
    // must not leak back to Codex as its active native model.
    let client_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-5.4")
        .to_string();
    let (request_body, tool_map, dropped) = adapt_request(&payload, model)?;
    if !dropped.is_empty() {
        let summary = dropped
            .iter()
            .map(|(kind, n)| format!("{kind}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[llm-proxy] codex side adapter dropped non-portable items (not sendable to a non-ChatGPT backend): {summary}"
        );
    }
    let base = state
        .upstream_base_url
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .trim_end_matches('/')
        .to_string();
    let key = state
        .upstream_key
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let upstream = state
        .http_stream
        .post(format!("{base}/chat/completions"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("x-bf-vk", key)
        .json(&request_body)
        .send()
        .await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())?;
    if !status.is_success() {
        let text = upstream.text().await.unwrap_or_default();
        anyhow::bail!("Side Chat Completions adapter upstream error (HTTP {status}): {text}");
    }
    let input_stream = upstream.bytes_stream();
    let output = async_stream::stream! {
        let id = format!("resp_mhd_{}", crate::providers::now_unix_ms());
        // Codex validates Responses item ids when it feeds the synthesized
        // response back into the next turn. In particular, message items must
        // use the `msg_` namespace; arbitrary proxy-local ids are rejected.
        let item_id = format!("msg_{}", crate::providers::now_unix_ms());
        let frame = |event: &str, data: Value| bytes::Bytes::from(format!("event: {event}\ndata: {data}\n\n"));
        yield Ok::<bytes::Bytes, std::io::Error>(frame("response.created", serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":id,"object":"response","status":"in_progress","model":client_model.clone(),"output":[]}})));
        let mut buf = String::new();
        let mut full_text = String::new();
        let mut sequence = 1u64;
        // The message item opens on the FIRST text delta rather than up front:
        // a turn that only calls a tool produces no text at all, and an empty
        // message item ahead of the call would read to Codex as the model
        // having answered.
        let mut text_started = false;
        let mut next_output_index = 0u64;
        let mut message_index = 0u64;
        let mut calls: BTreeMap<u64, ChatToolCall> = BTreeMap::new();
        // Chat Completions usage rides the final `include_usage` trailer chunk
        // (an empty `choices` array), which the delta scanner below skips — so
        // capture it here as it streams by and fold it into the synthesized
        // `response.completed` below. This keeps ONE extraction point for both
        // the native tap and the side adapter.
        let mut usage_input: Option<u64> = None;
        let mut usage_output: Option<u64> = None;
        let mut usage_cached: Option<u64> = None;
        futures_util::pin_mut!(input_stream);
        while let Some(chunk) = input_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(std::io::Error::other(e)); break; } };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf.drain(..=pos).collect::<String>();
                let data = line.trim().strip_prefix("data: ").unwrap_or("");
                if data.is_empty() || data == "[DONE]" { continue; }
                let Ok(event): Result<Value, _> = serde_json::from_str(data) else { continue };
                if let Some(u) = event.get("usage") {
                    usage_input = u.get("prompt_tokens").and_then(Value::as_u64).or(usage_input);
                    usage_output = u.get("completion_tokens").and_then(Value::as_u64).or(usage_output);
                    usage_cached = u
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .or(usage_cached);
                }
                if let Some(deltas) = event.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array) {
                    absorb_tool_call_deltas(deltas, &mut calls);
                }
                if let Some(text) = event.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                    if !text_started {
                        text_started = true;
                        message_index = next_output_index;
                        next_output_index += 1;
                        yield Ok(frame("response.output_item.added", serde_json::json!({"type":"response.output_item.added","sequence_number":sequence,"output_index":message_index,"item":{"id":item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}}))); sequence += 1;
                        yield Ok(frame("response.content_part.added", serde_json::json!({"type":"response.content_part.added","sequence_number":sequence,"item_id":item_id,"output_index":message_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}))); sequence += 1;
                    }
                    full_text.push_str(text);
                    yield Ok(frame("response.output_text.delta", serde_json::json!({"type":"response.output_text.delta","sequence_number":sequence,"delta":text,"item_id":item_id,"output_index":message_index,"content_index":0})));
                    sequence += 1;
                }
            }
        }
        let mut outputs = Vec::<Value>::new();
        if text_started {
            let part = serde_json::json!({"type":"output_text","text":full_text,"annotations":[]});
            yield Ok(frame("response.output_text.done", serde_json::json!({"type":"response.output_text.done","sequence_number":sequence,"text":full_text,"item_id":item_id,"output_index":message_index,"content_index":0}))); sequence += 1;
            yield Ok(frame("response.content_part.done", serde_json::json!({"type":"response.content_part.done","sequence_number":sequence,"item_id":item_id,"output_index":message_index,"content_index":0,"part":part}))); sequence += 1;
            let item = serde_json::json!({"id":item_id,"type":"message","status":"completed","role":"assistant","content":[part]});
            yield Ok(frame("response.output_item.done", serde_json::json!({"type":"response.output_item.done","sequence_number":sequence,"output_index":message_index,"item":item}))); sequence += 1;
            outputs.push(item);
        }
        // Tool calls arrive as argument fragments spread over many chunks, so
        // they can only be announced once the stream has ended and each call is
        // whole. Codex reads the item from `output_item.done` and
        // `response.completed` alike, so emitting both keeps it consistent.
        for (index, call) in &calls {
            let call_item_id = format!("{id}_call_{index}");
            let item = responses_call_item(&call_item_id, call, &tool_map);
            let output_index = next_output_index;
            next_output_index += 1;
            let mut opening = item.clone();
            opening["status"] = Value::String("in_progress".to_string());
            yield Ok(frame("response.output_item.added", serde_json::json!({"type":"response.output_item.added","sequence_number":sequence,"output_index":output_index,"item":opening}))); sequence += 1;
            yield Ok(frame("response.output_item.done", serde_json::json!({"type":"response.output_item.done","sequence_number":sequence,"output_index":output_index,"item":item}))); sequence += 1;
            outputs.push(item);
        }
        let mut completed = serde_json::json!({"type":"response.completed","sequence_number":sequence,"response":{"id":id,"object":"response","status":"completed","model":client_model,"output":outputs}});
        if let Some(usage) = responses_usage_from_chat(usage_input, usage_output, usage_cached) {
            completed["response"]["usage"] = usage;
        }
        yield Ok(frame("response.completed", completed));
    };
    Ok(Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(output))?)
}

/// Whether an incoming `/v1/models` request is a Codex discovery request.
pub fn is_codex_request(headers: &HeaderMap, query: Option<&str>) -> bool {
    headers.contains_key("authorization")
        && query.is_some_and(|query| query.contains("client_version="))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_round_trips_plain_and_zstd_payloads() {
        let payload = serde_json::json!({
            "model": "gpt-5.6-luna",
            "input": [{"type": "function_call_output", "call_id": "c", "output": "ok"}]
        });

        let plain = HeaderMap::new();
        let plain_body = encode_request(&plain, &payload).expect("plain encoding");
        assert_eq!(
            decode_request(&plain, &plain_body).expect("plain decoding"),
            payload
        );

        let mut zstd = HeaderMap::new();
        zstd.insert("content-encoding", HeaderValue::from_static("zstd"));
        let zstd_body = encode_request(&zstd, &payload).expect("zstd encoding");
        assert_eq!(
            decode_request(&zstd, &zstd_body).expect("zstd decoding"),
            payload
        );
    }

    #[test]
    fn parse_responses_usage_extracts_and_splits_cached_tokens() {
        let line = r#"data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"output_tokens":250,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":1250}}}"#;
        let (input, output, cache_read) = parse_responses_usage(line).expect("usage parsed");
        // `input_tokens` counts the TOTAL prompt, cached included; the trace `In`
        // column wants fresh tokens only, so the 800 cached are subtracted.
        assert_eq!(input, 200);
        assert_eq!(output, 250);
        assert_eq!(cache_read, Some(800));
    }

    #[test]
    fn parse_responses_usage_without_details_keeps_input_and_cache_read_none() {
        let line = r#"data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_1","object":"response","status":"completed","usage":{"input_tokens":123,"output_tokens":45}}}"#;
        let (input, output, cache_read) = parse_responses_usage(line).expect("usage parsed");
        // No `input_tokens_details` -> cache_read stays None and input is
        // unchanged (nothing was reported as served from the cache).
        assert_eq!(input, 123);
        assert_eq!(output, 45);
        assert_eq!(cache_read, None);
    }

    #[test]
    fn parse_responses_usage_ignores_garbage_and_other_events() {
        // Not valid JSON at all.
        assert_eq!(parse_responses_usage("data: not json {{{"), None);
        // A well-formed event of another type — nothing to record.
        assert_eq!(
            parse_responses_usage(r#"data: {"type":"response.output_text.delta","delta":"hi"}"#),
            None
        );
        // A `response.completed` with no usage object — fail open to no usage.
        assert_eq!(
            parse_responses_usage(
                r#"data: {"type":"response.completed","response":{"id":"resp_1"}}"#
            ),
            None
        );
        // A line without the `data:` prefix is not an SSE data line.
        assert_eq!(
            parse_responses_usage(r#"{"type":"response.completed","response":{"id":"resp_1"}}"#),
            None
        );
    }

    #[test]
    fn side_completed_carries_usage_when_reported_and_omits_when_not() {
        // Gateway reported full usage -> Responses-shaped object with the split.
        let usage = responses_usage_from_chat(Some(10), Some(5), Some(3)).expect("usage present");
        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 15);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 3);
        // Cached tokens not reported -> no `input_tokens_details` key at all.
        let usage = responses_usage_from_chat(Some(10), Some(5), None).expect("usage present");
        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 15);
        assert!(usage.get("input_tokens_details").is_none());
        // No usage at all -> the key is omitted entirely, never zeros.
        assert!(responses_usage_from_chat(None, None, None).is_none());
        // Malformed trailer (output missing) -> treated as no usage.
        assert!(responses_usage_from_chat(Some(10), None, Some(3)).is_none());
    }

    /// The `additional_tools` item as Codex actually sends it: one grammar-bound
    /// custom tool, one plain function, one namespace wrapping a function.
    /// Shapes copied from the captured corpus, descriptions abridged.
    fn additional_tools_item() -> Value {
        serde_json::json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": [
                {
                    "type": "custom",
                    "name": "exec",
                    "description": "Run JavaScript code to orchestrate tool calls",
                    "format": {"type": "grammar", "syntax": "lark", "definition": "start: SOURCE"}
                },
                {
                    "type": "function",
                    "name": "wait",
                    "description": "Waits on a yielded exec cell.",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {"cell_id": {"type": "string"}},
                        "required": ["cell_id"],
                        "additionalProperties": false
                    }
                },
                {
                    "type": "namespace",
                    "name": "collaboration",
                    "description": "Tools for spawning sub-agents.",
                    "tools": [{
                        "type": "function",
                        "name": "followup_task",
                        "description": "Send a follow-up task.",
                        "parameters": {"type": "object", "properties": {}}
                    }]
                }
            ]
        })
    }

    #[test]
    fn side_adapter_translates_every_tool_shape_codex_sends() {
        let payload = serde_json::json!({"input": [additional_tools_item()]});
        let (request, map, _) = adapt_request(&payload, "some-model").expect("adapted");
        let tools = request["tools"].as_array().expect("tools present");
        assert_eq!(tools.len(), 3, "namespace child is flattened, not dropped");

        // custom -> function over one raw-text property, and the grammar rides
        // along in the description because there is nowhere else to put it.
        assert_eq!(tools[0]["function"]["name"], "exec");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["input"]["type"],
            "string"
        );
        let exec_desc = tools[0]["function"]["description"].as_str().unwrap();
        assert!(
            exec_desc.contains("lark grammar"),
            "grammar syntax survives"
        );
        assert!(exec_desc.contains("start: SOURCE"), "grammar body survives");
        assert!(map.custom.contains("exec"));

        // function -> passthrough, `strict` included so a gateway that honours
        // it sees the same contract Codex declared.
        assert_eq!(tools[1]["function"]["name"], "wait");
        assert_eq!(tools[1]["function"]["strict"], false);
        assert_eq!(tools[1]["function"]["parameters"]["required"][0], "cell_id");

        // namespace -> flattened child, with the reverse mapping recorded.
        assert_eq!(tools[2]["function"]["name"], "collaboration__followup_task");
        assert_eq!(
            map.flattened.get("collaboration__followup_task").unwrap(),
            "followup_task"
        );
    }

    #[test]
    fn side_adapter_round_trips_a_custom_tool_call_and_its_output() {
        let payload = serde_json::json!({
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "input": [
                additional_tools_item(),
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "be brief"}]},
                {"type": "custom_tool_call", "call_id": "call_1", "name": "exec",
                 "input": "const r = await tools.shell_command({});"},
                {"type": "custom_tool_call_output", "call_id": "call_1",
                 "output": [{"type": "input_text", "text": "Exit code: 0"}]}
            ]
        });
        let (request, map, _) = adapt_request(&payload, "some-model").expect("adapted");
        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);

        // `developer` is Responses' name for the system role.
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be brief");

        // The raw text lands under the one property the tool schema declares.
        assert_eq!(messages[1]["role"], "assistant");
        let call = &messages[1]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["function"]["name"], "exec");
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap())
            .expect("arguments are JSON");
        assert_eq!(args["input"], "const r = await tools.shell_command({});");

        // The result comes back as a tool message bound to the same call id.
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["content"], "Exit code: 0");

        // Choice and parallelism are forwarded only alongside a tool list.
        assert_eq!(request["tool_choice"], "auto");
        assert_eq!(request["parallel_tool_calls"], true);

        // And the way back: the gateway answers with a plain function call,
        // which must reach Codex as the custom tool it originally declared.
        let call = ChatToolCall {
            id: "call_9".to_string(),
            name: "exec".to_string(),
            arguments: r#"{"input":"console.log(1)"}"#.to_string(),
        };
        let item = responses_call_item("item_9", &call, &map);
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["name"], "exec");
        assert_eq!(item["call_id"], "call_9");
        assert_eq!(item["input"], "console.log(1)", "unwrapped, not JSON");
    }

    #[test]
    fn side_adapter_passes_plain_function_calls_through_untouched() {
        let payload = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "call_2", "name": "wait",
                 "arguments": "{\"cell_id\":\"220\"}"},
                // A function output is a bare string, unlike a custom one.
                {"type": "function_call_output", "call_id": "call_2",
                 "output": "Script running with cell ID 220"}
            ]
        });
        let (request, _, _) = adapt_request(&payload, "some-model").expect("adapted");
        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"], "{\"cell_id\":\"220\"}",
            "JSON arguments are forwarded verbatim, not re-wrapped"
        );
        assert_eq!(messages[1]["content"], "Script running with cell ID 220");
        // No tools declared in this turn -> no tools key, and no tool_choice.
        assert!(request.get("tools").is_none());
        assert!(request.get("tool_choice").is_none());
    }

    #[test]
    fn side_adapter_groups_parallel_calls_into_one_assistant_message() {
        // Responses lists each call separately; Chat Completions wants them in
        // a single assistant message, or a gateway rejects the sequence.
        let payload = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "a", "name": "wait", "arguments": "{}"},
                {"type": "function_call", "call_id": "b", "name": "wait", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "a", "output": "one"},
                {"type": "function_call_output", "call_id": "b", "output": "two"}
            ]
        });
        let (request, _, _) = adapt_request(&payload, "some-model").expect("adapted");
        let messages = request["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "one assistant turn, then two results");
        assert_eq!(messages[0]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(messages[1]["tool_call_id"], "a");
        assert_eq!(messages[2]["tool_call_id"], "b");
    }

    #[test]
    fn side_adapter_reports_backend_owned_items_instead_of_dropping_them_silently() {
        let payload = serde_json::json!({
            "input": [
                {"type": "reasoning", "encrypted_content": "gAAAA...", "summary": []},
                {"type": "reasoning", "encrypted_content": "gAAAB...", "summary": []},
                {"type": "compaction"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });
        let (request, _, dropped) = adapt_request(&payload, "some-model").expect("adapted");
        // They cannot cross to another backend, so they do not travel — but the
        // caller gets a count to log rather than silence.
        assert_eq!(dropped.get("reasoning"), Some(&2));
        assert_eq!(dropped.get("compaction"), Some(&1));
        assert_eq!(request["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn side_adapter_refuses_shapes_it_cannot_translate() {
        // An unknown item type must fail loudly, not slip through as text.
        let payload = serde_json::json!({
            "input": [{"type": "computer_call", "call_id": "c"}]
        });
        assert!(adapt_request(&payload, "m").is_err());

        // Same for an unknown tool type: a tool list the model cannot use
        // correctly is worse than a failed request.
        let payload = serde_json::json!({
            "input": [{"type": "additional_tools", "role": "developer",
                       "tools": [{"type": "computer_use_preview", "name": "computer"}]}]
        });
        assert!(adapt_request(&payload, "m").is_err());

        // And for non-text content parts.
        let payload = serde_json::json!({
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_image", "image_url": "..."}]}]
        });
        assert!(adapt_request(&payload, "m").is_err());
    }

    #[test]
    fn tool_call_deltas_assemble_across_chunk_boundaries() {
        // Gateways send the name once and the arguments in fragments, and
        // parallel calls interleave by `index`.
        let mut calls = BTreeMap::new();
        absorb_tool_call_deltas(
            &[
                serde_json::json!({"index":0,"id":"call_a","function":{"name":"exec","arguments":"{\"in"}}),
            ],
            &mut calls,
        );
        absorb_tool_call_deltas(
            &[
                serde_json::json!({"index":1,"id":"call_b","function":{"name":"wait","arguments":"{}"}}),
            ],
            &mut calls,
        );
        absorb_tool_call_deltas(
            &[serde_json::json!({"index":0,"function":{"arguments":"put\":\"x\"}"}})],
            &mut calls,
        );
        assert_eq!(calls[&0].id, "call_a");
        assert_eq!(calls[&0].name, "exec");
        assert_eq!(calls[&0].arguments, r#"{"input":"x"}"#);
        assert_eq!(calls[&1].name, "wait");
    }

    #[test]
    fn responses_call_item_restores_namespaced_names_and_survives_bad_arguments() {
        let mut map = ToolMap::default();
        map.flattened.insert(
            "collaboration__followup_task".to_string(),
            "followup_task".to_string(),
        );
        map.custom.insert("exec".to_string());

        let call = ChatToolCall {
            id: "call_1".to_string(),
            name: "collaboration__followup_task".to_string(),
            arguments: "{\"target\":\"a\"}".to_string(),
        };
        let item = responses_call_item("i1", &call, &map);
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "followup_task", "namespace prefix removed");
        assert_eq!(item["arguments"], "{\"target\":\"a\"}");

        // A model that ignored the one-string schema and emitted raw source:
        // that text IS the input, so the call stays executable.
        let call = ChatToolCall {
            id: "call_2".to_string(),
            name: "exec".to_string(),
            arguments: "console.log(1)".to_string(),
        };
        let item = responses_call_item("i2", &call, &map);
        assert_eq!(item["type"], "custom_tool_call");
        assert_eq!(item["input"], "console.log(1)");
    }

    /// Conformance run against real captured Codex bodies.
    ///
    /// Opt-in: point `MHD_CODEX_CORPUS_DIR` at a directory of decompressed
    /// Responses request bodies (`*.json`) exported from `corpus-codex.db`. The
    /// bodies carry real prompts and are therefore never committed, so the test
    /// skips when the variable is unset — which is the normal case in CI.
    ///
    /// What it protects: the unit tests above assert the shapes we KNOW about.
    /// This one fails the moment a live Codex build sends an item or tool type
    /// the adapter has never seen, which is the failure mode that would
    /// otherwise surface as a broken session.
    #[test]
    fn side_adapter_translates_the_captured_corpus() {
        let Ok(dir) = std::env::var("MHD_CODEX_CORPUS_DIR") else {
            return;
        };
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).expect("corpus dir readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).expect("body readable");
            let payload: Value = serde_json::from_str(&raw).expect("body is JSON");
            let (request, map, _) = match adapt_request(&payload, "test-model") {
                Ok(adapted) => adapted,
                Err(e) => panic!("{} failed to adapt: {e}", path.display()),
            };

            // Every tool call in the history must reach the gateway paired with
            // its result, or the gateway rejects the conversation outright.
            let messages = request["messages"].as_array().expect("messages");
            let mut call_ids = HashSet::new();
            for message in messages {
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        call_ids.insert(call["id"].as_str().unwrap_or_default().to_string());
                    }
                }
            }
            for message in messages {
                if message["role"] == "tool" {
                    let id = message["tool_call_id"].as_str().unwrap_or_default();
                    assert!(
                        call_ids.contains(id),
                        "{}: tool result {id} has no matching call",
                        path.display()
                    );
                }
            }
            // A body that declared tools must still declare them afterwards.
            let declared_tools = payload["input"]
                .as_array()
                .map(|items| items.iter().any(|i| i["type"] == "additional_tools"))
                .unwrap_or(false);
            if declared_tools {
                assert!(
                    request["tools"].as_array().is_some_and(|t| !t.is_empty()),
                    "{}: tools were declared but none were forwarded",
                    path.display()
                );
                assert!(
                    !map.custom.is_empty() || !map.flattened.is_empty(),
                    "{}: no tool rewrite recorded, so the response direction \
                     would return calls in the wrong shape",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "no bodies found in {dir}");
        eprintln!("adapted {checked} captured Codex bodies");
    }
}
