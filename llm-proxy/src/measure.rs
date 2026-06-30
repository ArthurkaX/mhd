//! Live trim measurement engine — two warm-session runs + requests-table usage comparison.
//!
//! Drives real `claude -p` workloads through the running proxy (trim OFF arm, then trim ON arm),
//! measures REAL per-request usage from the proxy's own `requests` table (exact tokens), and
//! reports the delta. Designed to run on a background thread; callers poll [`MeasureProgress`].
//!
//! ## Library constraints
//!
//! - NEVER calls `std::process::exit`. All errors are returned as `Err(String)`.
//! - NEVER reads stdin. The `confirm` closure is the only I/O gate.
//! - NEVER prints to stdout/stderr. The caller does all I/O by watching [`MeasureProgress`].
//! - ALWAYS restores `trim_enabled` to its original value before returning, via a Drop guard.

use crate::config::config_dir;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── Constants ────────────────────────────────────────────────────────────────────────────

/// SolidWorks PC-suitability orchestrator: launches two subagents, reads sysreq
/// directly, then writes a verdict.
const PROMPT: &str = "\
You are evaluating whether THIS PC can run SolidWorks. Inside the added directory there are \
three files: cpu-memory.txt, gpu-devices.txt, and solidworks-sysreq.txt.\n\
Launch EXACTLY TWO subagents using your Task tool, in parallel:\n\
  - Subagent 1: read cpu-memory.txt IN FULL and report the CPU model, core/thread count, \
    total RAM, and OS version. Do not read the other files.\n\
  - Subagent 2: read gpu-devices.txt IN FULL and report the GPU/display adapter model(s) \
    and whether any is a professional/workstation card. Do not read the other files.\n\
Do NOT read cpu-memory.txt or gpu-devices.txt yourself -- delegate them to the subagents.\n\
While the subagents work, read solidworks-sysreq.txt yourself (it is small).\n\
After both subagents return, compare this PC's specs against the requirements and write a \
verdict as EXACTLY 10 numbered sentences (1 to 10): state CPU, RAM, GPU, OS findings, whether \
each meets the requirement, and a final SUITABLE / NOT SUITABLE / MARGINAL conclusion.\n\
No markdown, no headings, no code blocks, no lists. Then stop.";

/// Frozen SolidWorks system requirements reference, written as a static file
/// into the corpus so the orchestrator can read it directly.
const SOLIDWORKS_SYSREQ: &str = "\
SolidWorks 2024 - System Requirements (frozen reference)\n\
Operating System: Windows 10 64-bit or Windows 11 64-bit (Home/Pro/Enterprise).\n\
Processor: 3.3 GHz or higher clock speed; Intel or AMD x64 with SSE4.2 support; 4+ cores recommended.\n\
RAM: 16 GB or more recommended (8 GB absolute minimum); 32 GB for large assemblies.\n\
Graphics: A certified workstation GPU with OpenGL 4.x support is required for full functionality\n\
  (NVIDIA Quadro / RTX A-series, or AMD Radeon Pro). Consumer GeForce/Radeon cards are not certified\n\
  and may render incorrectly. Minimum 4 GB VRAM; 8 GB recommended.\n\
Storage: SSD strongly recommended; 20 GB free disk space for installation.\n\
Other: Microsoft .NET Framework 4.8; a certified graphics driver matching the SolidWorks release.\n\
A PC is SUITABLE only if OS, CPU, RAM and GPU all meet or exceed the minimums.";

/// How long to wait after writing settings.json for the daemon's file watcher to
/// pick up the change (milliseconds).
const SETTINGS_WAIT_MS: u64 = 1200;

/// Default path for the frozen corpus directory.
const CORPUS_DIR_NAME: &str = "mhd-bench-corpus";

/// Temp run directories for each measurement arm.
const OFF_RUN_DIR: &str = "mhd-bench-run-off";
const ON_RUN_DIR: &str = "mhd-bench-run-on";

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
}

/// Result of a completed two-run measurement.
#[derive(Clone, Debug)]
pub struct MeasureResult {
    pub off: ArmAggregate,
    pub on: ArmAggregate,
    /// Quota-weighted cost of the OFF arm (input + 1.25*cache_creation + 0.1*cache_read),
    /// rounded to whole tokens. This is the REAL quota cost, not a raw token sum.
    pub billed_input_off: u64,
    /// Quota-weighted cost of the ON arm.
    pub billed_input_on: u64,
    /// (billed_off - billed_on) / billed_off * 100, 0 if billed_off == 0.
    /// Positive => trim saved real quota; negative => trim cost more (cache thrash).
    pub input_saved_pct: f64,
    /// (on.trim_before - on.trim_after) / on.trim_before * 100 (ON arm only).
    pub trim_raw_pct: f64,
    pub verdict: String,
}

/// Configuration for one measurement run.
#[derive(Clone, Debug)]
pub struct MeasureConfig {
    pub db_path: PathBuf,
    pub dry_run: bool,
}

/// Which phase the FSM is in (for GUI rendering + CLI logging).
#[derive(Clone, Debug, PartialEq)]
pub enum MeasurePhase {
    Idle,
    Preflight,
    Snapshot,
    AwaitConfirm,
    RunOff,
    AggregateOff,
    RunOn,
    AggregateOn,
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
    pub trim_engine: Option<String>,
    pub corpus_dir: Option<PathBuf>,
    pub corpus_files: Vec<(String, usize)>,
    pub off: Option<ArmAggregate>,
    pub on: Option<ArmAggregate>,
    pub result: Option<MeasureResult>,
    pub error: Option<String>,
    /// Path to the ON-arm session transcript (transcript.txt in the run dir).
    pub on_transcript: Option<PathBuf>,
    /// Path to the OFF-arm session transcript (transcript.txt in the run dir).
    pub off_transcript: Option<PathBuf>,
}

impl MeasureProgress {
    pub fn new() -> Self {
        Self {
            phase: MeasurePhase::Idle,
            message: String::new(),
            trim_engine: None,
            corpus_dir: None,
            corpus_files: Vec::new(),
            off: None,
            on: None,
            result: None,
            error: None,
            on_transcript: None,
            off_transcript: None,
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

/// Read the current `trim_enabled` value from settings.json. Returns None if the
/// file is missing, unparseable, or the field is absent.
pub fn read_trim_enabled() -> Option<bool> {
    let v = read_settings_value()?;
    v.get("trim_enabled")?.as_bool()
}

/// Read the current `trim_engine` value from settings.json (for informational display).
pub fn read_trim_engine() -> Option<String> {
    let v = read_settings_value()?;
    v.get("trim_engine")?.as_str().map(|s| s.to_string())
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

/// Create the frozen corpus directory and populate it with system dumps for
/// the SolidWorks PC-suitability workload. Produces three files:
///   - cpu-memory.txt    (systeminfo + driverquery /v /fo list, concatenated)
///   - gpu-devices.txt   (pnputil /enum-devices)
///   - solidworks-sysreq.txt (static frozen requirements)
///
/// Returns the corpus path and the list of (filename, bytes) pairs, or an error
/// string if BOTH machine-dump files are missing.
pub fn snapshot_corpus() -> Result<(PathBuf, Vec<(String, usize)>), String> {
    let corpus_dir = std::env::temp_dir().join(CORPUS_DIR_NAME);
    let _ = std::fs::create_dir_all(&corpus_dir);

    let mut files: Vec<(String, usize)> = Vec::new();
    let mut got_cpu = false;
    let mut got_gpu = false;

    // cpu-memory.txt = systeminfo output FOLLOWED BY driverquery /v /fo list output.
    let cpu_path = corpus_dir.join("cpu-memory.txt");
    let si_out = Command::new("systeminfo").output().ok();
    let dq_out = Command::new("driverquery").args(["/v", "/fo", "list"]).output().ok();
    if let (Some(si), Some(dq)) = (&si_out, &dq_out) {
        if si.status.success() && dq.status.success() {
            let mut combined = si.stdout.clone();
            combined.extend_from_slice(&dq.stdout);
            let _ = std::fs::write(&cpu_path, &combined);
            got_cpu = true;
            let bytes = std::fs::metadata(&cpu_path).map(|m| m.len()).unwrap_or(0) as usize;
            files.push(("cpu-memory.txt".to_string(), bytes));
        }
    }

    // gpu-devices.txt = pnputil /enum-devices  (~125 KB)
    let gpu_path = corpus_dir.join("gpu-devices.txt");
    if let Some(_content) = capture_command_to_file("pnputil", &["/enum-devices"], &gpu_path) {
        got_gpu = true;
        let bytes = std::fs::metadata(&gpu_path).map(|m| m.len()).unwrap_or(0) as usize;
        files.push(("gpu-devices.txt".to_string(), bytes));
    }

    // solidworks-sysreq.txt = static frozen reference (always written).
    let sysreq_path = corpus_dir.join("solidworks-sysreq.txt");
    let _ = std::fs::write(&sysreq_path, SOLIDWORKS_SYSREQ);
    let bytes = std::fs::metadata(&sysreq_path).map(|m| m.len()).unwrap_or(0) as usize;
    files.push(("solidworks-sysreq.txt".to_string(), bytes));

    if !got_cpu && !got_gpu {
        return Err(
            "Both cpu-memory.txt and gpu-devices.txt are missing -- cannot proceed.\n\
             Make sure you are on Windows with systeminfo, driverquery and pnputil available."
                .to_string(),
        );
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
    // Howard Hinnant's civil_from_days expects DAYS since the Unix epoch, shifted
    // so the internal era starts at 0000-03-01. Convert seconds -> days first.
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
fn aggregate_arm(conn: &Connection, id0: i64) -> anyhow::Result<ArmAggregate> {
    let mut stmt = conn.prepare(
        "SELECT input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
         trim_tokens_before, trim_tokens_after \
         FROM requests WHERE id > ?1"
    )?;

    // Use row-by-row iteration with Option<i64> extraction per column
    // so NULL token fields are safely treated as 0.
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

/// Restores `trim_enabled` to the original value on drop if we ever toggled it.
///
/// A **hard invariant**: the original value MUST be restored on every exit path
/// (success, abort, error). This guard ensures that even if a new early return
/// is added later, restore runs.
///
/// After performing a manual restore at the normal end of the measurement,
/// call [`disarm`](TrimRestoreGuard::disarm) to prevent a double-restore.
struct TrimRestoreGuard {
    original: Option<bool>,
    dirty: bool,
}

impl TrimRestoreGuard {
    fn new(original: Option<bool>) -> Self {
        Self {
            original,
            dirty: false,
        }
    }

    /// Signal that we toggled trim_enabled -- the guard will restore on drop.
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
            if let Some(val) = self.original {
                let _ = set_trim_enabled(val);
            }
            // If original was None (field absent), leave it be.
        }
    }
}

// ── Counterbalanced-measurement helpers ─────────────────────────────────

/// Run a single measurement arm: flip trim, snapshot the requests high-water
/// mark, run one claude session, wait for the final completion UPDATE, and
/// aggregate the new rows. `label` is a short tag like "OFF#1" used in
/// progress messages.
fn run_one_arm(
    progress: &Arc<Mutex<MeasureProgress>>,
    ro_conn: &Connection,
    guard: &mut TrimRestoreGuard,
    trim_on: bool,
    corpus_dir: &Path,
    run_dir: &Path,
    run_phase: MeasurePhase,
    agg_phase: MeasurePhase,
    label: &str,
) -> Result<ArmAggregate, String> {
    set_phase(progress, run_phase, &format!("{label}: running claude session..."));
    set_msg(progress, &format!("  {label}: setting trim_enabled = {trim_on}..."));
    if let Err(e) = set_trim_enabled(trim_on) {
        let msg = format!("FATAL: Could not write settings.json: {e}");
        let mut p = progress.lock().unwrap();
        p.phase = MeasurePhase::Error;
        p.message.clone_from(&msg);
        p.error = Some(msg.clone());
        return Err(msg);
    }
    guard.mark_dirty();

    let id0 = max_id(ro_conn).unwrap_or(0);
    set_msg(progress, &format!("  {label}: requests id0 = {id0}"));

    match run_claude(corpus_dir, run_dir) {
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
        }
    }

    // Sleep so the final request's completion UPDATE lands.
    set_msg(
        progress,
        &format!("  {label}: waiting {POST_SESSION_SLEEP_MS}ms for final request completion..."),
    );
    std::thread::sleep(std::time::Duration::from_millis(POST_SESSION_SLEEP_MS));

    // Aggregate.
    set_phase(progress, agg_phase, &format!("{label}: aggregating from requests table..."));
    let agg = match aggregate_arm(ro_conn, id0) {
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
/// ALWAYS restores the original `trim_enabled` before returning (success,
/// abort, or error) -- a Drop guard enforces this invariant.
pub fn run_measurement(
    cfg: &MeasureConfig,
    progress: &Arc<Mutex<MeasureProgress>>,
    mut confirm: ConfirmFn,
) -> Result<Option<MeasureResult>, String> {
    let t0 = Instant::now();

    // ── S0 PREFLIGHT ────────────────────────────────────────────────────────

    set_phase(progress, MeasurePhase::Preflight, "=== Two-run trim measurement (requests-log based) ===");
    set_msg(progress, &format!("  DB: {}", cfg.db_path.display()));
    if cfg.dry_run {
        set_msg(progress, "  [DRY-RUN] No claude processes will be spawned.");
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

    // Read current trim state from settings.json.
    let original_trim_enabled = read_trim_enabled();
    let trim_engine = read_trim_engine();
    match original_trim_enabled {
        Some(v) => set_msg(progress, &format!("  Current trim_enabled: {v}")),
        None => set_msg(progress, "  trim_enabled: not found in settings.json (will create it)"),
    }
    match &trim_engine {
        Some(e) if e != "native" => {
            set_msg(
                progress,
                &format!(
                    "  Warning: trim_engine = \"{e}\" (not \"native\"). The measurement will \
                     reflect whatever engine is currently live."
                ),
            );
        }
        Some(e) => set_msg(progress, &format!("  trim_engine: {e}")),
        None => set_msg(progress, "  trim_engine: not found"),
    }

    // Store for progress.
    {
        let mut p = progress.lock().unwrap();
        p.trim_engine = trim_engine;
    }

    let mut guard = TrimRestoreGuard::new(original_trim_enabled);

    // ── S1 SNAPSHOT corpus ─────────────────────────────────────────────────

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Snapshot, "S1: Snapshotting system data to frozen corpus...");

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
    // Check which files failed.
    let names: Vec<String> = corpus_files.iter().map(|(n, _)| n.clone()).collect();
    if !names.contains(&"cpu-memory.txt".to_string()) {
        set_msg(progress, "    cpu-memory.txt  FAILED (systeminfo or driverquery not available or errored)");
    }
    if !names.contains(&"gpu-devices.txt".to_string()) {
        set_msg(progress, "    gpu-devices.txt  FAILED (pnputil not available or errored)");
    }
    // solidworks-sysreq.txt is static and always written successfully.

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
    // Two runs, ON first (cold) then OFF. We accept that cross-run cache
    // contamination is minor relative to the in-session cache_creation effect
    // (validated: in-session cache writes dominate the delta — OFF ~80k vs ON ~12k).
    // Running ON cold first — before anything warms its trimmed prefix — is the
    // trim-HOSTILE order: OFF (run 2) may read ON's warmed SHARED prefix, giving
    // the no-trim arm a small edge. If trim still wins under this handicap, the
    // real-world saving is at least this much (a lower bound, not a point estimate).

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::RunOn, "S3: Measuring arms (2 runs, ON cold first)");

    // Distinct run dirs avoid file-lock contention (cwd does not affect the cache).
    let on_run = std::env::temp_dir().join(ON_RUN_DIR);
    let off_run = std::env::temp_dir().join(OFF_RUN_DIR);

    // Run 1: ON (trim enabled), cold — its trimmed prefix is not pre-warmed.
    let on = run_one_arm(
        progress, &ro_conn, &mut guard, true, &corpus_dir, &on_run,
        MeasurePhase::RunOn, MeasurePhase::AggregateOn, "ON",
    )?;
    {
        let mut p = progress.lock().unwrap();
        p.on = Some(on.clone());
        p.on_transcript = Some(on_run.join("transcript.txt"));
    }

    // Run 2: OFF (trim disabled). May benefit from ON's warmed shared prefix.
    let off = run_one_arm(
        progress, &ro_conn, &mut guard, false, &corpus_dir, &off_run,
        MeasurePhase::RunOff, MeasurePhase::AggregateOff, "OFF",
    )?;
    {
        let mut p = progress.lock().unwrap();
        p.off = Some(off.clone());
        p.off_transcript = Some(off_run.join("transcript.txt"));
    }

    // ── S5 COMPARE ─────────────────────────────────────────────────────────

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Compare, "S5: Comparing arms...");

    // Quota-weighted cost: cache_read is cheap (0.1x) but NOT free, and trim moves
    // huge amounts of it in a warm cache. Weighting reflects real quota consumption.
    let billed_off_f = weighted_billed(&off);
    let billed_on_f = weighted_billed(&on);
    let billed_input_off = billed_off_f.round() as u64;
    let billed_input_on = billed_on_f.round() as u64;
    let input_saved_pct = if billed_off_f > 0.0 {
        (billed_off_f - billed_on_f) / billed_off_f * 100.0
    } else {
        0.0
    };
    let trim_raw_pct = if on.trim_before > 0 {
        (on.trim_before as f64 - on.trim_after as f64) / on.trim_before as f64 * 100.0
    } else {
        0.0
    };
    let verdict = if input_saved_pct >= 5.0 {
        "PROVEN".to_string()
    } else if input_saved_pct >= 0.0 {
        "INCONCLUSIVE".to_string()
    } else {
        "BACKWARDS (trim cost more -- cache effect)".to_string()
    };

    let result = MeasureResult {
        off: off.clone(),
        on: on.clone(),
        billed_input_off,
        billed_input_on,
        input_saved_pct,
        trim_raw_pct,
        verdict: verdict.clone(),
    };

    set_msg(progress, &format!("  Billed input: OFF {billed_input_off}  ON {billed_input_on}  saved {input_saved_pct:.1}%"));
    set_msg(progress, &format!("  Trim raw (ON): before {} after {}  {trim_raw_pct:.1}%", on.trim_before, on.trim_after));
    set_msg(progress, &format!("  Verdict: {verdict}"));

    {
        let mut p = progress.lock().unwrap();
        p.result = Some(result.clone());
    }

    // ── S6 RESTORE trim ────────────────────────────────────────────────────

    set_msg(progress, "");
    match original_trim_enabled {
        Some(v) => {
            set_msg(progress, &format!("  Restoring trim_enabled to {v}..."));
            if let Err(e) = set_trim_enabled(v) {
                set_msg(progress, &format!("Warning: could not restore trim_enabled: {e}"));
            }
        }
        None => {
            set_msg(
                progress,
                "  trim_enabled was absent at S0; leaving at current value.",
            );
        }
    }
    // The guard is now disarmed -- we handled restore explicitly.
    guard.disarm();

    // ── S7 PERSIST to bench_runs ───────────────────────────────────────────

    let elapsed_ms = t0.elapsed().as_millis() as i64;

    set_msg(progress, "");
    set_msg(progress, "S7: Persisting result to bench_runs...");

    // Open a short write connection with busy_timeout for the migration + insert.
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
            set_phase(progress, MeasurePhase::Done, "═══ Measure complete (not persisted) ═══");
            return Ok(Some(result));
        }
    };

    // Add live-measure columns if they don't exist.
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
         n_off_reqs, n_on_reqs, input_saved_pct, trim_raw_pct, verdict\
         ) VALUES (\
         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
         ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27\
         )",
        rusqlite::params![
            ts,
            "anthropic",
            0_i64,                                        // n_bodies
            on.n_requests as i64,                         // n_trimmed
            billed_input_off as i64,                      // tokens_off
            billed_input_on as i64,                       // tokens_on
            trim_raw_pct,                                 // avg_trim_pct
            0.0_f64,                                      // median_trim_pct
            0.0_f64,                                      // headroom_pct
            1_i64,                                        // fail_open_ok
            0_i64,                                        // deterministic
            elapsed_ms,                                   // elapsed_ms
            knobs_json,                                   // knobs_json
            "live2",                                      // kind
            off.input_tokens as i64,                      // off_input
            on.input_tokens as i64,                       // on_input
            off.cache_read_tokens as i64,                 // off_cache_read
            on.cache_read_tokens as i64,                  // on_cache_read
            off.cache_creation_tokens as i64,             // off_cache_creation
            on.cache_creation_tokens as i64,              // on_cache_creation
            on.trim_before as i64,                        // on_trim_before
            on.trim_after as i64,                         // on_trim_after
            off.n_requests as i64,                        // n_off_reqs
            on.n_requests as i64,                         // n_on_reqs
            input_saved_pct,                              // input_saved_pct
            trim_raw_pct,                                 // trim_raw_pct
            verdict,                                      // verdict
        ],
    );

    match insert_result {
        Ok(_) => set_msg(progress, &format!("  Inserted row at {ts}")),
        Err(e) => set_msg(progress, &format!("Warning: could not insert bench_runs row: {e}")),
    }

    set_msg(progress, "");
    set_phase(progress, MeasurePhase::Done, "═══ Measure complete ═══");

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
