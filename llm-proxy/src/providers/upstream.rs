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
) -> Result<(reqwest::Response, String)> {
    let base_url = state.upstream_base_url.read().unwrap().clone();
    let api_key = state.upstream_key.read().unwrap().clone();
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let resp = state
        .http
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("content-type", "application/json")
        .json(payload)
        .send()
        .await?;

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
    payload: Value,
    target_model: &str,
) -> Result<Value> {
    let debug = state.log_level.read().unwrap().dump_bodies();

    // Anthropic → OpenAI, then force the real upstream model id.
    let mut openai_payload = transform::anthropic_to_openai(payload);
    if let Value::Object(ref mut map) = openai_payload {
        map.insert("model".to_string(), Value::String(target_model.to_string()));
    }

    if debug {
        let base_url = state.upstream_base_url.read().unwrap().clone();
        eprintln!(
            "[llm-proxy] upstream request → {} (model={target_model}):\n{}",
            base_url,
            serde_json::to_string_pretty(&openai_payload)?
        );
    }

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

    let (resp, _url) = post_chat_completions(state, &openai_payload).await?;
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
    let mut openai_payload = transform::anthropic_to_openai(payload);
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

    let (resp, _url) = post_chat_completions(state, &openai_payload).await?;

    if log {
        state.log_line(&format!(
            "{} #{req_id} stream headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
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
fn sse(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// Forward a raw OpenAI-format request straight to the upstream (no transform).
/// Used by the `/v1/chat/completions` endpoint for OpenAI-native clients.
pub async fn send_raw_openai(state: &Arc<AppState>, payload: Value) -> Result<Value> {
    let (resp, _url) = post_chat_completions(state, &payload).await?;
    Ok(resp.json().await?)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::SYNTHETIC_THINKING_SIGNATURE;

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

    /// A minimal replica of the OpenAI → Anthropic SSE chunk-processing logic
    /// from `stream_request`, extracted for focused unit testing.  We keep it
    /// in the test module so the production stream stays untouched while we
    /// cover the tricky state-machine paths (reasoning → thinking, tool calls,
    /// block open/close ordering).
    struct TestTranslator {
        next_index: i64,
        open_index: Option<i64>,
        text_index: Option<i64>,
        thinking_index: Option<i64>,
        tool_map: std::collections::HashMap<i64, i64>,
        stop_reason: String,
        output_tokens: u64,
    }

    impl TestTranslator {
        fn new() -> Self {
            Self {
                next_index: 0,
                open_index: None,
                text_index: None,
                thinking_index: None,
                tool_map: std::collections::HashMap::new(),
                stop_reason: "end_turn".to_string(),
                output_tokens: 0,
            }
        }

        /// Process one parsed OpenAI chunk and return Anthropic SSE `(event, data)` pairs.
        fn process(&mut self, choice: &Value) -> Vec<(&'static str, Value)> {
            let mut events = Vec::new();

            // ── reasoning delta (thinking) ──────────────────────────
            if let Some(rc) = choice
                .get("delta")
                .and_then(|d| d.get("reasoning_content"))
                .and_then(|t| t.as_str())
            {
                if !rc.is_empty() {
                    if self.thinking_index.is_none() {
                        if let Some(oi) = self.open_index {
                            events.push((
                                "content_block_stop",
                                json!({
                                    "type": "content_block_stop", "index": oi
                                }),
                            ));
                        }
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.thinking_index = Some(idx);
                        self.open_index = Some(idx);
                        events.push((
                            "content_block_start",
                            json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": { "type": "thinking", "thinking": "" }
                            }),
                        ));
                    }
                    let idx = self.thinking_index.unwrap();
                    events.push((
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": { "type": "thinking_delta", "thinking": rc }
                        }),
                    ));
                }
            }

            // ── text delta ──────────────────────────────────────────
            if let Some(text) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|t| t.as_str())
            {
                if !text.is_empty() {
                    if self.text_index.is_none() {
                        if let Some(oi) = self.open_index {
                            if Some(oi) == self.thinking_index {
                                events.push((
                                    "content_block_delta",
                                    json!({
                                        "type": "content_block_delta", "index": oi,
                                        "delta": {
                                            "type": "signature_delta",
                                            "signature": SYNTHETIC_THINKING_SIGNATURE
                                        }
                                    }),
                                ));
                            }
                            events.push((
                                "content_block_stop",
                                json!({
                                    "type": "content_block_stop", "index": oi
                                }),
                            ));
                        }
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.text_index = Some(idx);
                        self.open_index = Some(idx);
                        events.push((
                            "content_block_start",
                            json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": { "type": "text", "text": "" }
                            }),
                        ));
                    }
                    let idx = self.text_index.unwrap();
                    events.push((
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": { "type": "text_delta", "text": text }
                        }),
                    ));
                }
            }

            // ── tool_call deltas ────────────────────────────────────
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
                                    events.push((
                                        "content_block_delta",
                                        json!({
                                            "type": "content_block_delta", "index": oi,
                                            "delta": {
                                                "type": "signature_delta",
                                                "signature": SYNTHETIC_THINKING_SIGNATURE
                                            }
                                        }),
                                    ));
                                }
                                events.push((
                                    "content_block_stop",
                                    json!({
                                        "type": "content_block_stop", "index": oi
                                    }),
                                ));
                            }
                            let idx = self.next_index;
                            self.next_index += 1;
                            self.tool_map.insert(ti, idx);
                            self.open_index = Some(idx);
                            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            events.push((
                                "content_block_start",
                                json!({
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
                            events.push((
                                "content_block_delta",
                                json!({
                                    "type": "content_block_delta", "index": anth_idx,
                                    "delta": { "type": "input_json_delta", "partial_json": args }
                                }),
                            ));
                        }
                    }
                }
            }

            // ── finish_reason ───────────────────────────────────────
            if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                self.stop_reason = match fr {
                    "length" => "max_tokens",
                    "tool_calls" => "tool_use",
                    "stop" => "end_turn",
                    other => other,
                }
                .to_string();
            }

            events
        }

        /// Return the closing events that should follow the final chunk.
        fn finalize(&self) -> Vec<(&'static str, Value)> {
            let mut events = Vec::new();

            match self.open_index {
                Some(oi) => {
                    if Some(oi) == self.thinking_index {
                        events.push((
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta", "index": oi,
                                "delta": {
                                    "type": "signature_delta",
                                    "signature": SYNTHETIC_THINKING_SIGNATURE
                                }
                            }),
                        ));
                    }
                    events.push((
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop", "index": oi
                        }),
                    ));
                }
                None => {
                    events.push((
                        "content_block_start",
                        json!({
                            "type": "content_block_start", "index": 0,
                            "content_block": { "type": "text", "text": "" }
                        }),
                    ));
                    events.push((
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop", "index": 0
                        }),
                    ));
                }
            }

            let stop_reason = if self.tool_map.is_empty() {
                self.stop_reason.clone()
            } else {
                "tool_use".to_string()
            };

            events.push((
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                    "usage": { "output_tokens": self.output_tokens }
                }),
            ));
            events.push(("message_stop", json!({"type": "message_stop"})));

            events
        }
    }

    // ── reasoning_content → thinking block tests ───────────────────

    #[test]
    fn test_reasoning_content_opens_thinking_block() {
        let mut t = TestTranslator::new();
        let chunk = json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "Let me think step by step."
                }
            }]
        });

        let events = t.process(&chunk["choices"][0]);
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
        let mut t = TestTranslator::new();

        // First delta: open the thinking block
        let events = t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": "Step 1. "}}]
            })["choices"][0],
        );
        assert_eq!(events.len(), 2); // start + delta
        assert_eq!(events[0].0, "content_block_start");

        // Second delta: same block, no new start
        let events = t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": "Step 2. "}}]
            })["choices"][0],
        );
        assert_eq!(events.len(), 1); // just delta
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["thinking"], "Step 2. ");
        assert_eq!(t.thinking_index, Some(0)); // still index 0
    }

    #[test]
    fn test_empty_reasoning_content_skipped() {
        let mut t = TestTranslator::new();

        // Empty reasoning content should not open a block
        let events = t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": ""}}]
            })["choices"][0],
        );
        assert!(events.is_empty());

        // Non-empty after empty should still work
        let events = t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": "Now thinking."}}]
            })["choices"][0],
        );
        assert_eq!(events.len(), 2); // start + delta
        assert_eq!(events[0].1["content_block"]["type"], "thinking");
    }

    #[test]
    fn test_reasoning_then_text_transition() {
        let mut t = TestTranslator::new();
        let mut all_events = Vec::new();

        // Reasoning chunk
        all_events.extend(t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": "Thinking..."}}]
            })["choices"][0],
        ));
        assert_eq!(t.thinking_index, Some(0));
        assert_eq!(t.open_index, Some(0));

        // Text chunk — should close thinking block, emit signature delta, open text block
        all_events.extend(t.process(
            &json!({
                "choices": [{"delta": {"content": "Answer."}}]
            })["choices"][0],
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

        assert_eq!(t.text_index, Some(1));
        assert_eq!(t.open_index, Some(1));
        assert!(t.thinking_index.is_some()); // still tracked for close logic
    }

    #[test]
    fn test_text_delta_opens_text_block() {
        let mut t = TestTranslator::new();

        let events = t.process(
            &json!({
                "choices": [{"delta": {"content": "Hello!"}}]
            })["choices"][0],
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["content_block"]["type"], "text");
        assert_eq!(events[1].0, "content_block_delta");
        assert_eq!(events[1].1["delta"]["text"], "Hello!");
    }

    #[test]
    fn test_multiple_text_deltas() {
        let mut t = TestTranslator::new();

        let _ = t.process(
            &json!({
                "choices": [{"delta": {"content": "Hello "}}]
            })["choices"][0],
        );

        let events = t.process(
            &json!({
                "choices": [{"delta": {"content": "world!"}}]
            })["choices"][0],
        );
        // Only a delta, no new start
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["text"], "world!");
        assert_eq!(events[0].1["index"], 0);
    }

    #[test]
    fn test_empty_text_delta_skipped() {
        let mut t = TestTranslator::new();

        let events = t.process(
            &json!({
                "choices": [{"delta": {"content": ""}}]
            })["choices"][0],
        );
        assert!(events.is_empty());
    }

    // ── Tool call tests ───────────────────────────────────────────

    #[test]
    fn test_tool_call_start() {
        let mut t = TestTranslator::new();

        let events = t.process(
            &json!({
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
            })["choices"][0],
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["content_block"]["type"], "tool_use");
        assert_eq!(events[0].1["content_block"]["id"], "call_abc123");
        assert_eq!(events[0].1["content_block"]["name"], "get_weather");
        assert!(t.tool_map.contains_key(&0));
    }

    #[test]
    fn test_tool_call_with_arguments() {
        let mut t = TestTranslator::new();

        // Start the tool call
        let _ = t.process(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_abc",
                            "function": {"name": "search", "arguments": ""}
                        }]
                    }
                }]
            })["choices"][0],
        );

        // Stream arguments
        let events = t.process(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"arguments": "{\"q\":\"test\"}"}
                        }]
                    }
                }]
            })["choices"][0],
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "content_block_delta");
        assert_eq!(events[0].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[0].1["delta"]["partial_json"], "{\"q\":\"test\"}");
    }

    #[test]
    fn test_multiple_tool_calls() {
        let mut t = TestTranslator::new();

        let events = t.process(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [
                        {"index": 0, "id": "call_0", "function": {"name": "fn_a", "arguments": ""}},
                        {"index": 1, "id": "call_1", "function": {"name": "fn_b", "arguments": ""}}
                    ]
                }
            }]
        })["choices"][0]);

        // First call opens block 0; second call closes block 0 then opens block 1.
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].1["index"], 0); // first tool_use start
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[1].0, "content_block_stop"); // close first block
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[2].1["index"], 1); // second tool_use start
        assert_eq!(events[2].0, "content_block_start");
        assert_eq!(t.tool_map.len(), 2);
    }

    // ── Finalize / closing events ──────────────────────────────────

    #[test]
    fn test_finalize_with_open_text_block() {
        let mut t = TestTranslator::new();
        let _ = t.process(
            &json!({
                "choices": [{"delta": {"content": "Hello"}}]
            })["choices"][0],
        );

        let events = t.finalize();
        // content_block_stop + message_delta + message_stop
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[1].0, "message_delta");
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[2].0, "message_stop");
    }

    #[test]
    fn test_finalize_no_content_emits_empty_text_block() {
        let t = TestTranslator::new();
        let events = t.finalize();
        // content_block_start + content_block_stop + message_delta + message_stop
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[0].1["index"], 0);
        assert_eq!(events[0].1["content_block"]["type"], "text");
        assert_eq!(events[1].0, "content_block_stop");
    }

    #[test]
    fn test_finalize_with_tool_call_sets_stop_reason_tool_use() {
        let mut t = TestTranslator::new();
        let _ = t.process(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_x",
                            "function": {"name": "fn", "arguments": ""}
                        }]
                    }
                }]
            })["choices"][0],
        );

        let events = t.finalize();
        let msg_delta = &events[1];
        assert_eq!(msg_delta.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_finalize_output_tokens() {
        let t = TestTranslator::new();
        // Finalize with no chunks, output_tokens stays 0
        let events = t.finalize();
        assert_eq!(events[2].1["usage"]["output_tokens"], 0);
    }

    // ── Full reasoning + text + finalize integration ───────────────

    #[test]
    fn test_reasoning_to_text_integration() {
        let mut t = TestTranslator::new();

        // 1. Reasoning
        let _ = t.process(
            &json!({
                "choices": [{"delta": {"reasoning_content": "Think..."}}]
            })["choices"][0],
        );

        // 2. Text
        let _ = t.process(
            &json!({
                "choices": [{"delta": {"content": "Answer"}}]
            })["choices"][0],
        );

        // 3. Finalize
        let events = t.finalize();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], 1); // text block index
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
    }
}
