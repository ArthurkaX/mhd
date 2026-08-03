//! Tune advisor — deterministic, read-only sweep of native trim knobs over the
//! Anthropic replay corpus.
//!
//! Recommends the "knee" value for `tool_max_desc_chars`: the point past which
//! further tightening yields little extra trim%.  No LLM, no network, no writes.

use crate::db_log::decompress_body;
use crate::native_trim::{NativeKnobs, trim_native, trim_native_openai};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

// ── helpers (mirror bench.rs) ────────────────────────────────────────────────

/// Same estimator as `bench.rs` (~4 chars/token over serialized JSON) so live
/// and offline numbers agree. NOT a real tokenizer.
fn est_tokens(v: &Value) -> u64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) as u64) / 4
}

// ── public types ─────────────────────────────────────────────────────────────

/// One measured point on the sweep.
pub struct SweepPoint {
    pub desc_chars: usize,  // the swept value
    pub avg_trim_pct: f64,  // avg-of-ratios over reduced bodies
    pub n_trimmed: usize,   // how many bodies were reduced
    pub fail_open_ok: bool, // no body grew at this setting
}

/// Result of a tune run.
pub struct TuneResult {
    pub points: Vec<SweepPoint>,    // every swept value, ascending desc_chars
    pub baseline_desc_chars: usize, // the current setting the caller passed in
    pub baseline_trim_pct: f64,     // trim% at (or nearest to) the baseline value
    pub recommended: usize,         // the knee pick (see pick_knee)
    pub recommended_trim_pct: f64,  // trim% at recommendation
    pub verdict: TuneVerdict,       // how worthwhile the recommendation is
    pub n_bodies: usize,
    pub elapsed_ms: u64,
}

// ── knee selection ───────────────────────────────────────────────────────────

/// Minimum marginal trim% gain (in percentage points) required to justify a
/// step to a more aggressive (smaller `desc_chars`) setting.
const KNEE_MIN_GAIN_PCT: f64 = 0.5;

/// Pick the "knee" desc_chars value from a sweep curve.
///
/// Rules, in order:
///
/// 1. Consider ONLY points with `fail_open_ok == true` AND
///    `desc_chars >= floor_desc_chars`.  Call these "eligible".
///
/// 2. If no eligible points, return `floor_desc_chars` (safe fallback).
///
/// 3. Walk eligible points from LARGEST desc_chars (least aggressive) to
///    SMALLEST (most aggressive).  At each step compute the marginal gain in
///    avg_trim_pct versus the previous (less aggressive) point.
///
/// 4. Pick the most-aggressive point such that EVERY step down from the
///    least-aggressive end still added at least `KNEE_MIN_GAIN_PCT` (0.5
///    percentage points).  As soon as a step's marginal gain drops below the
///    threshold, STOP and return the *previous* (less aggressive, safer)
///    desc_chars — do not chase the flat tail.
///
/// 5. If even the first step from the largest point already gains <
///    KNEE_MIN_GAIN_PCT, recommend the largest (least aggressive) eligible
///    value — tuning isn't worth it, stay conservative.
///
/// This "recommend the knee, never the max" bias avoids over-trimming, which
/// the project cannot measure for quality.
pub fn pick_knee(points: &[SweepPoint], floor_desc_chars: usize) -> usize {
    // Collect eligible points: fail_open_ok && desc_chars >= floor
    let mut eligible: Vec<&SweepPoint> = points
        .iter()
        .filter(|p| p.fail_open_ok && p.desc_chars >= floor_desc_chars)
        .collect();

    // Sort by desc_chars ascending (smallest = most aggressive first).
    eligible.sort_by_key(|p| p.desc_chars);

    if eligible.is_empty() {
        return floor_desc_chars;
    }

    // Walk from largest (least aggressive) to smallest (most aggressive).
    // eligible[0] = smallest desc_chars, eligible[len-1] = largest desc_chars.
    for i in (0..eligible.len() - 1).rev() {
        let less_aggressive = eligible[i + 1]; // larger desc_chars
        let more_aggressive = eligible[i]; // smaller desc_chars
        let gain = more_aggressive.avg_trim_pct - less_aggressive.avg_trim_pct;
        if gain < KNEE_MIN_GAIN_PCT {
            // Marginal gain too small — return the less aggressive value.
            return less_aggressive.desc_chars;
        }
    }

    // All steps had sufficient gain — return the most aggressive eligible value.
    eligible[0].desc_chars
}

// ── verdict classification ──────────────────────────────────────────────────

/// Advisor verdict: how worthwhile is changing the current setting?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuneVerdict {
    /// A clear knee exists and the gain over baseline clears the bar — recommend
    /// applying.
    Worthwhile,
    /// Some gain exists but it's small and/or only reachable by an aggressive cut
    /// (recommendation sits at/near the floor, or the curve is smooth with no real
    /// knee).  Advice: you *may* keep the current setting.
    Marginal,
    /// Ceiling of achievable gain is below the bar — nothing to tune, keep current.
    NotWorth,
}

/// A recommendation must beat baseline by at least this many trim-percentage-
/// points to be called Worthwhile.
const WORTHWHILE_MIN_GAIN_PCT: f64 = 3.0;

/// If the BEST achievable trim% (most aggressive eligible point) beats baseline
/// by less than this, it's NotWorth.
const NOTWORTH_MAX_CEILING_PCT: f64 = 1.0;

/// Classify how worthwhile tuning is, given the measured sweep, the knee
/// recommendation, and the baseline trim%.  Pure — no I/O.
///
/// Rules (checked in order):
///
/// 1. **ceiling gain** = (max `avg_trim_pct` among eligible points) -
///    `baseline_trim_pct`.  Eligible = `fail_open_ok && desc_chars >=
///    floor_desc_chars`.  If ceiling_gain < `NOTWORTH_MAX_CEILING_PCT` ⇒
///    `NotWorth` (best case barely helps).
///
/// 2. **rec gain** = `recommended_trim_pct - baseline_trim_pct`.  If
///    rec_gain >= `WORTHWHILE_MIN_GAIN_PCT` AND the recommendation is NOT
///    sitting at the floor (`recommended != floor_desc_chars`) ⇒ `Worthwhile`
///    (real knee, real gain).
///
/// 3. Otherwise ⇒ `Marginal` (gain is small, or only reachable by cutting
///    to the floor).
pub fn classify_verdict(
    points: &[SweepPoint],
    floor_desc_chars: usize,
    recommended: usize,
    recommended_trim_pct: f64,
    baseline_trim_pct: f64,
) -> TuneVerdict {
    // Eligible = fail_open_ok && desc_chars >= floor_desc_chars (same as pick_knee).
    let eligible: Vec<&SweepPoint> = points
        .iter()
        .filter(|p| p.fail_open_ok && p.desc_chars >= floor_desc_chars)
        .collect();

    // Guard: empty eligible set => NotWorth.
    if eligible.is_empty() {
        return TuneVerdict::NotWorth;
    }

    // Rule 1: ceiling gain.
    let max_trim = eligible
        .iter()
        .map(|p| p.avg_trim_pct)
        .fold(f64::NEG_INFINITY, f64::max);
    let ceiling_gain = max_trim - baseline_trim_pct;
    if ceiling_gain < NOTWORTH_MAX_CEILING_PCT {
        return TuneVerdict::NotWorth;
    }

    // Rule 2: recommendation gain and non-floor check.
    let rec_gain = recommended_trim_pct - baseline_trim_pct;
    if rec_gain >= WORTHWHILE_MIN_GAIN_PCT && recommended != floor_desc_chars {
        return TuneVerdict::Worthwhile;
    }

    // Rule 3: otherwise marginal.
    TuneVerdict::Marginal
}

// ── core tune run ───────────────────────────────────────────────────────────

/// Run a deterministic tune sweep over `tool_max_desc_chars`.
///
/// Opens `proxy.db` read-only, loads all Anthropic request bodies (exactly like
/// [`crate::bench::run_anthropic_bench`]), and for each value in `sweep` trims
/// every body with [`trim_native`].  Bodies are loaded once and reused.
///
/// `max_bodies` caps the corpus with a deterministic stratified subsample:
/// bodies are grouped by session (`run_id`) and each session contributes a
/// uniform share so every session is represented.  `0` = use all bodies.
/// No RNG, fully reproducible.
///
/// `progress` is called with `(done, total)` after each sweep value completes
/// (and once with `(0, total)` before the loop) so callers can show a progress
/// bar.
///
/// Returns `Ok(None)` when the corpus is empty or the table is missing.
pub fn run_tune(
    db_path: &Path,
    base: &NativeKnobs,
    sweep: &[usize],
    floor_desc_chars: usize,
    max_bodies: usize,
    mut progress: impl FnMut(usize, usize),
) -> Result<Option<TuneResult>, String> {
    let t0 = std::time::Instant::now();
    // The corpus lives next to proxy.db: a per-provider file when it exists,
    // else the legacy `request_bodies` table in proxy.db. Either way the
    // returned connection exposes the table as plain `request_bodies`.
    let dir = db_path.parent().unwrap_or(Path::new(""));
    let Some(conn) = crate::corpus::open_read(dir, "anthropic") else {
        return Ok(None);
    };

    let mut stmt = conn
        .prepare("SELECT run_id, body FROM request_bodies WHERE provider='anthropic' ORDER BY run_id, id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .map_err(|e| e.to_string())?;

    let mut bodies_with_run_id: Vec<(i64, Value)> = Vec::new();
    for r in rows.flatten() {
        let (run_id, body_bytes) = r;
        if let Some(s) = decompress_body(&body_bytes)
            && let Ok(v) = serde_json::from_str::<Value>(&s)
        {
            bodies_with_run_id.push((run_id, v));
        }
    }
    if bodies_with_run_id.is_empty() {
        return Ok(None);
    }

    // ── stratified session-aware subsample ──────────────────────────────
    // Group bodies by run_id (they arrive grouped because of ORDER BY run_id, id).
    let mut groups: Vec<(i64, Vec<Value>)> = Vec::new();
    for (run_id, body) in bodies_with_run_id {
        match groups.last_mut() {
            Some(last) if last.0 == run_id => last.1.push(body),
            _ => groups.push((run_id, vec![body])),
        }
    }

    let bodies: Vec<Value> = if max_bodies > 0 {
        let total: usize = groups.iter().map(|(_, v)| v.len()).sum();
        if total > max_bodies {
            let n_sessions = groups.len();
            let q = std::cmp::max(1, max_bodies / n_sessions);
            let mut sampled: Vec<Value> = Vec::with_capacity(max_bodies);
            for (_run_id, session) in &groups {
                if session.len() <= q {
                    sampled.extend(session.iter().cloned());
                } else {
                    let step = session.len() as f64 / q as f64;
                    sampled.extend((0..q).map(|i| session[(i as f64 * step) as usize].clone()));
                }
            }
            if sampled.len() > max_bodies {
                sampled.truncate(max_bodies);
            }
            sampled
        } else {
            groups.into_iter().flat_map(|(_, v)| v).collect()
        }
    } else {
        groups.into_iter().flat_map(|(_, v)| v).collect()
    };
    let n_bodies = bodies.len();

    // Sweep every value.  Bodies loaded once, reused across sweep values.
    let mut points: Vec<SweepPoint> = Vec::with_capacity(sweep.len());
    progress(0, sweep.len());
    for (idx, &v) in sweep.iter().enumerate() {
        let mut knobs = *base;
        knobs.tool_max_desc_chars = v;

        let mut ratios: Vec<f64> = Vec::new();
        let mut fail_open_ok = true;
        for body in &bodies {
            let before = est_tokens(body);
            let after = est_tokens(&trim_native(body.clone(), &knobs));
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

        points.push(SweepPoint {
            desc_chars: v,
            avg_trim_pct,
            n_trimmed,
            fail_open_ok,
        });
        progress(idx + 1, sweep.len());
    }

    // Baseline: the trim% at (or nearest to) the caller's current setting.
    let baseline_desc_chars = base.tool_max_desc_chars;
    let baseline_trim_pct = points
        .iter()
        .find(|p| p.desc_chars == baseline_desc_chars)
        .map(|p| p.avg_trim_pct)
        .unwrap_or(0.0);

    // Knee recommendation.
    let recommended = pick_knee(&points, floor_desc_chars);
    let recommended_trim_pct = points
        .iter()
        .find(|p| p.desc_chars == recommended)
        .map(|p| p.avg_trim_pct)
        .unwrap_or(0.0);

    // Verdict classification.
    let verdict = classify_verdict(
        &points,
        floor_desc_chars,
        recommended,
        recommended_trim_pct,
        baseline_trim_pct,
    );

    Ok(Some(TuneResult {
        points,
        baseline_desc_chars,
        baseline_trim_pct,
        recommended,
        recommended_trim_pct,
        verdict,
        n_bodies,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    }))
}

// ── bucket-aware tune ────────────────────────────────────────────────────────

/// Which corpus subset to tune — the risk/economics bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// provider='anthropic' AND requests.target='native' (Opus native — Claude knows the tools).
    Native,
    /// provider='anthropic' AND requests.target!='native' (Claude Code routed to a gateway model).
    CcGateway,
    /// provider='openai' (other harness: Zed/opencode/pi to a gateway model).
    OtherOpenai,
}

/// Which knob the sweep varies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepKnob {
    DescChars,
    ToolResultHead,
}

/// Run a bucket-aware tune sweep over one knob (tool_max_desc_chars or
/// tool_result_head).  Reuses `pick_knee` / `classify_verdict` from the
/// standard tune — note that `SweepPoint.desc_chars` holds the *swept value*
/// regardless of which knob is being varied (it is the x-axis).
pub fn run_bucket_tune(
    db_path: &Path,
    base: &NativeKnobs,
    sweep: &[usize],
    floor: usize,
    max_bodies: usize,
    bucket: Bucket,
    knob: SweepKnob,
    mut progress: impl FnMut(usize, usize),
) -> Result<Option<TuneResult>, String> {
    let t0 = std::time::Instant::now();
    // `requests` stays in proxy.db; the bodies may now live in a per-provider
    // file attached as the `corpus` schema (or fall back to the legacy table in
    // this same database). `None` means no corpus rows for this provider — the
    // same graceful empty as a missing table.
    let provider = match bucket {
        Bucket::Native | Bucket::CcGateway => "anthropic",
        Bucket::OtherOpenai => "openai",
    };
    let dir = db_path.parent().unwrap_or(Path::new(""));
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;
    let Some(schema) = crate::corpus::attach_read(&conn, dir, provider) else {
        return Ok(None);
    };

    // `schema` is one of the corpus module's own fixed literals (`"corpus"` or
    // `"main"`), never caller input — formatting it in is safe. `requests`
    // always stays in the connection's `main` database. The `provider` filter
    // is deliberately kept even though the per-provider file already holds a
    // single provider: it is a correct no-op there and strictly required on
    // the legacy fallback, so one query text works in both worlds.
    let query = match bucket {
        Bucket::Native => {
            format!(
                "SELECT rb.run_id, rb.body FROM {schema}.request_bodies rb \
                 JOIN main.requests r ON r.run_id=rb.run_id AND r.seq=rb.seq \
                 WHERE rb.provider='anthropic' AND r.target='native' \
                 ORDER BY rb.run_id, rb.id"
            )
        }
        Bucket::CcGateway => {
            format!(
                "SELECT rb.run_id, rb.body FROM {schema}.request_bodies rb \
                 JOIN main.requests r ON r.run_id=rb.run_id AND r.seq=rb.seq \
                 WHERE rb.provider='anthropic' AND r.target!='native' \
                 ORDER BY rb.run_id, rb.id"
            )
        }
        Bucket::OtherOpenai => {
            format!(
                "SELECT rb.run_id, rb.body FROM {schema}.request_bodies rb \
                 JOIN main.requests r ON r.run_id=rb.run_id AND r.seq=rb.seq \
                 WHERE rb.provider='openai' \
                 ORDER BY rb.run_id, rb.id"
            )
        }
    };

    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .map_err(|e| e.to_string())?;

    let mut bodies_with_run_id: Vec<(i64, Value)> = Vec::new();
    for r in rows.flatten() {
        let (run_id, body_bytes) = r;
        if let Some(s) = decompress_body(&body_bytes)
            && let Ok(v) = serde_json::from_str::<Value>(&s)
        {
            bodies_with_run_id.push((run_id, v));
        }
    }
    if bodies_with_run_id.is_empty() {
        return Ok(None);
    }

    // ── stratified session-aware subsample ──────────────────────────────
    let mut groups: Vec<(i64, Vec<Value>)> = Vec::new();
    for (run_id, body) in bodies_with_run_id {
        match groups.last_mut() {
            Some(last) if last.0 == run_id => last.1.push(body),
            _ => groups.push((run_id, vec![body])),
        }
    }

    let bodies: Vec<Value> = if max_bodies > 0 {
        let total: usize = groups.iter().map(|(_, v)| v.len()).sum();
        if total > max_bodies {
            let n_sessions = groups.len();
            let q = std::cmp::max(1, max_bodies / n_sessions);
            let mut sampled: Vec<Value> = Vec::with_capacity(max_bodies);
            for (_run_id, session) in &groups {
                if session.len() <= q {
                    sampled.extend(session.iter().cloned());
                } else {
                    let step = session.len() as f64 / q as f64;
                    sampled.extend((0..q).map(|i| session[(i as f64 * step) as usize].clone()));
                }
            }
            if sampled.len() > max_bodies {
                sampled.truncate(max_bodies);
            }
            sampled
        } else {
            groups.into_iter().flat_map(|(_, v)| v).collect()
        }
    } else {
        groups.into_iter().flat_map(|(_, v)| v).collect()
    };
    let n_bodies = bodies.len();

    // Pick the trim function based on bucket.
    let trim_fn: fn(Value, &NativeKnobs) -> Value = match bucket {
        Bucket::OtherOpenai => trim_native_openai,
        _ => trim_native,
    };

    // Sweep every value.  Bodies loaded once, reused across sweep values.
    let mut points: Vec<SweepPoint> = Vec::with_capacity(sweep.len());
    progress(0, sweep.len());
    for (idx, &v) in sweep.iter().enumerate() {
        let mut knobs = *base;
        match knob {
            SweepKnob::DescChars => knobs.tool_max_desc_chars = v,
            SweepKnob::ToolResultHead => knobs.tool_result_head = v,
        }

        let mut ratios: Vec<f64> = Vec::new();
        let mut fail_open_ok = true;
        for body in &bodies {
            let before = est_tokens(body);
            let after = est_tokens(&trim_fn(body.clone(), &knobs));
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

        points.push(SweepPoint {
            desc_chars: v,
            avg_trim_pct,
            n_trimmed,
            fail_open_ok,
        });
        progress(idx + 1, sweep.len());
    }

    // Baseline: the trim% at (or nearest to) the caller's current setting.
    let baseline_desc_chars = match knob {
        SweepKnob::DescChars => base.tool_max_desc_chars,
        SweepKnob::ToolResultHead => base.tool_result_head,
    };
    let baseline_trim_pct = points
        .iter()
        .find(|p| p.desc_chars == baseline_desc_chars)
        .map(|p| p.avg_trim_pct)
        .unwrap_or(0.0);

    // Knee recommendation.
    let recommended = pick_knee(&points, floor);
    let recommended_trim_pct = points
        .iter()
        .find(|p| p.desc_chars == recommended)
        .map(|p| p.avg_trim_pct)
        .unwrap_or(0.0);

    // Verdict classification.
    let verdict = classify_verdict(
        &points,
        floor,
        recommended,
        recommended_trim_pct,
        baseline_trim_pct,
    );

    Ok(Some(TuneResult {
        points,
        baseline_desc_chars,
        baseline_trim_pct,
        recommended,
        recommended_trim_pct,
        verdict,
        n_bodies,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    }))
}

// ── unit tests (pure, no DB) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{insert_body, open_write};
    use tempfile::TempDir;

    /// Helper: build a SweepPoint quickly.
    fn sp(desc_chars: usize, avg_trim_pct: f64, fail_open_ok: bool) -> SweepPoint {
        SweepPoint {
            desc_chars,
            avg_trim_pct,
            n_trimmed: 100, // arbitrary, not used by pick_knee
            fail_open_ok,
        }
    }

    /// A curve with a clear knee: aggressive end flattens after 120.
    /// Eligible points sorted by desc_chars:
    ///   80(15%), 100(14.8%), 120(14.5%), 150(13.5%), 200(11.5%), 300(10%), 500(9%)
    /// Walk from largest:
    ///   500->300 gain=1.0, 300->200 gain=1.5, 200->150 gain=2.0,
    ///   150->120 gain=1.0, 120->100 gain=0.3 < 0.5 => STOP, return 120.
    #[test]
    fn clear_knee_returns_knee_not_max() {
        let points = vec![
            sp(80, 15.0, true),
            sp(100, 14.8, true),
            sp(120, 14.5, true),
            sp(150, 13.5, true),
            sp(200, 11.5, true),
            sp(300, 10.0, true),
            sp(500, 9.0, true),
        ];
        assert_eq!(pick_knee(&points, 0), 120);
    }

    /// Flat curve: all marginal gains < 0.5.
    /// Walk: 500->300 gain=0.1 < 0.5 => STOP, return 500.
    #[test]
    fn flat_curve_returns_least_aggressive() {
        let points = vec![
            sp(80, 4.0, true),
            sp(100, 3.9, true),
            sp(120, 3.8, true),
            sp(150, 3.7, true),
            sp(200, 3.6, true),
            sp(300, 3.5, true),
            sp(500, 3.4, true),
        ];
        assert_eq!(pick_knee(&points, 0), 500);
    }

    /// The floor is respected (never returns below floor_desc_chars).
    /// Floor=120, so 80 and 100 are ineligible.
    /// Eligible: [120(14.5%), 150(13.5%), 200(11.5%)]
    /// Walk: 200->150 gain=2.0, 150->120 gain=1.0 => all steps ok => return 120.
    #[test]
    fn floor_respected() {
        let points = vec![
            sp(80, 15.0, true),
            sp(100, 14.8, true),
            sp(120, 14.5, true),
            sp(150, 13.5, true),
            sp(200, 11.5, true),
        ];
        assert_eq!(pick_knee(&points, 120), 120);
    }

    /// Fail_open_ok==false points are excluded from consideration.
    #[test]
    fn excludes_fail_open_points() {
        let points = vec![
            sp(80, 20.0, false), // ineligible: fail_open_ok=false
            sp(100, 15.0, true),
            sp(150, 10.0, true),
            sp(300, 5.0, true),
        ];
        // Eligible: [100(15%), 150(10%), 300(5%)]
        // Walk: 300->150 gain=5.0, 150->100 gain=5.0 => all ok => return 100.
        assert_eq!(pick_knee(&points, 0), 100);
    }

    /// No eligible points => return floor_desc_chars (safe fallback).
    #[test]
    fn no_eligible_returns_floor() {
        let points = vec![
            sp(80, 15.0, false),  // ineligible
            sp(100, 10.0, false), // ineligible
        ];
        assert_eq!(pick_knee(&points, 120), 120);
    }

    /// Empty points slice => return floor_desc_chars.
    #[test]
    fn empty_slice_returns_floor() {
        let points: Vec<SweepPoint> = vec![];
        assert_eq!(pick_knee(&points, 80), 80);
    }

    /// Floor filters out points strictly below it.
    #[test]
    fn floor_filters_below() {
        let points = vec![
            sp(50, 20.0, true), // below floor 80 => ineligible
            sp(80, 18.0, true),
            sp(120, 12.0, true),
            sp(200, 6.0, true),
        ];
        // Eligible: [80(18%), 120(12%), 200(6%)]
        // Walk: 200->120 gain=6.0, 120->80 gain=6.0 => all ok => return 80.
        assert_eq!(pick_knee(&points, 80), 80);
    }

    // ── classify_verdict tests ──────────────────────────────────────────

    /// A curve with a real knee well above floor, recommendation (120) beats
    /// baseline (200) by 3.0pp => Worthwhile.
    #[test]
    fn verdict_clear_knee_is_worthwhile() {
        let points = vec![
            sp(80, 15.0, true),
            sp(100, 14.8, true),
            sp(120, 14.5, true),
            sp(150, 13.5, true),
            sp(200, 11.5, true),
            sp(300, 10.0, true),
            sp(500, 9.0, true),
        ];
        // pick_knee returns 120 for this curve (floor=0).
        // baseline at 200 = 11.5%, rec at 120 = 14.5%, gain = 3.0pp >= 3.0
        // recommended=120 != floor=0
        assert_eq!(
            classify_verdict(&points, 0, 120, 14.5, 11.5),
            TuneVerdict::Worthwhile
        );
    }

    /// Smooth curve whose recommendation is the floor itself, with small gain
    /// (2.0pp < 3.0) => Marginal.
    #[test]
    fn verdict_floor_recommendation_is_marginal() {
        let points = vec![
            sp(80, 15.0, true),
            sp(100, 14.8, true),
            sp(120, 14.5, true),
            sp(150, 13.5, true),
            sp(200, 11.5, true),
            sp(300, 10.0, true),
            sp(500, 9.0, true),
        ];
        // Floor=150 filters out 80,100,120.
        // Eligible: 150(13.5%), 200(11.5%), 300(10%), 500(9%).
        // pick_knee would walk: 500->300 gain=1.0, 300->200 gain=1.5,
        //   200->150 gain=2.0 => all OK => recommended=150 (the floor).
        // baseline at 200 = 11.5%, rec at 150 = 13.5%, gain = 2.0pp < 3.0.
        // ceiling_gain = 13.5 - 11.5 = 2.0 >= 1.0 (not NotWorth).
        // rec==floor, so NOT Worthwhile => Marginal.
        assert_eq!(
            classify_verdict(&points, 150, 150, 13.5, 11.5),
            TuneVerdict::Marginal
        );
    }

    /// Nearly flat curve where ceiling gain < 1.0 => NotWorth.
    #[test]
    fn verdict_flat_curve_is_notworth() {
        let points = vec![
            sp(80, 3.8, true),
            sp(100, 3.7, true),
            sp(120, 3.6, true),
            sp(150, 3.5, true),
            sp(200, 3.4, true),
        ];
        // ceiling_gain = max(3.8, 3.7, 3.6, 3.5, 3.4) - 3.4 = 0.4 < 1.0
        assert_eq!(
            classify_verdict(&points, 0, 200, 3.4, 3.4),
            TuneVerdict::NotWorth
        );
    }

    /// No eligible points (all fail_open_ok=false) => NotWorth.
    #[test]
    fn verdict_no_eligible_is_notworth() {
        let points = vec![sp(80, 15.0, false), sp(100, 14.8, false)];
        assert_eq!(
            classify_verdict(&points, 0, 100, 14.8, 14.8),
            TuneVerdict::NotWorth
        );
    }

    /// Empty points slice => NotWorth.
    #[test]
    fn verdict_empty_points_is_notworth() {
        let points: Vec<SweepPoint> = vec![];
        assert_eq!(
            classify_verdict(&points, 0, 80, 0.0, 0.0),
            TuneVerdict::NotWorth
        );
    }

    // ── corpus JOIN parity ──────────────────────────────────────────────────

    /// The `requests` metadata rows (with the live schema's `target` column)
    /// that `run_bucket_tune`'s JOIN filters on.
    fn seed_requests(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE requests (
                 run_id INTEGER NOT NULL,
                 seq    INTEGER NOT NULL,
                 target TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_requests_run_seq ON requests(run_id, seq);",
        )
        .expect("requests schema");
        for (run_id, seq) in [(1i64, 1i64), (1, 2), (2, 1)] {
            conn.execute(
                "INSERT INTO requests (run_id, seq, target) VALUES (?1, ?2, 'native')",
                rusqlite::params![run_id, seq],
            )
            .expect("insert request");
        }
    }

    /// Insert three bodies (two runs; `run_id`-grouped so the sweep's session
    /// grouping sees identical order on both sides). `request_bodies` must
    /// already exist on `conn` — created either by the legacy hand-rolled table
    /// or by `corpus::open_write`.
    fn seed_bodies(conn: &Connection) {
        for (run_id, seq) in [(1u64, 1u64), (1, 2), (2, 1)] {
            let body = format!(
                r#"{{"model":"claude-sonnet-4-6","messages":[{{"role":"user","content":[{{
                     "type":"tool_result","tool_use_id":"tu_{run_id}_{seq}",
                     "content":"{}"}}]}}]}}"#,
                "A".repeat(9000)
            );
            insert_body(
                conn,
                run_id,
                seq,
                "2026-08-01T00:00:00Z",
                None,
                "anthropic",
                &body,
                0,
            );
        }
    }

    /// Run `run_bucket_tune` and reduce the result to the numbers that prove
    /// the JOIN fed identical rows: the body count plus every sweep point's
    /// values. Same computation on both sides => bit-identical floats.
    fn tune_summary(db_path: &Path) -> Result<(usize, Vec<(usize, usize, bool, f64)>), String> {
        let res = run_bucket_tune(
            db_path,
            &NativeKnobs::default(),
            &[4000],
            0,
            0,
            Bucket::Native,
            SweepKnob::DescChars,
            |_, _| {},
        )?
        .expect("corpus must not be empty");
        let points = res
            .points
            .iter()
            .map(|p| (p.desc_chars, p.n_trimmed, p.fail_open_ok, p.avg_trim_pct))
            .collect();
        Ok((res.n_bodies, points))
    }

    /// The whole split exists so the `request_bodies` JOIN can read the same
    /// rows through an attached per-provider corpus file as it did when both
    /// tables lived in one legacy database. This is the property that test
    /// pins: identical rows in, identical tune numbers out.
    #[test]
    fn bucket_tune_join_parity_legacy_vs_split() {
        let tmp = TempDir::new().expect("tempdir");

        // Legacy arrangement: both tables in one proxy.db — `attach_read` falls
        // back to `main.request_bodies` because no per-provider file exists.
        let legacy_dir = tmp.path().join("legacy");
        std::fs::create_dir_all(&legacy_dir).expect("legacy dir");
        let legacy_db = legacy_dir.join("proxy.db");
        {
            let conn = Connection::open(&legacy_db).expect("legacy open");
            conn.execute_batch(
                "CREATE TABLE request_bodies (
                     id        INTEGER PRIMARY KEY AUTOINCREMENT,
                     run_id    INTEGER NOT NULL,
                     seq       INTEGER NOT NULL,
                     ts        TEXT    NOT NULL,
                     model     TEXT,
                     provider  TEXT,
                     body      BLOB    NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_request_bodies_run_seq
                     ON request_bodies(run_id, seq);",
            )
            .expect("legacy request_bodies schema");
            seed_bodies(&conn);
            seed_requests(&conn);
        }

        // Split arrangement: requests in proxy.db, bodies in corpus-anthropic.db
        // — `attach_read` attaches the per-provider file as the `corpus` schema.
        let split_dir = tmp.path().join("split");
        std::fs::create_dir_all(&split_dir).expect("split dir");
        let split_db = split_dir.join("proxy.db");
        {
            let conn = Connection::open(&split_db).expect("split main open");
            seed_requests(&conn);
        }
        {
            let per = open_write(&split_dir, "anthropic").expect("split corpus open");
            seed_bodies(&per);
        }

        let legacy = tune_summary(&legacy_db).expect("legacy tune must run");
        let split = tune_summary(&split_db).expect("split tune must run");
        assert_eq!(
            legacy, split,
            "the JOIN must feed identical rows through the attached corpus file \
             and through the legacy single-database table"
        );
        assert_eq!(
            legacy.0, 3,
            "both arrangements must surface all three bodies"
        );
    }
}
