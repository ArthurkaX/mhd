//! Persistent configuration for llm-proxy.
//!
//! Lives in `~/.config/mhd/llm-proxy/` and spans multiple JSON files:
//!
//! | File            | Contents                                         | Security                   |
//! |-----------------|--------------------------------------------------|----------------------------|
//! | `settings.json` | port, log_level, upstream_base_url, tier targets | Plain JSON                 |
//! | `secrets.json`  | anthropic_key, upstream_key                      | DPAPI-encrypted (Windows)  |
//! | `providers.json`| Provider definitions (name, endpoint)            | Plain JSON                 |
//! | `models.json`   | Model pool (id, display_name, tags)              | Plain JSON                 |
//!
//! If the directory is empty on startup, default files are created so the user
//! has a template to edit. If an old `config.toml` is found, it's migrated to
//! the new format automatically.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod secrets;
pub use secrets::Secrets;

// ── ModelRef (provider-qualified model reference) ─────────────────────

/// A provider-qualified model reference used by vision and other features
/// that select a single model rather than a routing tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

// ── Settings (plain JSON) ─────────────────────────────────────────────

/// Proxy settings that are safe to store in plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Whether the proxy is enabled (auto-start on daemon boot).
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Listening port.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Base URL of the OpenAI-compatible upstream gateway. Includes `/v1`.
    #[serde(default = "default_upstream_base_url")]
    pub upstream_base_url: String,

    /// Routing target for the `opus` tier.
    #[serde(default = "default_opus_target")]
    pub opus_target: String,

    /// Routing target for the `sonnet` tier.
    #[serde(default = "default_sonnet_target")]
    pub sonnet_target: String,

    /// Routing target for the `haiku` tier.
    #[serde(default = "default_haiku_target")]
    pub haiku_target: String,

    /// Routing target for the `fable` tier.
    #[serde(default = "default_fable_target")]
    pub fable_target: String,

    /// Debug log level: "none", "minimal", "maximal".
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Bind IP address.
    #[serde(default = "default_bind_ip")]
    pub bind_ip: String,

    /// When true, opus requests without `thinking` in betas are downgraded.
    #[serde(default)]
    pub opus_downgrade_enabled: bool,

    /// When true, sonnet requests without `thinking` in betas are downgraded.
    #[serde(default)]
    pub sonnet_downgrade_enabled: bool,

    /// Selected model for vision screenshot feature (provider-qualified).
    #[serde(default)]
    pub vision_model: Option<ModelRef>,

    /// Custom prompt sent with the screenshot to the vision model.
    #[serde(default = "default_vision_prompt")]
    pub vision_prompt: String,

    /// Master switch for llmtrim-powered request compression.
    #[serde(default)]
    pub trim_enabled: bool,

    /// llmtrim preset: "auto" picks agent/code/rag/aggressive per-request from
    /// the payload shape, which is correct for mixed clients (Claude Code, Zed,
    /// raw OpenAI) without knowing the client.
    #[serde(default = "default_trim_preset")]
    pub trim_preset: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            port: default_port(),
            upstream_base_url: default_upstream_base_url(),
            opus_target: default_opus_target(),
            sonnet_target: default_sonnet_target(),
            haiku_target: default_haiku_target(),
            fable_target: default_fable_target(),
            log_level: default_log_level(),
            bind_ip: default_bind_ip(),
            opus_downgrade_enabled: false,
            sonnet_downgrade_enabled: false,
            vision_model: None,
            vision_prompt: default_vision_prompt(),
            trim_enabled: false,
            trim_preset: default_trim_preset(),
        }
    }
}

// ── Providers (plain JSON) ────────────────────────────────────────────

/// An upstream provider (OpenAI-compatible gateway).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    pub name: String,
    pub endpoint: String,
}

// ── Models (plain JSON) ───────────────────────────────────────────────

/// A selectable alternative model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    /// Provider name (must match a Provider.name).
    pub provider: String,
    /// Upstream model id sent to the gateway.
    pub id: String,
    /// Display name shown in selectors. Falls back to id.
    #[serde(default)]
    pub display_name: String,
    /// Tier tags: "opus", "sonnet", "haiku", "fable". Empty = hidden from
    /// quick-switch menus.
    #[serde(default)]
    pub tags: Vec<String>,
}

// ── Combined Config (public API, backward compatible) ─────────────────

/// Combined proxy config – the public API for callers that construct a Config
/// directly (e.g. the daemon embedding the proxy).
///
/// See [`Settings`] and [`Secrets`] for the persistent-on-disk representation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Anthropic API key — used for native passthrough.
    pub anthropic_key: String,
    /// Base URL of the OpenAI-compatible upstream gateway.
    pub upstream_base_url: String,
    /// Bearer key for the upstream gateway.
    pub upstream_key: String,
    /// Routing target for `opus`.
    pub opus_target: String,
    /// Routing target for `sonnet`.
    pub sonnet_target: String,
    /// Routing target for `haiku`.
    pub haiku_target: String,
    /// Routing target for `fable`.
    pub fable_target: String,
    /// Debug log level.
    pub log_level: String,
    /// Opus downgrade when no thinking.
    pub opus_downgrade_enabled: bool,
    /// Sonnet downgrade when no thinking.
    pub sonnet_downgrade_enabled: bool,
    /// Enable llmtrim-powered request compression.
    pub trim_enabled: bool,
    /// llmtrim preset: "auto" | "agent" | "aggressive" | "code" | "rag" | "safe".
    pub trim_preset: String,
}

impl Config {
    /// Merge a [`Settings`] and [`Secrets`] into a combined [`Config`].
    pub fn from_settings_secrets(settings: &Settings, secrets: &Secrets) -> Self {
        Self {
            anthropic_key: secrets.anthropic_key.clone(),
            upstream_base_url: settings.upstream_base_url.clone(),
            upstream_key: secrets.upstream_key.clone(),
            opus_target: settings.opus_target.clone(),
            sonnet_target: settings.sonnet_target.clone(),
            haiku_target: settings.haiku_target.clone(),
            fable_target: settings.fable_target.clone(),
            log_level: settings.log_level.clone(),
            opus_downgrade_enabled: settings.opus_downgrade_enabled,
            sonnet_downgrade_enabled: settings.sonnet_downgrade_enabled,
            trim_enabled: settings.trim_enabled,
            trim_preset: settings.trim_preset.clone(),
        }
    }

    /// Split a [`Config`] into its [`Settings`] and [`Secrets`] parts.
    pub fn into_settings(&self) -> Settings {
        Settings {
            enabled: true,
            port: 0, // port is not part of Config (passed separately to start_embedded_with)
            upstream_base_url: self.upstream_base_url.clone(),
            opus_target: self.opus_target.clone(),
            sonnet_target: self.sonnet_target.clone(),
            haiku_target: self.haiku_target.clone(),
            fable_target: self.fable_target.clone(),
            log_level: self.log_level.clone(),
            bind_ip: String::new(),
            opus_downgrade_enabled: self.opus_downgrade_enabled,
            sonnet_downgrade_enabled: self.sonnet_downgrade_enabled,
            vision_model: None,
            vision_prompt: default_vision_prompt(),
            trim_enabled: self.trim_enabled,
            trim_preset: self.trim_preset.clone(),
        }
    }

    /// Extract the secrets portion.
    pub fn into_secrets(&self) -> Secrets {
        Secrets {
            anthropic_key: self.anthropic_key.clone(),
            upstream_key: self.upstream_key.clone(),
            provider_keys: std::collections::HashMap::new(),
        }
    }

    /// Load config from disk, with automatic migration from old TOML format.
    pub fn load() -> anyhow::Result<Config> {
        load()
    }

    /// Persist config to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        save(self)
    }
}

impl Default for Config {
    fn default() -> Self {
        let settings = Settings::default();
        let secrets = Secrets::default();
        Self::from_settings_secrets(&settings, &secrets)
    }
}

// ── Default values ────────────────────────────────────────────────────

fn default_enabled() -> bool {
    true
}

fn default_port() -> u16 {
    3456
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

fn default_fable_target() -> String {
    "native".to_string()
}

fn default_log_level() -> String {
    "none".to_string()
}

fn default_vision_prompt() -> String {
    "Analyze this screenshot and return the useful visible text. Preserve the \
        original language and structure. Return only the result, without commentary."
        .to_string()
}

fn default_bind_ip() -> String {
    "127.0.0.1".to_string()
}

fn default_trim_preset() -> String {
    // auto lets llmtrim pick agent/code/rag/aggressive per-request from the
    // payload shape — correct for mixed clients without knowing the client.
    "auto".to_string()
}

// ── Path helpers ──────────────────────────────────────────────────────

/// `~/.config/mhd/llm-proxy/`
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("mhd")
        .join("llm-proxy")
}

// ── Load / Save (public API) ──────────────────────────────────────────

/// Load config from disk. If the directory is empty, defaults are created.
/// If an old `config.toml` exists, it is migrated to the new JSON format.
pub fn load() -> anyhow::Result<Config> {
    let dir = config_dir();

    // ── Load settings + secrets ────────────────────────────────
    let settings = load_settings_from(&dir).unwrap_or_else(|_| {
        let s = Settings::default();
        // Silently create default file on first run.
        let _ = persist_settings(&dir, &s);
        s
    });

    let secrets = load_secrets_from(&dir).unwrap_or_else(|_| {
        let s = Secrets::default();
        let _ = persist_secrets(&dir, &s);
        s
    });

    Ok(Config::from_settings_secrets(&settings, &secrets))
}

/// Persist config to disk (writes settings.json + secrets.json).
///
/// Uses a read-modify-write for settings.json so that fields outside
/// [`Config`]'s ownership (port, enabled) survive the round-trip unchanged.
pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    // Start from the current on-disk settings to preserve port/enabled.
    let mut settings = load_settings_from(&dir).unwrap_or_else(|_| Settings::default());
    settings.upstream_base_url = cfg.upstream_base_url.clone();
    settings.opus_target = cfg.opus_target.clone();
    settings.sonnet_target = cfg.sonnet_target.clone();
    settings.haiku_target = cfg.haiku_target.clone();
    settings.fable_target = cfg.fable_target.clone();
    settings.log_level = cfg.log_level.clone();
    settings.opus_downgrade_enabled = cfg.opus_downgrade_enabled;
    settings.sonnet_downgrade_enabled = cfg.sonnet_downgrade_enabled;
    // vision_model is managed separately via save_settings/load_settings

    persist_settings(&dir, &settings)?;
    persist_secrets(&dir, &cfg.into_secrets())?;
    Ok(())
}

// ── Per-file load/save helpers ────────────────────────────────────────

fn settings_path(dir: &PathBuf) -> PathBuf {
    dir.join("settings.json")
}

fn secrets_path(dir: &PathBuf) -> PathBuf {
    dir.join("secrets.json")
}

fn providers_path(dir: &PathBuf) -> PathBuf {
    dir.join("providers.json")
}

fn models_path(dir: &PathBuf) -> PathBuf {
    dir.join("models.json")
}

fn persist_settings(dir: &PathBuf, settings: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let data = serde_json::to_string_pretty(settings)?;
    std::fs::write(settings_path(dir), data)?;
    Ok(())
}

// ── Provider-qualified endpoint resolution ────────────────────────────

/// A fully resolved model endpoint ready for use by the vision client.
#[derive(Debug, Clone)]
pub struct ResolvedModelEndpoint {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub api_key: String,
}

/// Resolve a [`ModelRef`] into a complete endpoint with URL and API key.
///
/// Looks up the provider by name, then the model by id, and retrieves the
/// API key from secrets (falling back to the global `upstream_key`).
pub fn resolve_model_endpoint(
    model_ref: &ModelRef,
    providers: &[Provider],
    secrets: &Secrets,
) -> anyhow::Result<ResolvedModelEndpoint> {
    let provider = providers
        .iter()
        .find(|p| p.name == model_ref.provider)
        .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", model_ref.provider))?;

    // Normalize endpoint: ensure it ends with /chat/completions
    let endpoint = normalize_endpoint(&provider.endpoint);

    // Resolve API key: first try per-provider key, then global upstream_key
    let api_key = secrets
        .provider_keys
        .get(&model_ref.provider)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if !secrets.upstream_key.is_empty() {
                Some(secrets.upstream_key.as_str())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!("API key not found for provider '{}'", model_ref.provider)
        })?;

    Ok(ResolvedModelEndpoint {
        provider: model_ref.provider.clone(),
        model: model_ref.model.clone(),
        endpoint,
        api_key: api_key.to_string(),
    })
}

/// Normalize a provider endpoint to end with exactly one `/chat/completions` suffix.
/// Strips trailing `/v1` or trailing slashes and appends `/chat/completions`.
///
/// Used by the Claude Code proxy path, where `upstream_base_url` includes `/v1`
/// and the proxy appends its own `/chat/completions`.
pub fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    let trimmed = trimmed.trim_end_matches("/v1");
    format!("{}/chat/completions", trimmed)
}

/// Normalize an endpoint for direct OpenAI-compatible vision requests.
///
/// Keeps an existing `/v1` segment (unlike [`normalize_endpoint`]) and appends
/// `/chat/completions` only if it is not already present. This prevents the
/// `405 Not Allowed` errors that happen when a provider such as Ollama Cloud
/// receives `https://ollama.com/chat/completions` instead of
/// `https://ollama.com/v1/chat/completions`.
pub fn normalize_vision_endpoint(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        return trimmed.to_string();
    }
    format!("{}/chat/completions", trimmed)
}

fn load_settings_from(dir: &PathBuf) -> anyhow::Result<Settings> {
    let data = std::fs::read_to_string(settings_path(dir))?;
    let settings: Settings = serde_json::from_str(&data)?;
    Ok(settings)
}

fn persist_secrets(dir: &PathBuf, s: &Secrets) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let data = self::secrets::seal(s);
    std::fs::write(secrets_path(dir), data)?;
    Ok(())
}

fn load_secrets_from(dir: &PathBuf) -> anyhow::Result<Secrets> {
    let data = std::fs::read(secrets_path(dir))?;
    self::secrets::unseal(&data).ok_or_else(|| anyhow::anyhow!("failed to decrypt secrets"))
}

// ── Standalone file loaders (for daemon use) ──────────────────────────

/// Save settings to `settings.json`.
pub fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    let dir = config_dir();
    persist_settings(&dir, settings)
}

/// Load settings only (faster when you don't need secrets).
pub fn load_settings() -> anyhow::Result<Settings> {
    let dir = config_dir();
    load_settings_from(&dir)
}

/// Save secrets to `secrets.json` (DPAPI-encrypted).
pub fn save_secrets(s: &Secrets) -> anyhow::Result<()> {
    let dir = config_dir();
    persist_secrets(&dir, s)
}

/// Load secrets only.
pub fn load_secrets() -> anyhow::Result<Secrets> {
    let dir = config_dir();
    load_secrets_from(&dir)
}

/// Load providers from `providers.json`. Returns empty vec if missing.
pub fn load_providers() -> anyhow::Result<Vec<Provider>> {
    let path = providers_path(&config_dir());
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(serde_json::from_str(&data)?)
}

/// Save providers to `providers.json`.
pub fn save_providers(providers: &[Provider]) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let data = serde_json::to_string_pretty(providers)?;
    std::fs::write(providers_path(&dir), data)?;
    Ok(())
}

/// Load models from `models.json`. Returns empty vec if missing.
pub fn load_models() -> anyhow::Result<Vec<Model>> {
    let path = models_path(&config_dir());
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    Ok(serde_json::from_str(&data)?)
}

/// Save models to `models.json`.
pub fn save_models(models: &[Model]) -> anyhow::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let data = serde_json::to_string_pretty(models)?;
    std::fs::write(models_path(&dir), data)?;
    Ok(())
}
