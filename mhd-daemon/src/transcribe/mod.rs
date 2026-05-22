//! Transcibe module — speech-to-text dictation via sherpa-onnx / Parakeet.
//!
//! ## Phase 1: Single-shot MVP (done)
//! - Config, action, sidecar start/stop, WebSocket client, clipboard output.
//!
//! ## Phase 2: WASAPI microphone capture (done)
//! - Capture mic audio, convert to Parakeet format.
//!
//! ## Phase 3: Silence chunking + live pipeline (in progress)
//! - RMS segmenter emits chunks on pauses.
//! - Transcribe chunks while recording continues.
//!
//! ## Phase 4: Live preview overlay
//! - Preview overlay shows incremental results.
//!
//! ## Phase 5: Model registry, downloader, paste-on-blur, polish.

pub mod audio;
pub mod config;
pub mod session;
pub mod parakeet;
pub mod clipboard;
pub mod segmenter;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, LazyLock};
use std::thread;

use crate::config::raw::RawTranscribe;
use crate::transcribe::audio::AudioChunk;
use crate::transcribe::config::{TranscribeConfig, OutputMode};
use crate::transcribe::parakeet::{Sidecar, WsClient, find_free_port};
use crate::transcribe::segmenter::SegmentEvent;

/// The sherpa-onnx-ws model expects 16 kHz mono f32 audio as binary frames.
const TARGET_SR: u32 = 16000;
const CHUNK_MS: u64 = 50;

// ── Global pipeline state ─────────────────────────────────────────────

struct Pipeline {
    /// Thread handles (capture + pipeline).
    capture_handle: Option<thread::JoinHandle<()>>,
    pipeline_handle: Option<thread::JoinHandle<()>>,
    /// Signal to stop capture.
    running: Arc<AtomicBool>,
    /// Accumulated transcript text.
    transcript: Vec<String>,
    /// Sidecar handle.
    sidecar: Option<Sidecar>,
}

static PIPELINE: LazyLock<Mutex<Option<Pipeline>>> = LazyLock::new(|| Mutex::new(None));

/// Toggle transcription: start if idle, stop if recording.
///
/// This is called from the worker on each hotkey press.
pub fn toggle(config: TranscribeConfig) -> Result<String, String> {
    let mut pipeline_guard = PIPELINE.lock().map_err(|e| format!("lock error: {e}"))?;

    if pipeline_guard.is_some() {
        // ── STOP ──
        let mut pipeline = pipeline_guard.take().unwrap();

        // Signal capture to stop
        pipeline.running.store(false, Ordering::Relaxed);

        // Wait for threads to finish
        if let Some(h) = pipeline.capture_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = pipeline.pipeline_handle.take() {
            let _ = h.join();
        }

        // Stop sidecar
        if let Some(mut sc) = pipeline.sidecar.take() {
            sc.stop();
        }

        let text = pipeline.transcript.join(&config.join_separator);

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
        // Validate config
        config.validate().map_err(|errs| errs.join("; "))?;

        // 1. Start sherpa-onnx-ws sidecar
        let port = find_free_port()?;
        let mut sidecar = Sidecar::start(&config, port)?;

        // 2. Start WASAPI capture
        let running = Arc::new(AtomicBool::new(true));
        let audio_rx = audio::start_capture(running.clone())?;

        // 3. Start pipeline thread
        let transcript: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let transcript_clone = transcript.clone();
        let config_clone = config.clone();
        let pipeline_handle = thread::Builder::new()
            .name("transcribe-pipeline".into())
            .spawn(move || {
                run_pipeline(audio_rx, &config_clone, port, transcript_clone);
            })
            .map_err(|e| format!("cannot spawn pipeline thread: {e}"))?;

        // Wait a moment for the sidecar to be ready
        thread::sleep(std::time::Duration::from_millis(200));

        *pipeline_guard = Some(Pipeline {
            capture_handle: None, // audio::start_capture doesn't return handle; it's self-contained
            pipeline_handle: Some(pipeline_handle),
            running,
            transcript: Vec::new(),
            sidecar: Some(sidecar),
        });

        Ok("transcribe: session started".into())
    }
}

/// Run the pipeline: read audio chunks, segment, send to WS, collect results.
fn run_pipeline(
    audio_rx: std::sync::mpsc::Receiver<AudioChunk>,
    config: &TranscribeConfig,
    port: u16,
    transcript: Arc<Mutex<Vec<String>>>,
) {
    // Connect WebSocket to sherpa-onnx-ws
    let mut ws = match WsClient::connect("127.0.0.1", port, "/") {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("mhd: transcribe: WS connect: {e}");
            return;
        }
    };

    let mut segmenter = segmenter::RmsSegmenter::new(
        config.speech_rms_threshold,
        config.silence_ms,
        config.min_chunk_ms,
        config.max_chunk_ms,
        CHUNK_MS,
    );

    // We read from the channel and also read WS responses concurrently.
    // Use non-blocking check on both sides.
    let mut pending_results: Vec<String> = Vec::new();
    let mut current_segment: Option<Vec<f32>> = None;
    let mut done = false;
    let mut segment_id: u64 = 0;

    // Set WS read timeout to 100ms to poll for new audio chunks
    let _ = ws.stream().set_read_timeout(Some(std::time::Duration::from_millis(100)));

    while !done {
        // 1. Try to read an audio chunk (non-blocking with 100ms timeout)
        let audio_event = audio_rx.recv_timeout(std::time::Duration::from_millis(100));
        match audio_event {
            Ok(chunk) => {
                // Feed segmenter
                while let Some(event) = segmenter.feed(&chunk) {
                    match event {
                        SegmentEvent::Phrase(audio) => {
                            // Send to WS
                            if let Err(e) = ws.send_binary(&float32_to_bytes(&audio)) {
                                eprintln!("mhd: transcribe: WS send: {e}");
                                done = true;
                                break;
                            }
                            segment_id += 1;
                        }
                        SegmentEvent::Flush => {}
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // No audio — might be done, check running flag
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Capture thread finished — drain segmenter
                while let Some(event) = segmenter.finish() {
                    match event {
                        SegmentEvent::Phrase(audio) => {
                            if let Err(e) = ws.send_binary(&float32_to_bytes(&audio)) {
                                eprintln!("mhd: transcribe: WS send: {e}");
                                break;
                            }
                            segment_id += 1;
                        }
                        SegmentEvent::Flush => {}
                    }
                }
                done = true;
            }
        }

        // 2. Try to read WS responses
        loop {
            match ws.read_frame() {
                Ok((opcode, payload)) => {
                    match opcode {
                        0x1 | 0x2 => {
                            // Text or binary — treat as transcription result
                            if let Ok(text) = String::from_utf8(payload) {
                                let trimmed = text.trim().to_string();
                                if !trimmed.is_empty() {
                                    pending_results.push(trimmed);
                                }
                            }
                        }
                        0x8 => {
                            // Close frame
                            done = true;
                            break;
                        }
                        0x9 => {
                            // Ping — respond with pong
                            let _ = ws.send_pong(&[]);
                        }
                        _ => {}
                    }
                }
                Err(ref e) if e.contains("timed out") => {
                    break; // no data, continue loop
                }
                Err(e) => {
                    if !done {
                        eprintln!("mhd: transcribe: WS read: {e}");
                    }
                    done = true;
                    break;
                }
            }
        }
    }

    // Close WS connection
    let _ = ws.close();

    // Store results
    let mut transcript_guard = transcript.lock().unwrap();
    transcript_guard.extend(pending_results.drain(..));
}

/// Convert f32 slice to little-endian bytes (sherpa-onnx-ws expects PCM f32).
fn float32_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

// ── Legacy controller (kept for backward compat) ─────────────────────

/// (Legacy) Parse `[transcribe]` raw config.
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
