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
use axum::http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

use crate::state::AppState;

/// Build a request to Anthropic with auth/version/beta headers forwarded from
/// the incoming Claude Code request.
async fn build_request(
    state: &Arc<AppState>,
    incoming: &HeaderMap,
) -> reqwest::RequestBuilder {
    let client = reqwest::Client::new();
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
        let api_key = state.anthropic_key.read().await.clone();
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
    let resp = build_request(state, incoming).await.json(&payload).send().await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    let json: Value = resp.json().await?;
    Ok(json)
}

/// Streaming request — returns the raw upstream response so the SSE byte stream
/// can be piped straight through to Claude Code unchanged.
pub async fn stream_request(
    state: &Arc<AppState>,
    payload: Value,
    incoming: &HeaderMap,
) -> Result<reqwest::Response> {
    let resp = build_request(state, incoming).await.json(&payload).send().await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API error (HTTP {}): {}", status, body);
    }

    Ok(resp)
}
