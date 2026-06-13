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
async fn build_request(state: &Arc<AppState>, incoming: &HeaderMap) -> reqwest::RequestBuilder {
    let mut req = state
        .http
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
        let api_key = state.anthropic_key.read().unwrap().clone();
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

/// Non-streaming request — returns the parsed JSON body.
pub async fn send_request(
    state: &Arc<AppState>,
    payload: Value,
    incoming: &HeaderMap,
) -> Result<Value> {
    let log = state.log_level.read().unwrap().log_errors();
    let req_id = state.next_req_id();
    let _guard = InflightGuard::new(state.clone());
    let started = std::time::Instant::now();
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} native START inflight={inflight}",
            now_ms()
        ));
    }

    let resp = build_request(state, incoming)
        .await
        .json(&payload)
        .send()
        .await?;

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
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    let json: Value = resp.json().await?;
    if log {
        state.log_line(&format!(
            "{} #{req_id} native DONE after {} ms",
            now_ms(),
            started.elapsed().as_millis()
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
    payload: Value,
    incoming: &HeaderMap,
) -> Result<Body> {
    let log = state.log_level.read().unwrap().log_errors();
    let req_id = state.next_req_id();
    let guard = InflightGuard::new(state.clone());
    let started = std::time::Instant::now();
    if log {
        let inflight = state.inflight.load(std::sync::atomic::Ordering::SeqCst);
        state.log_line(&format!(
            "{} #{req_id} native stream START inflight={inflight}",
            now_ms()
        ));
    }

    let resp = build_request(state, incoming)
        .await
        .json(&payload)
        .send()
        .await?;

    if log {
        state.log_line(&format!(
            "{} #{req_id} native stream headers after {} ms",
            now_ms(),
            started.elapsed().as_millis()
        ));
    }

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    let mut byte_stream = resp.bytes_stream();
    let state_for_log = state.clone();
    let s = async_stream::stream! {
        // Hold the in-flight guard for the lifetime of the stream.
        let _guard = guard;
        let mut had_error = false;
        while let Some(item) = byte_stream.next().await {
            match item {
                Ok(chunk) => yield Ok::<_, std::io::Error>(chunk),
                Err(_) => { had_error = true; break },
            }
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
