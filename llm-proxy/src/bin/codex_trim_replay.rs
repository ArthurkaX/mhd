//! Offline replay report for the conservative Codex Responses trim.
//!
//! Usage: codex_trim_replay --db <path-to-proxy.db>
//! The provider corpus is read from the sibling corpus-codex.db file.

use llm_proxy::{codex_trim, config::config_dir, corpus, db_log::decompress_body};
use rusqlite::Connection;
use serde_json::Value;
use std::{env, path::PathBuf};

fn main() {
    let args: Vec<String> = env::args().collect();
    let db = args
        .windows(2)
        .find(|pair| pair[0] == "--db")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(|| config_dir().join("proxy.db"));
    let dir = db.parent().unwrap_or_else(|| std::path::Path::new("."));
    let path = corpus::corpus_path(dir, "codex");
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));

    let mut stmt = conn
        .prepare("SELECT body FROM request_bodies ORDER BY id")
        .expect("prepare corpus query");
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("read corpus rows");
    let mut total = 0usize;
    let mut applied = 0usize;
    let mut fail_open = 0usize;
    let mut before = 0usize;
    let mut after = 0usize;
    let mut stages = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut reasons = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut classes = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut relationship_failures = 0usize;
    let mut structured_failures = 0usize;
    let mut tool_names_failures = 0usize;
    let mut log_classified = 0usize;
    let mut log_repeat_applied = 0usize;
    let mut diagnostic_head_tail_applied = 0usize;
    // Protection-reason histogram over every inspected block: reason → (blocks, chars).
    let mut protection_hist = std::collections::BTreeMap::<&'static str, (usize, usize)>::new();
    let mut protected_blocks = 0usize;
    let mut protected_chars = 0usize;
    let mut eligible_blocks = 0usize;
    let mut eligible_chars = 0usize;
    let mut below_threshold_blocks = 0usize;
    let mut below_threshold_chars = 0usize;
    // Chars saved split by lever.
    let mut lever_savings = std::collections::BTreeMap::<&'static str, usize>::new();
    // Corpus-wide raw bytes (all bodies, not just shrunk ones) for the headline %.
    let mut corpus_before = 0usize;
    let mut corpus_after = 0usize;

    for row in rows {
        total += 1;
        let compressed = row.expect("read body");
        let json = decompress_body(&compressed).expect("decompress body");
        let body: Value = serde_json::from_str(&json).expect("parse JSON body");
        let body_bytes = serde_json::to_vec(&body).map(|v| v.len()).unwrap_or(0);
        corpus_before += body_bytes;
        let outcome = codex_trim::trim_responses(body.clone());
        *reasons.entry(outcome.reason).or_default() += 1;
        for class in &outcome.classes {
            *classes.entry(class).or_default() += 1;
            if *class == "logs / timestamped" {
                log_classified += 1;
            }
        }
        for (reason, chars) in &outcome.protection_reasons {
            let entry = protection_hist.entry(reason).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += chars;
            if *reason == "none (elided)" {
                if *chars >= codex_trim::MIN_TOOL_OUTPUT_CHARS {
                    eligible_blocks += 1;
                    eligible_chars += chars;
                } else {
                    below_threshold_blocks += 1;
                    below_threshold_chars += chars;
                }
            } else {
                protected_blocks += 1;
                protected_chars += chars;
            }
        }
        for (lever, chars) in &outcome.saved_by_lever {
            *lever_savings.entry(*lever).or_default() += chars;
        }
        let after_bytes = serde_json::to_vec(&outcome.body)
            .map(|v| v.len())
            .unwrap_or(0);
        corpus_after += after_bytes;
        if outcome.applied {
            let quality = codex_trim::quality_check(&body, &outcome.body);
            if !quality.relationships_preserved {
                relationship_failures += 1;
            }
            if !quality.structured_content_preserved {
                structured_failures += 1;
            }
            if !quality.tool_names_preserved {
                tool_names_failures += 1;
            }
            applied += 1;
            before += outcome.tokens_before as usize;
            after += outcome.tokens_after as usize;
            for stage in outcome.stages {
                if stage.name == "tool_output_whitespace_and_safe_repeats" {
                    log_repeat_applied += 1;
                }
                if stage.name == "tool_output_whitespace_and_safe_diagnostics" {
                    diagnostic_head_tail_applied += 1;
                }
                *stages.entry(stage.name).or_default() += 1;
            }
        } else {
            fail_open += 1;
        }
    }

    println!("rows: {total}");
    println!("trim candidates applied: {applied}");
    println!("fail-open/no-gain: {fail_open}");
    println!("reasons: {reasons:?}");
    println!("classified outputs: {classes:?}");
    println!("quality relationship failures: {relationship_failures}");
    println!("quality structured-content failures: {structured_failures}");
    println!("quality tool-names failures: {tool_names_failures}");
    println!("log outputs classified: {log_classified}");
    println!("log-repeat stage applied: {log_repeat_applied}");
    println!("safe-diagnostic head-tail stage applied: {diagnostic_head_tail_applied}");
    if applied > 0 {
        let saved = before.saturating_sub(after);
        println!("estimated tokens before: {before}");
        println!("estimated tokens after:  {after}");
        println!(
            "estimated tokens saved:  {saved} ({:.1}%)",
            saved as f64 * 100.0 / before as f64
        );
    }
    println!("stages: {stages:?}");
    println!("protection reasons (blocks, chars): {protection_hist:?}");
    println!("protected output:       {protected_blocks} blocks / {protected_chars} chars");
    println!("eligible output:        {eligible_blocks} blocks / {eligible_chars} chars");
    println!(
        "below output threshold:  {below_threshold_blocks} blocks / {below_threshold_chars} chars"
    );
    println!("chars saved by lever: {lever_savings:?}");
    if corpus_before > 0 {
        let corpus_saved = corpus_before.saturating_sub(corpus_after);
        println!("corpus raw bytes before: {corpus_before}");
        println!("corpus raw bytes after:  {corpus_after}");
        println!(
            "corpus raw savings:       {corpus_saved} ({:.2}%)",
            corpus_saved as f64 * 100.0 / corpus_before as f64
        );
    }
}
