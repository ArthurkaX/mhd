//! Codex session discovery and JSONL event parser.
//!
//! Reads Codex rollout event streams and extracts only the fields listed in
//! LLM_MONITOR_SPEC §6.2. Never loads a full file into memory.
//!
//! Real Codex JSONL format (NOT the spec's placeholder format):
//!
//! session_meta:
//!   {"timestamp":"...","type":"session_meta","payload":{"id":"<uuid>","session_id":"<uuid>",
//!    "timestamp":"...","cwd":"...","originator":"Codex Desktop","cli_version":"...",
//!    "model_provider":"openai"}}
//!
//! token_count (within event_msg):
//!   {"timestamp":"...","type":"event_msg","payload":{"type":"token_count",
//!    "info":{"last_token_usage":{...},"total_token_usage":{...},"model_context_window":...},
//!    "rate_limits":{...}}}

use std::fs;
use std::io::{BufRead, BufReader, Seek};
use std::path::{Path, PathBuf};

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

    // sessions/YYYY/DD/*.jsonl
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

// ── ISO 8601 timestamp parsing ───────────────────────────────────────────

/// Parse an ISO 8601 UTC timestamp (`2026-04-10T14:36:55.987Z`) to unix seconds.
fn parse_iso_timestamp(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date_part, time_part) = s.split_once('T')?;

    let mut parts = date_part.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;

    // Strip fractional seconds — keep only hh:mm:ss
    let time_base = time_part.split('.').next().unwrap_or(time_part);
    let mut tparts = time_base.split(':');
    let hour: i64 = tparts.next()?.parse().ok()?;
    let min: i64 = tparts.next()?.parse().ok()?;
    let sec: i64 = tparts.next()?.parse().ok()?;

    let days = days_from_epoch(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Days from 1970-01-01 using Howard Hinnant's algorithm.
fn days_from_epoch(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 12 } else { month };
    let era = y / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m - 3) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ── Streaming parser ─────────────────────────────────────────────────────

/// Stream a single JSONL file, yielding parsed events.
///
/// `start_offset` is the byte offset to resume from. Returns `(events, total_lines, skipped, final_offset)`.
/// `final_offset` is the byte position after the last *complete* line, so partial final lines
/// can be retried on the next scan.
pub fn parse_rollout(path: &Path, start_offset: u64) -> std::io::Result<ParseResult> {
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

    // Post-process: fill in session_id for token_count events.
    // Codex events don't carry session_id in token_count rows — use the
    // session_id from the nearest preceding session_meta event.
    let mut current_session_id = String::new();
    for event in &mut events {
        match event {
            CodexEvent::SessionMeta { session_id, .. } => {
                if !session_id.is_empty() {
                    current_session_id.clone_from(session_id);
                }
            }
            CodexEvent::TokenCount { session_id, .. } => {
                if session_id.is_empty() && !current_session_id.is_empty() {
                    session_id.clone_from(&current_session_id);
                }
            }
            CodexEvent::Other => {}
        }
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
    if let CodexEvent::TokenCount {
        ref mut source_offset,
        ..
    } = event
    {
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
///
/// Real Codex format:
///   session_meta:  {"timestamp":"...","type":"session_meta","payload":{...}}
///   token_count:   {"timestamp":"...","type":"event_msg","payload":{"type":"token_count",...}}
pub fn parse_event(line: &str) -> Result<CodexEvent, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(line)?;

    let event_type = value.get("type").and_then(|v| v.as_str());

    // ── session_meta ──
    if event_type == Some("session_meta") {
        let payload = value.get("payload");
        let session_id = payload
            .and_then(|p| p.get("session_id").or_else(|| p.get("id")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let started_at = payload
            .and_then(|p| p.get("timestamp"))
            .and_then(|v| v.as_str())
            .and_then(parse_iso_timestamp);

        return Ok(CodexEvent::SessionMeta {
            session_id,
            started_at,
            cwd: payload
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str())
                .map(String::from),
            project: None, // Codex format has no project field
            model: payload
                .and_then(|p| p.get("model_provider"))
                .and_then(|v| v.as_str())
                .map(String::from),
            cli_version: payload
                .and_then(|p| p.get("cli_version"))
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }

    // ── event_msg → token_count ──
    if event_type == Some("event_msg") {
        let payload = value.get("payload");

        if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) == Some("token_count") {
            // session_id not present in token_count events — filled by parse_rollout
            let event_at = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_iso_timestamp)
                .unwrap_or(0);

            let info = payload.and_then(|p| p.get("info"));
            let last = info.and_then(|i| i.get("last_token_usage"));
            let total = info.and_then(|i| i.get("total_token_usage"));

            let rl = payload.and_then(|p| p.get("rate_limits"));
            let rate_limits = rl.map(parse_rate_limits);

            return Ok(CodexEvent::TokenCount {
                session_id: String::new(),
                event_at,
                input_tokens: last
                    .and_then(|v| v.get("input_tokens"))
                    .and_then(|v| v.as_i64()),
                cached_input: last
                    .and_then(|v| v.get("cached_input_tokens"))
                    .and_then(|v| v.as_i64()),
                cache_write: last
                    .and_then(|v| v.get("cache_write_input_tokens"))
                    .and_then(|v| v.as_i64()),
                output_tokens: last
                    .and_then(|v| v.get("output_tokens"))
                    .and_then(|v| v.as_i64()),
                reasoning_tokens: last
                    .and_then(|v| v.get("reasoning_output_tokens"))
                    .and_then(|v| v.as_i64()),
                total_tokens: last
                    .and_then(|v| v.get("total_tokens"))
                    .and_then(|v| v.as_i64()),
                cumulative_tokens: total
                    .and_then(|v| v.get("total_tokens"))
                    .and_then(|v| v.as_i64()),
                context_window: info
                    .and_then(|i| i.get("model_context_window"))
                    .and_then(|v| v.as_i64()),
                rate_limits,
                source_offset: 0,
            });
        }
    }

    Ok(CodexEvent::Other)
}

fn parse_rate_limits(rl: &serde_json::Value) -> CodexRateLimits {
    CodexRateLimits {
        limit_id: rl
            .get("limit_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        limit_name: rl
            .get("limit_name")
            .and_then(|v| v.as_str())
            .map(String::from),
        plan_type: rl
            .get("plan_type")
            .and_then(|v| v.as_str())
            .map(String::from),
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
        credit_balance: c
            .get("credit_balance")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_no_codex_home() {
        let tmp = std::env::temp_dir().join("mhd_telemetry_test_no_codex");
        let _ = std::fs::remove_dir_all(&tmp);
        let sources = discover_sources(&tmp);
        assert!(sources.is_empty());
    }

    #[test]
    fn test_parse_session_meta() {
        let line = r#"{"timestamp":"2026-04-10T14:36:55.987Z","type":"session_meta","payload":{"id":"abc123","timestamp":"2026-04-10T14:35:18.675Z","cwd":"/home/user/project","model_provider":"openai","cli_version":"0.119.0-alpha.11"}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::SessionMeta {
                session_id,
                started_at,
                model,
                cli_version,
                cwd,
                ..
            } => {
                assert_eq!(session_id, "abc123");
                // 2026-04-10T14:35:18Z = 20553 days * 86400 + 14*3600+35*60+18 = 1775831718
                assert_eq!(started_at, Some(1775831718));
                assert_eq!(model.as_deref(), Some("openai"));
                assert_eq!(cli_version.as_deref(), Some("0.119.0-alpha.11"));
                assert_eq!(cwd.as_deref(), Some("/home/user/project"));
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_session_meta_with_session_id_field() {
        // Newer format has both session_id and id in payload
        let line = r#"{"timestamp":"2026-07-01T12:01:36.633Z","type":"session_meta","payload":{"session_id":"xyz789","id":"abc123","timestamp":"2026-07-01T12:01:00.800Z","cwd":"/home","model_provider":"openai"}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::SessionMeta { session_id, .. } => {
                // Should prefer session_id over id
                assert_eq!(session_id, "xyz789");
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_token_count() {
        let line = r#"{"timestamp":"2026-04-10T14:37:03.443Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"total_tokens":150},"total_token_usage":{"total_tokens":1000},"model_context_window":200000},"rate_limits":{"limit_id":"codex","plan_type":"plus","primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1775849059},"secondary":{"used_percent":0.0,"window_minutes":10080,"resets_at":1776435859}}}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::TokenCount {
                session_id,
                event_at,
                input_tokens,
                cached_input,
                output_tokens,
                total_tokens,
                cumulative_tokens,
                context_window,
                ..
            } => {
                assert_eq!(session_id, ""); // filled by parse_rollout
                // 2026-04-10T14:37:03Z = 20553 days * 86400 + 14*3600+37*60+3 = 1775831823
                assert_eq!(event_at, 1775831823);
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
    fn test_parse_token_count_with_null_info() {
        // Some token_count events have null info (quota-only updates)
        let line = r#"{"timestamp":"2026-04-10T14:37:03.443Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","plan_type":"plus","primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1775849059}}}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::TokenCount {
                input_tokens,
                cached_input,
                output_tokens,
                rate_limits,
                ..
            } => {
                assert!(input_tokens.is_none());
                assert!(cached_input.is_none());
                assert!(output_tokens.is_none());
                assert!(rate_limits.is_some());
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
        let line = r#"{"timestamp":"2026-04-10T14:36:55.987Z","type":"session_meta","payload":{"id":"s1","extra_field":"should_be_ignored"}}"#;
        let event = parse_event(line).unwrap();
        match event {
            CodexEvent::SessionMeta { session_id, .. } => {
                assert_eq!(session_id, "s1");
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_unknown_event_type() {
        let line = r#"{"timestamp":"2026-04-10T14:36:55.987Z","type":"response_item","payload":{"type":"message"}}"#;
        match parse_event(line).unwrap() {
            CodexEvent::Other => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_old_format_is_other() {
        // Old spec placeholder format should NOT match — it's not real Codex
        let line = r#"{"session_id":"abc123","event_type":"session_meta","started_at":1700000000}"#;
        match parse_event(line).unwrap() {
            CodexEvent::Other => {}
            other => panic!("expected Other (old format not valid), got {other:?}"),
        }
    }

    #[test]
    fn test_quota_window_normalization() {
        let line = r#"{"timestamp":"2026-04-10T14:37:03.443Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","plan_type":"pro","primary":{"window_minutes":300,"used_percent":45.0,"resets_at":1700000000},"secondary":{"window_minutes":10080,"used_percent":12.5}}}}"#;
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

    #[test]
    fn test_parse_rollout_tracks_session_id() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("mhd_test_rollout_session");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout-test.jsonl");

        let content = r#"{"timestamp":"2026-04-10T14:36:55.987Z","type":"session_meta","payload":{"id":"session-123","timestamp":"2026-04-10T14:35:18.675Z","cwd":"/home"}}
{"timestamp":"2026-04-10T14:37:03.443Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100}},"rate_limits":{"primary":{"window_minutes":300,"used_percent":1.0,"resets_at":1775849059}}}}
{"timestamp":"2026-04-10T14:37:04.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200}},"rate_limits":null}}
"#;
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();

        let result = parse_rollout(&path, 0).unwrap();
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.skipped, 0);

        // SessionMeta
        match &result.events[0] {
            CodexEvent::SessionMeta { session_id, .. } => {
                assert_eq!(session_id, "session-123");
            }
            other => panic!("expected SessionMeta, got {other:?}"),
        }

        // TokenCount events should have session_id filled in
        for (i, event) in result.events.iter().enumerate().skip(1) {
            match event {
                CodexEvent::TokenCount { session_id, .. } => {
                    assert_eq!(session_id, "session-123", "event {i} has wrong session_id");
                }
                other => panic!("expected TokenCount at {i}, got {other:?}"),
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_parse_iso_timestamp() {
        // 2026-04-10T14:36:55Z = 20553*86400 + 52615 = 1775831815
        assert_eq!(
            parse_iso_timestamp("2026-04-10T14:36:55.987Z"),
            Some(1775831815)
        );
        assert_eq!(parse_iso_timestamp("1970-01-01T00:00:00Z"), Some(0));
        // 2026-07-01T12:01:36Z = 20635*86400 + 43296 = 1782907296
        assert_eq!(
            parse_iso_timestamp("2026-07-01T12:01:36Z"),
            Some(1782907296)
        );
        assert_eq!(parse_iso_timestamp("not-a-timestamp"), None);
    }

    #[test]
    fn test_parse_real_file_first_events() {
        let home = codex_home();
        let archived = home.join("archived_sessions");
        if !archived.exists() {
            eprintln!("SKIP: no archived_sessions dir");
            return;
        }
        let files: Vec<_> = std::fs::read_dir(&archived)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        if files.is_empty() {
            eprintln!("SKIP: no archived session files");
            return;
        }

        let result = parse_rollout(&files[0], 0).unwrap();
        assert!(!result.events.is_empty());
        assert_eq!(result.skipped, 0);

        let has_session_meta = result
            .events
            .iter()
            .any(|e| matches!(e, CodexEvent::SessionMeta { .. }));
        let has_token_count = result
            .events
            .iter()
            .any(|e| matches!(e, CodexEvent::TokenCount { .. }));
        assert!(has_session_meta);
        assert!(has_token_count);

        for event in &result.events {
            if let CodexEvent::TokenCount { session_id, .. } = event {
                assert!(
                    !session_id.is_empty(),
                    "token_count should have session_id filled"
                );
            }
        }

        eprintln!(
            "OK: {} events ({} session_meta, {} token_count, skip {}) from {}",
            result.events.len(),
            result
                .events
                .iter()
                .filter(|e| matches!(e, CodexEvent::SessionMeta { .. }))
                .count(),
            result
                .events
                .iter()
                .filter(|e| matches!(e, CodexEvent::TokenCount { .. }))
                .count(),
            result.skipped,
            files[0].display(),
        );
    }

    #[test]
    fn test_full_import_from_real_file() {
        use crate::db;
        use crate::import;

        let home = codex_home();
        let archived = home.join("archived_sessions");
        if !archived.exists()
            || !std::fs::read_dir(&archived)
                .ok()
                .map(|mut e| e.next().is_some())
                .unwrap_or(false)
        {
            eprintln!("SKIP: no archived_sessions dir with files");
            return;
        }

        let tmp = std::env::temp_dir().join(format!("mhd_import_test_{}", std::process::id()));
        let db_path = tmp.join("telemetry.db");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut telemetry_db = db::open_or_create(&db_path).unwrap();
        let result = import::run_import(&mut telemetry_db, &home);

        eprintln!(
            "Import result: {} sources scanned, {} imported, {} sessions, {} model calls, {} quota samples, {} errors",
            result.sources_scanned,
            result.sources_imported,
            result.sessions_added,
            result.model_calls_added,
            result.quota_samples_added,
            result.errors.len(),
        );
        for err in &result.errors {
            eprintln!("  error: {err}");
        }

        assert!(
            result.sources_scanned > 0,
            "should find at least one source file"
        );
        assert!(
            result.sources_imported > 0,
            "should import at least one source"
        );
        assert!(result.sessions_added > 0, "should add at least one session");
        assert!(
            result.model_calls_added > 0,
            "should add at least one model call"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
