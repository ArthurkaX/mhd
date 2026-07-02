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

    /// Master switch for native request compression.
    /// Default: off.
    #[serde(default)]
    pub trim_enabled: bool,

    /// Master switch for replay-corpus capture (stores full pre-trim request bodies in proxy.db). Default: off.
    #[serde(default)]
    pub corpus_capture: bool,

    /// Whether to write structured events to `proxy.db`.
    /// Default: on — the DB log is opened at proxy startup when true.
    #[serde(default = "default_db_log_enabled")]
    pub db_log_enabled: bool,

    /// Max Unicode chars kept per tool `description` in the native trim engine.
    #[serde(default = "default_trim_tool_desc_chars")]
    pub trim_tool_desc_chars: usize,

    /// Max chars kept as the head of a `tool_result` block (native engine).
    #[serde(default = "default_trim_toolresult_head")]
    pub trim_toolresult_head: usize,

    /// Max chars kept as the tail of a `tool_result` block (native engine).
    #[serde(default = "default_trim_toolresult_tail")]
    pub trim_toolresult_tail: usize,

    /// Master switch for native Anthropic retry on HTTP 429 / 529. Default: off
    /// (validate offline before enabling in production).
    #[serde(default = "default_retry_enabled")]
    pub retry_enabled: bool,

    /// Total attempts (including the first) for the native Anthropic retry loop.
    #[serde(default = "default_retry_max_attempts")]
    pub retry_max_attempts: usize,

    /// Base backoff delay in ms for the native Anthropic retry loop. Each retry
    /// waits `retry_base_delay_ms << attempt` (capped at `retry_max_delay_ms`),
    /// unless a `retry-after` response header overrides it.
    #[serde(default = "default_retry_base_delay_ms")]
    pub retry_base_delay_ms: u64,

    /// Cap in ms for a single backoff wait in the native Anthropic retry loop.
    #[serde(default = "default_retry_max_delay_ms")]
    pub retry_max_delay_ms: u64,
    /// Master switch for the outbound rate limiter (token-bucket throttle) on
    /// the native Anthropic path. Default: off — validate offline before
    /// enabling.
    #[serde(default = "default_throttle_enabled")]
    pub throttle_enabled: bool,

    /// Steady refill rate (requests/sec) for the token-bucket throttle. A burst
    /// in excess of `throttle_burst` is spread at this rate.
    #[serde(default = "default_throttle_rate_per_sec")]
    pub throttle_rate_per_sec: f64,

    /// Bucket capacity (max instantaneous burst) for the token-bucket throttle.
    /// Below this size a burst passes immediately; above it the outbound rate is
    /// capped at `throttle_rate_per_sec`.
    #[serde(default = "default_throttle_burst")]
    pub throttle_burst: f64,

    /// Master switch for whitespace compression in the native trim engine.
    /// Default: off — validate offline first.
    #[serde(default = "default_trim_ws_enabled")]
    pub trim_ws_enabled: bool,

    /// Designated free/cheap-tier target. Requests resolving to this exact
    /// target get the light (declutter-only) trim profile. Empty = disabled.
    #[serde(default = "default_trim_free_target")]
    pub trim_free_target: String,

    /// Strip thinking/redacted_thinking blocks from old assistant turns (native engine).
    /// Default: off.
    #[serde(default = "default_trim_strip_thinking")]
    pub trim_strip_thinking: bool,

    /// When true (default), Layer 2 fence protection in the native trim engine
    /// only fires when the fenced content also looks code-like. Set false to
    /// revert to the old behavior (fence alone protects).
    #[serde(default = "default_trim_fence_requires_code")]
    pub trim_fence_requires_code: bool,

    /// Minimum glyph density for the arrow-driven diagram detector in the native
    /// trim engine. Default: 0.01 (1%). Real diagrams have median density ~0.022;
    /// stray arrows in noisy text have density ~0.0004.
    #[serde(default = "default_trim_arrow_density_min")]
    pub trim_arrow_density_min: f64,

    /// Maximum number of rows to keep in the `request_bodies` corpus table.
    /// Oldest rows are pruned after each insert. 0 = unlimited.
    /// Default: 5000.
    #[serde(default = "default_corpus_max_rows")]
    pub corpus_max_rows: usize,
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
            corpus_capture: false,
            db_log_enabled: default_db_log_enabled(),
            trim_tool_desc_chars: default_trim_tool_desc_chars(),
            trim_toolresult_head: default_trim_toolresult_head(),
            trim_toolresult_tail: default_trim_toolresult_tail(),
            retry_enabled: default_retry_enabled(),
            retry_max_attempts: default_retry_max_attempts(),
            retry_base_delay_ms: default_retry_base_delay_ms(),
            retry_max_delay_ms: default_retry_max_delay_ms(),
    throttle_enabled: default_throttle_enabled(),
    throttle_rate_per_sec: default_throttle_rate_per_sec(),
    throttle_burst: default_throttle_burst(),
            trim_ws_enabled: default_trim_ws_enabled(),
            trim_free_target: default_trim_free_target(),
            trim_strip_thinking: default_trim_strip_thinking(),
            trim_fence_requires_code: default_trim_fence_requires_code(),
            trim_arrow_density_min: default_trim_arrow_density_min(),
            corpus_max_rows: default_corpus_max_rows(),
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
    pub trim_enabled: bool,
    pub corpus_capture: bool,
    /// Whether to write structured events to `proxy.db`.
    pub db_log_enabled: bool,
    /// Max Unicode chars per tool description (native engine).
    pub trim_tool_desc_chars: usize,
    /// Max chars for the head of a tool_result block (native engine).
    pub trim_toolresult_head: usize,
    /// Max chars for the tail of a tool_result block (native engine).
    pub trim_toolresult_tail: usize,
    /// Master switch for native Anthropic retry on HTTP 429 / 529.
    pub retry_enabled: bool,
    /// Total attempts (including the first) for the native Anthropic retry loop.
    pub retry_max_attempts: usize,
    /// Base backoff delay in ms for the native Anthropic retry loop.
    pub retry_base_delay_ms: u64,
    /// Cap in ms for a single backoff wait in the native Anthropic retry loop.
    pub retry_max_delay_ms: u64,
    /// Master switch for the outbound rate limiter (token-bucket throttle).
    pub throttle_enabled: bool,
    /// Steady refill rate (requests/sec) for the token-bucket throttle.
    pub throttle_rate_per_sec: f64,
    /// Bucket capacity (max instantaneous burst) for the token-bucket throttle.
    pub throttle_burst: f64,
    /// Master switch for whitespace compression in the native trim engine.
    pub trim_ws_enabled: bool,
    /// Designated free/cheap-tier target. Empty = disabled.
    pub trim_free_target: String,
    /// Strip thinking/redacted_thinking from old assistant turns (native engine).
    pub trim_strip_thinking: bool,
    /// When true, Layer 2 fence protection only fires when fenced content looks code-like.
    pub trim_fence_requires_code: bool,
    /// Minimum glyph density for the arrow-driven diagram detector (default 0.01).
    pub trim_arrow_density_min: f64,
    /// Maximum rows to keep in the `request_bodies` corpus table (0 = unlimited).
    pub corpus_max_rows: usize,
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
            corpus_capture: settings.corpus_capture,
            db_log_enabled: settings.db_log_enabled,
            trim_tool_desc_chars: settings.trim_tool_desc_chars,
            trim_toolresult_head: settings.trim_toolresult_head,
            trim_toolresult_tail: settings.trim_toolresult_tail,
            retry_enabled: settings.retry_enabled,
            retry_max_attempts: settings.retry_max_attempts,
            retry_base_delay_ms: settings.retry_base_delay_ms,
            retry_max_delay_ms: settings.retry_max_delay_ms,
    throttle_enabled: settings.throttle_enabled,
    throttle_rate_per_sec: settings.throttle_rate_per_sec,
    throttle_burst: settings.throttle_burst,
            trim_ws_enabled: settings.trim_ws_enabled,
            trim_free_target: settings.trim_free_target.clone(),
            trim_strip_thinking: settings.trim_strip_thinking,
            trim_fence_requires_code: settings.trim_fence_requires_code,
            trim_arrow_density_min: settings.trim_arrow_density_min,
            corpus_max_rows: settings.corpus_max_rows,
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
            corpus_capture: self.corpus_capture,
            db_log_enabled: self.db_log_enabled,
            trim_tool_desc_chars: self.trim_tool_desc_chars,
            trim_toolresult_head: self.trim_toolresult_head,
            trim_toolresult_tail: self.trim_toolresult_tail,
            retry_enabled: self.retry_enabled,
            retry_max_attempts: self.retry_max_attempts,
            retry_base_delay_ms: self.retry_base_delay_ms,
            retry_max_delay_ms: self.retry_max_delay_ms,
    throttle_enabled: self.throttle_enabled,
    throttle_rate_per_sec: self.throttle_rate_per_sec,
    throttle_burst: self.throttle_burst,
            trim_ws_enabled: self.trim_ws_enabled,
            trim_free_target: self.trim_free_target.clone(),
            trim_strip_thinking: self.trim_strip_thinking,
            trim_fence_requires_code: self.trim_fence_requires_code,
            trim_arrow_density_min: self.trim_arrow_density_min,
            corpus_max_rows: self.corpus_max_rows,
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
    "maximal".to_string()
}

fn default_db_log_enabled() -> bool {
    true
}

fn default_trim_tool_desc_chars() -> usize {
    100
}

fn default_trim_toolresult_head() -> usize {
    3000
}

fn default_trim_toolresult_tail() -> usize {
    1000
}

fn default_retry_enabled() -> bool {
    false
}

fn default_retry_max_attempts() -> usize {
    3
}

fn default_retry_base_delay_ms() -> u64 {
    500
}

fn default_retry_max_delay_ms() -> u64 {
    8000
}

fn default_throttle_enabled() -> bool {
 false
}

fn default_throttle_rate_per_sec() -> f64 {
 10.0
}

fn default_throttle_burst() -> f64 {
 10.0
}

fn default_trim_ws_enabled() -> bool {
    false
}

fn default_trim_free_target() -> String {
    String::new()
}

fn default_trim_strip_thinking() -> bool {
    false
}

fn default_trim_fence_requires_code() -> bool {
    true
}

fn default_trim_arrow_density_min() -> f64 {
    0.01
}

fn default_corpus_max_rows() -> usize {
    5000
}

fn default_vision_prompt() -> String {
    "Analyze this screenshot and return the useful visible text. Preserve the \
        original language and structure. Return only the result, without commentary."
        .to_string()
}

fn default_bind_ip() -> String {
    "127.0.0.1".to_string()
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
    settings.db_log_enabled = cfg.db_log_enabled;
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
