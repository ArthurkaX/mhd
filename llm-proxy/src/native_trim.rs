//! Clean-room native trim engine (side artifact — NOT wired to the live path).
//!
//! v2 levers:
//! - Truncate every `"description"` string value (anywhere inside each
//!   `tools[]` element, including `input_schema` sub-properties and arrays)
//!   to `tool_max_desc_chars`.
//! - Compress large `tool_result` content blocks (string form or array form)
//!   to head + elision marker + tail.
//! - Whitespace compression (`ws_enabled`): strip trailing whitespace, collapse
//!   blank-line runs, collapse inner multi-spaces — gated by a protected-
//!   content detector that leaves ASCII diagrams, markdown, and code fences
//!   byte-identical.
//!
//! Pure, deterministic, fail-open: any unexpected shape leaves the body
//! unchanged. Never enlarges the body.

use std::collections::HashMap;

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
    /// Master switch for whitespace compression.
    /// Default: **false** — validate offline first, then enable via live toggle.
    pub ws_enabled: bool,
    /// Strip trailing spaces/tabs from each line. Default: true.
    pub ws_strip_trailing: bool,
    /// Collapse runs of MORE than this many consecutive blank lines down to
    /// exactly this many. Default: 5.
    pub ws_blank_run_max: usize,
    /// Collapse runs of 2+ spaces that are NOT at the start of a line to a
    /// single space. Leading indentation (spaces + tabs) is preserved. Tabs
    /// elsewhere are never touched. Default: true.
    pub ws_collapse_inner: bool,
}

impl Default for NativeKnobs {
    fn default() -> Self {
        Self {
            tool_max_desc_chars: 150,
            tool_result_head: 3000,
            tool_result_tail: 1000,
            ws_enabled: false,
            ws_strip_trailing: true,
            ws_blank_run_max: 5,
            ws_collapse_inner: true,
        }
    }
}

// ── protected content detector ────────────────────────────────────────────────

/// Returns `true` when the tool_result block must be left completely untouched
/// (no whitespace compression, no head/tail truncation).
///
/// Layered — any single layer returning `true` protects the whole block:
///
/// 1. **Provenance** — `src_ext` is the lowercased file extension of the file
///    the tool_result came from (derived by the caller from the correlated
///    `tool_use` block). Doc/art-prone extensions are always protected:
///    `md`, `markdown`, `txt`, `rst`, `adoc`, `org`.
/// 2. **Fenced code block** — `text.contains("```")` → protected.
/// 3. **Box-drawing / diagram glyphs** — count chars in the Unicode ranges
///    U+2500..=U+257F (box drawing) and U+2190..=U+21FF (arrows). If count
///    ≥ 6 → protected.
pub fn tool_result_protected(text: &str, src_ext: Option<&str>) -> bool {
    // Layer 1: provenance
    if let Some(ext) = src_ext {
        if matches!(ext, "md" | "markdown" | "txt" | "rst" | "adoc" | "org") {
            return true;
        }
    }
    // Layer 2: fenced code block
    if text.contains("```") {
        return true;
    }
    // Layer 3: box-drawing / diagram glyphs
    let glyph_count = text
        .chars()
        .filter(|&c| {
            ('\u{2500}'..='\u{257F}').contains(&c) || ('\u{2190}'..='\u{21FF}').contains(&c)
        })
        .count();
    glyph_count >= 6
}

// ── whitespace ops ────────────────────────────────────────────────────────────

/// Apply the enabled whitespace operations to `text`. Never enlarges.
///
/// Operations applied in order (each gated by its own knob):
/// 1. Strip trailing spaces/tabs from each line.
/// 2. Collapse runs of >ws_blank_run_max consecutive blank lines to exactly
///    ws_blank_run_max blank lines.
/// 3. Collapse runs of 2+ spaces in non-leading positions to a single space.
///    Leading indentation (spaces + tabs) is preserved; tabs in the rest of
///    the line are never removed or collapsed.
fn apply_ws_ops(text: &str, knobs: &NativeKnobs) -> String {
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

    // Step 1: strip trailing spaces/tabs
    if knobs.ws_strip_trailing {
        for line in &mut lines {
            let trimmed = line.trim_end_matches(|c| c == ' ' || c == '\t');
            if trimmed.len() < line.len() {
                *line = trimmed.to_string();
            }
        }
    }

    // Step 2: collapse blank-line runs (blank = empty after step 1)
    {
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        let mut blank_run: usize = 0;
        for line in lines {
            if line.is_empty() {
                blank_run += 1;
                if blank_run <= knobs.ws_blank_run_max {
                    out.push(line);
                }
                // else: discard the excess blank line (run > max → collapse to max)
            } else {
                blank_run = 0;
                out.push(line);
            }
        }
        lines = out;
    }

    // Step 3: collapse inner multi-spaces
    if knobs.ws_collapse_inner {
        for line in &mut lines {
            let new = collapse_inner_spaces(line);
            if new.len() < line.len() {
                *line = new;
            }
        }
    }

    let mut result = lines.join("\n");
    if ends_with_newline {
        result.push('\n');
    }
    result
}

/// Collapse runs of 2+ consecutive spaces in the non-leading portion of `line`
/// to a single space. Leading whitespace (spaces + tabs) is preserved verbatim.
/// Tabs in the non-leading portion are never removed or collapsed.
fn collapse_inner_spaces(line: &str) -> String {
    // Identify the boundary of leading whitespace.
    let inner_start = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    let leading = &line[..inner_start];
    let rest = &line[inner_start..];

    // Fast path: nothing to collapse.
    if rest.is_empty() || !rest.contains("  ") {
        return line.to_string();
    }

    let mut collapsed = String::with_capacity(rest.len());
    let mut in_space_run = false;
    for c in rest.chars() {
        if c == ' ' {
            if !in_space_run {
                collapsed.push(c);
                in_space_run = true;
            }
            // else: extra space in a run — skip it
        } else {
            in_space_run = false;
            collapsed.push(c);
        }
    }

    format!("{leading}{collapsed}")
}

// ── provenance map builder ────────────────────────────────────────────────────

/// Scan all messages for `tool_use` blocks and build a map of
/// `tool_use_id → Option<lowercased_file_extension>`.
///
/// The extension is derived from the `file_path` (or `path`) field inside the
/// tool_use's `input` object using [`std::path::Path::extension`]. If no path
/// is present (or the path has no extension), the value is `None`.
fn build_id_to_ext(messages: &[Value]) -> HashMap<String, Option<String>> {
    let mut map = HashMap::new();
    for msg in messages {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = b.get("id").and_then(|v| v.as_str()) else { continue };
            let ext = b.get("input").and_then(|inp| {
                let obj = inp.as_object()?;
                let path_str = obj
                    .get("file_path")
                    .or_else(|| obj.get("path"))
                    .and_then(|p| p.as_str())?;
                let ext_os = std::path::Path::new(path_str).extension()?;
                let ext_str = ext_os.to_str()?;
                Some(ext_str.to_ascii_lowercase())
            });
            map.insert(id.to_string(), ext);
        }
    }
    map
}

// ── tool_result compressor ────────────────────────────────────────────────────

/// Walk a message's content array; for each `tool_result` block:
/// - Look up provenance (file extension) via `id_to_ext`.
/// - Run `tool_result_protected` on the combined text.
/// - **If protected** → skip entirely (leave byte-identical).
/// - **If not protected** → apply whitespace ops (when enabled), then
///   head+tail compression.
///
/// Deterministic, fail-open, never enlarges. Only touches tool_result text —
/// never tool_use, user text, or images.
fn compress_tool_results(
    msg: &mut Value,
    knobs: &NativeKnobs,
    id_to_ext: &HashMap<String, Option<String>>,
) {
    let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for block in content.iter_mut() {
        let Some(b) = block.as_object_mut() else { continue };
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            continue;
        }

        // Resolve provenance extension for this tool_result.
        let tool_use_id = b
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let src_ext: Option<&str> = id_to_ext
            .get(&tool_use_id)
            .and_then(|opt| opt.as_deref());

        // Determine whether the block is protected.
        let protected = match b.get("content") {
            Some(Value::String(s)) => tool_result_protected(s, src_ext),
            Some(Value::Array(arr)) => {
                // Concatenate all inner text blocks for the detector.
                let combined: String = arr
                    .iter()
                    .filter_map(|inner| {
                        let ib = inner.as_object()?;
                        if ib.get("type").and_then(|t| t.as_str()) != Some("text") {
                            return None;
                        }
                        ib.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                tool_result_protected(&combined, src_ext)
            }
            _ => false,
        };

        if protected {
            // Leave the entire block byte-identical — no ws, no head/tail.
            continue;
        }

        // Not protected: apply ws then head/tail.
        match b.get_mut("content") {
            Some(Value::String(s)) => {
                if let Some(new) = transform_text(s, knobs) {
                    *s = new;
                }
            }
            Some(Value::Array(arr)) => {
                for inner in arr.iter_mut() {
                    let Some(ib) = inner.as_object_mut() else { continue };
                    if ib.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(Value::String(s)) = ib.get_mut("text") {
                        if let Some(new) = transform_text(s, knobs) {
                            *s = new;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Apply whitespace ops (if enabled) then head+tail compression to `s`.
/// Returns `Some(new)` if any transformation occurred; `None` if `s` is
/// unchanged (never enlarges).
fn transform_text(s: &str, knobs: &NativeKnobs) -> Option<String> {
    // Step 1: whitespace ops (only when enabled).
    let ws_out: Option<String> = if knobs.ws_enabled {
        let after = apply_ws_ops(s, knobs);
        if after != s { Some(after) } else { None }
    } else {
        None
    };

    // Step 2: head+tail compression on the (possibly ws-reduced) text.
    let current: &str = ws_out.as_deref().unwrap_or(s);
    let ht_out = compress_text(current, knobs);

    match (ws_out, ht_out) {
        (_, Some(compressed)) => Some(compressed), // head+tail result (already post-ws)
        (Some(ws_reduced), None) => Some(ws_reduced), // only ws changed
        (None, None) => None,                          // no change
    }
}

/// Return `Some(compressed)` when `s` is long enough to compress, else `None`.
/// Keeps the first `head` and last `tail` chars (char boundaries) with a marker
/// `\n…[elided N chars]…\n` between. Only compresses when it actually shrinks the
/// text (i.e. total chars > head + tail + marker overhead), so it never enlarges.
fn compress_text(s: &str, knobs: &NativeKnobs) -> Option<String> {
    let total = s.chars().count();
    let head = knobs.tool_result_head;
    let tail = knobs.tool_result_tail;
    // Only worth it if there is a meaningful middle to drop.
    if total <= head + tail {
        return None;
    }
    let elided = total - head - tail;
    let head_str: String = s.chars().take(head).collect();
    let tail_str: String = s.chars().skip(total - tail).collect();
    let candidate = format!("{head_str}\n…[elided {elided} chars]…\n{tail_str}");
    // Guard: never enlarge.
    if candidate.chars().count() >= total {
        return None;
    }
    Some(candidate)
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

// ── public entry point ────────────────────────────────────────────────────────

/// Apply the native trim engine to `body` using the provided `knobs`.
///
/// 1. Truncates tool descriptions (top-level + nested) inside `tools[]`.
/// 2. Builds a provenance map: scans all messages for `tool_use` blocks and
///    records `tool_use_id → Option<file_extension>`.
/// 3. Compresses large `tool_result` content blocks inside `messages[]`,
///    skipping any block detected as protected (diagrams, markdown, code fences).
///
/// Scoped strictly — never touches `system` or other top-level keys.
/// Deterministic and fail-open: unexpected shapes are left unchanged.
/// Never enlarges the body.
pub fn trim_native(mut body: Value, knobs: &NativeKnobs) -> Value {
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    // 1) Tool descriptions (top-level + nested), scoped to the tools array.
    if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools.iter_mut() {
            truncate_descriptions(tool, knobs.tool_max_desc_chars);
        }
    }
    // 2) Build tool_use_id → file_ext provenance map (immutable pass).
    let id_to_ext: HashMap<String, Option<String>> = build_id_to_ext(
        obj.get("messages")
            .and_then(|m| m.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]),
    );
    // 3) Large tool_result blocks (mutable pass, protection-gated).
    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            compress_tool_results(msg, knobs, &id_to_ext);
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
        NativeKnobs {
            tool_max_desc_chars: max_chars,
            tool_result_head: 1_000_000,
            tool_result_tail: 1_000_000,
            ..Default::default()
        }
    }

    /// Build knobs that only vary the tool_result budget; desc truncation is
    /// effectively disabled.
    fn result_only(head: usize, tail: usize) -> NativeKnobs {
        NativeKnobs {
            tool_max_desc_chars: usize::MAX,
            tool_result_head: head,
            tool_result_tail: tail,
            ..Default::default()
        }
    }

    // ── existing description-truncation tests ─────────────────────────────────

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
    fn desc_at_boundary_unchanged() {
        let exact = "B".repeat(100);
        let body = json!({ "tools": [{ "name": "t", "description": exact.clone() }] });
        let result = trim_native(body, &desc_only(100));
        assert_eq!(result["tools"][0]["description"].as_str().unwrap(), exact);
    }

    /// (c) Multiple tools: only the one over the limit is truncated.
    #[test]
    fn multiple_tools_only_long_truncated() {
        let long_desc = "L".repeat(200);
        let short_desc = "S".repeat(50);
        let body = json!({
            "tools": [
                { "name": "long_tool", "description": long_desc },
                { "name": "short_tool", "description": short_desc.clone() }
            ]
        });
        let result = trim_native(body, &desc_only(100));
        // First tool was truncated.
        let d0 = result["tools"][0]["description"].as_str().unwrap();
        assert_eq!(d0.chars().count(), 101);
        assert!(d0.ends_with('…'));
        // Second tool was left alone.
        let d1 = result["tools"][1]["description"].as_str().unwrap();
        assert_eq!(d1, short_desc);
    }

    /// (d) No tools array → body unchanged.
    #[test]
    fn no_tools_array_unchanged() {
        let body = json!({ "model": "claude-3", "messages": [] });
        let result = trim_native(body.clone(), &desc_only(100));
        assert_eq!(result, body);
    }

    /// (e) Multi-byte chars: truncation cuts on char boundaries.
    #[test]
    fn multibyte_desc_truncated() {
        let cyrillic = "Ж".repeat(100); // 2-byte chars, 200 bytes total
        let body = json!({
            "tools": [
                { "name": "tool_cyr", "description": cyrillic, "input_schema": { "type": "object", "properties": {} } }
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

    // ── existing tool_result compression tests ────────────────────────────────

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
        assert!(
            compressed.chars().count() < big_text.chars().count(),
            "compressed should be shorter than original"
        );
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
        let val = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(val, small_text, "small result must be left unchanged");
    }

    /// (k) Array-form tool_result: each inner text block is compressed
    /// individually when it exceeds the budget.
    #[test]
    fn array_form_tool_result_compressed() {
        let big_text = "B".repeat(5000);
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_arr",
                    "content": [{
                        "type": "text",
                        "text": big_text.clone()
                    }]
                }]
            }]
        });
        let knobs = result_only(100, 50);
        let result = trim_native(body, &knobs);
        let text = result["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.len() < big_text.len(), "array-form text should be compressed");
        assert!(text.contains("…[elided"), "should contain elision marker");
    }

    /// (l) tool_use blocks inside messages are not touched.
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
        let knobs = NativeKnobs {
            tool_max_desc_chars: 80,
            tool_result_head: 500,
            tool_result_tail: 200,
            ..Default::default()
        };
        let result_a = trim_native(body.clone(), &knobs);
        let result_b = trim_native(body, &knobs);
        assert_eq!(result_a, result_b, "must be deterministic");
    }

    // ── protected-content detector unit tests ─────────────────────────────────

    /// Layer 1: a doc-prone extension (md) protects any content.
    #[test]
    fn protected_layer1_md_ext() {
        assert!(
            tool_result_protected("plain text, nothing special", Some("md")),
            "md extension should be protected"
        );
    }

    /// Layer 1: each of the protected extensions is recognized.
    #[test]
    fn protected_layer1_all_doc_exts() {
        for ext in &["md", "markdown", "txt", "rst", "adoc", "org"] {
            assert!(
                tool_result_protected("hello world", Some(ext)),
                "extension {ext} should be protected"
            );
        }
    }

    /// Layer 1: an unprotected extension (log, rs, py, …) is not a match.
    #[test]
    fn protected_layer1_non_doc_ext_not_protected() {
        for ext in &["log", "rs", "py", "json", "toml", "sh"] {
            assert!(
                !tool_result_protected("hello world no fences no glyphs", Some(ext)),
                "extension {ext} should NOT be protected by layer 1"
            );
        }
    }

    /// Layer 1: no extension at all → not protected by layer 1.
    #[test]
    fn protected_layer1_none_ext() {
        assert!(
            !tool_result_protected("hello world", None),
            "None extension should not trigger layer 1"
        );
    }

    /// Layer 2: text containing triple-backtick → protected regardless of ext.
    #[test]
    fn protected_layer2_fence() {
        let text = "here is some code:\n```rust\nfn main() {}\n```\n";
        assert!(
            tool_result_protected(text, None),
            "triple-backtick should be protected"
        );
        // Also protected even with a non-doc ext.
        assert!(
            tool_result_protected(text, Some("log")),
            "triple-backtick + log ext should still be protected"
        );
    }

    /// Layer 2: no backtick → not triggered by layer 2.
    #[test]
    fn protected_layer2_no_fence_not_triggered() {
        assert!(
            !tool_result_protected("no backticks here at all", None),
            "no backtick should not trigger layer 2"
        );
    }

    /// Layer 3: the canonical diagram block with box-drawing + arrow glyphs.
    #[test]
    fn protected_layer3_diagram_glyphs() {
        let diagram = "\
                      ┌───────────────────────────┐\n\
       ┌───────────→──┤                           ├──→───────────┐\n\
       │   ┌───────→──┤       Control object      ├──→───────┐   │\n\
       ↑   ↑   ↑                                         ↓   ↓   ↓\n\
┌──────┴───┴───┴────┐                               ┌────┴───┴───┴──────┐\n\
│     Actuators     │                               │      Sensors      │\n\
└──────┬───┬───┬────┘                               └───┬───┬───┬───────┘";
        assert!(
            tool_result_protected(diagram, None),
            "diagram with box-drawing + arrows should be protected"
        );
    }

    /// Layer 3: exactly 5 glyphs → NOT protected (need ≥ 6).
    #[test]
    fn protected_layer3_five_glyphs_not_protected() {
        // 5 box-drawing chars: ┌ ─ ─ ─ ┐
        let text = "┌───┐ plain text here without backticks or doc extension";
        let glyph_count = text
            .chars()
            .filter(|&c| {
                ('\u{2500}'..='\u{257F}').contains(&c) || ('\u{2190}'..='\u{21FF}').contains(&c)
            })
            .count();
        // Sanity-check our test data has exactly 5.
        assert_eq!(glyph_count, 5, "test data should have exactly 5 glyphs");
        assert!(
            !tool_result_protected(text, None),
            "5 glyphs should NOT be protected (need ≥ 6)"
        );
    }

    /// Layer 3: exactly 6 glyphs → protected.
    #[test]
    fn protected_layer3_six_glyphs_protected() {
        // 6 box-drawing chars: ┌ ─ ─ ─ ─ ┐
        let text = "┌────┐ plain text";
        let glyph_count = text
            .chars()
            .filter(|&c| {
                ('\u{2500}'..='\u{257F}').contains(&c) || ('\u{2190}'..='\u{21FF}').contains(&c)
            })
            .count();
        assert_eq!(glyph_count, 6, "test data should have exactly 6 glyphs");
        assert!(
            tool_result_protected(text, None),
            "6 glyphs should be protected"
        );
    }

    // ── integration: diagram block left byte-identical ────────────────────────

    /// The canonical diagram as a tool_result — even with ws_enabled=true and
    /// a very small head/tail budget, the block must emerge byte-identical.
    #[test]
    fn diagram_block_untouched_by_trim() {
        let diagram = "\
                      ┌───────────────────────────┐\n\
       ┌───────────→──┤                           ├──→───────────┐\n\
       │   ┌───────→──┤       Control object      ├──→───────┐   │\n\
       ↑   ↑   ↑                                         ↓   ↓   ↓\n\
┌──────┴───┴───┴────┐                               ┌────┴───┴───┴──────┐\n\
│     Actuators     │                               │      Sensors      │\n\
└──────┬───┬───┬────┘                               └───┬───┬───┬───────┘";

        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_diag",
                    "content": diagram
                }]
            }]
        });

        // Tiny head/tail + ws enabled: without protection this WOULD compress.
        let knobs = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            ws_enabled: true,
            ..Default::default()
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(out, diagram, "diagram block must be left byte-identical");
    }

    // ── integration: md provenance protects via tool_use_id correlation ───────

    /// A tool_result correlated (via tool_use_id) to a Read tool_use whose
    /// input.file_path ends in .md must be left untouched — even without any
    /// box glyphs or code fences, and even with aggressive head/tail settings.
    #[test]
    fn md_provenance_protects_via_tool_use_id() {
        // Plain markdown text — no fences, no box glyphs.
        let md_content = "# Hello\n\nThis is plain markdown with no fences or diagrams.\n"
            .repeat(50); // make it long enough to normally trigger head/tail

        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_read_md",
                        "name": "Read",
                        "input": { "file_path": "/home/user/project/README.md" }
                    }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_read_md",
                        "content": md_content.clone()
                    }]
                }
            ]
        });

        // Very small budget + ws enabled → would compress any unprotected block.
        let knobs = NativeKnobs {
            tool_result_head: 20,
            tool_result_tail: 20,
            ws_enabled: true,
            ..Default::default()
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][1]["content"][0]["content"].as_str().unwrap();
        assert_eq!(out, md_content, "md-provenance block must be left byte-identical");
    }

    // ── integration: eligible block is ws-compressed ──────────────────────────

    /// A plain text tool_result (no ext, no fences, no glyphs) with:
    ///   - trailing spaces on some lines
    ///   - more than 5 consecutive blank lines
    ///   - inner multi-spaces (2+ consecutive spaces not at line start)
    ///   - a line with 4 leading spaces (leading indent MUST be preserved)
    /// should have those compressed when ws_enabled=true, and the leading
    /// indentation must be preserved.
    #[test]
    fn eligible_block_ws_compressed() {
        // Build the input text.
        let input = "line one   \nline two  \n".to_string()  // trailing spaces
            + "\n\n\n\n\n\n"                                  // 6 blank lines (> max=5)
            + "    indented line\n"                           // 4 leading spaces (must survive)
            + "foo  bar  baz\n"                               // inner multi-spaces
            + "tab\there\n";                                  // tab in non-leading pos (untouched)

        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_elig",
                    "content": input.clone()
                }]
            }]
        });

        let knobs = NativeKnobs {
            tool_max_desc_chars: usize::MAX,
            tool_result_head: 10_000, // large enough that head/tail won't trigger
            tool_result_tail: 10_000,
            ws_enabled: true,
            ws_strip_trailing: true,
            ws_blank_run_max: 5,
            ws_collapse_inner: true,
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"].as_str().unwrap();

        // 1. Trailing spaces stripped.
        assert!(!out.contains("line one   "), "trailing spaces should be stripped");
        assert!(out.contains("line one\n"), "content of line one should survive");

        // 2. Blank-line run collapsed: 6 blank lines → 5.
        //    The original has 7 consecutive \n ("line two\n" + 6 blank \n).
        //    After collapsing to 5 blank lines it becomes 6 consecutive \n.
        assert!(
            !out.contains("\n\n\n\n\n\n\n"),
            "7 consecutive \\n (6 blank lines) should be collapsed"
        );
        // Exactly 5 blank lines should remain between "line two" and "indented line".
        assert!(
            out.contains("line two\n\n\n\n\n\n    indented line"),
            "exactly 5 blank lines should remain after collapse"
        );

        // 3. Leading 4-space indent preserved.
        assert!(out.contains("    indented line"), "4-space leading indent must survive");

        // 4. Inner multi-spaces collapsed.
        assert!(!out.contains("foo  bar"), "double space in 'foo  bar' should be collapsed");
        assert!(out.contains("foo bar"), "inner space should be collapsed to single");
        assert!(!out.contains("bar  baz"), "double space in 'bar  baz' should be collapsed");

        // 5. Tab in non-leading position is untouched.
        assert!(out.contains("tab\there"), "tab in non-leading pos must survive");
    }

    // ── ws_enabled=false: no whitespace changes ───────────────────────────────

    /// When ws_enabled=false the ws ops are entirely skipped; only head/tail
    /// compression is applied (or not) as before.
    #[test]
    fn ws_disabled_no_whitespace_changes() {
        let input = "trailing   \n\n\n\n\n\n\nfoo  bar\n    leading";

        // With ws_disabled and head/tail large enough, the text is unchanged.
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_nodelta",
                    "content": input
                }]
            }]
        });
        let knobs = NativeKnobs {
            tool_result_head: 10_000,
            tool_result_tail: 10_000,
            ws_enabled: false, // no ws
            ..Default::default()
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(out, input, "ws_enabled=false must not change whitespace");
    }

    // ── determinism with ws on ────────────────────────────────────────────────

    #[test]
    fn deterministic_ws_on() {
        let text = "line   \n\n\n\n\n\n\nfoo  bar  baz\n    indented\n".repeat(10);
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_det2",
                    "content": text
                }]
            }]
        });
        let knobs = NativeKnobs {
            tool_result_head: 10_000,
            tool_result_tail: 10_000,
            ws_enabled: true,
            ..Default::default()
        };
        let a = trim_native(body.clone(), &knobs);
        let b = trim_native(body, &knobs);
        assert_eq!(a, b, "ws-on trim must be deterministic");
    }

    // ── fenced code block protected ───────────────────────────────────────────

    /// A tool_result containing triple-backtick is left untouched.
    #[test]
    fn fenced_block_protected() {
        let fenced = "Output:\n```json\n{\"key\": \"value\"}\n```\n".repeat(100);
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_fence",
                    "content": fenced.clone()
                }]
            }]
        });
        let knobs = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            ws_enabled: true,
            ..Default::default()
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert_eq!(out, fenced, "fenced block must be left byte-identical");
    }
}
