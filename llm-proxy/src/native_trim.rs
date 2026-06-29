//! Clean-room native trim engine (side artifact — NOT wired to the live path).
//!
//! v2 levers:
//! - Truncate every `"description"` string value (anywhere inside each
//!   `tools[]` element, including `input_schema` sub-properties and arrays)
//!   to `tool_max_desc_chars`.
//! - Compress large `tool_result` content blocks (string form or array form)
//!   to head + elision marker + tail.
//!
//! Pure, deterministic, fail-open: any unexpected shape leaves the body
//! unchanged. Never enlarges the body.

use serde_json::Value;

// ── knobs ─────────────────────────────────────────────────────────────────────

/// Tuning for the native trim engine.
#[derive(Debug, Clone, Copy)]
pub struct NativeKnobs {
    /// Truncate any tool `description` longer than this many chars.
    pub tool_max_desc_chars: usize,
    /// Tool_result blocks whose text exceeds `tool_result_head + tool_result_tail`
    /// by more than this margin are compressed to head + marker + tail.
    pub tool_result_head: usize,
    pub tool_result_tail: usize,
}

impl Default for NativeKnobs {
    fn default() -> Self {
        Self { tool_max_desc_chars: 150, tool_result_head: 3000, tool_result_tail: 1000 }
    }
}

// ── description truncator ─────────────────────────────────────────────────────

/// Recursively truncate every `"description"` string value (anywhere under
/// `node`) longer than `max_chars` Unicode chars to `max_chars` chars (cut on a
/// char boundary) + a single `…` marker. Deterministic; leaves all other
/// content untouched.
fn truncate_descriptions(node: &mut Value, max_chars: usize) {
    match node {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "description" {
                    if let Value::String(s) = v {
                        if s.chars().count() > max_chars {
                            let mut t: String = s.chars().take(max_chars).collect();
                            t.push('…');
                            *s = t;
                        }
                    } else {
                        // a non-string "description" — recurse in case it nests strings
                        truncate_descriptions(v, max_chars);
                    }
                } else {
                    truncate_descriptions(v, max_chars);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                truncate_descriptions(v, max_chars);
            }
        }
        _ => {}
    }
}

// ── tool_result compressor ────────────────────────────────────────────────────

/// Walk a message's content array; for each `tool_result` block, compress its
/// text payload (string form, or the text of each text block in the array form)
/// when it is large. Head+tail preservation with an elision marker. Deterministic,
/// fail-open, never enlarges. Only touches tool_result text — never tool_use,
/// user text, or images.
fn compress_tool_results(msg: &mut Value, knobs: &NativeKnobs) {
    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else { return };
    for block in content.iter_mut() {
        let Some(b) = block.as_object_mut() else { continue };
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") { continue; }
        match b.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(new) = compress_text(s, knobs) { *s = new; }
            }
            Some(Value::Array(arr)) => {
                for inner in arr.iter_mut() {
                    let Some(ib) = inner.as_object_mut() else { continue };
                    if ib.get("type").and_then(|t| t.as_str()) != Some("text") { continue; }
                    if let Some(Value::String(s)) = ib.get_mut("text") {
                        if let Some(new) = compress_text(s, knobs) { *s = new; }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Return Some(compressed) when `s` is long enough to compress, else None.
/// Keeps the first `head` and last `tail` chars (char boundaries) with a marker
/// `\n…[elided N chars]…\n` between. Only compresses when it actually shrinks the
/// text (i.e. total chars > head + tail + marker overhead), so it never enlarges.
fn compress_text(s: &str, knobs: &NativeKnobs) -> Option<String> {
    let total = s.chars().count();
    let head = knobs.tool_result_head;
    let tail = knobs.tool_result_tail;
    // Only worth it if there is a meaningful middle to drop.
    if total <= head + tail { return None; }
    let elided = total - head - tail;
    let head_str: String = s.chars().take(head).collect();
    let tail_str: String = s.chars().skip(total - tail).collect();
    let candidate = format!("{head_str}\n…[elided {elided} chars]…\n{tail_str}");
    // Guard: never enlarge.
    if candidate.chars().count() >= total { return None; }
    Some(candidate)
}

// ── public entry point ────────────────────────────────────────────────────────

/// Apply the native trim engine to `body` using the provided `knobs`.
///
/// 1. Truncates tool descriptions (top-level + nested) inside `tools[]`.
/// 2. Compresses large `tool_result` content blocks inside `messages[]`.
///
/// Scoped strictly — never touches `system` or other top-level keys.
/// Deterministic and fail-open: unexpected shapes are left unchanged.
/// Never enlarges the body.
pub fn trim_native(mut body: Value, knobs: &NativeKnobs) -> Value {
    let Some(obj) = body.as_object_mut() else { return body };
    // 1) tool descriptions (top-level + nested), scoped to the tools array.
    if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools.iter_mut() {
            truncate_descriptions(tool, knobs.tool_max_desc_chars);
        }
    }
    // 2) large tool_result blocks inside messages.
    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            compress_tool_results(msg, knobs);
        }
    }
    body
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Build knobs that only vary `tool_max_desc_chars`; tool_result heads/tails
    /// are pushed large enough to be irrelevant.
    fn desc_only(max_chars: usize) -> NativeKnobs {
        NativeKnobs { tool_max_desc_chars: max_chars, tool_result_head: 1_000_000, tool_result_tail: 1_000_000 }
    }

    /// Build knobs that only vary the tool_result budget; desc truncation is
    /// effectively disabled.
    fn result_only(head: usize, tail: usize) -> NativeKnobs {
        NativeKnobs { tool_max_desc_chars: usize::MAX, tool_result_head: head, tool_result_tail: tail }
    }

    // ── existing description-truncation tests (updated signature) ─────────────

    /// (a) A description longer than max_chars is truncated to max_chars+1 chars
    /// (the max_chars content chars + the '…' marker) and ends with '…'.
    #[test]
    fn long_desc_is_truncated() {
        let long_desc = "A".repeat(200);
        let body = json!({
            "tools": [
                { "name": "my_tool", "description": long_desc }
            ]
        });
        let result = trim_native(body, &desc_only(100));
        let desc = result["tools"][0]["description"].as_str().unwrap();
        // Should be 100 content chars + 1 ellipsis = 101 Unicode chars total.
        assert_eq!(desc.chars().count(), 101);
        assert!(desc.ends_with('…'), "should end with ellipsis");
        // The content part before ellipsis should be exactly 100 'A's.
        let content: String = desc.chars().take(100).collect();
        assert_eq!(content, "A".repeat(100));
    }

    /// (b) A description at or below max_chars is left unchanged.
    #[test]
    fn short_desc_is_unchanged() {
        let body = json!({
            "tools": [
                { "name": "my_tool", "description": "Short description" }
            ]
        });
        let result = trim_native(body.clone(), &desc_only(100));
        assert_eq!(result["tools"][0]["description"], body["tools"][0]["description"]);
    }

    /// Exactly at the boundary (== max_chars) is also left unchanged.
    #[test]
    fn desc_at_boundary_is_unchanged() {
        let exact = "B".repeat(100);
        let body = json!({
            "tools": [
                { "name": "my_tool", "description": exact.clone() }
            ]
        });
        let result = trim_native(body, &desc_only(100));
        assert_eq!(result["tools"][0]["description"].as_str().unwrap(), exact);
    }

    /// (c) A body with no `tools` key is returned byte-identical.
    #[test]
    fn no_tools_key_unchanged() {
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let result = trim_native(body.clone(), &NativeKnobs::default());
        assert_eq!(result, body);
    }

    /// (d) Determinism: two calls on the same input produce equal output.
    #[test]
    fn deterministic() {
        let long_desc = "X".repeat(300);
        let body = json!({
            "tools": [
                { "name": "tool_a", "description": long_desc.clone() },
                { "name": "tool_b", "description": "short" },
            ]
        });
        let result_a = trim_native(body.clone(), &desc_only(80));
        let result_b = trim_native(body, &desc_only(80));
        assert_eq!(result_a, result_b);
    }

    /// (e) Multi-byte (Cyrillic/emoji) description truncates on a char boundary
    /// without panicking, and the output ends with '…'.
    #[test]
    fn multibyte_truncates_on_char_boundary() {
        // Each Cyrillic char is 2 bytes; emoji are 4 bytes.
        let cyrillic = "Привет мир! ".repeat(20); // well over 100 chars
        let body = json!({
            "tools": [
                { "name": "tool_cyr", "description": cyrillic }
            ]
        });
        let result = trim_native(body, &desc_only(50));
        let desc = result["tools"][0]["description"].as_str().unwrap();
        // Must be valid UTF-8 (no panic), end with '…', and have the right char count.
        assert_eq!(desc.chars().count(), 51);
        assert!(desc.ends_with('…'));
        // Verify it's valid UTF-8 by checking the String length in bytes is sensible.
        assert!(desc.len() > 51, "Cyrillic chars are multi-byte, so byte len > char count");

        // Emoji variant
        let emoji = "🦀🎉🌟".repeat(30); // 90 chars, well over 50
        let body2 = json!({
            "tools": [
                { "name": "tool_emoji", "description": emoji }
            ]
        });
        let result2 = trim_native(body2, &desc_only(20));
        let desc2 = result2["tools"][0]["description"].as_str().unwrap();
        assert_eq!(desc2.chars().count(), 21);
        assert!(desc2.ends_with('…'));
    }

    /// (f) Nested description inside `input_schema.properties.<param>.description`
    /// is truncated, while a short top-level description on the same tool is untouched.
    #[test]
    fn nested_schema_property_desc_truncated() {
        let long_nested = "N".repeat(200);
        let body = json!({
            "tools": [{
                "name": "mcp_tool",
                "description": "short top",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "foo": {
                            "type": "string",
                            "description": long_nested
                        }
                    }
                }
            }]
        });
        let result = trim_native(body, &desc_only(100));
        // Top-level description unchanged (it was already short).
        assert_eq!(result["tools"][0]["description"].as_str().unwrap(), "short top");
        // Nested description truncated.
        let nested_desc = result["tools"][0]["input_schema"]["properties"]["foo"]["description"]
            .as_str()
            .unwrap();
        assert_eq!(nested_desc.chars().count(), 101);
        assert!(nested_desc.ends_with('…'));
        let content: String = nested_desc.chars().take(100).collect();
        assert_eq!(content, "N".repeat(100));
    }

    /// (g) Deeply nested description (e.g. `input_schema.properties.foo.items.description`)
    /// is also truncated.
    #[test]
    fn deeply_nested_desc_truncated() {
        let long_deep = "D".repeat(300);
        let body = json!({
            "tools": [{
                "name": "deep_tool",
                "description": "fine",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "foo": {
                            "type": "array",
                            "items": {
                                "type": "string",
                                "description": long_deep
                            }
                        }
                    }
                }
            }]
        });
        let result = trim_native(body, &desc_only(50));
        let deep_desc = result["tools"][0]["input_schema"]["properties"]["foo"]["items"]
            ["description"]
            .as_str()
            .unwrap();
        assert_eq!(deep_desc.chars().count(), 51);
        assert!(deep_desc.ends_with('…'));
        let content: String = deep_desc.chars().take(50).collect();
        assert_eq!(content, "D".repeat(50));
    }

    /// (h) Determinism still holds when nested descriptions are present.
    #[test]
    fn deterministic_with_nested_descs() {
        let long_top = "T".repeat(200);
        let long_nested = "P".repeat(200);
        let body = json!({
            "tools": [{
                "name": "tool_x",
                "description": long_top,
                "input_schema": {
                    "properties": {
                        "bar": { "description": long_nested }
                    }
                }
            }]
        });
        let result_a = trim_native(body.clone(), &desc_only(60));
        let result_b = trim_native(body, &desc_only(60));
        assert_eq!(result_a, result_b);
    }

    // ── new tool_result compression tests ─────────────────────────────────────

    /// (i) A large string-form tool_result is compressed: contains the elision
    /// marker, starts with the original head, ends with the original tail, and
    /// is shorter than the original.
    #[test]
    fn string_form_tool_result_compressed() {
        let big_text = "A".repeat(5000);
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_abc",
                    "content": big_text.clone()
                }]
            }]
        });
        let knobs = result_only(100, 50);
        let result = trim_native(body, &knobs);
        let compressed = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        // Must be shorter.
        assert!(compressed.chars().count() < big_text.chars().count(),
            "compressed should be shorter than original");
        // Must contain the elision marker.
        assert!(compressed.contains("…[elided"), "must contain elision marker");
        // Must start with the head and end with the tail.
        let head: String = big_text.chars().take(100).collect();
        let tail: String = big_text.chars().skip(big_text.chars().count() - 50).collect();
        assert!(compressed.starts_with(&head), "must start with original head");
        assert!(compressed.ends_with(&tail), "must end with original tail");
    }

    /// (j) A small tool_result (total chars <= head + tail) is left unchanged.
    #[test]
    fn small_tool_result_unchanged() {
        let small_text = "X".repeat(100);
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_small",
                    "content": small_text.clone()
                }]
            }]
        });
        // head=200, tail=200 — 100 chars fits inside.
        let knobs = result_only(200, 200);
        let result = trim_native(body, &knobs);
        let text = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(text, small_text, "small result must be unchanged");
    }

    /// (k) Array-form tool_result: text blocks are compressed, image block in the
    /// same array is untouched.
    #[test]
    fn array_form_tool_result_text_compressed_image_untouched() {
        let big_text = "B".repeat(5000);
        let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_arr",
                    "content": [
                        { "type": "text", "text": big_text.clone() },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": image_data } }
                    ]
                }]
            }]
        });
        let knobs = result_only(100, 50);
        let result = trim_native(body, &knobs);
        let blocks = result["messages"][0]["content"][0]["content"].as_array().unwrap();

        // Text block should be compressed.
        let text_val = blocks[0]["text"].as_str().unwrap();
        assert!(text_val.chars().count() < big_text.chars().count(), "text block must be compressed");
        assert!(text_val.contains("…[elided"), "text block must contain marker");

        // Image block must be completely untouched.
        assert_eq!(blocks[1]["type"].as_str().unwrap(), "image");
        assert_eq!(blocks[1]["source"]["data"].as_str().unwrap(), image_data,
            "image data must be untouched");
    }

    /// (l) A `tool_use` block with a large `input` is NOT touched — only
    /// tool_result content is compressed.
    #[test]
    fn tool_use_input_not_touched() {
        let big_input = "Z".repeat(5000);
        let body = json!({
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_use",
                    "name": "some_tool",
                    "input": { "data": big_input.clone() }
                }]
            }]
        });
        let knobs = result_only(100, 50);
        let result = trim_native(body, &knobs);
        let input_val = result["messages"][0]["content"][0]["input"]["data"].as_str().unwrap();
        assert_eq!(input_val, big_input, "tool_use input must not be touched");
    }

    /// (m) Determinism on a body with both long descriptions and long tool_results.
    #[test]
    fn deterministic_desc_and_tool_results() {
        let long_desc = "D".repeat(400);
        let big_result = "R".repeat(8000);
        let body = json!({
            "tools": [{
                "name": "heavy_tool",
                "description": long_desc,
                "input_schema": { "type": "object", "properties": {} }
            }],
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_det",
                    "content": big_result
                }]
            }]
        });
        let knobs = NativeKnobs { tool_max_desc_chars: 80, tool_result_head: 500, tool_result_tail: 200 };
        let result_a = trim_native(body.clone(), &knobs);
        let result_b = trim_native(body, &knobs);
        assert_eq!(result_a, result_b, "must be deterministic");
    }
}
