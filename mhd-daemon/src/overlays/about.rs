//! Styled native Win32 About dialog — layered rounded‑rect window
//! with themed visual style matching the OSD.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::native_theme::NativeTheme;
use crate::osd::{draw_rounded_rect, to_utf16_z};

// ── Layout constants (96 dpi base) ────────────────────────────────────

const ABT_WIDTH_BASE: i32 = 360;
const ABT_HEIGHT_BASE: i32 = 180;
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
    unsafe { RegisterClassW(&wc); }

    // No WS_EX_NOACTIVATE — modal dialogs should accept focus for Esc/Enter.
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
            break; // WM_QUIT
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn about_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            unsafe { DestroyWindow(hwnd).ok(); }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if wparam.0 == 0x1B /* VK_ESCAPE */ || wparam.0 == 0x0D /* VK_RETURN */ {
                unsafe { DestroyWindow(hwnd).ok(); }
                return LRESULT(0);
            }
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0); }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ── Painting ──────────────────────────────────────────────────────────

fn paint_about(hwnd: HWND, width: i32, height: i32, scale: f32, theme: &NativeTheme) {
    let screen_dc = unsafe { GetDC(None) };

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
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

    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe {
        CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
    };
    let Ok(dib) = dib else {
        unsafe { let _ = ReleaseDC(None, screen_dc); }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    // Background
    let radius = (ROUND_RADIUS_BASE * scale) as i32;
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        draw_rounded_rect(pixels, width, height, radius, theme.background);
    }

    // Fonts
    let font_name = to_utf16_z("Segoe UI");
    let font_title_h = -(18.0 * scale) as i32;
    let font_body_h = -(12.0 * scale) as i32;
    let font_small_h = -(10.0 * scale) as i32;

    let hfont_title = create_font(font_title_h, true, &font_name);
    let hfont_body = create_font(font_body_h, false, &font_name);
    let hfont_small = create_font(font_small_h, false, &font_name);

    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
    }

    let pad = (PADDING as f32 * scale) as i32;
    let left = pad + radius / 2;
    let right = width - pad;

    // ── Title ──
    let old_font = unsafe { SelectObject(dib_dc, hfont_title) };
    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }
    let mut title_wz = to_utf16_z("mhd");
    let mut title_rc = RECT {
        left,
        top: pad,
        right,
        bottom: pad + font_title_h.abs() * 3 / 2,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut title_wz, &mut title_rc, DT_LEFT | DT_SINGLELINE);
    }

    // ── Version ──
    let ver_y = title_rc.bottom + 4;
    unsafe { let _ = SelectObject(dib_dc, hfont_small); }
    unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut ver_wz = to_utf16_z(&ver);
    let mut ver_rc = RECT {
        left,
        top: ver_y,
        right,
        bottom: ver_y + font_small_h.abs() * 3 / 2,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut ver_wz, &mut ver_rc, DT_LEFT | DT_SINGLELINE);
    }

    // ── Separator line ──
    let sep_y = ver_rc.bottom + 12;
    let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
    let sep_rc = RECT {
        left,
        top: sep_y,
        right,
        bottom: sep_y + (1.0 * scale).max(1.0) as i32,
    };
    unsafe {
        let _ = FillRect(dib_dc, &sep_rc, sep_brush);
        let _ = DeleteObject(sep_brush);
    }

    // ── Body text ──
    let body_y = sep_rc.bottom + 14;
    unsafe { let _ = SelectObject(dib_dc, hfont_body); }
    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }

    let lines = [
        "Mouse & Hotkey Daemon for Windows",
        "Lightweight, single‑binary, DDC/CI support.",
    ];

    let mut line_y = body_y;
    for line in &lines {
        let mut lwz = to_utf16_z(line);
        let mut lrc = RECT {
            left,
            top: line_y,
            right,
            bottom: line_y + font_body_h.abs() * 3 / 2,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut lwz, &mut lrc, DT_LEFT | DT_SINGLELINE);
        }
        line_y = lrc.bottom + 4;
    }

    // ── Hint ──
    unsafe { let _ = SelectObject(dib_dc, hfont_small); }
    unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
    let mut hint_wz = to_utf16_z("Click or press Esc to close");
    let mut hint_rc = RECT {
        left,
        top: line_y + 16,
        right,
        bottom: line_y + 16 + font_small_h.abs() * 3 / 2,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut hint_wz, &mut hint_rc, DT_LEFT | DT_SINGLELINE);
    }

    // ── Cleanup ──
    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(hfont_title);
        let _ = DeleteObject(hfont_body);
        let _ = DeleteObject(hfont_small);
    }

    // ── UpdateLayeredWindow ──
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: width, cy: height };

    let work = monitor_work_rect();
    let pt_dst = POINT {
        x: work.left + (work.right - work.left - width) / 2,
        y: work.top + (work.bottom - work.top - height) / 2,
    };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            Some(&pt_dst),
            Some(&sz),
            dib_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

fn create_font(h: i32, bold: bool, name: &[u16]) -> HFONT {
    unsafe {
        CreateFontW(
            h, 0, 0, 0,
            if bold { FW_BOLD.0 as i32 } else { FW_NORMAL.0 as i32 },
            0, 0, 0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(name.as_ptr()),
        )
    }
}

fn monitor_work_rect() -> RECT {
    unsafe {
        let desktop = GetDesktopWindow();
        let hmon = MonitorFromWindow(desktop, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}
