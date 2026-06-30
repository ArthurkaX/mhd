//! In-process trim A/B bench over the Anthropic replay corpus.
//!
//! Deterministic, no LLM: replays recorded request bodies through the native
//! trim engine and reports input-token reduction as avg-of-ratios. Anthropic
//! shape only (the subscriber quota story is Anthropic/Claude Code specific).

use std::path::Path;
use serde_json::Value;
use rusqlite::{Connection, OpenFlags};
use crate::native_trim::{NativeKnobs, trim_native};
use crate::db_log::decompress_body;

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
pub fn run_anthropic_bench(db_path: &Path, knobs: &NativeKnobs) -> Result<Option<BenchResult>, String> {
    let t0 = std::time::Instant::now();
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;

    // Gracefully handle a missing table.
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_bodies'",
        [], |r| r.get::<_, i64>(0)).map(|n| n > 0).unwrap_or(false);
    if !exists { return Ok(None); }

    let mut stmt = conn.prepare(
        "SELECT body FROM request_bodies WHERE provider='anthropic' ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).map_err(|e| e.to_string())?;

    let mut bodies: Vec<Value> = Vec::new();
    for r in rows.flatten() {
        if let Some(s) = decompress_body(&r) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) { bodies.push(v); }
        }
    }
    if bodies.is_empty() { return Ok(None); }

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
        let after  = est_tokens(&trim_native(body.clone(), knobs));
        tokens_off += before;
        tokens_on  += after;
        if after > before { fail_open_ok = false; }
        if after < before && before > 0 {
            ratios.push((before - after) as f64 / before as f64 * 100.0);
        }
    }
    let n_trimmed = ratios.len();
    let avg_trim_pct = if ratios.is_empty() { 0.0 } else { ratios.iter().sum::<f64>() / n_trimmed as f64 };
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_trim_pct = if ratios.is_empty() { 0.0 } else { ratios[ratios.len()/2] };

    Ok(Some(BenchResult {
        n_bodies: bodies.len(), n_trimmed, tokens_off, tokens_on,
        avg_trim_pct, median_trim_pct, fail_open_ok, deterministic,
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
        assert!(after < before, "trim should reduce tokens for oversized desc");
    }
}
