use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::config::Config;

/// Which Claude tier an incoming request belongs to.
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

/// How verbose the proxy's debug logging should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DebugLevel {
    #[default]
    None,
    /// Errors only.
    Minimal,
    /// Full session dump including request/response bodies.
    Maximal,
}

impl DebugLevel {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "minimal" => Self::Minimal,
            "maximal" | "max" | "full" => Self::Maximal,
            _ => Self::None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Maximal => "maximal",
        }
    }

    /// True if we should dump full request/response bodies.
    pub fn dump_bodies(&self) -> bool {
        matches!(self, Self::Maximal)
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
    /// Target for downgraded opus (model id string).
    pub opus_downgrade_target: RwLock<String>,
    /// Shared HTTP client — reused across requests so connections (and TLS
    /// sessions) are pooled. Creating a fresh `reqwest::Client` per request
    /// defeats keep-alive and serializes parallel load behind new handshakes.
    pub http: reqwest::Client,
    /// Monotonic request id, for correlating log lines.
    pub req_seq: AtomicU64,
    /// Number of upstream requests currently in flight (observability only).
    pub inflight: AtomicU64,
    /// Path to the proxy's own log file (`~/.config/mhd/llm-proxy/proxy.log`).
    /// Timing/concurrency lines go here so they're visible regardless of how the
    /// host process redirects stderr.
    pub log_path: PathBuf,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Arc<Self> {
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
            opus_downgrade_target: RwLock::new(cfg.opus_downgrade_target.clone()),
            http: reqwest::Client::new(),
            req_seq: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            log_path: crate::config::config_dir().join("proxy.log"),
        })
    }

    /// Append a line to the proxy log file. Best-effort: errors are swallowed so
    /// logging never affects request handling.
    pub fn log_line(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{msg}");
        }
    }

    /// Allocate the next request id.
    pub fn next_req_id(&self) -> u64 {
        self.req_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Current target for a tier.
    pub fn target_for(&self, tier: Tier) -> Target {
        match tier {
            Tier::Opus => self.opus_target.read().unwrap().clone(),
            Tier::Sonnet => self.sonnet_target.read().unwrap().clone(),
            Tier::Haiku => self.haiku_target.read().unwrap().clone(),
            Tier::Fable => self.fable_target.read().unwrap().clone(),
        }
    }

    /// Set a tier's target by slot name ("opus"/"sonnet"/"haiku"/"fable").
    /// Returns false for an unknown slot.
    pub fn set_target(&self, slot: &str, target: Target) -> bool {
        match slot {
            "opus" => *self.opus_target.write().unwrap() = target,
            "sonnet" => *self.sonnet_target.write().unwrap() = target,
            "haiku" => *self.haiku_target.write().unwrap() = target,
            "fable" => *self.fable_target.write().unwrap() = target,
            _ => return false,
        }
        true
    }

    /// Snapshot current state back into a Config (for persisting changes).
    pub fn to_config(&self) -> Config {
        Config {
            anthropic_key: self.anthropic_key.read().unwrap().clone(),
            upstream_base_url: self.upstream_base_url.read().unwrap().clone(),
            upstream_key: self.upstream_key.read().unwrap().clone(),
            opus_target: self.opus_target.read().unwrap().as_str().to_string(),
            sonnet_target: self.sonnet_target.read().unwrap().as_str().to_string(),
            haiku_target: self.haiku_target.read().unwrap().as_str().to_string(),
            fable_target: self.fable_target.read().unwrap().as_str().to_string(),
            log_level: self.log_level.read().unwrap().as_str().to_string(),
            opus_downgrade_enabled: *self.opus_downgrade_enabled.read().unwrap(),
            opus_downgrade_target: self.opus_downgrade_target.read().unwrap().clone(),
        }
    }
}
