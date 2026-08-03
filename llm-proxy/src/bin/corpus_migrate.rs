//! corpus_migrate — one-shot migration of the legacy `request_bodies` corpus
//! out of `proxy.db` and into the per-provider `corpus-<provider>.db` files.
//!
//! Usage:
//!   corpus_migrate [--db <path>] [--dry-run] [--drop-legacy]
//!
//! START WITH --dry-run: it reports exactly what would move (rows and bytes per
//! provider) and changes nothing. That is the safe first step — run it before
//! anything else.
//!
//! The default run copies every row from `main.request_bodies` in `--db` into
//! `corpus-<provider>.db` files in the same directory, grouped by the row's
//! `provider` column. SQL NULL and any provider string that cannot be a safe
//! filename land in `corpus-other.db`, mirroring `corpus::corpus_path`. The
//! `body` BLOB is copied verbatim: it is already zstd-compressed and is never
//! decompressed or recompressed.
//!
//! Rows are identified by `(run_id, seq)`. A row whose `(run_id, seq)` already
//! exists in the target file is skipped, so running twice never duplicates and
//! re-running after a partial migration (or against files the live proxy
//! already wrote to) is safe.
//!
//! `--drop-legacy` is the destructive step, always opt-in and never implicit.
//! It runs only after the copy succeeds: it re-counts per provider and refuses
//! to drop unless every legacy row is accounted for in the new files, printing
//! the mismatch and exiting non-zero on any gap. When verified it drops
//! `request_bodies` and VACUUMS.
//!
//! Exit code is 0 on success, non-zero on any failure or refused drop, so the
//! tool is safe to drive from a script.

use llm_proxy::config::{Settings, config_dir, load_settings};
use llm_proxy::corpus;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Rows per transaction during the copy. Batching keeps SQLite's page cache
/// bounded (one batch of ~100 KB blobs at a time) and never loads the whole
/// ~530 MB corpus into memory.
const BATCH: usize = 500;

fn arg(flag: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == flag)
        .and_then(|i| a.get(i + 1).cloned())
}
fn has(flag: &str) -> bool {
    std::env::args().any(|x| x == flag)
}

/// Print an error to stderr and exit 1.
fn die(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

/// Compact byte formatting (2.3 KB, 4.1 MB, ...).
fn fmt_bytes(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1} GB", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1e3)
    } else {
        format!("{n} B")
    }
}

/// Display label for a provider value: SQL NULL becomes "(null)" so the report
/// never prints an empty slot.
fn provider_label(p: Option<&str>) -> &str {
    p.unwrap_or("(null)")
}

/// One provider's share of the legacy corpus, per the summary query.
struct ProviderGroup {
    /// Raw `provider` column value; `None` means SQL NULL (grouped into
    /// `corpus-other.db`, same as an unrepresentable string).
    provider: Option<String>,
    rows: i64,
    bytes: i64,
}

/// Copy outcome for one provider.
struct ProviderStats {
    /// Rows written this run (0 when everything was already present).
    inserted: u64,
    /// Rows already present in the target, skipped to keep the run idempotent.
    skipped: u64,
    /// Compressed bytes actually written this run.
    bytes: u64,
}

/// Verification outcome for one provider.
struct VerifyResult {
    legacy_rows: i64,
    target_rows: i64,
    /// Legacy `(run_id, seq)` pairs with no match in the target file.
    missing: i64,
}

impl VerifyResult {
    /// Verified iff every legacy row is accounted for in the target.
    fn ok(&self) -> bool {
        self.missing == 0
    }
}

/// Per-provider summary of the legacy table: rows and compressed bytes, one
/// entry per distinct `provider` value (SQL NULL included).
fn provider_groups(conn: &Connection) -> Result<Vec<ProviderGroup>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT provider, COUNT(*), COALESCE(SUM(LENGTH(body)), 0) \
             FROM request_bodies GROUP BY provider ORDER BY COUNT(*) DESC",
        )
        .map_err(|e| format!("provider summary prepare: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ProviderGroup {
                provider: r.get::<_, Option<String>>(0)?,
                rows: r.get::<_, i64>(1)?,
                bytes: r.get::<_, i64>(2)?,
            })
        })
        .map_err(|e| format!("provider summary query: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("provider summary row: {e}"))
}

/// Copy one provider's legacy rows into its per-provider file, in transactions
/// of [`BATCH`] rows.
///
/// The legacy `body` column is a zstd BLOB and is copied verbatim — never
/// decompressed and recompressed, which could change the bytes and would only
/// waste time. `corpus::insert_body` is deliberately NOT used here: it prunes
/// to the configured cap on every insert and would silently eat rows
/// mid-migration. Rows whose `(run_id, seq)` already exists in the target are
/// skipped, so a re-run never duplicates.
fn migrate_provider(
    legacy: &Connection,
    dir: &Path,
    provider: Option<&str>,
) -> Result<ProviderStats, String> {
    let key = provider.unwrap_or("");
    let path = corpus::corpus_path(dir, key);
    let mut target =
        corpus::open_write(dir, key).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut stmt = legacy
        .prepare(
            "SELECT run_id, seq, ts, model, body FROM request_bodies \
             WHERE provider IS ?1 ORDER BY id",
        )
        .map_err(|e| format!("legacy prepare: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![provider])
        .map_err(|e| format!("legacy query: {e}"))?;

    let mut inserted = 0u64;
    let mut skipped = 0u64;
    let mut bytes = 0u64;

    loop {
        let tx = target
            .transaction()
            .map_err(|e| format!("begin transaction: {e}"))?;
        let mut n = 0usize;
        while n < BATCH {
            let row = match rows.next().map_err(|e| format!("legacy read: {e}"))? {
                Some(row) => row,
                None => break,
            };
            let run_id: i64 = row.get(0).map_err(|e| format!("run_id: {e}"))?;
            let seq: i64 = row.get(1).map_err(|e| format!("seq: {e}"))?;
            let ts: String = row.get(2).map_err(|e| format!("ts: {e}"))?;
            let model: Option<String> = row.get(3).map_err(|e| format!("model: {e}"))?;
            let body: Vec<u8> = row.get(4).map_err(|e| format!("body: {e}"))?;
            let present: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM request_bodies WHERE run_id = ?1 AND seq = ?2)",
                    rusqlite::params![run_id, seq],
                    |r| r.get(0),
                )
                .map_err(|e| format!("exists check: {e}"))?;
            if present {
                skipped += 1;
            } else {
                tx.execute(
                    "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![run_id, seq, ts, model, provider, body],
                )
                .map_err(|e| format!("insert: {e}"))?;
                inserted += 1;
                bytes += body.len() as u64;
            }
            n += 1;
        }
        if n == 0 {
            break; // tx dropped => empty transaction rolled back
        }
        tx.commit().map_err(|e| format!("commit: {e}"))?;
    }
    Ok(ProviderStats {
        inserted,
        skipped,
        bytes,
    })
}

/// Re-count one provider's legacy rows and check that every `(run_id, seq)` is
/// present in the target file. `missing == 0` is the only acceptable verdict
/// before a drop.
fn verify_provider(
    legacy: &Connection,
    dir: &Path,
    provider: Option<&str>,
) -> Result<VerifyResult, String> {
    let key = provider.unwrap_or("");
    let path = corpus::corpus_path(dir, key);
    if !path.exists() {
        return Err(format!(
            "target file {} does not exist — the copy for this provider did not run",
            path.display()
        ));
    }
    let mut target =
        Connection::open(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let legacy_rows: i64 = legacy
        .query_row(
            "SELECT COUNT(*) FROM request_bodies WHERE provider IS ?1",
            rusqlite::params![provider],
            |r| r.get(0),
        )
        .map_err(|e| format!("re-count legacy: {e}"))?;

    // Stage the legacy identity pairs in a per-connection temp table, then
    // LEFT JOIN against the target: rows with no match are the missing ones.
    target
        .execute_batch(
            "CREATE TEMP TABLE legacy_ids (run_id INTEGER NOT NULL, seq INTEGER NOT NULL)",
        )
        .map_err(|e| format!("temp table: {e}"))?;
    let mut stmt = legacy
        .prepare("SELECT run_id, seq FROM request_bodies WHERE provider IS ?1")
        .map_err(|e| format!("legacy prepare: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![provider], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("legacy query: {e}"))?;
    {
        let tx = target
            .transaction()
            .map_err(|e| format!("begin transaction: {e}"))?;
        for row in rows {
            let (run_id, seq) = row.map_err(|e| format!("legacy row: {e}"))?;
            tx.execute(
                "INSERT INTO temp.legacy_ids (run_id, seq) VALUES (?1, ?2)",
                rusqlite::params![run_id, seq],
            )
            .map_err(|e| format!("temp insert: {e}"))?;
        }
        tx.commit().map_err(|e| format!("commit: {e}"))?;
    }

    let missing: i64 = target
        .query_row(
            "SELECT COUNT(*) FROM temp.legacy_ids li \
             LEFT JOIN request_bodies r ON r.run_id = li.run_id AND r.seq = li.seq \
             WHERE r.id IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("missing count: {e}"))?;
    let target_rows: i64 = target
        .query_row("SELECT COUNT(*) FROM request_bodies", [], |r| r.get(0))
        .map_err(|e| format!("target count: {e}"))?;

    Ok(VerifyResult {
        legacy_rows,
        target_rows,
        missing,
    })
}

/// Verify every provider group, printing what was checked. Returns Err (so the
/// legacy table is NOT dropped) when any row is missing.
fn verify_all(legacy: &Connection, dir: &Path) -> Result<(), String> {
    let groups = provider_groups(legacy)?;
    println!("Verifying every legacy row is accounted for in the new files:");
    let mut ok = true;
    for g in &groups {
        let res = verify_provider(legacy, dir, g.provider.as_deref())?;
        println!(
            "  {}: {} legacy rows, {} in {}, {} missing  [{}]",
            provider_label(g.provider.as_deref()),
            res.legacy_rows,
            res.target_rows,
            corpus::corpus_path(dir, g.provider.as_deref().unwrap_or("")).display(),
            res.missing,
            if res.ok() { "ok" } else { "MISSING" },
        );
        ok &= res.ok();
    }
    if !ok {
        return Err(
            "verification failed: some legacy rows are missing from the per-provider files; \
             the legacy table was NOT dropped"
                .to_string(),
        );
    }
    Ok(())
}

/// Drop the legacy table and VACUUM. Only ever called after a verified copy.
fn drop_table(legacy: &Connection, db_path: &Path) -> Result<(), String> {
    legacy
        .execute_batch("DROP TABLE request_bodies")
        .map_err(|e| format!("DROP TABLE request_bodies: {e}"))?;
    legacy
        .execute_batch("VACUUM")
        .map_err(|e| format!("VACUUM: {e}"))?;
    println!("Dropped request_bodies and VACUUMed {}.", db_path.display());
    Ok(())
}

/// The configured `corpus_max_rows` cap (0 = unlimited), read from
/// settings.json. Falls back to the documented default when the file is absent
/// — exactly what the live proxy does on first boot — rather than inventing a
/// number for the warning.
fn corpus_cap() -> usize {
    load_settings()
        .map(|s| s.corpus_max_rows)
        .unwrap_or_else(|_| Settings::default().corpus_max_rows)
}

/// Warn when a provider has more legacy rows than the configured cap. The
/// migration deliberately moves them ALL — the point is to preserve what
/// exists — but the next live capture into that file will prune it back down
/// to the cap. `cap == 0` means unlimited and silences this entirely.
fn warn_caps(groups: &[ProviderGroup], cap: usize, dir: &Path) {
    if cap == 0 {
        return;
    }
    for g in groups {
        if g.rows > cap as i64 {
            println!(
                "WARNING: {} has {} legacy rows, above the configured corpus_max_rows ({}).",
                provider_label(g.provider.as_deref()),
                g.rows,
                cap
            );
            println!("         This migration moves them ALL — nothing is dropped — but the next");
            println!(
                "         live capture into {} will prune that file back down to the cap.",
                corpus::corpus_path(dir, g.provider.as_deref().unwrap_or("")).display()
            );
        }
    }
}

fn run(db_path: &Path, dry_run: bool, drop_legacy: bool) -> Result<(), String> {
    let start = Instant::now();
    // The per-provider corpus files live next to proxy.db; a bare filename
    // with no parent falls back to the working directory, same as DbLog::open.
    let dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Read-only for a plain copy; read-write only when the user opted into the
    // destructive drop that needs DROP TABLE + VACUUM.
    let legacy = if drop_legacy {
        Connection::open(db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))?
    } else {
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("cannot open {}: {e}", db_path.display()))?
    };

    match legacy.query_row(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'request_bodies'",
        [],
        |r| r.get::<_, String>(0),
    ) {
        Ok(_) => {}
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            println!(
                "No legacy request_bodies table in {} — nothing to migrate.",
                db_path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(format!("cannot inspect {}: {e}", db_path.display())),
    }

    let groups = provider_groups(&legacy)?;
    let total_rows: i64 = groups.iter().map(|g| g.rows).sum();
    let total_bytes: i64 = groups.iter().map(|g| g.bytes).sum();

    if total_rows == 0 {
        println!(
            "The legacy request_bodies table in {} is empty — nothing to migrate.",
            db_path.display()
        );
        if drop_legacy {
            drop_table(&legacy, db_path)?;
        } else {
            println!("The legacy table is still present.");
        }
        return Ok(());
    }

    if dry_run {
        println!(
            "Dry run — no changes. Would move {} rows ({} compressed) from {} into:",
            total_rows,
            fmt_bytes(total_bytes as u64),
            db_path.display()
        );
        println!();
        println!("  {:<12} {:>6} {:>9}  target", "provider", "rows", "bytes");
        for g in &groups {
            println!(
                "  {:<12} {:>6} {:>9}  {}",
                provider_label(g.provider.as_deref()),
                g.rows,
                fmt_bytes(g.bytes as u64),
                corpus::corpus_path(&dir, g.provider.as_deref().unwrap_or("")).display()
            );
        }
        println!();
        warn_caps(&groups, corpus_cap(), &dir);
        println!("Nothing was changed. Re-run without --dry-run to copy; add --drop-legacy");
        println!("to remove the legacy table once the copy is verified.");
        return Ok(());
    }

    println!(
        "Migrating {} rows ({} compressed) from {} into per-provider files:",
        total_rows,
        fmt_bytes(total_bytes as u64),
        db_path.display()
    );
    println!();
    println!(
        "  {:<12} {:>6} {:>9} {:>7} {:>8}  target",
        "provider", "rows", "bytes", "copied", "skipped"
    );
    let mut total_copied = 0u64;
    let mut total_skipped = 0u64;
    let mut total_moved_bytes = 0u64;
    for g in &groups {
        let label = provider_label(g.provider.as_deref());
        let stats = migrate_provider(&legacy, &dir, g.provider.as_deref())
            .map_err(|e| format!("{label}: {e}"))?;
        println!(
            "  {:<12} {:>6} {:>9} {:>7} {:>8}  {}",
            label,
            g.rows,
            fmt_bytes(g.bytes as u64),
            stats.inserted,
            stats.skipped,
            corpus::corpus_path(&dir, g.provider.as_deref().unwrap_or("")).display()
        );
        total_copied += stats.inserted;
        total_skipped += stats.skipped;
        total_moved_bytes += stats.bytes;
    }
    println!(
        "  {:<12} {:>6} {:>9} {:>7} {:>8}",
        "TOTAL",
        total_rows,
        fmt_bytes(total_bytes as u64),
        total_copied,
        total_skipped,
    );
    println!();
    warn_caps(&groups, corpus_cap(), &dir);
    println!(
        "Moved {} rows ({} compressed) in {:.1}s.",
        total_copied,
        fmt_bytes(total_moved_bytes),
        start.elapsed().as_secs_f64()
    );

    if drop_legacy {
        verify_all(&legacy, &dir)?;
        drop_table(&legacy, db_path)?;
        println!("The legacy corpus is gone.");
    } else {
        println!(
            "The legacy request_bodies table in {} is still present.",
            db_path.display()
        );
        println!(
            "Remove it after checking the new files with: corpus_migrate --db {} --drop-legacy",
            db_path.display()
        );
    }
    Ok(())
}

fn main() {
    // Validate flags up front: a typo'd flag must fail loudly rather than
    // silently behave like the no-op default — the destructive path is opt-in.
    let a: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--db" => i += 1, // its value follows; checked below
            "--dry-run" | "--drop-legacy" => {}
            other => die(&format!("unknown argument: {other}")),
        }
        i += 1;
    }

    let db_path = if has("--db") {
        PathBuf::from(arg("--db").unwrap_or_else(|| die("--db requires a value")))
    } else {
        config_dir().join("proxy.db")
    };
    let dry_run = has("--dry-run");
    let drop_legacy = has("--drop-legacy");

    if let Err(e) = run(&db_path, dry_run, drop_legacy) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn temp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    /// A legacy `proxy.db` with the pre-split `request_bodies` schema, in a
    /// temp directory — never the user's real config dir.
    fn legacy_conn(dir: &Path) -> Connection {
        let conn = Connection::open(dir.join("proxy.db")).expect("open legacy");
        conn.execute_batch(
            "CREATE TABLE request_bodies (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 seq       INTEGER NOT NULL,
                 ts        TEXT    NOT NULL,
                 model     TEXT,
                 provider  TEXT,
                 body      BLOB    NOT NULL
             );
             CREATE INDEX idx_request_bodies_run_seq ON request_bodies(run_id, seq);",
        )
        .expect("legacy schema");
        conn
    }

    fn insert_row(conn: &Connection, run_id: i64, seq: i64, provider: &str, body: &[u8]) {
        conn.execute(
            "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body)
             VALUES (?1, ?2, '2026-08-01T00:00:00Z', NULL, ?3, ?4)",
            rusqlite::params![run_id, seq, provider, body],
        )
        .expect("insert legacy row");
    }

    fn count_in(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM request_bodies", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
    }

    #[test]
    fn migrate_copies_blobs_byte_for_byte() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        // Deliberately non-UTF8: the body must survive as raw bytes.
        let blob: Vec<u8> = vec![0x00, 0xde, 0xad, 0xbe, 0xef, 0xff, 0x00, 0x9c, 0x7f];
        insert_row(&legacy, 1, 1, "anthropic", &blob);

        let stats = migrate_provider(&legacy, dir, Some("anthropic")).expect("migrate");
        assert_eq!(stats.inserted, 1);
        assert_eq!(stats.bytes, blob.len() as u64);

        let target = Connection::open(dir.join("corpus-anthropic.db")).expect("open target");
        let stored: Vec<u8> = target
            .query_row("SELECT body FROM request_bodies", [], |r| r.get(0))
            .expect("row");
        assert_eq!(stored, blob, "blob must survive verbatim");
        drop(target);
    }

    #[test]
    fn migrate_is_idempotent() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        insert_row(&legacy, 1, 1, "anthropic", b"a");
        insert_row(&legacy, 1, 2, "anthropic", b"b");
        insert_row(&legacy, 2, 1, "openai", b"c");

        let s1 = migrate_provider(&legacy, dir, Some("anthropic")).expect("first migrate");
        assert_eq!(s1.inserted, 2);
        assert_eq!(s1.skipped, 0);

        let s2 = migrate_provider(&legacy, dir, Some("anthropic")).expect("second migrate");
        assert_eq!(s2.inserted, 0, "second run must not duplicate");
        assert_eq!(s2.skipped, 2, "both rows already present");

        let target = Connection::open(dir.join("corpus-anthropic.db")).expect("open target");
        assert_eq!(count_in(&target), 2, "no duplicates after re-run");
        drop(target);
    }

    #[test]
    fn provider_groups_aggregate_rows_and_bytes() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        insert_row(&legacy, 1, 1, "anthropic", b"12345");
        insert_row(&legacy, 1, 2, "anthropic", b"123");
        insert_row(&legacy, 2, 1, "openai", b"1234567");

        let groups = provider_groups(&legacy).expect("groups");
        assert_eq!(groups.len(), 2);
        let anthropic = groups
            .iter()
            .find(|g| g.provider.as_deref() == Some("anthropic"))
            .expect("anthropic group");
        assert_eq!(anthropic.rows, 2);
        assert_eq!(anthropic.bytes, 8);
        let openai = groups
            .iter()
            .find(|g| g.provider.as_deref() == Some("openai"))
            .expect("openai group");
        assert_eq!(openai.rows, 1);
        assert_eq!(openai.bytes, 7);
    }

    #[test]
    fn verify_passes_when_every_row_is_present() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        insert_row(&legacy, 1, 1, "anthropic", b"a");
        insert_row(&legacy, 1, 2, "anthropic", b"b");
        migrate_provider(&legacy, dir, Some("anthropic")).expect("migrate");

        let res = verify_provider(&legacy, dir, Some("anthropic")).expect("verify");
        assert!(res.ok(), "counts match: missing {}", res.missing);
        assert_eq!(res.legacy_rows, 2);
        assert_eq!(res.target_rows, 2);
    }

    #[test]
    fn verify_detects_a_missing_row() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        insert_row(&legacy, 1, 1, "anthropic", b"a");
        insert_row(&legacy, 1, 2, "anthropic", b"b");
        migrate_provider(&legacy, dir, Some("anthropic")).expect("migrate");
        // Sabotage the target so the counts diverge: one row vanishes.
        let target = Connection::open(dir.join("corpus-anthropic.db")).expect("open target");
        target
            .execute("DELETE FROM request_bodies WHERE seq = 2", [])
            .expect("sabotage");
        drop(target);

        let res = verify_provider(&legacy, dir, Some("anthropic")).expect("verify");
        assert!(!res.ok(), "counts differ -> must refuse to drop");
        assert_eq!(res.missing, 1);
    }

    #[test]
    fn verify_ok_is_counts_match() {
        assert!(
            VerifyResult {
                legacy_rows: 3,
                target_rows: 3,
                missing: 0,
            }
            .ok()
        );
        assert!(
            !VerifyResult {
                legacy_rows: 3,
                target_rows: 2,
                missing: 1,
            }
            .ok()
        );
    }

    #[test]
    fn migrate_sends_null_provider_to_other_file() {
        let tmp = temp();
        let dir = tmp.path();
        let legacy = legacy_conn(dir);
        legacy
            .execute(
                "INSERT INTO request_bodies (run_id, seq, ts, model, provider, body)
                 VALUES (1, 1, '2026-08-01T00:00:00Z', NULL, NULL, x'deadbeef')",
                [],
            )
            .expect("insert null-provider row");

        let stats = migrate_provider(&legacy, dir, None).expect("migrate null provider");
        assert_eq!(stats.inserted, 1);
        assert!(
            dir.join("corpus-other.db").exists(),
            "NULL provider must land in corpus-other.db"
        );
        let target = Connection::open(dir.join("corpus-other.db")).expect("open target");
        assert_eq!(count_in(&target), 1);
        drop(target);
    }
}
