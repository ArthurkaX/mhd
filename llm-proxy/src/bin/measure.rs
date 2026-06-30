//! measure -- two-run trim measurement using requests-table usage comparison.
//!
//! THIN CLI wrapper around the [`llm_proxy::measure`] library. Parses args,
//! drives the measurement on a background thread, and prints progress to stderr.
//!
//! Usage:
//! measure [--db <path>] [--dry-run]

use llm_proxy::measure::{
    ConfirmFn, MeasureConfig, MeasureProgress, run_measurement,
};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── arg parsing ─────────────────────────────────────────────────────────────────────────

struct Args {
    db_path: PathBuf,
    dry_run: bool,
}

fn parse_args() -> Args {
    let default_db = llm_proxy::config::config_dir().join("proxy.db");

    let mut db_path = default_db;
    let mut dry_run = false;

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
            other => {
                eprintln!("Unknown argument: {other} (ignored)");
            }
        }
        i += 1;
    }

    Args { db_path, dry_run }
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

// ── entry point ─────────────────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();

    let cfg = MeasureConfig {
        db_path: args.db_path,
        dry_run: args.dry_run,
    };

    // Build the confirm closure -- prints the S2 prompt and reads stdin 'go'.
    let confirm: ConfirmFn = Box::new(move || -> bool {
        eprintln!("S2: Confirmation");
        eprintln!(" This SPENDS real quota (two unconstrained claude sessions) and");
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
            eprintln!("═══ Results ═══");
            eprintln!("Two-run trim measurement (warm-cache, requests-log based)");
            eprintln!(
                "  OFF arm: {} reqs   input {} + cache_create {}   cache_read {}",
                measure_result.off.n_requests,
                fmt_tokens(measure_result.off.input_tokens),
                fmt_tokens(measure_result.off.cache_creation_tokens),
                fmt_tokens(measure_result.off.cache_read_tokens),
            );
            eprintln!(
                "  ON  arm: {} reqs   input {} + cache_create {}   cache_read {}",
                measure_result.on.n_requests,
                fmt_tokens(measure_result.on.input_tokens),
                fmt_tokens(measure_result.on.cache_creation_tokens),
                fmt_tokens(measure_result.on.cache_read_tokens),
            );
            eprintln!(
                "  Quota cost (1.25x cc + 0.1x cr): OFF {}  ON {}   saved {:.1}%",
                fmt_tokens(measure_result.billed_input_off),
                fmt_tokens(measure_result.billed_input_on),
                measure_result.input_saved_pct,
            );
            eprintln!(
                "  Trim raw (ON): before {} after {}   {:.1}%",
                fmt_tokens(measure_result.on.trim_before),
                fmt_tokens(measure_result.on.trim_after),
                measure_result.trim_raw_pct,
            );
            eprintln!("  Verdict: {}", measure_result.verdict);
            eprintln!(
                "  Note: two LLM sessions are not byte-identical; n_reqs shown for both so divergence is visible."
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
