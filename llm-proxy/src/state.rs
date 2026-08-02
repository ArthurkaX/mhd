use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Config;

/// Which tier an incoming request belongs to — one of the four Claude model
/// classes. The client axis (Claude Code vs Codex vs OpenAI) is tracked
/// separately via [`ClientKind`]; this enum only ever means "class of Claude
/// model", so it is `None` for requests from non-Claude clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Opus,
    Sonnet,
    Haiku,
    Fable,
}

impl Tier {
    /// Classify by the model id Claude Code sends.
    pub fn from_model(model: &str) -> Self {
        if model.contains("opus") {
            Self::Opus
        } else if model.contains("haiku") {
            Self::Haiku
        } else if model.contains("fable") {
            Self::Fable
        } else {
            // sonnet and anything unknown fall into the sonnet slot
            Self::Sonnet
        }
    }

    /// Slot name used on the wire / in config.
    pub fn slot(&self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Haiku => "haiku",
            Self::Fable => "fable",
        }
    }
}

/// Which client family a request came from. Determined at ingress from the
/// route it arrived on, never guessed from the model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    ClaudeCode,
    Codex,
    OpenAi,
}

impl ClientKind {
    /// Stable identifier used in config keys and the `client` DB column.
    pub fn slot(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::OpenAi => "openai",
        }
    }

    /// Parse a slot string (case-insensitive). Accepts the values returned by
    /// [`slot`](Self::slot); anything else — including empty — is rejected.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "claude_code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// Human-readable name for menus and the trace filter chip.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenAi => "OpenAI",
        }
    }

    /// Compact name for the trace table, whose Client column competes for
    /// width with the target model. Only "Claude Code" is long enough to be
    /// worth shortening; the other two already fit and stay spelled out, so
    /// the column never trades clarity for width it does not need.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "CC",
            Self::Codex => "Codex",
            Self::OpenAi => "OpenAI",
        }
    }

    /// All variants, for iterating menus/settings.
    pub fn all() -> [ClientKind; 3] {
        [Self::ClaudeCode, Self::Codex, Self::OpenAi]
    }
}

/// The wire protocol a request arrived on. Separate from [`ClientKind`]
/// because one client family can in principle speak more than one protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    AnthropicMessages,
    ChatCompletions,
    Responses,
}

impl WireApi {
    /// Stable identifier used in the `wire_api` DB column.
    pub fn slot(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

/// How verbose the proxy's debug logging should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DebugLevel {
    #[default]
    None,
    /// Errors only.
    Minimal,
    /// Log everything except content bodies (headers, tools, message structure).
    Detailed,
    /// Full session dump including request/response bodies.
    Maximal,
}

impl DebugLevel {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "minimal" => Self::Minimal,
            "detailed" => Self::Detailed,
            "maximal" | "max" | "full" => Self::Maximal,
            _ => Self::None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Detailed => "detailed",
            Self::Maximal => "maximal",
        }
    }

    /// True if we should dump full request/response bodies.
    pub fn dump_bodies(&self) -> bool {
        matches!(self, Self::Maximal)
    }

    /// True if we should log detailed request/response info (without bodies).
    pub fn log_detailed(&self) -> bool {
        matches!(self, Self::Detailed | Self::Maximal)
    }

    /// True if we should log errors.
    pub fn log_errors(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Where a tier routes to: the official Anthropic API, or a specific upstream
/// model id on the OpenAI-compatible gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Passthrough to api.anthropic.com (uses the client's own auth).
    Native,
    /// Route to the upstream gateway with this model id.
    Model(String),
}

/// Codex route target. Kept separate from Claude tier routing because Codex
/// uses the Responses API and has a native ChatGPT OAuth branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexTarget {
    Native,
    Model(String),
}

impl CodexTarget {
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case(NATIVE) || s.is_empty() {
            Self::Native
        } else {
            Self::Model(s.to_string())
        }
    }
    pub fn as_str(&self) -> &str {
        match self {
            Self::Native => NATIVE,
            Self::Model(id) => id,
        }
    }
}

/// A single proxy routing decision recorded for the live trace overlay.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub seq: u64,
    /// Client family this request came from (Claude Code, Codex, OpenAI).
    pub client: ClientKind,
    /// Wire protocol the request arrived on (Anthropic Messages, Chat
    /// Completions, or Responses).
    pub wire_api: WireApi,
    /// Resolved Claude tier. `None` for requests from non-Claude clients —
    /// the tier axis only means "class of Claude model" and does not apply.
    pub tier: Option<Tier>,
    /// Tier actually routed after downgrade. `None` for non-Claude clients.
    pub effective_tier: Option<Tier>,
    pub target: String,
    pub model: String,
    pub downgraded: bool,
    pub reason: String,
    /// Fresh (uncached) prompt tokens billed at 1× (0 until response arrives).
    pub input_tokens: u64,
    /// Output tokens reported in the response (0 until response arrives).
    pub output_tokens: u64,
    /// Tokens served from the prompt cache (cache_read_input_tokens / cached_tokens).
    /// `None` = the upstream reported no cache field at all (route is silent about
    /// caching); `Some(0)` = it reported zero. Never collapse the two.
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to the prompt cache this turn (Anthropic only).
    /// `None` on every OpenAI-compatible route — the concept has no wire field there.
    pub cache_creation_tokens: Option<u64>,
    /// True if the trim engine was applied to this request.
    pub trim_applied: bool,
    /// Estimated tokens before trimming.
    pub trim_tokens_before: u64,
    /// Estimated tokens after trimming.
    pub trim_tokens_after: u64,
    /// Active trim preset name (e.g. "agent", "auto"). Empty when not applied.
    pub trim_preset: String,
    /// JSON snapshot of active trim knobs. Empty when not applied.
    pub trim_config_json: String,
    /// JSON array of per-stage trim reports. Empty when not applied.
    pub trim_stages_json: String,
    /// Unix milliseconds when this entry was first pushed (set at push_trace time).
    /// Zero for entries created before this field was added; treat zero as unknown.
    pub started_ms: u64,
    /// FNV-1a 64-bit hash of the cacheable prefix (system + tools). Zero means
    /// unknown (old entries or OpenAI passthrough). Used in the Proxy Trace
    /// overlay to distinguish COLD (first fill) from MISS (warm but missed).
    pub prefix_hash: u64,
    /// HTTP status of the upstream response (None until the response arrives).
    /// 2xx = success; >=400 = error (rendered red + collapsed in the trace).
    pub status: Option<u16>,
    /// True when this request was a `max_tokens == 1` background probe (token
    /// count / cache-warm check) sent by Claude Code. Probes get their own
    /// throttle lane and render as a quiet gray "xN probe" line in the trace
    /// instead of a red error burst.
    pub is_probe: bool,
    /// Client HTTP User-Agent (e.g. "claude-cli/..."). Written at INSERT time.
    pub user_agent: Option<String>,
    /// Value of the `x-client-run-id` header, if the client sent one. Stitches
    /// this request to a harness-side session log. Never forwarded upstream.
    pub client_run_id: Option<String>,
    /// Prefix-shape measurements. Not a cache signal — see [`crate::prefix`].
    pub prefix: crate::prefix::PrefixStats,
}

pub const MAX_TRACE_ENTRIES: usize = 500;

/// A single vision screenshot request recorded for the live trace overlay.
#[derive(Debug, Clone)]
pub struct VisionTraceEntry {
    pub seq: u64,
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

pub const MAX_VISION_TRACE_ENTRIES: usize = 50;

/// The sentinel string used on the wire / in config to mean "Anthropic native".
pub const NATIVE: &str = "native";

impl Target {
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case(NATIVE) || s.is_empty() {
            Self::Native
        } else {
            Self::Model(s.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Native => NATIVE,
            Self::Model(id) => id,
        }
    }
}

/// Shared application state. All routing fields are runtime-mutable so the
/// daemon can switch targets without restarting Claude Code.
///
/// Uses `std::sync::RwLock` (not tokio's) so the routing state can be read and
/// written from synchronous code (the daemon) as well as from the async request
/// handlers. Lock hold times are tiny (a clone), so blocking is a non-issue.
pub struct AppState {
    pub anthropic_key: RwLock<String>,
    pub upstream_base_url: RwLock<String>,
    pub upstream_key: RwLock<String>,
    pub opus_target: RwLock<Target>,
    pub sonnet_target: RwLock<Target>,
    pub haiku_target: RwLock<Target>,
    pub fable_target: RwLock<Target>,
    pub codex_target: RwLock<CodexTarget>,
    pub log_level: RwLock<DebugLevel>,
    /// Opus downgrade when no thinking.
    pub opus_downgrade_enabled: RwLock<bool>,
    /// Sonnet downgrade when no thinking.
    pub sonnet_downgrade_enabled: RwLock<bool>,
    /// Shared HTTP client — reused across requests so connections (and TLS
    /// sessions) are pooled. Creating a fresh `reqwest::Client` per request
    /// defeats keep-alive and serializes parallel load behind new handshakes.
    pub http: reqwest::Client,
    /// Dedicated client for STREAMING responses. Unlike [`Self::http`], it sets
    /// NO total-request timeout — a total deadline would guillotine long
    /// generations mid-stream. Instead it bounds only connect time and idle time
    /// between chunks (`read_timeout`), so a genuinely dead/hung upstream is still
    /// caught while a long-but-healthy stream runs to completion.
    pub http_stream: reqwest::Client,
    /// Monotonic request id, for correlating log lines.
    pub req_seq: AtomicU64,
    /// Number of upstream requests currently in flight (observability only).
    pub inflight: AtomicU64,
    /// Structured SQLite log (lazily created on first enable).
    pub db_log: Mutex<Option<crate::db_log::DbLog>>,
    /// Ring buffer of recent routing decisions, newest at the back.
    pub trace: RwLock<VecDeque<TraceEntry>>,
    /// Ring buffer of recent vision screenshot requests, newest at the back.
    pub vision_trace: RwLock<VecDeque<VisionTraceEntry>>,
    /// Per-session last-request timestamp (epoch ms). Key = session hash.
    /// Only updated for Opus/Sonnet requests when a downgrade tier is enabled.
    pub session_last_ts: RwLock<HashMap<u64, u64>>,

    /// Master switch for native request compression.
    pub trim_enabled: RwLock<bool>,

    /// Per-client master switch for Claude Code. When false the Claude Code
    /// ingress route is refused and its background usage polling is skipped —
    /// the point is to stop mhd doing work for a client the user does not use
    /// at all.
    pub client_claude_code_enabled: RwLock<bool>,

    /// Per-client master switch for Codex. See
    /// [`Self::client_claude_code_enabled`].
    pub client_codex_enabled: RwLock<bool>,

    /// Per-client master switch for OpenAI-compatible clients. See
    /// [`Self::client_claude_code_enabled`].
    pub client_openai_enabled: RwLock<bool>,

    /// Codex trim switch. Trim for the Responses wire API is NOT implemented
    /// yet, so this gates nothing today; it exists so the client axis is
    /// complete and the engine can be added behind it without a UI change.
    pub trim_codex_enabled: RwLock<bool>,

    /// OpenAI-compatible client trim switch. Independent of `trim_enabled`;
    /// resolved once from settings at Config-build time (see
    /// [`crate::config::Settings::trim_openai`]).
    pub trim_openai_enabled: RwLock<bool>,

    /// Max Unicode chars per tool description (native engine knob).
    pub trim_tool_desc_chars: RwLock<usize>,

    /// Max chars for the head of a tool_result block (native engine knob).
    pub trim_toolresult_head: RwLock<usize>,

    /// Max chars for the head of a tool_result block (native Haiku requests).
    pub trim_head_haiku: RwLock<usize>,

    /// Max chars for the head of a tool_result block (OpenAI-harness requests).
    pub trim_head_harness: RwLock<usize>,

    /// Max chars for the tail of a tool_result block (native engine knob).
    pub trim_toolresult_tail: RwLock<usize>,

    /// Master switch for native Anthropic retry on HTTP 429 / 529. Live-tunable.
    pub retry_enabled: RwLock<bool>,
    /// Total attempts (including the first) for the native Anthropic retry loop.
    pub retry_max_attempts: RwLock<usize>,
    /// Base backoff delay in ms for the native Anthropic retry loop.
    pub retry_base_delay_ms: RwLock<u64>,
    /// Cap in ms for a single backoff wait in the native Anthropic retry loop.
    pub retry_max_delay_ms: RwLock<u64>,

    /// Master switch for the outbound rate limiter (token-bucket throttle).
    pub throttle_enabled: RwLock<bool>,
    /// Steady refill rate (requests/sec) for the token-bucket throttle.
    pub throttle_rate_per_sec: RwLock<f64>,
    /// Bucket capacity (max instantaneous burst) for the token-bucket throttle.
    pub throttle_burst: RwLock<f64>,
    /// Concrete token bucket — interior Mutex, live-tunable via set_rate.
    pub throttle_bucket: crate::throttle::TokenBucket,
    /// Separate throttle lane for `max_tokens == 1` background probes sent by
    /// Claude Code (token-count / cache-warm checks). A probe salvo draws from
    /// this bucket so it never queues ahead of a real request in the shared
    /// bucket. Fixed modest rate (burst 4, 4/sec) — not yet config-tunable.
    pub throttle_probe_bucket: crate::throttle::TokenBucket,

    /// Master switch for whitespace compression in the native trim engine.
    pub trim_ws_enabled: RwLock<bool>,

    /// Designated free/cheap-tier target. Empty string disables the feature.
    pub trim_free_target: RwLock<String>,

    /// Strip thinking/redacted_thinking from old assistant turns (native engine).
    pub trim_strip_thinking: RwLock<bool>,

    /// When true, Layer 2 fence protection requires the fenced content to also
    /// look code-like. Live-tunable.
    pub trim_fence_requires_code: RwLock<bool>,

    /// Minimum glyph density for the arrow-driven diagram detector. Live-tunable.
    pub trim_arrow_density_min: RwLock<f64>,

    /// Master switch for replay-corpus capture.
    pub corpus_capture_enabled: RwLock<bool>,

    /// Maximum rows to keep in the `request_bodies` corpus table (0 = unlimited).
    /// Fixed at proxy construction time; set from config.
    pub corpus_max_rows: usize,

    /// Last quota snapshot WRITTEN to the DB, with the instant it was written.
    /// Used to dedup: we only persist a new row on material change or after a
    /// minimum interval, keeping the quota table a sparse time-series.
    pub last_quota: RwLock<Option<(crate::db_log::QuotaSnapshot, std::time::Instant)>>,

    /// Last polled quota snapshot WRITTEN to the DB, with the instant it was
    /// written. Separate from `last_quota` because the header path and the
    /// OAuth poller populate different fields — sharing a slot would flip
    /// status fields Some↔None on every alternation and write a row every
    /// single poll.
    pub last_quota_poll: RwLock<Option<(crate::db_log::QuotaSnapshot, std::time::Instant)>>,

    /// Master switch for the OAuth quota poller. Default false — the daemon
    /// pushes the real value on proxy start (which may arrive after the watcher
    /// has already started), so staying off until explicitly enabled is the
    /// safe default.
    pub quota_poll_enabled: RwLock<bool>,

    /// Stable id for this daemon run (epoch-millis at construction). Used to
    /// group all requests from one process lifetime in the `requests` table.
    pub run_id: u64,

    /// Last request seen per (client run, route), for the prefix-shape metric.
    /// In-memory and TTL'd rather than a `requests` lookup: the previous digest
    /// is needed on the hot path and `db_log` is a single mutex shared with
    /// every write the proxy makes.
    pub prefix_tracker: crate::prefix::PrefixTracker,
}

/// Whether a utilisation value moved by >=0.01 (the epsilon for dedup).
/// Shared by the header path and the OAuth poller.
fn utilization_moved(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() >= 0.01,
        (None, None) => false,
        _ => true,
    }
}

/// Material change = utilisation moved >=0.01 on either window, OR any status /
/// representative_claim string changed.
fn quota_material_change(
    prev: &crate::db_log::QuotaSnapshot,
    next: &crate::db_log::QuotaSnapshot,
) -> bool {
    utilization_moved(prev.h5_utilization, next.h5_utilization)
        || utilization_moved(prev.d7_utilization, next.d7_utilization)
        || prev.h5_status != next.h5_status
        || prev.d7_status != next.d7_status
        || prev.representative_claim != next.representative_claim
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Arc::new(Self {
            prefix_tracker: crate::prefix::PrefixTracker::new(),
            anthropic_key: RwLock::new(cfg.anthropic_key.clone()),
            upstream_base_url: RwLock::new(cfg.upstream_base_url.clone()),
            upstream_key: RwLock::new(cfg.upstream_key.clone()),
            opus_target: RwLock::new(Target::parse(&cfg.opus_target)),
            sonnet_target: RwLock::new(Target::parse(&cfg.sonnet_target)),
            haiku_target: RwLock::new(Target::parse(&cfg.haiku_target)),
            fable_target: RwLock::new(Target::parse(&cfg.fable_target)),
            codex_target: RwLock::new(CodexTarget::parse(&cfg.codex_target)),
            log_level: RwLock::new(DebugLevel::parse(&cfg.log_level)),
            opus_downgrade_enabled: RwLock::new(cfg.opus_downgrade_enabled),
            sonnet_downgrade_enabled: RwLock::new(cfg.sonnet_downgrade_enabled),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to build HTTP client"),
            http_stream: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(30))
                .read_timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("Failed to build streaming HTTP client"),
            req_seq: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            db_log: Mutex::new(None),
            trace: RwLock::new(VecDeque::with_capacity(MAX_TRACE_ENTRIES)),
            vision_trace: RwLock::new(VecDeque::with_capacity(MAX_VISION_TRACE_ENTRIES)),
            session_last_ts: RwLock::new(HashMap::new()),
            trim_enabled: RwLock::new(cfg.trim_enabled),
            client_claude_code_enabled: RwLock::new(cfg.client_claude_code_enabled),
            client_codex_enabled: RwLock::new(cfg.client_codex_enabled),
            client_openai_enabled: RwLock::new(cfg.client_openai_enabled),
            trim_codex_enabled: RwLock::new(cfg.trim_codex_enabled),
            trim_openai_enabled: RwLock::new(cfg.trim_openai_enabled),
            trim_tool_desc_chars: RwLock::new(cfg.trim_tool_desc_chars),
            trim_toolresult_head: RwLock::new(cfg.trim_toolresult_head),
            trim_head_haiku: RwLock::new(cfg.trim_head_haiku),
            trim_head_harness: RwLock::new(cfg.trim_head_harness),
            trim_toolresult_tail: RwLock::new(cfg.trim_toolresult_tail),
            retry_enabled: RwLock::new(cfg.retry_enabled),
            retry_max_attempts: RwLock::new(cfg.retry_max_attempts),
            retry_base_delay_ms: RwLock::new(cfg.retry_base_delay_ms),
            retry_max_delay_ms: RwLock::new(cfg.retry_max_delay_ms),
            throttle_enabled: RwLock::new(cfg.throttle_enabled),
            throttle_rate_per_sec: RwLock::new(cfg.throttle_rate_per_sec),
            throttle_burst: RwLock::new(cfg.throttle_burst),
            throttle_bucket: crate::throttle::TokenBucket::new(
                cfg.throttle_burst,
                cfg.throttle_rate_per_sec,
            ),
            throttle_probe_bucket: crate::throttle::TokenBucket::new(4.0, 4.0),
            trim_ws_enabled: RwLock::new(cfg.trim_ws_enabled),
            trim_free_target: RwLock::new(cfg.trim_free_target.clone()),
            trim_strip_thinking: RwLock::new(cfg.trim_strip_thinking),
            trim_fence_requires_code: RwLock::new(cfg.trim_fence_requires_code),
            trim_arrow_density_min: RwLock::new(cfg.trim_arrow_density_min),
            corpus_capture_enabled: RwLock::new(cfg.corpus_capture),
            corpus_max_rows: cfg.corpus_max_rows,
            quota_poll_enabled: RwLock::new(false),
            last_quota: RwLock::new(None),
            last_quota_poll: RwLock::new(None),
            run_id,
        })
    }

    /// Append a text line to the database log.
    /// Best-effort: errors are swallowed.
    pub fn log_line(&self, msg: &str) {
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.insert(
                &crate::providers::now_ms(),
                &crate::db_log::LogEvent {
                    seq: 0,
                    event_type: "RAW".to_string(),
                    detail: Some(msg.to_string()),
                    ..Default::default()
                },
            );
        }
    }

    /// Log a structured event with typed fields.
    /// Timestamp is automatic. Writes to SQLite.
    pub fn log_event(&self, event: crate::db_log::LogEvent) {
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            let ts = crate::providers::now_ms();
            db.insert(&ts, &event);
        }
    }

    /// Enable or disable the database log.
    /// When enabling, the database is opened (and created if needed) lazily.
    pub fn set_db_log_enabled(&self, enabled: bool) {
        if enabled {
            let mut guard = self.db_log.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                let db_path = crate::config::config_dir().join("proxy.db");
                match crate::db_log::DbLog::open(&db_path, self.corpus_max_rows) {
                    Ok(db) => {
                        *guard = Some(db);
                    }
                    Err(e) => {
                        // Loud on purpose. When this fired the proxy kept serving
                        // traffic with logging silently off, and the loss only
                        // surfaced days later as a flat line on the dashboard.
                        eprintln!(
                            "mhd: proxy.db FAILED TO OPEN: {e}\n\
                             mhd: request/quota logging is DISABLED for this run \
                             -- the dashboard will show no new data."
                        );
                    }
                }
            }
        } else {
            let mut guard = self.db_log.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
    }

    /// Check whether the database log is currently enabled.
    pub fn is_db_log_enabled(&self) -> bool {
        self.db_log.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Record a request arrival for a given session and return the gap in seconds.
    /// Returns a sentinel (9999) on first request for this session so it's never downgraded.
    pub fn mark_session_request(&self, session_hash: u64, now_ms: u64) -> u64 {
        let mut map = self
            .session_last_ts
            .write()
            .unwrap_or_else(|e| e.into_inner());

        // Lazy TTL cleanup: if map is too large, remove entries older than 10 minutes
        if map.len() > 100 {
            let cutoff = now_ms.saturating_sub(600_000);
            map.retain(|_, &mut v| v > cutoff);
        }

        let prev = map.insert(session_hash, now_ms);
        match prev {
            Some(last) => ((now_ms.saturating_sub(last)) / 1000).min(9999),
            None => 9999, // first request in this session
        }
    }

    /// Record a routing decision into the ring buffer, evicting the oldest
    /// entry if the buffer is full. Also writes a typed row to proxy.db's
    /// `requests` table so everything visible in the overlay is queryable later.
    /// DB write happens whenever the log is enabled — independent of log_level.
    pub fn push_trace(&self, entry: TraceEntry) {
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            let row = crate::db_log::RequestRow {
                run_id: self.run_id,
                seq: entry.seq,
                ts_start: crate::providers::now_ms(),
                client: Some(entry.client.slot().to_string()),
                wire_api: Some(entry.wire_api.slot().to_string()),
                tier: entry.tier.map(|t| t.slot().to_string()),
                effective_tier: entry.effective_tier.map(|t| t.slot().to_string()),
                target: Some(entry.target.clone()),
                model: Some(entry.model.clone()),
                downgraded: entry.downgraded,
                downgrade_reason: if entry.downgraded {
                    Some(entry.reason.clone())
                } else {
                    None
                },
                trim_applied: entry.trim_applied,
                trim_preset: if entry.trim_applied {
                    Some(entry.trim_preset.clone())
                } else {
                    None
                },
                trim_config: if entry.trim_applied {
                    Some(entry.trim_config_json.clone())
                } else {
                    None
                },
                trim_tokens_before: if entry.trim_applied {
                    Some(entry.trim_tokens_before)
                } else {
                    None
                },
                trim_tokens_after: if entry.trim_applied {
                    Some(entry.trim_tokens_after)
                } else {
                    None
                },
                trim_stages: if entry.trim_applied {
                    Some(entry.trim_stages_json.clone())
                } else {
                    None
                },
                user_agent: entry.user_agent.clone(),
                client_run_id: entry.client_run_id.clone(),
                prefix: entry.prefix.clone(),
            };
            db.insert_request(&row);
        }
        let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
        if trace.len() >= MAX_TRACE_ENTRIES {
            trace.pop_front();
        }
        trace.push_back(entry);
    }

    /// Update token counts on the trace entry matching this req_id.
    /// Best-effort: silently no-ops if the entry was already evicted.
    /// Also updates the completion columns on proxy.db's `requests` row
    /// (independent of log_level — DB write happens whenever the log is open).
    ///
    /// `None` for a cache count means the upstream response had no such field;
    /// it is stored as NULL, not 0. Pass `Some(0)` only for a reported zero.
    pub fn update_trace_tokens(
        &self,
        req_id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        duration_ms: Option<u64>,
        status: Option<u16>,
        upstream_model: Option<&str>,
    ) {
        let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
        // Search from the back — the matching entry is almost certainly recent.
        for entry in trace.iter_mut().rev() {
            if entry.seq == req_id {
                entry.input_tokens = input_tokens;
                entry.output_tokens = output_tokens;
                entry.cache_read_tokens = cache_read_tokens;
                entry.cache_creation_tokens = cache_creation_tokens;
                entry.status = status;
                break;
            }
        }
        drop(trace);
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.update_request_completion(
                self.run_id,
                req_id,
                &crate::providers::now_ms(),
                duration_ms,
                Some(input_tokens),
                Some(output_tokens),
                cache_read_tokens,
                cache_creation_tokens,
                status,
                None,
                None,
                upstream_model,
            );
        }
    }

    /// Write a completion UPDATE for a request that terminated with an error,
    /// so the row never stays dangling (ts_end NULL). Input/output are recorded
    /// as 0 (a failed request bills nothing), but the cache columns are left
    /// NULL: a request that died never carried usage, and writing 0 would make
    /// the route look like it reported "no cache" in the `route_cache` verdict.
    /// Best-effort: errors are swallowed and the write is gated on the db being
    /// enabled — mirrors [`update_trace_tokens`].
    pub fn mark_request_failed(
        &self,
        req_id: u64,
        duration_ms: Option<u64>,
        status: Option<u16>,
        error: &str,
        error_kind: &str,
    ) {
        // Reflect the failure on the live trace entry so the Proxy Trace overlay
        // can render it (red + collapsed). Mirrors update_trace_tokens' locking.
        {
            let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
            for entry in trace.iter_mut().rev() {
                if entry.seq == req_id {
                    entry.status = status;
                    break;
                }
            }
        }
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.update_request_completion(
                self.run_id,
                req_id,
                &crate::providers::now_ms(),
                duration_ms,
                Some(0),
                Some(0),
                None,
                None,
                status,
                Some(error),
                Some(error_kind),
                None,
            );
        }
    }

    /// Flip the `is_probe` flag on the in-flight trace entry for `req_id`.
    /// Called from the native Anthropic path once the request body is known to
    /// be a `max_tokens == 1` background probe, so the Proxy Trace overlay can
    /// render it as a quiet gray "×N probe" line instead of a red error burst.
    /// No-op if the entry has already rotated out of the ring buffer.
    pub fn mark_probe(&self, req_id: u64) {
        let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
        for entry in trace.iter_mut().rev() {
            if entry.seq == req_id {
                entry.is_probe = true;
                break;
            }
        }
    }

    /// Close a request row left open by a mid-flight client cancellation.
    /// Delegates to the race-free [`crate::db_log::DbLog::mark_request_cancelled`]
    /// — a no-op if a real completion already wrote `ts_end`. Best-effort.
    pub fn mark_request_cancelled(&self, req_id: u64, duration_ms: Option<u64>) {
        // Keep the live overlay in sync with the durable row. A real
        // completion sets a status before the guard is dropped, so this only
        // marks entries that are still genuinely in flight. 499 is the
        // conventional client-closed status and lets Proxy Trace render the
        // cancellation instead of leaving token columns as unexplained dashes.
        {
            let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
            for entry in trace.iter_mut().rev() {
                if entry.seq == req_id && entry.status.is_none() {
                    entry.status = Some(499);
                    break;
                }
            }
        }
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.mark_request_cancelled(
                self.run_id,
                req_id,
                &crate::providers::now_ms(),
                duration_ms,
            );
        }
    }

    /// Per-route cache verdict from the `route_cache` view.
    /// Empty when the DB log is disabled — an empty list means "unknown",
    /// never "no route caches".
    pub fn route_cache(&self) -> Vec<crate::db_log::RouteCacheRow> {
        match self.db_log.lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|db| db.route_cache())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Write a free-text note to the `notes` table in proxy.db.
    /// Best-effort: no-ops if the log is disabled or the write fails.
    pub fn log_note(&self, text: &str) {
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.insert_note(&crate::providers::now_ms(), text);
        }
    }

    /// Record a finished Bench run into proxy.db (best-effort, uses the live writer conn).
    pub fn record_bench_run(
        &self,
        result: &crate::bench::BenchResult,
        headroom_pct: f64,
        knobs_json: &str,
    ) {
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            db.insert_bench_run(
                &crate::providers::now_ms(),
                "anthropic",
                result,
                headroom_pct,
                knobs_json,
            );
        }
    }

    /// Capture the full pre-trim request body into the replay corpus, if enabled.
    /// Best-effort: silent when the flag is off, when the DB log is closed, or on error.
    pub fn capture_request_body(
        &self,
        seq: u64,
        model: Option<&str>,
        provider: &str,
        body: &serde_json::Value,
    ) {
        if !*self
            .corpus_capture_enabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            return;
        }
        let body_str = match serde_json::to_string(body) {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Ok(guard) = self.db_log.lock()
            && let Some(ref db) = *guard
        {
            let ts = crate::providers::now_ms();
            db.insert_request_body(self.run_id, seq, &ts, model, provider, &body_str);
        }
    }

    /// Persist a quota snapshot to the `quota` table, but only when it materially
    /// changed since the last write or >=60s elapsed (keeps writes sparse). No-op
    /// if the snapshot has no quota fields, or db logging is off.
    pub fn record_quota(&self, snap: crate::db_log::QuotaSnapshot) {
        // 1. skip empty snapshots (no 5h AND no 7d utilization present)
        if snap.h5_utilization.is_none() && snap.d7_utilization.is_none() {
            return;
        }
        // 2. gate on db logging
        if !self.is_db_log_enabled() {
            return;
        }

        let now = std::time::Instant::now();
        let mut guard = self.last_quota.write().unwrap_or_else(|e| e.into_inner());
        let should_write = match guard.as_ref() {
            None => true,
            Some((prev, at)) => {
                quota_material_change(prev, &snap)
                    || now.duration_since(*at) >= std::time::Duration::from_secs(60)
            }
        };
        if !should_write {
            return;
        }

        if let Ok(db_guard) = self.db_log.lock()
            && let Some(ref db) = *db_guard
        {
            db.insert_quota(self.run_id, &crate::providers::now_ms(), &snap);
        }
        *guard = Some((snap, now));
    }

    /// Persist a polled (OAuth endpoint) quota snapshot to the `quota` table.
    ///
    /// Separate from `record_quota` because the polled snapshot only has
    /// h5/d7/fable utilisation + reset (no status/claim fields). Sharing
    /// `last_quota` would flip those status fields Some<->None on every
    /// alternation and write a row on every single poll.
    ///
    /// Guards: skip when all three utilizations are `None`, when db logging
    /// is disabled, when none of the three moved by >=0.01, and when <60s
    /// have elapsed since the last write. First write always goes through.
    pub fn record_quota_polled(&self, snap: crate::db_log::QuotaSnapshot) {
        // Skip empty snapshots (no h5, d7, AND no fable utilization).
        if snap.h5_utilization.is_none()
            && snap.d7_utilization.is_none()
            && snap.fable_utilization.is_none()
        {
            return;
        }
        if !self.is_db_log_enabled() {
            return;
        }

        let now = std::time::Instant::now();
        let mut guard = self
            .last_quota_poll
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let should_write = match guard.as_ref() {
            None => true,
            Some((prev, at)) => {
                utilization_moved(prev.h5_utilization, snap.h5_utilization)
                    || utilization_moved(prev.d7_utilization, snap.d7_utilization)
                    || utilization_moved(prev.fable_utilization, snap.fable_utilization)
                    || now.duration_since(*at) >= std::time::Duration::from_secs(60)
            }
        };
        if !should_write {
            return;
        }

        if let Ok(db_guard) = self.db_log.lock()
            && let Some(ref db) = *db_guard
        {
            db.insert_quota(self.run_id, &crate::providers::now_ms(), &snap);
        }
        *guard = Some((snap, now));
    }

    /// Whether the OAuth quota poller is currently enabled.
    pub fn is_quota_poll_enabled(&self) -> bool {
        *self
            .quota_poll_enabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Record a vision screenshot request into the ring buffer.
    pub fn push_vision_trace(&self, entry: VisionTraceEntry) {
        let mut trace = self.vision_trace.write().unwrap_or_else(|e| e.into_inner());
        if trace.len() >= MAX_VISION_TRACE_ENTRIES {
            trace.pop_front();
        }
        trace.push_back(entry);
    }

    /// Snapshot of recent vision screenshot requests (newest at the end).
    pub fn vision_trace_snapshot(&self) -> Vec<VisionTraceEntry> {
        self.vision_trace
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Snapshot of recent routing decisions (newest at the end).
    pub fn trace_snapshot(&self) -> Vec<TraceEntry> {
        self.trace
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Latest quota snapshot recorded from Anthropic rate-limit headers, if any.
    pub fn get_quota(&self) -> Option<crate::db_log::QuotaSnapshot> {
        self.last_quota
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(snap, _)| snap.clone())
    }

    /// Allocate the next request id.
    pub fn next_req_id(&self) -> u64 {
        self.req_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Current target for a tier.
    pub fn target_for(&self, tier: Tier) -> Target {
        match tier {
            Tier::Opus => self
                .opus_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            Tier::Sonnet => self
                .sonnet_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            Tier::Haiku => self
                .haiku_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            Tier::Fable => self
                .fable_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Set a tier's target by slot name ("opus"/"sonnet"/"haiku"/"fable").
    /// Returns false for an unknown slot.
    pub fn set_target(&self, slot: &str, target: Target) -> bool {
        match slot {
            "opus" => *self.opus_target.write().unwrap_or_else(|e| e.into_inner()) = target,
            "sonnet" => {
                *self
                    .sonnet_target
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = target
            }
            "haiku" => *self.haiku_target.write().unwrap_or_else(|e| e.into_inner()) = target,
            "fable" => *self.fable_target.write().unwrap_or_else(|e| e.into_inner()) = target,
            _ => return false,
        }
        true
    }

    pub fn codex_target(&self) -> CodexTarget {
        self.codex_target
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_codex_target(&self, target: CodexTarget) {
        *self.codex_target.write().unwrap_or_else(|e| e.into_inner()) = target;
    }

    /// Whether a given client's ingress is currently enabled.
    pub fn client_enabled(&self, client: ClientKind) -> bool {
        let lock = match client {
            ClientKind::ClaudeCode => &self.client_claude_code_enabled,
            ClientKind::Codex => &self.client_codex_enabled,
            ClientKind::OpenAi => &self.client_openai_enabled,
        };
        *lock.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Enable or disable a client's ingress.
    pub fn set_client_enabled(&self, client: ClientKind, on: bool) {
        let lock = match client {
            ClientKind::ClaudeCode => &self.client_claude_code_enabled,
            ClientKind::Codex => &self.client_codex_enabled,
            ClientKind::OpenAi => &self.client_openai_enabled,
        };
        *lock.write().unwrap_or_else(|e| e.into_inner()) = on;
    }

    /// Trim switch for a client. Each `ClientKind` has its own independent
    /// flag: Claude Code uses the `trim_enabled` master (kept under that name
    /// so existing settings.json files keep working), OpenAI uses
    /// `trim_openai_enabled`, Codex uses `trim_codex_enabled`.
    pub fn trim_enabled_for(&self, client: ClientKind) -> bool {
        let lock = match client {
            ClientKind::ClaudeCode => &self.trim_enabled,
            ClientKind::Codex => &self.trim_codex_enabled,
            ClientKind::OpenAi => &self.trim_openai_enabled,
        };
        *lock.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Set a client's trim switch. Writes only that client's flag — the other
    /// two are never touched, which is the whole point of the per-client split.
    pub fn set_trim_enabled_for(&self, client: ClientKind, on: bool) {
        let lock = match client {
            ClientKind::ClaudeCode => &self.trim_enabled,
            ClientKind::Codex => &self.trim_codex_enabled,
            ClientKind::OpenAi => &self.trim_openai_enabled,
        };
        *lock.write().unwrap_or_else(|e| e.into_inner()) = on;
    }

    /// Snapshot current state back into a Config (for persisting changes).
    pub fn to_config(&self) -> Config {
        Config {
            anthropic_key: self
                .anthropic_key
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            upstream_base_url: self
                .upstream_base_url
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            upstream_key: self
                .upstream_key
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            opus_target: self
                .opus_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_str()
                .to_string(),
            sonnet_target: self
                .sonnet_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_str()
                .to_string(),
            haiku_target: self
                .haiku_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_str()
                .to_string(),
            fable_target: self
                .fable_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_str()
                .to_string(),
            codex_target: self.codex_target().as_str().to_string(),
            log_level: self
                .log_level
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_str()
                .to_string(),
            opus_downgrade_enabled: *self
                .opus_downgrade_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            sonnet_downgrade_enabled: *self
                .sonnet_downgrade_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_enabled: *self.trim_enabled.read().unwrap_or_else(|e| e.into_inner()),
            client_claude_code_enabled: *self
                .client_claude_code_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            client_codex_enabled: *self
                .client_codex_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            client_openai_enabled: *self
                .client_openai_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_codex_enabled: *self
                .trim_codex_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_openai_enabled: *self
                .trim_openai_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            corpus_capture: *self
                .corpus_capture_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            db_log_enabled: self.is_db_log_enabled(),
            trim_tool_desc_chars: *self
                .trim_tool_desc_chars
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_toolresult_head: *self
                .trim_toolresult_head
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_head_haiku: *self
                .trim_head_haiku
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_head_harness: *self
                .trim_head_harness
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_toolresult_tail: *self
                .trim_toolresult_tail
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            retry_enabled: *self.retry_enabled.read().unwrap_or_else(|e| e.into_inner()),
            retry_max_attempts: *self
                .retry_max_attempts
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            retry_base_delay_ms: *self
                .retry_base_delay_ms
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            retry_max_delay_ms: *self
                .retry_max_delay_ms
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            throttle_enabled: *self
                .throttle_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            throttle_rate_per_sec: *self
                .throttle_rate_per_sec
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            throttle_burst: *self
                .throttle_burst
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_ws_enabled: *self
                .trim_ws_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_free_target: self
                .trim_free_target
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            trim_strip_thinking: *self
                .trim_strip_thinking
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_fence_requires_code: *self
                .trim_fence_requires_code
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            trim_arrow_density_min: *self
                .trim_arrow_density_min
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            corpus_max_rows: self.corpus_max_rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_kind_slot_round_trips_parse() {
        for kind in ClientKind::all() {
            assert_eq!(ClientKind::parse(kind.slot()), Some(kind));
        }
        // Parse is case-insensitive and whitespace-tolerant.
        assert_eq!(
            ClientKind::parse("CLAUDE_CODE"),
            Some(ClientKind::ClaudeCode)
        );
        assert_eq!(ClientKind::parse(" Codex "), Some(ClientKind::Codex));
    }

    #[test]
    fn client_kind_parse_rejects_garbage() {
        assert_eq!(ClientKind::parse(""), None);
        assert_eq!(ClientKind::parse("claude"), None);
        assert_eq!(ClientKind::parse("anthropic"), None);
        assert_eq!(ClientKind::parse("openai-messages"), None);
        assert_eq!(ClientKind::parse("gpt-5"), None);
    }

    #[test]
    fn client_kind_slots_are_stable() {
        assert_eq!(ClientKind::ClaudeCode.slot(), "claude_code");
        assert_eq!(ClientKind::Codex.slot(), "codex");
        assert_eq!(ClientKind::OpenAi.slot(), "openai");
        assert_eq!(ClientKind::all().len(), 3);
        assert_eq!(ClientKind::ClaudeCode.label(), "Claude Code");
        assert_eq!(ClientKind::Codex.label(), "Codex");
        assert_eq!(ClientKind::OpenAi.label(), "OpenAI");
        // The trace table shortens only the one name that does not fit.
        assert_eq!(ClientKind::ClaudeCode.short_label(), "CC");
        assert_eq!(ClientKind::Codex.short_label(), "Codex");
        assert_eq!(ClientKind::OpenAi.short_label(), "OpenAI");
    }

    #[test]
    fn wire_api_slots() {
        assert_eq!(WireApi::AnthropicMessages.slot(), "anthropic_messages");
        assert_eq!(WireApi::ChatCompletions.slot(), "chat_completions");
        assert_eq!(WireApi::Responses.slot(), "responses");
    }

    #[test]
    fn trim_enabled_for_returns_independent_flags() {
        let state = AppState::from_config(&Config::default());
        // Each client starts from its own flag. Defaults are all off.
        for client in ClientKind::all() {
            assert!(!state.trim_enabled_for(client));
        }
        // The returned flag is exactly the one set for that client, never a
        // shared master.
        state.set_trim_enabled_for(ClientKind::ClaudeCode, true);
        state.set_trim_enabled_for(ClientKind::Codex, true);
        state.set_trim_enabled_for(ClientKind::OpenAi, true);
        for client in ClientKind::all() {
            assert!(state.trim_enabled_for(client));
        }
        state.set_trim_enabled_for(ClientKind::ClaudeCode, false);
        assert!(!state.trim_enabled_for(ClientKind::ClaudeCode));
        assert!(state.trim_enabled_for(ClientKind::Codex));
        assert!(state.trim_enabled_for(ClientKind::OpenAi));
    }

    #[test]
    fn set_trim_enabled_for_does_not_disturb_other_clients() {
        let state = AppState::from_config(&Config::default());
        // Give every client a distinct starting state.
        state.set_trim_enabled_for(ClientKind::ClaudeCode, true);
        state.set_trim_enabled_for(ClientKind::Codex, false);
        state.set_trim_enabled_for(ClientKind::OpenAi, true);

        // Flipping ONE client leaves the other two exactly as they were.
        state.set_trim_enabled_for(ClientKind::Codex, true);
        assert!(state.trim_enabled_for(ClientKind::ClaudeCode));
        assert!(state.trim_enabled_for(ClientKind::Codex));
        assert!(state.trim_enabled_for(ClientKind::OpenAi));

        state.set_trim_enabled_for(ClientKind::ClaudeCode, false);
        assert!(!state.trim_enabled_for(ClientKind::ClaudeCode));
        assert!(state.trim_enabled_for(ClientKind::Codex));
        assert!(state.trim_enabled_for(ClientKind::OpenAi));

        state.set_trim_enabled_for(ClientKind::OpenAi, false);
        assert!(!state.trim_enabled_for(ClientKind::ClaudeCode));
        assert!(state.trim_enabled_for(ClientKind::Codex));
        assert!(!state.trim_enabled_for(ClientKind::OpenAi));
    }

    #[test]
    fn client_enabled_round_trips() {
        let state = AppState::from_config(&Config::default());
        // All three default on — a false default would kill existing routes
        // for users whose settings.json predates these keys.
        for client in ClientKind::all() {
            assert!(state.client_enabled(client));
        }
        state.set_client_enabled(ClientKind::Codex, false);
        assert!(!state.client_enabled(ClientKind::Codex));
        assert!(state.client_enabled(ClientKind::ClaudeCode));
        assert!(state.client_enabled(ClientKind::OpenAi));
        state.set_client_enabled(ClientKind::Codex, true);
        assert!(state.client_enabled(ClientKind::Codex));
    }
}
