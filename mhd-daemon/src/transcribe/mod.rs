//! Transcibe module — speech-to-text dictation via sherpa-onnx / Parakeet.
//!
//! ## Architecture
//!
//! `toggle()` is called from the worker on each hotkey press. It never blocks:
//! - First press spawns a **session thread** that downloads dependencies,
//!   starts the sidecar, captures audio, segments, sends to WebSocket,
//!   and collects results.
//! - Second press signals the thread to stop and returns the accumulated text.
//!
//! The session thread runs independently, so hotkeys remain responsive
//! even during long downloads (sherpa-onnx-ws ~18 MB, model ~465 MB).

pub mod audio;
pub mod config;
pub mod session;
pub mod parakeet;
pub mod clipboard;
pub mod segmenter;
pub mod downloader;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, LazyLock};
use std::thread;
use std::time::Duration;

use crate::config::raw::RawTranscribe;
use crate::transcribe::audio::AudioChunk;
use crate::transcribe::config::{TranscribeConfig, OutputMode};
use crate::transcribe::parakeet::{Sidecar, WsClient, find_free_port};
use crate::transcribe::segmenter::SegmentEvent;

const CHUNK_MS: u64 = 50;

// ── Global session state ────────────────────────────────────────────

/// Handle to a running session thread.
struct SessionHandle {
    /// Signal the thread to stop (download phase or pipeline phase).
    running: Arc<AtomicBool>,
    /// Thread join handle.
    handle: Option<thread::JoinHandle<()>>,
    /// Accumulated transcript (written by session thread, read on stop).
    transcript: Arc<Mutex<Vec<String>>>,
}

static SESSION: LazyLock<Mutex<Option<SessionHandle>>> = LazyLock::new(|| Mutex::new(None));

/// Toggle transcription: start if idle, stop if recording.
///
/// Non-blocking: returns immediately after spawning/stopping the session thread.
pub fn toggle(config: TranscribeConfig) -> Result<String, String> {
    let mut session_guard = SESSION.lock().map_err(|e| format!("lock error: {e}"))?;

    if let Some(mut session) = session_guard.take() {
        // ── STOP ──

        // Signal thread to stop (it will check running flag during download/pipeline)
        session.running.store(false, Ordering::Relaxed);

        // Don't join/detach — let it exit on its own.
        // The session thread checks running and exits promptly.
        // Sidecar::drop kills the child process when the thread exits.

        // Collect whatever transcript was accumulated so far
        let text = {
            let t = session.transcript.lock().unwrap();
            t.join(&config.join_separator)
        };

        // Deliver output
        if !text.is_empty() {
            match config.output {
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

        Ok(text)
    } else {
        // ── START ──

        config.validate().map_err(|errs| errs.join("; "))?;

        let running = Arc::new(AtomicBool::new(true));
        let transcript: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let running_clone = running.clone();
        let transcript_clone = transcript.clone();
        let config_clone = config.clone();

        let handle = thread::Builder::new()
            .name("transcribe-session".into())
            .spawn(move || {
                run_session(config_clone, running_clone, transcript_clone);
            })
            .map_err(|e| format!("cannot spawn session thread: {e}"))?;

        *session_guard = Some(SessionHandle {
            running,
            handle: Some(handle),
            transcript,
        });

        Ok("transcribe: session started".into())
    }
}

/// Session thread entry point.
///
/// 1. Downloads dependencies (if needed) — checks `running` periodically.
/// 2. Starts sherpa-onnx-ws sidecar.
/// 3. Starts WASAPI capture + pipeline thread.
/// 4. Waits for stop signal or errors, then cleans up.
fn run_session(
    config: TranscribeConfig,
    running: Arc<AtomicBool>,
    transcript: Arc<Mutex<Vec<String>>>,
) {
    // ── Resolve / download dependencies ─────────────────────────────────
    let resolved_config = match resolve_deps(&config, &running) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("mhd: transcribe: dependency error: {e}");
            return;
        }
    };

    if !running.load(Ordering::Relaxed) {
        return;
    }

    // ── Start sidecar ─────────────────────────────────────────────────
    let port = match find_free_port() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mhd: transcribe: cannot find free port: {e}");
            return;
        }
    };

    let mut sidecar = match Sidecar::start(&resolved_config, port) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mhd: transcribe: sidecar error: {e}");
            return;
        }
    };

    if !running.load(Ordering::Relaxed) {
        sidecar.stop();
        return;
    }

    // ── Start capture ─────────────────────────────────────────────────
    let audio_rx = match audio::start_capture(running.clone()) {
        Ok(rx) => rx,
        Err(e) => {
            eprintln!("mhd: transcribe: capture error: {e}");
            sidecar.stop();
            return;
        }
    };

    // ── Run pipeline ──────────────────────────────────────────────────
    run_pipeline(audio_rx, &resolved_config, port, transcript, &running);

    // ── Cleanup ───────────────────────────────────────────────────────
    sidecar.stop();
}

/// Download dependencies, checking `running` between steps.
fn resolve_deps(config: &TranscribeConfig, running: &AtomicBool) -> Result<TranscribeConfig, String> {
    let mut resolved = config.clone();

    // 1. sherpa-onnx-ws
    if running.load(Ordering::Relaxed) {
        if resolved.sherpa_onnx_ws.trim().is_empty()
            || !std::path::Path::new(&resolved.sherpa_onnx_ws).exists()
        {
            resolved.sherpa_onnx_ws = downloader::ensure_sherpa_onnx()?
                .to_string_lossy().to_string();
        }
    }

    // 2. Model
    if running.load(Ordering::Relaxed) {
        let model_path = std::path::Path::new(&resolved.model);
        if !model_path.is_absolute() || !model_path.exists() {
            // Bare model name or missing path — ensure downloaded
            let model_name = model_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !model_name.is_empty() {
                downloader::ensure_model(&model_name)?;
                let models_dir = downloader::models_dir()?;
                resolved.model = models_dir.join(&model_name)
                    .to_string_lossy().to_string();
            }
        }
    }

    Ok(resolved)
}

/// Run the pipeline: read audio chunks, segment, send to WS, collect results.
fn run_pipeline(
    audio_rx: std::sync::mpsc::Receiver<AudioChunk>,
    config: &TranscribeConfig,
    port: u16,
    transcript: Arc<Mutex<Vec<String>>>,
    running: &AtomicBool,
) {
    // Connect WebSocket to sherpa-onnx-ws
    let mut ws = match WsClient::connect("127.0.0.1", port, "/") {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("mhd: transcribe: WS connect: {e}");
            return;
        }
    };

    // Set read timeout to poll both WS and running flag
    let _ = ws.stream().set_read_timeout(Some(Duration::from_millis(200)));

    let mut segmenter = segmenter::RmsSegmenter::new(
        config.speech_rms_threshold,
        config.silence_ms,
        config.min_chunk_ms,
        config.max_chunk_ms,
        CHUNK_MS,
    );

    let mut pending_results: Vec<String> = Vec::new();

    loop {
        // Check stop signal
        if !running.load(Ordering::Relaxed) {
            // Drain remaining segments
            while let Some(event) = segmenter.finish() {
                if let SegmentEvent::Phrase(audio) = event {
                    let _ = ws.send_binary(&float32_to_bytes(&audio));
                }
            }
            break;
        }

        // Try to read audio chunk with timeout
        match audio_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                // Feed segmenter
                while let Some(event) = segmenter.feed(&chunk) {
                    if let SegmentEvent::Phrase(audio) = event {
                        if let Err(e) = ws.send_binary(&float32_to_bytes(&audio)) {
                            eprintln!("mhd: transcribe: WS send: {e}");
                            return;
                        }
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Capture thread ended — drain and exit
                while let Some(event) = segmenter.finish() {
                    if let SegmentEvent::Phrase(audio) = event {
                        let _ = ws.send_binary(&float32_to_bytes(&audio));
                    }
                }
                break;
            }
        }

        // Read WS responses
        loop {
            match ws.read_frame() {
                Ok((opcode, payload)) => {
                    match opcode {
                        0x1 | 0x2 => {
                            if let Ok(text) = String::from_utf8(payload) {
                                let trimmed = text.trim().to_string();
                                if !trimmed.is_empty() {
                                    pending_results.push(trimmed);
                                }
                            }
                        }
                        0x8 => return, // Close frame
                        0x9 => { let _ = ws.send_pong(&[]); }
                        _ => {}
                    }
                }
                Err(ref e) if e.contains("timed out") => break,
                Err(e) => {
                    eprintln!("mhd: transcribe: WS read: {e}");
                    return;
                }
            }
        }
    }

    // Close WS
    let _ = ws.close();

    // Store results
    let mut t = transcript.lock().unwrap();
    t.extend(pending_results.drain(..));
}

fn float32_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

// ── Config parsing (legacy) ─────────────────────────────────────────

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
