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
    #[serde(default)]
    pub preset: Option<String>,
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

/// Raw TOML `[keycast]` section.
#[derive(Debug, Deserialize)]
pub struct RawKeycast {
    /// Overlay position: top_left, top_center, top_right, bottom_left, bottom_center, bottom_right.
    #[serde(default)]
    pub position: Option<String>,
    /// How long a key label stays visible.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Show single printable keystrokes in a typing block. Default: false.
    #[serde(default)]
    pub show_typing: Option<bool>,
    /// Width of the typing block in characters. Default: 22.
    #[serde(default)]
    pub typing_width_chars: Option<u32>,
    /// How long a typed character stays visible. Default: 2500 ms.
    #[serde(default)]
    pub typing_duration_ms: Option<u64>,
}

/// Raw TOML `[codex_watcher]` section.
#[derive(Debug, Deserialize)]
pub struct RawCodexWatcher {
    /// Enable the background Codex telemetry watcher. Default: `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
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
    #[serde(default)]
    pub keycast: Option<RawKeycast>,
    /// Ordered list of power plan names for `switch_power_plan` with target="next".
    #[serde(default)]
    pub power_plans: Vec<String>,
    #[serde(default)]
    pub codex_watcher: Option<RawCodexWatcher>,
    #[serde(default)]
    pub binding: Vec<RawBinding>,
}
