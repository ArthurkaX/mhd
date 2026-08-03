//! Conservative, pure trimming for OpenAI Responses request bodies.
//!
//! This module is deliberately not wired into the proxy path yet. It defines
//! the first safe boundary: only text inside tool outputs may be normalized;
//! calls, ids, reasoning, messages, tools, and backend-owned state remain
//! untouched. Any shape outside that contract returns the original payload.

use serde_json::Value;

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
                    let normalized = normalize_tool_text(text);
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
                        let normalized = normalize_tool_text(&text);
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
            name: "tool_output_whitespace",
            bytes_before: before.len(),
            bytes_after: after.len(),
        }],
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
}
