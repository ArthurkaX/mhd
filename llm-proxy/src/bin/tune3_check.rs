//! tune3_check — bucket-aware sweep over tool_result_head.
//!
//! Usage:
//!   tune3_check [--db <path>] [--max-bodies <n>] [--floor <n>]
//!
//! Runs `run_bucket_tune` three times — once per bucket (Native, CcGateway,
//! OtherOpenai) — all sweeping `SweepKnob::ToolResultHead` over a fixed set
//! of values.
//!
//! Deterministic, read-only, no LLM.

use llm_proxy::config::config_dir;
use llm_proxy::native_trim::NativeKnobs;
use llm_proxy::tune::{run_bucket_tune, Bucket, SweepKnob, TuneVerdict};
use std::path::{Path, PathBuf};

/// Fixed sweep over tool_result_head.
const SWEEP: &[usize] = &[500, 1000, 2000, 3000, 5000, 8000];

// ── arg parsing ──────────────────────────────────────────────────────────────

fn parse_args() -> (PathBuf, usize, usize) {
    let default_db = config_dir().join("proxy.db");
    let mut db_path = default_db;
    let mut max_bodies: usize = 120;
    let mut floor: usize = 1000;

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
            "--max-bodies" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<usize>() {
                        Ok(v) => max_bodies = v,
                        _ => eprintln!(
                            "Warning: --max-bodies requires a non-negative integer; using default 120."
                        ),
                    }
                }
            }
            "--floor" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<usize>() {
                        Ok(v) => floor = v,
                        _ => eprintln!(
                            "Warning: --floor requires a positive integer; using default 1000."
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

// ── run one bucket ──────────────────────────────────────────────────────────

fn run_one(
    db_path: &Path,
    base: &NativeKnobs,
    floor: usize,
    max_bodies: usize,
    bucket: Bucket,
) {
    let label = match bucket {
        Bucket::Native => "Native",
        Bucket::CcGateway => "CcGateway",
        Bucket::OtherOpenai => "OtherOpenai",
    };

    match run_bucket_tune(
        db_path,
        base,
        SWEEP,
        floor,
        max_bodies,
        bucket,
        SweepKnob::ToolResultHead,
        |done, total| eprintln!("  [{label}] {done}/{total}"),
    ) {
        Ok(Some(result)) => {
            println!();
            println!(
                "═══════════════════════════════════════════════════════════════════════"
            );
            println!("  Bucket: {label} — tool_result_head sweep");
            println!(
                "═══════════════════════════════════════════════════════════════════════"
            );
            println!(
                "{:<12}  {:>10}  {:>12}  {:>14}",
                "head_value", "avg_trim%", "n_trimmed", "fail_open_ok"
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
                "  baseline:   head_value={}  avg_trim_pct={:.3}%",
                result.baseline_desc_chars, result.baseline_trim_pct
            );
            println!(
                "  recommended: head_value={}  avg_trim_pct={:.3}%",
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
            println!("{label}: empty");
            println!();
        }
        Err(e) => {
            eprintln!("{label}: error — {e}");
        }
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

fn main() {
    let (db_path, floor, max_bodies) = parse_args();
    eprintln!("DB: {}", db_path.display());
    eprintln!("Floor: {floor}");
    eprintln!("Max bodies: {max_bodies}");

    let base = NativeKnobs::default();

    run_one(&db_path, &base, floor, max_bodies, Bucket::Native);
    run_one(&db_path, &base, floor, max_bodies, Bucket::CcGateway);
    run_one(&db_path, &base, floor, max_bodies, Bucket::OtherOpenai);
}
