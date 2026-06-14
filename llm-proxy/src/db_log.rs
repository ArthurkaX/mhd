//! Structured logging via SQLite.
//!
//! Every proxy event is stored as a row in `proxy.db` with typed columns so
//! the log can be queried with SQL:
//!
//! ```sql
//! -- All downgrades with reason
//! SELECT * FROM events WHERE event_type = 'DOWNGRADE';
//!
//! -- Average stream duration by tier
//! SELECT tier, avg(duration_ms) FROM events
//!     WHERE event_type LIKE 'STREAM_%' AND duration_ms NOT NULL
//!     GROUP BY tier;
//!
//! -- Errors
//! SELECT * FROM events WHERE error NOT NULL;
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
            "CREATE TABLE IF NOT EXISTS events (
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
            CREATE INDEX IF NOT EXISTS idx_events_tier ON events(tier);",
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
}
