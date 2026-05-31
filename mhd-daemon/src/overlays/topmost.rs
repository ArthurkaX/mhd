//! Toggle topmost (pin) for the foreground window.
//!
//! Extracted from `worker.rs` for better modularity.  Worker dispatches
//! actions; this module provides the UI‑level pin/unpin functionality
//! via DWM + `SetWindowPos`.

#![allow(unsafe_op_in_unsafe_fn)]

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DwmSetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowTextW, HWND_NOTOPMOST,
    HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, SetWindowTextW, WS_EX_TOPMOST,
};
use windows::core::PCWSTR;

/// ASCII marker appended to the window title when pinned.
/// Renders in any font on any Windows version.
const PIN_MARKER: &str = " [Pin]";

/// DWM border colour when pinned — ABGR (orange‑ish accent).
const PIN_BORDER_COLOR: u32 = 0x00FFAA44;
/// Value that tells DWM to reset to the default colour.
const RESET_COLOR: u32 = 0xFFFFFFFE; // DWMWA_COLOR_DEFAULT / NONE

/// Toggle topmost state for the currently focused foreground window.
pub fn toggle() {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() {
            return;
        }

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let is_topmost = (ex_style as u32 & WS_EX_TOPMOST.0) != 0;

        if is_topmost {
            unpin(hwnd);
        } else {
            pin(hwnd);
        }
    }
}

unsafe fn pin(hwnd: HWND) {
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

    // Append marker to title
    add_title_marker(hwnd);

    // Set DWM border colour (accent tint) — works on Win10 20H1+
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &PIN_BORDER_COLOR as *const _ as *const _,
        4,
    );
    // Also tint the caption area slightly
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CAPTION_COLOR,
        &PIN_BORDER_COLOR as *const _ as *const _,
        4,
    );
}

unsafe fn unpin(hwnd: HWND) {
    let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

    // Restore original title (strip marker)
    remove_title_marker(hwnd);

    // Reset DWM border / caption colour to default
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &RESET_COLOR as *const _ as *const _,
        4,
    );
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CAPTION_COLOR,
        &RESET_COLOR as *const _ as *const _,
        4,
    );
}

unsafe fn add_title_marker(hwnd: HWND) {
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len > 0 {
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if !title.contains(PIN_MARKER) {
            let new_title = format!("{}{}", title, PIN_MARKER);
            let wide: Vec<u16> = new_title.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(wide.as_ptr()));
        }
    }
}

unsafe fn remove_title_marker(hwnd: HWND) {
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len > 0 {
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        if let Some(pos) = title.rfind(PIN_MARKER) {
            let new_title = title[..pos].to_string();
            let wide: Vec<u16> = new_title.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(wide.as_ptr()));
        }
    }
}
