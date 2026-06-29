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
}

impl DbLog {
    /// Open (or create) the SQLite database at `db_path` and create the schema
    /// if it doesn't exist.
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA user_version = 1;

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
                error_kind    TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_requests_run_seq ON requests(run_id, seq);
            CREATE INDEX IF NOT EXISTS idx_requests_model   ON requests(model);
            CREATE INDEX IF NOT EXISTS idx_requests_tier    ON requests(tier);

            CREATE TABLE IF NOT EXISTS notes (
                id   INTEGER PRIMARY KEY AUTOINCREMENT,
                ts   TEXT NOT NULL,
                text TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS request_bodies (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id    INTEGER NOT NULL,
                seq       INTEGER NOT NULL,
                ts        TEXT    NOT NULL,
                model     TEXT,
                provider  TEXT,
                body      TEXT    NOT NULL
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
            CREATE INDEX IF NOT EXISTS idx_quota_run ON quota(run_id, id);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert one structured event. Best-effort: errors are swallowed.
    pub fn insert(&self, ts: &str, event: &LogEvent) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO events (ts, seq, event_type, tier, effective_tier, target,
                                     model, target_model, reason, detail, inflight,
                                     duration_ms, error, error_kind, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    ts,
                    event.seq as i64,
                    &event.event_type,
                    event.tier,
                    event.effective_tier,
                    event.target,
                    event.model,
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
                    trim_tokens_before, trim_tokens_after, trim_stages
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    error_kind = ?11
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
    pub fn insert_request_body(&self, run_id: u64, seq: u64, ts: &str, model: Option<&str>, provider: &str, body: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![run_id as i64, seq as i64, ts, model, provider, body],
            );
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
}
