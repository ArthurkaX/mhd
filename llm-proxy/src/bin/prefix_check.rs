//! prefix_check — offline diagnostic checking whether native trim preserves
//! the message-content prefix between consecutive requests in a replay run.
//!
//! Usage:
//!   prefix_check [--db <path>] [--provider <anthropic|openai>]
//!
//! Reads all rows from `request_bodies`, groups by run_id ordered by seq,
//! and for each consecutive pair (N, N+1) within the same run, compares how
//! many leading `messages[]` turns are byte-identical both before and after
//! native trimming.  Reports pairs where trim reduces the shared prefix —
//! meaning trim may break implicit prefix-caching on the provider side.

use llm_proxy::config::config_dir;
use llm_proxy::db_log::decompress_body;
use llm_proxy::native_trim::{NativeKnobs, trim_native, trim_native_openai};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

// ── canonical JSON serialization (sorted keys) ───────────────────────────────

/// Recursively sort the keys of every JSON object, producing a canonical form
/// where two semantically identical Values (differing only in key order) yield
/// the same serialized string.
fn sort_keys_recursive(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), sort_keys_recursive(&m[k]));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_keys_recursive).collect()),
        other => other.clone(),
    }
}

/// Serialize a Value to JSON with object keys recursively sorted.  Two
/// semantically equal Values always produce the same string.
fn canonical_json(v: &Value) -> String {
    serde_json::to_string(&sort_keys_recursive(v))
        .expect("canonical JSON serialisation never fails")
}

// ── prefix counting ──────────────────────────────────────────────────────────

/// Count how many leading turns in the `messages` array are byte-identical
/// between two request bodies.
fn count_prefix_turns(body_a: &Value, body_b: &Value) -> usize {
    let msgs_a = body_a["messages"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let msgs_b = body_b["messages"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let min_len = msgs_a.len().min(msgs_b.len());
    for i in 0..min_len {
        if canonical_json(&msgs_a[i]) != canonical_json(&msgs_b[i]) {
            return i;
        }
    }
    min_len
}

// ── metrics per adjacent pair ────────────────────────────────────────────────

/// One row of analysis for a (N, N+1) pair within a single run.
struct PairMetric {
    run_id: i64,
    seq_n: i64,
    raw_prefix: usize,
    trimmed_prefix: usize,
    total_turns: usize,
}

// ── arg parsing ──────────────────────────────────────────────────────────────

fn parse_args() -> (PathBuf, String) {
    let default_db = config_dir().join("proxy.db");
    let default_provider = "anthropic".to_string();

    let mut db_path = default_db;
    let mut provider = default_provider;

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
            other => {
                eprintln!("Unknown argument: {other} (ignored)");
            }
        }
        i += 1;
    }

    (db_path, provider)
}

// ── corpus reader ────────────────────────────────────────────────────────────

struct BodyRow {
    run_id: i64,
    seq: i64,
    body: Value,
}

fn read_corpus(conn: &Connection, provider: &str) -> Vec<BodyRow> {
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
        "SELECT id, run_id, seq, body FROM request_bodies WHERE provider = ?1 ORDER BY run_id, seq",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut rows: Vec<BodyRow> = Vec::new();
    let mut parse_errors: usize = 0;

    let iter = stmt.query_map([provider], |row| {
        let _id: i64 = row.get(0)?;
        let run_id: i64 = row.get(1)?;
        let seq: i64 = row.get(2)?;
        let body_blob: Vec<u8> = row.get(3)?;
        Ok((run_id, seq, body_blob))
    });

    if let Ok(iter) = iter {
        for item in iter {
            match item {
                Ok((run_id, seq, body_blob)) => {
                    let body_str = match decompress_body(&body_blob) {
                        Some(s) => s,
                        None => {
                            parse_errors += 1;
                            continue;
                        }
                    };
                    match serde_json::from_str::<Value>(&body_str) {
                        Ok(body) => rows.push(BodyRow { run_id, seq, body }),
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

// ── entry point ──────────────────────────────────────────────────────────────

fn main() {
    let (db_path, provider) = parse_args();

    eprintln!("DB: {}", db_path.display());
    eprintln!("Provider: {provider}");

    // ── open DB read-only ─────────────────────────────────────────────────
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

    let rows = read_corpus(&conn, &provider);

    if rows.is_empty() {
        println!();
        println!("Corpus is empty — no rows in request_bodies for provider '{provider}'.");
        println!();
        println!("Set corpus_capture = true in llm-proxy settings, restart the daemon,");
        println!("generate some {provider} traffic, then re-run.");
        return;
    }

    eprintln!("Loaded {} rows.", rows.len());

    // ── group by run_id ───────────────────────────────────────────────────
    let mut by_run: BTreeMap<i64, Vec<BodyRow>> = BTreeMap::new();
    for row in rows {
        by_run.entry(row.run_id).or_default().push(row);
    }
    // Each run's Vec is already ordered by seq thanks to SQL ORDER BY.

    let n_runs = by_run.len();
    eprintln!("Grouped into {n_runs} run(s).");

    // ── analyse consecutive pairs ─────────────────────────────────────────
    let trim_fn: fn(Value, &NativeKnobs) -> Value = if provider == "openai" {
        trim_native_openai
    } else {
        trim_native
    };
    let knobs = NativeKnobs::default();
    let mut all_metrics: Vec<PairMetric> = Vec::new();
    let mut skipped_no_messages: usize = 0;

    for (run_id, bodies) in &by_run {
        for pair in bodies.windows(2) {
            let n = &pair[0];
            let n1 = &pair[1];

            // Both bodies must have a messages array to compare.
            if n.body["messages"].as_array().is_none() || n1.body["messages"].as_array().is_none() {
                skipped_no_messages += 1;
                continue;
            }

            let trimmed_n = trim_fn(n.body.clone(), &knobs);
            let trimmed_n1 = trim_fn(n1.body.clone(), &knobs);

            let raw_prefix = count_prefix_turns(&n.body, &n1.body);
            let trimmed_prefix = count_prefix_turns(&trimmed_n, &trimmed_n1);

            let total_turns = n.body["messages"].as_array().map(|a| a.len()).unwrap_or(0);

            all_metrics.push(PairMetric {
                run_id: *run_id,
                seq_n: n.seq,
                raw_prefix,
                trimmed_prefix,
                total_turns,
            });
        }
    }

    if skipped_no_messages > 0 {
        eprintln!("Skipped {skipped_no_messages} pair(s) with missing/empty messages array.");
    }

    // ── summary statistics ────────────────────────────────────────────────
    let n_pairs = all_metrics.len();

    let n_long_raw = all_metrics.iter().filter(|m| m.raw_prefix >= 1).count();
    let n_shrunk = all_metrics
        .iter()
        .filter(|m| m.trimmed_prefix < m.raw_prefix)
        .count();
    let shrunk_pct = if n_long_raw > 0 {
        100.0 * n_shrunk as f64 / n_long_raw as f64
    } else {
        0.0
    };

    let avg_raw = if n_pairs > 0 {
        all_metrics.iter().map(|m| m.raw_prefix as f64).sum::<f64>() / n_pairs as f64
    } else {
        0.0
    };
    let avg_trimmed = if n_pairs > 0 {
        all_metrics
            .iter()
            .map(|m| m.trimmed_prefix as f64)
            .sum::<f64>()
            / n_pairs as f64
    } else {
        0.0
    };

    // ── worst offenders: biggest absolute prefix drop ─────────────────────
    let mut worst: Vec<&PairMetric> = all_metrics.iter().collect();
    worst.sort_by(|a, b| {
        let drop_a = a.raw_prefix.saturating_sub(a.trimmed_prefix);
        let drop_b = b.raw_prefix.saturating_sub(b.trimmed_prefix);
        drop_b.cmp(&drop_a)
    });

    // ── print report ──────────────────────────────────────────────────────
    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Prefix Preservation Check  —  native trim vs raw");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  provider: {provider}   knobs: NativeKnobs::default()");
    println!();
    println!("  Runs analyzed:                {n_runs}");
    println!("  Adjacent pairs analysed:      {n_pairs}");
    println!("  Pairs with raw prefix >= 1:   {n_long_raw}");
    println!("  Pairs where trim shrank prefix: {n_shrunk}  ({shrunk_pct:.1}% of long-raw pairs)");
    println!();
    println!("  Avg raw prefix turns:         {avg_raw:.2}");
    println!("  Avg trimmed prefix turns:     {avg_trimmed:.2}");
    println!(
        "  Avg drop:                     {drop:.2}",
        drop = avg_raw - avg_trimmed
    );
    println!();

    if !worst.is_empty() {
        println!("  Worst offenders (biggest raw->trimmed prefix drop):");
        println!(
            "  {:>16}  {:>6}  {:>6}  {:>16}",
            "run_id", "seq(N)", "turns", "raw -> trimmed"
        );
        println!("  {}", "─".repeat(48));
        for m in worst.iter().take(10) {
            println!(
                "  {:>16}  {:>6}  {:>6}  {:>3} -> {:<3}",
                m.run_id, m.seq_n, m.total_turns, m.raw_prefix, m.trimmed_prefix,
            );
        }
    }
    println!();
}
