//! cache_bench — offline replay of the recorded corpus through a *modeled*
//! prefix cache, comparing raw bodies (OFF) against native-trim bodies (ON)
//! under Anthropic's cache pricing weights (input 1.0x, cache write 1.25x,
//! cache read 0.1x).
//!
//! Usage:
//!   cache_bench [<db path>] [--db <path>] [--calibrate]
//!              [--strip-thinking] [--ws] [--tool-desc <N>] [--head <N>]
//!              [--tail <N>] [--min-elide <N>]
//!
//! Reads `request_bodies` (provider='anthropic') from proxy.db, groups rows
//! into per-conversation chains by (run_id, session_hash), and replays each
//! chain once per arm. Prints an arm-by-arm bucket table, raw vs weighted
//! savings, a verdict, and the worst prefix breaks trim causes.
//!
//! `--calibrate` replaces the report with a calibration run: it compares the
//! modeled costs against what the provider actually billed and prints two
//! verdicts (TOKENIZER for the chars/4 estimator, CACHE MODEL for the shared-
//! prefix split). Until those read CALIBRATED, the savings numbers below are a
//! plausible model, not measured fact.
//!
//! This is a MODEL, not billing data: tokens are chars/4 and the cache split
//! is byte-proportional, both of which understate trim's benefit (trim cuts
//! dense code where tokens-per-byte is highest, while the shared system/tools
//! prefix is prose). The numbers are a floor on savings, never inflated.

use llm_proxy::cache_bench::{
    CacheVerdict, CalibrationReport, RatioStats, run_cache_bench, run_calibration,
};
use llm_proxy::config::config_dir;
use llm_proxy::native_trim::NativeKnobs;
use std::path::{Path, PathBuf};

/// Compact token formatting shared with the sibling bins (2.3K, 4.1M, ...).
fn fmt_tok(n: u64) -> String {
    if n == 0 {
        return "-".to_string();
    }
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// One-line gloss for the verdict line.
fn verdict_gloss(v: CacheVerdict) -> &'static str {
    match v {
        CacheVerdict::Proven => "(ON weighted cost >2% below OFF)",
        CacheVerdict::Inconclusive => "(within ±2% — noise for a chars/4 estimator)",
        CacheVerdict::Backwards => "(ON weighted cost >2% above OFF — trim is costing cache hits)",
    }
}

/// One table row for a ratio distribution in the calibration report.
fn print_ratio_row(label: &str, s: &RatioStats) {
    println!(
        "  {:<16}  {:>6}  {:>7.2}  {:>7.2}  {:>7.2}  {:>9.2}  {:>10.2}",
        label, s.n, s.median, s.p10, s.p90, s.within_2x, s.within_10x
    );
}

/// Which way the model errs for a live/model ratio's median: a median above 1
/// means live tokens exceed the model (under-estimate); below 1 the reverse.
fn direction(median: f64) -> &'static str {
    if median > 1.0 {
        "model under-estimates"
    } else {
        "model over-estimates"
    }
}

/// Higher = further from calibrated. A calibrated ratio scores 0; any failing
/// ratio scores at least 1, so a failing ratio always outranks a passing one,
/// and failing ratios are ordered by how far the median sits from 1.0.
fn badness(s: &RatioStats) -> f64 {
    if s.is_calibrated() {
        0.0
    } else {
        1.0 + (s.median - 1.0).abs()
    }
}

/// The least calibrated of the read/creation/shared ratios — the one that most
/// undermines the cache-model verdict. Returns `None` when none had comparable
/// rows.
fn worst_cache_ratio(report: &CalibrationReport) -> Option<(&'static str, RatioStats)> {
    let cands = [
        ("read", report.read_ratio()),
        ("creation", report.creation_ratio()),
        ("shared", report.shared_ratio()),
    ];
    let mut worst: Option<(&'static str, RatioStats)> = None;
    for (name, stats) in cands {
        let Some(stats) = stats else { continue };
        if worst
            .as_ref()
            .map(|(_, w)| badness(&stats) > badness(w))
            .unwrap_or(true)
        {
            worst = Some((name, stats));
        }
    }
    worst
}

/// The `--calibrate` entry point: replay the corpus and compare each request
/// against what the provider actually billed, printing ratio distributions and
/// two verdicts (TOKENIZER, CACHE MODEL). SUSPECT is an honest "do not trust the
/// bench verdicts yet", not a failure mode to paper over.
fn run_calibration_cmd(db_path: &Path, knobs: &NativeKnobs) {
    let report = match run_calibration(db_path, knobs) {
        Ok(Some(r)) => r,
        Ok(None) => {
            println!();
            println!("Corpus empty — no usable anthropic bodies in request_bodies.");
            println!();
            println!("Set corpus_capture = true in llm-proxy settings, restart the daemon,");
            println!("generate some Claude Code traffic, then re-run.");
            return;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Calibration — modeled replay vs. what the provider actually billed");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!(
        "  knobs: {}   estimator: ~chars/4, byte-proportional split",
        fmt_knobs(knobs)
    );
    println!();
    println!("  matched rows:           {}", report.rows.len());
    println!("  n_skipped_null:         {}", report.n_skipped_null);
    println!("  n_unmatched:            {}", report.n_unmatched);
    println!("  n_arm_on:               {}", report.n_arm_on);
    println!("  n_arm_off:              {}", report.n_arm_off);
    println!(
        "  n_distinct_trim_config: {}",
        report.n_distinct_trim_config
    );
    println!();
    println!("  ratio = live / model; 1.0 means the model is exact. Median is the");
    println!("  headline; p10/p90 bound the tail; within_2x / within_10x are the");
    println!("  fractions of rows inside the two-sided band [1/factor, factor].");
    println!();
    println!(
        "  {:<16}  {:>6}  {:>7}  {:>7}  {:>7}  {:>9}  {:>10}",
        "ratio", "n", "median", "p10", "p90", "within_2x", "within_10x"
    );
    println!("  {}", "─".repeat(62));
    for (label, stats) in [
        ("live/model total", report.total_ratio()),
        ("read", report.read_ratio()),
        ("creation", report.creation_ratio()),
        ("shared", report.shared_ratio()),
    ] {
        match stats {
            Some(s) => print_ratio_row(label, &s),
            None => println!("  {:<16}  {:>6}  (no comparable rows)", label, "-"),
        }
    }
    println!("  {}", "─".repeat(62));
    println!();

    let tokenizer = report.total_ratio();
    let cache_worst = worst_cache_ratio(&report);

    let tokenizer_line = match &tokenizer {
        Some(s) if s.is_calibrated() => "CALIBRATED".to_string(),
        Some(s) => format!(
            "SUSPECT  (live/model total median {:.2} — {})",
            s.median,
            direction(s.median)
        ),
        None => "SUSPECT  (no comparable rows)".to_string(),
    };
    let cache_line = match &cache_worst {
        Some((name, s)) if s.is_calibrated() => "CALIBRATED".to_string(),
        Some((name, s)) => format!(
            "SUSPECT  ({name}: median {:.2} — {})",
            s.median,
            direction(s.median)
        ),
        None => "SUSPECT  (no comparable rows)".to_string(),
    };

    println!("  verdict TOKENIZER:    {tokenizer_line}");
    println!("  verdict CACHE MODEL:  {cache_line}");
    println!();

    let tokenizer_ok = tokenizer
        .as_ref()
        .map(RatioStats::is_calibrated)
        .unwrap_or(false);
    let cache_ok = cache_worst
        .as_ref()
        .map(|(_, s)| s.is_calibrated())
        .unwrap_or(false);
    if !tokenizer_ok || !cache_ok {
        println!("  SUSPECT means the model does not track what the provider actually");
        println!("  billed. Until calibration passes, the bench's savings verdicts are");
        println!("  a plausible model, not measured fact — do not trust them yet.");
    }

    let _ = std::io::Write::flush(&mut std::io::stdout());
}

// ── knob-override helpers ─────────────────────────────────────────────────────

/// One-line rendering of the effective trim knobs for the report header, so a
/// run is self-describing even when flags override the defaults.
fn fmt_knobs(knobs: &NativeKnobs) -> String {
    serde_json::to_string(knobs).unwrap_or_else(|_| "<knobs serialize failed>".to_string())
}

/// The value of `--flag`'s argument at `i + 1`, advancing `i` past it. Errors
/// when the flag is the last argument and has no value to consume.
fn flag_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    if *i < args.len() {
        Ok(args[*i].clone())
    } else {
        Err(format!("{flag} requires a value"))
    }
}

/// Parse a `--flag`'s value as a non-negative integer, or error.
fn parse_count(value: &str, flag: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("{flag} expects a non-negative integer, got: {value}"))
}

/// Print an error to stderr and exit 1.
fn die(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

fn main() {
    // Default DB path resolves the same way as the sibling offline tools.
    let mut db_path = config_dir().join("proxy.db");
    let mut calibrate = false;
    // No flags → stays exactly `NativeKnobs::default()`, byte-identical to the
    // pre-flags behavior; each flag overrides one field.
    let mut knobs = NativeKnobs::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--db" => match flag_value(&args, &mut i, "--db") {
                Ok(v) => db_path = PathBuf::from(v),
                Err(e) => die(&e),
            },
            "--calibrate" => calibrate = true,
            "--strip-thinking" => knobs.strip_thinking = true,
            "--ws" => knobs.ws_enabled = true,
            "--tool-desc" | "--head" | "--tail" | "--min-elide" => {
                let value = flag_value(&args, &mut i, arg).unwrap_or_else(|e| die(&e));
                let n = parse_count(&value, arg).unwrap_or_else(|e| die(&e));
                match arg {
                    "--tool-desc" => knobs.tool_max_desc_chars = n,
                    "--head" => knobs.tool_result_head = n,
                    "--tail" => knobs.tool_result_tail = n,
                    _ => knobs.tool_result_min_elide = n,
                }
            }
            other if !other.starts_with('-') => db_path = PathBuf::from(other),
            other => {
                eprintln!("Error: unknown argument: {other}");
                eprintln!();
                eprintln!("Usage:");
                eprintln!("  cache_bench [<db path>] [--db <path>] [--calibrate]");
                eprintln!("  cache_bench [--strip-thinking] [--ws] [--tool-desc <N>] [--head <N>]");
                eprintln!("              [--tail <N>] [--min-elide <N>]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    eprintln!("DB: {}", db_path.display());

    if calibrate {
        run_calibration_cmd(&db_path, &knobs);
        return;
    }

    let res = match run_cache_bench(&db_path, &knobs) {
        Ok(Some(r)) => r,
        Ok(None) => {
            println!();
            println!("Corpus empty — no usable anthropic bodies in request_bodies.");
            println!();
            println!("Set corpus_capture = true in llm-proxy settings, restart the daemon,");
            println!("generate some Claude Code traffic, then re-run.");
            return;
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    let off_cost = res.off.weighted_cost();
    let on_cost = res.on.weighted_cost();
    let raw_saved_pct = if res.off.raw_tokens > 0 {
        (res.off.raw_tokens as f64 - res.on.raw_tokens as f64) / res.off.raw_tokens as f64 * 100.0
    } else {
        0.0
    };
    let weighted_saved_pct = if off_cost > 0.0 {
        (off_cost - on_cost) / off_cost * 100.0
    } else {
        0.0
    };

    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Cache-Weighted Offline Bench (modeled prefix cache, NOT billing data)");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!(
        "  knobs: {}   estimator: ~chars/4, byte-proportional split",
        fmt_knobs(&knobs)
    );
    println!();
    println!(
        "  {:<4}  {:>8}  {:>9}  {:>12}  {:>10}  {:>9}",
        "arm", "requests", "input", "cache_create", "cache_read", "weighted"
    );
    println!("  {}", "─".repeat(56));
    println!(
        "  {:<4}  {:>8}  {:>9}  {:>12}  {:>10}  {:>9}",
        "off",
        res.off.n_requests,
        fmt_tok(res.off.input_tokens),
        fmt_tok(res.off.cache_creation_tokens),
        fmt_tok(res.off.cache_read_tokens),
        format!("{off_cost:.0}")
    );
    println!(
        "  {:<4}  {:>8}  {:>9}  {:>12}  {:>10}  {:>9}",
        "on",
        res.on.n_requests,
        fmt_tok(res.on.input_tokens),
        fmt_tok(res.on.cache_creation_tokens),
        fmt_tok(res.on.cache_read_tokens),
        format!("{on_cost:.0}")
    );
    println!("  {}", "─".repeat(56));
    println!();
    println!("  raw_saved_pct:      {raw_saved_pct:.2}%");
    println!("  weighted_saved_pct: {weighted_saved_pct:.2}%");
    println!(
        "  verdict:            {}  {}",
        res.verdict,
        verdict_gloss(res.verdict)
    );
    println!(
        "  n_chains:           {}   (a small number here means chains merged wrong)",
        res.n_chains
    );
    println!(
        "  prefix breaks:      {}   ({} tokens lost)",
        res.off.n_prefix_breaks,
        fmt_tok(res.off.prefix_break_tokens)
    );
    println!();

    if !res.worst_breaks.is_empty() {
        println!("  Worst prefix breaks (trim shortens a prefix the harness kept stable —");
        println!("  those tokens move from the 0.10x cache-read bucket to the 1.0x input bucket):");
        println!(
            "  {:>16}  {:>6}  {:>24}  {:>18}  {:>12}",
            "run_id", "seq", "shared_off -> shared_on", "parts off -> on", "tokens_lost"
        );
        println!("  {}", "─".repeat(56));
        for b in &res.worst_breaks {
            println!(
                "  {:>16}  {:>6}  {:>24}  {:>18}  {:>12}",
                b.run_id,
                b.seq,
                format!("{} -> {}", b.shared_off, b.shared_on),
                format!("{} -> {}", b.shared_parts_off, b.shared_parts_on),
                fmt_tok(b.tokens_lost)
            );
        }
        println!();
    }

    let _ = std::io::Write::flush(&mut std::io::stdout());
}
