use std::sync::{Arc, RwLock};

use crate::config::Config;

/// Which Claude tier an incoming request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Opus,
    Sonnet,
    Haiku,
}

impl Tier {
    /// Classify by the model id Claude Code sends.
    pub fn from_model(model: &str) -> Self {
        if model.contains("opus") {
            Self::Opus
        } else if model.contains("haiku") {
            Self::Haiku
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
        }
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
    pub debug_dump: RwLock<bool>,
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
            debug_dump: RwLock::new(false),
        })
    }

    /// Current target for a tier.
    pub fn target_for(&self, tier: Tier) -> Target {
        match tier {
            Tier::Opus => self.opus_target.read().unwrap().clone(),
            Tier::Sonnet => self.sonnet_target.read().unwrap().clone(),
            Tier::Haiku => self.haiku_target.read().unwrap().clone(),
        }
    }

    /// Set a tier's target by slot name ("opus"/"sonnet"/"haiku"). Returns false
    /// for an unknown slot.
    pub fn set_target(&self, slot: &str, target: Target) -> bool {
        match slot {
            "opus" => *self.opus_target.write().unwrap() = target,
            "sonnet" => *self.sonnet_target.write().unwrap() = target,
            "haiku" => *self.haiku_target.write().unwrap() = target,
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
        }
    }
}
