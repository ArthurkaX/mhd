//! In-process trim A/B bench over the Anthropic replay corpus.
//!
//! Deterministic, no LLM: replays recorded request bodies through the native
//! trim engine and reports input-token reduction as avg-of-ratios. Anthropic
//! shape only (the subscriber quota story is Anthropic/Claude Code specific).

use crate::db_log::decompress_body;
use crate::native_trim::{NativeKnobs, trim_native};
use serde_json::Value;
use std::path::Path;

/// Same estimator as the `backtest` binary (~4 chars/token over serialized JSON)
/// so live and offline numbers agree. NOT a real tokenizer.
fn est_tokens(v: &Value) -> u64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) as u64) / 4
}

#[derive(Debug, Clone)]
pub struct BenchResult {
    /// Number of anthropic bodies measured.
    pub n_bodies: usize,
    /// Number of bodies the engine actually reduced (after < before).
    pub n_trimmed: usize,
    /// Sum of estimated input tokens, arm OFF (untouched bodies).
    pub tokens_off: u64,
    /// Sum of estimated input tokens, arm ON (post-trim).
    pub tokens_on: u64,
    /// avg-of-ratios trim % over reduced bodies (per-body mean of (before-after)/before*100).
    pub avg_trim_pct: f64,
    /// Median trim % over reduced bodies.
    pub median_trim_pct: f64,
    /// True iff no body grew (fail-open invariant held).
    pub fail_open_ok: bool,
    /// True iff a second pass over the first body produced identical output.
    pub deterministic: bool,
    /// Wall time of the run.
    pub elapsed_ms: u64,
}

/// Run the Anthropic A/B bench. Reads `request_bodies` (provider='anthropic'),
/// trims each with `knobs`, and aggregates. Read-only DB access.
///
/// Returns `Ok(None)` when the corpus has no anthropic rows (caller shows "empty").
pub fn run_anthropic_bench(
    db_path: &Path,
    knobs: &NativeKnobs,
) -> Result<Option<BenchResult>, String> {
    let t0 = std::time::Instant::now();
    // The corpus lives next to proxy.db: either the per-provider file
    // `corpus-anthropic.db` or, before the split, the legacy `request_bodies`
    // table inside proxy.db itself. `open_read` picks whichever one holds rows
    // and returns `None` when neither does — the same "no anthropic corpus"
    // state the sqlite_master probe used to detect, and `Ok(None)` here.
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let conn = match crate::corpus::open_read(dir, "anthropic") {
        Some(c) => c,
        None => return Ok(None),
    };

    let mut stmt = conn
        .prepare("SELECT body FROM request_bodies WHERE provider='anthropic' ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?;

    let mut bodies: Vec<Value> = Vec::new();
    for r in rows.flatten() {
        if let Some(s) = decompress_body(&r)
            && let Ok(v) = serde_json::from_str::<Value>(&s)
        {
            bodies.push(v);
        }
    }
    if bodies.is_empty() {
        return Ok(None);
    }

    // Determinism sanity: trim the first body twice; outputs must match.
    let deterministic = {
        let a = trim_native(bodies[0].clone(), knobs);
        let b = trim_native(bodies[0].clone(), knobs);
        a == b
    };

    let mut ratios: Vec<f64> = Vec::new();
    let (mut tokens_off, mut tokens_on) = (0u64, 0u64);
    let mut fail_open_ok = true;
    for body in &bodies {
        let before = est_tokens(body);
        let after = est_tokens(&trim_native(body.clone(), knobs));
        tokens_off += before;
        tokens_on += after;
        if after > before {
            fail_open_ok = false;
        }
        if after < before && before > 0 {
            ratios.push((before - after) as f64 / before as f64 * 100.0);
        }
    }
    let n_trimmed = ratios.len();
    let avg_trim_pct = if ratios.is_empty() {
        0.0
    } else {
        ratios.iter().sum::<f64>() / n_trimmed as f64
    };
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_trim_pct = if ratios.is_empty() {
        0.0
    } else {
        ratios[ratios.len() / 2]
    };

    Ok(Some(BenchResult {
        n_bodies: bodies.len(),
        n_trimmed,
        tokens_off,
        tokens_on,
        avg_trim_pct,
        median_trim_pct,
        fail_open_ok,
        deterministic,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_trim::NativeKnobs;

    #[test]
    fn test_est_tokens_known_value() {
        let v = serde_json::json!({"hello": "world"});
        // {"hello":"world"} is 16 chars -> 4 tokens
        assert_eq!(est_tokens(&v), 4);
    }

    #[test]
    fn test_est_tokens_does_not_panic() {
        let v = serde_json::json!(null);
        let t = est_tokens(&v);
        assert_eq!(t, 1); // "null" is 4 chars / 4 = 1
    }

    #[test]
    fn test_trim_reduces_large_tool_desc() {
        let body = serde_json::json!({
            "tools": [{
                "name": "test_tool",
                "description": "x".repeat(500),
                "input_schema": {"type": "object", "properties": {}}
            }],
            "messages": [{"role": "user", "content": "hello"}]
        });
        let knobs = NativeKnobs {
            tool_max_desc_chars: 50,
            ..Default::default()
        };
        let trimmed = trim_native(body.clone(), &knobs);
        let before = est_tokens(&body);
        let after = est_tokens(&trimmed);
        assert!(after <= before, "trim must not increase tokens");
        assert!(
            after < before,
            "trim should reduce tokens for oversized desc"
        );
    }

    #[test]
    fn test_bench_reads_legacy_proxy_db_fallback() {
        // End-to-end upgrade path: a proxy.db whose `request_bodies` table is
        // still the single shared corpus (no per-provider file exists) must
        // drive the bench exactly as before the storage split. Rows go in
        // through `corpus::insert_body` so the test exercises the real
        // compress+store path, not a hand-rolled table.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dir = tmp.path();
        let conn = rusqlite::Connection::open(dir.join("proxy.db")).expect("legacy open");
        conn.execute_batch(
            "CREATE TABLE request_bodies (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 seq       INTEGER NOT NULL,
                 ts        TEXT    NOT NULL,
                 model     TEXT,
                 provider  TEXT,
                 body      BLOB    NOT NULL
             );",
        )
        .expect("legacy schema");
        for (run_id, seq) in [(1, 1), (1, 2)] {
            let body = format!(
                r#"{{"run_id":{run_id},"seq":{seq},"messages":[{{"role":"user","content":"hello"}}]}}"#
            );
            crate::corpus::insert_body(
                &conn,
                run_id,
                seq,
                "2026-08-01T00:00:00Z",
                None,
                "anthropic",
                &body,
                0,
            );
        }
        drop(conn);
        assert!(
            !dir.join("corpus-anthropic.db").exists(),
            "no per-provider file — this test proves the legacy fallback"
        );

        let knobs = NativeKnobs::default();
        let result = run_anthropic_bench(&dir.join("proxy.db"), &knobs)
            .expect("bench must run against the legacy table")
            .expect("legacy rows must not read as an empty corpus");
        assert_eq!(result.n_bodies, 2, "both legacy rows measured");
        assert!(
            result.fail_open_ok,
            "trim must never grow a body (fail-open invariant)"
        );
    }
}
