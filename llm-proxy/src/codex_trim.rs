//! Conservative, pure trimming for OpenAI Responses request bodies.
//!
//! Only text inside tool outputs may be normalized;
//! calls, ids, reasoning, messages, tools, and backend-owned state remain
//! untouched. Any shape outside that contract returns the original payload.

use serde_json::Value;

use crate::content_mix::{ContentClass, classify_block};
use crate::native_trim::{compress_head_tail, truncate_descriptions};

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
                let Some(output) = candidate_input[index].get_mut("output") else {
                    return TrimOutcome::unchanged(body, "custom_output_missing");
                };
                if let Some(text) = output.as_str() {
                    let class = classify_block(text, None, "");
                    classes.push(class.label());
                    let normalized = transform_tool_text(
                        text,
                        class,
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

fn transform_tool_text(
    text: &str,
    class: ContentClass,
    repeated_line_trim_allowed: &mut bool,
    head_tail_trim_applied: &mut bool,
) -> String {
    // These classes are data-bearing. Even whitespace-only changes can be
    // meaningful in source, serialized records, or aligned columns.
    if matches!(
        class,
        ContentClass::SourceCode
            | ContentClass::StructuredData
            | ContentClass::Diff
            | ContentClass::Tabular
    ) {
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
    if diagnostic_class
        && let Some(compressed) = compress_head_tail(
            &normalized,
            SAFE_HEAD_CHARS,
            SAFE_TAIL_CHARS,
            SAFE_MIN_ELIDE,
        )
    {
        *head_tail_trim_applied = true;
        return compressed;
    }
    normalized
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
            if is_protected_class(text) {
                protected.push((item_index, 0, text.to_owned()));
            }
        } else if let Some(blocks) = output.as_array() {
            for (block_index, block) in blocks.iter().enumerate() {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if is_protected_class(text) {
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

fn is_protected_class(text: &str) -> bool {
    matches!(
        classify_block(text, None, ""),
        ContentClass::SourceCode | ContentClass::StructuredData | ContentClass::Tabular
    )
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
                "type": "function_call_output",
                "call_id": "call_123",
                "output": large_text()
            }],
            "tools": [{"type": "function", "name": "keep_me"}],
            "instructions": "keep instructions"
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(out.body["input"][0]["call_id"], "call_123");
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
                "type": "custom_tool_call_output",
                "call_id": "custom_123",
                "output": (0..1200).map(|i| format!("line {i}    \n\n\n\n")).collect::<String>()
            }]
        });
        let out = trim_responses(body);
        assert!(out.applied);
        assert_eq!(out.body["input"][0]["call_id"], "custom_123");
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
            "input": [{"type": "custom_tool_call_output", "call_id": "log_1", "output": output}]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(
            out.body["input"][0]["output"]
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
            "input": [{
                "type": "function_call_output",
                "call_id": "diag_1",
                "output": output
            }]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(out.body["input"][0]["call_id"], "diag_1");
        let trimmed = out.body["input"][0]["output"].as_str().unwrap();
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
            "input": [{
                "type": "function_call_output",
                "call_id": "ws_1",
                "output": large_text()
            }]
        });
        let original = serde_json::to_string(&body).unwrap();
        assert_eq!(trim_responses_text_if_enabled(&original, false), original);

        let trimmed = trim_responses_text_if_enabled(&original, true);
        assert_ne!(trimmed, original);
        let parsed: Value = serde_json::from_str(&trimmed).unwrap();
        assert_eq!(parsed["type"], "response.create");
        assert_eq!(parsed["input"][0]["call_id"], "ws_1");

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
            "input": [{
                "type": "function_call_output",
                "call_id": "ws_call",
                "output": large_text()
            }]
        });
        let text = serde_json::to_string(&body).unwrap();
        let outcome = trim_responses_text(&text).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.body["type"], "response.create");
        assert_eq!(outcome.body["input"][0]["call_id"], "ws_call");
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
}
