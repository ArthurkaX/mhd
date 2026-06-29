//! Passthrough provider — forwards requests to the official Anthropic API
//! without transforming the body.
//!
//! Auth is credential-agnostic:
//!   - If Claude Code sent an `Authorization` header (OAuth bearer from a
//!     Pro/Max subscription), forward it verbatim — no API key needed.
//!   - Otherwise fall back to the `anthropic_key` from config as `x-api-key`
//!     (classic API-key users).
//! Subscription auth also needs the `anthropic-beta` header forwarded, so we
//! pass through any beta/version headers Claude Code sent.

use anyhow::Result;
use axum::body::Body;
use axum::http::HeaderMap;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;

use crate::state::AppState;

use super::{InflightGuard, now_ms};

/// Build a request to Anthropic with auth/version/beta headers forwarded from
/// the incoming Claude Code request.
async fn build_request(state: &Arc<AppState>, incoming: &HeaderMap, streaming: bool) -> reqwest::RequestBuilder {
    let client = if streaming { &state.http_stream } else { &state.http };
    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("content-type", "application/json");

    // ── auth ───────────────────────────────────────────────────────────
    if let Some(auth) = incoming.get("authorization").and_then(|v| v.to_str().ok()) {
        // Subscription OAuth (or any bearer) — forward as-is.
        req = req.header("authorization", auth);
    } else if let Some(key) = incoming.get("x-api-key").and_then(|v| v.to_str().ok()) {
        // API key sent by the client — forward it.
        req = req.header("x-api-key", key);
    } else {
        // Fall back to the configured API key.
        let api_key = state.anthropic_key.read().unwrap_or_else(|e| e.into_inner()).clone();
        req = req.header("x-api-key", api_key);
    }

    // ── version / beta headers (needed for OAuth) ──────────────────────
    let version = incoming
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("2023-06-01");
    req = req.header("anthropic-version", version);

    if let Some(beta) = incoming.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        req = req.header("anthropic-beta", beta);
    }

    req
}

/// Parse `anthropic-ratelimit-unified-*` response headers into a typed
/// [`QuotaSnapshot`] and hand it to [`AppState::record_quota`] (which dedups).
/// Best-effort: never errors, never touches the body.
fn record_quota_headers(state: &Arc<AppState>, headers: &reqwest::header::HeaderMap) {
    let get = |name: &str| -> Option<String> {
        headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    };
    let snap = crate::db_log::QuotaSnapshot {
        h5_utilization: get("anthropic-ratelimit-unified-5h-utilization").and_then(|s| s.parse().ok()),
        h5_status:      get("anthropic-ratelimit-unified-5h-status"),
        h5_reset:       get("anthropic-ratelimit-unified-5h-reset").and_then(|s| s.parse().ok()),
        d7_utilization: get("anthropic-ratelimit-unified-7d-utilization").and_then(|s| s.parse().ok()),
        d7_status:      get("anthropic-ratelimit-unified-7d-status"),
        d7_reset:       get("anthropic-ratelimit-unified-7d-reset").and_then(|s| s.parse().ok()),
        representative_claim: get("anthropic-ratelimit-unified-representative-claim"),
        fallback_status:      get("anthropic-ratelimit-unified-fallback"),
        overage_status:       get("anthropic-ratelimit-unified-overage-status"),
    };
    state.record_quota(snap);
}

/// Non-streaming request — returns the parsed JSON body.
pub async fn send_request(
    state: &Arc<AppState>,
    req_id: u64,
    payload: Value,
    incoming: &HeaderMap,
) -> Result<Value> {
    let log = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_errors();
    let _guard = InflightGuard::new(state.clone());
    let started = std::time::Instant::now();
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} native START inflight={inflight}",
            now_ms()
        ));
    }
    let detailed = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_detailed();
    if detailed {
        state.log_line(&format!(
            "{} #{req_id} native REQ {}",
            now_ms(),
            super::summarize_payload(&payload)
        ));
    }

    let resp = match build_request(state, incoming, false)
        .await
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_connect() {
                "CONNECT_FAILED"
            } else if e.is_timeout() {
                "TIMEOUT"
            } else {
                "SEND_ERR"
            };
            if log {
                state.log_line(&format!(
                    "{} #{req_id} native SEND_ERROR {kind} after {} ms: {e}",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            }
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                kind,
            );
            return Err(e.into());
        }
    };

    // Record Anthropic quota snapshot from unified rate-limit headers.
    record_quota_headers(state, resp.headers());

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if log {
            state.log_line(&format!(
                "{} #{req_id} native ERROR {} after {} ms",
                now_ms(),
                status,
                started.elapsed().as_millis()
            ));
        }
        state.mark_request_failed(
            req_id,
            Some(started.elapsed().as_millis() as u64),
            Some(status.as_u16()),
            &format!("Anthropic API error (HTTP {}): {}", status, body),
            "HTTP_ERROR",
        );
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    let json: Value = resp.json().await?;

    // Push token usage into the trace entry.
    // On the Anthropic native path, input_tokens is already fresh (uncached).
    // cache_read_input_tokens and cache_creation_input_tokens are separate fields.
    if let Some(usage) = json.get("usage") {
        let inp = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let out = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if inp > 0 || out > 0 {
            state.update_trace_tokens(
                req_id,
                inp,
                out,
                cache_read,
                cache_creation,
                Some(started.elapsed().as_millis() as u64),
                Some(status.as_u16()),
            );
        }
    }

    if log {
        state.log_line(&format!(
            "{} #{req_id} native DONE after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }
    if detailed {
        let stop_reason = json
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let usage = json.get("usage").map(|u| u.to_string()).unwrap_or_default();
        state.log_line(&format!(
            "{} #{req_id} native RESP stop={} usage={}",
            now_ms(),
            stop_reason,
            usage
        ));
    }
    Ok(json)
}

/// Streaming request — pipes the raw upstream SSE byte stream straight through
/// to Claude Code unchanged. Returns an axum `Body`; the in-flight guard is
/// moved into the stream so the concurrency count stays accurate for the full
/// duration of the stream (even on mid-stream client disconnect).
pub async fn stream_request(
    state: &Arc<AppState>,
    req_id: u64,
    payload: Value,
    incoming: &HeaderMap,
) -> Result<Body> {
    let log = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_errors();
    let guard = InflightGuard::new(state.clone());
    let started = std::time::Instant::now();
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} native stream START inflight={inflight}",
            now_ms()
        ));
    }
    let detailed = state.log_level.read().unwrap_or_else(|e| e.into_inner()).log_detailed();
    if detailed {
        state.log_line(&format!(
            "{} #{req_id} native stream REQ {}",
            now_ms(),
            super::summarize_payload(&payload)
        ));
    }

    let resp = match build_request(state, incoming, true)
        .await
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let kind = if e.is_connect() {
                "CONNECT_FAILED"
            } else if e.is_timeout() {
                "TIMEOUT"
            } else {
                "SEND_ERR"
            };
            if log {
                state.log_line(&format!(
                    "{} #{req_id} native stream SEND_ERROR {kind} after {} ms: {e}",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            }
            state.mark_request_failed(
                req_id,
                Some(started.elapsed().as_millis() as u64),
                None,
                &e.to_string(),
                kind,
            );
            return Err(e.into());
        }
    };

    if log {
        state.log_line(&format!(
            "{} #{req_id} native stream headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }

    // Record Anthropic quota snapshot from unified rate-limit headers.
    record_quota_headers(state, resp.headers());

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        state.mark_request_failed(
            req_id,
            Some(started.elapsed().as_millis() as u64),
            Some(status.as_u16()),
            &format!("Anthropic API error (HTTP {}): {}", status, body),
            "HTTP_ERROR",
        );
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    let status_opt = Some(status.as_u16());
    let mut byte_stream = resp.bytes_stream();
    let state_for_log = state.clone();
    let s = async_stream::stream! {
        // Hold the in-flight guard for the lifetime of the stream.
        let _guard = guard;
        let mut had_error = false;
        let mut line_buf: Vec<u8> = Vec::new();
        // Anthropic splits usage across two events: `message_start` carries the
        // prompt side (input + cache_read + cache_creation), `message_delta`
        // carries the running output count. We must read BOTH — reading only
        // message_delta (the old behaviour) left input/cache at 0, so the In
        // column and the cache colour never populated on the native path.
        let mut base_input: u64 = 0;
        let mut cache_read: u64 = 0;
        let mut cache_creation: u64 = 0;
        let mut output_tokens: u64 = 0;
        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => { had_error = true; break },
            };

            // Accumulate lines and scan for usage on message_start / message_delta.
            line_buf.extend_from_slice(&chunk);
            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = line_buf.drain(..=pos).collect();
                if let Ok(line) = std::str::from_utf8(&line_bytes) {
                    let data = line.trim();
                    if let Some(json_str) = data.strip_prefix("data:") {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str.trim()) {
                            match v.get("type").and_then(|t| t.as_str()) {
                                // Prompt-side usage (incl. real Anthropic prompt cache).
                                Some("message_start") => {
                                    if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                                        if let Some(n) = usage.get("input_tokens").and_then(|n| n.as_u64()) {
                                            base_input = n;
                                        }
                                        if let Some(n) = usage.get("cache_read_input_tokens").and_then(|n| n.as_u64()) {
                                            cache_read = n;
                                        }
                                        if let Some(n) = usage.get("cache_creation_input_tokens").and_then(|n| n.as_u64()) {
                                            cache_creation = n;
                                        }
                                        if let Some(n) = usage.get("output_tokens").and_then(|n| n.as_u64()) {
                                            output_tokens = n;
                                        }
                                    }
                                }
                                // Output-side running total (occasionally restates input).
                                Some("message_delta") => {
                                    if let Some(usage) = v.get("usage") {
                                        if let Some(n) = usage.get("input_tokens").and_then(|n| n.as_u64()) {
                                            base_input = n;
                                        }
                                        if let Some(n) = usage.get("output_tokens").and_then(|n| n.as_u64()) {
                                            output_tokens = n;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            yield Ok::<_, std::io::Error>(chunk);
        }
        // Push token usage into the trace entry with correctly separated counts:
        //   input_tokens        = fresh (uncached) prompt tokens billed at 1×
        //   cache_read          = tokens served from the prompt cache
        //   cache_creation      = tokens written to the cache this turn
        let elapsed = started.elapsed().as_millis() as u64;
        if had_error {
            state_for_log.mark_request_failed(
                req_id,
                Some(elapsed),
                status_opt,
                "stream transport error",
                "STREAM_ERR",
            );
        } else if base_input > 0 || output_tokens > 0 {
            state_for_log.update_trace_tokens(
                req_id,
                base_input,
                output_tokens,
                cache_read,
                cache_creation,
                Some(elapsed),
                status_opt,
            );
        }
        if log {
            if had_error {
                state_for_log.log_line(&format!(
                    "{} #{req_id} native stream ERROR after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            } else {
                state_for_log.log_line(&format!(
                    "{} #{req_id} native stream DONE after {} ms",
                    now_ms(),
                    started.elapsed().as_millis()
                ));
            }
        }
    };

    Ok(Body::from_stream(s))
}
