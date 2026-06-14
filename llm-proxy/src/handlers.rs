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
use crate::state::{AppState, Target, Tier, TraceEntry};

/// Decide whether a request on an expensive tier (Opus/Sonnet) should keep
/// its tier or can be safely downgraded to the cheaper fallback.
///
/// # How it works
///
/// The proxy looks at **two signals together**:
///
/// 1. **Last assistant message** — did it contain `tool_use`?
/// 2. **Inter-request gap** — how long since the previous request arrived?
///
/// | Last assistant | Gap | Meaning | Action |
/// |----------------|-----|---------|--------|
/// | `tool_use` | < 25s | Fast tool loop — model talking to itself | **Downgrade** |
/// | `tool_use` | ≥ 25s | Pause — human returned or heavy tool finished | **Keep** |
/// | text / none | any | Real task or new conversation | **Keep** |
/// | `thinking.enabled` | any | Extended Thinking | **Keep** |
///
/// The time threshold (~25s) comes from empirical data: tool loop gaps are
/// almost always < 17s, human pauses are almost always > 34s.
fn should_keep_on_expensive_tier(payload: &Value, gap_s: u64) -> (bool, &'static str) {
    // 1. Extended Thinking → keep (strongest signal)
    if payload
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        == Some("enabled")
    {
        return (true, "thinking enabled");
    }

    // 2. Check if the previous assistant turn contained tool_use.
    let has_tool_use = payload
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|messages| {
            // Find the last assistant response before the final user message.
            messages
                .iter()
                .rev()
                .skip_while(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
                .and_then(|asst| {
                    asst.get("content").and_then(|c| c.as_array()).map(|arr| {
                        arr.iter()
                            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    })
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if has_tool_use && gap_s < 25 {
        // Fast tool loop — model is talking to itself, no human waiting.
        return (false, "tool_use loop");
    }

    // 3. Everything else → keep.
    (true, "real task")
}

/// Handler for `POST /v1/messages` — main Anthropic Messages API endpoint.
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
    let req_id = state.next_req_id();

    // Measure gap since previous request. Fast gaps (<25s) with tool_use
    // mean a tool loop — safe to downgrade. Large gaps mean a human was involved.
    let gap_s = state.mark_request_arrival();

    // Smart routing: downgrade cheap requests (tool loops)
    // while keeping real tasks on the expensive tier.
    // Downgrade cascades to the next tier: Opus→Sonnet, Sonnet→Haiku
    // (using whatever target that tier is configured to route to).
    let (effective_tier, reason) = {
        let opus_enabled = *state.opus_downgrade_enabled.read().unwrap();
        let sonnet_enabled = *state.sonnet_downgrade_enabled.read().unwrap();
        if (tier == Tier::Opus && opus_enabled) || (tier == Tier::Sonnet && sonnet_enabled) {
            let (keep, r) = should_keep_on_expensive_tier(&payload, gap_s);
            if keep {
                let target_keep = state.target_for(tier);
                state.log_event(crate::db_log::LogEvent {
                    seq: req_id,
                    event_type: "KEEP".to_string(),
                    tier: Some(format!("{tier:?}")),
                    effective_tier: Some(format!("{tier:?}")),
                    target: Some(target_keep.as_str().to_string()),
                    model: Some(model.clone()),
                    reason: Some(r.to_string()),
                    ..Default::default()
                });
                (tier, r)
            } else {
                let next_tier = match tier {
                    Tier::Opus => Tier::Sonnet,
                    _ => Tier::Haiku,
                };
                let next_target = state.target_for(next_tier);
                state.log_event(crate::db_log::LogEvent {
                    seq: req_id,
                    event_type: "DOWNGRADE".to_string(),
                    tier: Some(format!("{tier:?}")),
                    effective_tier: Some(format!("{next_tier:?}")),
                    target: Some(next_target.as_str().to_string()),
                    model: Some(model.clone()),
                    reason: Some(r.to_string()),
                    ..Default::default()
                });
                (next_tier, r)
            }
        } else {
            (tier, "")
        }
    };

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
        let tgt = state.target_for(effective_tier);
        eprintln!(
            "[llm-proxy] → {tier:?}{} ⇒ {} (model={model}, stream={stream}) | incoming auth: {auth}",
            if effective_tier != tier {
                format!("⇢{:?}", effective_tier)
            } else {
                String::new()
            },
            tgt.as_str()
        );
    }

    // Resolve where this tier routes: Anthropic native, or an upstream model.
    let target = state.target_for(effective_tier);

    // Record routing decision in the trace ring buffer.
    let downgraded = effective_tier != tier;
    state.push_trace(TraceEntry {
        seq: req_id,
        tier,
        effective_tier,
        target: target.as_str().to_string(),
        model: model.clone(),
        downgraded,
        reason: if downgraded {
            reason.to_string()
        } else {
            String::new()
        },
    });

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
