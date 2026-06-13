pub mod anthropic;
pub mod upstream;

use std::sync::Arc;

use crate::state::AppState;

/// Decrements the in-flight counter when dropped, so it stays accurate even if
/// a request errors out or the future/stream is cancelled mid-flight. Owns an
/// `Arc<AppState>` so it can be moved into a streaming response body and live
/// for the full duration of the stream.
pub struct InflightGuard(pub Arc<AppState>);

impl InflightGuard {
    pub fn new(state: Arc<AppState>) -> Self {
        state.inflight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(state)
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Wall-clock timestamp (`HH:MM:SS.mmm` UTC) for log line correlation.
pub fn now_ms() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}
