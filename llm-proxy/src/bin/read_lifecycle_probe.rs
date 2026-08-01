//! read_lifecycle_probe — MEASUREMENT-ONLY probe for the proposed "Read
//! lifecycle" trim lever: replacing the body of a `Read` tool_result with a
//! short marker once that read is known to be obsolete (SUPERSEDED by a later
//! re-read, or STALE after a later Edit/Write).
//!
//! Usage:
//!   read_lifecycle_probe [<db path>] [--db <path>]
//!
//! Reads `request_bodies` (provider='anthropic') from proxy.db, groups rows
//! into per-conversation chains (reusing `cache_bench`'s loader), and classifies
//! every Read tool_result in each chain's LAST request body. Prints the upside
//! (how many read bytes are obsolete), the flip-depth distribution (how deep in
//! the history each flip lands — deep flips force cache re-creation of the
//! whole tail), and the cost trade (bytes invalidated at the 1.25 cache-creation
//! weight vs read bytes reclaimed at the 0.10 cache-read weight).
//!
//! This is a MODEL, not billing data: tokens are ~chars/4 and the cache split
//! is byte-proportional. offset/limit read ranges are IGNORED (any read of a
//! file covers the whole file). Invalidation is DEDUPED per chain: the earliest
//! flip breaks the prefix and already invalidates everything after it, so later
//! flips inside that region add no extra cost.

use llm_proxy::config::config_dir;
use llm_proxy::read_lifecycle::{
    DepthStats, Obsolescence, ReadLifecycleReport, depth_stats, run_read_lifecycle,
};
use std::path::PathBuf;

/// The two headline weights, reused from `cache_bench` so offline numbers stay
/// comparable: cache creation costs ~1.25x a fresh input token, a cache read
/// ~0.10x.
const W_CACHE_CREATION: f64 = 1.25;
const W_CACHE_READ: f64 = 0.10;

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

/// One line of the depth-distribution row.
fn fmt_depth_row(label: &str, s: &DepthStats) -> String {
    format!(
        "  {label:<20}  n={:>4}  min={:>5}  p25={:>5}  median={:>6}  p75={:>5}  max={:>5}",
        s.n, s.min, s.p25, s.median, s.p75, s.max
    )
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
                eprintln!("  read_lifecycle_probe [<db path>] [--db <path>]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    eprintln!("DB: {}", db_path.display());

    let report = match run_read_lifecycle(&db_path) {
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

fn print_report(r: &ReadLifecycleReport) {
    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Read-Lifecycle Obsolescence Probe (MODELED estimate, NOT billing data)");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!();
    println!("  Token model: ~chars/4 over serialized JSON; cache split byte-proportional.");
    println!("  offset/limit read ranges IGNORED: any Read of a file covers the whole file.");
    println!("  Weights: cache-creation 1.25x, cache-read 0.10x (reused from cache_bench).");
    println!();
    println!(
        "  corpus:                 {} chains (last request body per chain)",
        r.n_chains
    );
    println!("  chains with Reads:      {}", r.n_chains_with_reads);
    println!("  chains with flips:      {}", r.n_chains_with_flips);
    println!(
        "  Read tool_results:      {}  ({} still-live)",
        r.n_reads, r.n_live_reads
    );
    println!();

    // ── upside: how much read content is obsolete ───────────────────────────
    println!("  ── UPSIDE: Read tool_result bytes ──────────────────────────────");
    let pct = |x: u64| -> f64 {
        if r.read_bytes_total > 0 {
            x as f64 / r.read_bytes_total as f64 * 100.0
        } else {
            0.0
        }
    };
    println!(
        "  total Read bytes:       {}",
        fmt_bytes(r.read_bytes_total)
    );
    println!(
        "    SUPERSEDED:            {} bytes  ({:>5.2}%)",
        fmt_bytes(r.superseded_bytes),
        pct(r.superseded_bytes)
    );
    println!(
        "    STALE:                 {} bytes  ({:>5.2}%)",
        fmt_bytes(r.stale_bytes),
        pct(r.stale_bytes)
    );
    println!(
        "    still-live:            {} bytes  ({:>5.2}%)",
        fmt_bytes(r.live_bytes),
        pct(r.live_bytes)
    );
    println!();

    // ── flip depth distribution ─────────────────────────────────────────────
    println!("  ── FLIP DEPTH: messages remaining after the flip index ──────────");
    println!("     shallow = cheap (little history to re-create), deep = expensive.");
    println!("     Distribution is per flip event, NOT deduped (each flip has a depth).");
    println!();
    println!(
        "  flip events:            {}   ({} superseded + {} stale)",
        r.n_flips, r.n_superseded_flips, r.n_stale_flips
    );
    match depth_stats(&r.depths) {
        Some(d) => {
            println!("  {}", fmt_depth_row("depth", &d));
            println!();
            println!(
                "     Interpretation: median {} means half of all flips sit no deeper",
                d.median
            );
            println!(
                "     than message index (n_messages - 1 - {}), i.e. within",
                d.median
            );
            println!("     {} messages of the end of the conversation.", d.median);
        }
        None => {
            println!("  depth distribution:     (no flips — nothing to distribute)");
            println!();
        }
    }

    // ── the trade ───────────────────────────────────────────────────────────
    println!("  ── COST TRADE: cache re-creation vs bytes reclaimed ─────────────");
    let benefit_weighted = r.reclaimed_bytes as f64 * W_CACHE_READ;
    let cost_weighted = r.invalidated_deduped_bytes as f64 * W_CACHE_CREATION;
    println!(
        "  Read bytes reclaimed:        {:>10} bytes  (× {W_CACHE_READ} cache-read = {:>10.0} cost-units)",
        fmt_bytes(r.reclaimed_bytes),
        benefit_weighted
    );
    println!(
        "  bytes invalidated (naive):   {:>10} bytes  (sum of bytes-after per flip, NO dedup)",
        fmt_bytes(r.invalidated_naive_bytes)
    );
    println!(
        "  bytes invalidated (deduped): {:>10} bytes  (per chain: bytes after the EARLIEST flip)",
        fmt_bytes(r.invalidated_deduped_bytes)
    );
    println!(
        "  invalidated × {W_CACHE_CREATION} (cache-create): {:>10.0} cost-units",
        cost_weighted
    );
    println!();
    println!("     Dedup rule: an earlier flip already invalidates everything after it, so a");
    println!("     later flip inside that region adds 0 extra. The deduped line is the honest");
    println!("     cost side; the naive line exists only to show what naive summation would");
    println!("     overstate.");
    if r.invalidated_naive_bytes > r.invalidated_deduped_bytes {
        println!(
            "     (naive overstates the cost side by {:.2}× on this corpus)",
            r.invalidated_naive_bytes as f64 / r.invalidated_deduped_bytes.max(1) as f64
        );
    }
    println!();
    if r.reclaimed_bytes == 0 && r.invalidated_deduped_bytes == 0 {
        println!("     No obsolete reads found — the corpus does not support a conclusion");
        println!("     about this lever either way.");
    } else if cost_weighted > benefit_weighted {
        println!(
            "     TRADE: cost side ({cost_weighted:.0}) exceeds the benefit side ({benefit_weighted:.0}) —"
        );
        println!("     at gross weights the lever looks EXPENSIVE; verify on the depth split.");
    } else {
        println!(
            "     TRADE: benefit side ({benefit_weighted:.0}) at least matches the cost side ({cost_weighted:.0}) —"
        );
        println!(
            "     at gross weights the lever is not obviously unaffordable; verify on the depth split."
        );
    }
    println!();

    // ── top-10 worst flips ──────────────────────────────────────────────────
    println!("  ── TOP-10 WORST FLIPS (by raw bytes invalidated) ────────────────");
    println!("     * = dominated: an earlier flip in the same chain already breaks the cache,");
    println!("       so this flip adds 0 to the deduped cost side.");
    println!();
    println!(
        "  {:<30}  {:>5}  {:>6}  {:>10}  {:>12}  {:>5}  {:>4}",
        "chain (run/session)", "msg", "depth", "reclaimed", "invalidated", "kind", "dom"
    );
    println!("  {}", "─".repeat(86));
    for f in r.flips.iter().take(10) {
        let kind = match f.obsolescence {
            Obsolescence::Superseded => "S",
            Obsolescence::Stale => "T",
            Obsolescence::Live => "L",
        };
        println!(
            "  {:<30}  {:>5}  {:>6}  {:>10}  {:>12}  {:>5}  {:>4}",
            f.chain_key,
            f.message_index,
            f.depth,
            fmt_bytes(f.content_bytes),
            fmt_bytes(f.bytes_after),
            kind,
            if f.dominated { "*" } else { "" }
        );
    }
    println!();
    println!("  (kind: S=superseded, T=stale; dom *= dominated by an earlier flip in the same");
    println!("   chain; invalidated = bytes of messages after the flip index, RAW not deduped)");
    println!();
    println!("  Honesty notes:");
    println!("    - Invalidation EXCLUDES the flipped message itself: only bytes strictly after");
    println!("      the flip index are counted, which UNDERSTATES the cost side.");
    println!("    - Both headline sides are GROSS weights, not the net (1.25 − 0.10) marginal");
    println!("      comparison: invalidated bytes would otherwise have been cache-read at 0.10x,");
    println!("      so the true one-shot cost of a break is closer to 1.15x per invalidated byte.");
    println!("      The reclaimed side (0.10x) is exactly what those bytes would have cost as a");
    println!("      cache read, so it is the right per-byte saving in an intact region.");
    println!("    - The depth split is the discriminating number: shallow flips (depth < ~30) are");
    println!("      cheap, deep flips (100+) dominate the invalidated total.");
}
