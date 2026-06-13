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
use serde_json::{json, Value};
use std::sync::Arc;

use crate::state::AppState;
use crate::transform;

use super::{now_ms, InflightGuard};

/// Send an Anthropic-format request to the upstream gateway, forcing the given
/// upstream model id.
pub async fn send_request(
    state: &Arc<AppState>,
    payload: Value,
    target_model: &str,
) -> Result<Value> {
    let base_url = state.upstream_base_url.read().unwrap().clone();
    let api_key = state.upstream_key.read().unwrap().clone();
    let debug = state.log_level.read().unwrap().dump_bodies();

    // Anthropic → OpenAI, then force the real upstream model id.
    let mut openai_payload = transform::anthropic_to_openai(payload);
    if let Value::Object(ref mut map) = openai_payload {
        map.insert("model".to_string(), Value::String(target_model.to_string()));
    }

    if debug {
        eprintln!(
            "[llm-proxy] upstream request → {} (model={target_model}):\n{}",
            base_url,
            serde_json::to_string_pretty(&openai_payload)?
        );
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    // Observability: log timing + concurrency under `maximal`. This is how you
    // confirm whether parallel requests are queueing behind one another.
    let log = state.log_level.read().unwrap().log_errors();
    let req_id = state.next_req_id();
    let _guard = InflightGuard::new(state.clone());
    let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    if log {
        state.log_line(&format!(
            "{} #{req_id} upstream START model={target_model} inflight={inflight}",
            now_ms()
        ));
    }

    let resp = state
        .http
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(&openai_payload)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if log {
            state.log_line(&format!(
                "{} #{req_id} upstream ERROR {} after {} ms",
                now_ms(),
                status,
                started.elapsed().as_millis()
            ));
        }
        anyhow::bail!("Upstream error (HTTP {}): {}", status, body);
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
    let anthropic_resp = transform::openai_to_anthropic(openai_resp);

    if debug {
        eprintln!(
            "[llm-proxy] upstream response (as Anthropic):\n{}",
            serde_json::to_string_pretty(&anthropic_resp)?
        );
    }

    Ok(anthropic_resp)
}

/// Streaming variant — sends `stream: true` to the upstream, then converts the
/// OpenAI SSE stream into Anthropic SSE events on the fly so Claude Code can
/// consume it. Text deltas only for now (tool-call deltas are a follow-up).
pub async fn stream_request(
    state: &Arc<AppState>,
    payload: Value,
    target_model: &str,
    requested_model: &str,
) -> Result<Body> {
    let base_url = state.upstream_base_url.read().unwrap().clone();
    let api_key = state.upstream_key.read().unwrap().clone();

    let mut openai_payload = transform::anthropic_to_openai(payload);
    if let Value::Object(ref mut map) = openai_payload {
        map.insert("model".to_string(), Value::String(target_model.to_string()));
        map.insert("stream".to_string(), Value::Bool(true));
        // Ask the gateway to include token usage in the final chunk if it can.
        map.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    // Observability: streaming is the path Claude Code actually uses. Log the
    // concurrency at send time so overlapping parallel requests are visible.
    let log = state.log_level.read().unwrap().log_errors();
    let req_id = state.next_req_id();
    let started = std::time::Instant::now();
    // Guard is moved into the stream below, so the in-flight count stays
    // elevated for the full duration of the stream (and decrements on drop even
    // if the client disconnects mid-stream).
    let guard = InflightGuard::new(state.clone());
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} stream START model={target_model} inflight={inflight}",
            now_ms()
        ));
    }

    let resp = state
        .http
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(&openai_payload)
        .send()
        .await?;

    if log {
        state.log_line(&format!(
            "{} #{req_id} stream headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Upstream error (HTTP {}): {}", status, body);
    }

    let requested_model = requested_model.to_string();
    let msg_id = format!(
        "msg_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
    );
    let mut byte_stream = resp.bytes_stream();

    // Clone the Arc so the stream has its own handle to log DONE/ERROR.
    let state_for_log = state.clone();

    let s = async_stream::stream! {
        use std::collections::HashMap;

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
        let mut buf = String::new();
        let mut stop_reason = "end_turn".to_string();
        let mut output_tokens: u64 = 0;
        let mut had_error = false;

        // Content-block bookkeeping. Blocks are opened lazily so text and tool
        // calls each get their own Anthropic index; the previously open block is
        // always closed before a new one starts.
        let mut next_index: i64 = 0;
        let mut open_index: Option<i64> = None;
        let mut text_index: Option<i64> = None;
        let mut thinking_index: Option<i64> = None;
        let mut tool_map: HashMap<i64, i64> = HashMap::new(); // openai tool idx → anthropic idx
        let mut any_tool = false;

        while let Some(item) = byte_stream.next().await {
            let chunk = match item { Ok(c) => c, Err(_) => { had_error = true; break } };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete lines.
            while let Some(pos) = buf.find('\n') {
                let line: String = buf[..pos].trim().to_string();
                buf.drain(..=pos);

                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };

                if let Some(u) = v.get("usage") {
                    if let Some(x) = u.get("completion_tokens").and_then(|x| x.as_u64()) {
                        output_tokens = x;
                    }
                }

                let Some(choice) = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                else { continue };

                // ── reasoning delta (thinking) ──────────────────────
                // Thinking-mode upstreams stream their chain-of-thought in
                // `reasoning_content` before the answer. Surface it as an
                // Anthropic `thinking` block so Claude Code stores and echoes
                // it back (the upstream rejects turns that drop it).
                if let Some(rc) = choice
                    .get("delta")
                    .and_then(|d| d.get("reasoning_content"))
                    .and_then(|t| t.as_str())
                {
                    if !rc.is_empty() {
                        if thinking_index.is_none() {
                            if let Some(oi) = open_index {
                                yield Ok(sse("content_block_stop", &json!({
                                    "type": "content_block_stop", "index": oi
                                })));
                            }
                            let idx = next_index; next_index += 1;
                            thinking_index = Some(idx); open_index = Some(idx);
                            yield Ok(sse("content_block_start", &json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": { "type": "thinking", "thinking": "" }
                            })));
                        }
                        let idx = thinking_index.unwrap();
                        yield Ok(sse("content_block_delta", &json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": { "type": "thinking_delta", "thinking": rc }
                        })));
                    }
                }

                // ── text delta ──────────────────────────────────────
                if let Some(text) = choice
                    .get("delta")
                    .and_then(|d| d.get("content"))
                    .and_then(|t| t.as_str())
                {
                    if !text.is_empty() {
                        if text_index.is_none() {
                            if let Some(oi) = open_index {
                                if Some(oi) == thinking_index {
                                    yield Ok(sse("content_block_delta", &json!({
                                        "type": "content_block_delta", "index": oi,
                                        "delta": {
                                            "type": "signature_delta",
                                            "signature": crate::transform::SYNTHETIC_THINKING_SIGNATURE
                                        }
                                    })));
                                }
                                yield Ok(sse("content_block_stop", &json!({
                                    "type": "content_block_stop", "index": oi
                                })));
                            }
                            let idx = next_index; next_index += 1;
                            text_index = Some(idx); open_index = Some(idx);
                            yield Ok(sse("content_block_start", &json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": { "type": "text", "text": "" }
                            })));
                        }
                        let idx = text_index.unwrap();
                        yield Ok(sse("content_block_delta", &json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": { "type": "text_delta", "text": text }
                        })));
                    }
                }

                // ── tool_call deltas ────────────────────────────────
                if let Some(tcs) = choice
                    .get("delta")
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|t| t.as_array())
                {
                    for tc in tcs {
                        let ti = tc.get("index").and_then(|x| x.as_i64()).unwrap_or(0);
                        let func = tc.get("function");

                        let anth_idx = match tool_map.get(&ti) {
                            Some(&x) => x,
                            None => {
                                if let Some(oi) = open_index {
                                    if Some(oi) == thinking_index {
                                        yield Ok(sse("content_block_delta", &json!({
                                            "type": "content_block_delta", "index": oi,
                                            "delta": {
                                                "type": "signature_delta",
                                                "signature": crate::transform::SYNTHETIC_THINKING_SIGNATURE
                                            }
                                        })));
                                    }
                                    yield Ok(sse("content_block_stop", &json!({
                                        "type": "content_block_stop", "index": oi
                                    })));
                                }
                                let idx = next_index; next_index += 1;
                                tool_map.insert(ti, idx);
                                open_index = Some(idx);
                                any_tool = true;
                                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                let name = func
                                    .and_then(|f| f.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                yield Ok(sse("content_block_start", &json!({
                                    "type": "content_block_start", "index": idx,
                                    "content_block": {
                                        "type": "tool_use", "id": id, "name": name, "input": {}
                                    }
                                })));
                                idx
                            }
                        };

                        if let Some(args) = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                        {
                            if !args.is_empty() {
                                yield Ok(sse("content_block_delta", &json!({
                                    "type": "content_block_delta", "index": anth_idx,
                                    "delta": { "type": "input_json_delta", "partial_json": args }
                                })));
                            }
                        }
                    }
                }

                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    stop_reason = match fr {
                        "length" => "max_tokens",
                        "tool_calls" => "tool_use",
                        "stop" => "end_turn",
                        other => other,
                    }.to_string();
                }
            }
        }

        // ── end-of-stream log ──────────────────────────────────────
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
        match open_index {
            Some(oi) => {
                if Some(oi) == thinking_index {
                    yield Ok(sse("content_block_delta", &json!({
                        "type": "content_block_delta", "index": oi,
                        "delta": {
                            "type": "signature_delta",
                            "signature": crate::transform::SYNTHETIC_THINKING_SIGNATURE
                        }
                    })));
                }
                yield Ok(sse("content_block_stop", &json!({
                    "type": "content_block_stop", "index": oi
                })));
            }
            None => {
                // No content at all — emit an empty text block so the message
                // is well-formed.
                yield Ok(sse("content_block_start", &json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" }
                })));
                yield Ok(sse("content_block_stop", &json!({
                    "type": "content_block_stop", "index": 0
                })));
            }
        }

        if any_tool { stop_reason = "tool_use".to_string(); }

        yield Ok(sse("message_delta", &json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": null },
            "usage": { "output_tokens": output_tokens }
        })));
        yield Ok(sse("message_stop", &json!({ "type": "message_stop" })));
    };

    Ok(Body::from_stream(s))
}

/// Wall-clock timestamp helper and the in-flight guard live in `providers::mod`.

/// Format a single Anthropic SSE event frame.
fn sse(event: &str, data: &Value) -> Bytes {    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// Forward a raw OpenAI-format request straight to the upstream (no transform).
/// Used by the `/v1/chat/completions` endpoint for OpenAI-native clients.
pub async fn send_raw_openai(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let base_url = state.upstream_base_url.read().unwrap().clone();
    let api_key = state.upstream_key.read().unwrap().clone();

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let resp = state
        .http
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Upstream error (HTTP {}): {}", status, body);
    }

    Ok(resp.json().await?)
}
