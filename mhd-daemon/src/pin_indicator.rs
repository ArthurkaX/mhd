//! Pin indicator — coloured border overlay for always‑on‑top windows.
//!
//! When a window is pinned via `toggle_topmost`, a transparent layered window
//! with a thin accent‑coloured border is placed over it. The overlay follows
//! the target window until unpinned.
//!
//! Architecture
//! ────────────
//! One background thread runs a message loop with a periodic timer.
//! Pinning/unpinning commands are sent via a channel. The thread manages
//! a map of (target_hwnd → overlay_hwnd).

use std::collections::HashMap;
use std::sync::mpsc;

use windows::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT, SIZE, COLORREF, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::native_theme::Argb;
use crate::osd::to_utf16_z;

// ── Constants ─────────────────────────────────────────────────────

const BORDER_WIDTH: i32 = 3;
const OVERLAY_CLASS: &str = "mhd_pin_indicator_cls";
const UPDATE_INTERVAL_MS: u32 = 100; // poll every 100ms

// ── Channel + handle ──────────────────────────────────────────────

// HWND is not Send, so we transmit raw isize handles
type HwndRaw = isize;

enum PinCommand {
    Add(HwndRaw),
    Remove(HwndRaw),
    #[allow(dead_code)]
    Clear,
    #[allow(dead_code)]
    Quit,
}

#[derive(Clone)]
pub struct PinIndicatorHandle {
    tx: mpsc::Sender<PinCommand>,
}

impl PinIndicatorHandle {
    pub fn pin_window(&self, hwnd: HWND) {
        let _ = self.tx.send(PinCommand::Add(hwnd.0 as isize));
    }

    pub fn unpin_window(&self, hwnd: HWND) {
        let _ = self.tx.send(PinCommand::Remove(hwnd.0 as isize));
    }
}

// ── Thread state ──────────────────────────────────────────────────

struct PinState {
    /// Map: target window HWND → overlay window HWND
    overlays: HashMap<isize, HWND>,
    /// Accent colour from the current theme
    accent: Argb,
}

// ── Thread entry ──────────────────────────────────────────────────

/// Spawn the pin‑indicator thread. Returns a handle for sending commands.
pub fn spawn() -> PinIndicatorHandle {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        run_loop(rx);
    });

    PinIndicatorHandle { tx }
}

fn run_loop(rx: mpsc::Receiver<PinCommand>) {
    // Register window class
    let cls_name = to_utf16_z(OVERLAY_CLASS);
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&wc);
    }

    // Create a hidden message‑only window to receive commands
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_OVERLAPPED,
            0, 0, 0, 0,
            None,
            None,
            hinstance,
            None,
        )
    };

    let Ok(hwnd) = hwnd else { return };

    // Initial theme colour
    let accent = Argb::new(255, 100, 180, 255); // fallback blue

    let state = Box::into_raw(Box::new(PinState {
        overlays: HashMap::new(),
        accent,
    }));
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }

    // Set a timer to periodically update overlay positions
    unsafe {
        let _ = SetTimer(hwnd, 1, UPDATE_INTERVAL_MS, None);
    }

    // Message loop
    let mut msg = MSG::default();
    loop {
        // Check channel
        match rx.try_recv() {
            Ok(PinCommand::Add(raw)) => {
                let target = HWND(raw as *mut _);
                add_overlay(hwnd, state, target);
            }
            Ok(PinCommand::Remove(raw)) => {
                let target = HWND(raw as *mut _);
                remove_overlay(state, target);
            }
            Ok(PinCommand::Clear) => {
                clear_all_overlays(state);
            }
            Ok(PinCommand::Quit) => {
                clear_all_overlays(state);
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }

        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !ret.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        let _ = Box::from_raw(state);
    }
}

// ── Window procedure ──────────────────────────────────────────────

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PinState;
                if !state_ptr.is_null() {
                    update_all_positions(&mut *state_ptr);
                }
                LRESULT(0)
            }

            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ── Overlay management ────────────────────────────────────────────

fn add_overlay(_msg_hwnd: HWND, state_ptr: *mut PinState, target: HWND) {
    let state = unsafe { &mut *state_ptr };
    if state.overlays.contains_key(&(target.0 as isize)) {
        return; // already pinned
    }

    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();
    let cls_name = to_utf16_z(OVERLAY_CLASS);

    let target_rect = unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(target, &mut rc);
        rc
    };

    let border = BORDER_WIDTH;
    let w = target_rect.right - target_rect.left + border * 2;
    let h = target_rect.bottom - target_rect.top + border * 2;

    let overlay = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            target_rect.left - border,
            target_rect.top - border,
            w,
            h,
            None,
            None,
            hinstance,
            None,
        )
    };

    let Ok(overlay) = overlay else { return };

    // Draw the border
    paint_overlay(overlay, state.accent, w, h);

    unsafe {
        let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE);
    }

    state.overlays.insert(target.0 as isize, overlay);
}

fn remove_overlay(state_ptr: *mut PinState, target: HWND) {
    let state = unsafe { &mut *state_ptr };
    if let Some(overlay) = state.overlays.remove(&(target.0 as isize)) {
        unsafe {
            let _ = DestroyWindow(overlay);
        }
    }
}

fn clear_all_overlays(state_ptr: *mut PinState) {
    let state = unsafe { &mut *state_ptr };
    for (_, overlay) in state.overlays.drain() {
        unsafe {
            let _ = DestroyWindow(overlay);
        }
    }
}

fn update_all_positions(state: &mut PinState) {
    let mut to_remove = Vec::new();

    for (&target_raw, &overlay) in &state.overlays {
        let target = HWND(target_raw as *mut _);
        // Check if target still exists and is topmost
        let is_valid = unsafe {
            let ex_style = GetWindowLongPtrW(target, GWL_EXSTYLE);
            (ex_style as u32 & WS_EX_TOPMOST.0) != 0 && IsWindow(target).as_bool()
        };

        if !is_valid {
            to_remove.push(target);
            continue;
        }

        // Update overlay position to match target window
        let target_rect = unsafe {
            let mut rc = RECT::default();
            let _ = GetWindowRect(target, &mut rc);
            rc
        };

        let border = BORDER_WIDTH;
        let x = target_rect.left - border;
        let y = target_rect.top - border;
        let w = target_rect.right - target_rect.left + border * 2;
        let h = target_rect.bottom - target_rect.top + border * 2;

        unsafe {
            let _ = SetWindowPos(
                overlay,
                None,
                x,
                y,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    for target in to_remove {
        if let Some(overlay) = state.overlays.remove(&(target.0 as isize)) {
            unsafe {
                let _ = DestroyWindow(overlay);
            }
        }
    }
}

// ── Border painting ───────────────────────────────────────────────

fn paint_overlay(hwnd: HWND, color: Argb, w: i32, h: i32) {
    let screen_dc = unsafe { GetDC(None) };

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(dib) = dib else {
        unsafe { let _ = ReleaseDC(None, screen_dc); }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    // Fill with transparent (all zeros)
    // Then draw the border: 4 edge rectangles
    let pixels = unsafe {
        std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize)
    };

    // Clear to transparent
    for px in pixels.iter_mut() {
        *px = 0;
    }

    let border = BORDER_WIDTH;

    // Top edge
    for y in 0..border {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            if idx < pixels.len() {
                pixels[idx] = color.to_premultiplied_argb_pixel();
            }
        }
    }

    // Bottom edge
    for y in (h - border)..h {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            if idx < pixels.len() {
                pixels[idx] = color.to_premultiplied_argb_pixel();
            }
        }
    }

    // Left edge (excluding corners already drawn)
    for y in border..(h - border) {
        for x in 0..border {
            let idx = (y * w + x) as usize;
            if idx < pixels.len() {
                pixels[idx] = color.to_premultiplied_argb_pixel();
            }
        }
    }

    // Right edge
    for y in border..(h - border) {
        for x in (w - border)..w {
            let idx = (y * w + x) as usize;
            if idx < pixels.len() {
                pixels[idx] = color.to_premultiplied_argb_pixel();
            }
        }
    }

    // Corners: anti-alias with slightly transparent
    // (Simple approach: just fill corner pixels)

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 220, // slightly transparent
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: w, cy: h };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            None,
            Some(&sz),
            dib_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}
