//! Offline replay of the recorded corpus through a *modeled* prefix cache,
//! comparing raw bodies (OFF arm) against native-trim bodies (ON arm) under
//! Anthropic's cache pricing weights (input 1.0x, cache write 1.25x, cache
//! read 0.1x).
//!
//! # This is a model, not billing data
//!
//! Token counts come from `chars/4` over the serialized JSON, and the cache
//! split is **byte-proportional** (a shared prefix of `S` bytes out of `N`
//! total is treated as `S/N` of the tokens). BOTH of these biases *understate*
//! trim's benefit: trim cuts dense code, where tokens-per-byte is highest,
//! while the shared `system`/`tools` prefix that stays cached is prose. The
//! tool therefore reports a **floor** on savings — it never inflates them.
//!
//! The replay is deterministic and read-only: `request_bodies` is grouped into
//! per-conversation chains (see [`run_cache_bench`]), each chain is walked
//! twice — once sending bodies verbatim, once through [`trim_native`] — and
//! the arms are compared on estimated quota-equivalent cost. The point of the
//! comparison is the *prefix break*: a transform that shortens the shared
//! prefix moves tokens out of the 0.1x cache-read bucket into the 1.0x input
//! bucket, so weighted cost can rise even while raw tokens fall.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::db_log::decompress_body;
use crate::native_trim::{NativeKnobs, trim_native};
// `shared_bytes` is the live prefix tracker's own comparison, reused verbatim
// rather than mirrored: calibrating this replay against the
// `prefix_shared_chars_sent` column is only meaningful while both measure the
// same thing, and two copies would drift silently.
use crate::prefix::{Part, digest_anthropic, shared_prefix_chars as shared_bytes};

// ── constants ───────────────────────────────────────────────────────────────

/// Anthropic quota-cost multipliers, relative to a fresh input token (= 1.0).
/// Cache writes cost ~1.25x; cache reads are billed at ~0.1x. Same values as
/// `measure.rs` so live and offline numbers stay comparable.
pub const W_INPUT: f64 = 1.0;
pub const W_CACHE_CREATION: f64 = 1.25;
pub const W_CACHE_READ: f64 = 0.10;

/// A session entry older than this is a cold start. Same horizon as
/// `prefix::TTL_MS` (5 minutes): provider prompt caches expire on a similar
/// timescale, so a "shared prefix" measured across a longer gap would not have
/// been a cache hit anyway.
const TTL_MS: u64 = 5 * 60 * 1000;

/// Relative weighted-cost gap that separates a signal from noise. Token
/// estimates are chars/4, too coarse to trust a smaller delta; 2% still
/// catches the effect this tool exists to expose — a prefix break moving
/// tokens from the 0.1x cache-read bucket to the 1.0x input bucket.
const VERDICT_REL: f64 = 0.02;

/// How many worst prefix breaks to keep in [`CacheBenchResult::worst_breaks`].
const MAX_WORST_BREAKS: usize = 10;

// ── public types ────────────────────────────────────────────────────────────

/// Per-arm aggregate over a full replay.
#[derive(Debug, Clone, Default)]
pub struct ArmBuckets {
    pub n_requests: usize,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Total estimated tokens sent, unweighted (the `chars/4` sum over every
    /// request body this arm sent).
    pub raw_tokens: u64,
    /// Requests where the ON arm's shared prefix was shorter than the OFF
    /// arm's. A break moves tokens out of the 0.1x cache-read bucket into the
    /// 1.0x input bucket, so weighted cost can rise even while raw tokens fall.
    pub n_prefix_breaks: usize,
    /// Estimated tokens lost to prefix breaks. Measured WITHIN the ON arm only
    /// (ON-arm bytes over the parts the raw arm matched but the trimmed arm did
    /// not, converted at the ON arm's token density), so it is bounded by the ON
    /// arm's own per-request totals. Never computed as a cross-arm byte
    /// subtraction `shared_off - shared_on`: those two byte counts are on
    /// different scales (the ON prefix is smaller partly because trim deleted
    /// content — the tool working, not a loss), so a cross-arm difference would
    /// count every successful trim's saved bytes as loss. Informational only —
    /// the cost of a break is already reflected in the bucket split above.
    pub prefix_break_tokens: u64,
}

impl ArmBuckets {
    /// Quota-equivalent cost in fresh-input-token units.
    pub fn weighted_cost(&self) -> f64 {
        self.input_tokens as f64 * W_INPUT
            + self.cache_creation_tokens as f64 * W_CACHE_CREATION
            + self.cache_read_tokens as f64 * W_CACHE_READ
    }
}

/// Verdict on whether trim helps under cache pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheVerdict {
    /// ON arm's weighted cost is meaningfully below OFF.
    Proven,
    /// Neither arm is meaningfully cheaper (within ±[`VERDICT_REL`]).
    Inconclusive,
    /// ON arm's weighted cost is meaningfully above OFF — trim is costing
    /// cache hits faster than it saves tokens.
    Backwards,
}

impl std::fmt::Display for CacheVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheVerdict::Proven => write!(f, "PROVEN"),
            CacheVerdict::Inconclusive => write!(f, "INCONCLUSIVE"),
            CacheVerdict::Backwards => write!(f, "BACKWARDS"),
        }
    }
}

/// One request where trim shortened the shared prefix, so tokens the raw arm
/// would have read from cache (0.1x) had to be sent as fresh input (1.0x).
///
/// `tokens_lost` is measured **within the ON (trimmed) arm only**: the bytes
/// of the ON arm's own parts that lie in the part index range
/// `[shared_parts_on, shared_parts_off)` — the parts the raw arm matched but
/// the trimmed arm did not — summed in ON-arm bytes and converted at the ON
/// arm's token density. That makes it a genuine loss: a part trim shrank is
/// billed by its remaining size, not its original one, so successful trimming
/// contributes nothing here.
///
/// Do not "simplify" this back to a cross-arm byte subtraction
/// (`shared_off - shared_on`): the two byte counts are on different scales —
/// the ON prefix is smaller partly because trim legitimately deleted content —
/// so a cross-arm difference is inflated by every byte trim saved and is not a
/// loss at all.
#[derive(Debug, Clone)]
pub struct PrefixBreak {
    pub run_id: i64,
    pub seq: i64,
    /// Shared prefix bytes on the OFF (raw) arm.
    pub shared_off: u32,
    /// Shared prefix bytes on the ON (trimmed) arm.
    pub shared_on: u32,
    /// NOT comparable to each other in bytes (see the struct doc): they exist
    /// only to show that a break occurred. The comparable quantity is the
    /// matched part count — this field vs [`Self::shared_parts_on`].
    pub shared_parts_off: u32,
    /// Matched digest parts on the ON (trimmed) arm. A drop below
    /// [`Self::shared_parts_off`] is exactly what defines a break.
    pub shared_parts_on: u32,
    /// Estimated tokens lost to the break, measured within the ON arm's own
    /// body and token density (hence bounded by the ON arm's total tokens for
    /// this request). Informational only.
    pub tokens_lost: u64,
}

/// Result of a completed [`run_cache_bench`] replay.
#[derive(Debug, Clone)]
pub struct CacheBenchResult {
    pub off: ArmBuckets,
    pub on: ArmBuckets,
    pub n_chains: usize,
    pub verdict: CacheVerdict,
    /// Up to [`MAX_WORST_BREAKS`] breaks, largest loss first.
    pub worst_breaks: Vec<PrefixBreak>,
}

// ── internals ───────────────────────────────────────────────────────────────

/// A single recorded request, fully decoded.
pub(crate) struct ChainRequest {
    pub(crate) run_id: i64,
    pub(crate) seq: i64,
    /// Epoch millis parsed from the row's `YYYY-MM-DD HH:MM:SS.mmm` timestamp.
    pub(crate) ts_ms: i64,
    /// PRE-trim body (what `request_bodies` stores).
    pub(crate) body: Value,
}

/// A per-conversation chain: requests of one `(run_id, session_hash)`,
/// ordered by `seq`.
pub(crate) struct Chain {
    pub(crate) requests: Vec<ChainRequest>,
}

/// Estimated tokens (`~chars/4` over the serialized body). Same estimator as
/// `bench::est_tokens` / the `backtest` binary, so all offline numbers agree.
pub(crate) fn est_tokens(v: &Value) -> u64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) as u64) / 4
}

/// Whether a `cache_control` key appears anywhere in the body (recursive).
/// Trim neither adds nor removes these markers, so this is arm-independent.
fn contains_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            if map.contains_key("cache_control") {
                return true;
            }
            map.values().any(contains_cache_control)
        }
        Value::Array(arr) => arr.iter().any(contains_cache_control),
        _ => false,
    }
}

/// Reproduce `handlers::session_hash` (private there): `DefaultHasher` over
/// the `system` field plus the first user message, both via `Value::to_string()`.
pub(crate) fn session_hash(payload: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Some(system) = payload.get("system") {
        system.to_string().hash(&mut hasher);
    }
    if let Some(messages) = payload.get("messages").and_then(|m| m.as_array())
        && let Some(first_user) = messages
            .iter()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        first_user.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// Parse a `YYYY-MM-DD HH:MM:SS.mmm` (UTC) timestamp — the exact format
/// `providers::now_ms` writes — into epoch milliseconds. Returns `None` on any
/// malformed input. Manual parser; no new crates.
fn parse_ts(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 23 {
        return None;
    }
    // Structural separators, checked before any arithmetic.
    if b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b' '
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
    {
        return None;
    }
    let year = parse_int(&b[0..4])?;
    let month = parse_int(&b[5..7])?;
    let day = parse_int(&b[8..10])?;
    let hour = parse_int(&b[11..13])?;
    let minute = parse_int(&b[14..16])?;
    let sec = parse_int(&b[17..19])?;
    let ms = parse_int(&b[20..23])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let secs = days * 86400 + hour * 3600 + minute * 60 + sec;
    Some(secs * 1000 + ms)
}

fn parse_int(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian; Howard
/// Hinnant's `days_from_civil` algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// One request's modeled cost, emitted per arm so a replay can be joined
/// against the live `requests` rows for calibration.
#[derive(Debug, Clone, Default)]
pub struct RequestCost {
    pub run_id: i64,
    pub seq: i64,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Always equals the three buckets summed.
    pub total_tokens: u64,
    pub shared_bytes: u64,
}

/// One request's shared-prefix accounting, returned by [`replay_request`] for
/// the prefix-break comparison between arms.
struct SharedInfo {
    /// Shared prefix bytes vs the arm's previous request.
    shared_bytes: u64,
    /// Number of leading digest parts (messages) that matched. Unlike the byte
    /// count this is comparable *across* arms: trim shrinks a part's bytes but
    /// never changes how many parts there are, so a drop here — and only a drop
    /// here — means the transform actually broke the prefix.
    shared_parts: usize,
    /// Byte length of each digest part of this request. Kept so the prefix-break
    /// loss can be measured WITHIN the arm: the bytes of the arm's own parts in
    /// the range `[shared_parts, other_arm.shared_parts)`. `total` is just the
    /// sum of these; the per-part list is what makes the range sum possible.
    part_lens: Vec<u32>,
    /// This request's estimated tokens.
    t: u64,
    /// Sum of digest part byte lengths.
    total: u64,
}

/// Replay one request of one arm through the modeled prefix cache.
///
/// Bucket semantics (matching the `requests` schema's three token columns):
/// - a cold **caching** request writes its whole body to the cache
///   (`cache_creation`) — the `cache_control` marker asks the provider to cache
///   it, and there is nothing to read yet;
/// - a cold or non-caching request is plain `input` (no cache interaction);
/// - a warm **caching** request reads the shared prefix (`cache_read`) and
///   writes the fresh tail beyond it (`cache_creation`). The split is
///   byte-proportional; the rounding remainder lands in `input` so the three
///   buckets sum to `T` exactly (asserted).
///
/// Returns the request's modeled cost, plus shared-prefix info only when a warm
/// caching comparison was possible — the only case where a shared-prefix number
/// is meaningful.
fn replay_request(
    run_id: i64,
    seq: i64,
    sent: Value,
    caching: bool,
    cold: bool,
    prev: &mut Option<Vec<Part>>,
    buckets: &mut ArmBuckets,
) -> (RequestCost, Option<SharedInfo>) {
    let parts = digest_anthropic(&sent);
    let total: u64 = parts.iter().map(|&(_, len)| len as u64).sum();
    let t = est_tokens(&sent);

    let warm_caching = caching && !cold && prev.is_some();
    let (shared, shared_parts) = if warm_caching {
        let prev_parts = prev.as_ref().expect("warm_caching implies a predecessor");
        let n = prev_parts
            .iter()
            .zip(parts.iter())
            .take_while(|(a, b)| a == b)
            .count();
        (shared_bytes(prev_parts, &parts), n)
    } else {
        (0, 0)
    };

    let (input, creation, read) = if warm_caching {
        // Byte-proportional cache split of T; the rounding remainder stays input.
        let read = (t * shared) / total.max(1);
        let creation = (t * total.saturating_sub(shared)) / total.max(1);
        (t - read - creation, creation, read)
    } else if caching && cold {
        // Whole body written to the cache; nothing read, nothing plain-input.
        (0, t, 0)
    } else {
        // No cache interaction at all.
        (t, 0, 0)
    };

    debug_assert_eq!(
        input + creation + read,
        t,
        "the three buckets must conserve the request's tokens exactly"
    );

    buckets.n_requests += 1;
    buckets.input_tokens += input;
    buckets.cache_creation_tokens += creation;
    buckets.cache_read_tokens += read;
    buckets.raw_tokens += t;

    // Per-part byte lengths, needed only when a warm-caching comparison might
    // later report a prefix break (see `SharedInfo`). Computed here, before
    // `parts` is moved into `prev`.
    let part_lens: Vec<u32> = if warm_caching {
        parts.iter().map(|&(_, len)| len).collect()
    } else {
        Vec::new()
    };

    *prev = Some(parts);

    let cost = RequestCost {
        run_id,
        seq,
        input_tokens: input,
        cache_creation_tokens: creation,
        cache_read_tokens: read,
        total_tokens: t,
        shared_bytes: shared,
    };

    let info = if warm_caching {
        Some(SharedInfo {
            shared_bytes: shared,
            shared_parts,
            part_lens,
            t,
            total,
        })
    } else {
        None
    };
    (cost, info)
}

/// Replay one chain on both arms, mutating both arms' buckets and collecting
/// any prefix breaks.
fn replay_chain(
    chain: &Chain,
    knobs: &NativeKnobs,
    off: &mut ArmBuckets,
    on: &mut ArmBuckets,
    breaks: &mut Vec<PrefixBreak>,
    off_costs: &mut Vec<RequestCost>,
    on_costs: &mut Vec<RequestCost>,
) {
    let mut off_prev: Option<Vec<Part>> = None;
    let mut on_prev: Option<Vec<Part>> = None;
    let mut prev_ts_ms: Option<i64> = None;

    for req in &chain.requests {
        // Cold = first in the chain, or a gap >= TTL from the chain's previous
        // request. The gap is measured within the chain — interleaved sessions
        // in the same run do not affect it.
        let cold = match prev_ts_ms {
            None => true,
            Some(prev) => req.ts_ms.saturating_sub(prev) >= TTL_MS as i64,
        };
        prev_ts_ms = Some(req.ts_ms);
        // Trim never adds or removes cache_control, so this is arm-independent
        // and computed once from the pre-trim body.
        let caching = contains_cache_control(&req.body);

        let (off_cost, off_info) = replay_request(
            req.run_id,
            req.seq,
            req.body.clone(),
            caching,
            cold,
            &mut off_prev,
            off,
        );
        let (on_cost, on_info) = replay_request(
            req.run_id,
            req.seq,
            trim_native(req.body.clone(), knobs),
            caching,
            cold,
            &mut on_prev,
            on,
        );
        off_costs.push(off_cost);
        on_costs.push(on_cost);

        if let (Some(oi), Some(ni)) = (&off_info, &on_info) {
            // A break is a drop in matched *parts*, never in matched bytes.
            // Comparing bytes across arms would flag every successful trim: the
            // ON arm's prefix is smaller in bytes precisely because trim deleted
            // some, which is the tool working, not the prefix breaking.
            if ni.shared_parts < oi.shared_parts {
                // Measure the loss WITHIN the ON arm: the bytes of the ON arm's
                // own parts over the index range [ni.shared_parts, oi.shared_parts)
                // — the parts the raw arm matched but the trimmed arm did not —
                // measured in ON-arm bytes. Do NOT subtract OFF-arm bytes from
                // ON-arm bytes: those are different scales (the ON prefix is
                // smaller partly because trim deleted content), so a cross-arm
                // difference is inflated by every byte trim saved. Converting at
                // the ON arm's own density keeps the loss bounded by the ON arm's
                // total for this request.
                let lost_bytes: u64 = ni
                    .part_lens
                    .iter()
                    .skip(ni.shared_parts)
                    .take(oi.shared_parts - ni.shared_parts)
                    .map(|&len| len as u64)
                    .sum();
                let break_tokens = (ni.t * lost_bytes) / ni.total.max(1);
                off.n_prefix_breaks += 1;
                off.prefix_break_tokens += break_tokens;
                on.n_prefix_breaks += 1;
                on.prefix_break_tokens += break_tokens;
                breaks.push(PrefixBreak {
                    run_id: req.run_id,
                    seq: req.seq,
                    shared_off: oi.shared_bytes as u32,
                    shared_on: ni.shared_bytes as u32,
                    shared_parts_off: oi.shared_parts as u32,
                    shared_parts_on: ni.shared_parts as u32,
                    tokens_lost: break_tokens,
                });
            }
        }
    }
}

/// Replay every chain on both arms. Returns `(off, on, breaks)` with the
/// breaks sorted largest-loss-first and truncated to [`MAX_WORST_BREAKS`].
fn replay_corpus(
    chains: &[Chain],
    knobs: &NativeKnobs,
) -> (ArmBuckets, ArmBuckets, Vec<PrefixBreak>) {
    let (off, on, breaks, _, _) = replay_corpus_with_costs(chains, knobs);
    (off, on, breaks)
}

/// As [`replay_corpus`], but also returns each arm's per-request modeled costs
/// (in corpus order) for calibration against the live `requests` rows.
fn replay_corpus_with_costs(
    chains: &[Chain],
    knobs: &NativeKnobs,
) -> (
    ArmBuckets,
    ArmBuckets,
    Vec<PrefixBreak>,
    Vec<RequestCost>,
    Vec<RequestCost>,
) {
    let mut off = ArmBuckets::default();
    let mut on = ArmBuckets::default();
    let mut breaks: Vec<PrefixBreak> = Vec::new();
    let mut off_costs: Vec<RequestCost> = Vec::new();
    let mut on_costs: Vec<RequestCost> = Vec::new();
    for chain in chains {
        replay_chain(
            chain,
            knobs,
            &mut off,
            &mut on,
            &mut breaks,
            &mut off_costs,
            &mut on_costs,
        );
    }
    // Sort by the within-arm loss magnitude, never by `shared_off - shared_on`
    // (a cross-arm byte subtraction on different scales — see `PrefixBreak`).
    breaks.sort_by(|a, b| b.tokens_lost.cmp(&a.tokens_lost));
    breaks.truncate(MAX_WORST_BREAKS);
    (off, on, breaks, off_costs, on_costs)
}

/// Compare the two arms' weighted costs. A relative gap within ±[`VERDICT_REL`]
/// reads as noise (the offline estimator is chars/4); beyond it the sign of
/// the gap decides.
fn compute_verdict(off: &ArmBuckets, on: &ArmBuckets) -> CacheVerdict {
    let off_cost = off.weighted_cost();
    let on_cost = on.weighted_cost();
    if off_cost <= 0.0 {
        return if on_cost > 0.0 {
            CacheVerdict::Backwards
        } else {
            CacheVerdict::Inconclusive
        };
    }
    let rel = (on_cost - off_cost) / off_cost;
    if rel <= -VERDICT_REL {
        CacheVerdict::Proven
    } else if rel >= VERDICT_REL {
        CacheVerdict::Backwards
    } else {
        CacheVerdict::Inconclusive
    }
}

// ── entry point ─────────────────────────────────────────────────────────────

/// Run the cache-weighted offline benchmark against a proxy.db.
///
/// Reads `request_bodies` (provider='anthropic'), groups rows into
/// per-conversation chains by `(run_id, session_hash)` — a `run_id` is a
/// daemon *process* run in which several unrelated Claude Code sessions
/// interleave, so grouping by `run_id` alone would falsely credit their shared
/// `system`/`tools` as cache hits — and replays each chain once per arm.
/// Read-only.
///
/// Returns `Ok(None)` when the corpus has no usable anthropic rows.
pub fn run_cache_bench(
    db_path: &Path,
    knobs: &NativeKnobs,
) -> Result<Option<CacheBenchResult>, String> {
    // The corpus lives next to proxy.db: a per-provider file when it exists,
    // else the legacy `request_bodies` table in proxy.db. Either way the
    // returned connection exposes the table as plain `request_bodies`.
    let dir = db_path.parent().unwrap_or(Path::new(""));
    let Some(conn) = crate::corpus::open_read(dir, "anthropic") else {
        return Ok(None);
    };

    let chains = load_chains(&conn, "main")?;
    if chains.is_empty() {
        return Ok(None);
    }
    let n_chains = chains.len();

    let (off, on, worst_breaks) = replay_corpus(&chains, knobs);
    let verdict = compute_verdict(&off, &on);

    Ok(Some(CacheBenchResult {
        off,
        on,
        n_chains,
        verdict,
        worst_breaks,
    }))
}

/// Read every anthropic body and group it into per-conversation chains.
///
/// Chains are keyed by `(run_id, session_hash)`, never by `run_id` alone:
/// `run_id` identifies the daemon *process* run, inside which unrelated client
/// sessions interleave. Grouping by `run_id` alone would let two strangers'
/// identical `system`/`tools` count as a cache hit.
pub(crate) fn load_chains(conn: &Connection, schema: &str) -> Result<Vec<Chain>, String> {
    // `schema` is always one of the corpus module's own fixed literals —
    // `"main"` when the connection came from `corpus::open_read`, or
    // `"corpus"`/`"main"` when it came from `corpus::attach_read` — never
    // caller input, so formatting it into the SQL is safe. Do not "fix" this
    // into a parameterized query: the qualifier is not a value.
    let mut stmt = conn
        .prepare(&format!(
            "SELECT run_id, seq, ts, body FROM {schema}.request_bodies \
             WHERE provider='anthropic' ORDER BY run_id, seq",
        ))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut requests: Vec<ChainRequest> = Vec::new();
    for row in rows.flatten() {
        let (run_id, seq, ts, blob) = row;
        let Some(ts_ms) = parse_ts(&ts) else {
            continue;
        };
        if let Some(s) = decompress_body(&blob)
            && let Ok(body) = serde_json::from_str::<Value>(&s)
        {
            requests.push(ChainRequest {
                run_id,
                seq,
                ts_ms,
                body,
            });
        }
    }

    let mut by_chain: HashMap<(i64, u64), Vec<ChainRequest>> = HashMap::new();
    for req in requests {
        by_chain
            .entry((req.run_id, session_hash(&req.body)))
            .or_default()
            .push(req);
    }
    Ok(by_chain
        .into_values()
        .map(|mut v| {
            v.sort_by_key(|r| r.seq);
            Chain { requests: v }
        })
        .collect())
}

// ── calibration ─────────────────────────────────────────────────────────────

/// One live request paired with the modeled cost of the arm that actually ran.
#[derive(Debug, Clone)]
pub struct CalibrationRow {
    pub run_id: i64,
    pub seq: i64,
    /// Which arm the live traffic corresponds to: live requests went through
    /// trim when this is true, so they must be compared to the ON arm.
    pub trim_applied: bool,
    pub model_total: u64,
    pub live_total: u64,
    pub model_read: u64,
    pub live_read: u64,
    pub model_creation: u64,
    pub live_creation: u64,
    pub model_shared_bytes: u64,
    pub live_shared_chars: Option<u64>,
}

/// Summary of a ratio distribution. The median is the headline: token counts
/// are heavy-tailed and one outsized request would dominate a mean.
#[derive(Debug, Clone)]
pub struct RatioStats {
    pub n: usize,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
    pub within_2x: f64,
    pub within_10x: f64,
}

impl RatioStats {
    /// A ratio distribution is calibrated when the typical row is within 2x and
    /// the tail does not run away past 10x.
    pub fn is_calibrated(&self) -> bool {
        self.n > 0 && (0.5..=2.0).contains(&self.median) && self.within_10x >= 0.80
    }
}

pub struct CalibrationReport {
    pub rows: Vec<CalibrationRow>,
    /// Live rows whose cache columns were NULL (upstream reported no such
    /// field) — not comparable, so excluded rather than counted as zero.
    pub n_skipped_null: usize,
    /// Replayed bodies with no matching `requests` row.
    pub n_unmatched: usize,
    pub n_arm_on: usize,
    pub n_arm_off: usize,
    /// Distinct `trim_config` values among matched rows. More than one means the
    /// corpus mixes knob settings while the replay used a single set — reported
    /// rather than silently averaged away.
    pub n_distinct_trim_config: usize,
}

/// Median, p10, p90 and within-factor fractions. Returns `None` for an empty
/// input rather than inventing a value.
pub fn ratio_stats(ratios: &[f64]) -> Option<RatioStats> {
    if ratios.is_empty() {
        return None;
    }
    let mut v = ratios.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |p: f64| -> f64 {
        let idx = (((v.len() - 1) as f64) * p).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    let frac = |factor: f64| -> f64 {
        let lo = 1.0 / factor;
        v.iter().filter(|&&r| r >= lo && r <= factor).count() as f64 / v.len() as f64
    };
    Some(RatioStats {
        n: v.len(),
        median: pick(0.5),
        p10: pick(0.10),
        p90: pick(0.90),
        within_2x: frac(2.0),
        within_10x: frac(10.0),
    })
}

impl CalibrationReport {
    /// Ratio of real tokens to `chars/4` — calibrates the token estimator.
    pub fn total_ratio(&self) -> Option<RatioStats> {
        self.ratios(|r| (r.live_total, r.model_total))
    }
    pub fn read_ratio(&self) -> Option<RatioStats> {
        self.ratios(|r| (r.live_read, r.model_read))
    }
    pub fn creation_ratio(&self) -> Option<RatioStats> {
        self.ratios(|r| (r.live_creation, r.model_creation))
    }
    /// The most direct check of the prefix model itself.
    pub fn shared_ratio(&self) -> Option<RatioStats> {
        let v: Vec<f64> = self
            .rows
            .iter()
            .filter_map(|r| {
                let live = r.live_shared_chars?;
                (r.model_shared_bytes > 0).then(|| live as f64 / r.model_shared_bytes as f64)
            })
            .collect();
        ratio_stats(&v)
    }

    /// Rows where the denominator is zero carry no information and are dropped.
    fn ratios(&self, f: impl Fn(&CalibrationRow) -> (u64, u64)) -> Option<RatioStats> {
        let v: Vec<f64> = self
            .rows
            .iter()
            .filter_map(|r| {
                let (live, model) = f(r);
                (model > 0).then(|| live as f64 / model as f64)
            })
            .collect();
        ratio_stats(&v)
    }
}

/// A live `requests` row, reduced to the fields the calibration compares.
struct LiveRow {
    trim_applied: bool,
    trim_config: Option<String>,
    input: u64,
    read: Option<u64>,
    creation: Option<u64>,
    shared_chars: Option<u64>,
}

/// Replay the corpus and compare each request against what the provider
/// actually billed.
///
/// Until this passes, the bench's verdicts are a plausible model rather than
/// measured fact.
pub fn run_calibration(
    db_path: &Path,
    knobs: &NativeKnobs,
) -> Result<Option<CalibrationReport>, String> {
    let dir = db_path.parent().unwrap_or(Path::new(""));
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;

    // `requests` stays in proxy.db; the bodies may now live in a per-provider
    // file attached as the `corpus` schema (or fall back to the legacy table
    // in this same database). `None` means no corpus rows for this provider.
    let Some(schema) = crate::corpus::attach_read(&conn, dir, "anthropic") else {
        return Ok(None);
    };

    let chains = load_chains(&conn, schema)?;
    if chains.is_empty() {
        return Ok(None);
    }
    let (_, _, _, off_costs, on_costs) = replay_corpus_with_costs(&chains, knobs);

    let mut stmt = conn
        .prepare(
            "SELECT run_id, seq, trim_applied, trim_config, input_tokens, \
             cache_read_tokens, cache_creation_tokens, prefix_shared_chars_sent \
             FROM main.requests",
        )
        .map_err(|e| e.to_string())?;
    let live_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                LiveRow {
                    trim_applied: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
                    trim_config: row.get::<_, Option<String>>(3)?,
                    input: row.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                    read: row.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as u64),
                    creation: row.get::<_, Option<i64>>(6)?.map(|v| v.max(0) as u64),
                    shared_chars: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
                },
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut live: HashMap<(i64, i64), LiveRow> = HashMap::new();
    for (run_id, seq, r) in live_rows.flatten() {
        live.insert((run_id, seq), r);
    }

    let mut rows = Vec::new();
    let mut n_skipped_null = 0usize;
    let mut n_unmatched = 0usize;
    let mut n_arm_on = 0usize;
    let mut n_arm_off = 0usize;
    let mut configs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (off_cost, on_cost) in off_costs.iter().zip(on_costs.iter()) {
        let key = (off_cost.run_id, off_cost.seq);
        let Some(l) = live.get(&key) else {
            n_unmatched += 1;
            continue;
        };
        let (Some(live_read), Some(live_creation)) = (l.read, l.creation) else {
            n_skipped_null += 1;
            continue;
        };
        // Compare against the arm that actually ran, not a fixed one: pairing
        // trimmed live traffic with the untrimmed arm would manufacture a
        // discrepancy that says nothing about the model.
        let model = if l.trim_applied {
            n_arm_on += 1;
            on_cost
        } else {
            n_arm_off += 1;
            off_cost
        };
        if let Some(cfg) = &l.trim_config {
            configs.insert(cfg.clone());
        }
        rows.push(CalibrationRow {
            run_id: off_cost.run_id,
            seq: off_cost.seq,
            trim_applied: l.trim_applied,
            model_total: model.total_tokens,
            live_total: l.input + live_read + live_creation,
            model_read: model.cache_read_tokens,
            live_read,
            model_creation: model.cache_creation_tokens,
            live_creation,
            model_shared_bytes: model.shared_bytes,
            live_shared_chars: l.shared_chars,
        });
    }

    Ok(Some(CalibrationReport {
        rows,
        n_skipped_null,
        n_unmatched,
        n_arm_on,
        n_arm_off,
        n_distinct_trim_config: configs.len(),
    }))
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── helpers ─────────────────────────────────────────────────────────────

    fn req(run_id: i64, seq: i64, ts_ms: i64, body: Value) -> ChainRequest {
        ChainRequest {
            run_id,
            seq,
            ts_ms,
            body,
        }
    }

    fn chain(requests: Vec<ChainRequest>) -> Chain {
        Chain { requests }
    }

    /// A small caching Anthropic body: system array with an ephemeral
    /// cache_control marker, three turns, no content trim could shrink.
    fn caching_body() -> Value {
        json!({
            "model": "claude-sonnet-4-6",
            "system": [{"type": "text", "text": "You are a helpful assistant.",
                        "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": "Summarize the repo."},
                {"role": "assistant", "content": "Here is a summary."},
                {"role": "user", "content": "Now focus on the tests."}
            ]
        })
    }

    /// Same body without the cache_control marker — no cache interaction.
    fn non_caching_body() -> Value {
        let mut v = caching_body();
        if let Some(sys) = v.get_mut("system")
            && let Some(arr) = sys.as_array_mut()
        {
            for item in arr.iter_mut() {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        v
    }

    /// A two-request chain whose ON arm's prefix breaks on the second request:
    /// request 1's only assistant message carries a thinking block (kept, since
    /// it is the last assistant), while request 2 appends a new turn so the
    /// same message is no longer last and `strip_thinking` removes the block.
    /// The raw bodies share a long prefix; the trimmed bodies diverge at the
    /// second message, which is exactly the signature this tool exists to show.
    fn strip_thinking_break_chain() -> Vec<Chain> {
        let sys = json!([{"type": "text", "text": "SYS",
                          "cache_control": {"type": "ephemeral"}}]);
        let mut msgs: Vec<Value> = Vec::new();
        msgs.push(json!({"role": "user", "content": "FIRST USER"}));
        msgs.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "T".repeat(400)},
            {"type": "text", "text": "I'll get started."}
        ]}));
        // The long, stable conversation tail: big user messages only, so the
        // thinking assistant stays the last assistant in request 1.
        for i in 0..20 {
            msgs.push(json!({"role": "user",
                "content": format!("turn {i} result ") + &"X".repeat(400)}));
        }
        let body1 = json!({"system": sys.clone(), "messages": msgs.clone()});
        let mut msgs2 = msgs.clone();
        msgs2.push(json!({"role": "assistant", "content": "wrapping up"}));
        msgs2.push(json!({"role": "user", "content": "final turn"}));
        let body2 = json!({"system": sys, "messages": msgs2});
        vec![chain(vec![req(7, 1, 0, body1), req(7, 2, 1000, body2)])]
    }

    // ── bucket conservation ─────────────────────────────────────────────────

    #[test]
    fn bucket_conservation_multi_request_chain() {
        let chains = strip_thinking_break_chain();
        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let (off, on, _) = replay_corpus(&chains, &knobs);
        for b in [&off, &on] {
            assert_eq!(
                b.input_tokens + b.cache_creation_tokens + b.cache_read_tokens,
                b.raw_tokens,
                "the three buckets must exactly sum to the raw token count"
            );
        }
    }

    // ── cold starts ─────────────────────────────────────────────────────────

    #[test]
    fn cold_start_without_cache_control_is_plain_input() {
        let body = non_caching_body();
        let chains = vec![chain(vec![req(1, 1, 0, body)])];
        let (off, on, _) = replay_corpus(&chains, &NativeKnobs::default());
        for b in [&off, &on] {
            assert_eq!(b.n_requests, 1);
            assert_eq!(b.input_tokens, b.raw_tokens);
            assert_eq!(b.cache_creation_tokens, 0);
            assert_eq!(b.cache_read_tokens, 0);
        }
    }

    #[test]
    fn cold_start_with_cache_control_writes_the_whole_body() {
        let body = caching_body();
        let chains = vec![chain(vec![req(1, 1, 0, body)])];
        let (off, on, _) = replay_corpus(&chains, &NativeKnobs::default());
        for b in [&off, &on] {
            assert_eq!(b.n_requests, 1);
            assert_eq!(b.cache_creation_tokens, b.raw_tokens);
            assert_eq!(b.input_tokens, 0);
            assert_eq!(b.cache_read_tokens, 0);
        }
    }

    // ── warm repeats ────────────────────────────────────────────────────────

    #[test]
    fn identical_body_repeated_lands_in_cache_read() {
        let body = caching_body();
        let t = est_tokens(&body);
        let chains = vec![chain(vec![
            req(1, 1, 0, body.clone()),
            req(1, 2, 1000, body),
        ])];
        let (off, on, _) = replay_corpus(&chains, &NativeKnobs::default());
        for b in [&off, &on] {
            assert_eq!(b.n_requests, 2);
            assert_eq!(b.raw_tokens, 2 * t);
            assert_eq!(b.cache_creation_tokens, t, "first request writes the body");
            assert_eq!(
                b.cache_read_tokens, t,
                "second identical request reads it all back"
            );
            assert_eq!(b.input_tokens, 0);
        }
    }

    // ── prefix breaks ───────────────────────────────────────────────────────

    #[test]
    fn prefix_break_is_detected() {
        let chains = strip_thinking_break_chain();
        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let (off, on, breaks) = replay_corpus(&chains, &knobs);
        assert!(
            off.n_prefix_breaks >= 1,
            "expected at least one prefix break on the OFF arm"
        );
        assert_eq!(off.n_prefix_breaks, on.n_prefix_breaks);
        assert!(
            !breaks.is_empty(),
            "worst-breaks list must include the break"
        );
        let b = &breaks[0];
        assert_eq!(b.run_id, 7);
        assert_eq!(b.seq, 2);
        assert!(
            b.shared_on < b.shared_off,
            "break must show a shorter shared prefix on the ON arm"
        );
    }

    #[test]
    fn successful_trim_shrinks_bodies_but_reports_no_loss() {
        // Both requests carry the same large, compressible tool_result, so trim
        // shrinks the bodies substantially while the prefix still matches on
        // every part. A loss metric built on a cross-arm byte subtraction would
        // count every saved byte as a "break"; the within-arm basis must not.
        let sys = json!([{"type": "text", "text": "SYS",
                          "cache_control": {"type": "ephemeral"}}]);
        let body = json!({
            "system": sys,
            "messages": [
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_big",
                    "content": "A".repeat(20_000)
                }]},
                {"role": "assistant", "content": "ok"}
            ]
        });
        let chains = vec![chain(vec![
            req(5, 1, 0, body.clone()),
            req(5, 2, 1000, body),
        ])];
        let knobs = NativeKnobs {
            tool_max_desc_chars: usize::MAX,
            tool_result_head: 100,
            tool_result_tail: 50,
            tool_result_min_elide: 1000,
            ..NativeKnobs::default()
        };
        let (off, on, breaks) = replay_corpus(&chains, &knobs);
        // Premise: trim actually shrank the bodies by a large margin, so the
        // assertion below is about substantial trimming, not a no-op case.
        assert!(
            off.raw_tokens > on.raw_tokens * 10,
            "trim must shrink the bodies substantially for this guard to mean \
             anything (off {} vs on {})",
            off.raw_tokens,
            on.raw_tokens
        );
        assert!(on.cache_read_tokens > 0, "the ON arm still shared a prefix");
        // Successful trimming must never be reported as a break or a loss.
        for b in [&off, &on] {
            assert_eq!(b.n_prefix_breaks, 0, "no break from successful trim");
            assert_eq!(b.prefix_break_tokens, 0);
        }
        assert!(breaks.is_empty());
    }

    #[test]
    fn prefix_break_loss_is_bounded_by_on_arm_total_tokens() {
        // A genuine break: appending a turn makes the earlier assistant's big
        // thinking block eligible for stripping on request 2 only, so the ON
        // prefix breaks while the OFF prefix holds. The reported loss is
        // measured within the ON arm, so it can never exceed the ON arm's own
        // total tokens for that request — a cross-arm byte subtraction could
        // report a loss larger than the trimmed body it happened to.
        let sys = json!([{"type": "text", "text": "SYS",
                          "cache_control": {"type": "ephemeral"}}]);
        let mut msgs: Vec<Value> = vec![json!({"role": "user", "content": "FIRST USER"})];
        msgs.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "T".repeat(6000)},
            {"type": "text", "text": "I'll get started."}
        ]}));
        for i in 0..5 {
            msgs.push(json!({"role": "user",
                "content": format!("turn {i} result ") + &"X".repeat(400)}));
        }
        let body1 = json!({"system": sys.clone(), "messages": msgs.clone()});
        let mut msgs2 = msgs.clone();
        msgs2.push(json!({"role": "assistant", "content": "wrapping up"}));
        msgs2.push(json!({"role": "user", "content": "final turn"}));
        let body2 = json!({"system": sys, "messages": msgs2});

        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let chains = vec![chain(vec![
            req(9, 1, 0, body1),
            req(9, 2, 1000, body2.clone()),
        ])];
        let (off, on, breaks) = replay_corpus(&chains, &knobs);
        assert_eq!(off.n_prefix_breaks, on.n_prefix_breaks);
        assert!(off.n_prefix_breaks >= 1, "expected a genuine prefix break");
        let b = &breaks[0];
        assert_eq!(b.run_id, 9);
        assert_eq!(b.seq, 2);
        assert!(
            b.shared_parts_on < b.shared_parts_off,
            "a break is a drop in matched parts, not in bytes"
        );
        assert!(b.tokens_lost > 0, "a genuine break must report some loss");
        // The ON arm's own total tokens for this request bound the reported loss.
        let on_total_tokens = est_tokens(&trim_native(body2, &knobs));
        assert!(
            b.tokens_lost <= on_total_tokens,
            "within-arm loss must not exceed the ON arm's own request total \
             (tokens_lost {} > on total {})",
            b.tokens_lost,
            on_total_tokens
        );
    }

    #[test]
    fn raw_tokens_fall_but_weighted_cost_rises() {
        let chains = strip_thinking_break_chain();
        let knobs = NativeKnobs {
            strip_thinking: true,
            ..NativeKnobs::default()
        };
        let (off, on, _) = replay_corpus(&chains, &knobs);
        assert!(
            off.raw_tokens > on.raw_tokens,
            "raw tokens must fall (thinking is stripped on the second request)"
        );
        assert!(
            on.weighted_cost() > off.weighted_cost(),
            "weighted cost must RISE even though raw tokens fall — this is the \
             signature of a prefix break moving tokens from the 0.1x cache-read \
             bucket into the 1.0x input bucket"
        );
    }

    // ── degenerate / structural cases ───────────────────────────────────────

    #[test]
    fn light_profile_that_cannot_shrink_is_arm_neutral() {
        let body = caching_body();
        let knobs = NativeKnobs::light();
        // Confirm the degenerate-case premise: light() leaves this body alone.
        assert_eq!(trim_native(body.clone(), &knobs), body);
        let chains = vec![chain(vec![
            req(2, 1, 0, body.clone()),
            req(2, 2, 1000, body),
        ])];
        let (off, on, breaks) = replay_corpus(&chains, &knobs);
        assert_eq!(
            off.raw_tokens, on.raw_tokens,
            "byte-identical arms must send identical tokens"
        );
        assert_eq!(off.weighted_cost(), on.weighted_cost());
        assert!(breaks.is_empty());
    }

    #[test]
    fn single_request_chain_is_just_a_cold_start() {
        let body = caching_body();
        let chains = vec![chain(vec![req(1, 1, 0, body)])];
        let (off, on, _) = replay_corpus(&chains, &NativeKnobs::default());
        assert_eq!(off.n_requests, 1);
        assert_eq!(on.n_requests, 1);
        assert_eq!(off.cache_creation_tokens, off.raw_tokens);
        assert_eq!(on.cache_creation_tokens, on.raw_tokens);
    }

    #[test]
    fn six_minute_gap_breaks_the_chain_into_a_cold_start() {
        // TTL is 5 minutes; a 6-minute gap must reset the cache.
        let body = caching_body();
        let t = est_tokens(&body);
        let chains = vec![chain(vec![
            req(3, 1, 0, body.clone()),
            req(3, 2, 60_000, body.clone()), // 1 minute later — warm
            req(3, 3, 60_000 + 360_000, body), // +6 minutes — cold again
        ])];
        let (off, on, _) = replay_corpus(&chains, &NativeKnobs::default());
        for b in [&off, &on] {
            assert_eq!(b.n_requests, 3);
            // req1 writes, req2 reads it back, req3 writes again (cold).
            assert_eq!(b.cache_creation_tokens, 2 * t);
            assert_eq!(b.cache_read_tokens, t);
            assert_eq!(b.input_tokens, 0);
            assert_eq!(b.raw_tokens, 3 * t);
        }
    }

    // ── timestamp parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_ts_reference_value() {
        // 2026-07-28 17:45:55.581 UTC = 1785260755581 ms.
        assert_eq!(parse_ts("2026-07-28 17:45:55.581"), Some(1785260755581));
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        assert_eq!(parse_ts("2026-07-28"), None);
        assert_eq!(parse_ts("garbage"), None);
        assert_eq!(parse_ts("2026-07-28 17:45:55"), None);
        assert_eq!(
            parse_ts("2026-13-01 00:00:00.000"),
            None,
            "month 13 is invalid"
        );
        assert_eq!(
            parse_ts("2026-07-32 00:00:00.000"),
            None,
            "day 32 is invalid"
        );
    }

    // ── session grouping ────────────────────────────────────────────────────

    #[test]
    fn session_hash_groups_same_conversation_ignores_new_turns() {
        let sys = json!([{"type": "text", "text": "SYS"}]);
        let mk = |n_turns: usize| {
            let mut msgs: Vec<Value> = vec![json!({"role": "user", "content": "FIRST"})];
            for i in 0..n_turns {
                msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
                msgs.push(json!({"role": "user", "content": format!("u{i}")}));
            }
            json!({"system": sys.clone(), "messages": msgs})
        };
        let a = mk(3);
        let b = mk(7);
        assert_eq!(
            session_hash(&a),
            session_hash(&b),
            "same system + first user must hash equal across turn counts"
        );
        let c = json!({"system": sys, "messages": [
            {"role": "user", "content": "A DIFFERENT first message"}
        ]});
        assert_ne!(
            session_hash(&a),
            session_hash(&c),
            "a different first user must hash differently"
        );
    }

    // ── ratio_stats / calibration ─────────────────────────────────────────

    #[test]
    fn ratio_stats_empty_is_none() {
        assert!(ratio_stats(&[]).is_none());
    }

    #[test]
    fn ratio_stats_single_element() {
        let s = ratio_stats(&[2.5]).expect("a single element is a valid input");
        assert_eq!(s.n, 1);
        assert_eq!(s.median, 2.5);
        assert_eq!(s.p10, 2.5);
        assert_eq!(s.p90, 2.5);
        assert_eq!(s.within_2x, 0.0, "2.5 sits outside the [0.5, 2.0] band");
        assert_eq!(s.within_10x, 1.0);
        assert!(
            !s.is_calibrated(),
            "median 2.5 is outside the calibrated band, even with nothing else in the tail"
        );
    }

    #[test]
    fn ratio_stats_known_odd_length() {
        // Percentile index is round((n - 1) * p); with n = 5 that maps 0.1, 0.5,
        // 0.9 onto sorted indexes 0, 2, 4.
        let s = ratio_stats(&[5.0, 3.0, 1.0, 4.0, 2.0]).expect("non-empty input");
        assert_eq!(s.n, 5);
        assert_eq!(s.median, 3.0);
        assert_eq!(s.p10, 1.0);
        assert_eq!(s.p90, 5.0);
        assert_eq!(s.within_2x, 2.0 / 5.0, "only 1.0 and 2.0 sit in [0.5, 2.0]");
        assert_eq!(s.within_10x, 1.0);
    }

    #[test]
    fn ratio_stats_outlier_does_not_move_median() {
        // One 1000x outlier must not drag the median up — that is precisely the
        // property the median was chosen for over the mean. The tail still shows
        // up at p90, which is where it belongs.
        let s = ratio_stats(&[1.0, 1.0, 1.0, 1000.0]).expect("non-empty input");
        assert_eq!(s.median, 1.0);
        assert_eq!(s.p10, 1.0);
        assert_eq!(s.p90, 1000.0);
        assert_eq!(s.within_2x, 0.75);
        assert_eq!(
            s.within_10x, 0.75,
            "the 1000x outlier is outside even the 10x band"
        );
    }

    #[test]
    fn ratio_stats_within_band_fractions() {
        // The band is two-sided: within factor F means the ratio lands in
        // [1/F, F]. So 0.3 is outside 2x (needs >= 0.5) but inside 10x
        // (needs >= 0.1); 9.0 is inside 10x but outside 2x; 11.0 is outside both.
        let s = ratio_stats(&[0.3, 1.0, 1.0, 2.0, 9.0, 11.0]).expect("non-empty input");
        assert_eq!(s.within_2x, 3.0 / 6.0, "the three 1.0/2.0 values");
        assert_eq!(s.within_10x, 5.0 / 6.0, "everything except the 11.0");
    }

    #[test]
    fn ratio_stats_is_calibrated_cases() {
        // Calibrated: median in [0.5, 2.0] and at least 80% of rows within 10x.
        let ok = ratio_stats(&[0.9, 1.0, 1.1, 1.5]).expect("non-empty input");
        assert!(ok.is_calibrated());

        // Uncalibrated: the tail runs away — the 1000x outlier drops within_10x
        // below the 80% bar even though the median is perfect.
        let runaway = ratio_stats(&[1.0, 1.0, 1.0, 1000.0]).expect("non-empty input");
        assert!(!runaway.is_calibrated());

        // Uncalibrated: the median itself sits outside the band.
        let off_median = ratio_stats(&[10.0]).expect("non-empty input");
        assert!(!off_median.is_calibrated());
    }
}
