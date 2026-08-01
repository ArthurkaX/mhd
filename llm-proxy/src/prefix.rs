//! Prefix-shape measurement: how much of a request is byte-identical to the
//! previous request of the same client session on the same route.
//!
//! # What this is *not*
//!
//! This is **not** a cache metric and must never be read as one. A route can be
//! 92% prefix-shaped and still report zero `cache_read_tokens` — `sva-ollama/glm-5.2`
//! does exactly that. Whether an upstream actually caches is answered by
//! [`crate::db_log::RouteCacheRow`] and `GET /stats/routes`, from the numbers the
//! upstream itself reports. The two live in separate columns, named differently,
//! on purpose: prefix shape says *"the request was cacheable in principle"*,
//! cache_read says *"the upstream charged us less for it"*. Combining them
//! produces a number that means nothing.
//!
//! What prefix shape *is* good for: it is the denominator for the cache question.
//! A route that reports no cache hits while receiving 5% prefix-shared requests
//! is behaving correctly. One that does it while receiving 95% is leaving money
//! on the table, and that is worth knowing before switching harnesses.
//!
//! # Shape of the digest
//!
//! A request is reduced to a list of `(hash, length)` pairs, one per prefix-
//! relevant part, in wire order:
//!
//! 1. a synthetic entry for `system`
//! 2. a synthetic entry for `tools`
//! 3. one entry per message
//!
//! The two synthetic leading entries matter. Providers key the prompt cache on
//! the whole serialized prefix, system and tool schemas included, and tool
//! schemas are typically the largest single stable block in an agent request.
//! Digesting messages alone would report a long shared prefix for a request
//! whose tool list had silently changed — systematically overstating how
//! cache-friendly a harness is.
//!
//! Lengths are UTF-8 byte counts of the canonical JSON for that part. That is
//! the unit the wire and the tokenizer both roughly track; "chars" in the column
//! name is a convenience, not a Unicode scalar count.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// How long a session entry stays eligible for comparison. Past this, a request
/// is treated as the start of a new session: provider prompt caches expire on a
/// similar horizon, so a "shared prefix" measured across a longer gap would not
/// have been a cache hit anyway.
const TTL_MS: u64 = 5 * 60 * 1000;

/// Max live sessions tracked. Bounds memory on a proxy that sees many distinct
/// clients; the oldest entry is evicted when full.
const MAX_SESSIONS: usize = 512;

/// Max digest entries persisted per row. `requests` is uncapped (unlike
/// `request_bodies`, which keeps 5000), so an unbounded digest would grow the DB
/// without limit. Beyond this the digest is dropped rather than truncated — a
/// truncated list would compare as a shorter conversation and silently fake a
/// shorter shared prefix.
const MAX_DIGEST_ENTRIES: usize = 512;

/// One `(hash, byte_len)` pair — a single prefix-relevant part of a request.
pub type Part = (u64, u32);

/// Per-request prefix measurements, ready to be written to the `requests` row.
///
/// Every field is `None` when it could not be measured, never 0. On the first
/// request of a session there is nothing to compare against, which is not the
/// same fact as "compared and shared nothing".
#[derive(Debug, Clone, Default)]
pub struct PrefixStats {
    /// JSON `[["<hex hash>", <len>], ...]` of the request as sent upstream.
    pub msg_digest: Option<String>,
    /// Total bytes across all digest parts — the denominator for the two
    /// shared-prefix counts.
    pub msg_total_chars: Option<u64>,
    /// Longest common prefix, in bytes, against the previous request of this
    /// session **before trim**. This is the harness's own prefix discipline,
    /// measured independently of anything the proxy does to the body.
    pub prefix_shared_chars: Option<u64>,
    /// Same, measured **after trim**, on the bytes actually sent upstream.
    /// Divergence from `prefix_shared_chars` is trim breaking a prefix the
    /// harness had kept stable — the one number that says our own compression
    /// is costing cache hits.
    pub prefix_shared_chars_sent: Option<u64>,
}

/// FNV-1a over the canonical JSON of a value. Same construction as the existing
/// `prefix_hash` in `handlers`, applied per part rather than to the whole head.
fn hash_value(v: &Value) -> Part {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in &bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    (hash, bytes.len() as u32)
}

/// Digest an Anthropic Messages request: `system`, `tools`, then each message.
///
/// Absent `system`/`tools` still produce an entry (hashing `null`), so a request
/// that *gains* a system prompt does not accidentally align with one that never
/// had it.
pub fn digest_anthropic(payload: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    parts.push(hash_value(payload.get("system").unwrap_or(&Value::Null)));
    parts.push(hash_value(payload.get("tools").unwrap_or(&Value::Null)));
    if let Some(msgs) = payload.get("messages").and_then(|m| m.as_array()) {
        parts.extend(msgs.iter().map(hash_value));
    }
    parts
}

/// Digest an OpenAI chat-completions request. There is no separate `system`
/// field — the system prompt is the first message — so only `tools` is
/// synthetic, and it is placed first to mirror [`digest_anthropic`]'s layout.
pub fn digest_openai(payload: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    parts.push(hash_value(payload.get("tools").unwrap_or(&Value::Null)));
    if let Some(msgs) = payload.get("messages").and_then(|m| m.as_array()) {
        parts.extend(msgs.iter().map(hash_value));
    }
    parts
}

/// Bytes shared by the leading parts of two digests. Stops at the first part
/// that differs in either hash or length.
pub(crate) fn shared_prefix_chars(prev: &[Part], cur: &[Part]) -> u64 {
    prev.iter()
        .zip(cur.iter())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.1 as u64)
        .sum()
}

fn total_chars(parts: &[Part]) -> u64 {
    parts.iter().map(|p| p.1 as u64).sum()
}

/// Render a digest as the stored JSON: `[["<hex>", <len>], ...]`.
/// Returns `None` past [`MAX_DIGEST_ENTRIES`] — see the constant's docs for why
/// this drops rather than truncates.
fn digest_json(parts: &[Part]) -> Option<String> {
    if parts.len() > MAX_DIGEST_ENTRIES {
        return None;
    }
    let rows: Vec<Value> = parts
        .iter()
        .map(|(h, len)| serde_json::json!([format!("{h:016x}"), len]))
        .collect();
    serde_json::to_string(&rows).ok()
}

struct Session {
    /// Digest of the previous request before trim.
    pre: Vec<Part>,
    /// Digest of the previous request as actually sent upstream.
    sent: Vec<Part>,
    /// When it was recorded, for TTL and for evicting the oldest session.
    at_ms: u64,
}

/// In-memory, TTL'd map of the last request seen per `(client run, route)`.
///
/// Deliberately not a SQLite lookup: the previous digest is needed on the
/// request hot path, and `db_log` is a single `Mutex<Connection>` shared with
/// every insert and update the proxy makes. A read there would serialize
/// requests behind unrelated writes to save a few hundred bytes of RAM.
#[derive(Default)]
pub struct PrefixTracker {
    sessions: Mutex<HashMap<String, Session>>,
}

impl PrefixTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this request's digests and measure them against the previous
    /// request of the same session. Returns the stats for the `requests` row.
    ///
    /// `key` should identify a client conversation on a route — see
    /// [`session_key`].
    pub fn observe(&self, key: &str, pre: Vec<Part>, sent: Vec<Part>, now_ms: u64) -> PrefixStats {
        let mut stats = PrefixStats {
            msg_digest: digest_json(&sent),
            msg_total_chars: Some(total_chars(&sent)),
            ..Default::default()
        };

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());

        // Drop expired entries opportunistically — no background timer needed,
        // and the map is small enough that a full scan per request is cheaper
        // than maintaining an eviction order.
        sessions.retain(|_, s| now_ms.saturating_sub(s.at_ms) < TTL_MS);

        if let Some(prev) = sessions.get(key) {
            stats.prefix_shared_chars = Some(shared_prefix_chars(&prev.pre, &pre));
            stats.prefix_shared_chars_sent = Some(shared_prefix_chars(&prev.sent, &sent));
        }

        if sessions.len() >= MAX_SESSIONS
            && !sessions.contains_key(key)
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.at_ms)
                .map(|(k, _)| k.clone())
        {
            sessions.remove(&oldest);
        }
        sessions.insert(
            key.to_string(),
            Session {
                pre,
                sent,
                at_ms: now_ms,
            },
        );

        stats
    }
}

/// Build the session key for the tracker.
///
/// Prefers the client's `x-client-run-id`. When the client does not send one,
/// falls back to its User-Agent, which pairs consecutive same-route requests
/// from the same tool — weaker, since two concurrent sessions of the same tool
/// collapse into one key and will read as a short shared prefix. The row's
/// `client_run_id` column records which case applied: NULL there means the
/// pairing was UA-based and the number is a lower bound.
pub fn session_key(client_run_id: Option<&str>, user_agent: Option<&str>, route: &str) -> String {
    match client_run_id {
        Some(id) => format!("run:{id}\u{1}{route}"),
        None => format!("ua:{}\u{1}{route}", user_agent.unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(system: &str, tools: Value, msgs: Vec<Value>) -> Value {
        json!({"system": system, "tools": tools, "messages": msgs})
    }

    #[test]
    fn first_request_of_a_session_has_no_shared_prefix_not_zero() {
        let t = PrefixTracker::new();
        let d = digest_anthropic(&body("s", json!([]), vec![json!({"role": "user"})]));
        let stats = t.observe("k", d.clone(), d, 0);
        assert!(stats.prefix_shared_chars.is_none());
        assert!(stats.prefix_shared_chars_sent.is_none());
        assert!(stats.msg_total_chars.unwrap() > 0);
    }

    #[test]
    fn appending_a_turn_shares_the_whole_previous_body() {
        let t = PrefixTracker::new();
        let m1 = json!({"role": "user", "content": "hello"});
        let m2 = json!({"role": "assistant", "content": "hi"});
        let first = digest_anthropic(&body("s", json!([]), vec![m1.clone()]));
        let second = digest_anthropic(&body("s", json!([]), vec![m1, m2]));
        let expected = total_chars(&first);

        t.observe("k", first.clone(), first, 0);
        let stats = t.observe("k", second.clone(), second, 1);
        assert_eq!(stats.prefix_shared_chars, Some(expected));
    }

    #[test]
    fn changing_tools_breaks_the_prefix_even_when_messages_match() {
        let t = PrefixTracker::new();
        let msgs = vec![json!({"role": "user", "content": "hello"})];
        let first = digest_anthropic(&body("s", json!([{"name": "a"}]), msgs.clone()));
        let second = digest_anthropic(&body("s", json!([{"name": "b"}]), msgs));

        t.observe("k", first.clone(), first, 0);
        let stats = t.observe("k", second.clone(), second, 1);
        // Only the `system` entry survives; tools and everything after it are
        // past the break. This is the case a messages-only digest would miss.
        let system_len = hash_value(&json!("s")).1 as u64;
        assert_eq!(stats.prefix_shared_chars, Some(system_len));
    }

    #[test]
    fn trim_breaking_the_prefix_shows_up_only_in_the_sent_column() {
        let t = PrefixTracker::new();
        let m1 = json!({"role": "user", "content": "long tool result"});
        let m2 = json!({"role": "assistant", "content": "ok"});
        let pre1 = digest_anthropic(&body("s", json!([]), vec![m1.clone()]));
        let pre2 = digest_anthropic(&body("s", json!([]), vec![m1.clone(), m2.clone()]));
        // Trim rewrote the first message on the second request only.
        let elided = json!({"role": "user", "content": "long …"});
        let sent1 = pre1.clone();
        let sent2 = digest_anthropic(&body("s", json!([]), vec![elided, m2]));

        t.observe("k", pre1, sent1, 0);
        let stats = t.observe("k", pre2, sent2, 1);
        assert!(stats.prefix_shared_chars.unwrap() > stats.prefix_shared_chars_sent.unwrap());
    }

    #[test]
    fn a_stale_session_is_not_compared_against() {
        let t = PrefixTracker::new();
        let d = digest_anthropic(&body("s", json!([]), vec![json!({"role": "user"})]));
        t.observe("k", d.clone(), d.clone(), 0);
        let stats = t.observe("k", d.clone(), d, TTL_MS + 1);
        assert!(stats.prefix_shared_chars.is_none());
    }

    #[test]
    fn distinct_sessions_do_not_bleed_into_each_other() {
        let t = PrefixTracker::new();
        let a = digest_anthropic(&body("a", json!([]), vec![json!({"role": "user"})]));
        let b = digest_anthropic(&body("b", json!([]), vec![json!({"role": "user"})]));
        t.observe(&session_key(Some("r1"), None, "route"), a.clone(), a, 0);
        let stats = t.observe(&session_key(Some("r2"), None, "route"), b.clone(), b, 1);
        assert!(stats.prefix_shared_chars.is_none());
    }

    #[test]
    fn oversized_digests_are_dropped_not_truncated() {
        let msgs: Vec<Value> = (0..MAX_DIGEST_ENTRIES + 1)
            .map(|i| json!({"role": "user", "content": i}))
            .collect();
        let d = digest_anthropic(&body("s", json!([]), msgs));
        assert!(digest_json(&d).is_none());
        // The counts are still measurable — only the per-part list is dropped.
        assert!(total_chars(&d) > 0);
    }
}
