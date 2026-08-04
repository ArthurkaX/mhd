//! Axum route handlers for the LLM proxy.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub mod control;
pub mod error;

use axum::{
    Json,
    body::Bytes,
    extract::{
        State,
        ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::Mutex as TokioMutex;

use crate::providers;
use crate::state::{AppState, ClientKind, CodexTarget, Target, Tier, TraceEntry, WireApi};

pub use control::{get_config, get_route_stats, health, set_model, toggle_debug};
pub use error::AppError;

/// Read the `x-client-run-id` header a harness may send to identify its session.
///
/// Purely observational: the value is logged onto the request row and is never
/// forwarded upstream (outbound headers are rebuilt from scratch in the provider
/// modules, so nothing has to be stripped). Values longer than 128 chars are
/// dropped rather than truncated — a run id that long is a client bug, and a
/// truncated one would silently collide with its siblings.
fn client_run_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-client-run-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(|s| s.to_string())
}

/// Derive a stable session hash from a messages payload.
/// The session is identified by the system prompt + the first user message,
/// which stay constant across tool loops in one conversation.
fn session_hash(payload: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    // Hash the system prompt
    if let Some(system) = payload.get("system") {
        system.to_string().hash(&mut hasher);
    }

    // Hash the first user message (stable across turns)
    if let Some(messages) = payload.get("messages").and_then(|m| m.as_array())
        && let Some(first_user) = messages
            .iter()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        first_user.to_string().hash(&mut hasher);
    }

    hasher.finish()
}

/// Compute the FNV-1a 64-bit hash of the cacheable prefix: system + tools.
/// These two fields are what Anthropic treats as the stable cache prefix.
/// Returns 0 if neither field is present (unknown prefix).
fn prefix_hash(payload: &Value) -> u64 {
    let system = payload.get("system").unwrap_or(&Value::Null);
    let tools = payload.get("tools").unwrap_or(&Value::Null);
    if matches!(system, Value::Null) && matches!(tools, Value::Null) {
        return 0;
    }
    let bytes = serde_json::to_vec(&[system, tools]).unwrap_or_default();
    if bytes.is_empty() {
        return 0;
    }
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in &bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

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
    if let Some(t) = payload
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        && (t == "enabled" || t == "adaptive")
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
    // A disabled client means the user does not use that client at all, so mHD
    // should do no work for it — a resource switch, not an access-control
    // feature. Refuse before any body parsing or upstream work.
    if !state.client_enabled(ClientKind::ClaudeCode) {
        return Err(AppError::disabled_client(ClientKind::ClaudeCode));
    }
    let model = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let tier = Tier::from_model(&model);
    let req_id = state.next_req_id();
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let client_run_id = client_run_id(&headers);

    // Calculate per-session gap for expensive tiers that can be downgraded.
    // Only for Opus/Sonnet — cheap tiers and background calls don't stamp the clock.
    let opus_enabled = *state
        .opus_downgrade_enabled
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let sonnet_enabled = *state
        .sonnet_downgrade_enabled
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let may_downgrade =
        (tier == Tier::Opus && opus_enabled) || (tier == Tier::Sonnet && sonnet_enabled);

    let gap_s = if may_downgrade {
        let s_hash = session_hash(&payload);
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        state.mark_session_request(s_hash, now)
    } else {
        9999 // sentinel — never downgrade
    };

    // Smart routing: downgrade cheap requests (tool loops)
    // while keeping real tasks on the expensive tier.
    // Downgrade cascades to the next tier: Opus→Sonnet, Sonnet→Haiku
    // (using whatever target that tier is configured to route to).
    // The routing decision (downgraded + reason) is captured as typed columns
    // on the requests DB row — no separate KEEP/DOWNGRADE event needed.
    let (effective_tier, reason) = {
        if (tier == Tier::Opus && opus_enabled) || (tier == Tier::Sonnet && sonnet_enabled) {
            let (keep, r) = should_keep_on_expensive_tier(&payload, gap_s);
            if keep {
                (tier, r)
            } else {
                let next_tier = match tier {
                    Tier::Opus => Tier::Sonnet,
                    _ => Tier::Haiku,
                };
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

    if state
        .log_level
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .dump_bodies()
    {
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

    // ── Corpus capture ──────────────────────────────────────────────
    // Capture the full pre-trim body for the replay corpus (no-op when disabled).
    state.capture_request_body(req_id, Some(&model), "anthropic", &payload);

    // ── Trim hook ───────────────────────────────────────────────────
    // Run native request compression before forwarding.
    // Fail-open: any error or no-gain returns the original body.
    // Trim metadata (preset, config, stages) rides on the requests DB row —
    // no separate TRIM event needed.
    // Digest the untouched body first: `prefix_shared_chars` must reflect the
    // harness's own prefix discipline, not what trim left of it.
    let pre_trim_digest = if state.is_db_log_enabled() {
        crate::prefix::digest_anthropic(&payload)
    } else {
        Vec::new()
    };
    let mut trim_applied = false;
    let mut trim_tokens_before = 0u64;
    let mut trim_tokens_after = 0u64;
    let mut trim_preset_str = String::new();
    let mut trim_config_json = String::new();
    let mut trim_stages_json = String::new();
    let payload = if state.trim_enabled_for(ClientKind::ClaudeCode) {
        // Read live-tunable native engine knobs.
        let native_knobs = crate::native_trim::NativeKnobs {
            tool_max_desc_chars: *state
                .trim_tool_desc_chars
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_head: if effective_tier == Tier::Haiku {
                *state
                    .trim_head_haiku
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
            } else {
                *state
                    .trim_toolresult_head
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
            },
            tool_result_tail: *state
                .trim_toolresult_tail
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ws_enabled: *state
                .trim_ws_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            strip_thinking: *state
                .trim_strip_thinking
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_fence_requires_code: *state
                .trim_fence_requires_code
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_arrow_density_min: *state
                .trim_arrow_density_min
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ..Default::default()
        };
        // Check if this target qualifies for the light trim profile.
        let free_target = state
            .trim_free_target
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let native_knobs = crate::trim::resolve_knobs(target.as_str(), &free_target, native_knobs);
        let out = crate::trim::trim_anthropic(payload, native_knobs);
        trim_applied = out.applied;
        trim_tokens_before = out.tokens_before;
        trim_tokens_after = out.tokens_after;

        if out.applied {
            let saved = trim_tokens_before.saturating_sub(trim_tokens_after);
            let pct = if trim_tokens_before > 0 {
                saved as f64 / trim_tokens_before as f64 * 100.0
            } else {
                0.0
            };

            trim_preset_str = out.preset.clone();
            trim_config_json = out.config_json.clone();
            trim_stages_json = serde_json::to_string(
                &out.stages
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "applied": s.applied,
                            "before": s.tokens_before,
                            "after": s.tokens_after,
                            "note": s.note,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default();

            if state
                .log_level
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .dump_bodies()
            {
                eprintln!(
                    "[llm-proxy] trim: −{} tok ({:.1}%) — preset={}",
                    saved, pct, out.preset,
                );
            }
        }

        out.body
    } else {
        payload
    };

    // ── Prefix shape ────────────────────────────────────────────────
    // Digest the body before and after trim, and measure both against the
    // previous request of this client session on this route. Gated on the DB
    // log: the digest hashes the whole body, and nothing reads the result when
    // there is nowhere to write it. NOT a cache signal — see `crate::prefix`.
    let prefix = if state.is_db_log_enabled() {
        let route = match &target {
            Target::Model(id) => id.clone(),
            Target::Native => model.clone(),
        };
        let key =
            crate::prefix::session_key(client_run_id.as_deref(), user_agent.as_deref(), &route);
        state.prefix_tracker.observe(
            &key,
            pre_trim_digest,
            crate::prefix::digest_anthropic(&payload),
            crate::providers::now_unix_ms(),
        )
    } else {
        crate::prefix::PrefixStats::default()
    };

    // Record routing decision in the trace ring buffer (and proxy.db requests row).
    let downgraded = effective_tier != tier;
    state.push_trace(TraceEntry {
        seq: req_id,
        client: ClientKind::ClaudeCode,
        wire_api: WireApi::AnthropicMessages,
        tier: Some(tier),
        effective_tier: Some(effective_tier),
        target: target.as_str().to_string(),
        model: model.clone(),
        downgraded,
        reason: if downgraded {
            reason.to_string()
        } else {
            String::new()
        },
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        trim_applied,
        trim_tokens_before,
        trim_tokens_after,
        trim_preset: trim_preset_str,
        trim_config_json,
        trim_stages_json,
        started_ms: crate::providers::now_unix_ms(),
        prefix_hash: prefix_hash(&payload),
        status: None,
        is_probe: payload.get("max_tokens").and_then(|v| v.as_u64()) == Some(1),
        user_agent,
        client_run_id,
        prefix,
    });

    if stream {
        let body = match &target {
            Target::Native => {
                providers::anthropic::stream_request(&state, req_id, payload, &headers).await?
            }
            Target::Model(id) => {
                providers::upstream::stream_request(&state, req_id, payload, id, &model).await?
            }
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .map_err(AppError::from);
    }

    let response = match &target {
        Target::Native => {
            providers::anthropic::send_request(&state, req_id, payload, &headers).await?
        }
        Target::Model(id) => providers::upstream::send_request(&state, req_id, payload, id).await?,
    };

    Ok(Json(response).into_response())
}

/// Handler for `POST /v1/chat/completions` — OpenAI-compatible passthrough to
/// the upstream gateway (for OpenAI-native clients like Zed).
///
/// Supports non-streaming and streaming responses. Trim (native compression)
/// is applied under the OpenAI client's own `trim_openai_enabled` switch —
/// independent of Claude Code's `trim_enabled` (see
/// [`crate::state::AppState::trim_enabled_for`]).
///
/// The request is recorded in the Proxy Trace ring buffer as an OpenAI
/// passthrough: `client = OpenAi`, `wire_api = ChatCompletions`, and `tier =
/// None` — the tier axis only ever means "class of Claude model", which does
/// not apply to non-Claude clients. Model + trim stats are recorded now, token
/// usage is filled from the upstream response, so OpenAI clients appear
/// alongside Claude Code traffic.
pub async fn post_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
    // Same gate as `/v1/messages`: a disabled client is a resource switch, not
    // access control — mHD does no work for a client the user does not use.
    if !state.client_enabled(ClientKind::OpenAi) {
        return Err(AppError::disabled_client(ClientKind::OpenAi));
    }
    let req_id = state.next_req_id();
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let client_run_id = client_run_id(&headers);
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // ── Corpus capture ──────────────────────────────────────────────
    // Capture the full pre-trim body for the replay corpus (no-op when disabled).
    state.capture_request_body(req_id, Some(&model), "openai", &payload);

    // ── Trim hook ───────────────────────────────────────────────────
    // The OpenAI client's own trim switch (`trim_openai_enabled`) — flipping
    // Claude Code's `trim_enabled` no longer moves this path (see
    // `trim_enabled_for`).
    // Trim metadata rides on the requests DB row — no separate TRIM event needed.
    // Digest the untouched body first — see the /v1/messages path.
    let pre_trim_digest = if state.is_db_log_enabled() {
        crate::prefix::digest_openai(&payload)
    } else {
        Vec::new()
    };
    let mut trim_applied = false;
    let mut trim_tokens_before = 0u64;
    let mut trim_tokens_after = 0u64;
    let mut trim_preset_str = String::new();
    let mut trim_config_json = String::new();
    let mut trim_stages_json = String::new();
    let payload = if state.trim_enabled_for(ClientKind::OpenAi) {
        // Read live-tunable native engine knobs (same reads as post_messages;
        // strip_thinking is irrelevant for OpenAI shape — set false).
        let native_knobs = crate::native_trim::NativeKnobs {
            tool_max_desc_chars: *state
                .trim_tool_desc_chars
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_head: *state
                .trim_head_harness
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_tail: *state
                .trim_toolresult_tail
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ws_enabled: *state
                .trim_ws_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            strip_thinking: false,
            tool_result_fence_requires_code: *state
                .trim_fence_requires_code
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_arrow_density_min: *state
                .trim_arrow_density_min
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ..Default::default()
        };
        // Check if this target qualifies for the light trim profile.
        let free_target = state
            .trim_free_target
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let native_knobs = crate::trim::resolve_knobs(&model, &free_target, native_knobs);
        let out = crate::trim::trim_openai(payload, native_knobs);
        trim_applied = out.applied;
        trim_tokens_before = out.tokens_before;
        trim_tokens_after = out.tokens_after;

        if out.applied {
            let saved = trim_tokens_before.saturating_sub(trim_tokens_after);
            let pct = if trim_tokens_before > 0 {
                saved as f64 / trim_tokens_before as f64 * 100.0
            } else {
                0.0
            };

            trim_preset_str = out.preset.clone();
            trim_config_json = out.config_json.clone();
            trim_stages_json = serde_json::to_string(
                &out.stages
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "applied": s.applied,
                            "before": s.tokens_before,
                            "after": s.tokens_after,
                            "note": s.note,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default();

            if state
                .log_level
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .dump_bodies()
            {
                eprintln!("[llm-proxy] trim(openai): −{} tok ({:.1}%)", saved, pct,);
            }
        }

        out.body
    } else {
        payload
    };

    // ── Prefix shape ────────────────────────────────────────────────
    // Same measurement as the /v1/messages path; the route here is always the
    // requested model (OpenAI passthrough does no tier routing).
    let prefix = if state.is_db_log_enabled() {
        let key =
            crate::prefix::session_key(client_run_id.as_deref(), user_agent.as_deref(), &model);
        state.prefix_tracker.observe(
            &key,
            pre_trim_digest,
            crate::prefix::digest_openai(&payload),
            crate::providers::now_unix_ms(),
        )
    } else {
        crate::prefix::PrefixStats::default()
    };

    // Record this passthrough in the trace ring buffer (and proxy.db requests row).
    // client = OpenAi, wire_api = ChatCompletions, tier = None (no Claude tier axis).
    // Tokens are 0 here and get filled from the upstream response usage (see `*_raw_openai`).
    // prefix_hash is 0 for OpenAI passthrough (no Anthropic-style system/tools prefix).
    state.push_trace(TraceEntry {
        seq: req_id,
        client: ClientKind::OpenAi,
        wire_api: WireApi::ChatCompletions,
        tier: None,
        effective_tier: None,
        target: model.clone(),
        model: model.clone(),
        downgraded: false,
        reason: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        trim_applied,
        trim_tokens_before,
        trim_tokens_after,
        trim_preset: trim_preset_str,
        trim_config_json,
        trim_stages_json,
        started_ms: crate::providers::now_unix_ms(),
        prefix_hash: 0,
        status: None,
        is_probe: false,
        user_agent,
        client_run_id,
        prefix,
    });

    if stream {
        let body = providers::upstream::stream_raw_openai(&state, req_id, payload).await?;
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .map_err(AppError::from);
    }

    let resp = providers::upstream::send_raw_openai(&state, req_id, payload).await?;
    Ok(Json(resp).into_response())
}

/// Handler for `GET /v1/models` — list available models in OpenAI format.
pub async fn get_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, AppError> {
    if crate::providers::codex::is_codex_request(&headers, uri.query()) {
        return Ok(crate::providers::codex::forward(
            &state,
            axum::http::Method::GET,
            uri.path(),
            uri.query(),
            &headers,
            bytes::Bytes::new(),
        )
        .await?);
    }
    let models = crate::config::load_models().unwrap_or_default();
    let data: Vec<Value> = models
        .into_iter()
        .map(|m| {
            let mut entry = serde_json::json!({
                "id": m.id,
                "object": "model",
                "owned_by": m.provider,
            });
            if !m.display_name.is_empty() {
                entry["display_name"] = Value::String(m.display_name);
            }
            entry
        })
        .collect();
    Ok(Json(serde_json::json!({
        "object": "list",
        "data": data,
    }))
    .into_response())
}

/// Native Codex Responses HTTPS fallback. The request body is forwarded as raw
/// bytes because Codex sends zstd-compressed JSON and expects SSE framing.
///
/// For a long time this was the only ingress route that never called
/// `push_trace`, which is why Codex traffic was invisible in the Proxy Trace
/// overlay. It now records a Codex/Responses entry on the same `next_req_id`
/// sequence as every other route.
pub async fn post_codex_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Result<Response, AppError> {
    // A disabled client means the user does not use that client at all, so mHD
    // should do no work for it — a resource switch, not an access-control
    // feature. Refuse before any body parsing or upstream work. The WebSocket
    // fallback still answers 404 regardless (see `get_codex_websocket_fallback`).
    if !state.client_enabled(ClientKind::Codex) {
        return Err(AppError::disabled_client(ClientKind::Codex));
    }

    let req_id = state.next_req_id();
    let started_ms = crate::providers::now_unix_ms();
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let client_run_id = client_run_id(&headers);

    // Decode once for corpus capture and the opt-in HTTPS trim. The original
    // payload is always what gets captured; trim must never rewrite the replay
    // corpus or make a failed transformation unrecoverable.
    let decoded_payload = crate::providers::codex::decode_request(&headers, &body).ok();
    let mut model_opt: Option<String> = None;
    if let Some(payload) = decoded_payload.as_ref() {
        model_opt = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        state.capture_request_body(req_id, model_opt.as_deref(), "codex", &payload);
    }
    let model = model_opt.unwrap_or_default();

    let mut outgoing_body = body.clone();
    let mut trim_applied = false;
    let mut trim_tokens_before = 0;
    let mut trim_tokens_after = 0;
    let mut trim_preset = String::new();
    let mut trim_config_json = String::new();
    let mut trim_stages_json = String::new();
    if state.trim_enabled_for(ClientKind::Codex)
        && let Some(payload) = decoded_payload.as_ref()
    {
        let outcome = crate::codex_trim::trim_responses(payload.clone());
        if outcome.applied
            && let Ok(encoded) = crate::providers::codex::encode_request(&headers, &outcome.body)
        {
            outgoing_body = encoded;
            trim_applied = true;
            trim_tokens_before = outcome.tokens_before;
            trim_tokens_after = outcome.tokens_after;
            trim_preset = "responses-v1".to_string();
            trim_config_json = serde_json::json!({
                "engine": "codex_responses",
                "stage": outcome.stages.first().map(|stage| stage.name).unwrap_or("none"),
                "classes": &outcome.classes,
            })
            .to_string();
            trim_stages_json = serde_json::to_string(
                &outcome
                    .stages
                    .iter()
                    .map(|stage| stage.name)
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default();
        }
    }

    // Record the request before forwarding so in-flight Codex requests are
    // visible in the trace. `tier`/`effective_tier` are None — Codex is not a
    // Claude tier. `status` and the token counts land when the usage tap
    // closes the row at end-of-stream for successes, and immediately on the
    // error path below.
    let target = state.codex_target_for_model(&model);
    state.push_trace(TraceEntry {
        seq: req_id,
        client: ClientKind::Codex,
        wire_api: WireApi::Responses,
        tier: None,
        effective_tier: None,
        target: target.as_str().to_string(),
        model: model.clone(),
        downgraded: false,
        reason: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        trim_applied,
        trim_tokens_before,
        trim_tokens_after,
        trim_preset,
        trim_config_json,
        trim_stages_json,
        started_ms,
        prefix_hash: 0,
        status: None,
        is_probe: false,
        user_agent,
        client_run_id,
        prefix: crate::prefix::PrefixStats::default(),
    });

    // Forward the raw bytes. On success the response is handed to a usage tap
    // that scans the SSE stream for the terminal `response.completed` event and
    // extracts its `usage` integers — metadata only, never prompt text, OAuth
    // tokens or headers. The tap yields chunks downstream unchanged and closes
    // the row at end-of-stream, so status/duration/tokens all measure the full
    // stream rather than the response headers.
    let result = match &target {
        CodexTarget::Native => {
            crate::providers::codex::forward(
                &state,
                axum::http::Method::POST,
                uri.path(),
                uri.query(),
                &headers,
                outgoing_body,
            )
            .await
        }
        CodexTarget::Model(model) => {
            crate::providers::codex::forward_side(&state, &headers, outgoing_body, model).await
        }
    };
    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            // The upstream never answered — close the request row so it does
            // not dangle (mirrors the failure path in the provider modules).
            state.mark_request_failed(
                req_id,
                Some(crate::providers::now_unix_ms().saturating_sub(started_ms)),
                None,
                &err.to_string(),
                "CODEX_ERR",
            );
            return Err(AppError::from(err));
        }
    };
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        // Success: install the usage tap and let it close the row when the
        // stream ends. The token counts ride the terminal `response.completed`
        // SSE event, only observable at end-of-stream, so the entry stays
        // "in flight" in the overlay until then — the correct reading for a
        // streaming response, and it makes the duration measure the stream
        // rather than the headers.
        Ok(crate::providers::codex::tap_response_usage(
            state.clone(),
            req_id,
            started_ms,
            status,
            resp,
        ))
    } else {
        // Error response: the body is not an SSE stream, so there is no usage
        // to extract. Close the row now with the duration measured to the
        // headers, mirroring the header-time close the tap replaces.
        let duration_ms = crate::providers::now_unix_ms().saturating_sub(started_ms);
        state.update_trace_tokens(
            req_id,
            0,
            0,
            None,
            None,
            Some(duration_ms),
            Some(status),
            None,
        );
        Ok(resp)
    }
}

struct ActiveCodexWebSocketRequest {
    req_id: u64,
    started_ms: u64,
    _guard: providers::InflightGuard,
}

type ActiveCodexWebSocket = Arc<TokioMutex<Option<ActiveCodexWebSocketRequest>>>;

/// Codex first attempts a WebSocket upgrade. Native requests are bridged to
/// the official ChatGPT endpoint. When a `gpt-5.4` request is selected for a
/// side provider, the bridge closes before forwarding it so Codex reconnects
/// through the HTTPS adapter, which is the only path that can translate to
/// Chat Completions.
pub async fn get_codex_websocket(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    if !state.client_enabled(ClientKind::Codex) {
        return Err(AppError::disabled_client(ClientKind::Codex));
    }
    let (upstream, response_headers) = crate::providers::codex::connect_websocket(&headers)
        .await
        .map_err(AppError::from)?;
    let on_upgrade = ws.on_upgrade(move |client| async move {
        bridge_codex_websocket(state, headers, client, upstream).await;
    });
    let mut response = on_upgrade.into_response();
    for name in [
        "x-reasoning-included",
        "x-models-etag",
        "openai-model",
        "x-codex-turn-state",
    ] {
        if let Some(value) = response_headers.get(name) {
            response.headers_mut().insert(name, value.clone());
        }
    }
    Ok(response)
}

async fn bridge_codex_websocket(
    state: Arc<AppState>,
    headers: HeaderMap,
    client: WebSocket,
    upstream: crate::providers::codex::CodexWebSocket,
) {
    let active: ActiveCodexWebSocket = Arc::new(TokioMutex::new(None));
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    let active_from_client = Arc::clone(&active);
    let state_from_client = Arc::clone(&state);
    let headers_from_client = headers.clone();
    let mut client_to_upstream = tokio::spawn(async move {
        while let Some(Ok(message)) = client_rx.next().await {
            let message = match message {
                AxumMessage::Text(text) => {
                    let original = text.to_string();
                    // A WebSocket can only carry the native Codex protocol.
                    // If this logical request is the selected offload model,
                    // close before forwarding it so Codex reconnects through
                    // the HTTPS side adapter. Other models remain native.
                    if codex_request_requires_https(&state_from_client, &original) {
                        let _ = upstream_tx
                            .send(tokio_tungstenite::tungstenite::Message::Close(None))
                            .await;
                        break;
                    }
                    let mut trim_outcome = None;
                    let outgoing = if state_from_client.trim_enabled_for(ClientKind::Codex) {
                        if let Some(outcome) = crate::codex_trim::trim_responses_text(&original) {
                            if outcome.applied {
                                trim_outcome = Some(outcome);
                            }
                        }
                        crate::codex_trim::trim_responses_text_if_enabled(&original, true)
                    } else {
                        crate::codex_trim::trim_responses_text_if_enabled(&original, false)
                    };
                    observe_codex_request(
                        &state_from_client,
                        &headers_from_client,
                        &active_from_client,
                        &original,
                        trim_outcome.as_ref(),
                    )
                    .await;
                    AxumMessage::Text(outgoing.into())
                }
                message => message,
            };
            let Some(message) = crate::providers::codex::to_tungstenite_message(message) else {
                continue;
            };
            if upstream_tx.send(message).await.is_err() {
                break;
            }
        }
    });

    let active_from_upstream = Arc::clone(&active);
    let state_from_upstream = Arc::clone(&state);
    let mut upstream_to_client = tokio::spawn(async move {
        while let Some(Ok(message)) = upstream_rx.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = &message {
                observe_codex_response(&state_from_upstream, &active_from_upstream, text.as_str())
                    .await;
            }
            let Some(message) = crate::providers::codex::to_axum_message(message) else {
                continue;
            };
            if client_tx.send(message).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut client_to_upstream => upstream_to_client.abort(),
        _ = &mut upstream_to_client => client_to_upstream.abort(),
    }

    // Dropping an active guard marks the logical request as client-cancelled.
    let _ = active.lock().await.take();
}

fn codex_request_requires_https(state: &Arc<AppState>, text: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if payload.get("type").and_then(Value::as_str) != Some("response.create") {
        return false;
    }
    let model = payload.get("model").and_then(Value::as_str).unwrap_or_default();
    state.codex_target_for_model(model) != CodexTarget::Native
}

async fn observe_codex_request(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    active: &ActiveCodexWebSocket,
    text: &str,
    trim: Option<&crate::codex_trim::TrimOutcome>,
) {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if payload.get("type").and_then(Value::as_str) != Some("response.create") {
        return;
    }

    let _ = active.lock().await.take();
    let req_id = state.next_req_id();
    let started_ms = crate::providers::now_unix_ms();
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let is_probe = payload.get("generate").and_then(Value::as_bool) == Some(false);
    state.capture_request_body(req_id, Some(&model), "codex", &payload);
    let (trim_applied, trim_tokens_before, trim_tokens_after, trim_config_json, trim_stages_json) =
        if let Some(outcome) = trim {
            (
                true,
                outcome.tokens_before,
                outcome.tokens_after,
                serde_json::json!({
                    "engine": "codex_responses",
                    "transport": "websocket",
                    "stage": outcome.stages.first().map(|stage| stage.name).unwrap_or("none"),
                    "classes": &outcome.classes,
                })
                .to_string(),
                serde_json::to_string(
                    &outcome
                        .stages
                        .iter()
                        .map(|stage| stage.name)
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default(),
            )
        } else {
            (false, 0, 0, String::new(), String::new())
        };
    state.push_trace(TraceEntry {
        seq: req_id,
        client: ClientKind::Codex,
        wire_api: WireApi::Responses,
        tier: None,
        effective_tier: None,
        target: CodexTarget::Native.as_str().to_string(),
        model,
        downgraded: false,
        reason: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: None,
        cache_creation_tokens: None,
        trim_applied,
        trim_tokens_before,
        trim_tokens_after,
        trim_preset: if trim_applied {
            "responses-v1".to_string()
        } else {
            String::new()
        },
        trim_config_json,
        trim_stages_json,
        started_ms,
        prefix_hash: 0,
        status: None,
        is_probe,
        user_agent: headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        client_run_id: client_run_id(headers),
        prefix: crate::prefix::PrefixStats::default(),
    });
    *active.lock().await = Some(ActiveCodexWebSocketRequest {
        req_id,
        started_ms,
        _guard: providers::InflightGuard::new(Arc::clone(state), req_id),
    });
}

async fn observe_codex_response(state: &Arc<AppState>, active: &ActiveCodexWebSocket, text: &str) {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let terminal = event_type == "response.completed"
        || event_type == "response.failed"
        || event_type == "response.incomplete"
        || event_type == "error";
    if !terminal {
        return;
    }
    let Some(request) = active.lock().await.take() else {
        return;
    };
    let duration_ms = crate::providers::now_unix_ms().saturating_sub(request.started_ms);
    if event_type == "response.completed" {
        let usage = crate::providers::codex::parse_responses_usage(&format!("data: {text}"))
            .unwrap_or((0, 0, None));
        state.update_trace_tokens(
            request.req_id,
            usage.0,
            usage.1,
            usage.2,
            None,
            Some(duration_ms),
            Some(200),
            None,
        );
    } else {
        let status = payload
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .or(Some(502));
        state.mark_request_failed(
            request.req_id,
            Some(duration_ms),
            status,
            "Codex WebSocket response failed",
            "CODEX_WS_ERR",
        );
    }
    drop(request);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Drain a response body (from an `AppError` render) into its JSON value.
    async fn error_body(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("read error body");
        serde_json::from_slice(&bytes).expect("error body is JSON")
    }

    /// The disabled-client gate must refuse each ingress route with a 503
    /// `mhd_client_disabled` carrying the client's slot — before any body
    /// parsing or upstream work happens.
    #[tokio::test]
    async fn disabled_client_gate_returns_503_with_slot() {
        let state = AppState::from_config(&Config::default());
        let expected = |slot: &str| {
            serde_json::json!({
                "error": {
                    "type": "mhd_client_disabled",
                    "message": format!("mHD proxy is disabled for client '{slot}'"),
                }
            })
        };

        // Claude Code route (`/v1/messages`).
        state.set_client_enabled(ClientKind::ClaudeCode, false);
        let resp = post_messages(
            State(state.clone()),
            HeaderMap::new(),
            Json(serde_json::json!({})),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_body(resp).await, expected("claude_code"));

        // OpenAI-compatible route (`/v1/chat/completions`).
        state.set_client_enabled(ClientKind::OpenAi, false);
        let resp = post_chat_completions(
            State(state.clone()),
            HeaderMap::new(),
            Json(serde_json::json!({})),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_body(resp).await, expected("openai"));

        // Codex route (`POST /v1/responses`).
        state.set_client_enabled(ClientKind::Codex, false);
        let resp = post_codex_responses(
            State(state.clone()),
            HeaderMap::new(),
            Uri::from_static("/v1/responses"),
            Bytes::new(),
        )
        .await
        .unwrap_err()
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_body(resp).await, expected("codex"));
    }

    /// With Codex enabled the gate must NOT fire, and the request must still be
    /// recorded in the trace as a Codex/Responses entry with no Claude tier.
    ///
    /// The side-target path is chosen for gpt-5.4 with a body that is valid JSON
    /// but has no input. The adapter bails before any network I/O — the trace
    /// entry is pushed before forwarding and survives the failure.
    #[tokio::test]
    async fn enabled_codex_path_is_not_gated_and_records_trace_entry() {
        let state = AppState::from_config(&Config::default());
        assert!(state.client_enabled(ClientKind::Codex));

        state.set_codex_target(CodexTarget::Model("test-side-model".to_string()));
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"model":"gpt-5.4"})).expect("encode body"),
        );
        let err = post_codex_responses(
            State(state.clone()),
            HeaderMap::new(),
            Uri::from_static("/v1/responses"),
            body,
        )
        .await
        .unwrap_err();
        // The gate did NOT fire: the request proceeded into forwarding and
        // failed there, so this is the generic 502, never the 503 gate.
        assert_eq!(err.into_response().status(), StatusCode::BAD_GATEWAY);

        let snapshot = state.trace_snapshot();
        let entry = snapshot.last().expect("codex request recorded in trace");
        assert_eq!(entry.client, ClientKind::Codex);
        assert_eq!(entry.wire_api, WireApi::Responses);
        assert_eq!(entry.tier, None);
        assert_eq!(entry.effective_tier, None);
        assert_eq!(entry.target, "test-side-model");
        assert!(!entry.downgraded);
        assert!(!entry.trim_applied);
    }
}
