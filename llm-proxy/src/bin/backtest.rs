//! backtest — re-apply candidate trim configurations to the replay corpus.
//!
//! Usage:
//!   backtest [--db <path>] [--desc-chars <comma-list>] [--engine <native>]
//!            [--provider <anthropic|openai>]
//!
//! Reads all rows from `request_bodies` in proxy.db, sweeps the
//! `tool_max_desc_chars` lever across the specified values, and prints a table
//! showing savings at each setting so the autotune curve is visible without
//! calling the real LLM.
//!
//! The corpus is populated when `corpus_capture = true` in llm-proxy settings.
//! --provider selects which corpus rows to measure (default: anthropic).
//! When --provider openai is given, the native engine calls trim_native_openai
//! instead of trim_native.

use llm_proxy::config::config_dir;
use llm_proxy::db_log::decompress_body;
use llm_proxy::native_trim::{NativeKnobs, trim_native_openai};
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

// ── unified token estimator ───────────────────────────────────────────────────

/// Unified token estimator (~4 chars/token over the serialized body).
/// Unified token estimator (~4 chars/token).
fn est_tokens(v: &serde_json::Value) -> u64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) as u64) / 4
}

// ── engine dispatch ───────────────────────────────────────────────────────────

/// Produce a trimmed body for a given provider and desc_chars sweep value.
/// - provider="openai"  → trim_native_openai
/// - provider="anthropic" → trim_native
fn apply_engine(
    _engine: &str,
    provider: &str,
    body: Value,
    v: usize,
    ws_on: bool,
    strip_thinking_on: bool,
    fence_requires_code: bool,
    arrow_density_min: f64,
) -> Value {
    let knobs = NativeKnobs {
        tool_max_desc_chars: v,
        ws_enabled: ws_on,
        strip_thinking: strip_thinking_on,
        tool_result_fence_requires_code: fence_requires_code,
        tool_result_arrow_density_min: arrow_density_min,
        ..NativeKnobs::default()
    };
    if provider == "openai" {
        trim_native_openai(body, &knobs)
    } else {
        llm_proxy::native_trim::trim_native(body, &knobs)
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

    let mut stmt =
        match conn.prepare("SELECT model, provider, body FROM request_bodies ORDER BY id") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

    let mut rows: Vec<BodyRow> = Vec::new();
    let mut parse_errors: usize = 0;

    let iter = stmt.query_map([], |row| {
        let model: String = row.get(0)?;
        let provider: String = row.get(1)?;
        let body_blob: Vec<u8> = row.get(2)?;
        Ok((model, provider, body_blob))
    });

    if let Ok(iter) = iter {
        for item in iter {
            match item {
                Ok((model, provider, body_blob)) => {
                    let body_str = match decompress_body(&body_blob) {
                        Some(s) => s,
                        None => {
                            parse_errors += 1;
                            continue;
                        }
                    };
                    match serde_json::from_str::<Value>(&body_str) {
                        Ok(body) => rows.push(BodyRow {
                            model,
                            provider,
                            body,
                        }),
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

fn parse_args() -> (
    PathBuf,
    Vec<usize>,
    String,
    bool,
    bool,
    String,
    bool,
    f64,
    usize,
) {
    let default_db = config_dir().join("proxy.db");
    let default_sweep: Vec<usize> = vec![40, 80, 120, 150, 200, 300];
    let default_engine = "native".to_string();
    let default_provider = "anthropic".to_string();

    let mut db_path = default_db;
    let mut sweep = default_sweep;
    let mut engine = default_engine;
    let mut ws_on = false;
    let mut strip_thinking_on = false;
    let mut provider = default_provider;
    // Mirror NativeKnobs::default() so backtest matches live behavior unless overridden.
    let mut fence_requires_code = true;
    let mut arrow_density_min = 0.01;
    // 0 = use all eligible bodies. >0 = deterministic uniform subsample for a
    // fast confirmatory run (avg-of-ratios is sample-stable; trades CI width for
    // speed on cumulative-history corpora where full sweeps are O(turns^2)).
    let mut max_bodies: usize = 0;

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
            "--engine" => {
                i += 1;
                if i < args.len() {
                    let val = args[i].as_str();
                    match val {
                        "native" => engine = val.to_string(),
                        other => {
                            eprintln!(
                                "Warning: unknown --engine value '{other}'; using default 'native'."
                            );
                        }
                    }
                }
            }
            "--provider" => {
                i += 1;
                if i < args.len() {
                    let val = args[i].as_str();
                    match val {
                        "anthropic" | "openai" => provider = val.to_string(),
                        other => {
                            eprintln!(
                                "Warning: unknown --provider value '{other}'; expected 'anthropic' or 'openai'. Using 'anthropic'."
                            );
                        }
                    }
                }
            }
            "--ws" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "on" => ws_on = true,
                        "off" => ws_on = false,
                        other => {
                            eprintln!(
                                "Warning: unknown --ws value '{other}'; expected 'on' or 'off'. Using 'off'."
                            );
                        }
                    }
                }
            }
            "--strip-thinking" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "on" => strip_thinking_on = true,
                        "off" => strip_thinking_on = false,
                        other => {
                            eprintln!(
                                "Warning: unknown --strip-thinking value '{other}'; expected 'on' or 'off'. Using 'off'."
                            );
                        }
                    }
                }
            }
            "--fence-requires-code" => {
                i += 1;
                if i < args.len() {
                    match args[i].as_str() {
                        "on" => fence_requires_code = true,
                        "off" => fence_requires_code = false,
                        other => {
                            eprintln!(
                                "Warning: unknown --fence-requires-code value '{other}'; expected 'on' or 'off'. Using 'on'."
                            );
                        }
                    }
                }
            }
            "--arrow-density-min" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<f64>() {
                        Ok(v) if v >= 0.0 => arrow_density_min = v,
                        _ => eprintln!(
                            "Warning: --arrow-density-min requires a non-negative f64; using default 0.01."
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
                            "Warning: --max-bodies requires a non-negative integer; using 0 (all bodies)."
                        ),
                    }
                }
            }
            other => {
                eprintln!("Unknown argument: {other}  (ignored)");
            }
        }
        i += 1;
    }

    (
        db_path,
        sweep,
        engine,
        ws_on,
        strip_thinking_on,
        provider,
        fence_requires_code,
        arrow_density_min,
        max_bodies,
    )
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() {
    let (
        db_path,
        sweep_values,
        engine,
        ws_on,
        strip_thinking_on,
        provider,
        fence_requires_code,
        arrow_density_min,
        max_bodies,
    ) = parse_args();

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

    // Eligible = rows matching the selected provider with parseable body.
    let mut eligible: Vec<&BodyRow> = rows.iter().filter(|r| r.provider == provider).collect();
    let n_other = rows.iter().filter(|r| r.provider != provider).count();
    let n_eligible_full = eligible.len();

    // Optional deterministic uniform subsample (--max-bodies N). Picks every
    // k-th body so the sample spans all sessions/sizes rather than one prefix,
    // keeping avg-of-ratios representative. No RNG → reproducible.
    if max_bodies > 0 && eligible.len() > max_bodies {
        let step = eligible.len() as f64 / max_bodies as f64;
        eligible = (0..max_bodies)
            .map(|i| eligible[(i as f64 * step) as usize])
            .collect();
    }

    eprintln!(
        "Corpus: {} total rows  ({} {provider} eligible{}, {} other skipped)",
        rows.len(),
        n_eligible_full,
        if eligible.len() != n_eligible_full {
            format!(" → sampled {}", eligible.len())
        } else {
            String::new()
        },
        n_other,
    );

    if eligible.is_empty() {
        println!();
        println!("No {provider} rows in corpus.");
        if provider == "openai" {
            println!("Generate some OpenAI-path traffic (Zed, opencode, pi) via the proxy,");
            println!("then re-run with --provider openai.");
        } else {
            println!("Generate some Claude Code traffic via the Anthropic path, then re-run.");
        }
        return;
    }

    // ── determinism sanity check (uses the selected engine + provider) ────────
    let det_v: usize = 150;
    let first_body = eligible[0].body.clone();
    let det_a = apply_engine(
        &engine,
        &provider,
        first_body.clone(),
        det_v,
        ws_on,
        strip_thinking_on,
        fence_requires_code,
        arrow_density_min,
    );
    let det_b = apply_engine(
        &engine,
        &provider,
        first_body,
        det_v,
        ws_on,
        strip_thinking_on,
        fence_requires_code,
        arrow_density_min,
    );
    if det_a == det_b {
        println!("determinism: OK");
    } else {
        println!("determinism: MISMATCH — two identical runs produced different output");
    }

    // ── sweep ─────────────────────────────────────────────────────────────────
    let mut results: Vec<SweepResult> = Vec::new();
    let sweep_start = std::time::Instant::now();

    for &v in &sweep_values {
        let mut ratios: Vec<f64> = Vec::new();
        let mut tokens_saved: u64 = 0;
        let mut dollars: f64 = 0.0;
        let val_start = std::time::Instant::now();
        eprint!("  [sweep] desc_chars={v} ");

        for (i, row) in eligible.iter().enumerate() {
            // Coarse heartbeat so a single slow value still visibly progresses.
            if i % 200 == 0 && i > 0 {
                eprint!(".");
            }
            let trimmed_body = apply_engine(
                &engine,
                &provider,
                row.body.clone(),
                v,
                ws_on,
                strip_thinking_on,
                fence_requires_code,
                arrow_density_min,
            );
            let before = est_tokens(&row.body);
            let after = est_tokens(&trimmed_body);
            if after < before {
                let ratio = (before - after) as f64 / before as f64 * 100.0;
                ratios.push(ratio);
                tokens_saved += before - after;
                dollars += (before - after) as f64 * input_rate_per_1m(&row.model) / 1_000_000.0;
            }
        }
        eprintln!(
            " done in {:.1}s ({} bodies)",
            val_start.elapsed().as_secs_f64(),
            eligible.len()
        );

        let n_trimmed = ratios.len();
        let avg_pct = if ratios.is_empty() {
            0.0
        } else {
            ratios.iter().sum::<f64>() / ratios.len() as f64
        };
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_pct = percentile_sorted(&ratios, 50.0);
        let p90_pct = percentile_sorted(&ratios, 90.0);

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
    eprintln!(
        "  [sweep] total {:.1}s",
        sweep_start.elapsed().as_secs_f64()
    );

    // ── table ─────────────────────────────────────────────────────────────────
    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Trim Backtest — tool_max_desc_chars sweep");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!(
        "  engine: {engine}   provider: {provider}   ws: {}   strip_thinking: {}   fence_requires_code: {}   arrow_density_min: {arrow_density_min}   estimator: ~chars/4 (unified)",
        if ws_on { "on" } else { "off" },
        if strip_thinking_on { "on" } else { "off" },
        if fence_requires_code { "on" } else { "off" }
    );
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
    println!(
        "Pricing (input only — trim doesn't affect output):  opus=$5/MTok  sonnet=$3/MTok  haiku=$1/MTok  fable=$10/MTok  unknown=$5/MTok"
    );
    println!("avg% = per-request mean of (before-after)/before*100, over trimmed rows only");
    println!();
    let _ = std::io::Write::flush(&mut std::io::stdout());
}
