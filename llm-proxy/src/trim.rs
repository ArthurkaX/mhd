//! Lossless/lossy request compression via [`llmtrim_core`].
//!
//! Runs the outgoing Anthropic Messages body through
//! [`llmtrim_core::rewrite_request`] to trim tool outputs, logs, diffs,
//! duplicate lines, fat JSON arrays and tool schemas — while leaving any
//! `cache_control`-frozen prefix byte-identical so the Anthropic prompt cache
//! keeps hitting.
//!
//! **Fail-open design:** any error, or a result that doesn't shrink, returns the
//! original body unchanged. Trim never breaks a request.

use serde_json::Value;

use llmtrim_core::compress_with_config;
use llmtrim_core::config::DenseConfig;
use llmtrim_core::ir::ProviderKind;

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

/// Run a provider-agnostic request body through `llmtrim-core`'s `rewrite_request`.
///
/// # Fail-open
///
/// Returns the original body in all of these cases:
/// - Body below [`TRIM_MIN_BYTES`].
/// - `rewrite_request` returns an error.
/// - Result JSON fails to re-parse.
/// - Result is not smaller (`after ≥ before`).
fn trim_with_provider(
    payload: Value,
    provider: Option<ProviderKind>,
    preset: &str,
    custom_config: Option<&DenseConfig>,
    config_json: String,
) -> TrimOutcome {
    let original = payload;
    let preset = preset.to_string();

    // Cheap guard: skip tiny bodies — nothing to gain, pure overhead.
    let input = match serde_json::to_string(&original) {
        Ok(s) if s.len() >= TRIM_MIN_BYTES => s,
        _ => {
            return TrimOutcome {
                body: original,
                applied: false,
                tokens_before: 0,
                tokens_after: 0,
                preset,
                stages: Vec::new(),
                config_json: String::new(),
            };
        }
    };

    let result = match custom_config {
        Some(cfg) => compress_with_config(&input, provider, cfg),
        None => llmtrim_core::rewrite_request(&input, provider, Some(&preset)),
    };

    match result {
        Ok(res) if res.input_tokens_after.0 < res.input_tokens_before.0 => {
            match serde_json::from_str::<Value>(&res.request_json) {
                Ok(body) => TrimOutcome {
                    body,
                    applied: true,
                    tokens_before: res.input_tokens_before.0 as u64,
                    tokens_after: res.input_tokens_after.0 as u64,
                    preset,
                    stages: res
                        .stages
                        .iter()
                        .map(|s| StageInfo {
                            name: s.name.clone(),
                            applied: s.applied,
                            tokens_before: s.tokens_before.0 as u64,
                            tokens_after: s.tokens_after.0 as u64,
                            note: s.note.clone(),
                        })
                        .collect(),
                    config_json,
                },
                Err(_) => TrimOutcome {
                    body: original,
                    applied: false,
                    tokens_before: 0,
                    tokens_after: 0,
                    preset,
                    stages: Vec::new(),
                    config_json: String::new(),
                },
            }
        }
        _ => TrimOutcome {
            body: original,
            applied: false,
            tokens_before: 0,
            tokens_after: 0,
            preset,
            stages: Vec::new(),
            config_json: String::new(),
        },
    }
}

/// Run the Anthropic request body through `llmtrim-core`'s `rewrite_request`.
///
/// # Fail-open
///
/// Returns the original body in all of these cases:
/// - Body below [`TRIM_MIN_BYTES`].
/// - `rewrite_request` returns an error.
/// - Result JSON fails to re-parse.
/// - Result is not smaller (`after ≥ before`).
pub fn trim_anthropic(payload: Value, preset: &str) -> TrimOutcome {
    // Build combined config from the accepted Phase 2 tunings.
    // Falls back to preset-based rewrite_request if the preset is unknown.
    let cfg = DenseConfig::preset(preset).map(|mut cfg| {
        cfg.normalize_unicode = true;
        cfg.toolout_max_lines = 25;
        cfg.tool_max_desc_chars = 150;
        cfg
    });
    // Snapshot the active knobs so the DB row knows exactly what ran.
    let config_json = serde_json::json!({
        "preset": preset,
        "normalize_unicode": true,
        "toolout_max_lines": 25,
        "tool_max_desc_chars": 150,
    })
    .to_string();
    trim_with_provider(payload, Some(ProviderKind::Anthropic), preset, cfg.as_ref(), config_json)
}

/// Run an OpenAI-format request body through `llmtrim-core`'s `rewrite_request`.
///
/// # Fail-open
///
/// Same guarantees as [`trim_anthropic`].
pub fn trim_openai(payload: Value, preset: &str) -> TrimOutcome {
    let config_json = serde_json::json!({ "preset": preset }).to_string();
    trim_with_provider(payload, Some(ProviderKind::OpenAi), preset, None, config_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small body below TRIM_MIN_BYTES should be passed through untouched.
    #[test]
    fn small_body_is_passthrough() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = trim_anthropic(body.clone(), "agent");
        assert!(!out.applied, "small body should not be trimmed");
        assert_eq!(out.body, body, "body should be unchanged");
    }

    /// An empty-ish payload should pass through.
    #[test]
    fn minimal_body_is_passthrough() {
        let body = serde_json::json!({"model": "claude-sonnet-4-20250514"});
        let out = trim_anthropic(body.clone(), "agent");
        assert!(!out.applied);
    }

    /// An unknown preset should not cause a panic and should fall back gracefully.
    #[test]
    fn unknown_preset_fallsthrough() {
        // Build a body large enough to trigger the trim attempt
        let msgs = vec![serde_json::json!({"role": "user", "content": "X".repeat(5000)})];
        let body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": msgs
        });
        // This should not panic — unknown preset may cause an Err or no-op
        let out = trim_anthropic(body, "nonexistent_preset_xyz");
        // Must not panic; applied may be true or false
        let _ = out;
    }

    /// Large OpenAI-shaped body with `trim_openai` should not panic.
    #[test]
    fn large_openai_body_no_panic() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "X".repeat(5000)})];
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": msgs
        });
        let out = trim_openai(body, "auto");
        // Must not panic; outcome is best-effort
        let _ = out;
    }
}
