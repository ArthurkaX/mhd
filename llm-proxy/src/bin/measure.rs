//! measure -- three-run trim measurement using requests-table usage comparison.
//!
//! THIN CLI wrapper around the [`llm_proxy::measure`] library. Parses args,
//! drives the measurement on a background thread, and prints progress to stderr.
//!
//! Usage:
//! measure [--db <path>] [--dry-run] [--side <model>]

use llm_proxy::measure::{ConfirmFn, MeasureConfig, MeasureProgress, run_measurement};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── arg parsing ─────────────────────────────────────────────────────────────────────────

struct Args {
    db_path: PathBuf,
    dry_run: bool,
    side_model: String,
}

fn parse_args() -> Args {
    let default_db = llm_proxy::config::config_dir().join("proxy.db");

    let mut db_path = default_db;
    let mut dry_run = false;
    let mut side_model = String::new();

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
            "--dry-run" => {
                dry_run = true;
            }
            "--side" => {
                i += 1;
                if i < args.len() {
                    side_model = args[i].clone();
                }
            }
            other => {
                eprintln!("Unknown argument: {other} (ignored)");
            }
        }
        i += 1;
    }

    Args {
        db_path,
        dry_run,
        side_model,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────

/// Format a token count into a human-friendly string (e.g. "1.5K", "2.3M").
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Format a duration in milliseconds into a human-friendly string.
fn fmt_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

// ── entry point ─────────────────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();

    let side_substitution = !args.side_model.is_empty();

    let cfg = MeasureConfig {
        db_path: args.db_path,
        dry_run: args.dry_run,
        side_substitution,
        side_model: args.side_model,
    };

    // Build the confirm closure -- prints the S2 prompt and reads stdin 'go'.
    let confirm: ConfirmFn = Box::new(move || -> bool {
        eprintln!("S2: Confirmation");
        eprintln!(" This SPENDS real quota (three unconstrained claude sessions) and");
        eprintln!(" requires NO other traffic on this account while running.");
        eprintln!(" Type 'go' to proceed:");
        eprint!(" > ");
        let _ = std::io::stdout().flush();

        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(_) => {
                let trimmed = input.trim().to_ascii_lowercase();
                trimmed == "go"
            }
            Err(e) => {
                eprintln!("Error reading stdin: {e}. Aborting.");
                false
            }
        }
    });

    let progress = Arc::new(Mutex::new(MeasureProgress::new()));
    let progress_driver = Arc::clone(&progress);
    let cfg_driver = cfg.clone();

    // Spawn the measurement driver on a background thread so we can poll progress.
    let handle = std::thread::Builder::new()
        .name("measure-driver".to_string())
        .spawn(move || run_measurement(&cfg_driver, &progress_driver, confirm))
        .expect("spawn measure-driver thread");

    // Poll progress on the main thread, printing each output line as it appears.
    let mut last_message = String::new();

    loop {
        // Poll the progress state.
        let (finished, message) = {
            let p = progress.lock().unwrap();
            (handle.is_finished(), p.message.clone())
        };

        // Print new output lines.
        if message != last_message {
            if message.is_empty() {
                eprintln!();
            } else {
                eprintln!("{}", message);
            }
            last_message = message;
        }

        if finished {
            break;
        }

        std::thread::sleep(Duration::from_millis(150));
    }

    // Final poll to catch any messages set between our last poll and thread exit.
    {
        let p = progress.lock().unwrap();
        if p.message != last_message {
            if p.message.is_empty() {
                eprintln!();
            } else {
                eprintln!("{}", p.message);
            }
        }
    }

    // Collect the result.
    let result = match handle.join() {
        Ok(r) => r,
        Err(_) => {
            eprintln!("FATAL: measurement driver thread panicked.");
            std::process::exit(1);
        }
    };

    match result {
        Ok(Some(measure_result)) => {
            eprintln!();
            eprintln!("=== Results ===");
            eprintln!("Three-run trim measurement (warm-cache, requests-log based)");
            eprintln!();
            eprintln!("  ECO arm:");
            eprintln!(
                "    {} reqs   input {} + cache_create {}   cache_read {}   elapsed {}",
                measure_result.eco.n_requests,
                fmt_tokens(measure_result.eco.input_tokens),
                fmt_tokens(measure_result.eco.cache_creation_tokens),
                fmt_tokens(measure_result.eco.cache_read_tokens),
                fmt_duration(measure_result.eco.elapsed_ms),
            );
            eprintln!("  NATIVE_ON arm:");
            eprintln!(
                "    {} reqs   input {} + cache_create {}   cache_read {}   elapsed {}",
                measure_result.native_on.n_requests,
                fmt_tokens(measure_result.native_on.input_tokens),
                fmt_tokens(measure_result.native_on.cache_creation_tokens),
                fmt_tokens(measure_result.native_on.cache_read_tokens),
                fmt_duration(measure_result.native_on.elapsed_ms),
            );
            eprintln!("  NATIVE_OFF arm:");
            eprintln!(
                "    {} reqs   input {} + cache_create {}   cache_read {}   elapsed {}",
                measure_result.native_off.n_requests,
                fmt_tokens(measure_result.native_off.input_tokens),
                fmt_tokens(measure_result.native_off.cache_creation_tokens),
                fmt_tokens(measure_result.native_off.cache_read_tokens),
                fmt_duration(measure_result.native_off.elapsed_ms),
            );
            eprintln!();
            eprintln!("  Quota cost (1.25x cc + 0.1x cr):");
            eprintln!(
                "    ECO:         {}  (saved {:.1}% vs native_off)",
                fmt_tokens(measure_result.cost_eco),
                measure_result.eco_saved_pct,
            );
            eprintln!(
                "    NATIVE_ON:   {}  (saved {:.1}% vs native_off)",
                fmt_tokens(measure_result.cost_native_on),
                measure_result.native_saved_pct,
            );
            eprintln!(
                "    NATIVE_OFF:  {}  (baseline)",
                fmt_tokens(measure_result.cost_native_off),
            );
            eprintln!();
            eprintln!(
                "  NATIVE verdict: {}  (trim only, all native)",
                measure_result.native_verdict
            );
            eprintln!(
                "  ECO verdict:    {}  (trim + offload effect)",
                measure_result.eco_verdict
            );
            eprintln!(
                "  Note: three LLM sessions are not byte-identical; n_reqs shown for each so divergence is visible."
            );

            std::process::exit(0);
        }
        Ok(None) => {
            // Aborted -- already printed by the library.
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }
}
