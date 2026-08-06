//! Pure trimming for OpenAI Responses request bodies, aligned with the native
//! Claude Code tool-result policy.
//!
//! Only text inside tool outputs may be normalized;
//! calls, ids, reasoning, messages, tools, and backend-owned state remain
//! untouched. Any shape outside that contract returns the original payload.
//!
//! Protection is by *provenance* — the command that produced an output, matched
//! through its `call_id` — plus the same fenced-code and diagram content gates
//! used by the native engine. Unknown/orphan outputs are eligible, just as an
//! unmapped native `tool_result` is; only known protected file reads remain
//! byte-identical.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::content_mix::{ContentClass, classify_block};
use crate::native_trim::{self, NativeKnobs, is_protected_ext, tool_result_protected};

// ── provenance-gated protection (A2) ─────────────────────────────────────────

/// Item-level protection reason labels. Per-block content protection ("content")
/// and the eligible fallback ("none (elided)") are decided per block in the
/// trim loop.
const PROVENANCE_READ: &str = "provenance_read";
const CONTENT: &str = "content";
const ELIGIBLE: &str = "none (elided)";
const STALE_IMAGE_MARKER: &str = "[mhd-trim: previous image omitted]";

/// Read verbs for the A2.2 file-read rule: a command is a protected file read
/// when it contains one of these verbs AND a token whose extension is in the
/// protected set. Search commands (`rg`/`grep`) are intentionally absent:
/// their results remain protected when the content classifier recognizes code,
/// structured data, or another byte-sensitive shape, while plain search text
/// may use the normal safe head/tail path.
/// Hyphens are kept so `Get-Content` reads as one word.
const READ_VERBS: [&str; 11] = [
    "get-content",
    "gc",
    "cat",
    "type",
    "sed",
    "head",
    "tail",
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
    /// Per output block: (protection reason, block char count). Reasons:
    /// `provenance_read` | `content` | `none (elided)`. Populated whenever
    /// the pass inspected the input.
    pub protection_reasons: Vec<(&'static str, usize)>,
    /// Chars saved by lever across this body: `head_tail` | `repeats` |
    /// `whitespace` | `tool_descriptions` | `stale_images`.
    pub saved_by_lever: BTreeMap<&'static str, usize>,
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
            protection_reasons: Vec::new(),
            saved_by_lever: BTreeMap::new(),
        }
    }

    /// Attach per-block protection and per-lever savings to a fail-open outcome
    /// whose loop still ran (e.g. `no_change`) so the replay histogram counts
    /// every inspected block, not just the ones in shrunk bodies.
    fn with_telemetry(
        mut self,
        protection_reasons: Vec<(&'static str, usize)>,
        saved_by_lever: BTreeMap<&'static str, usize>,
    ) -> Self {
        self.protection_reasons = protection_reasons;
        self.saved_by_lever = saved_by_lever;
        self
    }
}

/// Trim recognized tool-result text with the native Claude Code profile.
///
/// The function is fail-open by construction. Backend-owned state
/// (`previous_response_id`) and every non-tool-output item remain untouched,
/// except that already-completed image blocks in older user messages may be
/// removed. Explicit tool-output text in the request is eligible for the same
/// provenance-gated pass. An unknown input item or unknown output block returns
/// the original payload without modification.
pub fn trim_responses(body: Value) -> TrimOutcome {
    trim_responses_with_knobs(body, &codex_default_knobs())
}

/// Live/default Codex profile matching Claude Code's current native settings:
/// 1000 chars from each side, a 4000-char minimum elision, and whitespace ops
/// enabled. The HTTPS/WebSocket handlers replace these values with live state.
pub fn codex_default_knobs() -> NativeKnobs {
    NativeKnobs {
        tool_result_head: 1_000,
        tool_result_tail: 1_000,
        ws_enabled: true,
        ..NativeKnobs::default()
    }
}

/// Apply the Codex Responses trim with an explicit native-style profile.
pub fn trim_responses_with_knobs(body: Value, knobs: &NativeKnobs) -> TrimOutcome {
    let before = match serde_json::to_vec(&body) {
        Ok(bytes) => bytes,
        _ => return TrimOutcome::unchanged(body, "unserializable_body"),
    };

    let Some(object) = body.as_object() else {
        return TrimOutcome::unchanged(body, "unknown_root_shape");
    };
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

    let mut saved_by_lever: BTreeMap<&'static str, usize> = BTreeMap::new();

    // Images are needed for the latest user turn, but repeating every prior
    // base64 image in every subsequent full-history request is pure prompt
    // ballast after that turn has completed. Keep images only in the last
    // user message; older image-only messages receive a tiny marker so the
    // message remains valid and the model can see that the payload was
    // intentionally elided.
    let stale_image_savings = prune_stale_input_images(candidate_input);
    let mut changed = stale_image_savings > 0;
    if stale_image_savings > 0 {
        saved_by_lever.insert("stale_images", stale_image_savings);
    }
    let mut repeated_line_trim_allowed = false;
    let mut head_tail_trim_applied = false;
    let mut tool_output_changed = false;
    let mut classes = Vec::new();
    let mut protection_reasons: Vec<(&'static str, usize)> = Vec::new();
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
                    let normalized = transform_block(
                        text,
                        class,
                        item_reason,
                        knobs,
                        &mut repeated_line_trim_allowed,
                        &mut head_tail_trim_applied,
                        &mut saved_by_lever,
                        &mut protection_reasons,
                    );
                    tool_output_changed |= normalized != text;
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
                        let normalized = transform_block(
                            &text,
                            class,
                            item_reason,
                            knobs,
                            &mut repeated_line_trim_allowed,
                            &mut head_tail_trim_applied,
                            &mut saved_by_lever,
                            &mut protection_reasons,
                        );
                        tool_output_changed |= normalized != text;
                        changed |= normalized != text;
                        block["text"] = Value::String(normalized);
                    }
                }
            }
            // These item types are deliberately protected.
            //
            // `additional_tools` carries Codex's tool descriptions, and truncating
            // them is NOT the safe lever it is on the Anthropic path. The only
            // executable tool is `exec`, whose description is a ~25k-char
            // JavaScript API contract enumerating every callable `tools.*` member
            // of the V8 isolate. Cutting it to a fixed prefix strips that
            // enumeration, the model then invents member names, and every tool
            // call dies with a ReferenceError before reaching the shell. The two
            // other tools seen in the corpus are 350 and 43 chars, so a
            // name-scoped exemption would buy nothing either — the whole lever is
            // the `exec` contract. Leave descriptions byte-identical.
            "additional_tools" | "message" | "reasoning" | "compaction" | "compaction_trigger"
            | "custom_tool_call" | "function_call" => {}
            _ => return TrimOutcome::unchanged(body, "unknown_item_type"),
        }
    }

    if !changed {
        return TrimOutcome::unchanged(body, "no_change")
            .with_telemetry(protection_reasons, saved_by_lever);
    }
    let after = match serde_json::to_vec(&candidate) {
        Ok(bytes) if bytes.len() < before.len() => bytes,
        _ => {
            return TrimOutcome::unchanged(body, "no_gain")
                .with_telemetry(protection_reasons, saved_by_lever);
        }
    };
    let mut stages = Vec::new();
    if tool_output_changed {
        stages.push(StageInfo {
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
        });
    }
    if stale_image_savings > 0 {
        stages.push(StageInfo {
            name: "stale_input_images",
            bytes_before: before.len(),
            bytes_after: after.len(),
        });
    }
    TrimOutcome {
        body: candidate,
        applied: true,
        reason: "applied",
        tokens_before: (before.len() / 4) as u64,
        tokens_after: (after.len() / 4) as u64,
        stages,
        classes,
        protection_reasons,
        saved_by_lever,
    }
}

/// Remove `input_image` blocks from user messages older than the latest user
/// message. A request can contain many snapshots of a conversation, but only
/// the latest user turn can introduce an image that the current request still
/// needs to inspect. The newest user message is therefore kept byte-for-byte.
///
/// Returns the serialized character savings. The caller uses this as telemetry
/// and as the change bit; unknown message/content shapes are left untouched.
fn prune_stale_input_images(items: &mut [Value]) -> usize {
    let last_user_message = items.iter().rposition(|item| {
        item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user")
    });
    let Some(last_user_message) = last_user_message else {
        return 0;
    };

    let mut savings = 0usize;
    for (index, item) in items.iter_mut().enumerate() {
        if index == last_user_message
            || item.get("type").and_then(Value::as_str) != Some("message")
            || item.get("role").and_then(Value::as_str) != Some("user")
        {
            continue;
        }
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let before_bytes = serde_json::to_vec(content).map_or(0, |bytes| bytes.len());
        let before_blocks = content.len();
        content.retain(|block| block.get("type").and_then(Value::as_str) != Some("input_image"));
        if content.len() == before_blocks {
            continue;
        }
        if content.is_empty() {
            content.push(serde_json::json!({
                "type": "input_text",
                "text": STALE_IMAGE_MARKER,
            }));
        }
        let after_bytes = serde_json::to_vec(content).map_or(0, |bytes| bytes.len());
        savings += before_bytes.saturating_sub(after_bytes);
    }
    savings
}

/// Decode and trim one text WebSocket `response.create` message. Any other
/// event, invalid JSON, or unsupported shape returns `None` so the caller can
/// forward the original frame unchanged.
pub fn trim_responses_text(text: &str) -> Option<TrimOutcome> {
    trim_responses_text_with_knobs(text, &codex_default_knobs())
}

/// Decode and trim one `response.create` frame with an explicit profile.
pub fn trim_responses_text_with_knobs(text: &str, knobs: &NativeKnobs) -> Option<TrimOutcome> {
    let body = serde_json::from_str::<Value>(text).ok()?;
    if body.get("type").and_then(Value::as_str) != Some("response.create") {
        return None;
    }
    Some(trim_responses_with_knobs(body, knobs))
}

/// Prepare one client-to-upstream WebSocket text frame.
///
/// The disabled and non-`response.create` paths return the exact original
/// frame. This keeps the transport boundary fail-open and makes the bridge
/// policy independently testable without a live upstream connection.
pub fn trim_responses_text_if_enabled(text: &str, enabled: bool) -> String {
    trim_responses_text_if_enabled_with_knobs(text, enabled, &codex_default_knobs())
}

/// Prepare one client-to-upstream frame with an explicit profile.
pub fn trim_responses_text_if_enabled_with_knobs(
    text: &str,
    enabled: bool,
    knobs: &NativeKnobs,
) -> String {
    if !enabled {
        return text.to_owned();
    }
    let Some(outcome) = trim_responses_text_with_knobs(text, knobs) else {
        return text.to_owned();
    };
    if !outcome.applied {
        return text.to_owned();
    }
    serde_json::to_string(&outcome.body).unwrap_or_else(|_| text.to_owned())
}

/// Trim one tool-output block, recording its protection reason (per block, in
/// chars) and per-lever char savings for the replay report.
///
/// `item_reason` is the item-level provenance protection (`Some("provenance_read")`);
/// per-block content protection (fenced code, diagrams — CC parity layers 2-3)
/// is checked here too. A protected block is returned byte-identical. Otherwise
/// the block is eligible and goes through `transform_tool_text`.
fn transform_block(
    text: &str,
    class: ContentClass,
    item_reason: Option<&'static str>,
    knobs: &NativeKnobs,
    repeated_line_trim_allowed: &mut bool,
    head_tail_trim_applied: &mut bool,
    saved_by_lever: &mut BTreeMap<&'static str, usize>,
    protection_reasons: &mut Vec<(&'static str, usize)>,
) -> String {
    if let Some(reason) = item_reason {
        protection_reasons.push((reason, text.chars().count()));
        return text.to_owned();
    }
    if tool_result_protected(
        text,
        None,
        knobs.tool_result_fence_requires_code,
        knobs.tool_result_arrow_density_min,
    ) {
        protection_reasons.push((CONTENT, text.chars().count()));
        return text.to_owned();
    }
    protection_reasons.push((ELIGIBLE, text.chars().count()));
    transform_tool_text(
        text,
        class,
        knobs,
        repeated_line_trim_allowed,
        head_tail_trim_applied,
        saved_by_lever,
    )
}

/// Transform an eligible tool-output text block.
///
/// Native whitespace operations apply when enabled; repeated-line compression
/// stays gated on the four diagnostic classes; head/tail compression follows
/// the shared native `min_elide` gate. Every lever attributes its char savings
/// so the replay can split the total.
fn transform_tool_text(
    text: &str,
    class: ContentClass,
    knobs: &NativeKnobs,
    repeated_line_trim_allowed: &mut bool,
    head_tail_trim_applied: &mut bool,
    saved_by_lever: &mut BTreeMap<&'static str, usize>,
) -> String {
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
    let diagnostic_class = matches!(
        class,
        ContentClass::Logs
            | ContentClass::BuildDiagnostics
            | ContentClass::StackTrace
            | ContentClass::TestOutput
    );
    let repeated = if diagnostic_class {
        let compressed = compress_repeated_lines(text);
        let repeats_saved = text
            .chars()
            .count()
            .saturating_sub(compressed.chars().count());
        if repeats_saved > 0 {
            *saved_by_lever.entry("repeats").or_default() += repeats_saved;
        }
        compressed
    } else {
        text.to_owned()
    };
    let ws_baseline = if knobs.ws_enabled {
        native_trim::apply_ws_ops(&repeated, knobs)
    } else {
        repeated.clone()
    };
    let whitespace_saved = repeated
        .chars()
        .count()
        .saturating_sub(ws_baseline.chars().count());
    if whitespace_saved > 0 {
        *saved_by_lever.entry("whitespace").or_default() += whitespace_saved;
    }
    let transformed =
        native_trim::transform_text(&repeated, knobs).unwrap_or_else(|| repeated.clone());
    if transformed.contains("...[elided ") {
        *head_tail_trim_applied = true;
        let head_tail_saved = ws_baseline
            .chars()
            .count()
            .saturating_sub(transformed.chars().count());
        if head_tail_saved > 0 {
            *saved_by_lever.entry("head_tail").or_default() += head_tail_saved;
        }
    }
    transformed
}

/// Item-level provenance protection (A2.1-A2.2). Missing or unmatched
/// provenance is intentionally not a protection reason: the native Claude Code
/// path likewise trims an unmapped `tool_result` unless its content gate fires.
fn item_protection<'a>(
    call_id: Option<&'a str>,
    provenance: &HashMap<&'a str, String>,
) -> Option<&'static str> {
    let cid = call_id?;
    let command = provenance.get(cid)?;
    command_protection(command)
}

/// A2.2 on a lowercased command haystack: a protected file read (read verb AND
/// a protected-extension token). Patch output follows the native content gate;
/// it is not a separate byte-stability class in Claude Code.
fn command_protection(command_lower: &str) -> Option<&'static str> {
    if read_verb_seen(command_lower) && protected_ext_path_seen(command_lower) {
        return Some(PROVENANCE_READ);
    }
    None
}

/// Split a command haystack into matching words: alphanumeric runs plus
/// hyphens, so `Get-Content`-style tokens survive and words glued to JS
/// punctuation (`{command:"cat`) still match.
fn command_words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
}

fn read_verb_seen(command_lower: &str) -> bool {
    command_words(command_lower).any(|w| READ_VERBS.iter().any(|v| w.eq_ignore_ascii_case(v)))
}

/// Whether the command haystack references a *path* whose extension is in the
/// protected set: a `.` with an alphanumeric stem before it and an alphanumeric
/// extension run after it (`foo.rs`, `notes.md`, `src/main.py`), followed by a
/// non-alphanumeric boundary.
///
/// This deliberately does NOT match bare words equal to a protected extension.
/// Those fire constantly in the JS wrapper (`C:\` → word `c`, the JS variable
/// `r`, `tools.shell_command`), which would wrongly protect any command that
/// merely mentions a read verb; only a real dotted path counts.
fn protected_ext_path_seen(command_lower: &str) -> bool {
    let b = command_lower.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if b[i] == b'.' && i > 0 && b[i - 1].is_ascii_alphanumeric() {
            let ext_start = i + 1;
            let mut j = ext_start;
            while j < n && b[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let boundary_ok = j >= n || !b[j].is_ascii_alphanumeric();
            if boundary_ok && is_protected_ext(&command_lower[ext_start..j]) {
                return true;
            }
        }
        i += 1;
    }
    false
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
    /// Whether the input tool graph was valid before the trim. An existing
    /// malformed request is reported separately from a trim-induced change.
    pub relationships_valid_before: bool,
    pub relationships_valid_after: bool,
    pub structured_content_preserved: bool,
    /// The ordered `additional_tools[].tools[].name` list (and thus the tool
    /// count) is identical before/after. Description truncation must never drop
    /// or reorder a tool identity.
    pub tool_names_preserved: bool,
}

/// Check invariants that must hold for a transformed Responses body.
pub fn quality_check(before: &Value, after: &Value) -> QualityReport {
    let relationships_valid_before = relationships_valid(before);
    let relationships_valid_after = relationships_valid(after);
    QualityReport {
        relationships_preserved: relationship_keys(before) == relationship_keys(after)
            && relationships_valid_before
            && relationships_valid_after,
        relationships_valid_before,
        relationships_valid_after,
        structured_content_preserved: protected_content_unchanged(before, after),
        tool_names_preserved: tool_names(before) == tool_names(after),
    }
}

/// Validate the Responses tool graph, not just the visible `call_id` fields.
/// Every output must point to exactly one preceding function/custom call and
/// every call id must be unique within the request. This catches malformed
/// fixtures as well as accidental relationship changes during future trims.
fn relationships_valid(body: &Value) -> bool {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return true;
    };
    let mut calls = HashMap::<&str, usize>::new();
    let mut outputs = HashSet::<&str>::new();
    for (index, item) in items.iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "function_call" | "custom_tool_call" => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    return false;
                };
                if calls.insert(call_id, index).is_some() {
                    return false;
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    return false;
                };
                let Some(&call_index) = calls.get(call_id) else {
                    return false;
                };
                if call_index >= index || !outputs.insert(call_id) {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
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

/// Content-based protection (CC parity layers 2-3): fenced code and diagrams
/// stay byte-identical regardless of provenance. The quality gate verifies
/// exactly this set survives trim; known file-read provenance is enforced by
/// the trim path itself.
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
    fn backend_owned_state_remains_byte_stable_while_tool_output_is_trimmed() {
        let body = serde_json::json!({
            "previous_response_id": "resp_123",
            "input": [
                {"type": "function_call", "call_id": "c", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c", "output": large_text()}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(
            out.body["previous_response_id"],
            body["previous_response_id"]
        );
        assert_eq!(out.body["input"][0], body["input"][0]);
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
    }

    #[test]
    fn stale_images_are_removed_but_latest_user_image_is_preserved() {
        let old_image = format!("data:image/png;base64,{}", "A".repeat(20_000));
        let current_image = format!("data:image/png;base64,{}", "B".repeat(20_000));
        let body = serde_json::json!({
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_image", "image_url": old_image},
                        {"type": "input_text", "text": "analyze this image"}
                    ]
                },
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "done"}
                ]},
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "continue"},
                        {"type": "input_image", "image_url": current_image}
                    ]
                }
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert_eq!(out.body["input"][0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(out.body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out.body["input"][2], body["input"][2]);
        assert!(out.saved_by_lever.contains_key("stale_images"));
        assert_eq!(
            out.stages.last().map(|stage| stage.name),
            Some("stale_input_images")
        );
    }

    #[test]
    fn image_only_stale_message_gets_marker_and_current_image_is_unchanged() {
        let current_image = "data:image/png;base64,current";
        let old_image = format!("data:image/png;base64,{}", "O".repeat(10_000));
        let body = serde_json::json!({
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_image", "image_url": old_image}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_image", "image_url": current_image}
                ]}
            ]
        });
        let out = trim_responses(body);
        assert!(out.applied);
        assert_eq!(
            out.body["input"][0]["content"][0]["text"],
            STALE_IMAGE_MARKER
        );
        assert_eq!(
            out.body["input"][1]["content"][0]["image_url"],
            current_image
        );
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
        assert!(out.applied);
        assert!(
            !out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("mhd-trim: omitted")
        );
        assert!(
            out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
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
        assert!(out.applied);
        assert!(
            out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
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
        assert!(out.applied);
        assert!(
            out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
        assert!(quality_check(&body, &out.body).structured_content_preserved);
    }

    #[test]
    fn linked_source_and_json_outputs_are_left_byte_stable() {
        let source = (0..900)
            .map(|i| format!("fn keep_me_{i}() {{ return; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let json = serde_json::to_string(&(0..5000).collect::<Vec<_>>()).unwrap();
        let body = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "src_1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "src_1", "output": source},
                {"type": "function_call", "call_id": "json_1", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "json_1", "output": json}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
        assert!(
            out.body["input"][3]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
        assert!(quality_check(&body, &out.body).structured_content_preserved);
    }

    #[test]
    fn large_output_is_trimmed_even_when_request_is_below_old_global_gate() {
        let output = (0..240)
            .map(|i| format!("error: diagnostic line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "small_req", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "small_req", "output": output}
            ]
        });
        let before = serde_json::to_vec(&body).unwrap().len();
        assert!(
            before < 8 * 1024,
            "fixture must stay below the old request gate"
        );
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(out.tokens_after < out.tokens_before);
        assert_eq!(out.body["input"][1]["call_id"], "small_req");
    }

    #[test]
    fn quality_check_rejects_orphan_and_duplicate_relationships() {
        let orphan = serde_json::json!({
            "input": [{"type": "function_call_output", "call_id": "missing", "output": "x"}]
        });
        assert!(!quality_check(&orphan, &orphan).relationships_preserved);

        let duplicate = serde_json::json!({
            "input": [
                {"type": "function_call", "call_id": "same", "arguments": "{}"},
                {"type": "function_call", "call_id": "same", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "same", "output": "x"}
            ]
        });
        assert!(!quality_check(&duplicate, &duplicate).relationships_preserved);
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
    fn additional_tools_descriptions_are_never_truncated() {
        // Regression guard: the `exec` description is the JavaScript API contract
        // for the V8 isolate. Truncating it made the model invent `tools.*`
        // members and every call died with a ReferenceError. Descriptions must
        // survive byte-identical even when another lever trims the same body.
        let long = "x".repeat(25_000);
        let output = (0..1400)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = serde_json::json!({
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [
                        {"name": "exec", "description": long.clone()},
                        {"name": "wait", "description": long.clone()}
                    ]
                },
                {"type": "custom_tool_call", "call_id": "call_build",
                 "input": "const r = await tools.shell_command({command:\"cargo build 2>&1\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "call_build", "output": output}
            ]
        });
        let out = trim_responses(body.clone());
        // The elision lever still fires on the unprotected tool output ...
        assert!(out.applied);
        // ... while the tool block is returned untouched.
        assert_eq!(out.body["input"][0], body["input"][0]);
        assert_eq!(out.body["input"][0]["tools"][0]["description"], long);
        assert_eq!(out.body["input"][0]["tools"][1]["description"], long);
        assert!(quality_check(&body, &out.body).tool_names_preserved);
    }

    #[test]
    fn additional_tools_alone_is_a_no_op() {
        let body = serde_json::json!({
            "input": [{
                "type": "additional_tools",
                "role": "developer",
                "tools": [{"name": "exec", "description": "x".repeat(25_000)}]
            }]
        });
        let out = trim_responses(body.clone());
        assert!(!out.applied);
        assert_eq!(out.reason, "no_change");
        assert_eq!(out.body, body);
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
    fn orphan_output_uses_native_content_gates() {
        // Missing provenance is not an implicit protection class in Claude
        // Code. The output is eligible unless its content gate fires.
        let body = serde_json::json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "call_ghost",
                "output": large_text()
            }]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(
            out.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );

        let no_call_id = serde_json::json!({
            "input": [{"type": "custom_tool_call_output", "output": large_text()}]
        });
        let out2 = trim_responses(no_call_id.clone());
        assert!(out2.applied);
        assert!(
            out2.body["input"][0]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
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
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
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
        assert!(
            run_out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
    }

    #[test]
    fn read_verb_without_dotted_path_does_not_protect() {
        // A read verb with NO dotted path must not protect: the `c` in `C:\` and
        // the JS variable `r` are not file paths, so `Get-Content` with no file
        // argument is not a protected file read and the output is elided.
        let output = large_text();
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "n",
                 "input": "const r = await tools.shell_command({command:\"Get-Content -Raw\",\"workdir\":\"C:\\\\Users\\\\arthu\\\\dev\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "n", "output": output}
            ]
        });
        let out = trim_responses(body.clone());
        assert!(out.applied);
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
    }

    #[test]
    fn search_provenance_uses_content_gate_for_plain_results() {
        let output = large_text();
        let body = serde_json::json!({
            "input": [
                {"type": "custom_tool_call", "call_id": "search",
                 "input": "const r = await tools.shell_command({command:\"rg -n pattern notes.md\"}); text(r)"},
                {"type": "custom_tool_call_output", "call_id": "search", "output": output}
            ]
        });
        let out = trim_responses(body);
        assert!(
            out.applied,
            "plain search results should use safe compression"
        );
        assert!(
            out.body["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("...[elided ")
        );
    }

    #[test]
    fn rerun_on_trimmed_body_is_noop() {
        // Determinism + idempotence: the second pass must be a byte-stable no-op
        // (the `[mhd-trim: omitted` / `...[elided ` markers guard against nesting).
        // Both outputs are eligible, and the markers make the second pass a
        // genuine byte-stable no-op.
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
