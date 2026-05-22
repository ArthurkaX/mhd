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
#[cfg(feature = "blackbox")]
#[derive(Debug, Deserialize)]
pub struct RawBlackbox {
    /// Enable behavioural logging. Default: `false`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Seconds of inactivity before a session is considered ended.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
}

/// Raw TOML `[transcribe]` section.
#[derive(Debug, Deserialize)]
pub struct RawTranscribe {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub models_dir: Option<String>,
    #[serde(default)]
    pub sherpa_onnx_ws: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub show_preview: Option<bool>,
    #[serde(default)]
    pub keep_sidecar_warm: Option<bool>,
    #[serde(default)]
    pub threads: Option<u32>,
    #[serde(default)]
    pub silence_ms: Option<u64>,
    #[serde(default)]
    pub min_chunk_ms: Option<u64>,
    #[serde(default)]
    pub max_chunk_ms: Option<u64>,
    #[serde(default)]
    pub overlap_ms: Option<u64>,
    #[serde(default)]
    pub speech_rms_threshold: Option<f32>,
    #[serde(default)]
    pub join_separator: Option<String>,
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
    #[cfg(feature = "blackbox")]
    #[serde(default)]
    pub blackbox: Option<RawBlackbox>,
    #[serde(default)]
    pub transcribe: Option<RawTranscribe>,
    #[serde(default)]
    pub binding: Vec<RawBinding>,
}
