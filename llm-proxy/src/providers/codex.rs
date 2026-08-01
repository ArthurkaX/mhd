//! Native Codex (ChatGPT OAuth) Responses passthrough.
//!
//! This module deliberately does not parse or transform the Responses body.
//! Codex sends zstd-compressed JSON and expects an SSE response; preserving the
//! bytes and the negotiated headers is the safest native path.

use anyhow::Result;
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use std::sync::Arc;

use crate::state::AppState;
use serde_json::Value;

const CODEX_ORIGIN: &str = "https://chatgpt.com/backend-api/codex";

/// Decode a Codex JSON request for local corpus analysis while retaining the
/// original bytes for native forwarding.
pub fn decode_request(incoming: &HeaderMap, body: &[u8]) -> Result<Value> {
    let decoded = if incoming.get("content-encoding").and_then(|v| v.to_str().ok()) == Some("zstd") {
        zstd::decode_all(body)?
    } else {
        body.to_vec()
    };
    Ok(serde_json::from_slice(&decoded)?)
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

/// P5 compatibility adapter for gateways whose provider backend exposes only
/// Chat Completions. Text-only turns are translated losslessly; tool and
/// provider-owned continuation items fail explicitly instead of being dropped.
async fn forward_side_chat_adapter(
    state: &Arc<AppState>,
    payload: Value,
    model: &str,
) -> Result<Response> {
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
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let mut messages = Vec::<Value>::new();
    match input {
        Value::String(text) => messages.push(serde_json::json!({"role":"user","content":text})),
        Value::Array(items) => {
            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("message");
                if matches!(item_type, "reasoning" | "response.output_text" | "summary_text" | "additional_tools" | "item_reference") {
                    continue;
                }
                let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                    "developer" => "system",
                    role => role,
                };
                if item_type != "message" || item.get("content").is_none()
                {
                    anyhow::bail!("Codex side adapter does not support Responses item type '{item_type}'")
                }
                let content = text_content(item.get("content").unwrap_or(&Value::Null))?;
                messages.push(serde_json::json!({"role": role, "content": content}));
            }
        }
        _ => anyhow::bail!("Codex Responses input is missing or unsupported"),
    }
    let request_body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    let base = state.upstream_base_url.read().unwrap_or_else(|e| e.into_inner()).trim_end_matches('/').to_string();
    let key = state.upstream_key.read().unwrap_or_else(|e| e.into_inner()).clone();
    let upstream = state.http_stream.post(format!("{base}/chat/completions"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("x-bf-vk", key)
        .json(&request_body)
        .send().await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())?;
    if !status.is_success() {
        let text = upstream.text().await.unwrap_or_default();
        anyhow::bail!("Side Chat Completions adapter upstream error (HTTP {status}): {text}");
    }
    let input_stream = upstream.bytes_stream();
    let output = async_stream::stream! {
        let id = format!("mhd_resp_{}", crate::providers::now_unix_ms());
        let item_id = format!("{id}_item");
        let frame = |event: &str, data: Value| bytes::Bytes::from(format!("event: {event}\ndata: {data}\n\n"));
        yield Ok::<bytes::Bytes, std::io::Error>(frame("response.created", serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":id,"object":"response","status":"in_progress","output":[]}})));
        yield Ok(frame("response.output_item.added", serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}})));
        yield Ok(frame("response.content_part.added", serde_json::json!({"type":"response.content_part.added","sequence_number":2,"item_id":item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
        let mut buf = String::new();
        let mut full_text = String::new();
        let mut sequence = 3u64;
        futures_util::pin_mut!(input_stream);
        while let Some(chunk) = input_stream.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(std::io::Error::other(e)); break; } };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf.drain(..=pos).collect::<String>();
                let data = line.trim().strip_prefix("data: ").unwrap_or("");
                if data.is_empty() || data == "[DONE]" { continue; }
                let Ok(event): Result<Value, _> = serde_json::from_str(data) else { continue };
                if let Some(text) = event.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                    full_text.push_str(text);
                    yield Ok(frame("response.output_text.delta", serde_json::json!({"type":"response.output_text.delta","sequence_number":sequence,"delta":text,"item_id":item_id,"output_index":0,"content_index":0})));
                    sequence += 1;
                }
            }
        }
        let part = serde_json::json!({"type":"output_text","text":full_text,"annotations":[]});
        yield Ok(frame("response.output_text.done", serde_json::json!({"type":"response.output_text.done","sequence_number":sequence,"text":full_text,"item_id":item_id,"output_index":0,"content_index":0}))); sequence += 1;
        yield Ok(frame("response.content_part.done", serde_json::json!({"type":"response.content_part.done","sequence_number":sequence,"item_id":item_id,"output_index":0,"content_index":0,"part":part}))); sequence += 1;
        let item = serde_json::json!({"id":item_id,"type":"message","status":"completed","role":"assistant","content":[part]});
        yield Ok(frame("response.output_item.done", serde_json::json!({"type":"response.output_item.done","sequence_number":sequence,"output_index":0,"item":item}))); sequence += 1;
        yield Ok(frame("response.completed", serde_json::json!({"type":"response.completed","sequence_number":sequence,"response":{"id":id,"object":"response","status":"completed","output":[item]}})));
    };
    Ok(Response::builder().status(status).header("content-type", "text/event-stream").body(Body::from_stream(output))?)
}

/// Whether an incoming `/v1/models` request is a Codex discovery request.
pub fn is_codex_request(headers: &HeaderMap, query: Option<&str>) -> bool {
    headers.contains_key("authorization")
        && query.is_some_and(|query| query.contains("client_version="))
}
