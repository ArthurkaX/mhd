//! Transcribe configuration — parsed from `[transcribe]` section.

/// Runtime transcript output behaviour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputMode {
    /// Copy final text to clipboard.
    Clipboard,
    /// Copy and immediately send Ctrl+V.
    Paste,
    /// Hide overlay, restore target window, then paste.
    PasteOnBlur,
}

/// Transcribe-specific settings.
#[derive(Debug, Clone)]
pub struct TranscribeConfig {
    /// Whether the transcribe feature is available.
    pub enabled: bool,
    /// Model name or path (e.g. "parakeet-tdt-0.6b-v3").
    pub model: String,
    /// Directory holding model files.
    pub models_dir: String,
    /// Path to `sherpa-onnx-ws.exe`.
    pub sherpa_onnx_ws: String,
    /// How to deliver final transcript.
    pub output: OutputMode,
    /// Whether to show preview overlay during recording.
    pub show_preview: bool,
    /// Keep sidecar alive between sessions.
    pub keep_sidecar_warm: bool,
    /// CPU threads for inference.
    pub threads: u32,
    /// Silence duration (ms) to detect phrase end.
    pub silence_ms: u64,
    /// Minimum audio chunk duration (ms).
    pub min_chunk_ms: u64,
    /// Maximum audio chunk duration (ms) — force flush at this length.
    pub max_chunk_ms: u64,
    /// Overlap between adjacent chunks (ms).
    pub overlap_ms: u64,
    /// RMS threshold below which audio is considered silence.
    pub speech_rms_threshold: f32,
    /// Separator inserted between concatenated chunk texts.
    pub join_separator: String,
}

impl Default for TranscribeConfig {
    fn default() -> Self {
        TranscribeConfig {
            enabled: false,
            model: "parakeet-tdt-0.6b-v3".into(),
            models_dir: String::new(),      // auto-resolved to ~/.config/mhd/transcribe/models/
            sherpa_onnx_ws: String::new(),  // auto-downloaded to ~/.config/mhd/transcribe/bin/
            output: OutputMode::PasteOnBlur,
            show_preview: true,
            keep_sidecar_warm: false,
            threads: 4,
            silence_ms: 700,
            min_chunk_ms: 500,
            max_chunk_ms: 15000,
            overlap_ms: 250,
            speech_rms_threshold: 0.001,
            join_separator: " ".into(),
        }
    }
}

impl TranscribeConfig {
    /// Validate config fields. Returns `Ok(())` or a list of errors.
    /// Note: `sherpa_onnx_ws` and `models_dir` may be empty — they will be
    /// auto-downloaded to `~/.config/mhd/transcribe/` on first use.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.model.trim().is_empty() {
            errors.push("transcribe.model must not be empty".into());
        }
        if self.threads < 1 {
            errors.push("transcribe.threads must be >= 1".into());
        }
        if self.silence_ms == 0 {
            errors.push("transcribe.silence_ms must be > 0".into());
        }
        if self.min_chunk_ms == 0 {
            errors.push("transcribe.min_chunk_ms must be > 0".into());
        }
        if self.max_chunk_ms <= self.min_chunk_ms {
            errors.push("transcribe.max_chunk_ms must be > min_chunk_ms".into());
        }
        if self.overlap_ms >= self.min_chunk_ms {
            errors.push("transcribe.overlap_ms must be < min_chunk_ms".into());
        }

        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}
