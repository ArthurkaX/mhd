//! Background controller for the vision screenshot action.
//!
//! Manages single-flight execution: only one vision request can run at a time.
//! Capture, encoding, network request, and clipboard copy happen on a dedicated
//! background thread so the action worker remains responsive.

use llm_proxy::LogEvent;
use llm_proxy::state::VisionTraceEntry;
use llm_proxy::vision::DEFAULT_VISION_PROMPT;

use crate::app::{AppHandle, DaemonControl};
use crate::core::vision_guard::VisionGuard;
use crate::osd::OsdHandle;
use crate::win32::clipboard;
use crate::win32::encode_png;
use crate::win32::screen_capture::capture_foreground_monitor;

/// Execute a vision screenshot: capture, analyze, copy result.
///
/// This function is called from the action worker. It validates configuration
/// upfront and spawns the heavy work on a background thread.
pub fn execute(handle: &AppHandle) {
    let _guard = match VisionGuard::acquire(&handle.osd) {
        Some(g) => g,
        None => return,
    };

    let osd = handle.osd.clone();

    // Validate configuration on the calling thread so errors show immediately.
    let vision_model = match load_vision_model_config(handle) {
        Ok(vm) => vm,
        Err(msg) => {
            osd.show_notify(msg, 3000);
            return;
        }
    };

    osd.show_notify("Analyzing screenshot...", 3000);

    let handle = handle.clone();
    std::thread::Builder::new()
        .name("mhd-vision-screenshot".into())
        .spawn(move || {
            let result = run_vision_workflow(&handle, &vision_model, &osd);
            match result {
                Ok(()) => {
                    osd.show_notify("Screenshot result copied", 2000);
                    if !handle.quiet() {
                        eprintln!("mhd: vision screenshot: success");
                    }
                }
                Err(msg) => {
                    osd.show_notify(&msg, 3500);
                    if !handle.quiet() {
                        eprintln!("mhd: vision screenshot error: {msg}");
                    }
                }
            }
            // Guard drops here, clearing the running flag
        })
        .ok();
}

/// Load and validate the vision model configuration.
fn load_vision_model_config(handle: &AppHandle) -> Result<VisionModelConfig, String> {
    let cfg = handle.llm_proxy_config();

    let model_ref = cfg
        .vision_model
        .clone()
        .ok_or_else(|| "Configure a vision model first".to_string())?;

    // Find the provider and its secrets
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.name == model_ref.provider)
        .ok_or_else(|| "Vision model is unavailable".to_string())?;

    // Load secrets
    let secrets =
        llm_proxy::config::load_secrets().map_err(|_| "Could not load secrets".to_string())?;

    // Resolve API key: per-provider key first, then global upstream_key
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

    // Load the custom prompt from settings, falling back to the default.
    let prompt = llm_proxy::config::load_settings()
        .ok()
        .map(|s| s.vision_prompt)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_VISION_PROMPT.to_string());

    Ok(VisionModelConfig {
        provider: model_ref.provider.clone(),
        model: model_ref.model.clone(),
        endpoint: provider.endpoint.clone(),
        api_key: api_key.to_string(),
        prompt,
    })
}

struct VisionModelConfig {
    provider: String,
    model: String,
    endpoint: String,
    api_key: String,
    prompt: String,
}

/// Run the full vision workflow on a background thread.
fn run_vision_workflow(
    handle: &AppHandle,
    vm: &VisionModelConfig,
    _osd: &OsdHandle,
) -> Result<(), String> {
    // 1. Capture the screen
    if !handle.quiet() {
        eprintln!("mhd: vision: capturing foreground monitor...");
    }
    let captured = capture_foreground_monitor().map_err(|e| {
        eprintln!("mhd: vision: capture failed: {e}");
        "Could not capture the screen".to_string()
    })?;

    // 2. Encode as PNG
    if !handle.quiet() {
        eprintln!(
            "mhd: vision: encoding PNG ({}x{})...",
            captured.width, captured.height
        );
    }
    let png = encode_png(&captured).map_err(|e| {
        eprintln!("mhd: vision: PNG encoding failed: {e}");
        "Could not encode the screenshot".to_string()
    })?;

    // 3. Build the resolved endpoint and send to the vision model
    let endpoint_url = llm_proxy::config::normalize_vision_endpoint(&vm.endpoint);
    let seq = next_vision_seq();

    log_vision_start(seq, &vm.provider, &vm.model, &endpoint_url);

    if !handle.quiet() {
        eprintln!(
            "mhd: vision: sending request to {} model {} at {}...",
            vm.provider, vm.model, endpoint_url
        );
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| {
            eprintln!("mhd: vision: failed to create HTTP client: {e}");
            "Vision request failed".to_string()
        })?;

    let target = llm_proxy::ResolvedModelEndpoint {
        provider: vm.provider.clone(),
        model: vm.model.clone(),
        endpoint: endpoint_url.clone(),
        api_key: vm.api_key.clone(),
    };

    let started = std::time::Instant::now();
    let result = llm_proxy::vision::analyze_png_blocking(&client, &target, &vm.prompt, &png)
        .map_err(|e| {
            let duration_ms = started.elapsed().as_millis() as u64;
            let status = None::<u16>;
            log_vision_error(
                seq,
                &vm.provider,
                &vm.model,
                &endpoint_url,
                status,
                &e.to_string(),
                duration_ms,
            );
            eprintln!("mhd: vision: request failed: {e}");
            format!("Vision request failed: {e}")
        })?;

    let duration_ms = started.elapsed().as_millis() as u64;
    log_vision_success(seq, &vm.provider, &vm.model, &endpoint_url, duration_ms);

    // 4. Copy result to clipboard
    clipboard::set_text(&result).map_err(|e| {
        eprintln!("mhd: vision: clipboard write failed: {e}");
        "Could not copy the result".to_string()
    })?;

    if !handle.quiet() {
        let preview: String = result.chars().take(80).collect();
        eprintln!(
            "mhd: vision: result ({}) chars: \"{}\"",
            result.len(),
            preview
        );
    }

    Ok(())
}

/// Monotonic sequence number for vision screenshot requests.
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
            event_type: "VISION_START".to_string(),
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
            event_type: "VISION_OK".to_string(),
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
            event_type: "VISION_ERR".to_string(),
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
