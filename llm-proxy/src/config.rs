//! Persistent configuration for llm-proxy.
//!
//! Lives at `~/.config/mhd/llm-proxy/config.toml` (on Windows the home dir is
//! `%USERPROFILE%`). Holds API keys and the model routing table. If the file is
//! missing on startup it is created with defaults so the user has a template to
//! edit.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Anthropic API key — used for `opus`, which always passes through to the
    /// official Anthropic API.
    #[serde(default)]
    pub anthropic_key: String,

    /// Base URL of the OpenAI-compatible upstream (SVA / Bifrost gateway) used
    /// for `sonnet` and `haiku`. Should include the `/v1` suffix.
    #[serde(default = "default_upstream_base_url")]
    pub upstream_base_url: String,

    /// Bearer key for the upstream gateway.
    #[serde(default)]
    pub upstream_key: String,

    /// Routing target for the `opus` tier: `native` (Anthropic) or an upstream
    /// model id. Switchable at runtime via `GET /set_model/opus?id=...`.
    #[serde(default = "default_opus_target")]
    pub opus_target: String,

    /// Routing target for the `sonnet` tier.
    #[serde(default = "default_sonnet_target")]
    pub sonnet_target: String,

    /// Routing target for the `haiku` tier.
    #[serde(default = "default_haiku_target")]
    pub haiku_target: String,
}

fn default_upstream_base_url() -> String {
    "http://89.22.226.188:8080/v1".to_string()
}
fn default_opus_target() -> String {
    "native".to_string()
}
fn default_sonnet_target() -> String {
    "sva-opencode/deepseek-v4-pro".to_string()
}
fn default_haiku_target() -> String {
    "sva-opencode/deepseek-v4-flash".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            anthropic_key: String::new(),
            upstream_base_url: default_upstream_base_url(),
            upstream_key: String::new(),
            opus_target: default_opus_target(),
            sonnet_target: default_sonnet_target(),
            haiku_target: default_haiku_target(),
        }
    }
}

/// `~/.config/mhd/llm-proxy/`
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("mhd")
        .join("llm-proxy")
}

/// `~/.config/mhd/llm-proxy/config.toml`
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Load config from disk. If the file does not exist, write a default template
/// and return it.
pub fn load() -> anyhow::Result<Config> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let cfg: Config = toml::from_str(&s)?;
            Ok(cfg)
        }
        Err(_) => {
            let cfg = Config::default();
            save(&cfg)?;
            tracing::info!("Created default config at {}", path.display());
            Ok(cfg)
        }
    }
}

/// Persist config to disk, creating the directory if needed.
pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let s = toml::to_string_pretty(cfg)?;
    std::fs::write(config_path(), s)?;
    Ok(())
}
