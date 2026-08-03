//! Conservative, pure trimming for OpenAI Responses request bodies.
//!
//! Only text inside tool outputs may be normalized;
//! calls, ids, reasoning, messages, tools, and backend-owned state remain
//! untouched. Any shape outside that contract returns the original payload.

use serde_json::Value;

use crate::content_mix::{ContentClass, classify_block};

const MIN_BYTES: usize = 16 * 1024;

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
    let mut strong_trim_allowed = false;
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
                    let normalized = transform_tool_text(text, class, &mut strong_trim_allowed);
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
                        let normalized =
                            transform_tool_text(&text, class, &mut strong_trim_allowed);
                        changed |= normalized != text;
                        block["text"] = Value::String(normalized);
                    }
                }
            }
            // These item types are deliberately protected in the first stage.
            "message" | "reasoning" | "compaction" | "compaction_trigger" | "custom_tool_call"
            | "function_call" | "additional_tools" => {}
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
            name: if strong_trim_allowed {
                "tool_output_whitespace_and_log_repeats"
            } else {
                "tool_output_whitespace"
            },
            bytes_before: before.len(),
            bytes_after: after.len(),
        }],
        classes,
    }
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

fn transform_tool_text(text: &str, class: ContentClass, strong_trim_allowed: &mut bool) -> String {
    // These classes are data-bearing. Even whitespace-only changes can be
    // meaningful in source, serialized records, or aligned columns.
    if matches!(
        class,
        ContentClass::SourceCode | ContentClass::StructuredData | ContentClass::Tabular
    ) {
        return text.to_owned();
    }
    *strong_trim_allowed |= class == ContentClass::Logs;
    let normalized = normalize_tool_text(text);
    if class == ContentClass::Logs {
        compress_repeated_lines(&normalized)
    } else {
        normalized
    }
}

/// Replace only consecutive identical non-empty log lines with an explicit,
/// machine-readable marker. This is intentionally not used for source,
/// structured, tabular, or ambiguous content: removing repeated lines there
/// could alter a program, a record set, or a value sequence.
fn compress_repeated_lines(text: &str) -> String {
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
                "\n[mhd-trim: omitted {} repeated log lines]",
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
}

/// Check invariants that must hold for a transformed Responses body.
pub fn quality_check(before: &Value, after: &Value) -> QualityReport {
    QualityReport {
        relationships_preserved: relationship_keys(before) == relationship_keys(after),
        structured_content_preserved: protected_content_safe(after),
    }
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

fn protected_content_safe(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().all(|item| {
                let Some(kind) = item.get("type").and_then(Value::as_str) else {
                    return true;
                };
                if !matches!(kind, "function_call_output" | "custom_tool_call_output") {
                    return true;
                }
                let Some(output) = item.get("output") else {
                    return true;
                };
                let texts: Vec<&str> = if let Some(text) = output.as_str() {
                    vec![text]
                } else if let Some(blocks) = output.as_array() {
                    blocks
                        .iter()
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect()
                } else {
                    Vec::new()
                };
                texts.into_iter().all(|text| {
                    !matches!(
                        classify_block(text, None, ""),
                        ContentClass::SourceCode
                            | ContentClass::StructuredData
                            | ContentClass::Tabular
                    ) || !text.contains("[mhd-trim: omitted ")
                })
            })
        })
        .unwrap_or(true)
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
}
