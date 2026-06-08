use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;

/// Which Claude tier an incoming request maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `opus` → official Anthropic passthrough.
    Opus,
    /// `sonnet` → upstream gateway with the configured `sonnet_model`.
    Sonnet,
    /// `haiku` → upstream gateway with the configured `haiku_model`.
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
            // sonnet and anything unknown default to the sonnet slot
            Self::Sonnet
        }
    }
}

/// Shared application state. All fields are runtime-mutable so models and keys
/// can be switched without restarting Claude Code.
pub struct AppState {
    /// Anthropic API key (for opus passthrough).
    pub anthropic_key: RwLock<String>,
    /// Base URL of the OpenAI-compatible upstream (includes `/v1`).
    pub upstream_base_url: RwLock<String>,
    /// Bearer key for the upstream.
    pub upstream_key: RwLock<String>,
    /// Upstream model id for the sonnet slot.
    pub sonnet_model: RwLock<String>,
    /// Upstream model id for the haiku slot.
    pub haiku_model: RwLock<String>,
    /// Whether to dump request/response bodies for debugging.
    pub debug_dump: RwLock<bool>,
}

impl AppState {
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            anthropic_key: RwLock::new(cfg.anthropic_key.clone()),
            upstream_base_url: RwLock::new(cfg.upstream_base_url.clone()),
            upstream_key: RwLock::new(cfg.upstream_key.clone()),
            sonnet_model: RwLock::new(cfg.sonnet_model.clone()),
            haiku_model: RwLock::new(cfg.haiku_model.clone()),
            debug_dump: RwLock::new(false),
        })
    }

    /// Snapshot current state back into a Config (for persisting changes).
    pub async fn to_config(&self) -> Config {
        Config {
            anthropic_key: self.anthropic_key.read().await.clone(),
            upstream_base_url: self.upstream_base_url.read().await.clone(),
            upstream_key: self.upstream_key.read().await.clone(),
            sonnet_model: self.sonnet_model.read().await.clone(),
            haiku_model: self.haiku_model.read().await.clone(),
        }
    }
}
