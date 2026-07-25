//! Upstream provider — the OpenAI-compatible gateway (SVA / Bifrost) used for
//! the sonnet and haiku tiers.
//!
//! Flow: Anthropic request → transform to OpenAI → override model with the
//! configured upstream id → POST to `{base_url}/chat/completions` → transform
//! the OpenAI response back to Anthropic format for Claude Code.

use anyhow::Result;
use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

use crate::state::AppState;
use crate::transform;

use super::{InflightGuard, now_ms};

/// Build an authenticated POST to the upstream `/chat/completions` endpoint,
/// send it, check for a non-2xx status, and return the response plus the URL
/// (for logging). Every upstream request path needs this exact ritual.
async fn post_chat_completions(
    state: &Arc<AppState>,
    payload: &Value,
    streaming: bool,
) -> Result<(reqwest::Response, String)> {
    let base_url = state.upstream_base_url.read().unwrap_or_else(|e| e.into_inner()).clone();
    let api_key = state.upstream_key.read().unwrap_or_else(|e| e.into_inner()).clone();
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let client = if streaming { &state.http_stream } else { &state.http };
    let req_builder = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(payload);

    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_connect() {
                "CONNECT_FAILED"
            } else if e.is_timeout() {
                "TIMEOUT"
            } else {
                "SEND_ERR"
            };
            eprintln!("[llm-proxy] upstream SEND_ERROR {kind}: {e}");
            state.log_line(&format!("{} upstream SEND_ERROR {kind}: {e}", now_ms()));
            return Err(e.into());
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Upstream error (HTTP {}): {}", status, body);
    }
    Ok((resp, url))
}

/// Send an Anthropic-format request to the upstream gateway, forcing the given
/// upstream model id.
pub async fn send_request(
    state: &Arc<AppState>,
    req_id: u64,
    payload: Value,
    target_model: &str,
) -> Result<Value> {
    let debug = state.log_level.read().unwrap_or_else(|e| e.into_inner()).dump_bodies();

    // Anthropic → OpenAI, then force the real upstream model id.
    let mut openai_payload = transform::anthropic_to_openai(payload.clone());
    if let Value::Object(ref mut map) = openai_payload {
        map.insert("model".to_string(), Value::String(target_model.to_string()));
    }

    if debug {
        let base_url = state.upstream_base_url.read().unwrap_or_else(|e| e.into_inner()).clone();
        eprintln!(
            "[llm-proxy] upstream request → {} (model={target_model}):\n{}",
            base_url,
            serde_json::to_string_pretty(&openai_payload)?
        );
    }

    // Observability: log timing + concurrency under `maximal`. This is how you
    // confirm whether parallel requests are queueing behind one another.
    let log = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_errors();
    let _guard = InflightGuard::new(state.clone(), req_id);
    let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    if log {
        state.log_line(&format!(
            "{} #{req_id} upstream START model={target_model} inflight={inflight}",
            now_ms()
        ));
    }
    let detailed = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_detailed();
    if detailed {
        state.log_line(&format!(
            "{} #{req_id} upstream REQ {}",
            now_ms(),
            super::summarize_payload(&payload)
        ));
    }

    let (resp, _url) = match post_chat_completions(state, &openai_payload, false).await {
        Ok(v) => v,
        Err(e) => {
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                "UPSTREAM_ERR",
            );
            return Err(e);
        }
    };
    let status_opt = Some(resp.status().as_u16());

    // Hang detection: if the upstream was slow to respond with headers,
    // log a structured HANG event.
    const HANG_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(5);
    if started.elapsed() >= HANG_THRESHOLD {
        state.log_event(crate::db_log::LogEvent {
            seq: req_id,
            event_type: "HANG".to_string(),
            target_model: Some(target_model.to_string()),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..Default::default()
        });
    }

    let openai_resp: Value = resp.json().await?;
    if log {
        state.log_line(&format!(
            "{} #{req_id} upstream DONE after {} ms (inflight now {})",
            now_ms(),
            started.elapsed().as_millis(),
            state.inflight.load(std::sync::atomic::Ordering::SeqCst) - 1
        ));
    }
    // Extract cache info from the original OpenAI response before transforming.
    // OpenAI prompt_tokens = fresh + cached, so we subtract to get each bucket.
    // A missing prompt_tokens_details object means the upstream said nothing
    // about caching — that stays None (SQL NULL), never 0.
    let cached_tokens = openai_resp
        .get("usage")
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|n| n.as_u64());
    // The actual model the upstream served (may differ from target_model).
    let upstream_model = openai_resp
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    let mut anthropic_resp = transform::openai_to_anthropic(openai_resp);
    if let Some(obj) = anthropic_resp.as_object_mut() {
        obj.insert("id".to_string(), Value::String(super::synth_msg_id()));
        obj.insert(
            "model".to_string(),
            Value::String(target_model.to_string()),
        );
    }

    // Push token usage into the trace entry with correctly separated counts.
    if let Some(usage) = anthropic_resp.get("usage") {
        let prompt_total = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        // Always close the row on success (zero tokens included) so the
        // InflightGuard's Drop doesn't mislabel a 200 as CANCELLED.
        // When the cache bucket is unknown, no subtraction happens and
        // input_tokens carries the whole prompt — flagged as unknown, not zero.
        // cache_creation is None on every OpenAI route: no such wire field exists.
        let fresh_input = prompt_total.saturating_sub(cached_tokens.unwrap_or(0));
        state.update_trace_tokens(
            req_id,
            fresh_input,
            output_tokens,
            cached_tokens,
            None,
            Some(started.elapsed().as_millis() as u64),
            status_opt,
            upstream_model.as_deref(),
        );
    }

    if detailed {
        let stop_reason = anthropic_resp
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let usage = anthropic_resp
            .get("usage")
            .map(|u| u.to_string())
            .unwrap_or_default();
        state.log_line(&format!(
            "{} #{req_id} upstream RESP stop={} usage={}",
            now_ms(),
            stop_reason,
            usage
        ));
    }

    if debug {
        eprintln!(
            "[llm-proxy] upstream response (as Anthropic):\n{}",
            serde_json::to_string_pretty(&anthropic_resp)?
        );
    }

    Ok(anthropic_resp)
}

/// OpenAI → Anthropic SSE translator state machine.
///
/// Tracks content-block indices, lazily opens/closes blocks for thinking,
/// text, and tool calls, and accumulates token counts. Designed to be usable
/// from both the production [`stream_request`] body and from unit tests.
struct SseTranslator {
    next_index: i64,
    open_index: Option<i64>,
    text_index: Option<i64>,
    thinking_index: Option<i64>,
    tool_map: HashMap<i64, i64>,
    stop_reason: String,
    output_tokens: u64,
    input_tokens: u64,
    /// None until a chunk carries `prompt_tokens_details.cached_tokens`. Stays
    /// None on upstreams that never report caching, so the request row keeps
    /// "no cache field seen" distinct from "cache field said zero".
    cached_tokens: Option<u64>,
    any_tool: bool,
}

impl SseTranslator {
    fn new() -> Self {
        Self {
            next_index: 0,
            open_index: None,
            text_index: None,
            thinking_index: None,
            tool_map: HashMap::new(),
            stop_reason: "end_turn".to_string(),
            output_tokens: 0,
            input_tokens: 0,
            cached_tokens: None,
            any_tool: false,
        }
    }

    /// Process one parsed OpenAI SSE chunk (the full JSON value and its
    /// `choices[0]`), returning Anthropic SSE frames to emit.
    /// Capture token usage from a chunk. The OpenAI `include_usage` trailer
    /// arrives in a chunk with an **empty `choices` array**, so this must be
    /// called for every chunk *before* the `choices` guard skips it — otherwise
    /// `input_tokens`/`cached_tokens` are never read (issue: Reason colour never
    /// updated, In/Out columns stayed blank on streaming responses).
    fn observe_usage(&mut self, v: &Value) {
        let Some(u) = v.get("usage").filter(|u| !u.is_null()) else {
            return;
        };
        if let Some(x) = u.get("completion_tokens").and_then(|x| x.as_u64()) {
            self.output_tokens = x;
        }
        if let Some(x) = u.get("prompt_tokens").and_then(|x| x.as_u64()) {
            self.input_tokens = x;
        }
        if let Some(x) = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|x| x.as_u64())
        {
            self.cached_tokens = Some(x);
        }
    }

    fn process_chunk(&mut self, _v: &Value, choice: &Value) -> Vec<Bytes> {
        let mut frames = Vec::new();

        // ── reasoning delta (thinking) ────────────────────────────
        if let Some(rc) = choice
            .get("delta")
            .and_then(|d| d.get("reasoning_content"))
            .and_then(|t| t.as_str())
        {
            if !rc.is_empty() {
                if self.thinking_index.is_none() {
                    if let Some(oi) = self.open_index {
                        frames.push(sse(
                            "content_block_stop",
                            &json!({
                                "type": "content_block_stop", "index": oi
                            }),
                        ));
                    }
                    let idx = self.next_index;
                    self.next_index += 1;
                    self.thinking_index = Some(idx);
                    self.open_index = Some(idx);
                    frames.push(sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start", "index": idx,
                            "content_block": { "type": "thinking", "thinking": "" }
                        }),
                    ));
                }
                let idx = self.thinking_index.unwrap();
                frames.push(sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta", "index": idx,
                        "delta": { "type": "thinking_delta", "thinking": rc }
                    }),
                ));
            }
        }

        // ── text delta ────────────────────────────────────────────
        if let Some(text) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(|t| t.as_str())
        {
            if !text.is_empty() {
                if self.text_index.is_none() {
                    if let Some(oi) = self.open_index {
                        if Some(oi) == self.thinking_index {
                            frames.push(sse(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta", "index": oi,
                                    "delta": {
                                        "type": "signature_delta",
                                        "signature": transform::SYNTHETIC_THINKING_SIGNATURE
                                    }
                                }),
                            ));
                        }
                        frames.push(sse(
                            "content_block_stop",
                            &json!({
                                "type": "content_block_stop", "index": oi
                            }),
                        ));
                    }
                    let idx = self.next_index;
                    self.next_index += 1;
                    self.text_index = Some(idx);
                    self.open_index = Some(idx);
                    frames.push(sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start", "index": idx,
                            "content_block": { "type": "text", "text": "" }
                        }),
                    ));
                }
                let idx = self.text_index.unwrap();
                frames.push(sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta", "index": idx,
                        "delta": { "type": "text_delta", "text": text }
                    }),
                ));
            }
        }

        // ── tool_call deltas ──────────────────────────────────────
        if let Some(tcs) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for tc in tcs {
                let ti = tc.get("index").and_then(|x| x.as_i64()).unwrap_or(0);
                let func = tc.get("function");

                let anth_idx = match self.tool_map.get(&ti) {
                    Some(&x) => x,
                    None => {
                        if let Some(oi) = self.open_index {
                            if Some(oi) == self.thinking_index {
                                frames.push(sse(
                                    "content_block_delta",
                                    &json!({
                                        "type": "content_block_delta", "index": oi,
                                        "delta": {
                                            "type": "signature_delta",
                                            "signature": transform::SYNTHETIC_THINKING_SIGNATURE
                                        }
                                    }),
                                ));
                            }
                            frames.push(sse(
                                "content_block_stop",
                                &json!({
                                    "type": "content_block_stop", "index": oi
                                }),
                            ));
                        }
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.tool_map.insert(ti, idx);
                        self.open_index = Some(idx);
                        self.any_tool = true;
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        frames.push(sse(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": {
                                    "type": "tool_use", "id": id, "name": name, "input": {}
                                }
                            }),
                        ));
                        idx
                    }
                };

                if let Some(args) = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                {
                    if !args.is_empty() {
                        frames.push(sse(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta", "index": anth_idx,
                                "delta": { "type": "input_json_delta", "partial_json": args }
                            }),
                        ));
                    }
                }
            }
        }

        // ── finish_reason ─────────────────────────────────────────
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.stop_reason = match fr {
                "length" => "max_tokens",
                "tool_calls" => "tool_use",
                "stop" => "end_turn",
                other => other,
            }
            .to_string();
        }

        frames
    }

    /// Return the closing SSE frames that should follow the final chunk.
    fn finalize(&mut self, had_error: bool) -> Vec<Bytes> {
        let mut frames = Vec::new();

        // Close any open content block.
        match self.open_index {
            Some(oi) => {
                if Some(oi) == self.thinking_index {
                    frames.push(sse(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta", "index": oi,
                            "delta": {
                                "type": "signature_delta",
                                "signature": transform::SYNTHETIC_THINKING_SIGNATURE
                            }
                        }),
                    ));
                }
                frames.push(sse(
                    "content_block_stop",
                    &json!({
                        "type": "content_block_stop", "index": oi
                    }),
                ));
            }
            None => {
                // No content at all — emit an empty text block so the message
                // is well-formed.
                frames.push(sse(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start", "index": 0,
                        "content_block": { "type": "text", "text": "" }
                    }),
                ));
                frames.push(sse(
                    "content_block_stop",
                    &json!({
                        "type": "content_block_stop", "index": 0
                    }),
                ));
            }
        }

        if self.any_tool {
            self.stop_reason = "tool_use".to_string();
        }

        // On transport error signal an error rather than silently claiming
        // end_turn, so Claude Code can surface/retry.
        let final_stop_reason = if had_error {
            "error"
        } else {
            &self.stop_reason
        };

        frames.push(sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": final_stop_reason, "stop_sequence": null },
                "usage": { "input_tokens": self.input_tokens, "output_tokens": self.output_tokens }
            }),
        ));

        if !had_error {
            frames.push(sse("message_stop", &json!({"type": "message_stop"})));
        }

        frames
    }
}

/// Streaming variant — sends `stream: true` to the upstream, then converts the
/// OpenAI SSE stream into Anthropic SSE events on the fly so Claude Code can
/// consume it. Text deltas only for now (tool-call deltas are a follow-up).
pub async fn stream_request(
    state: &Arc<AppState>,
    req_id: u64,
    payload: Value,
    target_model: &str,
    requested_model: &str,
) -> Result<Body> {
    let mut openai_payload = transform::anthropic_to_openai(payload.clone());
    if let Value::Object(ref mut map) = openai_payload {
        map.insert("model".to_string(), Value::String(target_model.to_string()));
        map.insert("stream".to_string(), Value::Bool(true));
        // Ask the gateway to include token usage in the final chunk if it can.
        map.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    // Observability: streaming is the path Claude Code actually uses. Log the
    // concurrency at send time so overlapping parallel requests are visible.
    let log = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_errors();
    let started = std::time::Instant::now();
    // Guard is moved into the stream below, so the in-flight count stays
    // elevated for the full duration of the stream (and decrements on drop even
    // if the client disconnects mid-stream).
    let guard = InflightGuard::new(state.clone(), req_id);
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} stream START model={target_model} inflight={inflight}",
            now_ms()
        ));
    }
    let detailed = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_detailed();
    if detailed {
        state.log_line(&format!(
            "{} #{req_id} stream REQ {}",
            now_ms(),
            super::summarize_payload(&payload)
        ));
    }

    let (resp, _url) = match post_chat_completions(state, &openai_payload, true).await {
        Ok(v) => v,
        Err(e) => {
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                "UPSTREAM_ERR",
            );
            return Err(e);
        }
    };
    let status_opt = Some(resp.status().as_u16());

    // ── Hang detection ───────────────────────────────────────────
    // If headers took longer than HANG_THRESHOLD to come back, log a
    // structured HANG event so repeat-offending upstream models are visible.
    const HANG_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(5);
    if started.elapsed() >= HANG_THRESHOLD {
        state.log_event(crate::db_log::LogEvent {
            seq: req_id,
            event_type: "HANG".to_string(),
            target_model: Some(target_model.to_string()),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            ..Default::default()
        });
    }

    if log {
        state.log_line(&format!(
            "{} #{req_id} stream headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }

    let requested_model = requested_model.to_string();
    let msg_id = super::synth_msg_id();
    let mut byte_stream = resp.bytes_stream();

    // Clone the Arc so the stream has its own handle to log DONE/ERROR.
    let state_for_log = state.clone();

    let s = async_stream::stream! {
        // Hold the in-flight guard for the lifetime of the stream.
        let _guard = guard;

        // ── opening events ──────────────────────────────────────────
        let start = json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": requested_model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        });
        yield Ok::<Bytes, std::io::Error>(sse("message_start", &start));
        yield Ok(sse("ping", &json!({ "type": "ping" })));

        // ── translate OpenAI chunks ─────────────────────────────────
        let mut translator = SseTranslator::new();
        // Accumulate raw bytes, not a String, so UTF-8 sequences split
        // across TCP chunks are not corrupted (H3). We split on the `\n`
        // *byte* and decode each complete line separately.
        let mut buf: Vec<u8> = Vec::new();
        let mut had_error = false;

        while let Some(item) = byte_stream.next().await {
            let chunk = match item { Ok(c) => c, Err(_) => { had_error = true; break } };
            buf.extend_from_slice(&chunk);

            // Process complete lines (split on byte \n).
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw).trim().to_string();

                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };

                // Capture usage first: the `include_usage` trailer rides a chunk
                // with an empty `choices` array, which the guard below skips.
                translator.observe_usage(&v);

                let Some(choice) = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                else { continue };

                for frame in translator.process_chunk(&v, choice) {
                    yield Ok(frame);
                }
            }
        }

        // ── end-of-stream log ──────────────────────────────────────
        // Push final token counts into the trace entry with correctly separated
        // counts. OpenAI prompt_tokens = fresh + cached, so we subtract.
        let prompt_total = translator.input_tokens;
        let out_tok = translator.output_tokens;
        let cache_read = translator.cached_tokens;
        let fresh_in = prompt_total.saturating_sub(cache_read.unwrap_or(0));
        let elapsed = started.elapsed().as_millis() as u64;
        if had_error {
            state_for_log.mark_request_failed(
                req_id,
                Some(elapsed),
                status_opt,
                "stream transport error",
                "STREAM_ERR",
            );
        } else {
            state_for_log.update_trace_tokens(
                req_id,
                fresh_in,
                out_tok,
                cache_read,
                None,
                Some(elapsed),
                status_opt,
                None,
            );
        }

        if log {
            if had_error {
                state_for_log.log_line(&format!(
                    "{} #{req_id} stream ERROR after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            } else {
                state_for_log.log_line(&format!(
                    "{} #{req_id} stream DONE after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            }
        }

        // ── closing events ──────────────────────────────────────────
        for frame in translator.finalize(had_error) {
            yield Ok(frame);
        }
    };

    Ok(Body::from_stream(s))
}

// Wall-clock timestamp helper and the in-flight guard live in `providers::mod`.

/// Format a single Anthropic SSE event frame.
fn sse(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// Forward a raw OpenAI-format request straight to the upstream (no transform).
/// Used by the `/v1/chat/completions` endpoint for OpenAI-native clients.
pub async fn send_raw_openai(state: &Arc<AppState>, req_id: u64, payload: Value) -> Result<Value> {
    let started = std::time::Instant::now();
    let (resp, _url) = match post_chat_completions(state, &payload, false).await {
        Ok(v) => v,
        Err(e) => {
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                "UPSTREAM_ERR",
            );
            return Err(e);
        }
    };
    let status_opt = Some(resp.status().as_u16());
    let body: Value = resp.json().await?;
    let upstream_model = body
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    // Fill the trace entry's token counts from the OpenAI usage block
    // (prompt_tokens/completion_tokens, with input_/output_ as a fallback).
    // OpenAI prompt_tokens = fresh + cached, so we subtract to isolate each bucket.
    if let Some(usage) = body.get("usage").filter(|u| !u.is_null()) {
        let prompt_total = usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let out = usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|n| n.as_u64());
        // Always close the row on success (zero tokens included).
        let fresh_in = prompt_total.saturating_sub(cache_read.unwrap_or(0));
        state.update_trace_tokens(
            req_id,
            fresh_in,
            out,
            cache_read,
            None,
            Some(started.elapsed().as_millis() as u64),
            status_opt,
            upstream_model.as_deref(),
        );
    }

    Ok(body)
}

/// Streaming variant — sends `stream: true` to the upstream, then forwards all
/// SSE bytes through unchanged (no Anthropic transform). Used by the
/// `/v1/chat/completions` endpoint for OpenAI-native streaming clients.
pub async fn stream_raw_openai(
    state: &Arc<AppState>,
    req_id: u64,
    mut payload: Value,
) -> Result<Body> {
    // Inject stream=true and stream_options so the upstream sends SSE.
    if let Value::Object(ref mut map) = payload {
        map.insert("stream".to_string(), Value::Bool(true));
        map.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    let log = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_errors();
    let started = std::time::Instant::now();
    let guard = InflightGuard::new(state.clone(), req_id);
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} stream-openai START inflight={}",
            now_ms(),
            inflight
        ));
    }

    let (resp, _url) = match post_chat_completions(state, &payload, true).await {
        Ok(v) => v,
        Err(e) => {
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                "UPSTREAM_ERR",
            );
            return Err(e);
        }
    };
    let status_opt = Some(resp.status().as_u16());

    if log {
        state.log_line(&format!(
            "{} #{req_id} stream-openai headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }

    let mut byte_stream = resp.bytes_stream();
    let state_for_log = state.clone();

    let s = async_stream::stream! {
        let _guard = guard;
        let mut had_error = false;

        // Parallel line buffer used only to observe the OpenAI `usage` block.
        // The raw upstream bytes are forwarded unchanged — this never alters
        // what the client receives.
        let mut scan: Vec<u8> = Vec::new();
        let mut in_tok: u64 = 0;
        let mut out_tok: u64 = 0;
        let mut cached_tok: Option<u64> = None;

        while let Some(item) = byte_stream.next().await {
            let chunk = match item { Ok(c) => c, Err(_) => { had_error = true; break } };
            // Forward verbatim (Bytes clone is a cheap refcount bump).
            yield Ok::<Bytes, std::io::Error>(chunk.clone());

            // Observe: scan complete `data:` lines for a usage object.
            scan.extend_from_slice(&chunk);
            while let Some(pos) = scan.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = scan.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
                    if let Some(n) = usage
                        .get("prompt_tokens")
                        .or_else(|| usage.get("input_tokens"))
                        .and_then(|n| n.as_u64())
                    {
                        in_tok = n;
                    }
                    if let Some(n) = usage
                        .get("completion_tokens")
                        .or_else(|| usage.get("output_tokens"))
                        .and_then(|n| n.as_u64())
                    {
                        out_tok = n;
                    }
                    if let Some(n) = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|n| n.as_u64())
                    {
                        cached_tok = Some(n);
                    }
                }
            }
        }

        // Push final token counts into the trace entry with correctly separated
        // counts. OpenAI prompt_tokens = fresh + cached, so we subtract.
        let elapsed = started.elapsed().as_millis() as u64;
        if had_error {
            state_for_log.mark_request_failed(
                req_id,
                Some(elapsed),
                status_opt,
                "stream transport error",
                "STREAM_ERR",
            );
        } else {
            // Success path: always close the row (zero tokens included).
            let fresh_in = in_tok.saturating_sub(cached_tok.unwrap_or(0));
            state_for_log.update_trace_tokens(
                req_id,
                fresh_in,
                out_tok,
                cached_tok,
                None,
                Some(elapsed),
                status_opt,
                None,
            );
        }

        if log {
            if had_error {
                state_for_log.log_line(&format!(
                    "{} #{req_id} stream-openai ERROR after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            } else {
                state_for_log.log_line(&format!(
                    "{} #{req_id} stream-openai DONE after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            }
        }
    };

    Ok(Body::from_stream(s))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one SSE frame Bytes into `(event_name, data_json_string)` for
    /// assertion.  The tests call the *real* [`SseTranslator`] so they test
    /// the exact code that runs in production — no parallel copy to drift.
    fn parse_frame(frame: &Bytes) -> (String, String) {
        let s = String::from_utf8_lossy(frame);
        let mut lines = s.lines().filter(|l| !l.is_empty());
        let event = lines
            .next()
            .and_then(|l| l.strip_prefix("event: "))
            .unwrap_or("")
            .to_string();
        let data = lines
            .next()
            .and_then(|l| l.strip_prefix("data: "))
            .unwrap_or("")
            .to_string();
        (event, data)
    }

    /// Parse a sequence of SSE frame Bytes into `(event_name, Value)` pairs.
    fn parse_events(frames: &[Bytes]) -> Vec<(String, Value)> {
        frames
            .iter()
            .map(|f| {
                let (ev, data_str) = parse_frame(f);
                let data = serde_json::from_str(&data_str).unwrap_or(Value::Null);
                (ev, data)
            })
            .collect()
    }

    /// Shortcut: create a choice-containing chunk for the translator and return
    /// the parsed events.
    fn process_events(t: &mut SseTranslator, chunk_json: Value) -> Vec<(String, Value)> {
        let frames = t.process_chunk(&chunk_json, &chunk_json["choices"][0]);
        parse_events(&frames)
    }

    #[test]
    fn test_sse_formatting() {
        let data = json!({"type": "ping"});
        let result = sse("ping", &data);
        assert_eq!(
            String::from_utf8_lossy(&result),
            "event: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
    }

    #[test]
    fn test_sse_with_complex_data() {
        let data = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        let result = sse("content_block_start", &data);
        let s = String::from_utf8_lossy(&result);
        assert!(s.starts_with("event: content_block_start\n"));
        assert!(s.contains("\"type\":\"content_block_start\""));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn test_finish_reason_mapping() {
        // The finish_reason mapping used in stream_request
        fn map_finish_reason(fr: &str) -> &str {
            match fr {
                "length" => "max_tokens",
                "tool_calls" => "tool_use",
                "stop" => "end_turn",
                other => other,
            }
        }

        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("content_filter"), "content_filter");
    }

    // ── reasoning_content → thinking block tests ───────────────────

    #[test]
    fn test_reasoning_content_opens_thinking_block() {
        let mut t = SseTranslator::new();
        let chunk = json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "Let me think step by step."
                }
            }]
        });

        let events = process_events(&mut t, chunk);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[0].1["content_block"]["type"], "thinking");
        assert_eq!(events[1].0, "content_block_delta");
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[1].1["delta"]["type"], "thinking_delta");
        assert_eq!(
            events[1].1["delta"]["thinking"],
            "Let me think step by step."
        );
    }

    #[test]
    fn test_reasoning_content_multiple_deltas() {
        let mut t = SseTranslator::new();

        // First delta: open the thinking block
        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": "Step 1. "}}]
            }),
        );
        assert_eq!(events.len(), 2); // start + delta
        assert_eq!(events[0].0, "content_block_start");

        // Second delta: same block, no new start
        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": "Step 2. "}}]
            }),
        );
        assert_eq!(events.len(), 1); // just delta
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["thinking"], "Step 2. ");
    }

    #[test]
    fn test_empty_reasoning_content_skipped() {
        let mut t = SseTranslator::new();

        // Empty reasoning content should not open a block
        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": ""}}]
            }),
        );
        assert!(events.is_empty());

        // Non-empty after empty should still work
        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": "Now thinking."}}]
            }),
        );
        assert_eq!(events.len(), 2); // start + delta
        assert_eq!(events[0].1["content_block"]["type"], "thinking");
    }

    #[test]
    fn test_reasoning_then_text_transition() {
        let mut t = SseTranslator::new();
        let mut all_events = Vec::new();

        // Reasoning chunk
        all_events.extend(process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": "Thinking..."}}]
            }),
        ));

        // Text chunk — should close thinking block, emit signature delta, open text block
        all_events.extend(process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Answer."}}]
            }),
        ));

        // Events: signature_delta for thinking, content_block_stop for thinking,
        //         content_block_start for text, content_block_delta for text
        assert_eq!(all_events.len(), 6);
        // Check the transition events
        assert_eq!(all_events[2].0, "content_block_delta");
        assert_eq!(all_events[2].1["delta"]["type"], "signature_delta");
        assert_eq!(all_events[3].0, "content_block_stop");
        assert_eq!(all_events[3].1["index"], 0);
        assert_eq!(all_events[4].0, "content_block_start");
        assert_eq!(all_events[4].1["index"], 1);
        assert_eq!(all_events[4].1["content_block"]["type"], "text");
        assert_eq!(all_events[5].1["index"], 1);
    }

    #[test]
    fn test_text_delta_opens_text_block() {
        let mut t = SseTranslator::new();

        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Hello!"}}]
            }),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["content_block"]["type"], "text");
        assert_eq!(events[1].0, "content_block_delta");
        assert_eq!(events[1].1["delta"]["text"], "Hello!");
    }

    #[test]
    fn test_multiple_text_deltas() {
        let mut t = SseTranslator::new();

        let _ = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Hello "}}]
            }),
        );

        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "world!"}}]
            }),
        );
        // Only a delta, no new start
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["text"], "world!");
        assert_eq!(events[0].1["index"], 0);
    }

    #[test]
    fn test_empty_text_delta_skipped() {
        let mut t = SseTranslator::new();

        let events = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": ""}}]
            }),
        );
        assert!(events.is_empty());
    }

    // ── Tool call tests ───────────────────────────────────────────

    #[test]
    fn test_tool_call_start() {
        let mut t = SseTranslator::new();

        let events = process_events(
            &mut t,
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_abc123",
                            "function": {
                                "name": "get_weather",
                                "arguments": ""
                            }
                        }]
                    }
                }]
            }),
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["content_block"]["type"], "tool_use");
        assert_eq!(events[0].1["content_block"]["id"], "call_abc123");
        assert_eq!(events[0].1["content_block"]["name"], "get_weather");
    }

    #[test]
    fn test_tool_call_with_arguments() {
        let mut t = SseTranslator::new();

        // Start the tool call
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_abc",
                            "function": {"name": "search", "arguments": ""}
                        }]
                    }
                }]
            }),
        );

        // Stream arguments
        let events = process_events(
            &mut t,
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "{\"q\":\"test\"}"}
                        }]
                    }
                }]
            }),
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[0].1["delta"]["partial_json"], "{\"q\":\"test\"}");
    }

    #[test]
    fn test_multiple_tool_calls() {
        let mut t = SseTranslator::new();

        let events = process_events(
            &mut t,
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [
                            {"index": 0, "id": "call_0", "function": {"name": "fn_a", "arguments": ""}},
                            {"index": 1, "id": "call_1", "function": {"name": "fn_b", "arguments": ""}}
                        ]
                    }
                }]
            }),
        );

        // First call opens block 0; second call closes block 0 then opens block 1.
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[1].0, "content_block_stop");
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[2].0, "content_block_start");
        assert_eq!(events[2].1["index"], 1);
    }

    // ── Finalize / closing events ──────────────────────────────────

    fn finalize_events(t: &mut SseTranslator, had_error: bool) -> Vec<(String, Value)> {
        let frames = t.finalize(had_error);
        parse_events(&frames)
    }

    #[test]
    fn test_finalize_with_open_text_block() {
        let mut t = SseTranslator::new();
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Hello"}}]
            }),
        );

        let events = finalize_events(&mut t, false);
        // content_block_stop + message_delta + message_stop
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[1].0, "message_delta");
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[1].1["usage"]["input_tokens"], 0);
        assert_eq!(events[2].0, "message_stop");
    }

    #[test]
    fn test_finalize_no_content_emits_empty_text_block() {
        let mut t = SseTranslator::new();
        let events = finalize_events(&mut t, false);
        // content_block_start + content_block_stop + message_delta + message_stop
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[0].1["content_block"]["type"], "text");
        assert_eq!(events[1].0, "content_block_stop");
    }

    #[test]
    fn test_finalize_with_tool_call_sets_stop_reason_tool_use() {
        let mut t = SseTranslator::new();
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_x",
                            "function": {"name": "fn", "arguments": ""}
                        }]
                    }
                }]
            }),
        );

        let events = finalize_events(&mut t, false);
        let (_, msg_delta) = &events[1];
        assert_eq!(msg_delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_finalize_uses_input_tokens() {
        let mut t = SseTranslator::new();
        // Feed a chunk that carries usage with prompt_tokens.
        // In production, observe_usage is called before process_chunk so that
        // the `include_usage` trailer (empty choices) still populates the counts.
        let chunk = json!({
            "choices": [{"delta": {"content": "Hi"}}],
            "usage": { "prompt_tokens": 42, "completion_tokens": 7 }
        });
        t.observe_usage(&chunk);
        let _ = t.process_chunk(&chunk, &chunk["choices"][0]);

        let events = finalize_events(&mut t, false);
        assert_eq!(events[1].1["usage"]["input_tokens"], 42);
        assert_eq!(events[1].1["usage"]["output_tokens"], 7);
    }

    #[test]
    fn test_finalize_with_error_sets_stop_reason_error() {
        let mut t = SseTranslator::new();
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Partial"}}]
            }),
        );

        let events = finalize_events(&mut t, true);
        // content_block_stop + message_delta (no message_stop)
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].1["delta"]["stop_reason"], "error");
    }

    #[test]
    fn test_finalize_with_error_omits_message_stop() {
        let mut t = SseTranslator::new();
        let events = finalize_events(&mut t, true);
        assert_eq!(events.len(), 3); // empty block + message_delta (no message_stop)
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[1].0, "content_block_stop");
        assert_eq!(events[2].0, "message_delta");
        assert_eq!(events[2].1["delta"]["stop_reason"], "error");
        // No message_stop event
        assert!(events.iter().all(|(ev, _)| *ev != "message_stop"));
    }

    // ── Full reasoning + text + finalize integration ───────────────

    #[test]
    fn test_reasoning_to_text_integration() {
        let mut t = SseTranslator::new();

        // 1. Reasoning
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"reasoning_content": "Think..."}}]
            }),
        );

        // 2. Text
        let _ = process_events(
            &mut t,
            json!({
                "choices": [{"delta": {"content": "Answer"}}]
            }),
        );

        // 3. Finalize
        let events = finalize_events(&mut t, false);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], 1); // text block index
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
    }
}
