//! Axum route handlers for the LLM proxy.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;

use crate::providers;
use crate::state::{AppState, Target, Tier};

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
    let stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if state.log_level.read().unwrap().dump_bodies() {
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
        let tgt = state.target_for(tier);
        eprintln!(
            "[llm-proxy] → {tier:?} ⇒ {} (model={model}, stream={stream}) | incoming auth: {auth}",
            tgt.as_str()
        );
    }

    // Resolve where this tier routes: Anthropic native, or an upstream model.
    let target = state.target_for(tier);

    if stream {
        let body = match &target {
            Target::Native => {
                providers::anthropic::stream_request(&state, payload, &headers).await?
            }
            Target::Model(id) => {
                providers::upstream::stream_request(&state, payload, id, &model).await?
            }
        };
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .map_err(|e| AppError(e.into()))?);
    }

    let response = match &target {
        Target::Native => providers::anthropic::send_request(&state, payload, &headers).await?,
        Target::Model(id) => providers::upstream::send_request(&state, payload, id).await?,
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
pub struct SetModelBody {
    /// Routing target: `native` for Anthropic, or an upstream model id like
    /// `sva-opencode/glm-5.1`.
    id: String,
}

/// `POST /set_model/{slot}` — change a tier's routing target on the fly.
/// `slot` is `opus`, `sonnet`, `haiku`, or `fable`.
///
/// Body (JSON): `{"id": "<native|model-id>"}`
///
/// Examples:
///   curl -X POST "http://localhost:3456/set_model/sonnet" \
///        -H "content-type: application/json" \
///        -d '{"id":"sva-opencode/qwen3.7-max"}'
///   curl -X POST "http://localhost:3456/set_model/sonnet" \
///        -H "content-type: application/json" \
///        -d '{"id":"native"}'
pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(slot): Path<String>,
    Json(body): Json<SetModelBody>,
) -> Result<Json<Value>, AppError> {
    let target = Target::parse(&body.id);
    if !state.set_target(&slot, target.clone()) {
        return Err(AppError::bad_request(format!(
            "Unknown slot '{slot}'. Use: opus, sonnet, haiku, fable"
        )));
    }

    // Persist the change so it survives restarts.
    if let Err(e) = crate::config::save(&state.to_config()) {
        tracing::warn!("Failed to persist config: {e}");
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "slot": slot,
        "target": target.as_str(),
    })))
}

/// `GET /config` — show the current effective routing config (keys masked).
pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::json!({
        "opus": state.opus_target.read().unwrap().as_str(),
        "sonnet": state.sonnet_target.read().unwrap().as_str(),
        "haiku": state.haiku_target.read().unwrap().as_str(),
        "fable": state.fable_target.read().unwrap().as_str(),
        "upstream_base_url": *state.upstream_base_url.read().unwrap(),
        "anthropic_key_set": !state.anthropic_key.read().unwrap().is_empty(),
        "upstream_key_set": !state.upstream_key.read().unwrap().is_empty(),
        "log_level": state.log_level.read().unwrap().as_str(),
    }))
}

/// `POST /debug` — toggle debug dump mode.
pub async fn toggle_debug(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut ll = state.log_level.write().unwrap();
    let new = match *ll {
        crate::state::DebugLevel::None => crate::state::DebugLevel::Maximal,
        _ => crate::state::DebugLevel::None,
    };
    *ll = new;
    Json(serde_json::json!({ "log_level": new.as_str() }))
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
