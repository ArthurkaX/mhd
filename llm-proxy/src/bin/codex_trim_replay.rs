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
    let mut log_repeat_applied = 0usize;

    for row in rows {
        total += 1;
        let compressed = row.expect("read body");
        let json = decompress_body(&compressed).expect("decompress body");
        let body: Value = serde_json::from_str(&json).expect("parse JSON body");
        let outcome = codex_trim::trim_responses(body.clone());
        *reasons.entry(outcome.reason).or_default() += 1;
        for class in &outcome.classes {
            *classes.entry(class).or_default() += 1;
        }
        if outcome.applied {
            let quality = codex_trim::quality_check(&body, &outcome.body);
            if !quality.relationships_preserved {
                relationship_failures += 1;
            }
            if !quality.structured_content_preserved {
                structured_failures += 1;
            }
            applied += 1;
            before += outcome.tokens_before as usize;
            after += outcome.tokens_after as usize;
            for stage in outcome.stages {
                if stage.name == "tool_output_whitespace_and_log_repeats" {
                    log_repeat_applied += 1;
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
    println!("log-repeat stage applied: {log_repeat_applied}");
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
}
