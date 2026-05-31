//! Styled native Win32 About dialog — layered rounded‑rect window
//! with themed visual style matching the OSD.
//!
//! Uses the shared [`ShellRenderer`] + [`DibFrame`] stack from
//! [`crate::renderer`].

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DT_LEFT, DT_SINGLELINE, DeleteObject, SelectObject, SetBkMode, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::native_theme::NativeTheme;
use crate::osd::{ShellRenderer, centered_position, create_font, to_utf16_z};

// ── Layout constants (96 dpi base) ────────────────────────────────────

const ABT_WIDTH_BASE: i32 = 360;
const ABT_HEIGHT_BASE: i32 = 210;
const PADDING: i32 = 24;
const ROUND_RADIUS_BASE: f32 = 14.0;

// ── Public API ─────────────────────────────────────────────────────────

/// Show a modal About dialog on the calling thread using the given theme.
/// Blocks until the user dismisses it (click, Escape, or Enter).
pub fn show_about(theme: NativeTheme) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = to_utf16_z("mhd_about_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(about_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            ABT_WIDTH_BASE,
            ABT_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    // DPI scale
    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;
    let w = (ABT_WIDTH_BASE as f32 * scale) as i32;
    let h = (ABT_HEIGHT_BASE as f32 * scale) as i32;
    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
    }

    paint_about(hwnd, w, h, scale, &theme);

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    // Standard message loop — blocks until WM_QUIT.
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !ret.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            unsafe {
                DestroyWindow(hwnd).ok();
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if wparam.0 == 0x1B /* VK_ESCAPE */ || wparam.0 == 0x0D
            /* VK_RETURN */
            {
                unsafe {
                    DestroyWindow(hwnd).ok();
                }
                return LRESULT(0);
            }
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ── Painting ──────────────────────────────────────────────────────────

fn paint_about(hwnd: HWND, width: i32, height: i32, scale: f32, theme: &NativeTheme) {
    // Allocate DIB + rounded background via shared ShellRenderer.
    let mut shell = match ShellRenderer::new(width, height, theme, scale, ROUND_RADIUS_BASE as i32)
    {
        Some(s) => s,
        None => return,
    };

    // Fonts
    let font_title_h = -(18.0 * scale) as i32;
    let font_body_h = -(12.0 * scale) as i32;
    let font_small_h = -(10.0 * scale) as i32;

    let hfont_title = create_font(font_title_h, true, "Segoe UI");
    let hfont_body = create_font(font_body_h, false, "Segoe UI");
    let hfont_small = create_font(font_small_h, false, "Segoe UI");

    let radius = (ROUND_RADIUS_BASE * scale) as i32;
    let pad = (PADDING as f32 * scale) as i32;
    let left = pad + radius / 2;
    let right = width - pad;

    // ── Title ──
    unsafe {
        let _ = SetBkMode(shell.dc(), TRANSPARENT);
    }
    unsafe {
        let _ = SelectObject(shell.dc(), hfont_title);
    }
    shell.draw_text(
        "mHD",
        &RECT {
            left,
            top: pad,
            right,
            bottom: pad + font_title_h.abs() * 3 / 2,
        },
        theme.text,
        DT_LEFT | DT_SINGLELINE,
    );

    // ── Version ──
    let ver_y = pad + font_title_h.abs() * 3 / 2 + 4;
    unsafe {
        let _ = SelectObject(shell.dc(), hfont_small);
    }
    shell.draw_text(
        &format!("v{}", env!("CARGO_PKG_VERSION")),
        &RECT {
            left,
            top: ver_y,
            right,
            bottom: ver_y + font_small_h.abs() * 3 / 2,
        },
        theme.text_muted,
        DT_LEFT | DT_SINGLELINE,
    );

    // ── Separator line ──
    let sep_y = ver_y + font_small_h.abs() * 3 / 2 + 12;
    shell.draw_separator(sep_y, left, right);

    // ── Body text ──
    let body_y = sep_y + 14;
    unsafe {
        let _ = SelectObject(shell.dc(), hfont_body);
    }

    let lines = [
        "Minimal Hotkey Daemon for Windows",
        "Native hotkeys, overlays, DDC/CI, and audio tools.",
        "Author: ArthurkaX",
        "https://github.com/ArthurkaX",
    ];

    let mut line_y = body_y;
    for line in &lines {
        let line_h = font_body_h.abs() * 3 / 2;
        shell.draw_text(
            line,
            &RECT {
                left,
                top: line_y,
                right,
                bottom: line_y + line_h,
            },
            theme.text,
            DT_LEFT | DT_SINGLELINE,
        );
        line_y += line_h + 4;
    }

    // ── Hint ──
    unsafe {
        let _ = SelectObject(shell.dc(), hfont_small);
    }
    shell.draw_text(
        "Click or press Esc to close",
        &RECT {
            left,
            top: line_y + 16,
            right,
            bottom: line_y + 16 + font_small_h.abs() * 3 / 2,
        },
        theme.text_muted,
        DT_LEFT | DT_SINGLELINE,
    );

    // ── Cleanup fonts ──
    unsafe {
        let _ = DeleteObject(hfont_title);
        let _ = DeleteObject(hfont_body);
        let _ = DeleteObject(hfont_small);
    }

    // ── Present ──
    let (x, y) = centered_position(width, height);
    shell.present(hwnd, x, y, 255);
}
