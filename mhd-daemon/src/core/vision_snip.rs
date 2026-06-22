//! Background controller for the Vision Snip action.
//!
//! Lifecycle:
//! 1. Acquire shared vision single-flight guard.
//! 2. Validate model configuration.
//! 3. Resolve and capture the foreground monitor.
//! 4. Open the interactive overlay (separate thread).
//! 5. Wait for the user to Analyse or Cancel.
//! 6. Send the annotated image + structured prompt via the shared client.
//! 7. Copy the response to the clipboard.
//! 8. Release the guard on all exit paths.
//!
//! Model resolution, request execution, trace logging, and clipboard copy are
//! shared with `vision_screenshot` via [`crate::core::vision_common`].

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use crate::app::{AppHandle, DaemonControl};
use crate::core::vision_common::{self, VisionEndpoint, VisionKind};
use crate::core::vision_guard::VisionGuard;
use crate::osd::OsdHandle;
use crate::overlays::vision_snip::VisionSnipResult;
use crate::win32::encode_png;
use crate::win32::screen_capture::{self, CaptureTarget, CapturedImage};

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
    let endpoint = match vision_common::load_vision_endpoint(handle) {
        Ok(ep) => ep,
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
        VisionSnipResult::Analyze { image, prompt } => {
            handle_analyze(handle, &osd, endpoint, image, prompt);
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

/// Handle the Analyse outcome: encode the annotated image and send it with the
/// overlay-built structured prompt.
fn handle_analyze(
    handle: &AppHandle,
    osd: &OsdHandle,
    endpoint: VisionEndpoint,
    image: CapturedImage,
    prompt: String,
) {
    osd.show_notify("Analyzing annotated screenshot...", 3000);

    let handle = handle.clone();
    let osd = osd.clone();

    std::thread::Builder::new()
        .name("mhd-vision-snip-request".into())
        .spawn(move || {
            match run_vision_request(&handle, &endpoint, &image, &prompt) {
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

/// Encode the annotated image and send it with the structured prompt.
fn run_vision_request(
    handle: &AppHandle,
    endpoint: &VisionEndpoint,
    image: &CapturedImage,
    prompt: &str,
) -> Result<(), String> {
    let png = encode_png(image).map_err(|e| {
        eprintln!("mhd: vision snip: PNG encoding failed: {e}");
        "Could not prepare the annotated screenshot".to_string()
    })?;

    if !handle.quiet() {
        eprintln!(
            "mhd: vision snip: encoded annotated PNG ({}x{}px)",
            image.width, image.height
        );
    }

    vision_common::send_and_copy(handle, VisionKind::Snip, endpoint, prompt, &png)
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
