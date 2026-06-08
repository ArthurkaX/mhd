//! Axum route handlers for the LLM proxy.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use crate::providers;
use crate::state::{AppState, Tier};

/// Handler for `POST /v1/messages` — main Anthropic Messages API endpoint.
///
/// Routing by model tier:
///   - opus            → official Anthropic (passthrough)
///   - sonnet, unknown → upstream gateway with `sonnet_model`
///   - haiku           → upstream gateway with `haiku_model`
pub async fn post_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let tier = Tier::from_model(&model);
    let stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    if *state.debug_dump.read().await {
        // Show how Claude Code authenticated (helps verify OAuth passthrough).
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                let head: String = s.chars().take(20).collect();
                format!("{head}…(len {})", s.len())
            })
            .or_else(|| {
                headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| format!("x-api-key:{}…", s.chars().take(12).collect::<String>()))
            })
            .unwrap_or_else(|| "(none)".to_string());
        eprintln!(
            "[llm-proxy] → {tier:?} (model={model}, stream={stream}) | incoming auth: {auth}"
        );
    }

    // Resolve the upstream target model for non-opus tiers.
    let target = match tier {
        Tier::Opus => String::new(),
        Tier::Sonnet => state.sonnet_model.read().await.clone(),
        Tier::Haiku => state.haiku_model.read().await.clone(),
    };

    if stream {
        let body = match tier {
            Tier::Opus => {
                let resp = providers::anthropic::stream_request(&state, payload, &headers).await?;
                Body::from_stream(resp.bytes_stream())
            }
            _ => providers::upstream::stream_request(&state, payload, &target, &model).await?,
        };
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .map_err(|e| AppError(e.into()))?);
    }

    let response = match tier {
        Tier::Opus => providers::anthropic::send_request(&state, payload, &headers).await?,
        _ => providers::upstream::send_request(&state, payload, &target).await?,
    };

    Ok(Json(response).into_response())
}

/// Handler for `POST /v1/chat/completions` — OpenAI-compatible passthrough to
/// the upstream gateway (for OpenAI-native clients).
pub async fn post_chat_completions(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let resp = providers::upstream::send_raw_openai(&state, payload).await?;
    Ok(Json(resp))
}

// ─── Model switching endpoints ───────────────────────────────────────

#[derive(Deserialize)]
pub struct SetModelQuery {
    /// Upstream model id, e.g. `sva-opencode/glm-5.1`.
    id: String,
}

/// `GET /set_model/{slot}?id=<upstream-model-id>` — change which upstream model
/// a tier maps to, on the fly. `slot` is `sonnet` or `haiku`.
///
/// Example:
///   curl "http://localhost:3456/set_model/sonnet?id=sva-opencode/qwen3.7-max"
pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(slot): Path<String>,
    Query(q): Query<SetModelQuery>,
) -> Result<Json<Value>, AppError> {
    match slot.as_str() {
        "sonnet" => *state.sonnet_model.write().await = q.id.clone(),
        "haiku" => *state.haiku_model.write().await = q.id.clone(),
        other => {
            return Err(AppError::bad_request(format!(
                "Unknown slot '{other}'. Use: sonnet, haiku"
            )));
        }
    }

    // Persist the change so it survives restarts.
    if let Err(e) = crate::config::save(&state.to_config().await) {
        tracing::warn!("Failed to persist config: {e}");
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "slot": slot,
        "model": q.id,
    })))
}

/// `GET /config` — show the current effective routing config (keys masked).
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::json!({
        "opus": "official Anthropic (passthrough)",
        "sonnet_model": *state.sonnet_model.read().await,
        "haiku_model": *state.haiku_model.read().await,
        "upstream_base_url": *state.upstream_base_url.read().await,
        "anthropic_key_set": !state.anthropic_key.read().await.is_empty(),
        "upstream_key_set": !state.upstream_key.read().await.is_empty(),
    }))
}

/// `GET /debug` — toggle debug dump mode.
pub async fn toggle_debug(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut debug = state.debug_dump.write().await;
    *debug = !*debug;
    Json(serde_json::json!({ "debug_dump": *debug }))
}

/// `GET /health` — health check.
pub async fn health() -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "llm-proxy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ─── Error handling ──────────────────────────────────────────────────

/// Wrapper error type that renders as an HTTP error response.
pub struct AppError(anyhow::Error);

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self(anyhow::anyhow!(msg.into()))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let msg = format!("{}", self.0);
        let body = serde_json::json!({
            "error": { "type": "proxy_error", "message": msg }
        });
        (StatusCode::BAD_GATEWAY, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
