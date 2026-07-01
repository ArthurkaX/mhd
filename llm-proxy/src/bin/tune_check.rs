//! tune_check — offline tune-advisor sweep over tool_max_desc_chars.
//!
//! Usage:
//!   tune_check [--db <path>] [--floor <usize>] [--max-bodies <usize>]
//!
//! Reads all anthropic request bodies from the replay corpus, sweeps
//! tool_max_desc_chars across a fixed set of values, and prints a table
//! with the recommended knee value.
//!
//! Deterministic, read-only, no LLM.

use llm_proxy::config::config_dir;
use llm_proxy::native_trim::NativeKnobs;
use llm_proxy::tune::{run_tune, TuneVerdict};
use std::path::PathBuf;

/// Fixed sweep values (aggressive → conservative).
const DEFAULT_SWEEP: &[usize] = &[80, 100, 120, 150, 200, 300, 500];

// ── arg parsing ──────────────────────────────────────────────────────────────

fn parse_args() -> (PathBuf, usize, usize) {
    let default_db = config_dir().join("proxy.db");
    let mut db_path = default_db;
    let mut floor: usize = 80;
    let mut max_bodies: usize = 200;

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
            "--floor" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<usize>() {
                        Ok(v) => floor = v,
                        _ => eprintln!(
                            "Warning: --floor requires a positive integer; using default 80."
                        ),
                    }
                }
            }
            "--max-bodies" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<usize>() {
                        Ok(v) => max_bodies = v,
                        _ => eprintln!(
                            "Warning: --max-bodies requires a non-negative integer; using default 200."
                        ),
                    }
                }
            }
            other => {
                eprintln!("Unknown argument: {other} (ignored)");
            }
        }
        i += 1;
    }
    (db_path, floor, max_bodies)
}

// ── entry point ──────────────────────────────────────────────────────────────

fn main() {
    let (db_path, floor, max_bodies) = parse_args();
    eprintln!("DB: {}", db_path.display());
    eprintln!("Floor desc_chars: {floor}");
    eprintln!("Max bodies: {max_bodies}");

    let base = NativeKnobs::default();
    match run_tune(&db_path, &base, DEFAULT_SWEEP, floor, max_bodies, |done, total| {
        eprintln!("  sweep {done}/{total}...");
    }) {
        Ok(Some(result)) => {
            println!();
            println!(
                "═══════════════════════════════════════════════════════════════════════"
            );
            println!("  Tune Advisor — tool_max_desc_chars sweep");
            println!(
                "═══════════════════════════════════════════════════════════════════════"
            );
            println!(
                "{:<12}  {:>10}  {:>12}  {:>14}",
                "desc_chars", "avg_trim%", "n_trimmed", "fail_open_ok"
            );
            println!("{}", "─".repeat(52));
            for p in &result.points {
                println!(
                    "{:<12}  {:>10.3}  {:>12}  {:>14}",
                    p.desc_chars,
                    p.avg_trim_pct,
                    p.n_trimmed,
                    if p.fail_open_ok { "OK" } else { "FAIL" },
                );
            }
            println!("{}", "─".repeat(52));
            println!();
            println!("  n_bodies:   {}", result.n_bodies);
            println!(
                "  baseline:   desc_chars={}  avg_trim_pct={:.3}%",
                result.baseline_desc_chars, result.baseline_trim_pct
            );
            println!(
                "  recommended: desc_chars={}  avg_trim_pct={:.3}%",
                result.recommended, result.recommended_trim_pct
            );
            println!(
                "  delta (rec - base): {:.3}pct",
                result.recommended_trim_pct - result.baseline_trim_pct
            );
            println!("  elapsed:    {}ms", result.elapsed_ms);
            let gloss = match result.verdict {
                TuneVerdict::Worthwhile => {
                    "a clear gain — applying the recommendation is advised."
                }
                TuneVerdict::Marginal => {
                    "small gain, or only via an aggressive cut — keeping the current setting is reasonable."
                }
                TuneVerdict::NotWorth => {
                    "current setting is near-optimal — no change recommended."
                }
            };
            println!("  verdict:    {:?}  — {gloss}", result.verdict);
            println!();
        }
        Ok(None) => {
            println!();
            println!("Corpus empty — no anthropic bodies.");
            println!();
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
