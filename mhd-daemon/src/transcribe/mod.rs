//! Transcibe module — speech-to-text dictation via sherpa-onnx / Parakeet.
//!
//! ## Phase 1: Single-shot MVP (current)
//! - Config, action, sidecar start/stop, WebSocket client, clipboard output.
//! - Transcribe a pre-recorded WAV file.
//!
//! ## Phase 2: Microphone recording (WASAPI)
//! - Capture mic audio, convert to Parakeet format, transcribe, output.
//!
//! ## Phase 3: Silence chunking + live preview
//! - RMS segmenter emits chunks on pauses.
//! - Transcribe chunks while recording continues.
//! - Preview overlay shows incremental results.
//!
//! ## Phase 4: Model registry, downloader, paste-on-blur, polish.

pub mod config;
pub mod session;
pub mod parakeet;
pub mod clipboard;

use std::sync::Mutex;
use crate::config::raw::RawTranscribe;
use crate::transcribe::config::{TranscribeConfig, OutputMode};
use crate::transcribe::session::TranscribeSession;

/// Global controller for the transcribe feature.
/// Manages session lifecycle and resource cleanup.
pub struct TranscribeController {
    session: Mutex<TranscribeSession>,
    config: TranscribeConfig,
}

impl TranscribeController {
    /// Create a new controller from parsed config.
    pub fn new(config: TranscribeConfig) -> Self {
        let session = TranscribeSession::new(config.clone());
        TranscribeController {
            session: Mutex::new(session),
            config,
        }
    }

    /// Toggle transcription: start if idle, stop if recording.
    pub fn toggle(&self) -> Result<String, String> {
        let mut session = self.session.lock().unwrap();
        match session.state() {
            session::SessionState::Idle => {
                session.start()?;
                // Phase 1: For now this is a placeholder — real capture
                // comes in Phase 2. Return a message indicating success.
                Ok("transcribe: session started".into())
            }
            session::SessionState::Recording => {
                session.stop()?;
                let text = session.result().to_string();
                // Deliver output based on mode
                if !text.is_empty() {
                    match self.config.output {
                        OutputMode::Clipboard => {
                            clipboard::set_clipboard_text(&text)?;
                        }
                        OutputMode::Paste => {
                            clipboard::set_clipboard_text(&text)?;
                            clipboard::send_paste()?;
                        }
                        OutputMode::PasteOnBlur => {
                            clipboard::set_clipboard_text(&text)?;
                            clipboard::send_paste()?;
                        }
                    }
                }
                session.reset();
                Ok(text)
            }
            _ => Err("transcribe: session already active".into()),
        }
    }
}

/// Parse `[transcribe]` raw config.
pub fn parse_transcribe_config(raw: Option<&RawTranscribe>) -> TranscribeConfig {
    match raw {
        Some(r) => TranscribeConfig {
            enabled: r.enabled.unwrap_or(false),
            model: r.model.clone().unwrap_or_else(|| "parakeet-tdt-0.6b-v3".into()),
            models_dir: r.models_dir.clone().unwrap_or_default(),
            sherpa_onnx_ws: r.sherpa_onnx_ws.clone().unwrap_or_default(),
            output: match r.output.as_deref() {
                Some("clipboard") => OutputMode::Clipboard,
                Some("paste") => OutputMode::Paste,
                Some("paste_on_blur") | _ => OutputMode::PasteOnBlur,
            },
            show_preview: r.show_preview.unwrap_or(true),
            keep_sidecar_warm: r.keep_sidecar_warm.unwrap_or(false),
            threads: r.threads.unwrap_or(4),
            silence_ms: r.silence_ms.unwrap_or(700),
            min_chunk_ms: r.min_chunk_ms.unwrap_or(500),
            max_chunk_ms: r.max_chunk_ms.unwrap_or(15000),
            overlap_ms: r.overlap_ms.unwrap_or(250),
            speech_rms_threshold: r.speech_rms_threshold.unwrap_or(0.001),
            join_separator: r.join_separator.clone().unwrap_or_else(|| " ".into()),
        },
        None => TranscribeConfig::default(),
    }
}
