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
}

/// Raw TOML `[blackbox]` section.
#[derive(Debug, Deserialize)]
pub struct RawBlackbox {
    /// Enable behavioural logging. Default: `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Seconds of inactivity before a session is considered ended.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
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
    #[serde(default)]
    pub blackbox: Option<RawBlackbox>,
    #[serde(default)]
    pub binding: Vec<RawBinding>,
}
