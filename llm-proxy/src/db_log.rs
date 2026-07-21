//! Structured logging via SQLite.
//!
//! Two tables serve distinct purposes:
//!
//! - `events`   — generic one-off events: HANG, RAW text lines, vision, etc.
//! - `requests` — one row per proxy request, updated twice: at arrival (routing
//!                metadata + trim info) and at completion (token counts, status).
//! - `notes`    — free-text user notes with timestamp.
//!
//! The old semi-structured approach of stuffing token/trim/cache data into the
//! `reason` free-text field on generic events is replaced by typed columns on
//! `requests`. The `events` table stays for HANG and RAW log lines.
//!
//! ```sql
//! -- Requests with cache hits
//! SELECT * FROM requests WHERE cache_read_tokens > 0;
//!
//! -- Average trim savings by tier
//! SELECT tier, avg(trim_tokens_before - trim_tokens_after) FROM requests
//!     WHERE trim_applied = 1 GROUP BY tier;
//!
//! -- All errors
//! SELECT * FROM requests WHERE error NOT NULL;
//!
//! -- HANG events (still in generic events table)
//! SELECT * FROM events WHERE event_type = 'HANG';
//! ```

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

/// A single structured log event written to the SQLite database.
#[derive(Debug, Clone, Default)]
pub struct LogEvent {
    pub seq: u64,
    pub event_type: String,
    pub tier: Option<String>,
    pub effective_tier: Option<String>,
    pub target: Option<String>,
    pub model: Option<String>,
    pub target_model: Option<String>,
    pub reason: Option<String>,
    pub detail: Option<String>,
    pub inflight: Option<u64>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub error_kind: Option<String>,
    pub status: Option<u16>,
}

/// A typed row for the `requests` table, carrying the start-of-request fields.
/// Completion columns (ts_end, duration_ms, tokens, status, error) are written
/// separately via [`DbLog::update_request_completion`].
#[derive(Debug, Clone, Default)]
pub struct RequestRow {
    pub run_id: u64,
    pub seq: u64,
    pub ts_start: String,
    pub tier: Option<String>,
    pub effective_tier: Option<String>,
    pub target: Option<String>,
    pub model: Option<String>,
    pub downgraded: bool,
    pub downgrade_reason: Option<String>,
    pub trim_applied: bool,
    pub trim_preset: Option<String>,
    pub trim_config: Option<String>,
    pub trim_tokens_before: Option<u64>,
    pub trim_tokens_after: Option<u64>,
    pub trim_stages: Option<String>,
    pub user_agent: Option<String>,
}

/// One typed snapshot of Anthropic `anthropic-ratelimit-unified-*` quota state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaSnapshot {
    pub h5_utilization: Option<f64>,
    pub h5_status: Option<String>,
    pub h5_reset: Option<i64>,
    pub d7_utilization: Option<f64>,
    pub d7_status: Option<String>,
    pub d7_reset: Option<i64>,
    pub representative_claim: Option<String>,
    pub fallback_status: Option<String>,
    pub overage_status: Option<String>,
}

/// Wraps a SQLite connection.
pub struct DbLog {
    conn: Mutex<Connection>,
    /// Maximum number of rows to keep in `request_bodies`. 0 = unlimited.
    pub corpus_max_rows: usize,
}

impl DbLog {
    /// Open (or create) the SQLite database at `db_path` and create the schema
    /// if it doesn't exist. `corpus_max_rows` caps the `request_bodies` table
    /// (0 = unlimited).
    pub fn open(db_path: &Path, corpus_max_rows: usize) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        // Read schema version BEFORE the batch so we can detect old DBs
        // that need ALTER TABLE migration.
        let old_version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        conn.execute_batch(
            "PRAGMA user_version = 2;

            -- events: append-only log stream.
            -- Hot-path lines (REQ/RESP/START/DONE/STREAM_*) arrive via
            -- State::log_line() as one text blob in `detail`; insert() now
            -- parses that blob (classify_raw_detail) to backfill the typed
            -- columns (event_type/seq/target/model). Rows written before
            -- 2026-06-30 have those columns NULL (text only in `detail`) --
            -- do NOT trust pre-2026-06-30 events for column queries.
            -- For trim/token analytics use the `requests` table instead.
            CREATE TABLE IF NOT EXISTS events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts          TEXT    NOT NULL,
                seq         INTEGER NOT NULL,
                event_type  TEXT    NOT NULL,
                tier        TEXT,
                effective_tier TEXT,
                target      TEXT,
                model       TEXT,
                target_model TEXT,
                reason      TEXT,
                detail      TEXT,
                inflight    INTEGER,
                duration_ms INTEGER,
                error       TEXT,
                error_kind  TEXT,
                status      INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_events_seq  ON events(seq);
            CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
            CREATE INDEX IF NOT EXISTS idx_events_tier ON events(tier);

            -- requests: SOURCE OF TRUTH for analytics. One fully-typed row
            -- per request: trim_applied, trim_config (json: engine/desc/
            -- head/tail), trim_tokens_before/after, input/output/cache_read/
            -- cache_creation_tokens, downgraded, effective_tier. All trim%
            -- and token-savings numbers should be computed from here.
            CREATE TABLE IF NOT EXISTS requests (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id        INTEGER NOT NULL,
                seq           INTEGER NOT NULL,
                ts_start      TEXT    NOT NULL,
                ts_end        TEXT,
                duration_ms   INTEGER,
                tier          TEXT,
                effective_tier TEXT,
                target        TEXT,
                model         TEXT,
                downgraded    INTEGER,
                downgrade_reason TEXT,
                trim_applied  INTEGER,
                trim_preset   TEXT,
                trim_config   TEXT,
                trim_tokens_before INTEGER,
                trim_tokens_after  INTEGER,
                trim_stages   TEXT,
                input_tokens  INTEGER,
                output_tokens INTEGER,
                cache_read_tokens     INTEGER,
                cache_creation_tokens INTEGER,
                status        INTEGER,
                error         TEXT,
                error_kind    TEXT,
                user_agent    TEXT,
                upstream_model TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_requests_run_seq ON requests(run_id, seq);
            CREATE INDEX IF NOT EXISTS idx_requests_model   ON requests(model);
            CREATE INDEX IF NOT EXISTS idx_requests_tier    ON requests(tier);

            CREATE TABLE IF NOT EXISTS notes (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                ts   TEXT NOT NULL,
                text TEXT NOT NULL
            );

            -- request_bodies: corpus for offline backtest. `body` is a
            -- zstd-compressed BLOB (decompress_body). Capped at
            -- corpus_max_rows (default 5000), shared across anthropic+openai
            -- providers -- heavy deepseek/openai traffic evicts old anthropic
            -- bodies, so keep an eye on provider balance before backtesting.
            CREATE TABLE IF NOT EXISTS request_bodies (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id    INTEGER NOT NULL,
                seq       INTEGER NOT NULL,
                ts        TEXT    NOT NULL,
                model     TEXT,
                provider  TEXT,
                body      BLOB    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_request_bodies_run_seq ON request_bodies(run_id, seq);
            CREATE TABLE IF NOT EXISTS quota (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                ts                   TEXT    NOT NULL,
                run_id               INTEGER NOT NULL,
                h5_utilization       REAL,
                h5_status            TEXT,
                h5_reset             INTEGER,
                d7_utilization       REAL,
                d7_status            TEXT,
                d7_reset             INTEGER,
                representative_claim TEXT,
                fallback_status      TEXT,
                overage_status       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_quota_run ON quota(run_id, id);

            -- bench_runs: history of Bench A/B backtest runs (manual [Bench] button in
            -- Proxy Trace). Each row = one deterministic replay of the anthropic corpus
            -- through the native trim engine with the knobs that were live at run time.
            -- Anthropic-only. tokens_* are estimated (chars/4), same estimator as backtest.
            CREATE TABLE IF NOT EXISTS bench_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'anthropic',
                n_bodies INTEGER NOT NULL,
                n_trimmed INTEGER NOT NULL,
                tokens_off INTEGER NOT NULL,
                tokens_on INTEGER NOT NULL,
                avg_trim_pct REAL NOT NULL,
                median_trim_pct REAL NOT NULL,
                headroom_pct REAL NOT NULL,
                fail_open_ok INTEGER NOT NULL,
                deterministic INTEGER NOT NULL,
                elapsed_ms INTEGER NOT NULL,
                knobs_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_bench_runs_ts ON bench_runs(id);",
        )?;
        // Migration: add user_agent / upstream_model columns to pre-existing DBs.
        // Fresh DBs already have them from CREATE TABLE; old DBs (version < 2) need ALTERs.
        // `let _ =` tolerates "duplicate column" if the CREATE TABLE already added them.
        if old_version < 2 {
            let _ = conn.execute("ALTER TABLE requests ADD COLUMN user_agent TEXT", []);
            let _ = conn.execute("ALTER TABLE requests ADD COLUMN upstream_model TEXT", []);
            conn.pragma_update(None, "user_version", 2)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            corpus_max_rows,
        })
    }

    /// Insert one structured event. Best-effort: errors are swallowed.
    /// When the event is a "RAW" line with unset typed columns, attempts to parse
    /// the detail string into typed columns (see [`classify_raw_detail`]).
    pub fn insert(&self, ts: &str, event: &LogEvent) {
        if let Ok(conn) = self.conn.lock() {
            // Classify RAW detail lines with no pre-populated typed columns.
            let needs_classification = event.event_type == "RAW"
                && event.reason.is_none()
                && event.target.is_none()
                && event.detail.is_some();

            let (event_type, seq, target, model) = if needs_classification {
                classify_raw_detail(
                    event.detail.as_ref().unwrap(),
                    &event.event_type,
                    event.seq as i64,
                )
            } else {
                (
                    event.event_type.clone(),
                    event.seq as i64,
                    event.target.clone(),
                    event.model.clone(),
                )
            };

            let _ = conn.execute(
                "INSERT INTO events (ts, seq, event_type, tier, effective_tier, target,
                                     model, target_model, reason, detail, inflight,
                                     duration_ms, error, error_kind, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    ts,
                    seq,
                    &event_type,
                    event.tier,
                    event.effective_tier,
                    target,
                    model,
                    event.target_model,
                    event.reason,
                    event.detail,
                    event.inflight.map(|v| v as i64),
                    event.duration_ms.map(|v| v as i64),
                    event.error,
                    event.error_kind,
                    event.status.map(|v| v as i16),
                ],
            );
        }
    }

    /// Insert a request row (start-of-request columns). Completion columns are
    /// left NULL and updated later via [`update_request_completion`].
    /// Best-effort: errors are swallowed.
    pub fn insert_request(&self, row: &RequestRow) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO requests (
                    run_id, seq, ts_start, tier, effective_tier, target, model,
                    downgraded, downgrade_reason,
                    trim_applied, trim_preset, trim_config,
                    trim_tokens_before, trim_tokens_after, trim_stages,
                    user_agent
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    row.run_id as i64,
                    row.seq as i64,
                    &row.ts_start,
                    row.tier,
                    row.effective_tier,
                    row.target,
                    row.model,
                    row.downgraded as i64,
                    row.downgrade_reason,
                    row.trim_applied as i64,
                    row.trim_preset,
                    row.trim_config,
                    row.trim_tokens_before.map(|v| v as i64),
                    row.trim_tokens_after.map(|v| v as i64),
                    row.trim_stages,
                    row.user_agent,
                ],
            );
        }
    }

    /// Update the completion columns on the request row matching (run_id, seq).
    /// Best-effort: errors are swallowed.
    pub fn update_request_completion(
        &self,
        run_id: u64,
        seq: u64,
        ts_end: &str,
        duration_ms: Option<u64>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        status: Option<u16>,
        error: Option<&str>,
        error_kind: Option<&str>,
        upstream_model: Option<&str>,
    ) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "UPDATE requests SET
                    ts_end = ?3,
                    duration_ms = ?4,
                    input_tokens = ?5,
                    output_tokens = ?6,
                    cache_read_tokens = ?7,
                    cache_creation_tokens = ?8,
                    status = ?9,
                    error = ?10,
                    error_kind = ?11,
                    upstream_model = ?12
                 WHERE run_id = ?1 AND seq = ?2",
                rusqlite::params![
                    run_id as i64,
                    seq as i64,
                    ts_end,
                    duration_ms.map(|v| v as i64),
                    input_tokens as i64,
                    output_tokens as i64,
                    cache_read_tokens as i64,
                    cache_creation_tokens as i64,
                    status.map(|v| v as i64),
                    error,
                    error_kind,
                    upstream_model,
                ],
            );
        }
    }

    /// Close a request row that was left open (ts_end IS NULL) because the
    /// client cancelled mid-flight and the completion future was dropped.
    /// Race-free: the `ts_end IS NULL` guard in the WHERE clause means this
    /// no-ops if a real completion (success or error) already ran. Best-effort.
    pub fn mark_request_cancelled(&self, run_id: u64, seq: u64, ts_end: &str, duration_ms: Option<u64>) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "UPDATE requests SET
                    ts_end = ?3,
                    duration_ms = ?4,
                    status = 0,
                    error = 'client cancelled (dropped mid-flight)',
                    error_kind = 'CANCELLED'
                 WHERE run_id = ?1 AND seq = ?2 AND ts_end IS NULL",
                rusqlite::params![
                    run_id as i64,
                    seq as i64,
                    ts_end,
                    duration_ms.map(|v| v as i64),
                ],
            );
        }
    }

    /// Insert a user note. Best-effort: errors are swallowed.
    pub fn insert_note(&self, ts: &str, text: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO notes (ts, text) VALUES (?1, ?2)",
                rusqlite::params![ts, text],
            );
        }
    }

    /// Insert a captured pre-trim request body. Best-effort: errors are swallowed.
    /// The body is zstd-compressed (level 9) before storage.
    /// After a successful insert, prunes the oldest rows so at most
    /// `corpus_max_rows` rows remain (0 = unlimited).
    pub fn insert_request_body(&self, run_id: u64, seq: u64, ts: &str, model: Option<&str>, provider: &str, body: &str) {
        let compressed = compress_body(body);
        if compressed.is_empty() {
            return; // compression produced nothing (only on encode error); skip row
        }
        if let Ok(conn) = self.conn.lock() {
            let inserted = conn.execute(
                "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![run_id as i64, seq as i64, ts, model, provider, compressed],
            );
            if inserted.is_ok() && self.corpus_max_rows > 0 {
                let max = self.corpus_max_rows as i64;
                let _ = conn.execute(
                    "DELETE FROM request_bodies WHERE id NOT IN \
                     (SELECT id FROM request_bodies ORDER BY id DESC LIMIT ?1)",
                    rusqlite::params![max],
                );
            }
        }
    }

    /// Insert a quota snapshot row. Best-effort: errors are swallowed.
    pub fn insert_quota(&self, run_id: u64, ts: &str, q: &QuotaSnapshot) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO quota (ts, run_id, h5_utilization, h5_status, h5_reset,
                                    d7_utilization, d7_status, d7_reset, representative_claim,
                                    fallback_status, overage_status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    ts,
                    run_id as i64,
                    q.h5_utilization,
                    q.h5_status,
                    q.h5_reset,
                    q.d7_utilization,
                    q.d7_status,
                    q.d7_reset,
                    q.representative_claim,
                    q.fallback_status,
                    q.overage_status,
                ],
            );
        }
    }

    /// Insert one Bench A/B run into the history table. Best-effort: errors swallowed.
    /// `headroom_pct` is the precomputed analytic projection 1/(1-avg/100)-1 in percent.
    /// `knobs_json` is the serialized NativeKnobs snapshot used for the run.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_bench_run(
        &self,
        ts: &str,
        provider: &str,
        result: &crate::bench::BenchResult,
        headroom_pct: f64,
        knobs_json: &str,
    ) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO bench_runs (ts, provider, n_bodies, n_trimmed, tokens_off, tokens_on,
                     avg_trim_pct, median_trim_pct, headroom_pct, fail_open_ok, deterministic,
                     elapsed_ms, knobs_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                rusqlite::params![
                    ts, provider,
                    result.n_bodies as i64, result.n_trimmed as i64,
                    result.tokens_off as i64, result.tokens_on as i64,
                    result.avg_trim_pct, result.median_trim_pct, headroom_pct,
                    result.fail_open_ok as i64, result.deterministic as i64,
                    result.elapsed_ms as i64, knobs_json,
                ],
            );
        }
    }
}

// ── raw-detail classifier ─────────────────────────────────────────────────────

/// Attempt to parse a "RAW"-type detail line into typed columns.
///
/// Expected format (after the timestamp prefix):
/// ```text
/// #<id> <target> <EVENTWORD> <rest...>
/// ```
///
/// Extracts:
/// - `event_type` — classified from EVENTWORD (REQ, RESP, START, DONE,
///   STREAM_START, STREAM_DONE, ERROR, or RAW fallback)
/// - `seq` — the integer after `#`
/// - `target` — the token after the id (e.g. "native")
/// - `model` — from `model=<x>` if present
///
/// On any parse failure, returns the supplied fallback values unchanged
/// (fail-open — never panic, never lose data).
fn classify_raw_detail(
    detail: &str,
    fallback_event_type: &str,
    fallback_seq: i64,
) -> (String, i64, Option<String>, Option<String>) {
    let default = (
        fallback_event_type.to_string(),
        fallback_seq,
        None,
        None,
    );

    let hash_pos = match detail.find('#') {
        Some(p) => p,
        None => return default,
    };

    let after_hash = &detail[hash_pos + 1..];

    // Extract the numeric id after #.
    let id_end = after_hash
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_hash.len());
    if id_end == 0 {
        return default;
    }
    let seq: i64 = match after_hash[..id_end].parse() {
        Ok(n) => n,
        Err(_) => return default,
    };

    // The next token after #<id> is the target (e.g. "native").
    let after_id = after_hash[id_end..].trim_start();
    if after_id.is_empty() {
        return (default.0, seq, default.2, default.3);
    }
    let target_end = after_id
        .find(char::is_whitespace)
        .unwrap_or(after_id.len());
    let target = after_id[..target_end].to_string();

    // Classify event type and extract model from the remainder.
    let after_target = after_id[target_end..].trim_start();
    let event_type = classify_event_word(after_target);
    let model = extract_model_param(after_target);

    (event_type, seq, Some(target), model)
}

/// Classify the event word(s) after the target token into a canonical
/// event_type string.
fn classify_event_word(s: &str) -> String {
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return "RAW".to_string();
    }

    // Two-word patterns (stream DONE, stream START).
    if let Some(rest) = trimmed.strip_prefix("stream ") {
        let rest = rest.trim_start();
        if rest.starts_with("START") {
            return "STREAM_START".to_string();
        }
        if rest.starts_with("DONE") {
            return "STREAM_DONE".to_string();
        }
    }

    // Single-word event types.
    let word_end = trimmed
        .find(char::is_whitespace)
        .unwrap_or(trimmed.len());
    let word = &trimmed[..word_end];
    match word {
        "REQ" => return "REQ".to_string(),
        "RESP" => return "RESP".to_string(),
        "START" => return "START".to_string(),
        "DONE" => return "DONE".to_string(),
        _ => {}
    }

    // Lines containing _ERROR (e.g. SEND_ERROR, UPSTREAM_ERROR).
    if trimmed.contains("_ERROR") {
        return "ERROR".to_string();
    }

    "RAW".to_string()
}

/// Extract the model name from a `model=<value>` substring found in `s`.
fn extract_model_param(s: &str) -> Option<String> {
    let marker = "model=";
    let pos = s.find(marker)?;
    let after = &s[pos + marker.len()..];
    let end = after
        .find(|c: char| c == '|' || c.is_whitespace())
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    Some(after[..end].to_string())
}

// ── body compression helpers ──────────────────────────────────────────────────

const ZSTD_LEVEL: i32 = 9;

/// Compress a request body string with zstd (level 9).
/// Returns an empty Vec only if encoding itself fails (effectively never).
pub fn compress_body(s: &str) -> Vec<u8> {
    zstd::encode_all(s.as_bytes(), ZSTD_LEVEL).unwrap_or_default()
}

/// Decompress a zstd-compressed body blob back to a UTF-8 string.
/// Returns None if decompression or UTF-8 conversion fails (fail-open).
pub fn decompress_body(b: &[u8]) -> Option<String> {
    let decompressed = zstd::decode_all(b).ok()?;
    String::from_utf8(decompressed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json_with_unicode() {
        let input = r#"{"model":"claude-opus-4-5","messages":[{"role":"user","content":"Hello 🌍 — привет — 你好"}]}"#;
        let compressed = compress_body(input);
        let restored = decompress_body(&compressed).expect("decompress failed");
        assert_eq!(restored, input);
    }

    #[test]
    fn round_trip_empty_string() {
        let input = "";
        let compressed = compress_body(input);
        let restored = decompress_body(&compressed).expect("decompress of empty failed");
        assert_eq!(restored, input);
    }

    #[test]
    fn round_trip_large_body() {
        // ~100 KB of repeating JSON-like content.
        let chunk = r#"{"role":"user","content":"AAAA BBBB CCCC DDDD EEEE FFFF GGGG HHHH IIII JJJJ "}"#;
        let input: String = chunk.repeat(1100); // ~86 KB (78 bytes * 1100 = 85,800)
        assert!(input.len() >= 80_000);
        let compressed = compress_body(&input);
        // Sanity: compressed should be much smaller than the original.
        assert!(compressed.len() < input.len() / 4, "compression ratio unexpectedly poor");
        let restored = decompress_body(&compressed).expect("decompress of large body failed");
        assert_eq!(restored, input);
    }

    #[test]
    fn decompress_garbage_returns_none() {
        let garbage: &[u8] = b"\xDE\xAD\xBE\xEF\xFF\xFE garbage that is not zstd";
        assert!(decompress_body(garbage).is_none(), "expected None for garbage bytes");
    }

    // ── corpus_max_rows prune tests ───────────────────────────────────────────

    /// Build a DbLog backed by a named temp file that SQLite can open.
    /// rusqlite supports ":memory:" but we need a file path for `DbLog::open`.
    fn open_temp_db(corpus_max_rows: usize) -> (DbLog, tempfile::TempPath) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.into_temp_path();
        let db = DbLog::open(path.as_ref(), corpus_max_rows).expect("open");
        (db, path)
    }

    fn insert_n_bodies(db: &DbLog, n: u64) {
        for i in 0..n {
            let body = format!(r#"{{"seq":{}}}"#, i);
            db.insert_request_body(1, i, "2024-01-01T00:00:00Z", None, "test", &body);
        }
    }

    fn count_bodies(db: &DbLog) -> i64 {
        let guard = db.conn.lock().unwrap();
        guard
            .query_row("SELECT COUNT(*) FROM request_bodies", [], |row| row.get(0))
            .unwrap_or(0)
    }

    fn min_id_bodies(db: &DbLog) -> i64 {
        let guard = db.conn.lock().unwrap();
        guard
            .query_row("SELECT MIN(id) FROM request_bodies", [], |row| row.get(0))
            .unwrap_or(0)
    }

    #[test]
    fn corpus_prune_keeps_newest_n_rows() {
        let (db, _tmp) = open_temp_db(10);
        // Insert 12; only the newest 10 should survive.
        insert_n_bodies(&db, 12);
        assert_eq!(count_bodies(&db), 10, "expected 10 rows after pruning");
        // The oldest 2 (id=1, id=2) should be gone; MIN(id) must be 3.
        assert_eq!(min_id_bodies(&db), 3, "expected ids 3..12 to remain");
    }

    #[test]
    fn corpus_unlimited_keeps_all_rows() {
        let (db, _tmp) = open_temp_db(0); // 0 = unlimited
        insert_n_bodies(&db, 5);
        assert_eq!(count_bodies(&db), 5, "expected all 5 rows to remain");
    }
}
