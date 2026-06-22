//! Background controller for the Vision Snip action.
//!
//! Lifecycle:
//! 1. Acquire shared vision single-flight guard.
//! 2. Validate model configuration.
//! 3. Resolve and capture the foreground monitor.
//! 4. Open the interactive overlay (separate thread).
//! 5. Wait for user to Analyse or Cancel.
//! 6. Send the multimodal request via the consolidated client.
//! 7. Copy the response to the clipboard.
//! 8. Release the guard on all exit paths.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use llm_proxy::LogEvent;
use llm_proxy::state::VisionTraceEntry;

use crate::app::{AppHandle, DaemonControl};
use crate::core::vision_guard::VisionGuard;
use crate::osd::OsdHandle;
use crate::overlays::vision_snip::VisionSnipResult;
use crate::win32::clipboard;
use crate::win32::encode_png;
use crate::win32::screen_capture::{self, CaptureTarget};

/// Execute a Vision Snip session.
///
/// Called from the action worker. Spawns the overlay and waits for the user
/// to finish, then sends the request and copies the result.
pub fn execute(handle: &AppHandle) {
    let _guard = match VisionGuard::acquire(&handle.osd) {
        Some(g) => g,
        None => return,
    };

    let osd = handle.osd.clone();

    // 1. Validate configuration.
    let vision_cfg = match load_vision_config(handle) {
        Ok(cfg) => cfg,
        Err(msg) => {
            osd.show_notify(msg, 3000);
            return;
        }
    };

    // 2. Resolve and capture the foreground monitor.
    let capture_target = match screen_capture::resolve_foreground_monitor() {
        Ok(t) => t,
        Err(_) => {
            osd.show_notify("Could not determine the target monitor", 3000);
            return;
        }
    };

    let hmon_rect = capture_target_to_rect(&capture_target);

    let screenshot = match screen_capture::capture_target(&capture_target) {
        Ok(img) => img,
        Err(_) => {
            osd.show_notify("Could not capture the screen", 3000);
            return;
        }
    };

    // 3. Open the overlay.
    let rx = crate::overlays::vision_snip::show(handle.theme(), hmon_rect, screenshot);

    // 4. Wait for the user to finish.
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => break result,
            Err(RecvTimeoutError::Timeout) => {
                // Still waiting — check if daemon is shutting down.
                if !handle.running.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Overlay thread died unexpectedly.
                osd.show_notify("Could not open Vision Snip", 3000);
                return;
            }
        }
    };

    match result {
        VisionSnipResult::Analyze {
            image,
            annotations,
            prompt,
        } => {
            handle_analyze(handle, &osd, vision_cfg, image, annotations, prompt);
        }
        VisionSnipResult::Cancelled => {
            // No notification needed for cancellation.
            if !handle.quiet() {
                eprintln!("mhd: vision snip: cancelled");
            }
        }
        VisionSnipResult::Failed(msg) => {
            osd.show_notify(&msg, 3500);
            if !handle.quiet() {
                eprintln!("mhd: vision snip error: {msg}");
            }
        }
    }

    // Guard drops here, releasing the busy flag.
}

/// Handle the Analyse outcome: encode, send request, copy result.
fn handle_analyze(
    handle: &AppHandle,
    osd: &OsdHandle,
    cfg: VisionConfig,
    image: crate::win32::screen_capture::CapturedImage,
    annotations: Vec<crate::overlays::vision_snip::model::ModelAnnotation>,
    prompt: String,
) {
    osd.show_notify("Analyzing annotated screenshot...", 3000);

    let handle = handle.clone();
    let osd = osd.clone();

    std::thread::Builder::new()
        .name("mhd-vision-snip-request".into())
        .spawn(move || {
            let result = run_vision_request(&handle, &cfg, &osd, &image, &annotations, &prompt);
            match result {
                Ok(()) => {
                    osd.show_notify("Vision Snip result copied", 2000);
                    if !handle.quiet() {
                        eprintln!("mhd: vision snip: success");
                    }
                }
                Err(msg) => {
                    osd.show_notify(&msg, 3500);
                    if !handle.quiet() {
                        eprintln!("mhd: vision snip error: {msg}");
                    }
                }
            }
            // Guard held by execute(), released when execute() returns.
        })
        .ok();
}

/// Run the network request on a background thread.
fn run_vision_request(
    handle: &AppHandle,
    cfg: &VisionConfig,
    _osd: &OsdHandle,
    image: &crate::win32::screen_capture::CapturedImage,
    _annotations: &[crate::overlays::vision_snip::model::ModelAnnotation],
    _prompt: &str,
) -> Result<(), String> {
    // Encode the final image as PNG.
    let png = encode_png(image).map_err(|e| {
        eprintln!("mhd: vision snip: PNG encoding failed: {e}");
        "Could not prepare the annotated screenshot".to_string()
    })?;

    let endpoint_url = llm_proxy::config::normalize_vision_endpoint(&cfg.endpoint);
    let seq = next_vision_seq();

    log_vision_start(seq, &cfg.provider, &cfg.model, &endpoint_url);

    if !handle.quiet() {
        eprintln!(
            "mhd: vision snip: sending to {} model {} ({}x{}px)...",
            cfg.provider, cfg.model, image.width, image.height
        );
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            eprintln!("mhd: vision snip: HTTP client error: {e}");
            "Vision request failed".to_string()
        })?;

    let target = llm_proxy::ResolvedModelEndpoint {
        provider: cfg.provider.clone(),
        model: cfg.model.clone(),
        endpoint: endpoint_url.clone(),
        api_key: cfg.api_key.clone(),
    };

    let started = std::time::Instant::now();
    let result_text = llm_proxy::vision::analyze_png_blocking(&client, &target, &cfg.prompt, &png)
        .map_err(|e| {
            let duration_ms = started.elapsed().as_millis() as u64;
            log_vision_error(
                seq,
                &cfg.provider,
                &cfg.model,
                &endpoint_url,
                None,
                &e.to_string(),
                duration_ms,
            );
            eprintln!("mhd: vision snip: request failed: {e}");
            format!("Vision request failed: {e}")
        })?;

    let duration_ms = started.elapsed().as_millis() as u64;
    log_vision_success(seq, &cfg.provider, &cfg.model, &endpoint_url, duration_ms);

    // Copy to clipboard.
    clipboard::set_text(&result_text).map_err(|e| {
        eprintln!("mhd: vision snip: clipboard write failed: {e}");
        "Could not copy the result".to_string()
    })?;

    if !handle.quiet() {
        let preview: String = result_text.chars().take(80).collect();
        eprintln!(
            "mhd: vision snip: result ({}) chars: \"{}\"",
            result_text.len(),
            preview
        );
    }

    Ok(())
}

// ── Configuration loading ─────────────────────────────────────────────

struct VisionConfig {
    provider: String,
    model: String,
    endpoint: String,
    api_key: String,
    prompt: String,
}

fn load_vision_config(handle: &AppHandle) -> Result<VisionConfig, String> {
    let cfg = handle.llm_proxy_config();

    let model_ref = cfg
        .vision_model
        .clone()
        .ok_or_else(|| "Configure a vision model first".to_string())?;

    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == model_ref.provider)
        .ok_or_else(|| "Vision model is unavailable".to_string())?;

    let secrets =
        llm_proxy::config::load_secrets().map_err(|_| "Could not load secrets".to_string())?;

    let api_key = secrets
        .provider_keys
        .get(&model_ref.provider)
        .filter(|k| !k.is_empty())
        .map(|s| s.as_str())
        .or_else(|| {
            if !secrets.upstream_key.is_empty() {
                Some(secrets.upstream_key.as_str())
            } else {
                None
            }
        })
        .ok_or_else(|| "Vision provider API key is missing".to_string())?;

    // Vision Snip uses a dedicated built-in prompt, not the configurable vision_prompt.
    let prompt = String::from(
        "Analyze this screenshot according to the user's annotations. \
         The image contains labelled annotations that are described below.",
    );

    Ok(VisionConfig {
        provider: model_ref.provider.clone(),
        model: model_ref.model.clone(),
        endpoint: provider.endpoint.clone(),
        api_key: api_key.to_string(),
        prompt,
    })
}

// ── Utilities ─────────────────────────────────────────────────────────

fn capture_target_to_rect(ct: &CaptureTarget) -> windows::Win32::Foundation::RECT {
    windows::Win32::Foundation::RECT {
        left: ct.left,
        top: ct.top,
        right: ct.left + ct.width as i32,
        bottom: ct.top + ct.height as i32,
    }
}

fn next_vision_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn log_vision_start(seq: u64, provider: &str, model: &str, endpoint: &str) {
    crate::core::llm_proxy::log_vision(
        VisionTraceEntry {
            seq,
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            status: None,
            error: None,
            duration_ms: 0,
        },
        Some(LogEvent {
            seq,
            event_type: "VISION_SNIP_START".to_string(),
            model: Some(model.to_string()),
            target: Some(provider.to_string()),
            target_model: Some(endpoint.to_string()),
            ..Default::default()
        }),
    );
}

fn log_vision_success(seq: u64, provider: &str, model: &str, endpoint: &str, duration_ms: u64) {
    crate::core::llm_proxy::log_vision(
        VisionTraceEntry {
            seq,
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            status: Some(200),
            error: None,
            duration_ms,
        },
        Some(LogEvent {
            seq,
            event_type: "VISION_SNIP_OK".to_string(),
            model: Some(model.to_string()),
            target: Some(provider.to_string()),
            target_model: Some(endpoint.to_string()),
            duration_ms: Some(duration_ms),
            ..Default::default()
        }),
    );
}

fn log_vision_error(
    seq: u64,
    provider: &str,
    model: &str,
    endpoint: &str,
    status: Option<u16>,
    error: &str,
    duration_ms: u64,
) {
    crate::core::llm_proxy::log_vision(
        VisionTraceEntry {
            seq,
            provider: provider.to_string(),
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            status,
            error: Some(error.to_string()),
            duration_ms,
        },
        Some(LogEvent {
            seq,
            event_type: "VISION_SNIP_ERR".to_string(),
            model: Some(model.to_string()),
            target: Some(provider.to_string()),
            target_model: Some(endpoint.to_string()),
            duration_ms: Some(duration_ms),
            error: Some(error.to_string()),
            status,
            ..Default::default()
        }),
    );
}
