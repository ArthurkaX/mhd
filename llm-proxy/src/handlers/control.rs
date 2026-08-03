use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::Value;

use super::error::AppError;
use crate::state::{AppState, CodexTarget, Target};

#[derive(Deserialize)]
pub struct SetModelBody {
    pub id: String,
}

pub async fn set_model(
    State(state): State<Arc<AppState>>,
    Path(slot): Path<String>,
    Json(body): Json<SetModelBody>,
) -> Result<Json<Value>, AppError> {
    let target = Target::parse(&body.id);
    let cfg_before = state.to_config();
    let old_target = match slot.as_str() {
        "opus" => cfg_before.opus_target,
        "sonnet" => cfg_before.sonnet_target,
        "haiku" => cfg_before.haiku_target,
        "fable" => cfg_before.fable_target,
        "codex" => cfg_before.codex_target.clone(),
        _ => String::new(),
    };

    if slot == "codex" {
        state.set_codex_target(CodexTarget::parse(&body.id));
    } else if !state.set_target(&slot, target.clone()) {
        return Err(AppError::bad_request(format!(
            "Unknown slot '{slot}'. Use: opus, sonnet, haiku, fable, codex"
        )));
    }

    state.log_event(crate::db_log::LogEvent {
        seq: 0,
        event_type: "MODEL_SWITCH".to_string(),
        target: Some(if slot == "codex" {
            body.id.clone()
        } else {
            target.as_str().to_string()
        }),
        model: Some(slot.clone()),
        reason: Some(format!(
            "slot={} {} -> {}",
            slot,
            old_target,
            target.as_str()
        )),
        ..Default::default()
    });
    if let Err(e) = crate::config::save(&state.to_config()) {
        tracing::warn!("Failed to persist config: {e}");
    }

    let effective_target = if slot == "codex" {
        state.codex_target().as_str().to_string()
    } else {
        target.as_str().to_string()
    };
    Ok(Json(
        serde_json::json!({"status":"ok", "slot":slot, "target":effective_target}),
    ))
}

pub async fn get_config(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::json!({
        "opus": state.opus_target.read().unwrap_or_else(|e| e.into_inner()).as_str(),
        "sonnet": state.sonnet_target.read().unwrap_or_else(|e| e.into_inner()).as_str(),
        "haiku": state.haiku_target.read().unwrap_or_else(|e| e.into_inner()).as_str(),
        "fable": state.fable_target.read().unwrap_or_else(|e| e.into_inner()).as_str(),
        "codex": state.codex_target().as_str(),
        "upstream_base_url": *state.upstream_base_url.read().unwrap_or_else(|e| e.into_inner()),
        "anthropic_key_set": !state.anthropic_key.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "upstream_key_set": !state.upstream_key.read().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "log_level": state.log_level.read().unwrap_or_else(|e| e.into_inner()).as_str(),
    }))
}

pub async fn toggle_debug(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut ll = state.log_level.write().unwrap_or_else(|e| e.into_inner());
    let new = match *ll {
        crate::state::DebugLevel::None => crate::state::DebugLevel::Maximal,
        _ => crate::state::DebugLevel::None,
    };
    *ll = new;
    Json(serde_json::json!({ "log_level": new.as_str() }))
}

pub async fn get_route_stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(Value::Array(
        state
            .route_cache()
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "route": r.route, "reports": r.reports(), "caches": r.caches(),
                    "requests": r.requests, "reporting_requests": r.reporting_requests,
                    "cache_read_tokens": r.cache_read_tokens,
                    "cache_creation_tokens": r.cache_creation_tokens, "last_seen": r.last_seen,
                })
            })
            .collect(),
    ))
}

pub async fn health() -> Json<Value> {
    Json(
        serde_json::json!({"status":"ok", "service":"llm-proxy", "version":env!("CARGO_PKG_VERSION")}),
    )
}
