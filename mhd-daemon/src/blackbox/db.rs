//! SQLite persistence layer for the blackbox logger.
//!
//! Handles schema migrations, inserts, and batch transaction management.
//! All methods are thin wrappers over `rusqlite::Connection`.

use std::path::Path;

use rusqlite::{Connection, params};

/// Wraps a rusqlite connection with application-specific insert methods.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the SQLite database at `path`.
    ///
    /// Sets performance pragmas and runs any pending migrations.
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path)
            .map_err(|e| format!("cannot open blackbox db '{}': {e}", path.display()))?;

        // Performance / safety pragmas
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;"
        )
        .map_err(|e| format!("cannot set pragmas: {e}"))?;

        migrate(&conn)?;

        Ok(Db { conn })
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("cannot open in-memory db: {e}"))?;

        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;"
        )
        .map_err(|e| format!("cannot set pragmas: {e}"))?;

        migrate(&conn)?;

        Ok(Db { conn })
    }

    // ── Insert helpers ──────────────────────────────────────────────────

    /// Insert an event row, returning the new `rowid`.
    pub fn insert_event(
        &self,
        ts: u64,
        kind: &str,
        app: Option<&str>,
        win: Option<&str>,
        payload: Option<&str>,
    ) -> Result<i64, String> {
        self.conn
            .prepare_cached(
                "INSERT INTO events (ts, kind, app_name, win_title, payload) VALUES (?1, ?2, ?3, ?4, ?5)"
            )
            .and_then(|mut stmt| {
                stmt.insert(params![ts as i64, kind, app, win, payload])
            })
            .map_err(|e| format!("cannot insert event: {e}"))
    }

    /// Insert a session record (referencing an existing event).
    pub fn insert_session(
        &self,
        event_id: i64,
        started_ts: u64,
        duration_sec: u64,
        keyboard: u64,
        mouse: u64,
        end_reason: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions (event_id, started_ts, duration_sec, keyboard, mouse, end_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            )
            .and_then(|mut stmt| {
                stmt.insert(params![event_id, started_ts as i64, duration_sec as i64, keyboard as i64, mouse as i64, end_reason])
                    .map(|_| ())
            })
            .map_err(|e| format!("cannot insert session: {e}"))
    }

    /// Insert a note record (referencing an existing event).
    pub fn insert_note(&self, event_id: i64, text: &str) -> Result<(), String> {
        self.conn
            .prepare_cached("INSERT INTO notes (event_id, text) VALUES (?1, ?2)")
            .and_then(|mut stmt| stmt.insert(params![event_id, text]).map(|_| ()))
            .map_err(|e| format!("cannot insert note: {e}"))
    }

    // ── Transaction helpers ─────────────────────────────────────────────

    pub fn begin(&self) -> Result<(), String> {
        self.conn
            .execute_batch("BEGIN")
            .map_err(|e| format!("cannot begin transaction: {e}"))
    }

    pub fn commit(&self) -> Result<(), String> {
        self.conn
            .execute_batch("COMMIT")
            .map_err(|e| format!("cannot commit transaction: {e}"))
    }

}

// ── Schema migrations ────────────────────────────────────────────────────

const MIGRATIONS: &[&str] = &[
    // Migration 1: initial schema
    "CREATE TABLE IF NOT EXISTS events (
        id        INTEGER PRIMARY KEY,
        ts        INTEGER NOT NULL,
        kind      TEXT    NOT NULL,
        app_name  TEXT,
        win_title TEXT,
        payload   TEXT
    );
    CREATE TABLE IF NOT EXISTS sessions (
        event_id     INTEGER PRIMARY KEY REFERENCES events(id),
        started_ts   INTEGER NOT NULL,
        duration_sec INTEGER NOT NULL,
        keyboard     INTEGER NOT NULL,
        mouse        INTEGER NOT NULL,
        end_reason   TEXT
    );
    CREATE TABLE IF NOT EXISTS notes (
        event_id INTEGER PRIMARY KEY REFERENCES events(id),
        text     TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
    CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
    INSERT INTO schema_version (version) VALUES (1);",
];

fn migrate(conn: &Connection) -> Result<(), String> {
    // Ensure schema_version table exists
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);"
    )
    .map_err(|e| format!("cannot create schema_version table: {e}"))?;

    // Read current version (0 if none)
    let current: i64 = conn
        .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |row| {
            row.get(0)
        })
        .map_err(|e| format!("cannot read schema version: {e}"))?;

    // Apply pending migrations
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let v = (i + 1) as i64;
        if v > current {
            conn.execute_batch(sql)
                .map_err(|e| format!("migration v{v} failed: {e}"))?;
            conn.execute("INSERT INTO schema_version (version) VALUES (?1)", params![v])
                .map_err(|e| format!("cannot bump schema to v{v}: {e}"))?;
        }
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_and_migrate() {
        let db = Db::open_in_memory().unwrap();
        // Verify tables exist by querying schema
        let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"notes".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));
    }

    #[test]
    fn test_insert_event() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_event(1000, "test_event", Some("notepad"), None, Some("k=v")).unwrap();
        assert!(id > 0);

        // Query back
        let (ts, kind): (i64, String) = db.conn
            .query_row("SELECT ts, kind FROM events WHERE id = ?1", params![id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(ts, 1000);
        assert_eq!(kind, "test_event");
    }

    #[test]
    fn test_insert_session() {
        let db = Db::open_in_memory().unwrap();
        let event_id = db.insert_event(2000, "ses_end", None, None, None).unwrap();
        db.insert_session(event_id, 1900, 100, 50, 10, Some("stop")).unwrap();

        // Verify via query
        let (started, dur, k, m): (i64, i64, i64, i64) = db.conn
            .query_row(
                "SELECT started_ts, duration_sec, keyboard, mouse FROM sessions WHERE event_id = ?1",
                params![event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(started, 1900);
        assert_eq!(dur, 100);
        assert_eq!(k, 50);
        assert_eq!(m, 10);
    }

    #[test]
    fn test_insert_session_fk_fails() {
        // Foreign key to non-existent event should fail
        let db = Db::open_in_memory().unwrap();
        let result = db.insert_session(999, 0, 0, 0, 0, None);
        assert!(result.is_err(), "expected FK violation");
    }

    #[test]
    fn test_insert_note() {
        let db = Db::open_in_memory().unwrap();
        let event_id = db.insert_event(3000, "note", None, None, None).unwrap();
        db.insert_note(event_id, "hello world").unwrap();

        let text: String = db.conn
            .query_row("SELECT text FROM notes WHERE event_id = ?1", params![event_id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_migration_idempotent() {
        // Opening twice should not cause errors
        let db = Db::open_in_memory().unwrap();
        let v1: i64 = db.conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v1, 1);

        // Open another connection to the same in-memory DB won't work,
        // but we can verify the schema is already there
        let tables: Vec<String> = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"events".to_string()));
    }

    #[test]
    fn test_batch_commit() {
        let db = Db::open_in_memory().unwrap();
        db.begin().unwrap();
        for i in 0..60u64 {
            db.insert_event(i * 10, "batch", None, None, None).unwrap();
        }
        db.commit().unwrap();

        let count: i64 = db.conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 60);
    }
}
