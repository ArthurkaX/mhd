//! Lossless/lossy request compression via the native trim engine.
//!
//! Runs the outgoing request body through
//! [`crate::native_trim::trim_native`] / [`crate::native_trim::trim_native_openai`]
//! to trim tool outputs, logs, diffs, duplicate lines, fat JSON arrays and tool
//! schemas — while leaving any `cache_control`-frozen prefix byte-identical so
//! the Anthropic prompt cache keeps hitting.
//!
//! **Fail-open design:** any error, or a result that doesn't shrink, returns the
//! original body unchanged. Trim never breaks a request.

use serde_json::Value;

/// Pick the knob set for a resolved request target: the designated free-tier
/// `free_target` (if non-empty and matching) gets the light, non-lossy profile;
/// everything else gets the caller-supplied live-tunable knobs.
pub fn resolve_knobs(
    target: &str,
    free_target: &str,
    live: crate::native_trim::NativeKnobs,
) -> crate::native_trim::NativeKnobs {
    if !free_target.is_empty() && target == free_target {
        crate::native_trim::NativeKnobs::light()
    } else {
        live
    }
}

/// Minimum serialised body length (in bytes) before Trim even attempts work.
/// Below this, there is nothing worth compressing.
const TRIM_MIN_BYTES: usize = 4096;

/// Report for a single compression stage.
#[derive(Debug, Clone)]
pub struct StageInfo {
    pub name: String,
    pub applied: bool,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub note: Option<String>,
}

/// Outcome of a single trim attempt.
#[derive(Debug, Clone)]
pub struct TrimOutcome {
    /// The body to forward — rewritten, or the original on any failure/no-gain.
    pub body: Value,
    /// `true` only when we actually swapped in a smaller body.
    pub applied: bool,
    /// Estimated input tokens before trimming (0 if not applicable).
    pub tokens_before: u64,
    /// Estimated input tokens after trimming (0 if not applicable).
    pub tokens_after: u64,
    /// Active preset name.
    pub preset: String,
    /// Per-stage reports (empty if not applied).
    pub stages: Vec<StageInfo>,
    /// JSON snapshot of the active compression knobs (empty when not applied).
    pub config_json: String,
}

/// Run the Anthropic request body through the native trim engine.
///
/// # Fail-open
///
/// Returns the original body when:
/// - Body is below [`TRIM_MIN_BYTES`].
/// - The trimmed result is not smaller (`after ≥ before`).
/// - Any serialization error occurs.
pub fn trim_anthropic(payload: Value, native_knobs: crate::native_trim::NativeKnobs) -> TrimOutcome {
    // Cheap guard: skip tiny bodies — nothing to gain.
    let before_str = match serde_json::to_string(&payload) {
        Ok(s) if s.len() >= TRIM_MIN_BYTES => s,
        _ => {
            return TrimOutcome {
                body: payload,
                applied: false,
                tokens_before: 0,
                tokens_after: 0,
                preset: "native".to_string(),
                stages: Vec::new(),
                config_json: String::new(),
            };
        }
    };
    let tokens_before = (before_str.len() / 4) as u64;
    let knobs = native_knobs;
    let config_json = serde_json::json!({
        "engine": "native",
        "tool_max_desc_chars": knobs.tool_max_desc_chars,
        "tool_result_head": knobs.tool_result_head,
        "tool_result_tail": knobs.tool_result_tail,
        "ws_enabled": knobs.ws_enabled,
        "min_elide": knobs.tool_result_min_elide,
        "strip_thinking": knobs.strip_thinking,
        "fence_requires_code": knobs.tool_result_fence_requires_code,
        "arrow_density_min": knobs.tool_result_arrow_density_min,
    })
    .to_string();
    // Clone so we can fall back to the original on no-gain.
    let original = payload.clone();
    let trimmed = crate::native_trim::trim_native(payload, &knobs);
    let tokens_after = serde_json::to_string(&trimmed)
        .map(|s| (s.len() / 4) as u64)
        .unwrap_or(tokens_before);
    if tokens_after < tokens_before {
        TrimOutcome {
            body: trimmed,
            applied: true,
            tokens_before,
            tokens_after,
            preset: "native".to_string(),
            stages: Vec::new(),
            config_json,
        }
    } else {
        // No gain — forward the original unchanged.
        TrimOutcome {
            body: original,
            applied: false,
            tokens_before: 0,
            tokens_after: 0,
            preset: "native".to_string(),
            stages: Vec::new(),
            config_json: String::new(),
        }
    }
}

/// Run an OpenAI chat-completions body through the native trim engine.
///
/// Same fail-open guarantees as [`trim_anthropic`]: tiny bodies, no-gain results,
/// and serialization errors all return the original body unchanged.
pub fn trim_openai(payload: Value, native_knobs: crate::native_trim::NativeKnobs) -> TrimOutcome {
    // Cheap guard: skip tiny bodies — nothing to gain.
    let before_str = match serde_json::to_string(&payload) {
        Ok(s) if s.len() >= TRIM_MIN_BYTES => s,
        _ => {
            return TrimOutcome {
                body: payload,
                applied: false,
                tokens_before: 0,
                tokens_after: 0,
                preset: "native".to_string(),
                stages: Vec::new(),
                config_json: String::new(),
            };
        }
    };
    let tokens_before = (before_str.len() / 4) as u64;
    let knobs = native_knobs;
    let config_json = serde_json::json!({
        "engine": "native",
        "shape": "openai",
        "tool_max_desc_chars": knobs.tool_max_desc_chars,
        "tool_result_head": knobs.tool_result_head,
        "tool_result_tail": knobs.tool_result_tail,
        "ws_enabled": knobs.ws_enabled,
        "min_elide": knobs.tool_result_min_elide,
        "fence_requires_code": knobs.tool_result_fence_requires_code,
        "arrow_density_min": knobs.tool_result_arrow_density_min,
    })
    .to_string();
    // Clone so we can fall back to the original on no-gain.
    let original = payload.clone();
    let trimmed = crate::native_trim::trim_native_openai(payload, &knobs);
    let tokens_after = serde_json::to_string(&trimmed)
        .map(|s| (s.len() / 4) as u64)
        .unwrap_or(tokens_before);
    if tokens_after < tokens_before {
        TrimOutcome {
            body: trimmed,
            applied: true,
            tokens_before,
            tokens_after,
            preset: "native".to_string(),
            stages: Vec::new(),
            config_json,
        }
    } else {
        // No gain — forward the original unchanged.
        TrimOutcome {
            body: original,
            applied: false,
            tokens_before: 0,
            tokens_after: 0,
            preset: "native".to_string(),
            stages: Vec::new(),
            config_json: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_trim::NativeKnobs;

    // ── resolve_knobs ────────────────────────────────────────────────────

    #[test]
    fn resolve_knobs_matches_free_target() {
        let result = resolve_knobs("some-id", "some-id", NativeKnobs::default());
        assert_eq!(result.tool_max_desc_chars, usize::MAX);
        assert_eq!(result.tool_result_min_elide, usize::MAX);
        assert!(result.ws_enabled);
        assert!(result.strip_thinking);
    }

    #[test]
    fn resolve_knobs_non_match_returns_live() {
        let live = NativeKnobs {
            tool_max_desc_chars: 42,
            ..NativeKnobs::default()
        };
        let result = resolve_knobs("some-id", "other-id", live);
        assert_eq!(result.tool_max_desc_chars, 42);
    }

    #[test]
    fn resolve_knobs_empty_free_target_disabled() {
        let live = NativeKnobs {
            tool_max_desc_chars: 99,
            ..NativeKnobs::default()
        };
        // target != "" (non-empty target, empty free_target — always live)
        let result = resolve_knobs("anything", "", live);
        assert_eq!(result.tool_max_desc_chars, 99);

        // target == "" too — empty free_target can never accidentally match
        let result2 = resolve_knobs("", "", NativeKnobs::default());
        assert_eq!(result2.tool_max_desc_chars, NativeKnobs::default().tool_max_desc_chars);
    }

    /// A small body below TRIM_MIN_BYTES should be passed through untouched.
    #[test]
    fn small_body_is_passthrough() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = trim_anthropic(body.clone(), NativeKnobs::default());
        assert!(!out.applied, "small body should not be trimmed");
        assert_eq!(out.body, body, "body should be unchanged");
    }

    /// An empty-ish payload should pass through.
    #[test]
    fn minimal_body_is_passthrough() {
        let body = serde_json::json!({"model": "claude-sonnet-4-20250514"});
        let out = trim_anthropic(body.clone(), NativeKnobs::default());
        assert!(!out.applied);
    }

    /// A large body should not panic.
    #[test]
    fn large_body_no_panic() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "X".repeat(5000)})];
        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": msgs
        });
        let out = trim_anthropic(body, NativeKnobs::default());
        let _ = out;
    }

    /// Large OpenAI-shaped body should not panic.
    #[test]
    fn large_openai_body_no_panic() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "X".repeat(5000)})];
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": msgs
        });
        let out = trim_openai(body, NativeKnobs::default());
        let _ = out;
    }

    /// Native engine on a body with a long tool/function description:
    /// applied==true, body is smaller, config_json contains "shape":"openai".
    #[test]
    fn openai_native_long_desc_applied() {
        let long_desc = "D".repeat(500);
        let filler = "X".repeat(4000);
        let body = serde_json::json!({
            "model": "gpt-4o",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "do_thing",
                    "description": long_desc,
                    "parameters": { "type": "object", "properties": {} }
                }
            }],
            "messages": [{ "role": "user", "content": filler }]
        });
        let knobs = NativeKnobs {
            tool_max_desc_chars: 50,
            ..Default::default()
        };
        let out = trim_openai(body, knobs);
        assert!(out.applied, "native engine should apply on a long description");
        let body_str = serde_json::to_string(&out.body).unwrap();
        assert!(
            body_str.len() < serde_json::to_string(&serde_json::json!({
                "model": "gpt-4o",
                "tools": [{"type":"function","function":{"name":"do_thing","description":"D".repeat(500),"parameters":{"type":"object","properties":{}}}}],
                "messages":[{"role":"user","content":"X".repeat(4000)}]
            })).unwrap().len(),
            "trimmed body must be smaller"
        );
        assert!(
            out.config_json.contains("\"shape\":\"openai\""),
            "config_json must contain shape:openai, got: {}",
            out.config_json
        );
    }

    /// Native engine on a tiny body (< TRIM_MIN_BYTES): applied==false,
    /// body is returned unchanged.
    #[test]
    fn openai_native_tiny_body_passthrough() {
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let body_clone = body.clone();
        let out = trim_openai(body, NativeKnobs::default());
        assert!(!out.applied, "tiny body must not be applied");
        assert_eq!(out.body, body_clone, "tiny body must be returned unchanged");
    }
}
