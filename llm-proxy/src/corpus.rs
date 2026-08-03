//! Per-provider replay-corpus databases.
//!
//! WHY this split exists (a fact a future reader needs and cannot recover from
//! the code): the legacy corpus lives in one `request_bodies` table inside
//! `proxy.db`, capped by a row-count retention that is blind to the `provider`
//! column:
//!
//! ```sql
//! DELETE FROM request_bodies WHERE id NOT IN
//!   (SELECT id FROM request_bodies ORDER BY id DESC LIMIT ?1)
//! ```
//!
//! Measured on the live database at the 5000-row cap: anthropic 4416 rows @
//! ~104 KB, openai 574 rows @ ~154 KB, codex 10 rows @ ~23 KB. A Codex session
//! therefore evicts large anthropic bodies one-for-one with small Codex ones,
//! silently biasing offline trim backtests toward whatever provider was busiest
//! last. The fix is one SQLite file per provider so each gets an independent
//! retention lifetime. `db_log.rs` still owns the legacy table as the source
//! for the one-time migration and for anything written before this split.
//!
//! Each file is self-describing: it carries the same `request_bodies` schema as
//! `proxy.db`, INCLUDING the now-redundant `provider` column, so a file copied
//! out for offline analysis names its own provider and migration is a straight
//! row copy.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// Filename fragment used for any provider that cannot be represented safely.
const SAFE_FALLBACK: &str = "other";

/// The same `request_bodies` table as `db_log.rs`, kept column-for-column so
/// migration is a straight row copy and a copied-out file self-describes.
const SCHEMA: &str = "
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
";

/// Map a provider string onto a safe filename fragment: only `[a-z0-9_-]` is
/// accepted, and anything else — empty, path separators, `..`, uppercase —
/// collapses to [`SAFE_FALLBACK`]. Keeps a stray provider value from ever
/// escaping the config directory.
fn sanitize(provider: &str) -> &str {
    if !provider.is_empty()
        && provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        provider
    } else {
        SAFE_FALLBACK
    }
}

/// `dir/corpus-<provider>.db`, next to `proxy.db`, with `provider` sanitized to
/// `[a-z0-9_-]` (anything else becomes `other`). Always inside `dir` — a hostile
/// provider can never escape it.
pub fn corpus_path(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("corpus-{}.db", sanitize(provider)))
}

/// Open (or create) the per-provider corpus database and create the schema if
/// absent. Fails only when the file cannot be opened or the schema batch does
/// not run — the caller decides how to surface that.
pub fn open_write(dir: &Path, provider: &str) -> rusqlite::Result<Connection> {
    let path = corpus_path(dir, provider);
    let conn = Connection::open(&path)?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Insert one captured pre-trim request body, then prune to `max_rows`
/// (0 = unlimited). `provider` is stored as given — the per-file split already
/// came from the file name; the column stays so each file is self-describing.
///
/// Because the file holds a single provider, the row-count cap is now naturally
/// per-provider: a busy codex session can no longer evict anthropic bodies.
/// Best-effort: errors are swallowed, matching `DbLog::insert_request_body`.
pub fn insert_body(
    conn: &Connection,
    run_id: u64,
    seq: u64,
    ts: &str,
    model: Option<&str>,
    provider: &str,
    body: &str,
    max_rows: usize,
) {
    let compressed = crate::db_log::compress_body(body);
    if compressed.is_empty() {
        return; // compression produced nothing (only on encode error); skip row
    }
    let inserted = conn.execute(
        "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![run_id as i64, seq as i64, ts, model, provider, compressed],
    );
    if inserted.is_ok() && max_rows > 0 {
        let max = max_rows as i64;
        let _ = conn.execute(
            "DELETE FROM request_bodies WHERE id NOT IN \
             (SELECT id FROM request_bodies ORDER BY id DESC LIMIT ?1)",
            rusqlite::params![max],
        );
    }
}

/// Whether `request_bodies` in `conn` holds at least one row. False when the
/// table is missing or empty — a reader treats both identically.
pub fn has_rows(conn: &Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM request_bodies", [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Open the corpus for a provider: the per-provider file when it exists AND
/// holds at least one row, else the legacy `request_bodies` table in
/// `dir/proxy.db` when that still has rows, else `None`.
///
/// **Either/or, never a union.** Every reader orders by `id` (`ORDER BY id`,
/// `ORDER BY run_id, id`) and `id` is a per-file AUTOINCREMENT space. Unioning
/// two files would interleave two independent id sequences and silently produce
/// a wrong chronological order — for a replay corpus that means fabricated
/// conversation chains. The either/or keeps `ORDER BY id` exactly as correct as
/// it is today; a mixed state (some rows migrated, some not) is resolved by
/// running the migration, not by merging at read time.
///
/// Never creates a file as a side effect: existence is checked up front and the
/// connection is opened `SQLITE_OPEN_READ_ONLY`.
pub fn open_read(dir: &Path, provider: &str) -> Option<Connection> {
    let path = corpus_path(dir, provider);
    if path.exists() {
        if let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            && has_rows(&conn)
        {
            return Some(conn);
        }
    }
    let legacy = dir.join("proxy.db");
    if legacy.exists() {
        if let Ok(conn) = Connection::open_with_flags(&legacy, OpenFlags::SQLITE_OPEN_READ_ONLY)
            && has_rows(&conn)
        {
            return Some(conn);
        }
    }
    None
}

/// Rows in `schema.request_bodies` for `provider`. The schema name is always
/// one of this module's own hard-coded literals (`corpus` or `main`), never
/// caller input, so building the SQL with `format!` is safe.
fn provider_row_count(conn: &Connection, schema: &str, provider: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {schema}.request_bodies WHERE provider = ?1"),
        rusqlite::params![provider],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// Whether a schema named `name` is currently attached to `conn`. Used after
/// an ATTACH failure to tell "the schema name is already in use" apart from
/// every other reason ATTACH could have failed.
fn schema_is_attached(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM pragma_database_list WHERE name = ?1",
        rusqlite::params![name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Attaches the corpus for `provider` to an already-open connection (in
/// practice a read-only `proxy.db`) so a query can join `request_bodies`
/// against `requests`, which stays in `proxy.db`.
///
/// Returns the schema qualifier the caller must use for `request_bodies`:
/// `Some("corpus")` when the per-provider file was attached, `Some("main")`
/// when falling back to the legacy table in the connection's own database,
/// `None` when neither holds rows for this provider.
pub fn attach_read(conn: &Connection, dir: &Path, provider: &str) -> Option<&'static str> {
    let path = corpus_path(dir, provider);
    if path.exists() {
        // Check existence before ATTACH: on a missing file SQLite CREATES it,
        // the same trap `open_read` already avoids. That check also removes
        // the only reason to want the `file:...?mode=ro` URI form (accidental
        // creation), and every caller is read-only by construction. These are
        // Windows paths with backslashes, drive letters and possible spaces,
        // so hand-rolling URI percent-encoding would be a bug farm — instead
        // the path is bound as a parameter and never interpolated into SQL.
        let path_arg = path.to_string_lossy();
        let already_attached = match conn.execute(
            "ATTACH DATABASE ?1 AS corpus",
            rusqlite::params![path_arg.as_ref()],
        ) {
            Ok(_) => {
                if provider_row_count(conn, "corpus", provider) > 0 {
                    return Some("corpus");
                }
                // The file exists but holds no rows for this provider: detach
                // so the schema name does not linger. A leftover `corpus`
                // schema would make a second attach_read on this connection
                // report a phantom corpus.
                let _ = conn.execute_batch("DETACH DATABASE corpus");
                false
            }
            Err(_) => {
                // ATTACH can fail because the schema name is already in use by
                // an earlier attach_read on the same connection. Treat that as
                // the corpus already being attached rather than as a miss.
                schema_is_attached(conn, "corpus")
            }
        };
        if already_attached {
            return Some("corpus");
        }
    }
    // Legacy fallback. The table is shared across providers, so it must be
    // checked WITH the provider filter — rows belonging to another provider
    // prove nothing about this one.
    if provider_row_count(conn, "main", provider) > 0 {
        Some("main")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    /// Build a legacy `proxy.db` holding one `request_bodies` row for `provider`.
    /// Mirrors the db_log schema so the fallback reader sees exactly what a real
    /// pre-split database looks like.
    fn legacy_corpus(dir: &Path, provider: &str, run_id: u64) {
        let conn = Connection::open(dir.join("proxy.db")).expect("legacy open");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_bodies (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 seq       INTEGER NOT NULL,
                 ts        TEXT    NOT NULL,
                 model     TEXT,
                 provider  TEXT,
                 body      BLOB    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_request_bodies_run_seq ON request_bodies(run_id, seq);",
        )
        .expect("legacy schema");
        let body = format!(r#"{{"run_id":{run_id}}}"#);
        insert_body(
            &conn,
            run_id,
            1,
            "2026-08-01T00:00:00Z",
            None,
            provider,
            &body,
            0,
        );
    }

    fn count_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM request_bodies", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
    }

    fn insert_n(conn: &Connection, provider: &str, n: u64, max_rows: usize) {
        for i in 0..n {
            let body = format!(r#"{{"seq":{i}}}"#);
            insert_body(
                conn,
                1,
                i,
                "2026-08-01T00:00:00Z",
                None,
                provider,
                &body,
                max_rows,
            );
        }
    }

    #[test]
    fn corpus_path_builds_expected_name_and_sanitizes() {
        assert_eq!(
            corpus_path(Path::new("cfg"), "anthropic"),
            PathBuf::from("cfg/corpus-anthropic.db")
        );
        // A hostile provider can never escape `dir`; anything outside
        // [a-z0-9_-] collapses to the fallback name.
        for hostile in [
            "../../evil",
            "a b",
            "",
            "../x",
            "a/b",
            r"a\b",
            ".",
            "..",
            "a.b",
            "Anthropic",
            "тест",
        ] {
            assert_eq!(
                corpus_path(Path::new("cfg"), hostile),
                PathBuf::from("cfg/corpus-other.db"),
                "hostile provider {hostile:?} must land on the fallback name"
            );
        }
        // Allowed characters survive, including multi-segment slug names.
        assert_eq!(
            corpus_path(Path::new("cfg"), "openai"),
            PathBuf::from("cfg/corpus-openai.db")
        );
        assert_eq!(
            corpus_path(Path::new("cfg"), "deepseek-v4-pro"),
            PathBuf::from("cfg/corpus-deepseek-v4-pro.db")
        );
    }

    #[test]
    fn corpus_open_write_creates_schema_and_round_trips() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let conn = open_write(dir, "anthropic").expect("open_write");
        assert!(
            dir.join("corpus-anthropic.db").exists(),
            "per-provider file must be created"
        );
        let body =
            r#"{"model":"claude-opus-4-5","messages":[{"role":"user","content":"Hello 🌍"}]}"#;
        insert_body(
            &conn,
            7,
            3,
            "2026-08-01T00:00:00Z",
            Some("claude-opus-4-5"),
            "anthropic",
            body,
            0,
        );
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT body FROM request_bodies WHERE run_id = 7 AND seq = 3",
                [],
                |r| r.get(0),
            )
            .expect("row");
        assert_eq!(
            crate::db_log::decompress_body(&stored).as_deref(),
            Some(body),
            "zstd round-trip must be lossless"
        );
        // The now-redundant provider column is stored deliberately: a file
        // copied out for offline analysis names its own provider.
        let stored_provider: String = conn
            .query_row("SELECT provider FROM request_bodies", [], |r| r.get(0))
            .expect("provider");
        assert_eq!(stored_provider, "anthropic");
    }

    #[test]
    fn corpus_retention_is_per_provider_file() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let max_rows = 4usize;

        let anthropic = open_write(dir, "anthropic").expect("anthropic open");
        insert_n(&anthropic, "anthropic", (max_rows + 3) as u64, max_rows);
        assert_eq!(
            count_rows(&anthropic),
            max_rows as i64,
            "anthropic file pruned to its own cap"
        );

        // A second provider's file is untouched: it keeps every row it got,
        // even while the anthropic file sits at its cap.
        let codex = open_write(dir, "codex").expect("codex open");
        insert_n(&codex, "codex", 7, 0);
        assert_eq!(count_rows(&codex), 7, "codex keeps all of its own rows");
        assert_eq!(
            count_rows(&anthropic),
            max_rows as i64,
            "anthropic's cap did not move when codex wrote"
        );

        // The two live in separate files — this is the whole point of the split.
        assert!(dir.join("corpus-anthropic.db").exists());
        assert!(dir.join("corpus-codex.db").exists());
    }

    #[test]
    fn corpus_max_rows_zero_is_unlimited() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let conn = open_write(dir, "openai").expect("openai open");
        insert_n(&conn, "openai", 5, 0);
        assert_eq!(count_rows(&conn), 5, "max_rows 0 keeps every row");
    }

    #[test]
    fn corpus_open_read_prefers_per_provider_then_legacy() {
        let tmp = temp_dir();
        let dir = tmp.path();

        // Nothing anywhere -> None, and no file is created as a side effect.
        assert!(open_read(dir, "anthropic").is_none(), "nothing to read");
        assert!(!dir.join("corpus-anthropic.db").exists());
        assert!(!dir.join("proxy.db").exists());

        // Legacy proxy.db holds the corpus; per-provider file absent -> fallback.
        legacy_corpus(dir, "anthropic", 1);
        {
            let read = open_read(dir, "anthropic").expect("must fall back to legacy proxy.db");
            assert_eq!(
                count_rows(&read),
                1,
                "legacy row visible through the fallback"
            );
        }

        // Per-provider file gains its own rows -> it now wins over legacy.
        let per = open_write(dir, "anthropic").expect("per-provider open");
        insert_body(
            &per,
            2,
            1,
            "2026-08-01T00:00:00Z",
            None,
            "anthropic",
            r#"{"a":2}"#,
            0,
        );
        drop(per);
        {
            let read = open_read(dir, "anthropic").expect("must prefer the per-provider file");
            let run_id: i64 = read
                .query_row("SELECT run_id FROM request_bodies", [], |r| r.get(0))
                .expect("run");
            assert_eq!(run_id, 2, "reads the per-provider file, not the legacy one");
        }
    }

    #[test]
    fn corpus_open_read_empty_per_provider_falls_back_to_legacy() {
        let tmp = temp_dir();
        let dir = tmp.path();
        legacy_corpus(dir, "anthropic", 1);

        // Per-provider file exists (schema created) but holds no rows yet: the
        // reader must not prefer an empty file over the legacy corpus.
        let per = open_write(dir, "anthropic").expect("per-provider open");
        drop(per);
        let read = open_read(dir, "anthropic").expect("empty per-provider must fall back");
        let run_id: i64 = read
            .query_row("SELECT run_id FROM request_bodies", [], |r| r.get(0))
            .expect("run");
        assert_eq!(
            run_id, 1,
            "legacy row served, not the empty per-provider file"
        );
    }

    /// Minimal `requests` metadata table in the main database — the read-side
    /// partner of `request_bodies` that stays in `proxy.db` while the bodies
    /// moved to per-provider files.
    fn requests_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                 run_id INTEGER NOT NULL,
                 seq    INTEGER NOT NULL,
                 model  TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_requests_run_seq ON requests(run_id, seq);",
        )
        .expect("requests schema");
    }

    fn insert_request(conn: &Connection, run_id: u64, seq: u64, model: &str) {
        conn.execute(
            "INSERT INTO requests (run_id, seq, model) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id as i64, seq as i64, model],
        )
        .expect("insert request");
    }

    /// Run the canonical reader JOIN and return `(run_id, seq, model)` rows,
    /// ordered so the single-file and cross-file results are comparable.
    /// `bodies`/`requests` are schema-qualified table names: either plain
    /// (`request_bodies`, `requests`) or attached (`corpus.request_bodies`,
    /// `main.requests`).
    fn join_rows(conn: &Connection, bodies: &str, requests: &str) -> Vec<(i64, i64, String)> {
        let sql = format!(
            "SELECT r.run_id, r.seq, r.model \
             FROM {bodies} rb \
             JOIN {requests} r ON r.run_id = rb.run_id AND r.seq = rb.seq \
             ORDER BY r.run_id, r.seq"
        );
        let mut stmt = conn.prepare(&sql).expect("join prepare");
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("join rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("join collect")
    }

    #[test]
    fn attach_read_prefers_per_provider_file() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let main = Connection::open(dir.join("proxy.db")).expect("proxy open");
        requests_table(&main);
        let per = open_write(dir, "anthropic").expect("per-provider open");
        insert_body(
            &per,
            1,
            1,
            "2026-08-01T00:00:00Z",
            None,
            "anthropic",
            r#"{"a":1}"#,
            0,
        );
        drop(per);

        assert_eq!(attach_read(&main, dir, "anthropic"), Some("corpus"));
        let n: i64 = main
            .query_row("SELECT COUNT(*) FROM corpus.request_bodies", [], |r| {
                r.get(0)
            })
            .expect("attached count");
        assert_eq!(n, 1, "per-provider row reachable via the corpus schema");
    }

    #[test]
    fn attach_read_falls_back_to_legacy_table() {
        let tmp = temp_dir();
        let dir = tmp.path();
        legacy_corpus(dir, "anthropic", 1);
        let main = Connection::open(dir.join("proxy.db")).expect("proxy open");
        assert_eq!(attach_read(&main, dir, "anthropic"), Some("main"));
        assert!(
            !dir.join("corpus-anthropic.db").exists(),
            "no per-provider file should be created for the legacy fallback"
        );
    }

    #[test]
    fn attach_read_none_creates_no_file() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let main = Connection::open(dir.join("proxy.db")).expect("proxy open");
        // An existing but empty table must not read as a corpus.
        main.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_bodies (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 seq       INTEGER NOT NULL,
                 ts        TEXT    NOT NULL,
                 model     TEXT,
                 provider  TEXT,
                 body      BLOB    NOT NULL
             );",
        )
        .expect("empty schema");
        assert_eq!(attach_read(&main, dir, "anthropic"), None);
        assert!(
            !dir.join("corpus-anthropic.db").exists(),
            "attach_read must not create the per-provider file as a side effect"
        );
    }

    #[test]
    fn attach_read_joins_across_files() {
        // Single-file baseline: bodies and requests live in one database.
        let baseline_tmp = temp_dir();
        let baseline =
            Connection::open(baseline_tmp.path().join("proxy.db")).expect("baseline open");
        baseline
            .execute_batch(
                "CREATE TABLE request_bodies (
                     id        INTEGER PRIMARY KEY AUTOINCREMENT,
                     run_id    INTEGER NOT NULL,
                     seq       INTEGER NOT NULL,
                     ts        TEXT    NOT NULL,
                     model     TEXT,
                     provider  TEXT,
                     body      BLOB    NOT NULL
                 );",
            )
            .expect("baseline bodies");
        requests_table(&baseline);
        for (run_id, seq) in [(1, 1), (1, 2), (2, 1)] {
            insert_body(
                &baseline,
                run_id,
                seq,
                "2026-08-01T00:00:00Z",
                None,
                "anthropic",
                &format!(r#"{{"run_id":{run_id},"seq":{seq}}}"#),
                0,
            );
            insert_request(&baseline, run_id, seq, "claude-opus-4-5");
        }
        let baseline_rows = join_rows(&baseline, "request_bodies", "requests");

        // Split-file arrangement: bodies in corpus-anthropic.db, requests in
        // proxy.db. The JOIN must go through the attached `corpus` schema.
        let split_tmp = temp_dir();
        let split_main = Connection::open(split_tmp.path().join("proxy.db")).expect("split main");
        requests_table(&split_main);
        for (run_id, seq) in [(1, 1), (1, 2), (2, 1)] {
            insert_request(&split_main, run_id, seq, "claude-opus-4-5");
        }
        let split_per = open_write(split_tmp.path(), "anthropic").expect("split per-provider");
        for (run_id, seq) in [(1, 1), (1, 2), (2, 1)] {
            insert_body(
                &split_per,
                run_id,
                seq,
                "2026-08-01T00:00:00Z",
                None,
                "anthropic",
                &format!(r#"{{"run_id":{run_id},"seq":{seq}}}"#),
                0,
            );
        }
        drop(split_per);

        assert_eq!(
            attach_read(&split_main, split_tmp.path(), "anthropic"),
            Some("corpus")
        );
        let split_rows = join_rows(&split_main, "corpus.request_bodies", "main.requests");
        assert_eq!(
            split_rows, baseline_rows,
            "cross-file join must match the single-file join"
        );
    }

    #[test]
    fn attach_read_provider_filter_applies_to_legacy() {
        let tmp = temp_dir();
        let dir = tmp.path();
        legacy_corpus(dir, "anthropic", 1);
        let main = Connection::open(dir.join("proxy.db")).expect("proxy open");
        assert_eq!(attach_read(&main, dir, "anthropic"), Some("main"));
        assert_eq!(
            attach_read(&main, dir, "codex"),
            None,
            "legacy rows for another provider must not serve as this provider's corpus"
        );
    }

    #[test]
    fn attach_read_is_idempotent_on_same_connection() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let per = open_write(dir, "anthropic").expect("per-provider open");
        insert_body(
            &per,
            1,
            1,
            "2026-08-01T00:00:00Z",
            None,
            "anthropic",
            r#"{"a":1}"#,
            0,
        );
        drop(per);
        let main = Connection::open(dir.join("proxy.db")).expect("proxy open");
        assert_eq!(attach_read(&main, dir, "anthropic"), Some("corpus"));
        // Second call: the `corpus` schema name is already in use, which must
        // be treated as already-attached rather than as a miss.
        assert_eq!(
            attach_read(&main, dir, "anthropic"),
            Some("corpus"),
            "repeat attach on the same connection must not fail"
        );

        // The empty-file path: DETACH on empty must leave the connection clean
        // enough for a later attach to succeed (no stale schema name).
        let empty_tmp = temp_dir();
        let empty_dir = empty_tmp.path();
        let empty_per = open_write(empty_dir, "codex").expect("empty per-provider");
        drop(empty_per);
        let main2 = Connection::open(empty_dir.join("proxy.db")).expect("proxy open");
        assert_eq!(attach_read(&main2, empty_dir, "codex"), None);
        assert_eq!(
            attach_read(&main2, empty_dir, "codex"),
            None,
            "empty corpus must not linger after DETACH"
        );
    }

    #[test]
    fn attach_read_works_on_read_only_connection() {
        // Production callers hold proxy.db read-only; the ATTACH/DETACH cycle
        // must work without write access to the main file.
        let tmp = temp_dir();
        let dir = tmp.path();
        let per = open_write(dir, "anthropic").expect("per-provider open");
        insert_body(
            &per,
            1,
            1,
            "2026-08-01T00:00:00Z",
            None,
            "anthropic",
            r#"{"a":1}"#,
            0,
        );
        drop(per);
        let _ = Connection::open(dir.join("proxy.db")).expect("create proxy.db");
        let ro =
            Connection::open_with_flags(dir.join("proxy.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("read-only open");
        assert_eq!(attach_read(&ro, dir, "anthropic"), Some("corpus"));
    }
}
