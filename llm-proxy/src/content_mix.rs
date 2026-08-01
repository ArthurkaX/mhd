//! Content-mix probe — a MEASUREMENT-ONLY tool_result classifier. Not a shipped
//! behavior and not billing data.
//!
//! The question this answers: **what is our tool_result traffic actually made
//! of?** Before building content-type-specific compressors (for logs, diffs,
//! JSON, test output, stack traces, ...) we need the byte mix, because a
//! brilliant log compressor is worthless if the corpus has almost no log output.
//!
//! Every `tool_result` block's text is classified by content type and counted
//! by byte share, both over the whole corpus and over the subset large enough
//! for `tool_result_min_elide` (default 4000 chars) to touch. Per class we also
//! report how many bytes the existing `tool_result_protected` gate already
//! protects: a class that is already protected cannot be compressed further
//! without changing that policy, which determines whether a compressor for it
//! is even reachable.
//!
//! # Model and its simplifications (read before trusting the output)
//!
//! - **Bytes = chars of the extracted text.** The unit `tool_result_min_elide`
//!   gates on is chars, so we count chars and call them bytes (identical for
//!   ASCII). The serialized-JSON wrapper bytes are ignored.
//! - **Every appearance counts.** The same block re-appears in every later
//!   request body of a conversation (request bodies are snapshots). We count
//!   each appearance, so persistent blocks dominate — which is exactly the
//!   *transmitted-bytes* view that drives cost.
//! - **Extension is authoritative for `Read` results.** A Read of a file
//!   returns the file, so `file_path`'s extension is the reliable signal (the
//!   task spec says so). Code/config/doc extensions map directly to a class;
//!   a Read of an extension-less file falls through to content heuristics.
//!   Non-Read results (Bash, Grep, Edit confirmations, ...) are always
//!   content-classified.
//! - **Heuristic line-shape detectors, honestly bucketed.** Each detector has
//!   a bar; a block whose detectors all miss lands in Prose ("nothing
//!   fired"), and a block where two detectors fire at comparable strength
//!   lands in Ambiguous. A class that only wins by a weak heuristic is worse
//!   than an honest unknown.
//! - **Protection uses `NativeKnobs::default()`** — `fence_requires_code=true`,
//!   `arrow_density_min=0.01` — the live default policy, applied exactly as the
//!   trim path applies it (combined text for array content, provenance ext).

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::cache_bench::{Chain, load_chains};
use crate::native_trim::{NativeKnobs, build_id_to_tool_info, tool_result_protected};

// ── constants ─────────────────────────────────────────────────────────────────

/// The block-size bar for the "actionable subset": `tool_result_min_elide`'s
/// default. A block at or above this many chars is what trim could possibly
/// touch (modulo protection and the head+tail gate — see the report).
pub const LARGE_BYTES: usize = 4000;

/// How many lines each detector samples. Enough to see structure; bounds the
/// cost on megabyte blocks.
const SAMPLE_LINES: usize = 2000;

/// Two detectors whose confidences are within this margin are a tie: the block
/// is reported as Ambiguous rather than forced into one class.
const CONFLICT_MARGIN: f64 = 0.15;

// ── content classes ───────────────────────────────────────────────────────────

/// The content class of a `tool_result` block's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentClass {
    /// Code by file extension, line-numbered shape, or code keywords.
    SourceCode,
    /// JSON / YAML / TOML / XML (by extension or content parse).
    StructuredData,
    /// Unified diffs / patches (`+`/`-`/`@@` line structure).
    Diff,
    /// Timestamped output and level-marked lines (INFO/WARN/ERROR/...).
    Logs,
    /// Test-runner output (`test ... ok`, PASS/FAILED, summary lines).
    TestOutput,
    /// Stack traces / panics / tracebacks.
    StackTrace,
    /// Compiler / build diagnostics (`error[E...]:`, `warning:`, cargo lines).
    BuildDiagnostics,
    /// Consistently aligned columnar output.
    Tabular,
    /// `ls -l` listings, tree glyphs, glob path lists.
    DirListing,
    /// Nothing fired — the honest catch-all (human prose AND machine output
    /// with no detectable structure).
    Prose,
    /// Two or more detectors fired at comparable strength.
    Ambiguous,
}

impl ContentClass {
    /// Short table-row label.
    pub fn label(self) -> &'static str {
        match self {
            ContentClass::SourceCode => "source code",
            ContentClass::StructuredData => "structured data",
            ContentClass::Diff => "diffs / patches",
            ContentClass::Logs => "logs / timestamped",
            ContentClass::TestOutput => "test-runner output",
            ContentClass::StackTrace => "stack traces / panics",
            ContentClass::BuildDiagnostics => "build diagnostics",
            ContentClass::Tabular => "tabular output",
            ContentClass::DirListing => "dir listings / trees",
            ContentClass::Prose => "prose / other",
            ContentClass::Ambiguous => "ambiguous / unclassified",
        }
    }

    /// The full class set in report order.
    pub const ALL_CLASSES: [ContentClass; 11] = [
        ContentClass::SourceCode,
        ContentClass::StructuredData,
        ContentClass::Diff,
        ContentClass::Logs,
        ContentClass::TestOutput,
        ContentClass::StackTrace,
        ContentClass::BuildDiagnostics,
        ContentClass::Tabular,
        ContentClass::DirListing,
        ContentClass::Prose,
        ContentClass::Ambiguous,
    ];
}

// ── per-class aggregates ──────────────────────────────────────────────────────

/// One of the largest distinct blocks of a class (by content, deduped on a
/// text hash) with a redacted one-line excerpt.
#[derive(Debug, Clone)]
pub struct LargeBlock {
    /// Chars of the block's extracted text.
    pub bytes: usize,
    /// How many request bodies this exact content appeared in.
    pub appearances: usize,
    pub run_id: i64,
    pub seq: i64,
    /// First meaningful line, ~100 chars, secrets redacted.
    pub excerpt: String,
}

/// Aggregated stats for one content class.
#[derive(Debug, Clone)]
pub struct ClassStats {
    pub blocks: usize,
    /// Distinct block contents (by text hash) — how much is repetition vs
    /// variety within the class.
    pub distinct: usize,
    pub bytes: u64,
    pub protected_bytes: u64,
    pub protected_blocks: usize,
    /// Every appearance's byte size, for the median.
    pub sizes: Vec<usize>,
    /// Top-5 largest distinct blocks, sorted largest first.
    pub largest: Vec<LargeBlock>,
}

impl ClassStats {
    /// Median appearance size, or `None` for an empty class.
    pub fn median_size(&self) -> Option<usize> {
        median(&self.sizes)
    }
}

/// The full content-mix report over a corpus.
#[derive(Debug, Clone)]
pub struct ContentMixReport {
    pub n_chains: usize,
    pub n_bodies: usize,
    pub n_blocks: usize,
    pub total_bytes: u64,
    pub n_large_blocks: usize,
    pub large_bytes: u64,
    /// All blocks (every appearance in every request body).
    pub all: HashMap<ContentClass, ClassStats>,
    /// Blocks with `bytes >= LARGE_BYTES` — the subset trim could touch.
    pub large: HashMap<ContentClass, ClassStats>,
}

// ── classification ────────────────────────────────────────────────────────────

/// Classify a tool_result text. `src_ext` is the originating tool_use's file
/// extension; `tool_name` the originating tool ("Read", "Bash", ...).
///
/// Extension is authoritative for `Read` results (the content IS the file). All
/// other blocks are content-classified, with every detector that crosses its
/// bar contributing a confidence; ties (within [`CONFLICT_MARGIN`]) are
/// reported as Ambiguous, and a block with no firing detector is Prose.
pub fn classify_block(text: &str, src_ext: Option<&str>, tool_name: &str) -> ContentClass {
    if tool_name == "Read" {
        if let Some(cls) = ext_to_class(src_ext) {
            return cls;
        }
    }

    let mut hits: Vec<(ContentClass, f64)> = Vec::new();
    for (cls, conf) in [
        (ContentClass::StackTrace, stack_detector(text)),
        (ContentClass::Diff, diff_detector(text)),
        (ContentClass::TestOutput, test_detector(text)),
        (ContentClass::BuildDiagnostics, build_detector(text)),
        (ContentClass::Logs, log_detector(text)),
        (ContentClass::SourceCode, source_detector(text)),
        (ContentClass::StructuredData, structured_detector(text)),
        (
            ContentClass::DirListing,
            dirlisting_detector(text, tool_name),
        ),
        (ContentClass::Tabular, tabular_detector(text)),
    ] {
        if let Some(conf) = conf {
            hits.push((cls, conf));
        }
    }
    if hits.is_empty() {
        return ContentClass::Prose;
    }
    hits.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let (top_cls, top_conf) = hits[0];
    if let Some((_, second_conf)) = hits.get(1)
        && *second_conf > top_conf - CONFLICT_MARGIN
    {
        return ContentClass::Ambiguous;
    }
    top_cls
}

/// Map a file extension to its authoritative class. `None` for unmapped
/// extensions (those fall through to content heuristics).
fn ext_to_class(ext: Option<&str>) -> Option<ContentClass> {
    let e = ext?.to_ascii_lowercase();
    let e = e.as_str();
    if matches!(
        e,
        // source code — the exact Layer-1 code list from `native_trim`
        "py" | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "go"
            | "java"
            | "kt"
            | "scala"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cxx"
            | "cs"
            | "rb"
            | "php"
            | "swift"
            | "m"
            | "mm"
            | "lua"
            | "dart"
            | "ex"
            | "exs"
            | "sh"
            | "bash"
            | "zsh"
            | "ps1"
            | "sql"
            | "r"
            | "jl"
            | "pl"
            | "pm"
            | "css"
            | "scss"
            | "less"
            | "vue"
            | "svelte"
    ) {
        Some(ContentClass::SourceCode)
    } else if matches!(
        e,
        // structured / config — edited exactly too
        "json" | "jsonc" | "yaml" | "yml" | "toml" | "ini" | "xml" | "html" | "htm"
    ) {
        Some(ContentClass::StructuredData)
    } else if matches!(e, "md" | "markdown" | "txt" | "rst" | "adoc" | "org") {
        Some(ContentClass::Prose)
    } else if e == "log" {
        Some(ContentClass::Logs)
    } else if matches!(e, "diff" | "patch") {
        Some(ContentClass::Diff)
    } else if matches!(e, "csv" | "tsv") {
        Some(ContentClass::Tabular)
    } else {
        None
    }
}

/// Extract the combined text of a tool_result block: a string content, or the
/// text blocks of an array content joined by newlines (same combining rule as
/// the trim path's protection check). `None` when there is no text at all.
fn extract_text(b: &serde_json::Map<String, Value>) -> Option<String> {
    match b.get("content") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|inner| {
                    let ib = inner.as_object()?;
                    if ib.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return None;
                    }
                    ib.get("text").and_then(|t| t.as_str()).map(str::to_string)
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

// ── line helpers ──────────────────────────────────────────────────────────────

/// Fraction of non-blank sampled lines matching `pred`.
fn line_frac(text: &str, pred: impl Fn(&str) -> bool) -> f64 {
    let mut matched = 0usize;
    let mut total = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if pred(line) {
            matched += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        matched as f64 / total as f64
    }
}

/// Whether a (trimmed) line begins with a common timestamp shape.
fn timestamp_start(t: &str) -> bool {
    let b = t.as_bytes();
    let n = b.len();
    // YYYY-MM-DD (also matches YYYY-MM-DDTHH:MM and YYYY-MM-DD HH:MM).
    if n >= 10 && b[4] == b'-' && b[7] == b'-' && b[8].is_ascii_digit() && b[9].is_ascii_digit() {
        return true;
    }
    // [HH:MM:SS ...]
    if n >= 9 && b[0] == b'[' && b[4] == b':' && b[7] == b':' {
        return true;
    }
    // [YYYY-MM-DD ...]
    if n >= 10 && b[0] == b'[' && b[5] == b'-' && b[8] == b'-' {
        return true;
    }
    // HH:MM:SS
    if n >= 8
        && b[2] == b':'
        && b[5] == b':'
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7].is_ascii_digit()
    {
        return true;
    }
    // H:MM:SS
    if n >= 7
        && b[1] == b':'
        && b[4] == b':'
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
    {
        return true;
    }
    // MM/DD/YYYY or DD/MM/YYYY
    let mut digits = 0u32;
    let mut slashes = 0u32;
    for &c in b.iter().take(12) {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c == b'/' && digits > 0 {
            slashes += 1;
        } else {
            break;
        }
    }
    if slashes >= 2 && digits >= 6 {
        return true;
    }
    // "Jan 12 10:00:00" style syslog.
    if n >= 4
        && b[0].is_ascii_alphabetic()
        && b[1].is_ascii_alphabetic()
        && b[2].is_ascii_alphabetic()
    {
        let m = &t[..3];
        if matches!(
            m,
            "Jan"
                | "Feb"
                | "Mar"
                | "Apr"
                | "May"
                | "Jun"
                | "Jul"
                | "Aug"
                | "Sep"
                | "Oct"
                | "Nov"
                | "Dec"
        ) && (b[3] == b' ' || b[3] == b'-')
        {
            return true;
        }
    }
    false
}

/// Whether a line contains a standalone uppercase log-level token (INFO, WARN,
/// ERROR, ...) in its first 60 chars. Case-sensitive on purpose: lowercase
/// `error:` is compiler output, not a level marker.
fn level_marker(t: &str) -> bool {
    let head: String = t.chars().take(60).collect();
    head.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(
            w,
            "INFO" | "WARN" | "WARNING" | "ERROR" | "DEBUG" | "TRACE" | "FATAL" | "SEVERE"
        )
    })
}

/// Whether a trimmed line is an `ls -l` entry (permissions + columns).
fn is_ls_l_line(t: &str) -> bool {
    let b = t.as_bytes();
    if b.len() < 11 {
        return false;
    }
    if !matches!(b[0], b'-' | b'd' | b'l' | b'c' | b'b' | b's' | b'p') {
        return false;
    }
    if !b[1..10].iter().all(|c| {
        matches!(
            c,
            b'r' | b'w' | b'x' | b'-' | b's' | b'S' | b't' | b'T' | b'd' | b'l'
        )
    }) {
        return false;
    }
    b[10] == b' ' || b[10] == b'\t'
}

/// Whether a line contains file-tree glyphs.
fn is_tree_line(t: &str) -> bool {
    t.contains("├──")
        || t.contains("└──")
        || t.contains("│")
        || t.contains("|--")
        || t.contains("`--")
}

// ── detectors ─────────────────────────────────────────────────────────────────

/// Content-based code: line-numbered Read shape, `N:content` excerpts, or code
/// keywords.
fn source_detector(text: &str) -> Option<f64> {
    // Each route contributes a confidence; the highest that fires wins, so a
    // block that is both keyword-code AND grep-excerpt reports the stronger
    // signal (0.8) rather than the weaker early-return (0.6).
    let mut best: Option<f64> = None;
    let track = |c: f64, best: &mut Option<f64>| {
        if best.map(|b| c > b).unwrap_or(true) {
            *best = Some(c);
        }
    };

    // Route 1: `^\s*\d+\t` — the Read tool's output format for source files.
    let mut ln = 0usize;
    for line in text.lines().take(40) {
        let rest = line.trim_start_matches([' ', '\t']);
        let mut digits = 0usize;
        for c in rest.chars() {
            if c.is_ascii_digit() {
                digits += 1;
            } else {
                break;
            }
        }
        if digits >= 1
            && rest
                .as_bytes()
                .get(digits)
                .map(|&b| b == b'\t')
                .unwrap_or(false)
        {
            ln += 1;
        }
    }
    if ln >= 3 {
        track(0.75, &mut best);
    }

    // Route 2: `N:content` excerpts (file views with line prefixes).
    let colon_frac = line_frac(text, |line| {
        let t = line.trim_start();
        let b = t.as_bytes();
        let mut i = 0usize;
        while i < b.len() && i < 5 && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || i >= b.len() || b[i] != b':' {
            return false;
        }
        i + 1 < b.len() && !b[i + 1].is_ascii_digit()
    });
    if colon_frac >= 0.3 {
        track(0.65, &mut best);
    }

    // Route 3: code keywords.
    const KEYWORDS: [&str; 11] = [
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
        "enum ",
    ];
    let mut kw = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim_start();
        if KEYWORDS.iter().any(|k| t.contains(k)) {
            kw += 1;
        }
        if kw >= 6 {
            break;
        }
    }
    if kw >= 6 {
        track(0.6, &mut best);
    }

    // Route 4: grep / ripgrep / sed code excerpts — `path:line: content`,
    // `path-14- content`, `path:14- content`. The path is the reliable part
    // (a `.`, `/` or `\` in the first 40 chars), which keeps `12:34:56`
    // timestamps and plain prose from matching.
    let grep_frac = line_frac(text, |line| {
        let t = line.trim_start();
        let head: String = t.chars().take(80).collect();
        let b = head.as_bytes();
        // Path anchor before the line-number marker.
        if !b[..b.len().min(40)]
            .iter()
            .any(|c| *c == b'.' || *c == b'/' || *c == b'\\')
        {
            return false;
        }
        // A digit run followed by `:` or `-` (the line-number marker).
        let mut i = 0usize;
        while i < b.len() {
            if b[i].is_ascii_digit() {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j < b.len()
                    && (b[j] == b':' || b[j] == b'-')
                    && (j + 1 >= b.len() || !b[j + 1].is_ascii_digit())
                {
                    return true;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        false
    });
    if grep_frac >= 0.5 {
        track(0.8, &mut best);
    }

    best
}

/// Unified diffs / patches: hunk markers, diff headers, or a strong `+`/`-`
/// structure that is not a markdown bullet list.
fn diff_detector(text: &str) -> Option<f64> {
    let mut add = 0usize;
    let mut del = 0usize;
    let mut hunk = 0usize;
    let mut bullet = 0usize;
    let mut header = false;
    let mut total = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        total += 1;
        if t.starts_with("@@") && t[1..].contains("@@") {
            hunk += 1;
        } else if t.starts_with("+++") || t.starts_with("---") {
            header = true;
        } else if t.starts_with('+') {
            add += 1;
        } else if t.starts_with('-') {
            del += 1;
            if t.starts_with("- ") {
                bullet += 1;
            }
        } else if t.starts_with("diff --git") || t.starts_with("diff -u") || t.starts_with("Index:")
        {
            header = true;
        }
    }
    if total == 0 {
        return None;
    }
    let both = add > 0 && del > 0;
    if hunk > 0 && both {
        return Some(0.85);
    }
    if header && both && (add + del) as f64 / total as f64 >= 0.25 {
        return Some(0.8);
    }
    if both && (add + del) as f64 / total as f64 >= 0.5 && (bullet as f64 / del as f64) < 0.5 {
        return Some(0.6);
    }
    None
}

/// Stack traces / panics / tracebacks: a strong signature (panic, Traceback,
/// backtrace, goroutine dump) plus frame evidence.
fn stack_detector(text: &str) -> Option<f64> {
    let mut strong = false;
    let mut frames = 0usize;
    let mut total = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        total += 1;
        let l = t.to_ascii_lowercase();
        if l.contains("panicked at")
            || l.starts_with("traceback (most recent call last)")
            || l.starts_with("stack backtrace:")
            || l.starts_with("backtrace:")
            || l.starts_with("goroutine ")
            || l.starts_with("panic: ")
            || l.contains(" in <module>")
            || (l.contains("thread '") && l.contains("panicked"))
        {
            strong = true;
        }
        // Frame lines: `at ...` (Java/Node), `File "..."` (Python), Go
        // runtime/hex frames, and `N:` numbered frames (Rust backtrace).
        let mut num_i = 0usize;
        while num_i < t.len() && num_i < 6 && t.as_bytes()[num_i].is_ascii_digit() {
            num_i += 1;
        }
        let numbered_frame = num_i > 0 && num_i < t.len() && t.as_bytes()[num_i] == b':';
        if t.starts_with("at ")
            || t.starts_with("file \"")
            || l.contains("runtime/panic")
            || l.contains("+0x")
            || numbered_frame
        {
            frames += 1;
        }
    }
    if !strong {
        return None;
    }
    let frame_frac = frames as f64 / total.max(1) as f64;
    if frames >= 3 || frame_frac >= 0.2 {
        Some(0.85)
    } else {
        Some(0.7)
    }
}

/// Test-runner output: rust `test ... ok`, go `--- PASS/FAIL`, pytest
/// separators/summaries, TAP, unittest lines.
fn test_detector(text: &str) -> Option<f64> {
    let mut hits = 0usize;
    let mut strong = 0usize;
    let mut total = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        total += 1;
        let l = t.to_ascii_lowercase();
        let hit = l.starts_with("test result:")
            || (l.starts_with("running ") && l.contains(" test"))
            || l.starts_with("====")
            || l.starts_with("----")
            || l.starts_with("--- pass")
            || l.starts_with("--- fail")
            || l.starts_with("--- skip")
            || l.starts_with("ok ")
            || l.starts_with("not ok ")
            || l.starts_with("failures:")
            || l.contains(" passed; ")
            || l.contains(" passed in ")
            || l.contains(" failed in ")
            || l.contains(" tests passed")
            || l.contains(" tests failed")
            || l.starts_with("passed ")
            || l.starts_with("failed ")
            || l.starts_with("tests run:")
            || l.starts_with("test run ")
            || (l.starts_with("test ") && l.contains(" ... "))
            || l == "pass"
            || l == "fail"
            || l == "ok"
            || l.starts_with("pytest")
            || l.contains("assertionerror")
            || (l.starts_with("ran ") && l.contains(" test"));
        if hit {
            hits += 1;
        }
        if l.starts_with("test result:")
            || l.contains(" passed; ")
            || l.contains(" passed in ")
            || l.starts_with("tests run:")
            || l.starts_with("====")
            || l.starts_with("----")
        {
            strong += 1;
        }
    }
    if total == 0 {
        return None;
    }
    let frac = hits as f64 / total as f64;
    if strong > 0 && frac >= 0.15 {
        Some(0.8)
    } else if frac >= 0.4 {
        Some(0.5 + frac * 0.3)
    } else {
        None
    }
}

/// Compiler / build diagnostics: `error[E...]:`, `warning:`, `-->`, cargo
/// progress lines.
fn build_detector(text: &str) -> Option<f64> {
    let mut hits = 0usize;
    let mut total = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        total += 1;
        let l = t.to_ascii_lowercase();
        if l.starts_with("error[")
            || l.starts_with("error:")
            || l.starts_with("warning[")
            || l.starts_with("warning:")
            || l.starts_with("note:")
            || l.starts_with("help:")
            || l.starts_with("-->")
            || l.starts_with("error ")
            || l.starts_with("warning ")
            || l.starts_with("compiling ")
            || l.starts_with("checking ")
            || l.starts_with("finished ")
            || l.starts_with("building ")
            || l.starts_with("doc-tests")
            || l.starts_with("downloading ")
            || l.starts_with("updating ")
            || l.starts_with("locking ")
            || l.starts_with("build failed")
            || l.starts_with("build succeeded")
            || l.contains("error[")
        {
            hits += 1;
        }
    }
    if total == 0 {
        return None;
    }
    let frac = hits as f64 / total as f64;
    if frac >= 0.3 {
        Some(0.6 + frac * 0.3)
    } else {
        None
    }
}

/// Timestamped / level-marked logs.
fn log_detector(text: &str) -> Option<f64> {
    let frac = line_frac(text, |line| {
        let t = line.trim_start();
        !t.is_empty() && (timestamp_start(t) || level_marker(t))
    });
    if frac >= 0.5 { Some(frac) } else { None }
}

/// JSON / NDJSON / XML / TOML / YAML.
fn structured_detector(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let first = trimmed.chars().next();
    // Complete JSON parse — the strongest structured signal.
    if matches!(first, Some('{') | Some('[')) && serde_json::from_str::<Value>(trimmed).is_ok() {
        return Some(1.0);
    }
    // NDJSON: most non-blank lines are JSON values.
    if matches!(first, Some('{') | Some('[')) {
        let mut json_lines = 0usize;
        let mut total = 0usize;
        for line in text.lines().take(SAMPLE_LINES) {
            let t = line.trim_start();
            if t.is_empty() {
                continue;
            }
            total += 1;
            if t.starts_with('{') || t.starts_with('[') {
                json_lines += 1;
            }
        }
        if total > 0 && json_lines as f64 / total as f64 >= 0.8 && json_lines >= 3 {
            return Some(0.8);
        }
    }
    // XML: opener tags at line starts.
    if first == Some('<') {
        let open = line_frac(text, |l| l.trim_start().starts_with('<'));
        if open >= 0.5 {
            return Some(0.7);
        }
    }
    // TOML: [section] headers and `key = value`.
    let toml = line_frac(text, |l| {
        let t = l.trim_start();
        if t.starts_with('[') && t.trim_end().ends_with(']') {
            return true;
        }
        if let Some(eq) = t.find('=') {
            if eq == 0 || eq > 60 {
                return false;
            }
            let key = &t[..eq];
            let key = key.trim_end();
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
        } else {
            false
        }
    });
    if toml >= 0.4 {
        return Some(0.55);
    }
    // YAML: `key: value` (modest confidence — prose "Note:" lines also match).
    let yaml = line_frac(text, |l| {
        let t = l.trim_start();
        if t.is_empty()
            || t.starts_with('{')
            || t.starts_with('[')
            || t.starts_with('"')
            || timestamp_start(t)
        {
            return false;
        }
        if let Some(ci) = t.find(':') {
            if ci == 0 || ci > 80 {
                return false;
            }
            let key = &t[..ci];
            if !key
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\'' | ' '))
            {
                return false;
            }
            let rest = t[ci + 1..].trim_start();
            !rest.starts_with('{') && !rest.starts_with('[')
        } else {
            false
        }
    });
    if yaml >= 0.5 { Some(0.5) } else { None }
}

/// Directory listings / file trees: `ls -l`, tree glyphs, `total N`, or a Glob
/// path list.
fn dirlisting_detector(text: &str, tool_name: &str) -> Option<f64> {
    let tree = line_frac(text, |l| is_tree_line(l));
    if tree >= 0.3 {
        return Some(0.75);
    }
    let ls = line_frac(text, |l| is_ls_l_line(l.trim_start()));
    if ls >= 0.3 {
        return Some(0.7);
    }
    let first = text.lines().next().unwrap_or("");
    let ft = first.trim_start();
    if ft.starts_with("total ")
        && ft[6..]
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        return Some(0.7);
    }
    if tool_name == "Glob" {
        let pathy = line_frac(text, |l| {
            let t = l.trim();
            t.contains('/') || t.contains('\\') || t.contains('.')
        });
        if pathy >= 0.5 {
            return Some(0.6);
        }
    }
    None
}

/// Consistent, ALIGNED columnar output (whitespace columns or pipe-delimited
/// rows), excluding dir-listing lines. Alignment means a run of >= 2 spaces:
/// real tables pad columns, while `git log --oneline`, `key = value` configs
/// and plain prose use single spaces and are therefore not tables.
fn tabular_detector(text: &str) -> Option<f64> {
    let mut aligned_counts: HashMap<usize, usize> = HashMap::new();
    let mut aligned = 0usize;
    let mut total = 0usize;
    let mut pipe_rows = 0usize;
    for line in text.lines().take(SAMPLE_LINES) {
        let t = line.trim();
        if t.is_empty() || is_ls_l_line(t) || is_tree_line(t) {
            continue;
        }
        total += 1;
        let nsp = t.split_whitespace().count();
        if t.contains("  ") {
            // Column-aligned row (a run of 2+ spaces between columns).
            aligned += 1;
            *aligned_counts.entry(nsp).or_default() += 1;
        }
        if t.matches('|').count() >= 2 && nsp <= 2 {
            pipe_rows += 1;
        }
    }
    if total == 0 {
        return None;
    }
    let mut mode_fields = 0usize;
    let mut mode_count = 0usize;
    for (fields, cnt) in &aligned_counts {
        if *fields >= 3 && *cnt > mode_count {
            mode_fields = *fields;
            mode_count = *cnt;
        }
    }
    let aligned_frac = aligned as f64 / total as f64;
    let mode_frac = mode_count as f64 / aligned.max(1) as f64;
    if aligned_frac >= 0.5 && mode_fields >= 3 && mode_frac >= 0.5 {
        return Some(0.6);
    }
    if pipe_rows > 0 && pipe_rows as f64 / total as f64 >= 0.5 {
        return Some(0.55);
    }
    None
}

// ── excerpts and redaction ────────────────────────────────────────────────────

/// First meaningful line of the block, ~100 chars, secrets redacted.
fn make_excerpt(text: &str) -> String {
    let mut chosen = "";
    for line in text.lines() {
        if line.trim().len() >= 12 {
            chosen = line;
            break;
        }
    }
    if chosen.is_empty() {
        chosen = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    }
    let mut s: String = chosen.chars().take(100).collect();
    if chosen.chars().count() > 100 {
        s.push('…');
    }
    redact_secrets(&s)
}

/// Replace secret-looking runs (Anthropic/OpenAI keys, Bearer tokens, long hex
/// or base64 strings) with `[REDACTED]`. Prose and short identifiers pass
/// through untouched.
fn redact_secrets(s: &str) -> String {
    let b = s.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < n {
        if b[i..].starts_with(b"sk-ant-") {
            let tok = token_len(&b[i + 7..]);
            if tok >= 8 {
                out.push_str("[REDACTED]");
                i += 7 + tok;
                continue;
            }
        }
        if b[i..].starts_with(b"sk-") {
            let tok = token_len(&b[i + 3..]);
            if tok >= 16 {
                out.push_str("[REDACTED]");
                i += 3 + tok;
                continue;
            }
        }
        if b[i..].starts_with(b"Bearer ") {
            let tok = token_len(&b[i + 7..]);
            if tok >= 8 {
                out.push_str("Bearer [REDACTED]");
                i += 7 + tok;
                continue;
            }
        }
        if is_hex(b[i]) {
            let mut j = i;
            while j < n && is_hex(b[j]) {
                j += 1;
            }
            let run = &b[i..j];
            let digits = run.iter().filter(|c| c.is_ascii_digit()).count();
            if run.len() >= 20 && digits >= 3 {
                out.push_str("[REDACTED]");
                i = j;
                continue;
            }
        }
        if is_b64(b[i]) {
            let mut j = i;
            while j < n && is_b64(b[j]) {
                j += 1;
            }
            // Extend over trailing '=' padding (real base64) — but never let a
            // mid-word '=' (like `hash=...`) join the run: the core scan above
            // stops at '=', so only trailing padding is absorbed.
            let mut k = j;
            while k < n && b[k] == b'=' {
                k += 1;
            }
            let run = &b[i..k];
            let non_alpha = run.iter().filter(|c| !c.is_ascii_alphabetic()).count();
            if run.len() >= 28 && non_alpha >= 4 {
                out.push_str("[REDACTED]");
                i = k;
                continue;
            }
        }
        let len = utf8_len(b[i]);
        out.push_str(&s[i..i + len]);
        i += len;
    }
    out
}

fn token_len(rest: &[u8]) -> usize {
    rest.iter()
        .take_while(|c| {
            c.is_ascii_alphanumeric()
                || **c == b'_'
                || **c == b'-'
                || **c == b'='
                || **c == b'+'
                || **c == b'/'
        })
        .count()
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || (b'A'..=b'F').contains(&c)
}

fn is_b64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/'
}

/// Byte length of the UTF-8 char whose first byte is `b`.
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Content-addressable hash over a block's text (for distinct-block counting).
fn hash_text(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Median of a sample, or `None` for an empty one.
pub fn median(sizes: &[usize]) -> Option<usize> {
    if sizes.is_empty() {
        return None;
    }
    let mut v = sizes.to_vec();
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    })
}

// ── body analysis ─────────────────────────────────────────────────────────────

/// Classify every tool_result block in one request body. Returns
/// `(class, bytes, protected, text)` per block, in corpus order.
fn analyze_body(body: &Value, knobs: &NativeKnobs) -> Vec<(ContentClass, usize, bool, String)> {
    let messages: &[Value] = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let id_to_info = build_id_to_tool_info(messages);
    let mut out = Vec::new();
    for msg in messages {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(text) = extract_text(b) else {
                continue;
            };
            let bytes = text.chars().count();
            if bytes == 0 {
                continue;
            }
            let tool_use_id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
            let info = id_to_info.get(tool_use_id);
            let name = info.map(|i| i.name.as_str()).unwrap_or("");
            let ext: Option<&str> = info.and_then(|i| i.ext.as_deref());
            let class = classify_block(&text, ext, name);
            let protected = tool_result_protected(
                &text,
                ext,
                knobs.tool_result_fence_requires_code,
                knobs.tool_result_arrow_density_min,
            );
            out.push((class, bytes, protected, text));
        }
    }
    out
}

// ── corpus entry point ────────────────────────────────────────────────────────

/// Run the content-mix measurement over every request body in a proxy.db.
/// Read-only. Returns `Ok(None)` when the corpus has no usable anthropic rows,
/// mirroring `cache_bench::run_cache_bench`.
pub fn run_content_mix(db_path: &Path) -> Result<Option<ContentMixReport>, String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;

    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_bodies'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !exists {
        return Ok(None);
    }

    let chains: Vec<Chain> = load_chains(&conn)?;
    if chains.is_empty() {
        return Ok(None);
    }

    let knobs = NativeKnobs::default();
    let mut all: HashMap<ContentClass, Accum> = HashMap::new();
    let mut large: HashMap<ContentClass, Accum> = HashMap::new();
    let mut n_bodies = 0usize;

    for chain in &chains {
        for req in &chain.requests {
            n_bodies += 1;
            for (class, bytes, protected, text) in analyze_body(&req.body, &knobs) {
                all.entry(class)
                    .or_default()
                    .observe(bytes, protected, req.run_id, req.seq, &text);
                if bytes >= LARGE_BYTES {
                    large
                        .entry(class)
                        .or_default()
                        .observe(bytes, protected, req.run_id, req.seq, &text);
                }
            }
        }
    }

    let all: HashMap<ContentClass, ClassStats> =
        all.into_iter().map(|(k, v)| (k, v.finalize())).collect();
    let large: HashMap<ContentClass, ClassStats> =
        large.into_iter().map(|(k, v)| (k, v.finalize())).collect();

    Ok(Some(ContentMixReport {
        n_chains: chains.len(),
        n_bodies,
        n_blocks: all.values().map(|s| s.blocks).sum(),
        total_bytes: all.values().map(|s| s.bytes).sum(),
        n_large_blocks: large.values().map(|s| s.blocks).sum(),
        large_bytes: large.values().map(|s| s.bytes).sum(),
        all,
        large,
    }))
}

/// Internal accumulation for one class.
#[derive(Debug, Default)]
struct Accum {
    blocks: usize,
    bytes: u64,
    protected_bytes: u64,
    protected_blocks: usize,
    sizes: Vec<usize>,
    distinct: HashMap<u64, LargeBlock>,
}

impl Accum {
    fn observe(&mut self, bytes: usize, protected: bool, run_id: i64, seq: i64, text: &str) {
        self.blocks += 1;
        self.bytes += bytes as u64;
        if protected {
            self.protected_blocks += 1;
            self.protected_bytes += bytes as u64;
        }
        self.sizes.push(bytes);
        let h = hash_text(text);
        let entry = self.distinct.entry(h).or_insert_with(|| LargeBlock {
            bytes,
            appearances: 0,
            run_id,
            seq,
            excerpt: make_excerpt(text),
        });
        if bytes > entry.bytes {
            entry.bytes = bytes;
            entry.run_id = run_id;
            entry.seq = seq;
            entry.excerpt = make_excerpt(text);
        }
        entry.appearances += 1;
    }

    fn finalize(self) -> ClassStats {
        let distinct = self.distinct.len();
        let mut largest: Vec<LargeBlock> = self.distinct.into_values().collect();
        largest.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then(b.appearances.cmp(&a.appearances))
        });
        largest.truncate(5);
        ClassStats {
            blocks: self.blocks,
            distinct,
            bytes: self.bytes,
            protected_bytes: self.protected_bytes,
            protected_blocks: self.protected_blocks,
            sizes: self.sizes,
            largest,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── classifier fixtures ─────────────────────────────────────────────────

    const RS_CODE: &str = "     1\tfn main() {\n     2\t    println!(\"hi\");\n     3\t}\n";
    const JSON_TEXT: &str = "{\n  \"name\": \"x\",\n  \"count\": 3\n}";
    const MD_TEXT: &str = "# Title\n\nSome *markdown* text.\n";
    const LOG_TEXT: &str = "2026-07-30 10:00:00 INFO  service starting\n2026-07-30 10:00:01 WARN  retrying\n2026-07-30 10:00:02 ERROR failed\n";
    const DIFF_TEXT: &str = "diff --git a/f.rs b/f.rs\nindex 123..456 100644\n--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,4 @@\n fn a() {\n-    old();\n+    new();\n }\n";
    const TEST_TEXT: &str = "running 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... FAILED\ntest result: FAILED. 2 passed; 1 failed\n";
    const TRACEBACK: &str = "Traceback (most recent call last):\n  File \"main.py\", line 3, in <module>\n    foo()\nZeroDivisionError: division by zero\n";
    const PANIC: &str = "thread 'main' panicked at src/main.rs:10:5:\nindex out of bounds\nstack backtrace:\n   0: std::panicking::begin_panic\n   1: hello::main\n";
    const CARGO_ERR: &str = "error[E0308]: mismatched types\n --> src/main.rs:10:5\n  |\n10 |     let x: u32 = \"hi\";\n  |              ^ expected u32, found &str\nhelp: try using a different type\n";
    const LS_LISTING: &str = "total 244\ndrwxr-xr-x 1 user user    0 Jul 30 10:00 src\n-rw-r--r-- 1 user user 1234 Jul 30 10:00 Cargo.toml\n-rw-r--r-- 1 user user  567 Jul 30 10:00 README.md\n";
    const TREE: &str = "src\n├── main.rs\n├── lib.rs\n└── tests\n    └── test.rs\n";
    const PS_TABLE: &str = "USER       PID  %CPU %MEM    VSZ   RSS TTY      STAT START   TIME COMMAND\narthu      123  0.0  0.1  12345  6789 pts/0    S    10:00   0:00 bash\nroot        1  0.0  0.0  12345  6789 ?        Ss   09:00   0:01 systemd\n";
    const PROSE: &str = "This is a plain paragraph of human text. It does not contain any\nstructural markers, timestamps, or code. Just words on lines.\nAnother sentence here for good measure.\n";

    fn cls(text: &str, ext: Option<&str>, tool: &str) -> ContentClass {
        classify_block(text, ext, tool)
    }

    // ── extension authority (Read) ──────────────────────────────────────────

    #[test]
    fn read_code_file_by_extension() {
        assert_eq!(cls(RS_CODE, Some("rs"), "Read"), ContentClass::SourceCode);
        assert_eq!(cls(RS_CODE, Some("py"), "Read"), ContentClass::SourceCode);
    }

    #[test]
    fn read_structured_file_by_extension() {
        assert_eq!(
            cls(JSON_TEXT, Some("json"), "Read"),
            ContentClass::StructuredData
        );
        assert_eq!(
            cls("a: 1\nb: 2\n", Some("yaml"), "Read"),
            ContentClass::StructuredData
        );
    }

    #[test]
    fn read_doc_file_by_extension_is_prose() {
        assert_eq!(cls(MD_TEXT, Some("md"), "Read"), ContentClass::Prose);
        assert_eq!(cls("plain", Some("txt"), "Read"), ContentClass::Prose);
    }

    #[test]
    fn read_log_and_diff_files() {
        assert_eq!(cls(LOG_TEXT, Some("log"), "Read"), ContentClass::Logs);
        assert_eq!(cls(DIFF_TEXT, Some("diff"), "Read"), ContentClass::Diff);
    }

    #[test]
    fn extension_wins_even_when_content_says_otherwise() {
        // Extension is the reliable signal for a Read: the content IS the file.
        assert_eq!(
            cls("hello world", Some("rs"), "Read"),
            ContentClass::SourceCode
        );
        assert_eq!(
            cls("hello world", Some("json"), "Read"),
            ContentClass::StructuredData
        );
    }

    #[test]
    fn unmapped_extension_falls_through_to_content() {
        assert_eq!(cls(RS_CODE, None, "Read"), ContentClass::SourceCode);
    }

    // ── content detectors ───────────────────────────────────────────────────

    #[test]
    fn bash_code_snippet_detected_by_line_numbers() {
        assert_eq!(cls(RS_CODE, None, "Bash"), ContentClass::SourceCode);
    }

    #[test]
    fn grep_output_detected_as_source_code() {
        let grep = "src/main.rs:14:    let x = 1;\nsrc/lib.rs:92:    pub fn foo() {}\nsrc/util.rs:7:    return true;\n";
        assert_eq!(cls(grep, None, "Grep"), ContentClass::SourceCode);
        assert_eq!(cls(grep, None, "Bash"), ContentClass::SourceCode);
    }

    #[test]
    fn sed_excerpt_output_detected_as_source_code() {
        let sed = "src/ide_bridge/ide_online_helpers.py-605-            except Exception as e:\nsrc/ide_bridge/ide_online_helpers.py-606-                raise\nsrc/ide_bridge/ide_online_helpers.py-607-            pass\n";
        assert_eq!(cls(sed, None, "Bash"), ContentClass::SourceCode);
    }

    #[test]
    fn timestamps_are_not_grep_paths() {
        // `12:34:56` style times must not trip the file:line grep detector.
        let t = "12:34:56 one\ntime 23:45:67 two\nno colon here three\n";
        assert_ne!(cls(t, None, "Bash"), ContentClass::SourceCode);
    }

    #[test]
    fn git_log_oneline_is_not_tabular() {
        // Single-space commit lines are a listing, not an aligned table.
        let log = "890d92b feat(bench): add reproducible ordinary suite\n15e080b refactor: rename the package cli\n0d1eead docs: move the GitHub meta files\n";
        assert_ne!(cls(log, None, "Bash"), ContentClass::Tabular);
    }

    #[test]
    fn json_parses_as_structured() {
        assert_eq!(cls(JSON_TEXT, None, "Bash"), ContentClass::StructuredData);
    }

    #[test]
    fn ndjson_is_structured() {
        let nd = "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        assert_eq!(cls(nd, None, "Bash"), ContentClass::StructuredData);
    }

    #[test]
    fn toml_config_detected() {
        let toml = "# mu config\nname = \"mhd\"\nverbose = true\n";
        assert_eq!(cls(toml, None, "Bash"), ContentClass::StructuredData);
    }

    #[test]
    fn yaml_config_detected() {
        let yaml = "name: mhd\nverbose: true\nretries: 3\n";
        assert_eq!(cls(yaml, None, "Bash"), ContentClass::StructuredData);
    }

    #[test]
    fn unified_diff_detected() {
        assert_eq!(cls(DIFF_TEXT, None, "Bash"), ContentClass::Diff);
    }

    #[test]
    fn timestamped_log_detected() {
        assert_eq!(cls(LOG_TEXT, None, "Bash"), ContentClass::Logs);
    }

    #[test]
    fn bracket_level_log_detected() {
        let b = "[INFO]  starting\n[WARN]  retrying\n[ERROR] failed\n";
        assert_eq!(cls(b, None, "Bash"), ContentClass::Logs);
    }

    #[test]
    fn rust_test_output_detected() {
        assert_eq!(cls(TEST_TEXT, None, "Bash"), ContentClass::TestOutput);
    }

    #[test]
    fn pytest_output_detected() {
        let p = "================================== FAILURES ===================================\n____ test_foo ____\n    assert False\n=========================== 1 failed in 0.5s ===========================\n";
        assert_eq!(cls(p, None, "Bash"), ContentClass::TestOutput);
    }

    #[test]
    fn python_traceback_detected() {
        assert_eq!(cls(TRACEBACK, None, "Bash"), ContentClass::StackTrace);
    }

    #[test]
    fn rust_panic_detected() {
        assert_eq!(cls(PANIC, None, "Bash"), ContentClass::StackTrace);
    }

    #[test]
    fn cargo_build_error_detected() {
        assert_eq!(cls(CARGO_ERR, None, "Bash"), ContentClass::BuildDiagnostics);
    }

    #[test]
    fn ls_listing_detected() {
        assert_eq!(cls(LS_LISTING, None, "Bash"), ContentClass::DirListing);
    }

    #[test]
    fn tree_output_detected() {
        assert_eq!(cls(TREE, None, "Bash"), ContentClass::DirListing);
    }

    #[test]
    fn glob_output_detected_as_listing() {
        let glob = "src/main.rs\nsrc/lib.rs\nCargo.toml\nREADME.md\n";
        assert_eq!(cls(glob, None, "Glob"), ContentClass::DirListing);
    }

    #[test]
    fn tabular_output_detected() {
        assert_eq!(cls(PS_TABLE, None, "Bash"), ContentClass::Tabular);
    }

    #[test]
    fn pipe_delimited_rows_detected() {
        let pipe = "a|b|c\n1|2|3\n4|5|6\n7|8|9\n";
        assert_eq!(cls(pipe, None, "Bash"), ContentClass::Tabular);
    }

    #[test]
    fn plain_prose_is_prose() {
        assert_eq!(cls(PROSE, None, "Bash"), ContentClass::Prose);
        assert_eq!(cls(PROSE, None, "Agent"), ContentClass::Prose);
    }

    #[test]
    fn conflicting_signals_are_ambiguous() {
        // A timestamped test run fires both the logs and test-runner detectors
        // at comparable strength — genuinely ambiguous.
        let ts_test = "2026-07-30 10:00:00 running 3 tests\n2026-07-30 10:00:01 test a ... ok\n2026-07-30 10:00:02 test b ... ok\n=== summary ===\n2026-07-30 10:00:03 test result: ok. 3 passed; 0 failed\n";
        assert_eq!(cls(ts_test, None, "Bash"), ContentClass::Ambiguous);
    }

    #[test]
    fn markdown_bullet_list_is_not_a_diff() {
        let bullets = "- first item\n- second item\n- third item\n- fourth item\n";
        assert_ne!(cls(bullets, None, "Bash"), ContentClass::Diff);
    }

    // ── extract_text ────────────────────────────────────────────────────────

    #[test]
    fn extract_string_content() {
        let b = json!({"type": "tool_result", "content": "hello world"});
        assert_eq!(
            extract_text(b.as_object().unwrap()).as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn extract_array_content_joins_text_blocks() {
        let b = json!({"type": "tool_result", "content": [
            {"type": "text", "text": "line one"},
            {"type": "image", "source": {"type": "base64"}},
            {"type": "text", "text": "line two"},
        ]});
        assert_eq!(
            extract_text(b.as_object().unwrap()).as_deref(),
            Some("line one\nline two")
        );
    }

    #[test]
    fn extract_no_text_is_none() {
        let b = json!({"type": "tool_result", "content": []});
        assert!(extract_text(b.as_object().unwrap()).is_none());
        let b2 = json!({"type": "tool_result"});
        assert!(extract_text(b2.as_object().unwrap()).is_none());
    }

    // ── excerpts and redaction ──────────────────────────────────────────────

    #[test]
    fn excerpt_picks_first_meaningful_line() {
        let e = make_excerpt(JSON_TEXT);
        assert!(
            e.contains("name"),
            "excerpt should be the first real line: {e:?}"
        );
        assert!(e.len() <= 101, "excerpt must be capped ~100 chars");
    }

    #[test]
    fn redact_anthropic_key() {
        assert_eq!(
            redact_secrets("key=sk-ant-abcdefghijklmnopqrst"),
            "key=[REDACTED]"
        );
    }

    #[test]
    fn redact_long_hex_run() {
        assert_eq!(
            redact_secrets("hash=0123456789abcdef0123456789abcdef01234567 rest"),
            "hash=[REDACTED] rest"
        );
    }

    #[test]
    fn redact_bearer_token() {
        assert_eq!(
            redact_secrets("Authorization: Bearer abcdefghijklmnopqrstuvwxyz"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn redact_leaves_prose_and_short_words_alone() {
        let prose = "the quick brown fox jumps over the lazy dog";
        assert_eq!(redact_secrets(prose), prose);
        let short = "id=abc123";
        assert_eq!(redact_secrets(short), short);
    }

    #[test]
    fn redact_long_all_letters_word_is_not_a_token() {
        // A 28+ char all-letter word is a word, not base64 (needs non-alpha).
        let w = "supercalifragilisticexpialidociousx";
        assert_eq!(redact_secrets(w), w);
    }

    // ── median ──────────────────────────────────────────────────────────────

    #[test]
    fn median_empty_is_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[1, 2, 3]), Some(2));
        assert_eq!(median(&[1, 2, 3, 4]), Some(2));
    }

    // ── protection (as the live path sees it) ───────────────────────────────

    #[test]
    fn code_ext_block_is_protected() {
        assert!(tool_result_protected(RS_CODE, Some("rs"), true, 0.01));
    }

    #[test]
    fn plain_log_without_ext_is_not_protected() {
        assert!(!tool_result_protected(LOG_TEXT, None, true, 0.01));
    }

    #[test]
    fn json_without_ext_is_not_protected() {
        assert!(!tool_result_protected(JSON_TEXT, None, true, 0.01));
    }

    // ── body-level analysis wiring ──────────────────────────────────────────

    #[test]
    fn analyze_body_resolves_read_provenance_and_classifies() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu1", "name": "Read",
                     "input": {"file_path": "src/main.rs"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu1", "content": RS_CODE},
                ]},
            ]
        });
        let knobs = NativeKnobs::default();
        let obs = analyze_body(&body, &knobs);
        assert_eq!(obs.len(), 1);
        let (class, bytes, protected, _) = &obs[0];
        assert_eq!(*class, ContentClass::SourceCode);
        assert_eq!(*bytes, RS_CODE.chars().count());
        assert!(*protected, "a Read of .rs is protected by provenance");
    }

    #[test]
    fn analyze_body_bash_diff_is_not_protected() {
        let body = json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu2", "name": "Bash",
                     "input": {"command": "git diff"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu2", "content": DIFF_TEXT},
                ]},
            ]
        });
        let knobs = NativeKnobs::default();
        let obs = analyze_body(&body, &knobs);
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].0, ContentClass::Diff);
        assert!(
            !obs[0].2,
            "a bash diff has no provenance ext, so the protection gate does not stop elision"
        );
    }
}
