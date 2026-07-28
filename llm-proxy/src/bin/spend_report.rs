//! spend_report — read Claude Code session transcripts and report actual historical spend.
//!
//! Usage:
//!   spend_report [DIR]
//!
//! DIR defaults to %USERPROFILE%\.claude\projects (Windows) or ~/.claude/projects.
//! Walks all *.jsonl files recursively; skips any malformed lines without panicking.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

// ── provider-agnostic usage record ───────────────────────────────────────────

/// One billing event from any supported transcript source.
/// Fields map directly to Anthropic token categories; future providers can
/// populate the same struct through their own reader function.
#[derive(Debug, Default)]
struct UsageRecord {
    model: String,
    /// YYYY-MM-DD (UTC) — extracted from the ISO-8601 timestamp string.
    date: String,
    /// Fresh (uncached) input tokens billed at base input rate.
    input: u64,
    /// Output tokens.
    output: u64,
    /// Cache-read tokens (billed at 0.1× input rate).
    cache_read: u64,
    /// Ephemeral 5-minute cache creation (billed at 1.25× input rate).
    cache_5m: u64,
    /// Ephemeral 1-hour cache creation (billed at 2.0× input rate).
    cache_1h: u64,
}

// ── Claude Code JSONL reader ──────────────────────────────────────────────────

/// Read all Claude Code assistant records from all *.jsonl files under `dir`.
/// This is the only provider-specific function; the aggregator below works on
/// `UsageRecord` slices from any source.
fn read_claude_code_jsonl(dir: &Path) -> Vec<UsageRecord> {
    let mut records = Vec::new();
    walk_jsonl(dir, &mut records);
    records
}

fn walk_jsonl(dir: &Path, out: &mut Vec<UsageRecord>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            parse_jsonl_file(&path, out);
        }
    }
}

fn parse_jsonl_file(path: &Path, out: &mut Vec<UsageRecord>) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Never panic — skip malformed lines.
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only assistant messages carry billing data.
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }

        // Timestamp must exist and be long enough for YYYY-MM-DD.
        let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
        if ts.len() < 10 {
            continue;
        }
        let date = ts[..10].to_string();

        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let model = msg
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };

        let input = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Prefer the fine-grained cache_creation sub-object (5m vs 1h tiers).
        // Fall back to legacy cache_creation_input_tokens treated as 1h.
        let (cache_5m, cache_1h) = if let Some(cc) = usage.get("cache_creation") {
            let m5 = cc
                .get("ephemeral_5m_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let h1 = cc
                .get("ephemeral_1h_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (m5, h1)
        } else {
            let total = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (0, total) // conservative: assume 1h (higher rate)
        };

        out.push(UsageRecord {
            model,
            date,
            input,
            output,
            cache_read,
            cache_5m,
            cache_1h,
        });
    }
}

// ── pricing ───────────────────────────────────────────────────────────────────

/// Returns `(in_per_1m, out_per_1m)` in USD for a known model family, or `None`.
/// Matching is substring-based on the lower-cased model id.
fn price_per_1m(model: &str) -> Option<(f64, f64)> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        Some((5.0, 25.0))
    } else if m.contains("sonnet") {
        Some((3.0, 15.0))
    } else if m.contains("haiku") {
        Some((1.0, 5.0))
    } else if m.contains("fable") {
        Some((10.0, 50.0))
    } else {
        None
    }
}

/// Compute cost for one record.  Returns `None` if the model is unknown.
fn record_cost(r: &UsageRecord) -> Option<f64> {
    let (inp, out) = price_per_1m(&r.model)?;
    let cost = (r.input as f64 * inp
        + r.output as f64 * out
        + r.cache_read as f64 * (0.1 * inp)
        + r.cache_5m as f64 * (1.25 * inp)
        + r.cache_1h as f64 * (2.0 * inp))
        / 1_000_000.0;
    Some(cost)
}

// ── aggregation ───────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct Totals {
    cost: f64,
    /// True if at least one record contributed a known cost.
    has_known_cost: bool,
    /// True if at least one record had an unknown model (cost was skipped).
    has_unknown_model: bool,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_5m: u64,
    cache_1h: u64,
}

impl Totals {
    fn add_record(&mut self, r: &UsageRecord) {
        match record_cost(r) {
            Some(c) => {
                self.cost += c;
                self.has_known_cost = true;
            }
            None => {
                self.has_unknown_model = true;
            }
        }
        self.input += r.input;
        self.output += r.output;
        self.cache_read += r.cache_read;
        self.cache_5m += r.cache_5m;
        self.cache_1h += r.cache_1h;
    }

    fn cost_str(&self) -> String {
        if self.has_known_cost && self.has_unknown_model {
            format!("${:.4}+?", self.cost)
        } else if self.has_known_cost {
            format!("${:.4}", self.cost)
        } else {
            "(unknown)".to_string()
        }
    }
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

// ── report ────────────────────────────────────────────────────────────────────

fn print_report(records: &[UsageRecord]) {
    let mut by_day: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_model: HashMap<String, Totals> = HashMap::new();
    let mut grand = Totals::default();

    for r in records {
        by_day.entry(r.date.clone()).or_default().add_record(r);
        by_model.entry(r.model.clone()).or_default().add_record(r);
        grand.add_record(r);
    }

    println!("═══════════════════════════════════════════════════════════════════════════");
    println!("  Claude Code Spend Report");
    println!("═══════════════════════════════════════════════════════════════════════════");
    println!();

    // ── daily breakdown ───────────────────────────────────────────────────────
    println!("Daily breakdown");
    println!("{}", "─".repeat(75));
    println!(
        "{:<12}  {:>11}  {:>9} {:>9} {:>9} {:>7} {:>7}",
        "Date", "Cost", "In", "Out", "CacheRd", "C5m", "C1h"
    );
    println!("{}", "─".repeat(75));
    for (day, t) in &by_day {
        println!(
            "{:<12}  {:>11}  {:>9} {:>9} {:>9} {:>7} {:>7}",
            day,
            t.cost_str(),
            fmt_tok(t.input),
            fmt_tok(t.output),
            fmt_tok(t.cache_read),
            fmt_tok(t.cache_5m),
            fmt_tok(t.cache_1h)
        );
    }
    println!("{}", "─".repeat(75));
    println!(
        "{:<12}  {:>11}  {:>9} {:>9} {:>9} {:>7} {:>7}",
        "TOTAL",
        grand.cost_str(),
        fmt_tok(grand.input),
        fmt_tok(grand.output),
        fmt_tok(grand.cache_read),
        fmt_tok(grand.cache_5m),
        fmt_tok(grand.cache_1h)
    );

    println!();

    // ── per-model totals ──────────────────────────────────────────────────────
    println!("Per-model totals");
    println!("{}", "─".repeat(85));
    println!(
        "{:<32}  {:>11}  {:>9} {:>9} {:>9} {:>7} {:>7}",
        "Model", "Cost", "In", "Out", "CacheRd", "C5m", "C1h"
    );
    println!("{}", "─".repeat(85));
    let mut models: Vec<(&String, &Totals)> = by_model.iter().collect();
    // Sort by cost descending, unknown (cost=0) at bottom.
    models.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });
    for (model, t) in &models {
        println!(
            "{:<32}  {:>11}  {:>9} {:>9} {:>9} {:>7} {:>7}",
            model,
            t.cost_str(),
            fmt_tok(t.input),
            fmt_tok(t.output),
            fmt_tok(t.cache_read),
            fmt_tok(t.cache_5m),
            fmt_tok(t.cache_1h)
        );
    }

    println!();

    // ── grand total ───────────────────────────────────────────────────────────
    println!("═══════════════════════════════════════════════════════════════════════════");
    println!("  Grand total: {}", grand.cost_str());
    println!(
        "  Tokens — in: {}  out: {}  cache_read: {}  c5m: {}  c1h: {}",
        fmt_tok(grand.input),
        fmt_tok(grand.output),
        fmt_tok(grand.cache_read),
        fmt_tok(grand.cache_5m),
        fmt_tok(grand.cache_1h)
    );
    println!("═══════════════════════════════════════════════════════════════════════════");
    println!();

    // ── pricing notes ─────────────────────────────────────────────────────────
    println!("Pricing notes:");
    println!("  cache_read           billed at 0.1× input rate");
    println!("  ephemeral_5m (C5m)   billed at 1.25× input rate");
    println!("  ephemeral_1h (C1h)   billed at 2.0× input rate");

    // Cache creation spend analysis
    let cache_5m_cost: f64 = records
        .iter()
        .filter_map(|r| {
            price_per_1m(&r.model).map(|(inp, _)| r.cache_5m as f64 * 1.25 * inp / 1_000_000.0)
        })
        .sum();
    let cache_1h_cost: f64 = records
        .iter()
        .filter_map(|r| {
            price_per_1m(&r.model).map(|(inp, _)| r.cache_1h as f64 * 2.0 * inp / 1_000_000.0)
        })
        .sum();
    let cache_creation_total = cache_5m_cost + cache_1h_cost;
    let cache_read_cost: f64 = records
        .iter()
        .filter_map(|r| {
            price_per_1m(&r.model).map(|(inp, _)| r.cache_read as f64 * 0.1 * inp / 1_000_000.0)
        })
        .sum();

    if grand.cost > 0.0 {
        let creation_pct = 100.0 * cache_creation_total / grand.cost;
        let read_pct = 100.0 * cache_read_cost / grand.cost;
        println!();
        println!(
            "  Cache creation: ${:.4} ({:.1}% of spend) — 5m: ${:.4}  1h: ${:.4}",
            cache_creation_total, creation_pct, cache_5m_cost, cache_1h_cost
        );
        println!(
            "  Cache reads:    ${:.4} ({:.1}% of spend)",
            cache_read_cost, read_pct
        );
        if creation_pct > 20.0 {
            println!();
            println!(
                "  *** Cache creation dominates spend ({:.1}%) — large context or frequent \
                 new-session writes. ***",
                creation_pct
            );
        }
    }

    // Unknown models
    let unknown_models: Vec<&String> = models
        .iter()
        .filter(|(m, _)| price_per_1m(m).is_none())
        .map(|(m, _)| *m)
        .collect();
    if !unknown_models.is_empty() {
        println!();
        println!("  Unknown model IDs (cost not computed):");
        for m in &unknown_models {
            println!("    {}", m);
        }
    }

    println!();
    println!("  Records: {}  |  Files scanned recursively", records.len());
}

// ── entry point ───────────────────────────────────────────────────────────────

fn default_projects_dir() -> PathBuf {
    // Windows: %USERPROFILE%\.claude\projects
    // Unix:    ~/.claude/projects
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".claude").join("projects")
}

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_projects_dir);

    eprintln!("Scanning: {}", dir.display());

    if !dir.exists() {
        eprintln!("Error: directory does not exist: {}", dir.display());
        std::process::exit(1);
    }

    let records = read_claude_code_jsonl(&dir);

    if records.is_empty() {
        eprintln!("No assistant records found under {}", dir.display());
        return;
    }

    print_report(&records);
}
