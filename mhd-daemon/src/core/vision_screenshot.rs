//! Background controller for the vision screenshot action.
//!
//! Manages single-flight execution: only one vision request can run at a time.
//! Capture, encoding, network request, and clipboard copy happen on a dedicated
//! background thread so the action worker remains responsive.
//!
//! Model resolution, request execution, trace logging, and clipboard copy are
//! shared with `vision_snip` via [`crate::core::vision_common`].

use llm_proxy::vision::DEFAULT_VISION_PROMPT;

use crate::app::{AppHandle, DaemonControl};
use crate::core::vision_common::{self, VisionEndpoint, VisionKind};
use crate::core::vision_guard::VisionGuard;
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
    let endpoint = match vision_common::load_vision_endpoint(handle) {
        Ok(ep) => ep,
        Err(msg) => {
            osd.show_notify(msg, 3000);
            return;
        }
    };
    let prompt = load_vision_prompt();

    osd.show_notify("Analyzing screenshot...", 3000);

    let handle = handle.clone();
    std::thread::Builder::new()
        .name("mhd-vision-screenshot".into())
        .spawn(move || {
            match run_vision_workflow(&handle, &endpoint, &prompt) {
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
            // Guard drops here, clearing the running flag.
        })
        .ok();
}

/// Load the configurable vision prompt, falling back to the built-in default.
fn load_vision_prompt() -> String {
    llm_proxy::config::load_settings()
        .ok()
        .map(|s| s.vision_prompt)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_VISION_PROMPT.to_string())
}

/// Capture the foreground monitor, encode it, and send it for analysis.
fn run_vision_workflow(
    handle: &AppHandle,
    endpoint: &VisionEndpoint,
    prompt: &str,
) -> Result<(), String> {
    if !handle.quiet() {
        eprintln!("mhd: vision: capturing foreground monitor...");
    }
    let captured = capture_foreground_monitor().map_err(|e| {
        eprintln!("mhd: vision: capture failed: {e}");
        "Could not capture the screen".to_string()
    })?;

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

    vision_common::send_and_copy(handle, VisionKind::Screenshot, endpoint, prompt, &png)
}
