//! Read-lifecycle obsolescence probe — a MEASUREMENT-ONLY estimate, not a
//! shipped behavior and not billing data.
//!
//! We are considering a "Read lifecycle" trim lever: once a `Read` tool_result
//! is known to be obsolete, replace its body with a short marker. Two
//! obsolescence kinds exist:
//!
//! - **SUPERSEDED** — the same file is read again later in the conversation;
//! - **STALE** — an `Edit`/`Write`/`NotebookEdit` on that file happens later.
//!
//! The concern this tool measures is *affordability*: the classification
//! depends on what comes LATER, so when a read flips to obsolete, the bytes of
//! an ALREADY-SENT message change, breaking the provider prompt cache from that
//! point onward. Unlike `strip_thinking` (which breaks near the tail, costing
//! 1-2 messages), a flip can land DEEP in the history, forcing re-creation of
//! everything after it. Few breaks at a high price may be worse than many at a
//! low price.
//!
//! # Model and its simplifications (read before trusting the output)
//!
//! - **Chars/4, not billing.** Token counts are `chars/4` over serialized JSON,
//!   the same estimator as `cache_bench`. We report byte counts directly and
//!   weight them with the Anthropic quota multipliers reused from
//!   [`crate::cache_bench`] (`W_CACHE_CREATION` = 1.25, `W_CACHE_READ` = 0.10).
//! - **Whole-file reads.** `offset`/`limit` ranges are IGNORED: any `Read` of a
//!   file is treated as covering the whole file, so a later read of any range
//!   of the same file supersedes it.
//! - **Single final request per chain.** We analyze only the LAST request body
//!   of each chain (it contains the full conversation snapshot), exactly as the
//!   probe spec demands. All flips are detected simultaneously on that body.
//! - **Invalidation excludes the flipped message itself.** "Bytes after index
//!   `i`" counts messages strictly after the flipped read; the flipped message's
//!   own remaining bytes are not counted. That UNDERSTATES cost, so it is a
//!   conservative choice for the affordability question.
//! - **Deduped per chain.** If several flips occur in one chain, the earliest
//!   (smallest message index) breaks the prefix and therefore already
//!   invalidates everything after it; a later flip inside that region costs
//!   nothing extra. Per chain the invalidation is the bytes after the EARLIEST
//!   flip index; later flips are marked `dominated` and add 0 to the deduped
//!   total. Summing each flip's raw "bytes after" naively would inflate the
//!   cost side dramatically.
//! - **Both headline sides are gross.** Cost charges invalidated bytes at the
//!   full 1.25 cache-creation weight; benefit credits reclaimed bytes at the
//!   0.10 cache-read weight they would otherwise have cost (the right per-byte
//!   saving — those bytes would have been cache-read in an intact region).
//!   These are gross magnitudes, not the net (1.25 − 0.10) marginal comparison
//!   a real lever would face on the invalidated side.

use std::path::Path;

use serde_json::Value;

use crate::cache_bench::{Chain, load_chains, session_hash};
use crate::native_trim::build_id_to_tool_info;
use crate::prefix::digest_anthropic;

// ── types ─────────────────────────────────────────────────────────────────────

/// Obsolescence class of a `Read` tool_result, decided by what happens LATER
/// in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obsolescence {
    /// No later `Read`/`Edit`/`Write`/`NotebookEdit` touches the same file.
    Live,
    /// The same file is read again later in the conversation.
    Superseded,
    /// The file is edited/written later in the conversation.
    Stale,
}

/// One `Read` tool_result in a chain's final request body.
#[derive(Debug, Clone)]
pub struct ReadInstance {
    pub message_index: usize,
    /// Normalized `file_path` of the originating tool (separator-normalized,
    /// same convention as `native_trim::ToolUseInfo::file_path`).
    pub file_path: String,
    /// Serialized JSON byte length of the `content` a marker would replace.
    pub content_bytes: u64,
    pub obsolescence: Obsolescence,
    /// How many messages remain after `message_index` (`n_messages - 1 - i`).
    pub depth: usize,
    /// Serialized byte length of all messages strictly after `message_index`
    /// (digest part lengths, matching `cache_bench`'s byte accounting).
    pub bytes_after: u64,
}

/// A Read that flipped (is obsolete): the flip candidate the lever would act on.
#[derive(Debug, Clone)]
pub struct Flip {
    pub chain_key: String,
    pub run_id: i64,
    pub session_hash: u64,
    pub message_index: usize,
    /// Read bytes reclaimed by replacing the body with a marker.
    pub content_bytes: u64,
    pub obsolescence: Obsolescence,
    /// Messages remaining after the flip index (the flip depth; shallow =
    /// cheap, deep = expensive).
    pub depth: usize,
    /// Serialized bytes of messages strictly after the flip index. This is the
    /// RAW invalidation if this flip were the chain's only one; it is NOT the
    /// deduped contribution when [`Flip::dominated`] is true.
    pub bytes_after: u64,
    /// True when an earlier flip in the same chain already breaks the prefix
    /// before this one, so this flip adds no extra invalidation to the
    /// deduped total.
    pub dominated: bool,
}

/// Per-chain analysis of the final request body.
#[derive(Debug, Clone, Default)]
pub struct ChainAnalysis {
    pub run_id: i64,
    pub session_hash: u64,
    pub n_messages: usize,
    pub reads: Vec<ReadInstance>,
    pub flips: Vec<Flip>,
    pub read_bytes_total: u64,
    pub superseded_bytes: u64,
    pub stale_bytes: u64,
    pub live_bytes: u64,
    pub reclaimed_bytes: u64,
    /// Deduped invalidation for this chain: bytes after the EARLIEST flip
    /// index (0 when no flips). Later flips inside the broken region add 0.
    pub invalidated_deduped: u64,
}

/// Aggregates over every chain's final request body.
#[derive(Debug, Clone, Default)]
pub struct ReadLifecycleReport {
    pub n_chains: usize,
    pub n_chains_with_reads: usize,
    pub n_chains_with_flips: usize,
    pub n_reads: usize,
    pub n_live_reads: usize,
    pub read_bytes_total: u64,
    pub superseded_bytes: u64,
    pub stale_bytes: u64,
    pub live_bytes: u64,
    pub n_flips: usize,
    pub n_superseded_flips: usize,
    pub n_stale_flips: usize,
    pub reclaimed_bytes: u64,
    /// Naive (undeduped) sum of every flip's `bytes_after`. Kept only to show
    /// how much the per-chain dedup prevents.
    pub invalidated_naive_bytes: u64,
    /// Deduped total: per chain, bytes after the earliest flip index, summed.
    pub invalidated_deduped_bytes: u64,
    /// Flip depths (messages remaining after each flip), for the distribution.
    pub depths: Vec<usize>,
    /// Every flip, sorted by raw `bytes_after` descending for the top-N table.
    pub flips: Vec<Flip>,
}

/// Min / p25 / median / p75 / max of a depth distribution.
#[derive(Debug, Clone, Copy)]
pub struct DepthStats {
    pub n: usize,
    pub min: usize,
    pub p25: usize,
    pub median: usize,
    pub p75: usize,
    pub max: usize,
}

// ── analysis ──────────────────────────────────────────────────────────────────

/// Analyze one chain's FINAL request body: classify every Read tool_result and
/// compute the flips plus the per-chain deduped invalidation.
///
/// `body` must be a complete Messages payload; the chain's last body is a full
/// conversation snapshot, which is why the probe uses it.
pub fn analyze_chain(run_id: i64, session_hash: u64, body: &Value) -> ChainAnalysis {
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.clone())
        .unwrap_or_default();
    let n = messages.len();

    // tool_use_id -> (name, normalized file_path) for the whole conversation.
    // The tool_use that produced a tool_result always precedes it, but the map
    // is built over all messages so order does not matter here.
    let id_to_info = build_id_to_tool_info(&messages);

    // Per-message tool_use events on named files: (name, normalized path).
    // Only the tools that can obsolete a read are kept; a read's own producing
    // tool_use lives at index i-1, strictly before the scans below, so it can
    // never self-trigger.
    let mut events: Vec<Vec<(String, String)>> = vec![Vec::new(); n];
    for (idx, msg) in messages.iter().enumerate() {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(name, "Read" | "Edit" | "Write" | "NotebookEdit") {
                continue;
            }
            let Some(input) = b.get("input").and_then(|v| v.as_object()) else {
                continue;
            };
            let Some(path) = input
                .get("file_path")
                .or_else(|| input.get("path"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            events[idx].push((name.to_string(), normalize_path(path)));
        }
    }

    // Byte accounting via the digest part lengths: `digest_anthropic` returns
    // [system, tools, msg0, msg1, ...], so message i is part i+2 and the bytes
    // of messages strictly after message i are the part lengths at i+3 onward.
    let parts = digest_anthropic(body);
    let bytes_after_index =
        |i: usize| -> u64 { parts.iter().skip(i + 3).map(|&(_, len)| len as u64).sum() };

    let mut reads: Vec<ReadInstance> = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = b.get("tool_use_id").and_then(|v| v.as_str()) else {
                continue;
            };
            // Provenance: match this tool_result back to the tool that produced
            // it. Keep only Read tool_results that carry a file path.
            let Some(info) = id_to_info.get(tool_use_id) else {
                continue;
            };
            if info.name != "Read" {
                continue;
            }
            let Some(fp) = &info.file_path else { continue };

            let obsolescence = classify_read(idx, n, fp, &events);
            reads.push(ReadInstance {
                message_index: idx,
                file_path: fp.clone(),
                content_bytes: tool_result_content_bytes(b),
                obsolescence,
                depth: n.saturating_sub(1).saturating_sub(idx),
                bytes_after: bytes_after_index(idx),
            });
        }
    }

    // Aggregate read bytes by class and collect the flip candidates.
    let mut read_bytes_total = 0u64;
    let mut superseded_bytes = 0u64;
    let mut stale_bytes = 0u64;
    let mut live_bytes = 0u64;
    let mut reclaimed_bytes = 0u64;
    let mut flips: Vec<Flip> = Vec::new();
    for r in &reads {
        read_bytes_total += r.content_bytes;
        match r.obsolescence {
            Obsolescence::Superseded => superseded_bytes += r.content_bytes,
            Obsolescence::Stale => stale_bytes += r.content_bytes,
            Obsolescence::Live => live_bytes += r.content_bytes,
        }
        if r.obsolescence != Obsolescence::Live {
            reclaimed_bytes += r.content_bytes;
            flips.push(Flip {
                chain_key: format!("run={run_id} sess={session_hash:016x}"),
                run_id,
                session_hash,
                message_index: r.message_index,
                content_bytes: r.content_bytes,
                obsolescence: r.obsolescence,
                depth: r.depth,
                bytes_after: r.bytes_after,
                dominated: false,
            });
        }
    }

    // Per-chain dedup: the earliest flip index dominates the break. An earlier
    // break already invalidates everything after it, so a later flip inside
    // that region costs nothing extra. Naively summing each flip's `bytes_after`
    // would count the same tail multiple times and inflate the cost side
    // dramatically.
    let min_flip_index = flips.iter().map(|f| f.message_index).min();
    let invalidated_deduped = match min_flip_index {
        Some(i0) => bytes_after_index(i0),
        None => 0,
    };
    for f in flips.iter_mut() {
        // Dominated = an earlier flip in the same chain already breaks the
        // prefix before this one, so this flip adds no extra invalidation.
        f.dominated = min_flip_index
            .map(|i0| f.message_index > i0)
            .unwrap_or(false);
    }

    ChainAnalysis {
        run_id,
        session_hash,
        n_messages: n,
        reads,
        flips,
        read_bytes_total,
        superseded_bytes,
        stale_bytes,
        live_bytes,
        reclaimed_bytes,
        invalidated_deduped,
    }
}

/// Classify one Read tool_result at message `idx` on file `fp`.
///
/// Finds the EARLIEST later message with a `Read`/`Edit`/`Write`/`NotebookEdit`
/// on the same file. A `Read` there means SUPERSEDED; an edit/write means STALE.
/// When the earliest index mixes both (a batched assistant turn can call Read
/// and Edit together), the write wins: the file's content changed, which is the
/// more terminal form of obsolescence.
fn classify_read(idx: usize, n: usize, fp: &str, events: &[Vec<(String, String)>]) -> Obsolescence {
    for j in (idx + 1)..n {
        let mut matched = false;
        let mut stale = false;
        for (name, event_fp) in &events[j] {
            if event_fp == fp {
                matched = true;
                if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit") {
                    stale = true;
                }
            }
        }
        if matched {
            return if stale {
                Obsolescence::Stale
            } else {
                Obsolescence::Superseded
            };
        }
    }
    Obsolescence::Live
}

/// Serialized JSON byte length of a tool_result's `content` — the bytes a
/// short marker would replace. Same serialization as the digest's part lengths,
/// so reclaimed bytes and message bytes are on the same scale.
fn tool_result_content_bytes(b: &serde_json::Map<String, Value>) -> u64 {
    match b.get("content") {
        None => 0,
        Some(v) => serde_json::to_vec(v).map(|s| s.len() as u64).unwrap_or(0),
    }
}

/// Same separator normalization as `native_trim::build_id_to_tool_info`
/// (`\` and `/` compare equal), applied to tool_use paths extracted for event
/// matching so the two sides of a match always use the same key space.
fn normalize_path(p: &str) -> String {
    p.trim().replace('\\', "/")
}

// ── corpus entry point ────────────────────────────────────────────────────────

/// Load every chain's FINAL request body and run the read-lifecycle analysis
/// over it. Read-only. Returns `Ok(None)` when the corpus has no usable
/// anthropic rows, mirroring `cache_bench::run_cache_bench`.
pub fn run_read_lifecycle(db_path: &Path) -> Result<Option<ReadLifecycleReport>, String> {
    // The corpus lives next to proxy.db: a per-provider file when it exists,
    // else the legacy `request_bodies` table in proxy.db. Either way the
    // returned connection exposes the table as plain `request_bodies`.
    let dir = db_path.parent().unwrap_or(Path::new(""));
    let Some(conn) = crate::corpus::open_read(dir, "anthropic") else {
        return Ok(None);
    };

    let chains: Vec<Chain> = load_chains(&conn, "main")?;
    if chains.is_empty() {
        return Ok(None);
    }

    let mut report = ReadLifecycleReport {
        n_chains: chains.len(),
        ..ReadLifecycleReport::default()
    };

    for chain in &chains {
        let Some(last) = chain.requests.last() else {
            continue;
        };
        let shash = session_hash(&last.body);
        let a = analyze_chain(last.run_id, shash, &last.body);
        if !a.reads.is_empty() {
            report.n_chains_with_reads += 1;
        }
        if !a.flips.is_empty() {
            report.n_chains_with_flips += 1;
        }
        report.n_reads += a.reads.len();
        report.n_live_reads += a
            .reads
            .iter()
            .filter(|r| r.obsolescence == Obsolescence::Live)
            .count();
        report.read_bytes_total += a.read_bytes_total;
        report.superseded_bytes += a.superseded_bytes;
        report.stale_bytes += a.stale_bytes;
        report.live_bytes += a.live_bytes;
        report.n_flips += a.flips.len();
        report.n_superseded_flips += a
            .flips
            .iter()
            .filter(|f| f.obsolescence == Obsolescence::Superseded)
            .count();
        report.n_stale_flips += a
            .flips
            .iter()
            .filter(|f| f.obsolescence == Obsolescence::Stale)
            .count();
        report.reclaimed_bytes += a.reclaimed_bytes;
        report.invalidated_naive_bytes += a.flips.iter().map(|f| f.bytes_after).sum::<u64>();
        report.invalidated_deduped_bytes += a.invalidated_deduped;
        report.depths.extend(a.flips.iter().map(|f| f.depth));
        report.flips.extend(a.flips);
    }

    // Worst-first for the top-N table: largest raw invalidation tail, then
    // deepest. The deduped aggregate (not this per-flip sort) is the honest
    // cost number.
    report.flips.sort_by(|a, b| {
        b.bytes_after
            .cmp(&a.bytes_after)
            .then(b.depth.cmp(&a.depth))
    });

    Ok(Some(report))
}

/// Min/p25/median/p75/max of a depth sample. Uses the same percentile index
/// convention as `cache_bench::ratio_stats` (`round((n-1)*p)`) so offline
/// reports agree.
pub fn depth_stats(depths: &[usize]) -> Option<DepthStats> {
    if depths.is_empty() {
        return None;
    }
    let mut v = depths.to_vec();
    v.sort_unstable();
    let pick = |p: f64| -> usize {
        let idx = (((v.len() - 1) as f64) * p).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    Some(DepthStats {
        n: v.len(),
        min: v[0],
        p25: pick(0.25),
        median: pick(0.50),
        p75: pick(0.75),
        max: v[v.len() - 1],
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── helpers ─────────────────────────────────────────────────────────────

    fn tool_use(id: &str, name: &str, file_path: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": name,
               "input": {"file_path": file_path}})
    }

    fn tool_result(id: &str, content: &str) -> Value {
        json!({"type": "tool_result", "tool_use_id": id, "content": content})
    }

    fn asst(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }

    fn user_msg(blocks: Vec<Value>) -> Value {
        json!({"role": "user", "content": blocks})
    }

    fn body(messages: Vec<Value>) -> Value {
        json!({"model": "claude-sonnet-4-6", "messages": messages})
    }

    fn analyze(messages: Vec<Value>) -> ChainAnalysis {
        analyze_chain(7, 0xabcdef, &body(messages))
    }

    fn first_read(a: &ChainAnalysis) -> &ReadInstance {
        a.reads
            .first()
            .expect("expected at least one Read tool_result")
    }

    // ── flip detection ──────────────────────────────────────────────────────

    #[test]
    fn superseded_by_a_later_read_of_the_same_file() {
        // Read a.rs at msg 1; the assistant at msg 2 issues a new Read of a.rs.
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "first read")]),
            asst(vec![tool_use("tu2", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu2", "second read")]),
        ]);
        assert_eq!(a.reads.len(), 2);
        let r = &a.reads[0];
        assert_eq!(r.message_index, 1);
        assert_eq!(r.obsolescence, Obsolescence::Superseded);
        // Flip depth: 2 messages remain after index 1.
        assert_eq!(r.depth, 2);
        // The second read is still live in this snapshot.
        assert_eq!(a.reads[1].obsolescence, Obsolescence::Live);
        assert_eq!(a.flips.len(), 1);
        assert_eq!(a.flips[0].obsolescence, Obsolescence::Superseded);
    }

    #[test]
    fn stale_by_a_later_edit_and_a_later_write() {
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![tool_use("tu2", "Edit", "a.rs")]),
            user_msg(vec![tool_result("tu2", "edit applied")]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Stale);

        let b = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "b.rs")]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![tool_use("tu2", "Write", "b.rs")]),
            user_msg(vec![tool_result("tu2", "written")]),
        ]);
        assert_eq!(first_read(&b).obsolescence, Obsolescence::Stale);
    }

    #[test]
    fn read_of_a_different_file_is_not_an_obsoletion_event() {
        // a.rs is never touched again — b.rs is a different file.
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "read a")]),
            asst(vec![tool_use("tu2", "Read", "b.rs")]),
            user_msg(vec![tool_result("tu2", "read b")]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Live);
        assert_eq!(a.flips.len(), 0);
    }

    #[test]
    fn earliest_obsoletion_event_wins() {
        // A re-read at msg 2 supersedes BEFORE the edit at msg 4 — the read is
        // superseded, not stale, because the earliest later event decides.
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![tool_use("tu2", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu2", "re-read")]),
            asst(vec![tool_use("tu3", "Edit", "a.rs")]),
            user_msg(vec![tool_result("tu3", "edited")]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Superseded);
    }

    #[test]
    fn mixed_same_index_read_plus_edit_classifies_stale() {
        // One assistant turn can batch a Read and an Edit of the same file; the
        // write is the terminal event, so the earlier read is stale.
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![
                tool_use("tu2", "Read", "a.rs"),
                tool_use("tu3", "Edit", "a.rs"),
            ]),
            user_msg(vec![
                tool_result("tu2", "re-read"),
                tool_result("tu3", "edited"),
            ]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Stale);
    }

    #[test]
    fn notebook_edit_counts_as_stale() {
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "nb.ipynb")]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![tool_use("tu2", "NotebookEdit", "nb.ipynb")]),
            user_msg(vec![tool_result("tu2", "edited")]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Stale);
    }

    #[test]
    fn read_tool_result_without_a_file_path_is_skipped() {
        // The orphan tool_result has no producing tool_use, so provenance
        // cannot resolve it — it must not be classified or flipped.
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![
                tool_result("tu1", "read"),
                tool_result("tu_unknown", "orphan"),
            ]),
        ]);
        // Only the resolvable a.rs read is counted.
        assert_eq!(a.reads.len(), 1);
    }

    #[test]
    fn path_separators_are_normalized_for_matching() {
        // The tool_result's producing Read uses a backslash path; the later
        // obsoleting Read uses a forward slash — they must still match.
        let a = analyze(vec![
            asst(vec![
                json!({"type": "tool_use", "id": "tu1", "name": "Read",
                             "input": {"file_path": "src\\a.rs"}}),
            ]),
            user_msg(vec![tool_result("tu1", "read")]),
            asst(vec![tool_use("tu2", "Read", "src/a.rs")]),
            user_msg(vec![tool_result("tu2", "re-read")]),
        ]);
        assert_eq!(first_read(&a).obsolescence, Obsolescence::Superseded);
    }

    // ── per-chain dedup ─────────────────────────────────────────────────────

    #[test]
    fn per_chain_dedup_counts_only_the_earliest_flip_tail() {
        // Two reads of a.rs, both later superseded. Flip at msg 1 and msg 3.
        // The earliest flip (msg 1) already invalidates everything after it, so
        // the later flip (msg 3) adds no extra invalidation.
        let messages = vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu1", "read 1")]),
            asst(vec![tool_use("tu2", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu2", "read 2")]),
            asst(vec![tool_use("tu3", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu3", "read 3")]),
        ];
        let a = analyze_chain(7, 0xabcdef, &body(messages.clone()));
        assert_eq!(a.flips.len(), 2);
        // flips are in message order: index 1 then index 3.
        let f0 = &a.flips[0];
        let f1 = &a.flips[1];
        assert_eq!((f0.message_index, f1.message_index), (1, 3));
        assert!(!f0.dominated, "the earliest flip bears the chain's break");
        assert!(
            f1.dominated,
            "a later flip inside the broken region is dominated"
        );

        // Deduped invalidation = bytes after index 1 (the earliest flip).
        let parts = digest_anthropic(&body(messages));
        let bytes_after_1: u64 = parts.iter().skip(1 + 3).map(|&(_, l)| l as u64).sum();
        assert_eq!(a.invalidated_deduped, bytes_after_1);
        assert_eq!(a.invalidated_deduped, f0.bytes_after);
        // The naive sum (bytes_after(1) + bytes_after(3)) is strictly larger:
        // it double-counts the shared tail, which is exactly what the dedup
        // prevents.
        assert!(f1.bytes_after > 0);
        assert!(f0.bytes_after + f1.bytes_after > a.invalidated_deduped);
    }

    #[test]
    fn two_flips_at_the_same_index_share_one_break() {
        // A user message carries two Read tool_results; one is an orphan whose
        // producing tool_use is absent, so only tu1 resolves. Both would sit at
        // index 1 and share one break; the accounting must never double-count a
        // break (the two-flip case is exercised directly above).
        let a = analyze(vec![
            asst(vec![tool_use("tu1", "Read", "a.rs")]),
            user_msg(vec![
                tool_result("tu1", "read a part 1"),
                tool_result("tu1b", "read a part 2"),
            ]),
            asst(vec![tool_use("tu2", "Read", "a.rs")]),
            user_msg(vec![tool_result("tu2", "re-read")]),
        ]);
        assert_eq!(a.flips.len(), 1);
        assert!(!a.flips[0].dominated);
        assert_eq!(a.invalidated_deduped, a.flips[0].bytes_after);
    }

    #[test]
    fn chain_without_reads_has_no_flips_and_zero_invalidation() {
        let a = analyze(vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ]);
        assert!(a.reads.is_empty());
        assert!(a.flips.is_empty());
        assert_eq!(a.reclaimed_bytes, 0);
        assert_eq!(a.invalidated_deduped, 0);
    }

    #[test]
    fn non_read_tools_do_not_flip() {
        // Grep/Edit tool_use ids resolve to non-Read names — their tool_results
        // must not be counted as Read instances at all.
        let a = analyze(vec![
            asst(vec![
                tool_use("tu1", "Read", "a.rs"),
                tool_use("tg1", "Grep", "a.rs"),
                tool_use("te1", "Edit", "a.rs"),
            ]),
            user_msg(vec![
                tool_result("tu1", "read"),
                tool_result("tg1", "matches"),
                tool_result("te1", "done"),
            ]),
        ]);
        assert_eq!(a.reads.len(), 1);
        assert_eq!(a.reads[0].file_path, "a.rs");
        assert_eq!(a.reads[0].obsolescence, Obsolescence::Live);
    }

    // ── depth stats ─────────────────────────────────────────────────────────

    #[test]
    fn depth_stats_known_sample() {
        let s = depth_stats(&[1, 2, 3, 4, 5]).expect("non-empty sample");
        assert_eq!(s.n, 5);
        assert_eq!(s.min, 1);
        assert_eq!(s.max, 5);
        // Percentile index round((n-1)*p): 0, 1, 2, 3, 4.
        assert_eq!(s.p25, 2);
        assert_eq!(s.median, 3);
        assert_eq!(s.p75, 4);
    }

    #[test]
    fn depth_stats_empty_is_none() {
        assert!(depth_stats(&[]).is_none());
    }
}
