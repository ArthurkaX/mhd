//! Codex session discovery and JSONL event parser.
//!
//! Reads Codex rollout event streams and extracts only the fields listed in
//! LLM_MONITOR_SPEC §6.2. Never loads a full file into memory.

use std::path::{Path, PathBuf};
use std::fs;
use std::io::{BufRead, BufReader, Seek};

const CODEX_HOME_ENV: &str = "CODEX_HOME";
const DEFAULT_CODEX_HOME: &str = ".codex";

/// A single parsed event from a Codex rollout JSONL row.
#[derive(Debug, Clone)]
pub enum CodexEvent {
    SessionMeta {
        session_id: String,
        started_at: Option<i64>,
        cwd: Option<String>,
        project: Option<String>,
        model: Option<String>,
        cli_version: Option<String>,
    },
    TokenCount {
        session_id: String,
        event_at: i64,
        input_tokens: Option<i64>,
        cached_input: Option<i64>,
        cache_write: Option<i64>,
        output_tokens: Option<i64>,
        reasoning_tokens: Option<i64>,
        total_tokens: Option<i64>,
        cumulative_tokens: Option<i64>,
        context_window: Option<i64>,
        rate_limits: Option<CodexRateLimits>,
        source_offset: i64,
    },
    /// Unknown or uninteresting event type — skipped.
    Other,
}

/// Rate-limit block extracted from a token_count event.
#[derive(Debug, Clone)]
pub struct CodexRateLimits {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<CodexQuotaWindow>,
    pub secondary: Option<CodexQuotaWindow>,
    pub credits: Option<CodexCredits>,
}

/// A single quota window (primary or secondary).
#[derive(Debug, Clone)]
pub struct CodexQuotaWindow {
    pub window_kind: String,
    pub window_minutes: Option<i64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CodexCredits {
    pub has_credits: Option<bool>,
    pub unlimited_credits: Option<bool>,
    pub credit_balance: Option<String>,
}

/// A discovered Codex rollout file.
#[derive(Debug, Clone)]
pub struct CodexSource {
    pub canonical_path: PathBuf,
    pub file_identity: Option<String>,
    pub len: u64,
    pub modified: Option<i64>,
    pub is_archived: bool,
}

// ── Session discovery ────────────────────────────────────────────────────

/// Resolve the Codex home directory.
pub fn codex_home() -> PathBuf {
    if let Ok(env) = std::env::var(CODEX_HOME_ENV) {
        return PathBuf::from(env);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(DEFAULT_CODEX_HOME)
}

/// Scan for all rollout JSONL files under CODEX_HOME.
pub fn discover_sources(codex_home: &Path) -> Vec<CodexSource> {
    let mut sources = Vec::new();

    // sessions/**/*.jsonl
    let sessions_dir = codex_home.join("sessions");
    if sessions_dir.exists() {
        collect_jsonl(&sessions_dir, &mut sources, false);
    }

    // archived_sessions/*.jsonl
    let archived_dir = codex_home.join("archived_sessions");
    if archived_dir.exists() {
        collect_jsonl(&archived_dir, &mut sources, true);
    }

    sources
}

fn collect_jsonl(dir: &Path, out: &mut Vec<CodexSource>, archived: bool) {
    let walk = match walk_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for path in walk {
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let identity = path.file_stem().map(|s| s.to_string_lossy().to_string());
        out.push(CodexSource {
            canonical_path: path,
            file_identity: identity,
            len: meta.len(),
            modified,
            is_archived: archived,
        });
    }
}

fn walk_dir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                result.extend(walk_dir(&path)?);
            } else {
                result.push(path);
            }
        }
    }
    Ok(result)
}

// ── Streaming parser ─────────────────────────────────────────────────────

/// Stream a single JSONL file, yielding parsed events.
///
/// `start_offset` is the byte offset to resume from. Returns `(events, total_lines, skipped, final_offset)`.
/// `final_offset` is the byte position after the last *complete* line, so partial final lines
/// can be retried on the next scan.
pub fn parse_rollout(
    path: &Path,
    start_offset: u64,
) -> std::io::Result<ParseResult> {
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(start_offset))?;

    let mut events = Vec::new();
    let mut total_lines = 0u64;
    let mut skipped = 0u64;
    let mut last_complete_offset = start_offset;

    loop {
        let line_start = reader.stream_position()?;

        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF
        }

        total_lines += 1;

        // Check for partial final line (EOF without newline)
        if !line.ends_with('\n') {
            // Don't advance cursor past the start of a partial line
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            last_complete_offset = line_start + n as u64;
            continue;
        }

        match parse_event_at(trimmed, line_start) {
            Ok(CodexEvent::Other) => {}
            Ok(event) => events.push(event),
            Err(_) => {
                skipped += 1;
            }
        }

        last_complete_offset = line_start + n as u64;
    }

    Ok(ParseResult {
        events,
        total_lines,
        skipped,
        final_offset: last_complete_offset,
    })
}

/// Like `parse_event` but fills in `source_offset`.
fn parse_event_at(line: &str, offset: u64) -> Result<CodexEvent, serde_json::Error> {
    let mut event = parse_event(line)?;
    if let CodexEvent::TokenCount { ref mut source_offset, .. } = event {
        *source_offset = offset as i64;
    }
    Ok(event)
}

/// Result from parsing one rollout file.
#[derive(Debug)]
pub struct ParseResult {
    pub events: Vec<CodexEvent>,
    pub total_lines: u64,
    pub skipped: u64,
    pub final_offset: u64,
}

/// Parse a single JSONL row into a Codex event.
pub fn parse_event(line: &str) -> Result<CodexEvent, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(line)?;

    // Determine event type from the JSON structure.
    // Expected structure:
    //   { "session_id": "...", "event_msg": { "payload": { "type": "token_count", ... } } }
    // or { "session_id": "...", "event_type": "session_meta", ... }

    // session_meta events
    if value.get("event_type").and_then(|v| v.as_str()) == Some("session_meta") {
        return Ok(CodexEvent::SessionMeta {
            session_id: value
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            started_at: value
                .get("started_at")
                .or_else(|| value.get("timestamp"))
                .and_then(|v| v.as_i64()),
            cwd: value.get("cwd").and_then(|v| v.as_str()).map(String::from),
            project: value
                .get("project")
                .and_then(|v| v.as_str())
                .map(String::from),
            model: value
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from),
            cli_version: value
                .get("cli_version")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }

    // Look for event_msg.payload.type == "token_count"
    let session_id = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let event_at = value
        .get("timestamp")
        .or_else(|| value.get("event_at"))
        .and_then(|v| v.as_i64());

    let payload_type = value
        .get("event_msg")
        .and_then(|msg| msg.get("payload"))
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str());

    if payload_type == Some("token_count") {
        let payload = value
            .get("event_msg")
            .and_then(|msg| msg.get("payload"))
            .and_then(|p| p.get("info"));

        // Extract from last_token_usage
        let last = payload.and_then(|i| i.get("last_token_usage"));
        let total = payload.and_then(|i| i.get("total_token_usage"));

        // rate_limits block may be at payload.info.rate_limits or value.event_msg.payload.rate_limits
        let rl = payload.and_then(|i| i.get("rate_limits"))
            .or_else(|| {
                value.get("event_msg")
                    .and_then(|msg| msg.get("payload"))
                    .and_then(|p| p.get("rate_limits"))
            });

        let rate_limits = rl.map(parse_rate_limits);

        return Ok(CodexEvent::TokenCount {
            session_id,
            event_at: event_at.unwrap_or(0),
            input_tokens: last.and_then(|v| v.get("input_tokens")).and_then(|v| v.as_i64()),
            cached_input: last.and_then(|v| v.get("cached_input_tokens")).and_then(|v| v.as_i64()),
            cache_write: last.and_then(|v| v.get("cache_write_input_tokens")).and_then(|v| v.as_i64()),
            output_tokens: last.and_then(|v| v.get("output_tokens")).and_then(|v| v.as_i64()),
            reasoning_tokens: last.and_then(|v| v.get("reasoning_output_tokens")).and_then(|v| v.as_i64()),
            total_tokens: last.and_then(|v| v.get("total_tokens")).and_then(|v| v.as_i64()),
            cumulative_tokens: total.and_then(|v| v.get("total_tokens")).and_then(|v| v.as_i64()),
            context_window: payload.and_then(|i| i.get("model_context_window")).and_then(|v| v.as_i64()),
            rate_limits,
            source_offset: 0, // filled in by the caller
        });
    }

    Ok(CodexEvent::Other)
}

fn parse_rate_limits(rl: &serde_json::Value) -> CodexRateLimits {
    CodexRateLimits {
        limit_id: rl.get("limit_id").and_then(|v| v.as_str()).map(String::from),
        limit_name: rl.get("limit_name").and_then(|v| v.as_str()).map(String::from),
        plan_type: rl.get("plan_type").and_then(|v| v.as_str()).map(String::from),
        primary: rl.get("primary").map(|w| parse_window(w, "primary")),
        secondary: rl.get("secondary").map(|w| parse_window(w, "secondary")),
        credits: rl.get("credits").map(parse_credits),
    }
}

fn parse_window(w: &serde_json::Value, kind: &str) -> CodexQuotaWindow {
    let window_minutes = w.get("window_minutes").and_then(|v| v.as_i64());
    let kind = match window_minutes {
        Some(300) => "5h".to_string(),
        Some(10080) => "7d".to_string(),
        Some(m) => format!("{m}m"),
        None => kind.to_string(),
    };
    CodexQuotaWindow {
        window_kind: kind,
        window_minutes,
        used_percent: w.get("used_percent").and_then(|v| v.as_f64()),
        resets_at: w.get("resets_at").and_then(|v| v.as_i64()),
    }
}

fn parse_credits(c: &serde_json::Value) -> CodexCredits {
    CodexCredits {
        has_credits: c.get("has_credits").and_then(|v| v.as_bool()),
        unlimited_credits: c.get("unlimited_credits").and_then(|v| v.as_bool()),
        credit_balance: c.get("credit_balance").and_then(|v| v.as_str()).map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_no_codex_home() {
        // Should not panic even when CODEX_HOME doesn't exist
        let tmp = std::env::temp_dir().join("mhd_telemetry_test_no_codex");
        let _ = std::fs::remove_dir_all(&tmp);
        let sources = discover_sources(&tmp);
        assert!(sources.is_empty());
    }

    #[test]
    fn test_parse_session_meta() {
        let line = r#"{"session_id":"abc123","event_type":"session_meta","started_at":1700000000,"model":"claude-opus-4","cli_version":"0.1.0","cwd":"/home/user/project"}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::SessionMeta { session_id, started_at, model, cli_version, cwd, .. } => {
                assert_eq!(session_id, "abc123");
                assert_eq!(started_at, Some(1700000000));
                assert_eq!(model.as_deref(), Some("claude-opus-4"));
                assert_eq!(cli_version.as_deref(), Some("0.1.0"));
                assert_eq!(cwd.as_deref(), Some("/home/user/project"));
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_token_count() {
        let line = r#"{"session_id":"s1","timestamp":1700000100,"event_msg":{"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"total_tokens":150},"total_token_usage":{"total_tokens":1000},"model_context_window":200000}}}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::TokenCount { session_id, event_at, input_tokens, cached_input, output_tokens, total_tokens, cumulative_tokens, context_window, .. } => {
                assert_eq!(session_id, "s1");
                assert_eq!(event_at, 1700000100);
                assert_eq!(input_tokens, Some(100));
                assert_eq!(cached_input, Some(20));
                assert_eq!(output_tokens, Some(50));
                assert_eq!(total_tokens, Some(150));
                assert_eq!(cumulative_tokens, Some(1000));
                assert_eq!(context_window, Some(200000));
            }
            other => panic!("expected TokenCount, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_malformed() {
        assert!(parse_event("not json at all").is_err());
        assert!(parse_event("").is_err());
    }

    #[test]
    fn test_parse_unknown_fields_ignored() {
        let line = r#"{"session_id":"s1","event_type":"session_meta","extra_field":"should_be_ignored","nested":{"also":true}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::SessionMeta { session_id, .. } => {
                assert_eq!(session_id, "s1");
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_with_unknown_event_type() {
        let line = r#"{"session_id":"s1","event_type":"some_future_event","unknown_field":123}"#;
        match parse_event(line).unwrap() {
            CodexEvent::Other => {} // correct
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn test_quota_window_normalization() {
        let line = r#"{"session_id":"s1","timestamp":1700000100,"event_msg":{"payload":{"type":"token_count","info":{"rate_limits":{"limit_id":"tier-1","limit_name":"Professional","plan_type":"pro","primary":{"window_minutes":300,"used_percent":45.0,"resets_at":1700000000},"secondary":{"window_minutes":10080,"used_percent":12.5}}}}}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::TokenCount { rate_limits, .. } => {
                let rl = rate_limits.unwrap();
                let pri = rl.primary.unwrap();
                assert_eq!(pri.window_kind, "5h");
                assert_eq!(pri.used_percent, Some(45.0));
                let sec = rl.secondary.unwrap();
                assert_eq!(sec.window_kind, "7d");
                assert_eq!(sec.used_percent, Some(12.5));
            }
            other => panic!("expected TokenCount, got {other:?}"),
        }
    }
}
