//! Telemetry database — SQLite-backed store for LLM usage metadata.
//!
//! Schema versioning via `PRAGMA user_version`. Migrations are forward-only
//! and transactional. Never stores prompt, reasoning, or transcript content.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, params};

/// Current schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 3;

/// Database configuration.
pub struct TelemetryDb {
    conn: Connection,
}

/// Errors from database operations.
#[derive(Debug)]
pub enum TelemetryError {
    Sql(rusqlite::Error),
    /// Only set when the DB path's parent directory cannot be created.
    Io(std::io::Error),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryError::Sql(e) => write!(f, "sqlite: {e}"),
            TelemetryError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<rusqlite::Error> for TelemetryError {
    fn from(e: rusqlite::Error) -> Self {
        TelemetryError::Sql(e)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Resolve the default telemetry database path.
///
/// `%USERPROFILE%\.config\mhd\telemetry.db`
pub fn default_db_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    home.join(".config").join("mhd").join("telemetry.db")
}

/// Ensure the parent directory for `path` exists.
fn ensure_parent(path: &std::path::Path) -> Result<(), TelemetryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(TelemetryError::Io)?;
    }
    Ok(())
}

// ── Connections ──────────────────────────────────────────────────────────

/// Open (or create) the telemetry database and run pending migrations.
pub fn open_or_create(path: &std::path::Path) -> Result<TelemetryDb, TelemetryError> {
    ensure_parent(path)?;
    let mut conn = Connection::open(path)?;
    apply_pragmas(&mut conn);
    migrate(&mut conn)?;
    Ok(TelemetryDb { conn })
}

/// Open an existing telemetry database read-only (no migrations run).
pub fn open_readonly(path: &std::path::Path) -> Result<TelemetryDb, TelemetryError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(TelemetryDb { conn })
}

fn apply_pragmas(conn: &mut Connection) {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .ok();
}

// ── Migrations ───────────────────────────────────────────────────────────

/// Run all pending migrations inside one transaction.
fn migrate(conn: &mut Connection) -> Result<(), TelemetryError> {
    let current: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.transaction()?;

    // v1: initial schema
    if current < 1 {
        tx.execute_batch(
            "CREATE TABLE sources (
                id              INTEGER PRIMARY KEY,
                provider        TEXT NOT NULL,
                canonical_path  TEXT NOT NULL UNIQUE,
                file_identity   TEXT,
                last_offset     INTEGER NOT NULL DEFAULT 0,
                last_size       INTEGER NOT NULL DEFAULT 0,
                last_modified   INTEGER,
                last_imported   INTEGER,
                status          TEXT NOT NULL DEFAULT 'ok',
                error           TEXT
            );

            CREATE TABLE sessions (
                id              INTEGER PRIMARY KEY,
                provider        TEXT NOT NULL,
                external_id     TEXT NOT NULL,
                source_id       INTEGER,
                started_at      INTEGER,
                last_event_at   INTEGER,
                cwd             TEXT,
                project         TEXT,
                model           TEXT,
                cli_version     TEXT,
                archived        INTEGER NOT NULL DEFAULT 0,
                UNIQUE(provider, external_id),
                FOREIGN KEY(source_id) REFERENCES sources(id)
            );

            CREATE TABLE model_calls (
                id                  INTEGER PRIMARY KEY,
                provider            TEXT NOT NULL,
                session_id          INTEGER NOT NULL,
                event_at            INTEGER NOT NULL,
                source_offset       INTEGER NOT NULL,
                model               TEXT,
                context_window      INTEGER,
                input_tokens        INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
                output_tokens       INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens    INTEGER NOT NULL DEFAULT 0,
                total_tokens        INTEGER NOT NULL DEFAULT 0,
                cumulative_tokens   INTEGER,
                UNIQUE(session_id, source_offset),
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );

            CREATE TABLE quota_samples (
                id                  INTEGER PRIMARY KEY,
                provider            TEXT NOT NULL,
                session_id          INTEGER,
                event_at            INTEGER NOT NULL,
                limit_id            TEXT,
                limit_name          TEXT,
                plan_type           TEXT,
                window_kind         TEXT NOT NULL,
                window_minutes      INTEGER,
                used_percent        REAL,
                resets_at           INTEGER,
                has_credits         INTEGER,
                unlimited_credits   INTEGER,
                credit_balance      TEXT,
                source_offset       INTEGER NOT NULL,
                UNIQUE(provider, session_id, source_offset, window_kind),
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );

            CREATE INDEX idx_model_calls_time
                ON model_calls(event_at);

            CREATE INDEX idx_model_calls_session_time
                ON model_calls(session_id, event_at);

            CREATE INDEX idx_quota_provider_window_time
                ON quota_samples(provider, window_kind, event_at);",
        )?;
    }

    // v2: user-authored timeline markers (for example account / plan changes).
    if current < 2 {
        tx.execute_batch(
            "CREATE TABLE notes (
                id          INTEGER PRIMARY KEY,
                event_at    INTEGER NOT NULL,
                provider    TEXT,
                plan_type   TEXT,
                text        TEXT NOT NULL
            );

            CREATE INDEX idx_notes_time ON notes(event_at);",
        )?;
    }

    // v3: models are found in `turn_context`, not `token_count`. Replay the
    // existing JSONL files once so legacy model_calls can be backfilled.
    if current < 3 {
        tx.execute("UPDATE sources SET last_offset = 0, last_size = 0", [])?;
    }

    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

// ── Accessors ────────────────────────────────────────────────────────────

impl TelemetryDb {
    /// Borrow the inner SQLite connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Borrow mutably (needed for transactions).
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Current schema version.
    pub fn schema_version(&self) -> i64 {
        self.conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap_or(0)
    }

    /// Add a user-authored marker to the telemetry timeline.
    pub fn insert_note(
        &self,
        provider: Option<&str>,
        plan_type: Option<&str>,
        text: &str,
    ) -> Result<(), TelemetryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.conn.execute(
            "INSERT INTO notes (event_at, provider, plan_type, text)
             VALUES (?1, ?2, ?3, ?4)",
            params![now, provider, plan_type, text],
        )?;
        Ok(())
    }

    /// Store a live API snapshot into quota_samples.
    ///
    /// Inserts rows with `source_offset = -1` (sentinel) so the dashboard
    /// can query the latest live data alongside historical JSONL samples.
    pub fn store_live_snapshot(
        &self,
        provider: &str,
        window_kind: &str,
        window_minutes: i64,
        used_percent: f64,
        resets_at: Option<i64>,
        plan_type: Option<&str>,
    ) -> Result<(), TelemetryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.conn().execute(
            "INSERT INTO quota_samples
             (provider, session_id, event_at, window_kind, window_minutes,
              used_percent, resets_at, plan_type, source_offset)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, -1)",
            params![
                provider,
                now,
                window_kind,
                window_minutes,
                used_percent,
                resets_at,
                plan_type
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn test_migrate_empty() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&mut conn);
        migrate(&mut conn).unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn test_migrate_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&mut conn);
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap(); // second call must be a no-op
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn test_v3_migration_replays_existing_sources_for_model_backfill() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&mut conn);
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO sources (provider, canonical_path, last_offset, last_size) \
             VALUES ('codex', 'C:/rollout.jsonl', 123, 456)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();

        migrate(&mut conn).unwrap();

        let source: (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_size FROM sources WHERE canonical_path = 'C:/rollout.jsonl'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(source, (0, 0));
    }

    #[test]
    fn test_tables_exist() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&mut conn);
        migrate(&mut conn).unwrap();
        for table in &[
            "sources",
            "sessions",
            "model_calls",
            "quota_samples",
            "notes",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table '{table}' should exist after migration");
        }
    }

    #[test]
    fn test_insert_note() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&mut conn);
        migrate(&mut conn).unwrap();
        let db = TelemetryDb { conn };

        db.insert_note(Some("codex"), Some("plus"), "Switching account")
            .unwrap();

        let row: (String, String, String) = db
            .conn()
            .query_row("SELECT provider, plan_type, text FROM notes", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(
            row,
            ("codex".into(), "plus".into(), "Switching account".into())
        );
    }
}
