//! backtest — re-apply candidate trim configurations to the replay corpus.
//!
//! Usage:
//!   backtest [--db <path>] [--desc-chars <comma-list>]
//!
//! Reads all rows from `request_bodies` in proxy.db, sweeps the
//! `tool_max_desc_chars` lever across the specified values, and prints a table
//! showing savings at each setting so the autotune curve is visible without
//! calling the real LLM.
//!
//! The corpus is populated when `corpus_capture = true` in llm-proxy settings.
//! Only `provider = "anthropic"` rows are swept in v1; openai rows are counted
//! but skipped (future work).

use llm_proxy::config::config_dir;
use llm_proxy::trim::{TrimKnobs, TrimProvider, trim_with_knobs};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;

// ── pricing ───────────────────────────────────────────────────────────────────

/// Input cost in USD per 1M tokens, matched by case-insensitive substring.
/// Default 5.0 (Opus rate) for unknown models. Trim only reduces input tokens
/// so the output rate is irrelevant here.
fn input_rate_per_1m(model: &str) -> f64 {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        5.0
    } else if m.contains("sonnet") {
        3.0
    } else if m.contains("haiku") {
        1.0
    } else if m.contains("fable") {
        10.0
    } else {
        5.0 // conservative default — matches Opus
    }
}

// ── corpus row ────────────────────────────────────────────────────────────────

struct BodyRow {
    model: String,
    provider: String,
    body: Value,
}

// ── metrics ───────────────────────────────────────────────────────────────────

struct SweepResult {
    desc_chars: usize,
    n_eligible: usize,
    n_trimmed: usize,
    avg_pct: f64,
    median_pct: f64,
    p90_pct: f64,
    tokens_saved: u64,
    dollars_saved: f64,
}

/// Pick a percentile from a sorted slice (nearest-rank, 0-indexed).
fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── formatting helpers ────────────────────────────────────────────────────────

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

// ── corpus reader ─────────────────────────────────────────────────────────────

fn read_corpus(conn: &Connection) -> Vec<BodyRow> {
    // Gracefully handle a missing table (corpus not yet populated).
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_bodies'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);

    if !table_exists {
        return Vec::new();
    }

    let mut stmt = match conn.prepare(
        "SELECT model, provider, body FROM request_bodies ORDER BY id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut rows: Vec<BodyRow> = Vec::new();
    let mut parse_errors: usize = 0;

    let iter = stmt.query_map([], |row| {
        let model: String = row.get(0)?;
        let provider: String = row.get(1)?;
        let body_str: String = row.get(2)?;
        Ok((model, provider, body_str))
    });

    if let Ok(iter) = iter {
        for item in iter {
            match item {
                Ok((model, provider, body_str)) => {
                    match serde_json::from_str::<Value>(&body_str) {
                        Ok(body) => rows.push(BodyRow { model, provider, body }),
                        Err(_) => parse_errors += 1,
                    }
                }
                Err(_) => parse_errors += 1,
            }
        }
    }

    if parse_errors > 0 {
        eprintln!("Skipped {parse_errors} rows with unparseable body JSON.");
    }

    rows
}

// ── arg parsing ───────────────────────────────────────────────────────────────

fn parse_args() -> (PathBuf, Vec<usize>) {
    let default_db = config_dir().join("proxy.db");
    let default_sweep: Vec<usize> = vec![40, 80, 120, 150, 200, 300];

    let mut db_path = default_db;
    let mut sweep = default_sweep;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                if i < args.len() {
                    db_path = PathBuf::from(&args[i]);
                }
            }
            "--desc-chars" => {
                i += 1;
                if i < args.len() {
                    let parsed: Vec<usize> = args[i]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<usize>().ok())
                        .collect();
                    if parsed.is_empty() {
                        eprintln!(
                            "Warning: --desc-chars produced no valid values; using defaults."
                        );
                    } else {
                        sweep = parsed;
                    }
                }
            }
            other => {
                eprintln!("Unknown argument: {other}  (ignored)");
            }
        }
        i += 1;
    }

    (db_path, sweep)
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() {
    let (db_path, sweep_values) = parse_args();

    eprintln!("DB: {}", db_path.display());

    // Open read-only — never write to the corpus.
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot open DB at {}: {e}", db_path.display());
            eprintln!();
            eprintln!("If the file does not exist yet, set corpus_capture = true in");
            eprintln!("llm-proxy settings, restart the daemon, generate some Claude Code");
            eprintln!("traffic, then re-run.");
            std::process::exit(1);
        }
    };

    let rows = read_corpus(&conn);

    if rows.is_empty() {
        println!();
        println!("Corpus is empty — no rows in request_bodies.");
        println!();
        println!("Set corpus_capture = true in llm-proxy settings, restart the daemon,");
        println!("generate some Claude Code traffic, then re-run.");
        return;
    }

    // Eligible = anthropic rows with parseable body (already filtered by read_corpus).
    let eligible: Vec<&BodyRow> = rows.iter().filter(|r| r.provider == "anthropic").collect();
    let n_openai = rows.iter().filter(|r| r.provider != "anthropic").count();

    eprintln!(
        "Corpus: {} total rows  ({} anthropic eligible, {} non-anthropic skipped)",
        rows.len(),
        eligible.len(),
        n_openai,
    );

    if eligible.is_empty() {
        println!();
        println!("No anthropic rows in corpus (non-anthropic rows are skipped in v1).");
        println!("Generate some Claude Code traffic via the Anthropic path, then re-run.");
        return;
    }

    // ── determinism sanity check ──────────────────────────────────────────────
    let det_knobs = TrimKnobs { tool_max_desc_chars: 150, ..TrimKnobs::default() };
    let first_body = eligible[0].body.clone();
    let det_a = trim_with_knobs(first_body.clone(), TrimProvider::Anthropic, &det_knobs);
    let det_b = trim_with_knobs(first_body, TrimProvider::Anthropic, &det_knobs);
    if det_a.body == det_b.body {
        println!("determinism: OK");
    } else {
        println!("determinism: MISMATCH — two identical runs produced different output");
    }

    // ── sweep ─────────────────────────────────────────────────────────────────
    let mut results: Vec<SweepResult> = Vec::new();

    for &v in &sweep_values {
        let knobs = TrimKnobs { tool_max_desc_chars: v, ..TrimKnobs::default() };

        let mut ratios: Vec<f64> = Vec::new();
        let mut total_before: u64 = 0;
        let mut total_after: u64 = 0;
        let mut dollars: f64 = 0.0;

        for row in &eligible {
            let out = trim_with_knobs(row.body.clone(), TrimProvider::Anthropic, &knobs);
            if out.applied && out.tokens_before > 0 {
                let ratio = (out.tokens_before - out.tokens_after) as f64
                    / out.tokens_before as f64
                    * 100.0;
                ratios.push(ratio);
                total_before += out.tokens_before;
                total_after += out.tokens_after;
                let saved = out.tokens_before - out.tokens_after;
                dollars += saved as f64 * input_rate_per_1m(&row.model) / 1_000_000.0;
            }
        }

        let n_trimmed = ratios.len();
        let avg_pct = if ratios.is_empty() {
            0.0
        } else {
            ratios.iter().sum::<f64>() / ratios.len() as f64
        };
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_pct = percentile_sorted(&ratios, 50.0);
        let p90_pct = percentile_sorted(&ratios, 90.0);
        let tokens_saved = total_before.saturating_sub(total_after);

        results.push(SweepResult {
            desc_chars: v,
            n_eligible: eligible.len(),
            n_trimmed,
            avg_pct,
            median_pct,
            p90_pct,
            tokens_saved,
            dollars_saved: dollars,
        });
    }

    // ── table ─────────────────────────────────────────────────────────────────
    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Trim Backtest — tool_max_desc_chars sweep");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!();
    println!(
        "{:<10}  {:>16}  {:>6}  {:>7}  {:>5}  {:>9}  {:>8}",
        "desc_chars", "trimmed/eligible", "avg%", "median%", "p90%", "tok_saved", "$_saved"
    );
    println!("{}", "─".repeat(71));
    for r in &results {
        println!(
            "{:<10}  {:>16}  {:>6.1}  {:>7.1}  {:>5.1}  {:>9}  {:>8}",
            r.desc_chars,
            format!("{}/{}", r.n_trimmed, r.n_eligible),
            r.avg_pct,
            r.median_pct,
            r.p90_pct,
            fmt_tok(r.tokens_saved),
            format!("${:.4}", r.dollars_saved),
        );
    }
    println!("{}", "─".repeat(71));
    println!();
    println!("Pricing (input only — trim doesn't affect output):  opus=$5/MTok  sonnet=$3/MTok  haiku=$1/MTok  fable=$10/MTok  unknown=$5/MTok");
    println!("avg% = per-request mean of (before-after)/before*100, over trimmed rows only");
    println!();
}
