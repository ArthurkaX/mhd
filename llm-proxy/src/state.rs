use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Config;

/// Which tier an incoming request belongs to — one of the four Claude tiers,
/// or `OpenAi` for a raw `/v1/chat/completions` passthrough (Zed and other
/// OpenAI-native clients). The `OpenAi` variant carries no routing/downgrade
/// semantics; it exists only so passthrough requests show up in the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    /// Raw OpenAI-compatible passthrough (no Claude tier / no downgrade logic).
    OpenAi,
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
            Self::OpenAi => "openai",
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

/// A single proxy routing decision recorded for the live trace overlay.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    pub seq: u64,
    pub tier: Tier,
    pub effective_tier: Tier,
    pub target: String,
    pub model: String,
    pub downgraded: bool,
    pub reason: String,
    /// Fresh (uncached) prompt tokens billed at 1× (0 until response arrives).
    pub input_tokens: u64,
    /// Output tokens reported in the response (0 until response arrives).
    pub output_tokens: u64,
    /// Tokens served from the prompt cache (cache_read_input_tokens / cached_tokens).
    pub cache_read_tokens: u64,
    /// Tokens written to the prompt cache this turn (Anthropic only; 0 for OpenAI).
    pub cache_creation_tokens: u64,
    /// True if llmtrim was applied to this request.
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
    pub log_level: RwLock<DebugLevel>,
    /// Opus downgrade when no thinking.
    pub opus_downgrade_enabled: RwLock<bool>,
    /// Sonnet downgrade when no thinking.
    pub sonnet_downgrade_enabled: RwLock<bool>,
    /// Shared HTTP client — reused across requests so connections (and TLS
    /// sessions) are pooled. Creating a fresh `reqwest::Client` per request
    /// defeats keep-alive and serializes parallel load behind new handshakes.
    pub http: reqwest::Client,
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

    /// Master switch for llmtrim request compression.
    pub trim_enabled: RwLock<bool>,

    /// Stable id for this daemon run (epoch-millis at construction). Used to
    /// group all requests from one process lifetime in the `requests` table.
    pub run_id: u64,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Arc::new(Self {
            anthropic_key: RwLock::new(cfg.anthropic_key.clone()),
            upstream_base_url: RwLock::new(cfg.upstream_base_url.clone()),
            upstream_key: RwLock::new(cfg.upstream_key.clone()),
            opus_target: RwLock::new(Target::parse(&cfg.opus_target)),
            sonnet_target: RwLock::new(Target::parse(&cfg.sonnet_target)),
            haiku_target: RwLock::new(Target::parse(&cfg.haiku_target)),
            fable_target: RwLock::new(Target::parse(&cfg.fable_target)),
            log_level: RwLock::new(DebugLevel::parse(&cfg.log_level)),
            opus_downgrade_enabled: RwLock::new(cfg.opus_downgrade_enabled),
            sonnet_downgrade_enabled: RwLock::new(cfg.sonnet_downgrade_enabled),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to build HTTP client"),
            req_seq: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            db_log: Mutex::new(None),
            trace: RwLock::new(VecDeque::with_capacity(MAX_TRACE_ENTRIES)),
            vision_trace: RwLock::new(VecDeque::with_capacity(MAX_VISION_TRACE_ENTRIES)),
            session_last_ts: RwLock::new(HashMap::new()),
            trim_enabled: RwLock::new(cfg.trim_enabled),
            run_id,
        })
    }

    /// Append a text line to the database log.
    /// Best-effort: errors are swallowed.
    pub fn log_line(&self, msg: &str) {
        if let Ok(guard) = self.db_log.lock() {
            if let Some(ref db) = *guard {
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
    }

    /// Log a structured event with typed fields.
    /// Timestamp is automatic. Writes to SQLite.
    pub fn log_event(&self, event: crate::db_log::LogEvent) {
        if let Ok(guard) = self.db_log.lock() {
            if let Some(ref db) = *guard {
                let ts = crate::providers::now_ms();
                db.insert(&ts, &event);
            }
        }
    }

    /// Enable or disable the database log.
    /// When enabling, the database is opened (and created if needed) lazily.
    pub fn set_db_log_enabled(&self, enabled: bool) {
        if enabled {
            let mut guard = self.db_log.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                let db_path = crate::config::config_dir().join("proxy.db");
                match crate::db_log::DbLog::open(&db_path) {
                    Ok(db) => {
                        *guard = Some(db);
                    }
                    Err(e) => {
                        eprintln!("mhd: failed to open proxy.db: {e}");
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
        if let Ok(guard) = self.db_log.lock() {
            if let Some(ref db) = *guard {
                let row = crate::db_log::RequestRow {
                    run_id: self.run_id,
                    seq: entry.seq,
                    ts_start: crate::providers::now_ms(),
                    tier: Some(entry.tier.slot().to_string()),
                    effective_tier: Some(entry.effective_tier.slot().to_string()),
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
                };
                db.insert_request(&row);
            }
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
    pub fn update_trace_tokens(
        &self,
        req_id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) {
        let mut trace = self.trace.write().unwrap_or_else(|e| e.into_inner());
        // Search from the back — the matching entry is almost certainly recent.
        for entry in trace.iter_mut().rev() {
            if entry.seq == req_id {
                entry.input_tokens = input_tokens;
                entry.output_tokens = output_tokens;
                entry.cache_read_tokens = cache_read_tokens;
                entry.cache_creation_tokens = cache_creation_tokens;
                break;
            }
        }
        drop(trace);
        if let Ok(guard) = self.db_log.lock() {
            if let Some(ref db) = *guard {
                db.update_request_completion(
                    self.run_id,
                    req_id,
                    &crate::providers::now_ms(),
                    None,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    None,
                    None,
                    None,
                );
            }
        }
    }

    /// Write a free-text note to the `notes` table in proxy.db.
    /// Best-effort: no-ops if the log is disabled or the write fails.
    pub fn log_note(&self, text: &str) {
        if let Ok(guard) = self.db_log.lock() {
            if let Some(ref db) = *guard {
                db.insert_note(&crate::providers::now_ms(), text);
            }
        }
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
            // OpenAI passthrough is never routed through tier targets; this
            // arm only satisfies exhaustiveness and is never reached.
            Tier::OpenAi => Target::Native,
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
            db_log_enabled: self.is_db_log_enabled(),
        }
    }
}
