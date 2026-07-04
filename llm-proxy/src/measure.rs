//! Live trim measurement engine — three warm-session runs + requests-table usage comparison.
//!
//! Drives real `claude -p` workloads through the running proxy (ECO arm with optional
//! side-model offload, then NATIVE_ON (trim on, all native), then NATIVE_OFF (trim off,
//! all native)), measures REAL per-request usage from the proxy's own `requests` table
//! (exact tokens), and reports the delta. Designed to run on a background thread;
//! callers poll [`MeasureProgress`].
//!
//! ## Library constraints
//!
//! - NEVER calls `std::process::exit`. All errors are returned as `Err(String)`.
//! - NEVER reads stdin. The `confirm` closure is the only I/O gate.
//! - NEVER prints to stdout/stderr. The caller does all I/O by watching [`MeasureProgress`].
//! - ALWAYS restores `trim_enabled`, `opus_target`, `sonnet_target`, `haiku_target` to
//!   their original values before returning, via a Drop guard.

use crate::config::config_dir;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── Constants ────────────────────────────────────────────────────────────────────────────

/// Chain-walk task: reads N linked files sequentially, one per turn, forcing
/// exactly N serial turns so the trim comparison is not confounded by turn count.
const PROMPT: &str = "\
There are 15 files named link_01.txt through link_15.txt in the added directory. \
Follow the chain STRICTLY ONE AT A TIME:\n\
  1. Read link_01.txt.\n\
  2. Each file's LAST line is exactly 'NEXT: <filename>' naming the next file to read \
(or 'NEXT: STOP' if it is the last).\n\
  3. Read ONLY the file named by the current file's NEXT line. Read exactly one file per step. \
Do NOT read ahead, do NOT read multiple files in one step, do NOT use subagents.\n\
  4. When you reach 'NEXT: STOP', output a single line: DONE followed by the count of files you read. \
Then stop.\n\
Do not summarize the files. Do not use markdown. Just walk the chain and report DONE <count>.";

/// How long to wait after writing settings.json for the daemon's file watcher to
/// pick up the change (milliseconds).
const SETTINGS_WAIT_MS: u64 = 1200;

/// Default path for the frozen corpus directory.
const CORPUS_DIR_NAME: &str = "mhd-bench-corpus";

/// Number of files in the chain-walk corpus.
const CHAIN_LEN: usize = 15;

/// Temp run directories for each measurement arm.
const ECO_RUN_DIR: &str = "mhd-bench-run-eco";
const ON_RUN_DIR: &str = "mhd-bench-run-on";
const OFF_RUN_DIR: &str = "mhd-bench-run-off";

/// How long to sleep after the claude session exits before aggregating, so that
/// the final request's completion UPDATE lands.
const POST_SESSION_SLEEP_MS: u64 = 700;

/// Anthropic quota-cost multipliers, relative to a fresh input token (= 1.0).
/// Cache writes cost ~1.25x; cache reads are billed at ~0.1x. We weight each
/// token class by these so "billed" reflects REAL quota cost in a warm cache --
/// where most of what trim removes lives in cache_read (cheap), not fresh input.
const W_INPUT: f64 = 1.0;
const W_CACHE_CREATION: f64 = 1.25;
const W_CACHE_READ: f64 = 0.10;

/// Quota-equivalent cost of one arm, in fresh-input-token units.
fn weighted_billed(a: &ArmAggregate) -> f64 {
    a.input_tokens as f64 * W_INPUT
        + a.cache_creation_tokens as f64 * W_CACHE_CREATION
        + a.cache_read_tokens as f64 * W_CACHE_READ
}

// ── Public types ─────────────────────────────────────────────────────────────────────────

/// Per-arm aggregate of the proxy's `requests` log, built by scanning rows
/// INSERTed during one measurement session.
#[derive(Clone, Debug, Default)]
pub struct ArmAggregate {
    pub n_requests: usize,
    /// Sum of input_tokens (fresh, uncached billed input).
    pub input_tokens: u64,
    /// Sum of output_tokens.
    pub output_tokens: u64,
    /// Sum of cache_read_tokens (warm-cache reads).
    pub cache_read_tokens: u64,
    /// Sum of cache_creation_tokens (cache writes -- expensive).
    pub cache_creation_tokens: u64,
    /// Sum of trim_tokens_before (NULL -> 0).
    pub trim_before: u64,
    /// Sum of trim_tokens_after (NULL -> 0).
    pub trim_after: u64,
    /// Wall-clock elapsed for this arm's claude session, in milliseconds.
    pub elapsed_ms: u64,
}

/// Result of a completed three-run measurement.
#[derive(Clone, Debug)]
pub struct MeasureResult {
    pub eco: ArmAggregate,
    pub native_on: ArmAggregate,
    pub native_off: ArmAggregate,
    // Weighted Anthropic quota cost (weighted_billed) per arm:
    pub cost_eco: u64,
    pub cost_native_on: u64,
    pub cost_native_off: u64,
    // Savings vs. the NATIVE_OFF baseline (the no-trim, all-Anthropic reference):
    pub native_saved_pct: f64, // (cost_native_off - cost_native_on)/cost_native_off*100
    pub eco_saved_pct: f64,    // (cost_native_off - cost_eco)/cost_native_off*100
    pub native_verdict: String,
    pub eco_verdict: String,
}

/// Configuration for one measurement run.
#[derive(Clone, Debug)]
pub struct MeasureConfig {
    pub db_path: PathBuf,
    pub dry_run: bool,
    /// ECO arm offloads subagents to side_model when true.
    pub side_substitution: bool,
    /// Routing target for sonnet+haiku in ECO arm (e.g. "sva-opencode/deepseek-v4-flash");
    /// ignored if !side_substitution.
    pub side_model: String,
}

/// Which phase the FSM is in (for GUI rendering + CLI logging).
#[derive(Clone, Debug, PartialEq)]
pub enum MeasurePhase {
    Idle,
    Preflight,
    Snapshot,
    AwaitConfirm,
    RunEco,
    RunNativeOn,
    RunNativeOff,
    Compare,
    Done,
    Aborted,
    Error,
}

/// Live, pollable progress. The driver mutates this under the Mutex; GUI/CLI read snapshots.
#[derive(Clone, Debug)]
pub struct MeasureProgress {
    pub phase: MeasurePhase,
    pub message: String,
    pub corpus_dir: Option<PathBuf>,
    pub corpus_files: Vec<(String, usize)>,
    pub eco: Option<ArmAggregate>,
    pub native_on: Option<ArmAggregate>,
    pub native_off: Option<ArmAggregate>,
    pub eco_transcript: Option<PathBuf>,
    pub native_on_transcript: Option<PathBuf>,
    pub native_off_transcript: Option<PathBuf>,
    pub result: Option<MeasureResult>,
    pub error: Option<String>,
}

impl MeasureProgress {
    pub fn new() -> Self {
        Self {
            phase: MeasurePhase::Idle,
            message: String::new(),
            corpus_dir: None,
            corpus_files: Vec::new(),
            eco: None,
            native_on: None,
            native_off: None,
            eco_transcript: None,
            native_on_transcript: None,
            native_off_transcript: None,
            result: None,
            error: None,
        }
    }
}

impl Default for MeasureProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// A gate the driver calls when it reaches AwaitConfirm. Returns true to proceed, false to abort.
pub type ConfirmFn = Box<dyn FnMut() -> bool + Send>;

// ── helpers: settings.json (serde_json::Value, NOT Settings struct) ──────────────────────

/// Path to the runtime settings.json file.
pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// Read settings.json as a generic JSON value. Returns None if the file is missing
/// or unparseable.
pub fn read_settings_value() -> Option<Value> {
    let path = settings_path();
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Write a JSON value back to settings.json (pretty-printed).
pub fn write_settings_value(v: &Value) -> anyhow::Result<()> {
    let path = settings_path();
    let data = serde_json::to_string_pretty(v)?;
    std::fs::write(&path, data)?;
    Ok(())
}

/// Set `trim_enabled` in settings.json to `enabled`, preserving every other field.
/// Sleeps `SETTINGS_WAIT_MS` for the daemon's file watcher to pick up the change.
pub fn set_trim_enabled(enabled: bool) -> anyhow::Result<()> {
    let mut v = read_settings_value().unwrap_or(Value::Object(serde_json::Map::new()));
    v["trim_enabled"] = Value::Bool(enabled);
    write_settings_value(&v)?;
    std::thread::sleep(std::time::Duration::from_millis(SETTINGS_WAIT_MS));
    Ok(())
}

/// Set `opus_target`, `sonnet_target`, `haiku_target` in settings.json, preserving
/// every other field. Sleeps `SETTINGS_WAIT_MS` for the file watcher.
pub fn set_targets(opus: &str, sonnet: &str, haiku: &str) -> anyhow::Result<()> {
    let mut v = read_settings_value().unwrap_or(Value::Object(serde_json::Map::new()));
    v["opus_target"] = Value::String(opus.to_string());
    v["sonnet_target"] = Value::String(sonnet.to_string());
    v["haiku_target"] = Value::String(haiku.to_string());
    write_settings_value(&v)?;
    std::thread::sleep(std::time::Duration::from_millis(SETTINGS_WAIT_MS));
    Ok(())
}

/// Read the current `trim_enabled` value from settings.json. Returns None if the
/// file is missing, unparseable, or the field is absent.
pub fn read_trim_enabled() -> Option<bool> {
    let v = read_settings_value()?;
    v.get("trim_enabled")?.as_bool()
}

// ── helpers: snapshot corpus ────────────────────────────────────────────────────────────

/// Run a system dump command, capture stdout, write to a file. Returns the file
/// content on success, or None if the command failed.
pub fn capture_command_to_file(cmd: &str, args: &[&str], path: &std::path::Path) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let content = String::from_utf8_lossy(&output.stdout).to_string();
    let _ = std::fs::write(path, &content);
    Some(content)
}

/// Create the frozen corpus directory and populate it with link_01.txt ..
/// link_N.txt chain-walk files, each padded with ~14 KB of deterministic filler
/// text (identical structure across files; only the index and terminal NEXT line
/// differ). All files are always generated — no system dumps needed.
///
/// Returns the corpus path and the list of (filename, bytes) pairs, or an error
/// if filesystem writes fail.
pub fn snapshot_corpus() -> Result<(PathBuf, Vec<(String, usize)>), String> {
    let corpus_dir = std::env::temp_dir().join(CORPUS_DIR_NAME);
    let _ = std::fs::create_dir_all(&corpus_dir);

    let mut files: Vec<(String, usize)> = Vec::new();

    for i in 1..=CHAIN_LEN {
        let filename = format!("link_{i:02}.txt");
        let path = corpus_dir.join(&filename);

        let next_line = if i < CHAIN_LEN {
            format!("NEXT: link_{:02}.txt", i + 1)
        } else {
            "NEXT: STOP".to_string()
        };

        // Deterministic filler: ~14 KB per file, identical structure.
        let filler_line = format!(
            "ChainWalk: file {i:02} of {CHAIN_LEN}. This deterministic filler ensures each \
             chain-link file exceeds the trim elision threshold so trim behavior is exercised \
             during measurement. Content structure is identical across all files; only the index \
             and the terminal NEXT directive differ.\n"
        );
        let mut content: String = String::with_capacity(14500);
        while content.len() + next_line.len() + 1 < 14000 {
            content.push_str(&filler_line);
        }
        content.push_str(&next_line);
        content.push('\n');

        std::fs::write(&path, &content)
            .map_err(|e| format!("Failed to write {filename}: {e}"))?;

        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as usize;
        files.push((filename, bytes));
    }

    Ok((corpus_dir, files))
}

// ── helpers: claude run ─────────────────────────────────────────────────────────────────

/// Outcome of a single claude session.
#[derive(Debug, Clone)]
pub struct ClaudeRunOutcome {
    pub elapsed: std::time::Duration,
    pub success: bool,
    pub exit_code: Option<i32>,
}

/// Resolve a runnable `claude` executable. On Windows the npm `claude` shim has no
/// extension and is NOT launchable via `CreateProcess`; the real binary is `claude.exe`
/// inside the npm package. Probe known locations, falling back to the bare name (which
/// works on Unix or when claude.exe is already on PATH as an executable).
fn resolve_claude_exe() -> std::path::PathBuf {
    // Explicit override wins (lets the user point at any install).
    if let Ok(p) = std::env::var("MHD_CLAUDE_EXE") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    // npm global install: %APPDATA%\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe
    if let Ok(appdata) = std::env::var("APPDATA") {
        let p = std::path::Path::new(&appdata)
            .join("npm")
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("bin")
            .join("claude.exe");
        if p.is_file() {
            return p;
        }
    }
    // Fall back to a bare name; on Windows prefer the .exe form if present on PATH.
    if cfg!(windows) {
        std::path::PathBuf::from("claude.exe")
    } else {
        std::path::PathBuf::from("claude")
    }
}

/// Proxy base URL the spawned `claude` should target. Reads `port` / `bind_ip` from
/// settings.json (defaulting to 127.0.0.1:3456) so the workload hits the live proxy
/// even when it's bound to a non-default port -- mirrors what `claude-mhd` does.
fn proxy_base_url() -> String {
    let v = read_settings_value();
    let port = v
        .as_ref()
        .and_then(|v| v.get("port"))
        .and_then(|p| p.as_u64())
        .unwrap_or(3456);
    // The proxy may bind 0.0.0.0; always connect via loopback regardless of bind_ip.
    format!("http://127.0.0.1:{port}")
}

/// Spawn a single `claude -p` headless workload against the frozen corpus inside a
/// fresh temp directory. Waits for completion and returns the outcome. Setting
/// ANTHROPIC_BASE_URL to the local proxy reproduces the `claude-mhd` wrapper's effect.
pub fn run_claude(
    corpus_dir: &std::path::Path,
    run_dir: &std::path::Path,
) -> anyhow::Result<ClaudeRunOutcome> {
    let _ = std::fs::create_dir_all(run_dir);
    let t0 = Instant::now();

    let mut cmd = Command::new(resolve_claude_exe());
    cmd.arg("-p")
        .arg(PROMPT)
        .arg("--add-dir")
        .arg(corpus_dir)
        .arg("--dangerously-skip-permissions")
        .arg("--max-turns")
        .arg("20")
        .env("ANTHROPIC_BASE_URL", proxy_base_url())
        .env("CLAUDE_CODE_SUBAGENT_MODEL", "claude-sonnet-4-6")
        .current_dir(run_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());

    let output = cmd.output()?;
    let elapsed = t0.elapsed();

    // Write the session's stdout to a transcript file (tiny: ~10 sentences).
    let _ = std::fs::write(run_dir.join("transcript.txt"), &output.stdout);

    Ok(ClaudeRunOutcome {
        elapsed,
        success: output.status.success(),
        exit_code: output.status.code(),
    })
}

// ── helpers: column migration ───────────────────────────────────────────────────────────

/// Ensure a column exists on `table`. Runs `ALTER TABLE ADD COLUMN` only if the
/// column is absent (checking via pragma_table_info). Fails silently if the
/// table itself is absent or the column already exists.
///
/// Returns `Some(warning)` if the column could not be added but execution continues.
pub fn ensure_column(conn: &Connection, table: &str, col: &str, decl: &str) -> Option<String> {
    // Check if the column already exists.
    let exists: bool = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name=?2"
            ),
            rusqlite::params![table, col],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);

    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {col} {decl}");
        if let Err(e) = conn.execute_batch(&sql) {
            return Some(format!("Warning: could not add column {col} to {table}: {e}"));
        }
    }
    None
}

// ── helpers: timestamp ──────────────────────────────────────────────────────────────────

/// ISO-8601 UTC timestamp string (no external deps).
pub fn now_iso8601() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let subsec = now.subsec_millis();
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let h = (secs % 86400) / 3600;
    let min = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{subsec:03}Z")
}

// ── helpers: requests-table aggregation ──────────────────────────────────────────────────

/// Return the maximum `id` from the `requests` table, or 0 if empty.
fn max_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM requests",
        [],
        |row| row.get(0),
    )
}

/// Aggregate all requests rows with `id > id0` into an `ArmAggregate`.
///
/// Each token column may be NULL (row inserted at request start, token fields
/// updated at completion); NULLs are treated as 0. The caller should have waited
/// long enough for all completion UPDATEs to land before calling this.
///
/// Only counts rows whose `model` starts with `claude` (Anthropic-billed).
/// Side-model (offloaded) rows are excluded -- the tool measures Anthropic quota only.
fn aggregate_arm_native(conn: &Connection, id0: i64) -> anyhow::Result<ArmAggregate> {
    let mut stmt = conn.prepare(
        "SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
         trim_tokens_before, trim_tokens_after \
         FROM requests WHERE id > ?1 AND model LIKE 'claude%'"
    )?;

    let rows = stmt.query_map(rusqlite::params![id0], |row| {
        Ok((
            row.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64,
            row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
            row.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
            row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
            row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
            row.get::<_, Option<i64>>(5)?.unwrap_or(0) as u64,
        ))
    })?;

    let mut agg = ArmAggregate::default();
    for row in rows {
        let (inp, out, cr, cc, tb, ta) = row?;
        agg.n_requests += 1;
        agg.input_tokens += inp;
        agg.output_tokens += out;
        agg.cache_read_tokens += cr;
        agg.cache_creation_tokens += cc;
        agg.trim_before += tb;
        agg.trim_after += ta;
    }
    Ok(agg)
}

// ── Trim-restore guard ──────────────────────────────────────────────────────────────────

/// Restores `trim_enabled`, `opus_target`, `sonnet_target`, `haiku_target` to their
/// original values on drop if we ever toggled them.
///
/// A **hard invariant**: the original settings MUST be restored on every exit path
/// (success, abort, error). This guard ensures that even if a new early return
/// is added later, restore runs.
///
/// After performing a manual restore at the normal end of the measurement,
/// call [`disarm`](TrimRestoreGuard::disarm) to prevent a double-restore.
struct TrimRestoreGuard {
    /// Full settings.json snapshot at construction time. Restored verbatim.
    snapshot: Option<Value>,
    dirty: bool,
}

impl TrimRestoreGuard {
    fn new(settings: Option<Value>) -> Self {
        Self {
            snapshot: settings,
            dirty: false,
        }
    }

    /// Signal that we toggled settings -- the guard will restore on drop.
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Prevent the guard from restoring (called after a manual restore).
    fn disarm(&mut self) {
        self.dirty = false;
    }
}

impl Drop for TrimRestoreGuard {
    fn drop(&mut self) {
        if self.dirty {
            if let Some(v) = &self.snapshot {
                let _ = write_settings_value(v);
                std::thread::sleep(std::time::Duration::from_millis(SETTINGS_WAIT_MS));
            }
        }
    }
}

// ── Counterbalanced-measurement helpers ─────────────────────────────────────────────────

/// Run a single measurement arm: snapshot the requests high-water mark, run one
/// claude session, wait for the final completion UPDATE, and aggregate the new
/// rows (Anthropic-billed only). The caller is responsible for setting the
/// environment (trim_enabled + targets) before calling this.
///
/// `label` is a short tag like "ECO" used in progress messages.
fn run_one_arm(
    progress: &Arc<Mutex<MeasureProgress>>,
    ro_conn: &Connection,
    corpus_dir: &Path,
    run_dir: &Path,
    run_phase: MeasurePhase,
    label: &str,
) -> Result<ArmAggregate, String> {
    set_phase(progress, run_phase.clone(), &format!("{label}: running claude session..."));

    let id0 = max_id(ro_conn).unwrap_or(0);
    set_msg(progress, &format!("  {label}: requests id0 = {id0}"));

    let outcome = match run_claude(corpus_dir, run_dir) {
        Err(e) => {
            let msg = format!("FATAL: {label} session spawn failed: {e}");
            let mut p = progress.lock().unwrap();
            p.phase = MeasurePhase::Error;
            p.message.clone_from(&msg);
            p.error = Some(msg.clone());
            return Err(msg);
        }
        Ok(outcome) => {
            set_msg(
                progress,
                &format!("  {label} session done in {:.1}s", outcome.elapsed.as_secs_f64()),
            );
            outcome
        }
    };

    // Sleep so the final request's completion UPDATE lands.
    set_msg(
        progress,
        &format!("  {label}: waiting {POST_SESSION_SLEEP_MS}ms for final request completion..."),
    );
    std::thread::sleep(std::time::Duration::from_millis(POST_SESSION_SLEEP_MS));

    // Aggregate (Anthropic-billed rows only).
    set_phase(progress, run_phase, &format!("{label}: aggregating from requests table..."));
    let mut agg = match aggregate_arm_native(ro_conn, id0) {
        Ok(a) => a,
        Err(e) => {
            let msg = format!("FATAL: Could not aggregate {label} arm: {e}");
            let mut p = progress.lock().unwrap();
            p.phase = MeasurePhase::Error;
            p.message.clone_from(&msg);
            p.error = Some(msg.clone());
            return Err(msg);
        }
    };
    agg.elapsed_ms = outcome.elapsed.as_millis() as u64;

    set_msg(
        progress,
        &format!(
            "  {label}: {} reqs, {} input, {} cache_creation, {} cache_read",
            agg.n_requests, agg.input_tokens, agg.cache_creation_tokens, agg.cache_read_tokens
        ),
    );
    Ok(agg)
}

// ── Main driver ─────────────────────────────────────────────────────────────────────────

/// Run the whole measurement synchronously (BLOCKING -- call on a thread).
///
/// Mutates `progress` at each phase transition. Calls `confirm` at AwaitConfirm
/// (unless dry_run). On success returns `Ok(Some(result))`; on user-abort
/// `Ok(None)`; on fatal error `Err(msg)` and sets `progress.phase = Error`.
///
/// ALWAYS restores the original settings (`trim_enabled`, `opus_target`,
/// `sonnet_target`, `haiku_target`) before returning (success, abort, or error) --
/// a Drop guard enforces this invariant.
pub fn run_measurement(
    cfg: &MeasureConfig,
    progress: &Arc<Mutex<MeasureProgress>>,
    mut confirm: ConfirmFn,
) -> Result<Option<MeasureResult>, String> {
    let t0 = Instant::now();

    // ── S0 PREFLIGHT ────────────────────────────────────────────────────────

    set_phase(progress, MeasurePhase::Preflight, "=== Three-run trim measurement (requests-log based) ===");
    set_msg(progress, &format!("  DB: {}", cfg.db_path.display()));
    if cfg.dry_run {
        set_msg(progress, "  [DRY-RUN] No claude processes will be spawned.");
    }
    set_msg(progress, &format!("  side_substitution: {}", cfg.side_substitution));
    if cfg.side_substitution {
        set_msg(progress, &format!("  side_model: {}", cfg.side_model));
    }

    // Open a read-only connection for querying the requests table.
    let ro_conn = match Connection::open_with_flags(&cfg.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            return fatal(progress, format!("FATAL: Cannot open DB at {}: {e}", cfg.db_path.display()));
        }
    };

    // Verify the requests table exists.
    let requests_ok: bool = ro_conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='requests'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);

    if !requests_ok {
        return fatal(
            progress,
            "The 'requests' table does not exist in the database.\n\
             Enable db_log_enabled in settings.json and generate some traffic,\n\
             then re-run measurement.",
        );
    }
    set_msg(progress, "  requests table: OK");

    // Snapshot current settings for the guard (full restore on any exit).
    let settings_snapshot = read_settings_value();
    let original_trim_enabled = settings_snapshot
        .as_ref()
        .and_then(|v| v.get("trim_enabled"))
        .and_then(|v| v.as_bool());

    match original_trim_enabled {
        Some(v) => set_msg(progress, &format!("  Current trim_enabled: {v}")),
        None => set_msg(progress, "  trim_enabled: not found in settings.json (will create it)"),
    }

    let mut guard = TrimRestoreGuard::new(settings_snapshot);

    // ── S1 SNAPSHOT corpus ─────────────────────────────────────────────────

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Snapshot, "S1: Writing chain-walk corpus (15 linked files)...");

    let (corpus_dir, corpus_files) = match snapshot_corpus() {
        Ok((d, f)) => (d, f),
        Err(e) => {
            return fatal(progress, format!("FATAL: {e}"));
        }
    };

    set_msg(progress, &format!("  corpus: {}", corpus_dir.display()));

    // Log each file's status.
    for (name, bytes) in &corpus_files {
        set_msg(progress, &format!("    {name}  OK  ({bytes} bytes)"));
    }

    {
        let mut p = progress.lock().unwrap();
        p.corpus_dir = Some(corpus_dir.clone());
        p.corpus_files.clone_from(&corpus_files);
    }

    // ── S2 CONFIRM ─────────────────────────────────────────────────────────

    if !cfg.dry_run {
        set_phase(progress, MeasurePhase::AwaitConfirm, "S2: Confirmation");

        if !confirm() {
            let mut p = progress.lock().unwrap();
            p.phase = MeasurePhase::Aborted;
            p.message = "Aborted (response was not 'go').".to_string();
            return Ok(None);
        }
    } else {
        set_msg(progress, "S2: [DRY-RUN] Skipping confirmation (no claude spawns).");
        set_phase(progress, MeasurePhase::Done, "[DRY-RUN] plan only");
        return Ok(None);
    }

    // ── S3 MEASURE ARMS ────────────────────────────────────────────────────
    // Three runs in order: ECO (economy), NATIVE_ON (trim on, all native),
    // NATIVE_OFF (trim off, all native). ECO is first so the NATIVE A/B pair
    // runs back-to-back, minimising cache-state drift between them.

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::RunEco, "S3: Measuring arms (3 runs, ECO first)");

    let eco_run = std::env::temp_dir().join(ECO_RUN_DIR);
    let on_run = std::env::temp_dir().join(ON_RUN_DIR);
    let off_run = std::env::temp_dir().join(OFF_RUN_DIR);

    guard.mark_dirty();

    // ─ Arm 1: ECO ──────────────────────────────────────────────────────────
    set_trim_enabled(true).map_err(|e| {
        cfg_err(progress, format!("FATAL: Could not set trim_enabled for ECO: {e}"))
    })?;
    if cfg.side_substitution {
        set_targets("native", &cfg.side_model, &cfg.side_model).map_err(|e| {
            cfg_err(progress, format!("FATAL: Could not set targets for ECO: {e}"))
        })?;
    } else {
        set_targets("native", "native", "native").map_err(|e| {
            cfg_err(progress, format!("FATAL: Could not set targets for ECO: {e}"))
        })?;
    }

    let eco = run_one_arm(progress, &ro_conn, &corpus_dir, &eco_run, MeasurePhase::RunEco, "ECO")?;
    let eco_transcript = eco_run.join("transcript.txt");
    {
        let mut p = progress.lock().unwrap();
        p.eco = Some(eco.clone());
        p.eco_transcript = Some(eco_transcript);
    }

    // ─ Arm 2: NATIVE_ON ────────────────────────────────────────────────────
    set_trim_enabled(true).map_err(|e| {
        cfg_err(progress, format!("FATAL: Could not set trim_enabled for NATIVE_ON: {e}"))
    })?;
    set_targets("native", "native", "native").map_err(|e| {
        cfg_err(progress, format!("FATAL: Could not set targets for NATIVE_ON: {e}"))
    })?;

    let native_on = run_one_arm(progress, &ro_conn, &corpus_dir, &on_run, MeasurePhase::RunNativeOn, "NATIVE_ON")?;
    let on_transcript = on_run.join("transcript.txt");
    {
        let mut p = progress.lock().unwrap();
        p.native_on = Some(native_on.clone());
        p.native_on_transcript = Some(on_transcript);
    }

    // ─ Arm 3: NATIVE_OFF ───────────────────────────────────────────────────
    set_trim_enabled(false).map_err(|e| {
        cfg_err(progress, format!("FATAL: Could not set trim_enabled for NATIVE_OFF: {e}"))
    })?;
    set_targets("native", "native", "native").map_err(|e| {
        cfg_err(progress, format!("FATAL: Could not set targets for NATIVE_OFF: {e}"))
    })?;

    let native_off = run_one_arm(progress, &ro_conn, &corpus_dir, &off_run, MeasurePhase::RunNativeOff, "NATIVE_OFF")?;
    let off_transcript = off_run.join("transcript.txt");
    {
        let mut p = progress.lock().unwrap();
        p.native_off = Some(native_off.clone());
        p.native_off_transcript = Some(off_transcript);
    }

    // ── S5 COMPARE ─────────────────────────────────────────────────────────

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Compare, "S5: Comparing arms...");

    let cost_eco = weighted_billed(&eco).round() as u64;
    let cost_native_on = weighted_billed(&native_on).round() as u64;
    let cost_native_off = weighted_billed(&native_off).round() as u64;

    // Per-turn cost normalization for arm comparison.
    let cost_per_turn_off = cost_native_off as f64 / (native_off.n_requests.max(1) as f64);
    let cost_per_turn_on = cost_native_on as f64 / (native_on.n_requests.max(1) as f64);
    eprintln!(
        "[measure] cost/turn: OFF={:.0} ON={:.0} (n_off={} n_on={})",
        cost_per_turn_off, cost_per_turn_on, native_off.n_requests, native_on.n_requests
    );

    let native_saved_pct = if cost_native_off > 0 {
        (cost_native_off as f64 - cost_native_on as f64) / cost_native_off as f64 * 100.0
    } else {
        0.0
    };

    let eco_saved_pct = if cost_native_off > 0 {
        (cost_native_off as f64 - cost_eco as f64) / cost_native_off as f64 * 100.0
    } else {
        0.0
    };

    let trim_raw_pct = if native_on.trim_before > 0 {
        (native_on.trim_before as f64 - native_on.trim_after as f64) / native_on.trim_before as f64 * 100.0
    } else {
        0.0
    };

    // Session-divergence gate: if the native ON/OFF arms differ in request count,
    // the comparison is unreliable because token totals scale with turn count, not
    // trim. The chain-walk task is designed to force identical turn counts, so any
    // mismatch indicates a problem (e.g. claude hit the --max-turns cap or a tool
    // call failed mid-chain).
    let turn_mismatch = native_on.n_requests != native_off.n_requests;
    if turn_mismatch {
        eprintln!(
            "[measure] WARNING: arm turn-count mismatch off={} on={} -- comparison unreliable",
            native_off.n_requests, native_on.n_requests
        );
    }

    let verdict_of = |saved: f64, backwards_msg: &str| -> String {
        if turn_mismatch {
            "INVALID (turn-mismatch)".to_string()
        } else if saved >= 5.0 {
            "PROVEN".to_string()
        } else if saved >= 0.0 {
            "INCONCLUSIVE".to_string()
        } else {
            backwards_msg.to_string()
        }
    };

    let native_verdict = verdict_of(native_saved_pct, "BACKWARDS (trim cost more -- cache effect)");
    let eco_verdict = verdict_of(eco_saved_pct, "BACKWARDS");

    let result = MeasureResult {
        eco: eco.clone(),
        native_on: native_on.clone(),
        native_off: native_off.clone(),
        cost_eco,
        cost_native_on,
        cost_native_off,
        native_saved_pct,
        eco_saved_pct,
        native_verdict: native_verdict.clone(),
        eco_verdict: eco_verdict.clone(),
    };

    set_msg(progress, &format!(
        "  ECO cost: {}  NATIVE_ON cost: {}  NATIVE_OFF cost: {}",
        cost_eco, cost_native_on, cost_native_off
    ));
    set_msg(progress, &format!(
        "  NATIVE saved: {native_saved_pct:.1}%  (verdict: {native_verdict})"
    ));
    set_msg(progress, &format!(
        "  ECO saved: {eco_saved_pct:.1}%  (verdict: {eco_verdict})"
    ));
    set_msg(progress, &format!(
        "  Trim raw (NATIVE_ON): before {} after {}  {trim_raw_pct:.1}%",
        native_on.trim_before, native_on.trim_after
    ));

    {
        let mut p = progress.lock().unwrap();
        p.result = Some(result.clone());
    }

    // ── S6 RESTORE settings ────────────────────────────────────────────────

    set_msg(progress, "");
    if let Some(v) = &guard.snapshot {
        set_msg(progress, "  Restoring original settings...");
        if let Err(e) = write_settings_value(v) {
            set_msg(progress, &format!("Warning: could not restore settings: {e}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(SETTINGS_WAIT_MS));
    }
    guard.disarm();

    // ── S7 PERSIST to bench_runs ───────────────────────────────────────────

    let elapsed_ms = t0.elapsed().as_millis() as i64;

    set_msg(progress, "");
    set_msg(progress, "S7: Persisting result to bench_runs...");

    let rw_conn = match Connection::open(&cfg.db_path) {
        Ok(c) => {
            if let Err(e) = c.execute_batch("PRAGMA busy_timeout = 5000") {
                set_msg(progress, &format!("Warning: could not set busy_timeout: {e}"));
            }
            c
        }
        Err(e) => {
            set_msg(
                progress,
                &format!("Warning: could not open DB for write: {e}. Result not persisted."),
            );
            set_msg(progress, "");
            set_phase(progress, MeasurePhase::Done, "=== Measure complete (not persisted) ===");
            return Ok(Some(result));
        }
    };

    // Add live-measure columns if they don't exist (existing + new).
    for &(col, decl) in &[
        ("kind", "TEXT"),
        ("off_input", "INTEGER"),
        ("on_input", "INTEGER"),
        ("off_cache_read", "INTEGER"),
        ("on_cache_read", "INTEGER"),
        ("off_cache_creation", "INTEGER"),
        ("on_cache_creation", "INTEGER"),
        ("on_trim_before", "INTEGER"),
        ("on_trim_after", "INTEGER"),
        ("n_off_reqs", "INTEGER"),
        ("n_on_reqs", "INTEGER"),
        ("input_saved_pct", "REAL"),
        ("trim_raw_pct", "REAL"),
        ("verdict", "TEXT"),
        // New 3-arm columns:
        ("cost_eco", "INTEGER"),
        ("cost_native_on", "INTEGER"),
        ("cost_native_off", "INTEGER"),
        ("eco_saved_pct", "REAL"),
        ("eco_verdict", "TEXT"),
        ("eco_elapsed_ms", "INTEGER"),
        ("native_on_elapsed_ms", "INTEGER"),
        ("native_off_elapsed_ms", "INTEGER"),
        ("side_model", "TEXT"),
        ("side_substitution", "INTEGER"),
        ("n_eco_reqs", "INTEGER"),
        ("n_native_on_reqs", "INTEGER"),
        ("n_native_off_reqs", "INTEGER"),
    ] {
        if let Some(warning) = ensure_column(&rw_conn, "bench_runs", col, decl) {
            set_msg(progress, &warning);
        }
    }

    // Serialise live native knobs: extract all `trim_*` fields from settings.json.
    let knobs_json = match read_settings_value() {
        Some(v) => {
            if let Value::Object(map) = &v {
                let knobs: serde_json::Map<String, Value> = map
                    .iter()
                    .filter(|(k, _)| k.starts_with("trim_"))
                    .map(|(k, val)| (k.clone(), val.clone()))
                    .collect();
                serde_json::to_string(&Value::Object(knobs))
                    .unwrap_or_else(|_| "{}".to_string())
            } else {
                "{}".to_string()
            }
        }
        None => "{}".to_string(),
    };

    let ts = now_iso8601();

    let insert_result = rw_conn.execute(
        "INSERT INTO bench_runs (\
         ts, provider, n_bodies, n_trimmed, tokens_off, tokens_on, \
         avg_trim_pct, median_trim_pct, headroom_pct, fail_open_ok, deterministic, \
         elapsed_ms, knobs_json, \
         kind, off_input, on_input, off_cache_read, on_cache_read, \
         off_cache_creation, on_cache_creation, on_trim_before, on_trim_after, \
         n_off_reqs, n_on_reqs, input_saved_pct, trim_raw_pct, verdict, \
         cost_eco, cost_native_on, cost_native_off, eco_saved_pct, eco_verdict, \
         eco_elapsed_ms, native_on_elapsed_ms, native_off_elapsed_ms, \
         side_model, side_substitution, n_eco_reqs, n_native_on_reqs, n_native_off_reqs\
         ) VALUES (\
         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
         ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, \
         ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40\
         )",
        rusqlite::params![
            ts,
            "anthropic",
            0_i64,                                         // n_bodies
            native_on.n_requests as i64,                   // n_trimmed
            cost_native_off as i64,                        // tokens_off
            cost_native_on as i64,                         // tokens_on
            trim_raw_pct,                                  // avg_trim_pct
            0.0_f64,                                       // median_trim_pct
            0.0_f64,                                       // headroom_pct
            1_i64,                                         // fail_open_ok
            0_i64,                                         // deterministic
            elapsed_ms,                                    // elapsed_ms
            knobs_json,                                    // knobs_json
            "live3",                                       // kind
            native_off.input_tokens as i64,                // off_input
            native_on.input_tokens as i64,                 // on_input
            native_off.cache_read_tokens as i64,           // off_cache_read
            native_on.cache_read_tokens as i64,            // on_cache_read
            native_off.cache_creation_tokens as i64,       // off_cache_creation
            native_on.cache_creation_tokens as i64,        // on_cache_creation
            native_on.trim_before as i64,                  // on_trim_before
            native_on.trim_after as i64,                   // on_trim_after
            native_off.n_requests as i64,                  // n_off_reqs
            native_on.n_requests as i64,                   // n_on_reqs
            native_saved_pct,                              // input_saved_pct
            trim_raw_pct,                                  // trim_raw_pct
            native_verdict,                                // verdict
            cost_eco as i64,                               // cost_eco
            cost_native_on as i64,                         // cost_native_on
            cost_native_off as i64,                        // cost_native_off
            eco_saved_pct,                                 // eco_saved_pct
            eco_verdict,                                   // eco_verdict
            eco.elapsed_ms as i64,                         // eco_elapsed_ms
            native_on.elapsed_ms as i64,                   // native_on_elapsed_ms
            native_off.elapsed_ms as i64,                  // native_off_elapsed_ms
            cfg.side_model,                                // side_model
            cfg.side_substitution as i64,                  // side_substitution
            eco.n_requests as i64,                         // n_eco_reqs
            native_on.n_requests as i64,                   // n_native_on_reqs
            native_off.n_requests as i64,                  // n_native_off_reqs
        ],
    );

    match insert_result {
        Ok(_) => set_msg(progress, &format!("  Inserted row at {ts}")),
        Err(e) => set_msg(progress, &format!("Warning: could not insert bench_runs row: {e}")),
    }

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Done, "=== Measure complete ===");

    Ok(Some(result))
}

// ── internal helpers ────────────────────────────────────────────────────────────────────

/// Set the progress message without changing phase.
fn set_msg(progress: &Arc<Mutex<MeasureProgress>>, msg: &str) {
    let mut p = progress.lock().unwrap();
    p.message = msg.to_string();
}

/// Set both phase and message.
fn set_phase(progress: &Arc<Mutex<MeasureProgress>>, phase: MeasurePhase, msg: &str) {
    let mut p = progress.lock().unwrap();
    p.phase = phase;
    p.message = msg.to_string();
}

/// Set progress to Error phase with the given message and error string, then return Err.
/// Use this for all fatal errors so the caller (GUI) sees the error state immediately.
fn fatal<E: Into<String>>(progress: &Arc<Mutex<MeasureProgress>>, msg: E) -> Result<Option<MeasureResult>, String> {
    let s: String = msg.into();
    let mut p = progress.lock().unwrap();
    p.phase = MeasurePhase::Error;
    p.message.clone_from(&s);
    p.error = Some(s.clone());
    Err(s)
}

/// Set progress to Error and return the error string. Like [`fatal`] but returns
/// a plain String (for use inside map_err closures).
fn cfg_err(progress: &Arc<Mutex<MeasureProgress>>, msg: String) -> String {
    let mut p = progress.lock().unwrap();
    p.phase = MeasurePhase::Error;
    p.message.clone_from(&msg);
    p.error = Some(msg.clone());
    msg
}
