//! content_mix — MEASUREMENT-ONLY probe: what is our tool_result traffic made
//! of? Reads `request_bodies` (provider='anthropic') from proxy.db, classifies
//! every `tool_result` block's text by content type, and reports the byte mix
//! over the whole corpus and over the subset large enough for trim to touch.
//!
//! Usage:
//!   content_mix [<db path>] [--db <path>]
//!
//! The classification is heuristic. The report is honest about that: a block
//! whose detectors all miss lands in "prose / other", and a block where two
//! detectors fire at comparable strength lands in "ambiguous / unclassified".
//! See `llm-proxy/src/content_mix.rs` for the full detector rules.
//!
//! This is a MODEL, not billing data: "bytes" = chars of the extracted text,
//! and every block appearance in every request body counts (the transmitted-
//! bytes view — persistent blocks re-count in each later body).

use llm_proxy::config::config_dir;
use llm_proxy::content_mix::{
    ClassStats, ContentClass, ContentMixReport, LARGE_BYTES, run_content_mix,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// Compact byte formatting shared with the sibling offline tools (2.3K, 4.1M).
fn fmt_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Median as a display string (`-` when empty).
fn fmt_median(n: Option<usize>) -> String {
    match n {
        Some(v) => fmt_bytes(v as u64),
        None => "-".to_string(),
    }
}

fn main() {
    let mut db_path = config_dir().join("proxy.db");
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--db" => {
                i += 1;
                if i < args.len() {
                    db_path = PathBuf::from(&args[i]);
                } else {
                    eprintln!("Error: --db requires a value");
                    std::process::exit(1);
                }
            }
            other if !other.starts_with('-') => db_path = PathBuf::from(other),
            other => {
                eprintln!("Error: unknown argument: {other}");
                eprintln!();
                eprintln!("Usage:");
                eprintln!("  content_mix [<db path>] [--db <path>]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    eprintln!("DB: {}", db_path.display());

    let report = match run_content_mix(&db_path) {
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

    print_report(&report);
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn print_report(r: &ContentMixReport) {
    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Content Mix — tool_result blocks by content type (MEASUREMENT ONLY)");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!(
        "  corpus:        {} chains, {} request bodies",
        r.n_chains, r.n_bodies
    );
    println!("  blocks:        {} tool_results with text", r.n_blocks);
    println!("  total bytes:   {}", fmt_bytes(r.total_bytes));
    println!(
        "  large subset:  {} blocks >= {LARGE_BYTES} chars ({} bytes)",
        r.n_large_blocks,
        fmt_bytes(r.large_bytes)
    );
    println!();
    println!("  units:     \"bytes\" = chars of the extracted tool_result text — the unit");
    println!("             tool_result_min_elide gates on. Blocks with no text excluded.");
    println!("  counting:  every block appearance in every request body counts, so a");
    println!("             persistent block re-counts in each later body (transmitted-");
    println!("             bytes view — what the proxy sends and what cost is set by).");
    println!("             \"distinct\" = unique block contents by text hash.");
    println!("  protected: tool_result_protected with NativeKnobs::default() knobs");
    println!("             (fence_requires_code=true, arrow_density_min=0.01).");
    println!("  classes:   heuristic line-shape detectors; AMBIGUOUS = two detectors");
    println!("             fired at comparable strength; PROSE / OTHER = nothing fired");
    println!("             (the honest catch-all — includes machine output with no");
    println!("             detectable structure). Detector rules: src/content_mix.rs.");
    println!("  large bar: LARGE_BYTES (= tool_result_min_elide). The actual head/tail");
    println!("             elision gate also needs total >= head+tail+min_elide = 8000");
    println!("             at the default head=3000/tail=1000, so the 4000-bar is the");
    println!("             min_elide-only view (a superset of what trim would touch).");
    println!();

    println!("  ---- ALL BLOCKS ----");
    print_table(&r.all);
    println!();
    println!("  ---- BLOCKS >= {LARGE_BYTES} chars (the actionable subset) ----");
    print_table(&r.large);
    println!();
    print_tops(r);
}

fn print_table(stats: &HashMap<ContentClass, ClassStats>) {
    println!(
        "  {:<24}  {:>7}  {:>8}  {:>10}  {:>6}  {:>8}  {:>11}  {:>6}",
        "class", "blocks", "distinct", "bytes", "%", "median", "protected", "prot%"
    );
    println!("  {}", "─".repeat(96));
    let total: u64 = stats.values().map(|s| s.bytes).sum();
    for cls in ContentClass::ALL_CLASSES {
        let Some(s) = stats.get(&cls) else { continue };
        let pct = if total > 0 {
            s.bytes as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        let prot_pct = if s.bytes > 0 {
            s.protected_bytes as f64 / s.bytes as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  {:<24}  {:>7}  {:>8}  {:>10}  {:>5.2}%  {:>8}  {:>11}  {:>5.2}%",
            cls.label(),
            s.blocks,
            s.distinct,
            fmt_bytes(s.bytes),
            pct,
            fmt_median(s.median_size()),
            fmt_bytes(s.protected_bytes),
            prot_pct
        );
    }
    println!("  {}", "─".repeat(96));
    println!(
        "  {:<24}  {:>7}  {:>8}  {:>10}  {:>5.2}%  {:>8}  {:>11}",
        "total",
        stats.values().map(|s| s.blocks).sum::<usize>(),
        stats.values().map(|s| s.distinct).sum::<usize>(),
        fmt_bytes(total),
        100.0,
        "-",
        fmt_bytes(stats.values().map(|s| s.protected_bytes).sum::<u64>())
    );
    println!();
    println!("  (prot% = protected bytes / class bytes; a class already >= 100% protected is");
    println!("   unreachable for further compression without changing the protection policy.)");
    println!();
}

fn print_tops(r: &ContentMixReport) {
    println!("  TOP-5 LARGEST DISTINCT BLOCKS PER CLASS (bytes = block size, ×n = appearances,");
    println!("  excerpts truncated to ~100 chars with secret-looking runs redacted)");
    println!();
    for cls in ContentClass::ALL_CLASSES {
        let Some(s) = r.all.get(&cls) else { continue };
        if s.largest.is_empty() {
            continue;
        }
        println!("  {}:", cls.label());
        for (i, lb) in s.largest.iter().enumerate() {
            println!(
                "    {}. {:>9} B  ×{:>3} app  run={} seq={}",
                i + 1,
                lb.bytes,
                lb.appearances,
                lb.run_id,
                lb.seq
            );
            println!("       {}", lb.excerpt);
        }
        println!();
    }
}
