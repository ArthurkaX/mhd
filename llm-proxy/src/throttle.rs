//! Outbound rate limiter for the native Anthropic path.
//!
//! A token bucket smooths bursts of parallel requests from Claude Code into a
//! steady stream, so they spread over time instead of hitting Anthropic's
//! per-minute request rate limit all at once (which returns HTTP 429). This is
//! a RATE limiter (requests/sec), not a concurrency limiter: a permit is taken
//! only at send time and is never held for the request's duration.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cap on a single sleep inside [`TokenBucket::acquire`], so a misconfigured
/// rate can't hang a request forever. The loop re-checks after waking.
const MAX_SLEEP_SECS: f64 = 5.0;

/// Token-bucket rate limiter. `capacity` tokens accrue at `refill_per_sec`.
/// Acquiring a permit waits until a whole token is available.
pub struct TokenBucket {
 inner: Mutex<Bucket>,
}

struct Bucket {
 tokens: f64,
 capacity: f64,
 refill_per_sec: f64,
 last: Instant,
}

impl TokenBucket {
 /// Create a bucket with the given burst `capacity` (in whole tokens) and a
 /// steady refill rate of `refill_per_sec` tokens per second. The bucket
 /// starts full.
 pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
 Self {
 inner: Mutex::new(Bucket {
 tokens: capacity,
 capacity,
 refill_per_sec,
 last: Instant::now(),
 }),
 }
 }

 /// Live-tunable: update the bucket's capacity and refill rate. Current token
 /// balance is clamped to the new capacity (a shrinking capacity drops surplus
 /// tokens; a growing one is filled gradually by subsequent refills).
 pub fn set_rate(&self, capacity: f64, refill_per_sec: f64) {
 let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
 b.capacity = capacity;
 b.refill_per_sec = refill_per_sec;
 if b.tokens > capacity {
 b.tokens = capacity;
 }
 }

 /// Acquire one permit, waiting (in a cooperative async loop) until a whole
 /// token is available. If `refill_per_sec <= 0`, returns immediately — the
 /// caller decides whether the throttle is enabled, but we guard the division
 /// defensively so we can't divide by zero.
 pub async fn acquire(&self) {
 loop {
 let wait_secs = {
 let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
 if b.refill_per_sec <= 0.0 {
 // Throttle effectively disabled — don't divide by zero.
 return;
 }
 let now = Instant::now();
 let elapsed = now.duration_since(b.last).as_secs_f64();
 b.last = now;
 b.tokens = (b.tokens + elapsed * b.refill_per_sec).min(b.capacity);
 if b.tokens >= 1.0 {
 b.tokens -= 1.0;
 return;
 }
 // How long until exactly 1 token accrues at the current rate.
 (1.0 - b.tokens) / b.refill_per_sec
 };
 // Sleep WITHOUT holding the Mutex across the await.
 let sleep_secs = wait_secs.min(MAX_SLEEP_SECS);
 if sleep_secs > 0.0 {
 tokio::time::sleep(Duration::from_secs_f64(sleep_secs)).await;
 }
 }
 }
}