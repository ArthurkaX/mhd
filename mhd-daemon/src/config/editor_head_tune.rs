//! Background Tune runner for the LLM Trim settings page. Runs the
//! tool_result_head sweep across the three corpus buckets and exposes the
//! measured results so each head dropdown can show real trim% per value.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::editor_state::{HeadGroup, HEAD_SWEEP};

/// Minimum bodies in a bucket before we trust its numbers; below this we
/// report "not enough data yet".
const MIN_BODIES: usize = 20;

/// One bucket's measured sweep, reduced to what the UI needs.
#[derive(Clone)]
pub struct HeadBucketOut {
    pub recommended: usize,
    pub verdict: String,
    pub n_bodies: usize,
    /// (swept head value, avg_trim_pct) for every HEAD_SWEEP point.
    pub points: Vec<(usize, f64)>,
}

#[derive(Clone)]
pub struct HeadTuneProgress {
    pub running: bool,
    pub completed: bool,
    /// Reserved for surfacing a run-level failure; runs currently collapse
    /// per-bucket errors into `None` results rather than a global error.
    #[allow(dead_code)]
    pub error: Option<String>,
    pub done_buckets: usize,
    /// index 0 = Native, 1 = CcGateway, 2 = OtherOpenai
    pub results: [Option<HeadBucketOut>; 3],
}

impl HeadTuneProgress {
    fn fresh() -> Self {
        Self { running: true, completed: false, error: None, done_buckets: 0,
               results: [None, None, None] }
    }
}

static HEAD_TUNE: Mutex<Option<Arc<Mutex<HeadTuneProgress>>>> = Mutex::new(None);
static HEAD_TUNE_RUNNING: AtomicBool = AtomicBool::new(false);

struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Some(arc) = HEAD_TUNE.lock().unwrap().clone() {
            if let Ok(mut p) = arc.lock() { p.running = false; p.completed = true; }
        }
        HEAD_TUNE_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// True while a run is in flight.
pub fn is_running() -> bool { HEAD_TUNE_RUNNING.load(Ordering::SeqCst) }

/// Clone the current progress snapshot, if a run has ever started.
pub fn snapshot() -> Option<HeadTuneProgress> {
    HEAD_TUNE.lock().unwrap().clone().and_then(|arc| arc.lock().ok().map(|p| p.clone()))
}

/// Start a background head sweep across the three buckets. No-op if already running.
pub fn start() {
    if HEAD_TUNE_RUNNING.swap(true, Ordering::SeqCst) { return; }
    let arc = Arc::new(Mutex::new(HeadTuneProgress::fresh()));
    *HEAD_TUNE.lock().unwrap() = Some(arc.clone());

    std::thread::Builder::new()
        .name("mhd-head-tune".into())
        .spawn(move || {
            let _guard = RunGuard;
            let db = llm_proxy::config::config_dir().join("proxy.db");
            let base = llm_proxy::native_trim::NativeKnobs::default();
            let sweep: Vec<usize> = HEAD_SWEEP.to_vec();
            let buckets = [
                llm_proxy::tune::Bucket::Native,
                llm_proxy::tune::Bucket::CcGateway,
                llm_proxy::tune::Bucket::OtherOpenai,
            ];
            for (idx, &bucket) in buckets.iter().enumerate() {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    llm_proxy::tune::run_bucket_tune(
                        &db, &base, &sweep, 200, 120, bucket,
                        llm_proxy::tune::SweepKnob::ToolResultHead, |_d, _t| {},
                    )
                }));
                let out = match outcome {
                    Ok(Ok(Some(r))) => Some(HeadBucketOut {
                        recommended: r.recommended,
                        verdict: format!("{:?}", r.verdict),
                        n_bodies: r.n_bodies,
                        points: r.points.iter().map(|p| (p.desc_chars, p.avg_trim_pct)).collect(),
                    }),
                    _ => None, // Ok(None), Err, or panic all collapse to "no data"
                };
                if let Ok(mut p) = arc.lock() { p.results[idx] = out; p.done_buckets = idx + 1; }
            }
        })
        .ok();
}

/// Results index for a group's bucket.
fn group_bucket_idx(group: HeadGroup) -> usize {
    match group {
        HeadGroup::NativeBig | HeadGroup::NativeHaiku => 0,
        HeadGroup::Harness => 2,
    }
}

/// Measured description for one head value in a group, or None if unmeasured /
/// insufficient. Format e.g. "trim 41.2%" (+ "  ★ rec" when this is the knee).
pub fn measured_value_desc(group: HeadGroup, head: usize) -> Option<String> {
    let snap = snapshot()?;
    let out = snap.results[group_bucket_idx(group)].as_ref()?;
    if out.n_bodies < MIN_BODIES { return None; }
    let (_, pct) = out.points.iter().find(|(v, _)| *v == head)?;
    let star = if out.recommended == head { "  \u{2605} rec" } else { "" };
    Some(format!("trim {:.1}%{}", pct, star))
}

/// One-line status under a group row: measured recommendation, or a running /
/// low-data note, or None to fall back to the canned help text.
pub fn measured_group_line(group: HeadGroup) -> Option<String> {
    let snap = snapshot()?;
    if snap.running { return Some("Calculating\u{2026}".to_string()); }
    let out = snap.results[group_bucket_idx(group)].as_ref();
    match out {
        Some(o) if o.n_bodies >= MIN_BODIES => Some(format!(
            "Tune: rec {} \u{00b7} {} (n={})", o.recommended, o.verdict, o.n_bodies)),
        _ if snap.completed => Some("Not enough data yet \u{2014} keep capturing.".to_string()),
        _ => None,
    }
}

/// Label for the Calculate button given current state.
pub fn calculate_button_label() -> String {
    match snapshot() {
        Some(p) if p.running => format!("Calculating\u{2026} ({}/3)", p.done_buckets),
        _ => "Calculate".to_string(),
    }
}
