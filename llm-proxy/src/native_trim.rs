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
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct NativeKnobs {
    /// Truncate any tool `description` longer than this many chars.
    pub tool_max_desc_chars: usize,
    /// Tool_result blocks whose text exceeds `tool_result_head + tool_result_tail`
    /// by more than this margin are compressed to head + marker + tail.
    pub tool_result_head: usize,
    pub tool_result_tail: usize,
    /// Minimum number of chars that must be dropped before compression is applied.
    /// Avoids spending overhead on trivially small elisions. Default: **4000**.
    /// With defaults head=3000/tail=1000, compression only triggers at total ≥ 8000.
    pub tool_result_min_elide: usize,
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
    /// Strip `thinking` / `redacted_thinking` content blocks from every
    /// assistant message **except** the last assistant message.
    /// Anthropic accepts omitting whole thinking blocks from prior turns, but
    /// the latest assistant message must be sent byte-identical.
    /// Default: **false**.
    pub strip_thinking: bool,
    /// When `true` (default), Layer 2 fence protection only fires when the
    /// fenced content also looks code-like (line-numbered shape or code
    /// keywords). When `false`, a triple-backtick alone protects (old
    /// behavior). Set `false` to revert without a rebuild.
    pub tool_result_fence_requires_code: bool,
    /// Minimum glyph density (arrow_count + box_count relative to total chars)
    /// for the arrow-driven diagram route (Route a) to protect the block.
    /// Default: **0.01** (1%). Measured: on opencode corpus median arrow glyph
    /// density is ~0.0004 (noise); on anthropic corpus real diagrams have
    /// median density ~0.0217. 0.01 cleanly separates them.
    pub tool_result_arrow_density_min: f64,
}

impl Default for NativeKnobs {
    fn default() -> Self {
        Self {
            tool_max_desc_chars: 150,
            tool_result_head: 3000,
            tool_result_tail: 1000,
            tool_result_min_elide: 4000,
            ws_enabled: false,
            ws_strip_trailing: true,
            ws_blank_run_max: 5,
            ws_collapse_inner: true,
            strip_thinking: false,
            tool_result_fence_requires_code: true,
            tool_result_arrow_density_min: 0.01,
        }
    }
}

impl NativeKnobs {
    /// Declutter-only profile for a designated free/cheap-tier model: keeps
    /// whitespace cleanup and old-thinking stripping (lossless), and disables
    /// both lossy levers (tool-description truncation, tool_result head/tail
    /// compression) by making their trigger conditions unreachable — not a
    /// "generous" setting, a structural no-op for those two code paths.
    pub fn light() -> Self {
        Self {
            tool_max_desc_chars: usize::MAX,
            tool_result_min_elide: usize::MAX,
            ws_enabled: true,
            strip_thinking: true,
            ..Self::default()
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
/// 3. **Diagram detection — density + structural glyphs.**
///    An absolute glyph count ≥ 6 misfires on large files where a few unicode
///    frame chars appear incidentally (stray `│` in comments, git/cargo output).
///    A density + structural check separates genuine diagrams (small, glyph-dense,
///    structured) from large text that merely contains a handful of drawing chars.
///    Three routes to protection (any one suffices):
///    - **Arrow-driven diagram:** `arrow_count ≥ 3` — arrows are rarely
///      incidental in flow/control diagrams.
///    - **Dense box diagram:** `box_count + arrow_count ≥ 6` AND glyph density
///      `(count / total_chars) ≥ 2%` — a real grid is dense; a 50KB file with
///      stray frames is well under this threshold.
///    - **Cornered boxes:** `corner_count ≥ 4` (a closed box needs 4 corners)
///      AND `box_count ≥ 8` (guards against coincidental corner chars in prose).
/// Returns `true` if `text` looks like actual source code (not a JSON dump,
/// log, or day-report). Used by Layer 2 when `fence_requires_code=true`.
///
/// Two routes (either suffices):
/// - **Line-numbered shape**: ≥ 3 lines in the first 40 that begin with
///   optional whitespace, then one or more digits, then a tab (`^\s*\d+\t`).
///   This matches the Read tool's output format for source files.
/// - **Code keywords**: total occurrences of `fn `, `struct `, `impl `,
///   `pub `, `def `, `class `, `import `, `function `, `interface `, `trait `
///   (trailing space keeps them word-like and avoids matching inside JSON keys)
///   is ≥ 2.
fn looks_like_code(text: &str) -> bool {
    // Route 1: line-numbered shape (scan first 40 lines).
    let mut line_num_count: usize = 0;
    for line in text.lines().take(40) {
        // Strip optional leading whitespace, then expect digits, then '\t'.
        let rest = line.trim_start_matches(|c: char| c == ' ' || c == '\t');
        if rest.is_empty() {
            continue;
        }
        let digit_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        if digit_end > 0 && rest.as_bytes().get(digit_end) == Some(&b'\t') {
            line_num_count += 1;
            if line_num_count >= 3 {
                return true;
            }
        }
    }
    // Route 2: code keyword frequency.
    const KEYWORDS: &[&str] = &[
        "fn ",
        "struct ",
        "impl ",
        "pub ",
        "def ",
        "class ",
        "import ",
        "function ",
        "interface ",
        "trait ",
    ];
    let mut kw_total: usize = 0;
    for kw in KEYWORDS {
        // Count non-overlapping occurrences.
        let mut pos = 0;
        while let Some(idx) = text[pos..].find(kw) {
            kw_total += 1;
            if kw_total >= 2 {
                return true;
            }
            pos += idx + kw.len();
        }
    }
    false
}

pub fn tool_result_protected(
    text: &str,
    src_ext: Option<&str>,
    fence_requires_code: bool,
    arrow_density_min: f64,
) -> bool {
    // Layer 1: provenance
    if let Some(ext) = src_ext {
        if matches!(
            ext,
            // documentation
            "md" | "markdown" | "txt" | "rst" | "adoc" | "org"
            // source code — never elide; the model needs exact bytes to Edit
            | "py" | "js" | "mjs" | "cjs" | "ts" | "jsx" | "tsx" | "rs" | "go"
            | "java" | "kt" | "scala" | "c" | "h" | "cc" | "cpp" | "hpp" | "cxx"
            | "cs" | "rb" | "php" | "swift" | "m" | "mm" | "lua" | "dart" | "ex" | "exs"
            | "sh" | "bash" | "zsh" | "ps1" | "sql" | "r" | "jl" | "pl" | "pm"
            // structured / config — edited exactly too
            | "json" | "jsonc" | "yaml" | "yml" | "toml" | "ini" | "xml"
            | "html" | "htm" | "css" | "scss" | "less" | "vue" | "svelte"
        ) {
            return true;
        }
    }
    // Layer 2: fenced code block.
    // When fence_requires_code=true, a fence alone does NOT protect — it only
    // protects if the fenced content also looks code-like. When false, a
    // triple-backtick alone protects (original behavior).
    if text.contains("```") {
        if !fence_requires_code || looks_like_code(text) {
            return true;
        }
    }
    // Layer 3: diagram detection — density + structural glyphs.
    let mut box_count: usize = 0;
    let mut arrow_count: usize = 0;
    let mut corner_count: usize = 0;
    let mut total_chars: usize = 0;
    for c in text.chars() {
        total_chars += 1;
        if ('\u{2500}'..='\u{257F}').contains(&c) {
            box_count += 1;
            // Corners: ┌ ┐ └ ┘ ╔ ╗ ╚ ╝
            if matches!(
                c,
                '\u{250C}'
                    | '\u{2510}'
                    | '\u{2514}'
                    | '\u{2518}'
                    | '\u{2554}'
                    | '\u{2557}'
                    | '\u{255A}'
                    | '\u{255D}'
            ) {
                corner_count += 1;
            }
        } else if ('\u{2190}'..='\u{21FF}').contains(&c) {
            arrow_count += 1;
        }
    }
    let combined = box_count + arrow_count;
    let density = combined as f64 / total_chars.max(1) as f64;
    // Route a: arrow-driven diagram — needs both ≥3 arrow glyphs AND density
    // above threshold (prevents false protection of large texts with stray
    // arrows; measured median density on opencode noise is 0.0004 vs real
    // diagrams 0.0217, so the default 0.01 cleanly separates them).
    if arrow_count >= 3 && density >= arrow_density_min {
        return true;
    }
    // Route b: dense box diagram
    if combined >= 6 && density >= 0.02 {
        return true;
    }
    // Route c: cornered boxes
    corner_count >= 4 && box_count >= 8
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
            let Some(id) = b.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
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
        let Some(b) = block.as_object_mut() else {
            continue;
        };
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            continue;
        }

        // Resolve provenance extension for this tool_result.
        let tool_use_id = b
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let src_ext: Option<&str> = id_to_ext.get(&tool_use_id).and_then(|opt| opt.as_deref());

        // Determine whether the block is protected.
        let fence_requires_code = knobs.tool_result_fence_requires_code;
        let arrow_density_min = knobs.tool_result_arrow_density_min;
        let protected = match b.get("content") {
            Some(Value::String(s)) => {
                tool_result_protected(s, src_ext, fence_requires_code, arrow_density_min)
            }
            Some(Value::Array(arr)) => {
                // Concatenate all inner text blocks for the detector.
                let combined: String = arr
                    .iter()
                    .filter_map(|inner| {
                        let ib = inner.as_object()?;
                        if ib.get("type").and_then(|t| t.as_str()) != Some("text") {
                            return None;
                        }
                        ib.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                tool_result_protected(&combined, src_ext, fence_requires_code, arrow_density_min)
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
                    let Some(ib) = inner.as_object_mut() else {
                        continue;
                    };
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
        (None, None) => None,                      // no change
    }
}

/// FNV-1a 64-bit hash over `data`. No new crates — inline implementation.
/// Same input always produces the same output (content-addressable).
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Return `Some(compressed)` when `s` is long enough to compress, else `None`.
///
/// - Applies a `min_elide` gate: the chars dropped must be ≥ `tool_result_min_elide`.
/// - Cuts on line boundaries (head ends at a `\n`; tail starts after a `\n`) so
///   no half-line fragments are emitted. Falls back to raw char cut when no `\n`
///   exists in the respective window.
/// - Embeds a deterministic recovery marker containing a FNV-1a id of the elided
///   middle so the same content always gets the same id (content-addressable seam
///   for a future expand-by-id feature).
/// - Never enlarges (`candidate.chars().count() < total` is guaranteed before returning).
fn compress_text(s: &str, knobs: &NativeKnobs) -> Option<String> {
    let total = s.chars().count();
    let head = knobs.tool_result_head;
    let tail = knobs.tool_result_tail;

    // Gate 1: enough chars to drop (checked on raw budget before line-snapping).
    let elided_raw = total.saturating_sub(head + tail);
    if elided_raw < knobs.tool_result_min_elide {
        return None;
    }

    // Line-aware head cut: take up to `head` chars, back off to last \n so
    // head_str ends with a complete line (inclusive of the \n).
    let head_chars: Vec<char> = s.chars().take(head).collect();
    let head_str: String = match head_chars.iter().rposition(|&c| c == '\n') {
        Some(last_nl) => head_chars[..=last_nl].iter().collect(),
        None => head_chars.into_iter().collect(), // no \n in window — raw char cut
    };

    // Line-aware tail cut: take last `tail` chars, advance past first \n so
    // tail_str starts at a line boundary.
    let tail_start_char = total.saturating_sub(tail);
    let tail_chars: Vec<char> = s.chars().skip(tail_start_char).collect();
    let tail_str: String = match tail_chars.iter().position(|&c| c == '\n') {
        Some(first_nl) => tail_chars[first_nl + 1..].iter().collect(),
        None => tail_chars.into_iter().collect(), // no \n in window — raw char cut
    };

    // Recompute elided after line-snapping (line-aware cuts only reduce head/tail,
    // so elided ≥ elided_raw ≥ min_elide — but re-check for safety).
    let head_len = head_str.chars().count();
    let tail_len = tail_str.chars().count();
    let elided = total.saturating_sub(head_len + tail_len);
    if elided < knobs.tool_result_min_elide {
        return None;
    }

    // Deterministic 6-hex-char id of the elided middle content (low 24 bits of FNV-1a).
    let elided_middle: String = s.chars().skip(head_len).take(elided).collect();
    let hash = fnv1a_64(elided_middle.as_bytes());
    let id_hex = format!("{:06x}", hash & 0x00FF_FFFF);

    let marker = format!(
        "\n...[elided {elided} chars \u{00B7} id {id_hex} \u{00B7} re-read source to view]...\n"
    );
    let candidate = format!("{head_str}{marker}{tail_str}");

    // Guard: never enlarge (should always hold when elided >> marker_len, but kept for safety).
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
// ── strip old thinking blocks ─────────────────────────────────────────────────

/// Remove `thinking` / `redacted_thinking` blocks from every assistant message
/// EXCEPT the last assistant message (which Anthropic requires unmodified when
/// it is part of an active tool-use turn). Whole-block omission only — never
/// modifies a block. Deterministic, fail-open.
fn strip_old_thinking(messages: &mut [Value]) {
    // index of the last assistant message
    let last_assistant = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
    let Some(last_idx) = last_assistant else {
        return;
    };
    for (i, msg) in messages.iter_mut().enumerate() {
        if i == last_idx {
            continue;
        } // leave latest assistant untouched
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        content.retain(|b| {
            let t = b.get("type").and_then(|t| t.as_str());
            t != Some("thinking") && t != Some("redacted_thinking")
        });
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
    // 4) Strip thinking/redacted_thinking from old assistant turns (optional).
    if knobs.strip_thinking {
        if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
            strip_old_thinking(messages);
        }
    }
    body
}

// ── OpenAI entry point ────────────────────────────────────────────────────────

/// Trim an OpenAI chat-completions request body (Zed/opencode/pi shape) using
/// the same primitives as the Anthropic engine. Truncates tool/function
/// descriptions (top-level + nested in `parameters`) and compresses the string
/// content of `role:"tool"` messages (protected-gate + ws + head/tail). Does NOT
/// strip thinking (OpenAI has no signed thinking blocks) and does NOT touch
/// tool_calls arguments. Deterministic, fail-open, never enlarges.
///
/// Levers applied:
/// 1. `tools[i].function.description` and any `"description"` keys nested inside
///    `tools[i].function.parameters` (JSON schema) → `truncate_descriptions`.
///    (Same helper as Anthropic; it recurses over any JSON subtree.)
/// 2. `messages[]` where `role == "tool"` and `content` is a string →
///    protected-gate check (`tool_result_protected(s, None)`) then
///    `transform_text` (ws + head/tail).  `src_ext` is `None` because OpenAI
///    `role:tool` messages carry no file-path provenance field; content-based
///    protection (fences, diagrams) still applies.
///
/// Deliberately skipped:
/// - `strip_thinking`: Anthropic-specific; OpenAI bodies carry no signed
///   thinking blocks.  `reasoning_content` on assistant turns is also left
///   untouched — it belongs to the upstream gateway, not to us.
/// - `tool_calls[].function.arguments`: the model's generated JSON args; we
///   never trim those (same policy as Anthropic `tool_use.input`).
pub fn trim_native_openai(mut body: Value, knobs: &NativeKnobs) -> Value {
    let Some(obj) = body.as_object_mut() else {
        return body;
    };
    // 1) Tool/function descriptions: top-level + nested in parameters schema.
    //    `truncate_descriptions` recurses over any JSON subtree, so calling it
    //    on the whole `tools[i]` element covers both `function.description` AND
    //    every `description` key buried inside `function.parameters`.
    if let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools.iter_mut() {
            truncate_descriptions(tool, knobs.tool_max_desc_chars);
        }
    }
    // 2) role:"tool" message string content → protected-gate + ws + head/tail.
    if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let is_tool = msg.get("role").and_then(|r| r.as_str()) == Some("tool");
            if !is_tool {
                continue;
            }
            let Some(content) = msg.get_mut("content") else {
                continue;
            };
            if let Value::String(s) = content {
                // Pass src_ext=None: no file-path provenance on OpenAI tool msgs;
                // content-based protection (fences / diagram glyphs) still fires.
                if !tool_result_protected(
                    s,
                    None,
                    knobs.tool_result_fence_requires_code,
                    knobs.tool_result_arrow_density_min,
                ) {
                    if let Some(new) = transform_text(s, knobs) {
                        *s = new;
                    }
                }
            }
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

    // ── light profile (declutter-only, non-lossy) ─────────────────────────

    /// The `light()` profile keeps whitespace and thinking-strip enabled while
    /// structurally disabling both lossy levers (tool_desc truncation and
    /// tool_result head/tail compression). On a large body that would normally
    /// compress under default knobs, the output must be byte-identical to the
    /// input, and a long tool description must survive untouched.
    #[test]
    fn light_profile_is_inert() {
        // tool_result body big enough to compress under Default::default()
        let big_text = "A".repeat(9000);
        let body = serde_json::json!({
            "model": "test",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tu01",
                            "content": big_text
                        }
                    ]
                }
            ],
            "tools": [
                {
                    "name": "big_tool",
                    "description": "X".repeat(5000)
                }
            ]
        });

        let knobs = NativeKnobs::light();

        // Lossy levers structurally disabled
        assert_eq!(knobs.tool_max_desc_chars, usize::MAX);
        assert_eq!(knobs.tool_result_min_elide, usize::MAX);

        // Lossless levers enabled
        assert!(knobs.ws_enabled);
        assert!(knobs.strip_thinking);

        // Run through native trim — tool_result must be byte-identical (no
        // compression markers inserted).
        let result = trim_native(body, &knobs);
        let content = &result["messages"][0]["content"][0]["content"];
        assert_eq!(content.as_str().unwrap(), big_text);

        // Tool description must survive untouched.
        let desc = result["tools"][0]["description"].as_str().unwrap();
        assert_eq!(desc.len(), 5000);
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
        assert_eq!(
            result["tools"][0]["description"],
            body["tools"][0]["description"]
        );
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
        assert!(
            desc.len() > 51,
            "Cyrillic chars are multi-byte, so byte len > char count"
        );

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
        assert_eq!(
            result["tools"][0]["description"].as_str().unwrap(),
            "short top"
        );
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
        let deep_desc =
            result["tools"][0]["input_schema"]["properties"]["foo"]["items"]["description"]
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
        let compressed = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        // Must be shorter.
        assert!(
            compressed.chars().count() < big_text.chars().count(),
            "compressed should be shorter than original"
        );
        // Must contain the elision marker.
        assert!(
            compressed.contains("...[elided"),
            "must contain elision marker"
        );
        // Must start with the head and end with the tail.
        let head: String = big_text.chars().take(100).collect();
        let tail: String = big_text
            .chars()
            .skip(big_text.chars().count() - 50)
            .collect();
        assert!(
            compressed.starts_with(&head),
            "must start with original head"
        );
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
        let val = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
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
        assert!(
            text.len() < big_text.len(),
            "array-form text should be compressed"
        );
        assert!(text.contains("...[elided"), "should contain elision marker");
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
        let input_val = result["messages"][0]["content"][0]["input"]["data"]
            .as_str()
            .unwrap();
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
            tool_result_protected("plain text, nothing special", Some("md"), true, 0.01),
            "md extension should be protected"
        );
    }

    /// Layer 1: each of the protected extensions is recognized.
    #[test]
    fn protected_layer1_all_doc_exts() {
        for ext in &["md", "markdown", "txt", "rst", "adoc", "org"] {
            assert!(
                tool_result_protected("hello world", Some(ext), true, 0.01),
                "extension {ext} should be protected"
            );
        }
    }

    /// Layer 1: an unprotected extension (log, rs, py, …) is not a match.
    #[test]
    fn protected_layer1_non_doc_ext_not_protected() {
        for ext in &["log", "rs", "py", "json", "toml", "sh"] {
            assert!(
                !tool_result_protected("hello world no fences no glyphs", Some(ext), true, 0.01),
                "extension {ext} should NOT be protected by layer 1"
            );
        }
    }

    /// Layer 1: no extension at all → not protected by layer 1.
    #[test]
    fn protected_layer1_none_ext() {
        assert!(
            !tool_result_protected("hello world", None, true, 0.01),
            "None extension should not trigger layer 1"
        );
    }

    /// Layer 2: text containing triple-backtick → protected regardless of ext.
    #[test]
    fn protected_layer2_fence() {
        // Text contains 2+ code keywords (`fn ` and `pub `) → looks_like_code=true → protected.
        let text = "here is some code:\n```rust\npub fn main() {}\npub fn helper() {}\n```\n";
        assert!(
            tool_result_protected(text, None, true, 0.01),
            "fenced code (2+ keywords) should be protected when fence_requires_code=true"
        );
        assert!(
            tool_result_protected(text, None, false, 0.01),
            "fenced code should be protected when fence_requires_code=false"
        );
        // Also protected even with a non-doc ext.
        assert!(
            tool_result_protected(text, Some("log"), true, 0.01),
            "fenced code + log ext should still be protected"
        );
    }

    /// Layer 2: no backtick → not triggered by layer 2.
    #[test]
    fn protected_layer2_no_fence_not_triggered() {
        assert!(
            !tool_result_protected("no backticks here at all", None, true, 0.01),
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
            tool_result_protected(diagram, None, true, 0.01),
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
            !tool_result_protected(text, None, true, 0.01),
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
            tool_result_protected(text, None, true, 0.01),
            "6 glyphs should be protected"
        );
    }

    // ── new layer-3 density / structural tests ────────────────────────────────

    /// A large (~3000-char) source-like text that contains only 6 stray box-drawing
    /// chars has density = 6/3000 ≈ 0.2% (well below the 2% threshold), no arrows,
    /// and no corners → NOT protected. This is the overshoot the new rule fixes.
    #[test]
    fn layer3_density_large_source_with_stray_glyphs_not_protected() {
        // 2994 plain ASCII chars + 6 stray │ (U+2502, box-drawing vertical bar).
        let base: String = "a".repeat(2994);
        let text = format!("{base}││││││");
        assert_eq!(text.chars().count(), 3000);
        let box_count = text
            .chars()
            .filter(|&c| ('\u{2500}'..='\u{257F}').contains(&c))
            .count();
        assert_eq!(box_count, 6, "should have exactly 6 box glyphs");
        assert!(
            !tool_result_protected(&text, None, true, 0.01),
            "large source with 6 stray glyphs (density 0.2%) must NOT be protected"
        );
    }

    /// A small dense box diagram (glyphs > 2% of chars) IS protected via the
    /// dense-box route (b) and the cornered-boxes route (c).
    #[test]
    fn layer3_density_small_dense_diagram_protected() {
        // A 5-row grid; glyph density is >> 2%.
        let diagram = "┌──┬──┐\n│  │  │\n├──┼──┤\n│  │  │\n└──┴──┘\n";
        let total = diagram.chars().count();
        let glyph_count = diagram
            .chars()
            .filter(|&c| {
                ('\u{2500}'..='\u{257F}').contains(&c) || ('\u{2190}'..='\u{21FF}').contains(&c)
            })
            .count();
        let density = glyph_count as f64 / total as f64;
        assert!(
            density >= 0.02,
            "test data must have >= 2% density; actual {density:.3}"
        );
        assert!(
            tool_result_protected(diagram, None, true, 0.01),
            "small dense box diagram must be protected"
        );
    }

    /// An arrow-only flow diagram with ≥ 3 arrow chars IS protected (route a).
    #[test]
    fn layer3_arrow_flow_protected() {
        // Three → (U+2192) chars — meets the arrow_count ≥ 3 threshold.
        let flow = "A → B → C → D";
        let arrow_count = flow
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        assert!(
            arrow_count >= 3,
            "test data must have >= 3 arrows; got {arrow_count}"
        );
        assert!(
            tool_result_protected(flow, None, true, 0.01),
            "flow diagram with >= 3 arrows must be protected"
        );
    }

    /// Cargo/git-style multiline output with one stray unicode char is well under
    /// all three layer-3 thresholds → NOT protected.
    #[test]
    fn layer3_cargo_style_output_not_protected() {
        // The | chars in compiler output are ASCII U+007C, not box drawing.
        // One stray U+2502 (│) is added to keep the test realistic; density << 2%.
        let output = concat!(
            "Compiling myapp v0.1.0 (/workspace)\n",
            "warning: unused import: `std::io`\n",
            "  --> src/main.rs:1:5\n",
            "   |\n",
            "1  | use std::io;\n",
            "   | ^^^^^^^^^^^\n",
            "   |\n",
            "   = note: `#[warn(unused_imports)]` on by default\n",
            "warning: 1 warning emitted │\n", // one stray box-drawing char
        );
        let total = output.chars().count();
        let box_count = output
            .chars()
            .filter(|&c| ('\u{2500}'..='\u{257F}').contains(&c))
            .count();
        let arrow_count = output
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        let corner_count = output
            .chars()
            .filter(|&c| {
                matches!(
                    c,
                    '\u{250C}'
                        | '\u{2510}'
                        | '\u{2514}'
                        | '\u{2518}'
                        | '\u{2554}'
                        | '\u{2557}'
                        | '\u{255A}'
                        | '\u{255D}'
                )
            })
            .count();
        let density = (box_count + arrow_count) as f64 / total.max(1) as f64;
        // Sanity-check that our test data really is below all three routes.
        assert!(
            arrow_count < 3,
            "test data must have < 3 arrows; got {arrow_count}"
        );
        assert!(
            corner_count < 4,
            "test data must have < 4 corners; got {corner_count}"
        );
        assert!(
            density < 0.02,
            "test data density must be < 2%; got {density:.4}"
        );
        assert!(
            !tool_result_protected(output, None, true, 0.01),
            "cargo output with one stray unicode char must NOT be protected"
        );
    }

    /// Layer 1 (provenance) and Layer 2 (fence) override the density check: they
    /// protect text that has zero drawing chars.
    #[test]
    fn layer1_and_layer2_override_density() {
        let plain = "hello world ".repeat(100); // no glyphs, no fences
        // Layer 1: .md extension forces protection.
        assert!(
            tool_result_protected(&plain, Some("md"), true, 0.01),
            ".md provenance must protect glyph-free text"
        );
        // Layer 2: when fence_requires_code=false (legacy mode), a bare fence
        // protects even glyph-free, non-code text.
        let with_fence = format!("{plain}```");
        assert!(
            tool_result_protected(&with_fence, None, false, 0.01),
            "fence must protect glyph-free text when fence_requires_code=false (legacy)"
        );
        // When fence_requires_code=true (default), a fence around non-code is NOT protected.
        assert!(
            !tool_result_protected(&with_fence, None, true, 0.01),
            "fence around non-code text must NOT be protected when fence_requires_code=true"
        );
    }

    // ── arrow-density gate tests ──────────────────────────────────────────────

    /// (1) Large text (~5000 chars) with exactly 3 stray arrow glyphs scattered
    /// in it → density ≈ 3/5000 = 0.0006 << 0.01 → NOT protected despite
    /// arrow_count >= 3. This is the false-positive the gate kills.
    #[test]
    fn arrow_low_density_not_protected() {
        let mut text = String::with_capacity(5500);
        for i in 0..120 {
            text.push_str(&format!(
                "[worker {}] task completed in {}ms\n",
                i % 10,
                (i * 7) % 100
            ));
        }
        // Inject 3 arrow chars (stray, in bulk log output):
        text.push_str("  →  →  →  (stray arrows in bulk log output)\n");
        // Sanity: arrow_count >= 3, total >> 300 → density << 0.01.
        let arrow_count: usize = text
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        assert!(
            arrow_count >= 3,
            "test data must have >= 3 arrows; got {arrow_count}"
        );
        let total = text.chars().count();
        let density = arrow_count as f64 / total as f64;
        assert!(
            density < 0.01,
            "test data density must be < 0.01; got {density:.6}"
        );
        assert!(
            !tool_result_protected(&text, None, true, 0.01),
            "large text with 3 stray arrows must NOT be protected at default density gate"
        );
    }

    /// (2) A small dense multi-line arrow flow: 6 arrows in ~28 chars
    /// → density ≈ 0.21 >> 0.01 → protected. This is the real diagram the
    /// gate must preserve. (Existing `layer3_arrow_flow_protected` covers the
    /// single-line "A → B → C → D" case; this adds a distinct multi-line variant.)
    #[test]
    fn arrow_high_density_protected() {
        // 6 arrow chars (3 per line) in a compact two-line diagram, ~28 chars total.
        let flow = "A → B → C → D\nE → F → G → H\n";
        let arrow_count: usize = flow
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        let total = flow.chars().count();
        let density = arrow_count as f64 / total as f64;
        assert_eq!(arrow_count, 6, "test data must have exactly 6 arrows");
        assert!(
            density >= 0.01,
            "test data density must be >= 0.01; got {density:.4}"
        );
        assert!(
            tool_result_protected(flow, None, true, 0.01),
            "small dense arrow flow must be protected at default density gate"
        );
    }

    /// (3) A text with 3 arrows at density just below 0.01 (3/301 ≈ 0.00997) is
    /// NOT protected when the gate is active (min=0.01) but IS protected when
    /// the gate is off (min=0.0, old absolute behavior). Proves the knob
    /// actually gates.
    #[test]
    fn arrow_density_threshold_boundary() {
        // 3 arrows in 301 total Unicode chars → density = 3/301 ≈ 0.00997.
        let text = format!("{}→ → →", "x".repeat(296));
        let arrow_count: usize = text
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        let total = text.chars().count();
        let density = arrow_count as f64 / total as f64;
        assert_eq!(arrow_count, 3, "test data must have exactly 3 arrows");
        assert!(
            density < 0.01,
            "test data density must be < 0.01 (got {density:.6}) to exercise gate"
        );
        // Gate off (min=0.0): old absolute behavior → protected (arrow_count >= 3 alone).
        assert!(
            tool_result_protected(&text, None, true, 0.0),
            "arrow_density_min=0.0 must protect (old absolute behavior)"
        );
        // Gate active (min=0.01): density below threshold → NOT protected.
        assert!(
            !tool_result_protected(&text, None, true, 0.01),
            "arrow_density_min=0.01 must block same text below threshold"
        );
    }

    /// (4) With arrow_density_min = 0.0 (gate fully off), any text with >= 3
    /// arrows is protected regardless of density — back-compat / A/B-off path.
    /// Also confirms that a text with < 3 arrows is NOT protected even when
    /// the gate is off.
    #[test]
    fn arrow_density_gate_off_restores_absolute() {
        // Build a large text (~5000 chars) with 3 arrows, density << 0.01.
        let mut text = String::with_capacity(5500);
        for i in 0..120 {
            text.push_str(&format!(
                "[worker {}] task completed in {}ms\n",
                i % 10,
                (i * 7) % 100
            ));
        }
        text.push_str("  →  →  →  (stray arrows)\n");
        let arrow_count: usize = text
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        assert!(
            arrow_count >= 3,
            "test data must have >= 3 arrows; got {arrow_count}"
        );
        // Gate off (min=0.0): >= 3 arrows alone suffices → protected.
        assert!(
            tool_result_protected(&text, None, true, 0.0),
            "arrow_density_min=0.0 must protect any text with >=3 arrows (back-compat)"
        );
        // A text with < 3 arrows is NOT protected even with gate off.
        let no_arrows = "plain text with no arrow glyphs at all";
        assert_eq!(
            no_arrows
                .chars()
                .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
                .count(),
            0,
            "control text must have zero arrows"
        );
        assert!(
            !tool_result_protected(no_arrows, None, true, 0.0),
            "arrow_density_min=0.0 must still require >= 3 arrows"
        );
    }

    /// (5) Regression guard: a clearly-real multi-line arrow diagram stays
    /// protected at the 0.01 default, proving the density gate did not break
    /// genuine diagram protection. (Existing `layer3_arrow_flow_protected`
    /// covers the canonical single-line "A → B → C → D" case.)
    #[test]
    fn arrow_regression_diagram_preserved() {
        // A realistic request/response flow: 6 arrows (3 per direction) in ~53 chars.
        let diagram = "Request → Auth → API → DB\nResponse ← Auth ← API ← DB\n";
        let arrow_count: usize = diagram
            .chars()
            .filter(|&c| ('\u{2190}'..='\u{21FF}').contains(&c))
            .count();
        let total = diagram.chars().count();
        let density = arrow_count as f64 / total as f64;
        assert_eq!(arrow_count, 6, "test data must have exactly 6 arrows");
        assert!(
            density >= 0.01,
            "test data density must be >= 0.01; got {density:.4}"
        );
        assert!(
            tool_result_protected(diagram, None, true, 0.01),
            "realistic multi-line arrow diagram must be protected at 0.01 default"
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
        let out = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(out, diagram, "diagram block must be left byte-identical");
    }

    // ── integration: md provenance protects via tool_use_id correlation ───────

    /// A tool_result correlated (via tool_use_id) to a Read tool_use whose
    /// input.file_path ends in .md must be left untouched — even without any
    /// box glyphs or code fences, and even with aggressive head/tail settings.
    #[test]
    fn md_provenance_protects_via_tool_use_id() {
        // Plain markdown text — no fences, no box glyphs.
        let md_content =
            "# Hello\n\nThis is plain markdown with no fences or diagrams.\n".repeat(50); // make it long enough to normally trigger head/tail

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
        let out = result["messages"][1]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            out, md_content,
            "md-provenance block must be left byte-identical"
        );
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
            + "tab\there\n"; // tab in non-leading pos (untouched)

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
            ..Default::default()
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();

        // 1. Trailing spaces stripped.
        assert!(
            !out.contains("line one   "),
            "trailing spaces should be stripped"
        );
        assert!(
            out.contains("line one\n"),
            "content of line one should survive"
        );

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
        assert!(
            out.contains("    indented line"),
            "4-space leading indent must survive"
        );

        // 4. Inner multi-spaces collapsed.
        assert!(
            !out.contains("foo  bar"),
            "double space in 'foo  bar' should be collapsed"
        );
        assert!(
            out.contains("foo bar"),
            "inner space should be collapsed to single"
        );
        assert!(
            !out.contains("bar  baz"),
            "double space in 'bar  baz' should be collapsed"
        );

        // 5. Tab in non-leading position is untouched.
        assert!(
            out.contains("tab\there"),
            "tab in non-leading pos must survive"
        );
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
        let out = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
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

    // ── min_elide gate ────────────────────────────────────────────────────────

    /// middle=5000 >= default min_elide=4000 → compresses;
    /// middle=2000 < default min_elide=4000 → left whole.
    #[test]
    fn min_elide_gate() {
        let knobs = NativeKnobs {
            tool_result_head: 3000,
            tool_result_tail: 1000,
            // tool_result_min_elide = 4000 from Default
            tool_max_desc_chars: usize::MAX,
            ..Default::default()
        };

        // total=9000, middle=5000 >= 4000 → should compress
        let big_text = "A".repeat(9000);
        assert!(
            compress_text(&big_text, &knobs).is_some(),
            "middle 5000 >= min_elide 4000 must compress"
        );

        // total=6000, middle=2000 < 4000 → must NOT compress
        let medium_text = "A".repeat(6000);
        assert!(
            compress_text(&medium_text, &knobs).is_none(),
            "middle 2000 < min_elide 4000 must NOT compress"
        );
    }

    // ── line-aware cuts ───────────────────────────────────────────────────────

    /// When the head char-budget lands mid-line, the cut backs off to the last
    /// preceding \n; when the tail char-budget starts mid-line, it advances to
    /// the start of the next complete line.
    #[test]
    fn line_aware_cuts_on_newline_boundary() {
        // total = 9 + 9 + 8000 + 1 + 9 + 1 = 8029 chars
        // head=14 lands mid "line two"; tail=14 lands inside the "AAA\n" run.
        let text = "line one\n".to_string()   // positions  0-8  (9 chars)
            + "line two\n"                    // positions  9-17 (9 chars)
            + &"A".repeat(8000)               // positions 18-8017
            + "\nline last\n"; // positions 8018-8028 (11 chars)

        let knobs = NativeKnobs {
            tool_result_head: 14,
            tool_result_tail: 14,
            tool_max_desc_chars: usize::MAX,
            ..Default::default() // min_elide=4000; elided_raw=8029-28=8001 ≥ 4000
        };
        let result = compress_text(&text, &knobs).expect("should compress");

        // Head must end at the last \n within the 14-char window → "line one\n"
        assert!(
            result.starts_with("line one\n"),
            "head should end at \\n boundary, not mid-line; got: {:?}",
            result.get(..20).unwrap_or(&result)
        );
        // Tail must start after the first \n in the tail window → "line last\n"
        assert!(
            result.ends_with("line last\n"),
            "tail should start at line boundary"
        );
        // No partial fragment from the discarded second line
        assert!(
            !result.contains("line tw"),
            "partial 'line two' fragment must not appear in output"
        );
        // Result is shorter than original
        assert!(result.chars().count() < text.chars().count());
    }

    // ── marker: id field and determinism ─────────────────────────────────────

    /// Output contains "elided" and "id " followed by 6 lowercase hex chars;
    /// calling twice on the same input produces identical output.
    #[test]
    fn marker_contains_id_and_is_deterministic() {
        let text = "first line\n".to_string() + &"A".repeat(9000) + "\nlast line\n";
        let knobs = NativeKnobs {
            tool_result_head: 11,
            tool_result_tail: 11,
            tool_max_desc_chars: usize::MAX,
            ..Default::default()
        };
        let r1 = compress_text(&text, &knobs).expect("should compress");
        let r2 = compress_text(&text, &knobs).expect("should compress");

        // Determinism
        assert_eq!(r1, r2, "same input must produce identical output");

        // Marker contains "elided"
        assert!(r1.contains("elided"), "marker must contain 'elided'");

        // Marker contains "id " followed by exactly 6 hex chars
        let id_byte_pos = r1.find("id ").expect("marker must contain 'id '");
        let after = &r1[id_byte_pos + "id ".len()..];
        let hex_part: String = after.chars().take(6).collect();
        assert_eq!(hex_part.len(), 6, "id must be 6 chars");
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "id must be lowercase hex, got: {hex_part:?}"
        );
    }

    /// Two inputs that differ only in their elided middle produce different ids.
    #[test]
    fn different_elided_content_yields_different_id() {
        fn extract_id(s: &str) -> String {
            let pos = s.find("id ").expect("must have 'id ' in marker");
            s[pos + "id ".len()..pos + "id ".len() + 6].to_string()
        }

        let knobs = NativeKnobs {
            tool_result_head: 100,
            tool_result_tail: 100,
            tool_max_desc_chars: usize::MAX,
            ..Default::default()
        };

        let text_a = "head\n".to_string() + &"A".repeat(8000) + "\ntail";
        let text_b = "head\n".to_string() + &"B".repeat(8000) + "\ntail";

        let r_a = compress_text(&text_a, &knobs).expect("A should compress");
        let r_b = compress_text(&text_b, &knobs).expect("B should compress");

        let id_a = extract_id(&r_a);
        let id_b = extract_id(&r_b);
        assert_ne!(
            id_a, id_b,
            "different elided content must yield different id"
        );
    }

    // ── fenced code block protected ───────────────────────────────────────────

    /// A tool_result containing a fenced REAL code block (fn keyword) is left
    /// untouched when fence_requires_code=true (default).
    #[test]
    fn fenced_code_block_protected() {
        // Contains `fn ` keyword → looks_like_code=true → protected.
        let fenced = "Output:\n```rust\nfn foo() { 42 }\n```\n".repeat(100);
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
            ..Default::default() // fence_requires_code=true
        };
        let result = trim_native(body, &knobs);
        let out = result["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            out, fenced,
            "fenced real-code block must be left byte-identical"
        );
    }

    /// A tool_result with a fenced JSON blob (no code keywords) is left
    /// untouched when fence_requires_code=false (legacy mode), but IS
    /// compressed when fence_requires_code=true (new default).
    #[test]
    fn fenced_json_block_legacy_vs_new() {
        let fenced = "Output:\n```json\n{\"key\": \"value\"}\n```\n".repeat(100);
        let build_body = || {
            json!({
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_fence",
                        "content": fenced.clone()
                    }]
                }]
            })
        };

        // Legacy: fence alone protects.
        let legacy_knobs = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            tool_result_fence_requires_code: false,
            ..Default::default()
        };
        let r_legacy = trim_native(build_body(), &legacy_knobs);
        let out_legacy = r_legacy["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            out_legacy, fenced,
            "fence_requires_code=false: JSON fence must be left byte-identical (legacy)"
        );

        // New default: fenced JSON is NOT protected → gets compressed.
        // Use min_elide=0 so any elision triggers the marker, regardless of absolute size.
        let new_knobs = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            tool_result_min_elide: 0,
            tool_result_fence_requires_code: true,
            ..Default::default()
        };
        let r_new = trim_native(build_body(), &new_knobs);
        let out_new = r_new["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            out_new.contains("...[elided"),
            "fence_requires_code=true: fenced JSON blob must be compressed (not protected)"
        );
    }
    // ── strip_thinking tests ──────────────────────────────────────────────────

    /// Prior assistant turns lose their thinking blocks; the last assistant keeps them.
    /// Layout: assistant[thinking, tool_use] / user[tool_result] / assistant[thinking, tool_use]
    ///         / user[tool_result] — last assistant is the SECOND assistant.
    #[test]
    fn strip_thinking_removes_from_old_assistant_keeps_last() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "old thoughts" },
                        { "type": "tool_use", "id": "tu1", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu1", "content": "r1" }]
                },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "latest thoughts" },
                        { "type": "tool_use", "id": "tu2", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu2", "content": "r2" }]
                }
            ]
        });

        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let result = trim_native(body, &knobs);

        // First assistant: thinking removed, tool_use kept.
        let first = &result["messages"][0]["content"];
        let types0: Vec<&str> = first
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(
            types0,
            vec!["tool_use"],
            "first assistant must lose thinking, keep tool_use"
        );

        // Last (second) assistant: content unchanged — both thinking and tool_use present.
        let last = &result["messages"][2]["content"];
        let types2: Vec<&str> = last
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(
            types2,
            vec!["thinking", "tool_use"],
            "last assistant must keep thinking untouched"
        );
        assert_eq!(
            last[0]["thinking"], "latest thoughts",
            "thinking content must be byte-identical"
        );
    }

    /// redacted_thinking in an old assistant is removed; in the last assistant it is preserved.
    #[test]
    fn strip_redacted_thinking_old_removed_last_kept() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "redacted_thinking", "data": "redacted" },
                        { "type": "text", "text": "hello" }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu1", "content": "r" }]
                },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "redacted_thinking", "data": "redacted latest" },
                        { "type": "text", "text": "world" }
                    ]
                }
            ]
        });

        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let result = trim_native(body, &knobs);

        // First assistant: redacted_thinking stripped, text kept.
        let first = &result["messages"][0]["content"];
        let types: Vec<&str> = first
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(types, vec!["text"], "old redacted_thinking must be removed");

        // Last assistant: both blocks preserved.
        let last = &result["messages"][2]["content"];
        let last_types: Vec<&str> = last
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(
            last_types,
            vec!["redacted_thinking", "text"],
            "last assistant must be byte-identical"
        );
        assert_eq!(last[0]["data"], "redacted latest");
    }

    /// When the last message is a user message, the last ASSISTANT (which comes
    /// before it) still keeps its thinking; assistants before it are stripped.
    #[test]
    fn strip_thinking_last_user_message_assistant_before_kept() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "early" },
                        { "type": "tool_use", "id": "tu1", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu1", "content": "r" }]
                },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "middle" },
                        { "type": "tool_use", "id": "tu2", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": "final user message"
                }
            ]
        });

        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let result = trim_native(body, &knobs);

        // First assistant: stripped.
        let first_types: Vec<&str> = result["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(
            first_types,
            vec!["tool_use"],
            "early assistant thinking must be stripped"
        );

        // Second (last) assistant (index 2): thinking kept.
        let second_types: Vec<&str> = result["messages"][2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(
            second_types,
            vec!["thinking", "tool_use"],
            "last assistant must keep thinking"
        );
        assert_eq!(result["messages"][2]["content"][0]["thinking"], "middle");
    }

    /// strip_thinking=false leaves messages byte-identical.
    #[test]
    fn strip_thinking_false_messages_unchanged() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "some thoughts" },
                        { "type": "tool_use", "id": "tu1", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu1", "content": "r" }]
                }
            ]
        });

        let knobs = NativeKnobs {
            strip_thinking: false,
            ..NativeKnobs::default()
        };
        let result = trim_native(body.clone(), &knobs);
        assert_eq!(
            result["messages"], body["messages"],
            "strip_thinking=false must not change messages"
        );
    }

    /// Determinism: running strip_thinking twice produces identical results.
    #[test]
    fn strip_thinking_deterministic() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "t1" },
                        { "type": "redacted_thinking", "data": "rd1" },
                        { "type": "tool_use", "id": "tu1", "name": "bash", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "tu1", "content": "r" }]
                },
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "t2" },
                        { "type": "text", "text": "done" }
                    ]
                }
            ]
        });

        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let a = trim_native(body.clone(), &knobs);
        let b = trim_native(body, &knobs);
        assert_eq!(a, b, "strip_thinking must be deterministic");
    }

    // ── trim_native_openai tests ──────────────────────────────────────────────

    /// (oa) Both the top-level function.description and a nested description
    /// inside function.parameters.properties are truncated.
    #[test]
    fn openai_tool_descriptions_truncated_top_and_nested() {
        let long_top = "T".repeat(300);
        let long_nested = "N".repeat(300);
        let body = json!({
            "model": "gpt-4o",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": long_top,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": long_nested
                            }
                        }
                    }
                }
            }],
            "messages": []
        });

        let knobs = NativeKnobs {
            tool_max_desc_chars: 50,
            ..Default::default()
        };
        let result = trim_native_openai(body, &knobs);

        // Top-level description truncated.
        let top_desc = result["tools"][0]["function"]["description"]
            .as_str()
            .unwrap();
        assert_eq!(
            top_desc.chars().count(),
            51,
            "top-level description must be truncated to 50+ellipsis"
        );
        assert!(top_desc.ends_with('…'));

        // Nested parameter description truncated.
        let nested_desc =
            result["tools"][0]["function"]["parameters"]["properties"]["path"]["description"]
                .as_str()
                .unwrap();
        assert_eq!(
            nested_desc.chars().count(),
            51,
            "nested description must be truncated to 50+ellipsis"
        );
        assert!(nested_desc.ends_with('…'));
    }

    /// (ob) A large plain-text role:tool message is compressed (contains the
    /// elision marker); a small one is left unchanged.
    #[test]
    fn openai_tool_message_large_compressed_small_unchanged() {
        let big_text = "A".repeat(9000);
        let small_text = "X".repeat(50);
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": big_text.clone()
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_2",
                    "content": small_text.clone()
                }
            ]
        });

        let knobs = NativeKnobs {
            tool_result_head: 100,
            tool_result_tail: 50,
            ..Default::default()
        };
        let result = trim_native_openai(body, &knobs);

        // Large message: compressed with elision marker.
        let big_out = result["messages"][0]["content"].as_str().unwrap();
        assert!(
            big_out.chars().count() < big_text.chars().count(),
            "large tool message must be compressed"
        );
        assert!(
            big_out.contains("...[elided"),
            "must contain elision marker"
        );

        // Small message: unchanged.
        let small_out = result["messages"][1]["content"].as_str().unwrap();
        assert_eq!(
            small_out, small_text,
            "small tool message must be unchanged"
        );
    }

    /// (oc) A role:tool message whose content is a fenced REAL code block is
    /// PROTECTED — left byte-identical even with an aggressive budget.
    /// A fenced JSON blob (no code keywords) is protected only when
    /// fence_requires_code=false (legacy mode); with the default (true) it
    /// is compressed.
    #[test]
    fn openai_tool_message_fenced_protected() {
        // Real code: `fn ` keyword → protected with both knob values.
        let fenced_code = "```rust\nfn main() { println!(\"hi\"); }\n```\n".repeat(200);
        let body_code = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "tool",
                "tool_call_id": "call_fence_code",
                "content": fenced_code.clone()
            }]
        });
        let knobs_default = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            ws_enabled: true,
            ..Default::default() // fence_requires_code=true
        };
        let result_code = trim_native_openai(body_code, &knobs_default);
        let out_code = result_code["messages"][0]["content"].as_str().unwrap();
        assert_eq!(
            out_code, fenced_code,
            "fenced real-code tool message must be left byte-identical"
        );

        // JSON fence: with fence_requires_code=false (legacy) → protected.
        let fenced_json = "```json\n{\"key\": \"value\"}\n```\n".repeat(200);
        let body_json = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "tool",
                "tool_call_id": "call_fence_json",
                "content": fenced_json.clone()
            }]
        });
        let knobs_legacy = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            tool_result_fence_requires_code: false,
            ..Default::default()
        };
        let result_legacy = trim_native_openai(body_json.clone(), &knobs_legacy);
        let out_legacy = result_legacy["messages"][0]["content"].as_str().unwrap();
        assert_eq!(
            out_legacy, fenced_json,
            "fence_requires_code=false: fenced JSON must be left byte-identical"
        );

        // JSON fence: with fence_requires_code=true (default) → compressed.
        let result_new = trim_native_openai(body_json, &knobs_default);
        let out_new = result_new["messages"][0]["content"].as_str().unwrap();
        assert!(
            out_new.contains("...[elided"),
            "fence_requires_code=true: fenced JSON must be compressed on OpenAI path"
        );
    }

    /// (od) tool_calls[].function.arguments on an assistant message is NOT modified.
    #[test]
    fn openai_tool_calls_arguments_not_touched() {
        let big_args = "Z".repeat(9000);
        let body = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "do_thing",
                        "arguments": big_args.clone()
                    }
                }]
            }]
        });

        let knobs = NativeKnobs {
            tool_result_head: 10,
            tool_result_tail: 10,
            ..Default::default()
        };
        let result = trim_native_openai(body, &knobs);
        let args = result["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, big_args, "tool_calls arguments must not be touched");
    }

    /// (oe) Determinism: two identical calls produce identical output.
    #[test]
    fn openai_trim_deterministic() {
        let long_desc = "D".repeat(400);
        let big_content = "R".repeat(9000);
        let body = json!({
            "model": "gpt-4o",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "tool",
                    "description": long_desc,
                    "parameters": { "type": "object", "properties": {} }
                }
            }],
            "messages": [
                { "role": "user", "content": "hello" },
                {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": big_content
                }
            ]
        });

        let knobs = NativeKnobs {
            tool_max_desc_chars: 80,
            tool_result_head: 500,
            tool_result_tail: 200,
            ..Default::default()
        };
        let a = trim_native_openai(body.clone(), &knobs);
        let b = trim_native_openai(body, &knobs);
        assert_eq!(a, b, "trim_native_openai must be deterministic");
    }

    // ── fence_requires_code knob tests ────────────────────────────────────────

    /// A fenced JSON/log blob (no code keywords, no line-number shape) is NOT
    /// protected when fence_requires_code=true, but IS protected when false.
    #[test]
    fn fence_requires_code_json_log_not_protected_by_default() {
        // A realistic JSON dump: no `fn`, `def`, `class` etc.; no \d+\t lines.
        let json_blob = concat!(
            "```json\n",
            "{\n",
            "  \"total_keyboard\": 42,\n",
            "  \"session_id\": \"abc123\",\n",
            "  \"timestamp\": \"2026-06-29T10:00:00Z\",\n",
            "  \"events\": [\"key\", \"click\", \"scroll\"]\n",
            "}\n",
            "```\n",
        )
        .repeat(200); // repeat to ensure it's large enough to compress

        // With knob=true (new default): not protected → compression applies.
        assert!(
            !tool_result_protected(&json_blob, None, true, 0.01),
            "fenced JSON blob must NOT be protected when fence_requires_code=true"
        );
        // With knob=false (legacy): fence alone protects.
        assert!(
            tool_result_protected(&json_blob, None, false, 0.01),
            "fenced JSON blob must BE protected when fence_requires_code=false (legacy)"
        );
    }

    /// A fenced real source listing (line-numbered shape) IS still protected
    /// with fence_requires_code=true.
    #[test]
    fn fence_requires_code_line_numbered_source_protected() {
        // Simulate the Read tool output: lines prefixed with "N\t" (number-tab).
        let source = concat!(
            "```rust\n",
            "1\tuse std::collections::HashMap;\n",
            "2\t\n",
            "3\tpub struct Foo {\n",
            "4\t    bar: usize,\n",
            "5\t}\n",
            "6\t\n",
            "7\timpl Foo {\n",
            "8\t    pub fn new() -> Self {\n",
            "9\t        Self { bar: 0 }\n",
            "10\t    }\n",
            "11\t}\n",
            "```\n",
        );
        // Has ≥ 3 lines matching \d+\t → looks_like_code=true → protected.
        assert!(
            tool_result_protected(source, None, true, 0.01),
            "line-numbered source listing must be protected even with fence_requires_code=true"
        );
    }

    /// A fenced source with code keywords (fn/def/class) IS protected with
    /// fence_requires_code=true.
    #[test]
    fn fence_requires_code_keyword_source_protected() {
        let source = "```python\ndef greet(name):\n    class Greeter:\n        pass\n```\n";
        // Has `def ` and `class ` → kw_total ≥ 2 → looks_like_code=true.
        assert!(
            tool_result_protected(source, None, true, 0.01),
            "fenced source with code keywords must be protected when fence_requires_code=true"
        );
    }

    /// A diagram (Layer 3) is still protected regardless of the knob value.
    #[test]
    fn fence_requires_code_diagram_always_protected() {
        // Three → arrows → Layer 3 route (a): arrow_count ≥ 3.
        let diagram = "A → B → C → D (flow)";
        assert!(
            tool_result_protected(diagram, None, true, 0.01),
            "diagram (arrow route) must be protected when fence_requires_code=true"
        );
        assert!(
            tool_result_protected(diagram, None, false, 0.01),
            "diagram (arrow route) must be protected when fence_requires_code=false"
        );
    }

    /// An md-provenance result (Layer 1) is still protected regardless of the
    /// knob value, even with no fences or diagram glyphs.
    #[test]
    fn fence_requires_code_md_provenance_always_protected() {
        let plain_md = "# Heading\n\nJust some plain markdown text with no fences.\n".repeat(20);
        assert!(
            tool_result_protected(&plain_md, Some("md"), true, 0.01),
            "md provenance must be protected when fence_requires_code=true"
        );
        assert!(
            tool_result_protected(&plain_md, Some("md"), false, 0.01),
            "md provenance must be protected when fence_requires_code=false"
        );
    }
}
