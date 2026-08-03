//! Conservative, pure trimming for OpenAI Responses request bodies.
//!
//! Only text inside tool outputs may be normalized;
//! calls, ids, reasoning, messages, tools, and backend-owned state remain
//! untouched. Any shape outside that contract returns the original payload.
//!
//! Protection is by *provenance* — the command that produced an output, matched
//! through its `call_id` — not by content class. Outputs that are file reads of
//! code/doc files, orphaned outputs (no matching call), and `apply_patch`
//! context are left byte-identical; everything else is eligible for
//! whitespace / repeated-line / head-tail compression.

use std::collections::HashMap;

use serde_json::Value;

use crate::content_mix::{ContentClass, classify_block};
use crate::native_trim::{
    compress_head_tail, is_protected_ext, tool_result_protected, truncate_descriptions,
};

// Small Codex turns are common. Keep a floor to avoid touching tiny payloads,
// but let the content-aware pass inspect bodies from 8 KiB upward. The pass
// still changes only recognized tool-output text and remains fail-open.
const MIN_BYTES: usize = 8 * 1024;
// Codex diagnostics in the observed corpus are shorter than the native CC
// 8K trigger. Keep a substantial head and tail, but make the Codex gate fit
// those real tool outputs without widening eligibility to unknown content.
const SAFE_HEAD_CHARS: usize = 2_000;
const SAFE_TAIL_CHARS: usize = 800;
const SAFE_MIN_ELIDE: usize = 2_000;
// CC parity: truncate tool `description` strings to the same cap as the native
// engine's `NativeKnobs::tool_max_desc_chars` default.
const MAX_DESC_CHARS: usize = 150;

// ── provenance-gated protection (A2) ─────────────────────────────────────────

/// Item-level protection reason labels. A block protected by provenance (or
/// orphaned) is left byte-identical. Per-block content protection ("content")
/// and the eligible fallback are decided inside `transform_tool_text`.
const ORPHAN: &str = "orphan";
const PROVENANCE_READ: &str = "provenance_read";
const APPLY_PATCH: &str = "apply_patch";

/// Read verbs for the A2.2 file-read rule: a command is a protected file read
/// when it contains one of these verbs AND a token whose extension is in the
/// protected set. Hyphens are kept so `Get-Content` reads as one word.
const READ_VERBS: [&str; 13] = [
    "get-content",
    "gc",
    "cat",
    "type",
    "sed",
    "head",
    "tail",
    "rg",
    "grep",
    "less",
    "more",
    "nl",
    "bat",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageInfo {
    pub name: &'static str,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrimOutcome {
    pub body: Value,
    pub applied: bool,
    pub reason: &'static str,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub stages: Vec<StageInfo>,
    pub classes: Vec<&'static str>,
}

impl TrimOutcome {
    fn unchanged(body: Value, reason: &'static str) -> Self {
        Self {
            body,
            applied: false,
            reason,
            tokens_before: 0,
            tokens_after: 0,
            stages: Vec::new(),
            classes: Vec::new(),
        }
    }
}

/// Trim only harmless whitespace in recognized tool-result text.
///
/// The function is fail-open by construction. A request with backend-owned
/// state (`previous_response_id`), an unknown input item, or an unknown output
/// block is returned byte-for-byte equivalent as JSON without modification.
pub fn trim_responses(body: Value) -> TrimOutcome {
    let before = match serde_json::to_vec(&body) {
        Ok(bytes) if bytes.len() >= MIN_BYTES => bytes,
        _ => return TrimOutcome::unchanged(body, "below_min_size"),
    };

    let Some(object) = body.as_object() else {
        return TrimOutcome::unchanged(body, "unknown_root_shape");
    };
    if object.contains_key("previous_response_id") {
        return TrimOutcome::unchanged(body, "backend_owned_state");
    }
    let Some(input) = object.get("input").and_then(Value::as_array) else {
        return TrimOutcome::unchanged(body, "missing_input");
    };

    let mut candidate = body.clone();
    let candidate_input = candidate
        .as_object_mut()
        .and_then(|root| root.get_mut("input"))
        .and_then(Value::as_array_mut)
        .expect("input shape was checked above");

    // A1: immutable provenance pass — `call_id → lowercased command text`. The
    // `custom_tool_call.input` is JavaScript source, not JSON; it is used
    // verbatim as the haystack (the shell command is a string literal inside
    // it). `function_call.arguments` is JSON text, also used verbatim.
    let provenance: HashMap<&str, String> = input
        .iter()
        .filter_map(|item| {
            let call_id = item.get("call_id").and_then(Value::as_str)?;
            let command = match item.get("type").and_then(Value::as_str)? {
                "custom_tool_call" => item.get("input").and_then(Value::as_str),
                "function_call" => item.get("arguments").and_then(Value::as_str),
                _ => return None,
            }?;
            Some((call_id, command.to_ascii_lowercase()))
        })
        .collect();

    let mut changed = false;
    let mut repeated_line_trim_allowed = false;
    let mut head_tail_trim_applied = false;
    let mut classes = Vec::new();
    for (index, original_item) in input.iter().enumerate() {
        let Some(item_type) = original_item.get("type").and_then(Value::as_str) else {
            return TrimOutcome::unchanged(body, "unknown_item_shape");
        };
        match item_type {
            "function_call_output" | "custom_tool_call_output" => {
                let item_reason = item_protection(
                    original_item.get("call_id").and_then(Value::as_str),
                    &provenance,
                );
                let Some(output) = candidate_input[index].get_mut("output") else {
                    return TrimOutcome::unchanged(body, "custom_output_missing");
                };
                if let Some(text) = output.as_str() {
                    let class = classify_block(text, None, "");
                    classes.push(class.label());
                    let normalized = transform_tool_text(
                        text,
                        class,
                        item_reason,
                        &mut repeated_line_trim_allowed,
                        &mut head_tail_trim_applied,
                    );
                    changed |= normalized != text;
                    *output = Value::String(normalized);
                } else {
                    let Some(blocks) = output.as_array_mut() else {
                        return TrimOutcome::unchanged(body, "custom_output_unsupported_type");
                    };
                    for block in blocks {
                        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                            return TrimOutcome::unchanged(body, "unknown_custom_output_block");
                        };
                        if block_type != "input_text" {
                            return TrimOutcome::unchanged(body, "invalid_custom_output_text");
                        }
                        let Some(text) =
                            block.get("text").and_then(Value::as_str).map(str::to_owned)
                        else {
                            return TrimOutcome::unchanged(body, "invalid_custom_output_text");
                        };
                        let class = classify_block(&text, None, "");
                        classes.push(class.label());
                        let normalized = transform_tool_text(
                            &text,
                            class,
                            item_reason,
                            &mut repeated_line_trim_allowed,
                            &mut head_tail_trim_applied,
                        );
                        changed |= normalized != text;
                        block["text"] = Value::String(normalized);
                    }
                }
            }
            "additional_tools" => {
                // Codex has no top-level `tools` key — descriptions live inside
                // an `input` item of this type. Truncate only the `tools` array;
                // fail-open (no-op) when `tools` is missing or not an array.
                if let Some(tools) = candidate_input[index]
                    .get_mut("tools")
                    .and_then(Value::as_array_mut)
                {
                    let before = serde_json::to_string(tools).unwrap_or_default();
                    for tool in tools.iter_mut() {
                        truncate_descriptions(tool, MAX_DESC_CHARS);
                    }
                    let after = serde_json::to_string(tools).unwrap_or_default();
                    changed |= before.chars().count() > after.chars().count();
                }
            }
            // These item types are deliberately protected.
            "message" | "reasoning" | "compaction" | "compaction_trigger" | "custom_tool_call"
            | "function_call" => {}
            _ => return TrimOutcome::unchanged(body, "unknown_item_type"),
        }
    }

    if !changed {
        return TrimOutcome::unchanged(body, "no_change");
    }
    let after = match serde_json::to_vec(&candidate) {
        Ok(bytes) if bytes.len() < before.len() => bytes,
        _ => return TrimOutcome::unchanged(body, "no_gain"),
    };
    TrimOutcome {
        body: candidate,
        applied: true,
        reason: "applied",
        tokens_before: (before.len() / 4) as u64,
        tokens_after: (after.len() / 4) as u64,
        stages: vec![StageInfo {
            name: if repeated_line_trim_allowed {
                if head_tail_trim_applied {
                    "tool_output_whitespace_and_safe_diagnostics"
                } else {
                    "tool_output_whitespace_and_safe_repeats"
                }
            } else if head_tail_trim_applied {
                "tool_output_whitespace_and_safe_diagnostics"
            } else {
                "tool_output_whitespace"
            },
            bytes_before: before.len(),
            bytes_after: after.len(),
        }],
        classes,
    }
}

/// Decode and trim one text WebSocket `response.create` message. Any other
/// event, invalid JSON, or unsupported shape returns `None` so the caller can
/// forward the original frame unchanged.
pub fn trim_responses_text(text: &str) -> Option<TrimOutcome> {
    let body = serde_json::from_str::<Value>(text).ok()?;
    if body.get("type").and_then(Value::as_str) != Some("response.create") {
        return None;
    }
    Some(trim_responses(body))
}

/// Prepare one client-to-upstream WebSocket text frame.
///
/// The disabled and non-`response.create` paths return the exact original
/// frame. This keeps the transport boundary fail-open and makes the bridge
/// policy independently testable without a live upstream connection.
pub fn trim_responses_text_if_enabled(text: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_owned();
    }
    let Some(outcome) = trim_responses_text(text) else {
        return text.to_owned();
    };
    if !outcome.applied {
        return text.to_owned();
    }
    serde_json::to_string(&outcome.body).unwrap_or_else(|_| text.to_owned())
}

fn normalize_tool_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_lines = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_lines += 1;
            if blank_lines > 2 {
                continue;
            }
        } else {
            blank_lines = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// Transform one tool-output text block.
///
/// `item_reason` is the item-level provenance protection (`Some("orphan" |
/// "provenance_read" | "apply_patch")`) when provenance protects this output;
/// per-block content protection (fenced code, diagrams — CC parity layers 2-3)
/// is checked here. A protected block is returned byte-identical.
///
/// Otherwise the block is eligible regardless of content class: whitespace
/// normalization always applies, repeated-line compression stays gated on the
/// four diagnostic classes (it is a separate, safe op), and head/tail
/// compression now applies unconditionally to any eligible block.
fn transform_tool_text(
    text: &str,
    class: ContentClass,
    item_reason: Option<&'static str>,
    repeated_line_trim_allowed: &mut bool,
    head_tail_trim_applied: &mut bool,
) -> String {
    if item_reason.is_some() || tool_result_protected(text, None, true, 0.01) {
        return text.to_owned();
    }
    // A4 idempotence: an already-trimmed block carries one of our markers and
    // must pass through byte-identical. Without this guard, re-normalizing an
    // elided block could collapse a blank-line run at the head/marker seam
    // (the head cut can end in blanks, and the marker adds one more newline),
    // so the second pass would not be a no-op. Same marker philosophy as
    // `compress_repeated_lines`.
    if text.contains("[mhd-trim: omitted ") || text.contains("...[elided ") {
        return text.to_owned();
    }
    *repeated_line_trim_allowed |= matches!(
        class,
        ContentClass::Logs
            | ContentClass::BuildDiagnostics
            | ContentClass::StackTrace
            | ContentClass::TestOutput
    );
    let normalized = normalize_tool_text(text);
    let diagnostic_class = matches!(
        class,
        ContentClass::Logs
            | ContentClass::BuildDiagnostics
            | ContentClass::StackTrace
            | ContentClass::TestOutput
    );
    let normalized = if diagnostic_class {
        compress_repeated_lines(&normalized)
    } else {
        normalized
    };
    if let Some(compressed) = compress_head_tail(
        &normalized,
        SAFE_HEAD_CHARS,
        SAFE_TAIL_CHARS,
        SAFE_MIN_ELIDE,
    ) {
        *head_tail_trim_applied = true;
        return compressed;
    }
    normalized
}

/// Item-level provenance protection (A2.1-A2.3). Returns the reason when the
/// output's `call_id` is an orphan or its command reads a protected file or
/// applies a patch; `None` when provenance does not protect.
fn item_protection<'a>(
    call_id: Option<&'a str>,
    provenance: &HashMap<&'a str, String>,
) -> Option<&'static str> {
    let Some(cid) = call_id else {
        return Some(ORPHAN);
    };
    let Some(command) = provenance.get(cid) else {
        return Some(ORPHAN);
    };
    command_protection(command)
}

/// A2.2 + A2.3 on a lowercased command haystack: a protected file read
/// (read verb AND a protected-extension token) or an `apply_patch`.
fn command_protection(command_lower: &str) -> Option<&'static str> {
    if read_verb_seen(command_lower) && protected_ext_word_seen(command_lower) {
        return Some(PROVENANCE_READ);
    }
    if command_lower.contains("apply_patch") {
        return Some(APPLY_PATCH);
    }
    None
}

/// Split a command haystack into matching words: alphanumeric runs plus
/// hyphens, so `Get-Content` / `apply_patch`-style tokens survive and words
/// glued to JS punctuation (`{command:"cat`) still match.
fn command_words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
}

fn read_verb_seen(command_lower: &str) -> bool {
    command_words(command_lower).any(|w| READ_VERBS.iter().any(|v| w.eq_ignore_ascii_case(v)))
}

fn protected_ext_word_seen(command_lower: &str) -> bool {
    command_words(command_lower).any(is_protected_ext)
}

/// Replace only consecutive identical non-empty diagnostic lines with an
/// explicit, machine-readable marker. This is intentionally not used for
/// source, structured, tabular, or ambiguous content: removing repeated lines
/// there could alter a program, a record set, or a value sequence.
fn compress_repeated_lines(text: &str) -> String {
    const MARKER: &str = "[mhd-trim: omitted ";
    // A replayed/already-trimmed body must remain stable; never nest markers.
    if text.contains(MARKER) {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        let mut end = i + 1;
        while end < lines.len() && lines[end] == lines[i] {
            end += 1;
        }
        let count = end - i;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(lines[i]);
        if count >= 3 && !lines[i].trim().is_empty() {
            out.push_str(&format!(
                "\n[mhd-trim: omitted {} repeated diagnostic lines]",
                count - 1
            ));
        }
        i = end;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityReport {
    pub relationships_preserved: bool,
    pub structured_content_preserved: bool,
    /// The ordered `additional_tools[].tools[].name` list (and thus the tool
    /// count) is identical before/after. Description truncation must never drop
    /// or reorder a tool identity.
    pub tool_names_preserved: bool,
}

/// Check invariants that must hold for a transformed Responses body.
pub fn quality_check(before: &Value, after: &Value) -> QualityReport {
    QualityReport {
        relationships_preserved: relationship_keys(before) == relationship_keys(after),
        structured_content_preserved: protected_content_unchanged(before, after),
        tool_names_preserved: tool_names(before) == tool_names(after),
    }
}

/// The ordered list of every `additional_tools[].tools[].name` across all
/// `additional_tools` items in the body. Comparing two bodies by this vector
/// proves the tool identity list (order and count) survived trim unchanged.
fn tool_names(body: &Value) -> Vec<String> {
    let mut names = Vec::new();
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return names;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
            continue;
        }
        let Some(tools) = item.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                names.push(name.to_owned());
            }
        }
    }
    names
}

fn relationship_keys(body: &Value) -> Vec<(usize, String, Option<String>)> {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    (
                        index,
                        item.get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        item.get("call_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn protected_content_unchanged(before: &Value, after: &Value) -> bool {
    protected_texts(before)
        .into_iter()
        .all(|(item, block, text)| {
            output_text(after, item, block).is_some_and(|candidate| candidate == text)
        })
}

fn protected_texts(body: &Value) -> Vec<(usize, usize, String)> {
    let mut protected = Vec::new();
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return protected;
    };
    for (item_index, item) in items.iter().enumerate() {
        if !matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call_output") | Some("custom_tool_call_output")
        ) {
            continue;
        }
        let Some(output) = item.get("output") else {
            continue;
        };
        if let Some(text) = output.as_str() {
            if is_content_protected(text) {
                protected.push((item_index, 0, text.to_owned()));
            }
        } else if let Some(blocks) = output.as_array() {
            for (block_index, block) in blocks.iter().enumerate() {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if is_content_protected(text) {
                    protected.push((item_index, block_index, text.to_owned()));
                }
            }
        }
    }
    protected
}

fn output_text(body: &Value, item_index: usize, block_index: usize) -> Option<String> {
    let output = body.get("input")?.get(item_index)?.get("output")?;
    if block_index == 0 {
        if let Some(text) = output.as_str() {
            return Some(text.to_owned());
        }
    }
    output
        .as_array()?
        .get(block_index)?
        .get("text")?
        .as_str()
        .map(str::to_owned)
}

/// Content-based protection (A2.4, CC parity layers 2-3): fenced code and
/// diagrams stay byte-identical regardless of provenance. The quality gate
/// verifies exactly this set survives trim — provenance-protected blocks
/// (orphan / file read / apply_patch) are enforced by the trim path itself.
fn is_content_protected(text: &str) -> bool {
    tool_result_protected(text, None, true, 0.01)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large_text() -> String {
        (0..1200)
            .map(|i| format!("tool output line {i}    \n\n\n\n"))
            .collect()
    }

    #[test]
    fn trims_only_tool_output_and_preserves_relationship_fields() {
        let body = serde_json::json!({
            "model": "gpt-5.6-luna",
            "input": [{
                "type": "function_call",
                "call_id": "call_123",
                "arguments": "{\"cell_id\":\"220\",\"yield_time_ms\":1000}"
            }, {
                "type": "function_call_output",
                "call_id": "call_123",
                "output": large_text()
            }],
            "tools": [{"type": "function", "name": "keep_me"}],
            "instructions": "keep instructions"
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(out.body["input"][1]["call_id"], "call_123");
        assert_eq!(out.body["tools"][0]["name"], "keep_me");
        assert_eq!(out.body["instructions"], "keep instructions");
        assert!(out.tokens_after < out.tokens_before);
    }

    #[test]
    fn backend_owned_state_fails_open() {
        let body = serde_json::json!({
            "previous_response_id": "resp_123",
            "input": [{"type": "function_call_output", "call_id": "c", "output": large_text()}]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.body, body);
    }

    #[test]
    fn trims_string_custom_output_without_touching_call_id() {
        let body = serde_json::json!({
            "input": [{
                "type": "custom_tool_call",
                "call_id": "custom_123",
                "input": "const r = await tools.shell_command({command:\"echo hi\"}); text(r)"
            }, {
                "type": "custom_tool_call_output",
                "call_id": "custom_123",
                "output": (0..1200).map(|i| format!("line {i}    \n\n\n\n")).collect::<String>()
            }]
        });
        let out = trim_responses(body);
        assert!(out.applied);
        assert_eq!(out.body["input"][1]["call_id"], "custom_123");
    }

    #[test]
    fn unknown_shapes_fail_open() {
        let body = serde_json::json!({
            "input": [{"type": "future_item", "payload": large_text()}]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.body, body);
    }

    #[test]
    fn log_repeats_use_explicit_marker_and_preserve_relationships() {
        let line = "2026-08-03 12:00:00 INFO  worker heartbeat";
        let output = (0..900)
            .map(|i| {
                if i % 9 == 0 {
                    format!("{line} {i}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "log_1",
                 "input": "const r = await tools.shell_command({command:\"npm test\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "log_1", "output": output}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("[mhd-trim: omitted ")
        );
        assert!(quality_check(&body, &out.body).relationships_preserved);
    }

    #[test]
    fn source_and_json_are_not_repeated_line_compressed() {
        let source = (0..900)
            .map(|_| "fn keep_me() { return; }")
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [{"type": "function_call_output", "call_id": "src_1", "output": source}]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.reason, "no_change");
        assert!(
            !out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("mhd-trim: omitted")
        );
        assert!(quality_check(&body, &out.body).structured_content_preserved);
    }

    #[test]
    fn diagnostics_use_shared_head_tail_and_keep_call_id() {
        let output = (0..1400)
            .map(|i| format!("error[E{:04}]: diagnostic line {i}", i % 1000))
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "diag_1", "arguments": "{\"cell_id\":\"1\"}"},
                {
                    "type": "function_call_output",
                    "call_id": "diag_1",
                    "output": output
                }
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(out.body["input"][1]["call_id"], "diag_1");
        let trimmed = out.body["input"][1]["output"].as_str().unwrap();
        assert!(trimmed.contains("...[elided "));
        assert!(trimmed.starts_with("error[E0000]: diagnostic line 0"));
        assert!(trimmed.ends_with("error[E0399]: diagnostic line 1399"));
        assert!(
            out.stages
                .iter()
                .any(|stage| { stage.name == "tool_output_whitespace_and_safe_diagnostics" })
        );
        assert!(quality_check(&body, &out.body).relationships_preserved);
    }

    #[test]
    fn diff_output_is_left_byte_stable() {
        let diff = (0..900)
            .map(|i| format!("@@ -{i},1 +{i},1 @@\n-old {i}\n+new {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [{"type": "function_call_output", "call_id": "diff_1", "output": diff}]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.body, body);
        assert!(quality_check(&body, &out.body).structured_content_preserved);
    }

    #[test]
    fn json_output_is_left_byte_stable() {
        let body = serde_json::json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "json_1",
                "output": serde_json::to_string(&(0..5000).collect::<Vec<_>>()).unwrap()
            }]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.reason, "no_change");
        assert!(quality_check(&body, &out.body).structured_content_preserved);
    }

    #[test]
    fn websocket_trim_is_opt_in_and_preserves_non_create_frames() {
        let body = serde_json::json!({
            "type": "response.create",
            "input": [
                {"type": "function_call", "call_id": "ws_1", "arguments": "{\"cell_id\":\"1\"}"},
                {
                    "type": "function_call_output",
                    "call_id": "ws_1",
                    "output": large_text()
                }
            ]
        });
        let original = serde_json::to_string(&body).unwrap();
        assert_eq!(trim_responses_text_if_enabled(&original, false), original);

        let trimmed = trim_responses_text_if_enabled(&original, true);
        assert_ne!(trimmed, original);
        let parsed: Value = serde_json::from_str(&trimmed).unwrap();
        assert_eq!(parsed["type"], "response.create");
        assert_eq!(parsed["input"][1]["call_id"], "ws_1");

        let other = r#"{"type":"response.cancel","response_id":"resp_1"}"#;
        assert_eq!(trim_responses_text_if_enabled(other, true), other);
        assert_eq!(trim_responses_text_if_enabled("not-json", true), "not-json");
    }

    #[test]
    fn log_marker_is_idempotent() {
        let text = "2026-08-03 12:00:00 INFO worker\n[mhd-trim: omitted 8 repeated log lines]";
        assert_eq!(compress_repeated_lines(text), text);
    }

    #[test]
    fn websocket_response_create_trim_preserves_event_type() {
        let body = serde_json::json!({
            "type": "response.create",
            "input": [
                {"type": "function_call", "call_id": "ws_call", "arguments": "{\"cell_id\":\"1\"}"},
                {
                    "type": "function_call_output",
                    "call_id": "ws_call",
                    "output": large_text()
                }
            ]
        });
        let text = serde_json::to_string(&body).unwrap();
        let outcome = trim_responses_text(&text).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.body["type"], "response.create");
        assert_eq!(outcome.body["input"][1]["call_id"], "ws_call");
    }

    #[test]
    fn websocket_non_create_event_is_fail_open() {
        assert!(trim_responses_text(r#"{"type":"response.cancel"}"#).is_none());
    }

    #[test]
    fn additional_tools_descriptions_truncated_and_names_survive() {
        let long = "x".repeat(5_000);
        let body = serde_json::json!({
            "input": [{
                "type": "additional_tools",
                "role": "developer",
                "tools": [
                    {"name": "exec", "description": long.clone()},
                    {"name": "wait", "description": long}
                ]
            }]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        let tools = &out.body["input"][0]["tools"];
        assert_eq!(tools[0]["name"], "exec");
        assert_eq!(tools[1]["name"], "wait");
        let d0 = tools[0]["description"].as_str().unwrap();
        assert_eq!(d0.chars().count(), MAX_DESC_CHARS + 1, "150 chars + marker");
        assert!(d0.ends_with('…'));
        let d1 = tools[1]["description"].as_str().unwrap();
        assert_eq!(d1.chars().count(), MAX_DESC_CHARS + 1);
        assert!(d1.ends_with('…'));
        assert!(quality_check(&body, &out.body).tool_names_preserved);
    }

    #[test]
    fn additional_tools_missing_tools_array_fails_open() {
        // Over MIN_BYTES so the pass runs, but `tools` is not an array → no-op.
        let body = serde_json::json!({
            "input": [{
                "type": "additional_tools",
                "role": "developer",
                "tools": "x".repeat(9_000)
            }]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.reason, "no_change");
        assert_eq!(out.body, body);
    }

    #[test]
    fn orphan_output_is_byte_stable() {
        // A call_id with no matching call, and an output with no call_id at all,
        // are orphans — without provenance we cannot know the bytes are not an
        // exact file read a later apply_patch depends on, so both stay intact.
        let body = serde_json::json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "call_ghost",
                "output": large_text()
            }]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.reason, "no_change");
        assert_eq!(out.body, body);

        let no_call_id = serde_json::json!({
            "input": [{"type": "custom_tool_call_output", "output": large_text()}]
        });
        let out2 = trim_responses(no_call_id.clone());
        assert!(!out2.applied);
        assert_eq!(out2.body, no_call_id);
    }

    #[test]
    fn cat_rs_provenance_protects_large_output() {
        // `cat foo.rs` is a file read of a protected extension: even a large
        // output that would otherwise be elided stays byte-identical.
        let output = large_text();
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_read",
                 "input": "const r = await tools.shell_command({command:\"cat foo.rs\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "call_read", "output": output.clone()}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.body, body);
        assert_eq!(out.body["input"][1]["output"], output);
    }

    #[test]
    fn cargo_build_provenance_does_not_protect() {
        // `cargo build 2>&1` has neither a read verb nor a protected path — the
        // output is eligible and head/tail elision applies.
        let output = (0..1400)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "call_build",
                 "input": "const r = await tools.shell_command({command:\"cargo build 2>&1\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "call_build", "output": output}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(out.body["input"][1]["output"].as_str().unwrap().contains("...[elided "));
    }

    #[test]
    fn read_notes_md_protects_but_runner_does_not() {
        // A read verb + a protected extension protects...
        let output = large_text();
        let read_body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "r",
                 "input": "const r = await tools.shell_command({command:\"Get-Content notes.md -Raw\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "r", "output": output.clone()}
            ]
        });
        let read_out = trim_responses(read_body.clone());
        assert!(!read_out.applied);
        assert_eq!(read_out.body, read_body);

        // ...but a bare `.sh` argument to a runner is not a read: verb and
        // extension must BOTH be present, so this output is elided.
        let run_body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "s",
                 "input": "const r = await tools.shell_command({command:\"run_tests.sh --verbose\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "s", "output": output}
            ]
        });
        let run_out = trim_responses(run_body.clone());
        assert!(run_out.applied);
        assert!(run_out.body["input"][1]["output"].as_str().unwrap().contains("...[elided "));
    }

    #[test]
    fn rerun_on_trimmed_body_is_noop() {
        // Determinism + idempotence: the second pass must be a byte-stable no-op
        // (the `[mhd-trim: omitted` / `...[elided ` markers guard against nesting).
        // The orphan-protected output stays large so the trimmed body remains
        // above MIN_BYTES and the second pass genuinely re-inspects it rather
        // than bailing out at below_min_size.
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "c",
                 "input": "const r = await tools.shell_command({command:\"echo hello\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "c", "output": large_text()},
                {"type": "function_call_output", "call_id": "orphan", "output": large_text()}
            ]
        });
        let first = trim_responses(body.clone());
        assert!(first.applied);
        let second = trim_responses(first.body.clone());
        assert!(!second.applied);
        assert_eq!(second.reason, "no_change");
        assert_eq!(second.body, first.body);
    }
}
