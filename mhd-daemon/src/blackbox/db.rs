//! SQLite persistence layer for the blackbox logger.
//!
//! Handles schema migrations, inserts, and batch transaction management.
//! All methods are thin wrappers over `rusqlite::Connection`.

use std::path::Path;

use rusqlite::{Connection, params};

/// Current schema version. Any mismatch → drop & recreate.
const CURRENT_SCHEMA: i64 = 2;

const SCHEMA_SQL: &str = "
    CREATE TABLE events (
        id        INTEGER PRIMARY KEY,
        ts        INTEGER NOT NULL,
        kind      TEXT    NOT NULL,
        app_name  TEXT,
        win_title TEXT,
        payload   TEXT
    );
    CREATE TABLE sessions (
        event_id     INTEGER PRIMARY KEY REFERENCES events(id),
        started_ts   INTEGER NOT NULL,
        duration_sec INTEGER NOT NULL,
        active_sec   INTEGER NOT NULL,
        keyboard     INTEGER NOT NULL,
        clicks       INTEGER NOT NULL,
        wheel        INTEGER NOT NULL,
        moves        INTEGER NOT NULL,
        end_reason   TEXT
    );
    CREATE TABLE app_spans (
        id               INTEGER PRIMARY KEY,
        session_event_id INTEGER NOT NULL REFERENCES events(id),
        app              TEXT,
        win_title        TEXT,
        started_ts       INTEGER NOT NULL,
        duration_sec     INTEGER NOT NULL,
        keyboard         INTEGER NOT NULL,
        clicks           INTEGER NOT NULL,
        wheel            INTEGER NOT NULL,
        moves            INTEGER NOT NULL
    );
    CREATE TABLE notes (
        event_id INTEGER PRIMARY KEY REFERENCES events(id),
        text     TEXT NOT NULL
    );
    CREATE TABLE app_category (
        app      TEXT PRIMARY KEY,
        category TEXT NOT NULL
    );
    CREATE INDEX events_ts        ON events(ts);
    CREATE INDEX events_kind_ts   ON events(kind, ts);
    CREATE INDEX sessions_started ON sessions(started_ts);
    CREATE INDEX spans_session    ON app_spans(session_event_id);
    CREATE INDEX spans_app        ON app_spans(app);
";

// Seed categories. INSERT OR IGNORE so user edits survive a non-wipe open.
const SEED_CATEGORIES: &[(&str, &str)] = &[
    ("mhd", "work"),
    ("Astra.IDE", "work"),
    ("RustRover", "work"),
    ("Code", "work"),
    ("WindowsTerminal", "work"),
    ("TOTALCMD64", "file"),
    ("explorer", "file"),
    ("zen", "browse"),
    ("msedge", "browse"),
    ("chrome", "browse"),
    ("firefox", "browse"),
    ("Telegram", "comm"),
    ("Discord", "comm"),
    ("KingdomCome", "game"),
];

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
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(|e| format!("cannot set pragmas: {e}"))?;

        migrate(&conn)?;

        Ok(Db { conn })
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn =
            Connection::open_in_memory().map_err(|e| format!("cannot open in-memory db: {e}"))?;

        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;",
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
        active_sec: u64,
        keyboard: u64,
        clicks: u64,
        wheel: u64,
        moves: u64,
        end_reason: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions (event_id, started_ts, duration_sec, active_sec, keyboard, clicks, wheel, moves, end_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            )
            .and_then(|mut stmt| {
                stmt.insert(params![
                    event_id,
                    started_ts as i64,
                    duration_sec as i64,
                    active_sec as i64,
                    keyboard as i64,
                    clicks as i64,
                    wheel as i64,
                    moves as i64,
                    end_reason,
                ])
                .map(|_| ())
            })
            .map_err(|e| format!("cannot insert session: {e}"))
    }

    /// Insert an app-span record.
    pub fn insert_app_span(
        &self,
        session_event_id: i64,
        app: Option<&str>,
        win: Option<&str>,
        started_ts: u64,
        duration_sec: u64,
        keyboard: u64,
        clicks: u64,
        wheel: u64,
        moves: u64,
    ) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT INTO app_spans (session_event_id, app, win_title, started_ts, duration_sec, keyboard, clicks, wheel, moves)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            )
            .and_then(|mut stmt| {
                stmt.insert(params![
                    session_event_id,
                    app,
                    win,
                    started_ts as i64,
                    duration_sec as i64,
                    keyboard as i64,
                    clicks as i64,
                    wheel as i64,
                    moves as i64,
                ])
                .map(|_| ())
            })
            .map_err(|e| format!("cannot insert app_span: {e}"))
    }

    /// Ensure an app has a category row (creates with 'unknown' if missing).
    pub fn ensure_app_category(&self, app: &str) -> Result<(), String> {
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO app_category (app, category) VALUES (?1, 'unknown')",
            )
            .and_then(|mut stmt| stmt.insert(params![app]).map(|_| ()))
            .map_err(|e| format!("cannot ensure app_category: {e}"))
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

// ── Schema migration ─────────────────────────────────────────────────────

fn migrate(conn: &Connection) -> Result<(), String> {
    // 1. Ensure schema_version table exists
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")
        .map_err(|e| format!("cannot create schema_version table: {e}"))?;

    // 2. Read current version (0 if none)
    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("cannot read schema version: {e}"))?;

    // 3. If already at CURRENT_SCHEMA → skip
    if current == CURRENT_SCHEMA {
        return Ok(());
    }

    // 4. Version mismatch → drop everything (old or unknown version)
    conn.execute_batch(
        "DROP TABLE IF EXISTS app_spans;
         DROP TABLE IF EXISTS sessions;
         DROP TABLE IF EXISTS notes;
         DROP TABLE IF EXISTS app_category;
         DROP TABLE IF EXISTS events;
         DELETE FROM schema_version;",
    )
    .map_err(|e| format!("cannot drop old tables: {e}"))?;

    // 5. Create fresh schema + stamp version
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| format!("cannot create schema: {e}"))?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        params![CURRENT_SCHEMA],
    )
    .map_err(|e| format!("cannot stamp schema version: {e}"))?;

    // 6. Seed categories
    for &(app, category) in SEED_CATEGORIES {
        conn.execute(
            "INSERT OR IGNORE INTO app_category (app, category) VALUES (?1, ?2)",
            params![app, category],
        )
        .map_err(|e| format!("cannot seed category for '{app}': {e}"))?;
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
        let tables: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"notes".to_string()));
        assert!(tables.contains(&"app_spans".to_string()));
        assert!(tables.contains(&"app_category".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));
    }

    #[test]
    fn test_insert_event() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .insert_event(1000, "test_event", Some("notepad"), None, Some("k=v"))
            .unwrap();
        assert!(id > 0);

        let (ts, kind): (i64, String) = db
            .conn
            .query_row(
                "SELECT ts, kind FROM events WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ts, 1000);
        assert_eq!(kind, "test_event");
    }

    #[test]
    fn test_insert_session() {
        let db = Db::open_in_memory().unwrap();
        let event_id = db.insert_event(2000, "ses_end", None, None, None).unwrap();
        db.insert_session(event_id, 1900, 100, 90, 50, 8, 2, 40, Some("stop"))
            .unwrap();

        let (started, dur, active, k, c, w, m): (i64, i64, i64, i64, i64, i64, i64) = db.conn
            .query_row(
                "SELECT started_ts, duration_sec, active_sec, keyboard, clicks, wheel, moves FROM sessions WHERE event_id = ?1",
                params![event_id],
                |row| Ok((
                    row.get(0)?, row.get(1)?, row.get(2)?,
                    row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
                )),
            )
            .unwrap();
        assert_eq!(started, 1900);
        assert_eq!(dur, 100);
        assert_eq!(active, 90);
        assert_eq!(k, 50);
        assert_eq!(c, 8);
        assert_eq!(w, 2);
        assert_eq!(m, 40);
    }

    #[test]
    fn test_insert_session_fk_fails() {
        let db = Db::open_in_memory().unwrap();
        let result = db.insert_session(999, 0, 0, 0, 0, 0, 0, 0, None);
        assert!(result.is_err(), "expected FK violation");
    }

    #[test]
    fn test_insert_app_span() {
        let db = Db::open_in_memory().unwrap();
        let eid = db.insert_event(100, "ses_end", None, None, None).unwrap();
        db.insert_app_span(eid, Some("RustRover"), Some("x"), 100, 50, 30, 5, 1, 20)
            .unwrap();

        let (app, keyboard, moves): (Option<String>, i64, i64) = db
            .conn
            .query_row(
                "SELECT app, keyboard, moves FROM app_spans WHERE session_event_id = ?1",
                params![eid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(app.as_deref(), Some("RustRover"));
        assert_eq!(keyboard, 30);
        assert_eq!(moves, 20);
    }

    #[test]
    fn test_app_category_seeded() {
        let db = Db::open_in_memory().unwrap();
        let cat: String = db
            .conn
            .query_row(
                "SELECT category FROM app_category WHERE app = ?1",
                params!["RustRover"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cat, "work");
    }

    #[test]
    fn test_migration_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let v1: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(v1, CURRENT_SCHEMA);

        let tables: Vec<String> = db
            .conn
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

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 60);
    }
}
