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
    /// Provider name (must match a RawProvider.name). Empty = migrated to the
    /// first provider at parse time (back-compat with the old single-endpoint
    /// format, where models had no provider).
    #[serde(default)]
    pub provider: String,
    /// Upstream model id sent to the gateway.
    pub id: String,
    /// Display name shown in selectors. Falls back to id.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Deprecated: old config used `name` for the display name. Migrated to
    /// `display_name` at parse time when the latter is absent.
    #[serde(default)]
    pub name: Option<String>,
    /// Tier tags: "opus", "sonnet", "haiku", "fable".
    /// Empty = hidden from quick-switch menus.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Raw TOML `[[llm_proxy.provider]]` entry — one upstream provider.
#[derive(Debug, Clone, Deserialize)]
pub struct RawProvider {
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub api_key: String,
}

/// Raw TOML `[llm_proxy]` section.
#[derive(Debug, Deserialize)]
pub struct RawLlmProxy {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Debug log level: "none" | "minimal" | "maximal".
    #[serde(default)]
    pub log_level: Option<String>,
    /// Ordered list of upstream providers (OpenAI-compatible).
    #[serde(default)]
    pub provider: Vec<RawProvider>,
    /// Optional Anthropic API key (for native passthrough when not using OAuth).
    #[serde(default)]
    pub anthropic_key: Option<String>,
    /// Default routing target per tier: "native" or upstream model id.
    #[serde(default)]
    pub opus: Option<String>,
    #[serde(default)]
    pub sonnet: Option<String>,
    #[serde(default)]
    pub haiku: Option<String>,
    #[serde(default)]
    pub fable: Option<String>,
    /// Selectable alternative models (shared pool).
    #[serde(default)]
    pub model: Vec<RawLlmModel>,
    // ── Deprecated fields (migrated to provider[0] at parse time) ──
    /// Deprecated: use `[[llm_proxy.provider]]` instead.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Deprecated: use `[[llm_proxy.provider]]` api_key instead.
    #[serde(default)]
    pub api_key: Option<String>,
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
