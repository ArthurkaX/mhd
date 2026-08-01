//! Axum route handlers for the LLM proxy.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;

use crate::providers;
use crate::state::{AppState, CodexTarget, Target, Tier, TraceEntry};

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
    let payload = if *state.trim_enabled.read().unwrap_or_else(|e| e.into_inner()) {
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
            .map_err(|e| AppError(e.into()));
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
/// is applied under the same single `state.trim_enabled` flag that governs the
/// `/v1/messages` path.
///
/// The request is recorded in the Proxy Trace ring buffer as a `Tier::OpenAi`
/// passthrough (model + trim stats now, token usage filled from the upstream
/// response), so OpenAI clients appear alongside Claude Code traffic.
pub async fn post_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
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
    // Same single flag as the /v1/messages path.
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
    let payload = if *state.trim_enabled.read().unwrap_or_else(|e| e.into_inner()) {
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
    // Tokens are 0 here and get filled from the upstream response usage (see `*_raw_openai`).
    // prefix_hash is 0 for OpenAI passthrough (no Anthropic-style system/tools prefix).
    state.push_trace(TraceEntry {
        seq: req_id,
        tier: Tier::OpenAi,
        effective_tier: Tier::OpenAi,
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
            .map_err(|e| AppError(e.into()));
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
pub async fn post_codex_responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Result<Response, AppError> {
    // Capture the decoded native wire request only when the existing explicit
    // corpus-capture setting is enabled. The raw OAuth header is never stored.
    if let Ok(payload) = crate::providers::codex::decode_request(&headers, &body) {
        let req_id = state.next_req_id();
        let model = payload.get("model").and_then(Value::as_str);
        state.capture_request_body(req_id, model, "codex", &payload);
    }
    match state.codex_target() {
        CodexTarget::Native => {}
        CodexTarget::Model(model) => {
            return Ok(crate::providers::codex::forward_side(&state, &headers, body, &model).await?);
        }
    }
    Ok(crate::providers::codex::forward(
        &state,
        axum::http::Method::POST,
        uri.path(),
        uri.query(),
        &headers,
        body,
    )
    .await?)
}

/// Codex first attempts a WebSocket upgrade. We intentionally return a quick
/// 404 so the supported HTTPS/SSE fallback is selected; no OAuth is sent to a
/// different destination and no fake WebSocket framing is attempted.
pub async fn get_codex_websocket_fallback() -> StatusCode {
    StatusCode::NOT_FOUND
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

    // Capture the previous target for this slot so the switch is auditable.
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

    // Record the model/provider switch so cache misses after it are explainable.
    state.log_event(crate::db_log::LogEvent {
        seq: 0,
        event_type: "MODEL_SWITCH".to_string(),
        target: Some(if slot == "codex" { body.id.clone() } else { target.as_str().to_string() }),
        model: Some(slot.clone()),
        reason: Some(format!(
            "slot={} {} -> {}",
            slot,
            old_target,
            target.as_str()
        )),
        ..Default::default()
    });

    // Persist the change so it survives restarts.
    if let Err(e) = crate::config::save(&state.to_config()) {
        tracing::warn!("Failed to persist config: {e}");
    }

    let effective_target = if slot == "codex" {
        state.codex_target().as_str().to_string()
    } else {
        target.as_str().to_string()
    };
    Ok(Json(serde_json::json!({
        "status": "ok",
        "slot": slot,
        "target": effective_target,
    })))
}

/// `GET /config` — show the current effective routing config (keys masked).
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

/// `POST /debug` — toggle debug dump mode.
pub async fn toggle_debug(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut ll = state.log_level.write().unwrap_or_else(|e| e.into_inner());
    let new = match *ll {
        crate::state::DebugLevel::None => crate::state::DebugLevel::Maximal,
        _ => crate::state::DebugLevel::None,
    };
    *ll = new;
    Json(serde_json::json!({ "log_level": new.as_str() }))
}

/// `GET /health` — health check.
/// `GET /stats/routes` — per-route cache verdict, so a client can decide at
/// runtime whether a route is worth shaping requests for instead of hardcoding
/// an assumption.
///
/// Each entry reports two independent booleans:
///   * `reports` — the route sends cache fields at all.
///   * `caches`  — it has actually served cached tokens.
///
/// `reports: false` means **nothing is known** about this route's caching. It is
/// not the same answer as `reports: true, caches: false` ("asked, told no"), and
/// clients must not collapse the two. `reporting_requests` is exposed so callers
/// can see how much evidence is behind the verdict.
///
/// Counts cover successful requests only, and only those logged since the
/// schema-v3 migration — older rows cannot distinguish absent from zero. A route
/// that has only pre-migration traffic is therefore absent from this list
/// entirely, which again means "unknown", not "does not cache".
/// Returns an empty array when the DB log is disabled.
pub async fn get_route_stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let rows = state.route_cache();
    Json(Value::Array(
        rows.into_iter()
            .map(|r| {
                serde_json::json!({
                    "route": r.route,
                    "reports": r.reports(),
                    "caches": r.caches(),
                    "requests": r.requests,
                    "reporting_requests": r.reporting_requests,
                    "cache_read_tokens": r.cache_read_tokens,
                    "cache_creation_tokens": r.cache_creation_tokens,
                    "last_seen": r.last_seen,
                })
            })
            .collect(),
    ))
}

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
