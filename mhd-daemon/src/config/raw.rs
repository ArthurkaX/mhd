use serde::Deserialize;

/// Raw TOML binding entry.
#[derive(Debug, Deserialize)]
pub struct RawBinding {
    pub trigger: String,
    pub action: String,
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub keys: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub target_scheme: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

/// Raw TOML `[blackbox]` section.
#[cfg(feature = "blackbox")]
#[derive(Debug, Deserialize)]
pub struct RawBlackbox {
    /// Enable behavioural logging. Default: `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Seconds of inactivity before a session is considered ended.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
    /// End active sessions when the Windows session is locked. Default: `true`.
    #[serde(default)]
    pub track_locks: Option<bool>,
    /// End active sessions when Windows suspends. Default: `true`.
    #[serde(default)]
    pub track_suspend: Option<bool>,
    /// Redact window titles containing any of these substrings. Default: disabled.
    #[serde(default)]
    pub window_title_filter: Option<Vec<String>>,
}

/// Raw TOML `[[llm_proxy.model]]` entry — one selectable alternative model.
#[derive(Debug, Deserialize)]
pub struct RawLlmModel {
    /// Short display label shown in the selector (e.g. "DeepSeek V4 Pro").
    pub name: String,
    /// Upstream model id sent to the gateway (e.g. "sva-opencode/deepseek-v4-pro").
    pub id: String,
}

/// Raw TOML `[llm_proxy]` section.
#[derive(Debug, Deserialize)]
pub struct RawLlmProxy {
    /// Launch the proxy automatically when the daemon starts. Default: `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Port the proxy listens on. Default: `3456`.
    #[serde(default)]
    pub port: Option<u16>,
    /// OpenAI-compatible upstream gateway base URL (includes `/v1`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bearer key for the upstream gateway.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional Anthropic API key (for native passthrough when not using OAuth).
    #[serde(default)]
    pub anthropic_key: Option<String>,
    /// Default routing target for each tier: "native" or an upstream model id.
    #[serde(default)]
    pub opus: Option<String>,
    #[serde(default)]
    pub sonnet: Option<String>,
    #[serde(default)]
    pub haiku: Option<String>,
    /// Selectable alternative models (the shared pool for all tiers).
    #[serde(default)]
    pub model: Vec<RawLlmModel>,
}

/// Raw TOML `[quicknote]` section.
#[derive(Debug, Deserialize)]
pub struct RawQuickNote {
    /// Enable quick note. Default: `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Directory for saved notes. Default: `~/.config/mhd/notes/`.
    #[serde(default)]
    pub notes_dir: Option<String>,
}

/// Raw TOML `[quickdraw]` section.
#[derive(Debug, Deserialize)]
pub struct RawQuickDraw {
    /// Directory for saved drawings. Default: `~/.config/mhd/screenshots/`.
    #[serde(default)]
    pub draw_dir: Option<String>,
}

/// Top-level TOML config structure.
#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub active_scheme: Option<String>,
    #[serde(default)]
    pub theme: Option<String>,
    /// Volume adjustment step for `media_volume_up` / `media_volume_down`.
    #[serde(default)]
    pub volume_step: Option<u32>,
    /// Whether to autostart mhd at user logon (via scheduled task).
    #[serde(default)]
    pub autostart: Option<bool>,
    /// Behavioural logger.
    #[cfg(feature = "blackbox")]
    #[serde(default)]
    pub blackbox: Option<RawBlackbox>,
    #[serde(default)]
    pub quicknote: Option<RawQuickNote>,
    #[serde(default)]
    pub quickdraw: Option<RawQuickDraw>,
    /// LLM proxy integration (model routing for Claude Code).
    #[serde(default)]
    pub llm_proxy: Option<RawLlmProxy>,
    /// Ordered list of power plan names for `switch_power_plan` with target="next".
    #[serde(default)]
    pub power_plans: Vec<String>,
    #[serde(default)]
    pub binding: Vec<RawBinding>,
}
