//! Styled native Win32 Settings panel (modal, tray‑thread).
//!
//! Fully layered per‑pixel‑alpha window (same technique as OSD/About).
//! All controls are drawn manually via GDI on a DIB — no child HWNDs,
//! so the window can be semi‑transparent with glass themes.
//!
//! Architecture
//! ────────────
//! • One DIB section, updated on paint / control changes.
//! • Hit‑testing done manually in `WM_NCHITTEST` + `WM_LBUTTONDOWN`.
//! • Combo box is emulated: a static text label with a click‑to‑expand
//!   popup list (a second layered popup).
//! • Buttons are hit‑tested rectangles drawn on the DIB.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::app::AppHandle;
use crate::native_theme::{load_theme_from_path, NativeTheme};
use crate::osd::{draw_rounded_rect, to_utf16_z};

// ── Layout constants (96 dpi base) ─────────────────────────────────

const WIN_WIDTH_BASE: i32 = 480;
const WIN_HEIGHT_BASE: i32 = 380;
const PADDING: i32 = 24;
const HEADER_HEIGHT_BASE: i32 = 64;
const FOOTER_HEIGHT_BASE: i32 = 52;
const ROW_HEIGHT_BASE: i32 = 32;
const ROW_GAP: i32 = 8;
const LABEL_WIDTH_BASE: i32 = 80;
const BTN_WIDTH_BASE: i32 = 100;
const BTN_HEIGHT_BASE: i32 = 30;
const COMBO_HIT_HEIGHT: i32 = 24;
const ROUND_RADIUS_BASE: f32 = 14.0;

// ── Combo popup constants ──────────────────────────────────────────

const COMBO_POPUP_WIDTH: i32 = 260;
const COMBO_POPUP_ITEM_HEIGHT: i32 = 24;
const COMBO_POPUP_MAX_VISIBLE: i32 = 8;

// ── Hit‑test regions ───────────────────────────────────────────────

const HT_HEADER: isize = 10; // custom region IDs above HTCAPTION
const HT_THEME_COMBO: isize = 20;
const HT_THEME_ARROW: isize = 21;
const HT_BTN_APPLY: isize = 30;
const HT_BTN_CLOSE: isize = 31;

// ── State ───────────────────────────────────────────────────────────

unsafe impl Send for Layout {}
unsafe impl Sync for Layout {}

#[derive(Copy, Clone)]
struct Layout {
    scale: f32,
    win_w: i32,
    win_h: i32,
    pad: i32,
    header_h: i32,
    footer_h: i32,
    row_h: i32,
    label_w: i32,
    combo_x: i32,
    combo_w: i32,
    combo_y: i32,
    arrow_x: i32,
    arrow_w: i32,
    btn_h: i32,
    btn_w: i32,
    btn_y: i32,
    apply_x: i32,
    close_x: i32,
    radius: i32,
}

struct SettingsState {
    handle: AppHandle,
    theme: NativeTheme,
    hwnd: HWND,
    layout: Layout,
    /// Theme names for the combo box
    theme_names: Vec<String>,
    /// Currently selected theme index
    theme_sel: usize,
    /// Combo popup window (when open)
    combo_popup: Option<HWND>,
    /// Whether the combo popup is open
    combo_open: Arc<AtomicBool>,
}

// ── Public API ──────────────────────────────────────────────────────

/// Open the mhd Settings panel on the current (tray) thread.
/// Blocks until the user dismisses the window.
pub fn show_config_editor(handle: AppHandle) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = to_utf16_z("mhd_settings_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(settings_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        return;
    }

    // Also register combo item class
    let item_cls = to_utf16_z("mhd_combo_item_cls");
    let item_wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(combo_item_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(item_cls.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&item_wc); }

    let theme = handle.theme();

    // Build theme list
    let theme_names = build_theme_list(&theme);
    let theme_sel = theme_names
        .iter()
        .position(|n| *n == theme.name)
        .unwrap_or(0);

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            WIN_WIDTH_BASE,
            WIN_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;
    let (win_w, win_h) = (
        (WIN_WIDTH_BASE as f32 * scale) as i32,
        (WIN_HEIGHT_BASE as f32 * scale) as i32,
    );
    let layout = compute_layout(scale);

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, win_w, win_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    let combo_open = Arc::new(AtomicBool::new(false));

    let state = Box::into_raw(Box::new(SettingsState {
        handle: handle.clone(),
        theme: theme.clone(),
        hwnd,
        layout,
        theme_names,
        theme_sel,
        combo_popup: None,
        combo_open,
    }));
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }

    // Paint initial content
    paint_settings(hwnd, state, &layout);

    // Center on primary monitor
    let work = monitor_work_rect();
    let pos_x = work.left + (work.right - work.left - win_w) / 2;
    let pos_y = work.top + (work.bottom - work.top - win_h) / 2;
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            pos_x,
            pos_y,
            win_w,
            win_h,
            SWP_NOZORDER | SWP_NOSIZE,
        );
    }

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }

    // Nested message loop
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

    // Free state
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
        if !ptr.is_null() {
            close_combo_popup(&mut *ptr);
            let _ = Box::from_raw(ptr);
        }
    }
}

// ── Layout ─────────────────────────────────────────────────────────

fn compute_layout(scale: f32) -> Layout {
    let pad = (PADDING as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;
    let btn_h = (BTN_HEIGHT_BASE as f32 * scale) as i32;
    let btn_w = (BTN_WIDTH_BASE as f32 * scale) as i32;
    let win_w = (WIN_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (WIN_HEIGHT_BASE as f32 * scale) as i32;
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
    let combo_x = pad + label_w + 8;
    let combo_w = win_w - pad * 2 - label_w - 8;
    let combo_y = header_h + (row_h - combo_h) / 2;

    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;

    let radius = (ROUND_RADIUS_BASE * scale) as i32;

    Layout {
        scale,
        win_w,
        win_h,
        pad,
        header_h,
        footer_h,
        row_h,
        label_w,
        combo_x,
        combo_w,
        combo_y,
        arrow_x: combo_x + combo_w - combo_h,
        arrow_w: combo_h,
        btn_h,
        btn_w,
        btn_y,
        apply_x: win_w - pad * 2 - btn_w * 2,
        close_x: win_w - pad - btn_w,
        radius,
    }
}

// ── Theme list ──────────────────────────────────────────────────────

fn build_theme_list(default_theme: &NativeTheme) -> Vec<String> {
    let mut names = Vec::new();
    names.push("built-in dark".to_string());

    let dir = crate::native_theme::themes_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(_stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(t) = load_theme_from_path(&path) {
                    if !names.contains(&t.name) {
                        names.push(t.name.clone());
                    }
                }
            }
        }
    }

    names.sort_by(|a, b| {
        if a == "built-in dark" {
            std::cmp::Ordering::Less
        } else if b == "built-in dark" {
            std::cmp::Ordering::Greater
        } else {
            a.to_lowercase().cmp(&b.to_lowercase())
        }
    });
    names.dedup();
    names
}

// ── Painting ───────────────────────────────────────────────────────

fn paint_settings(hwnd: HWND, state_ptr: *mut SettingsState, layout: &Layout) {
    let state = unsafe { &*state_ptr };
    let theme = &state.theme;
    let lay = layout;

    let screen_dc = unsafe { GetDC(None) };

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: lay.win_w,
            biHeight: -lay.win_h,
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
    let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(dib) = dib else {
        unsafe {
            let _ = ReleaseDC(None, screen_dc);
        }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    // ── Background rounded rect ────────────────────────────────────
    unsafe {
        let pixels =
            std::slice::from_raw_parts_mut(bits as *mut u32, (lay.win_w * lay.win_h) as usize);
        draw_rounded_rect(pixels, lay.win_w, lay.win_h, lay.radius, theme.background);
    }

    // ── GDI painting helpers ───────────────────────────────────────
    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
    }

    let title_font = create_font(-(18.0 * lay.scale) as i32, true, "Segoe UI");
    let body_font = create_font(-(12.0 * lay.scale) as i32, false, "Segoe UI");
    let small_font = create_font(-(10.0 * lay.scale) as i32, false, "Segoe UI");

    // ── Header: title ──────────────────────────────────────────────
    let old_font = unsafe { SelectObject(dib_dc, title_font) };
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut title_wz = to_utf16_z("mhd Settings");
    let mut title_rc = RECT {
        left: lay.pad,
        top: lay.pad / 2,
        right: lay.win_w - lay.pad,
        bottom: lay.pad / 2 + 18 + 6,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut title_wz, &mut title_rc, DT_LEFT | DT_SINGLELINE);
    }

    // Separator line under header
    unsafe {
        let _ = SelectObject(dib_dc, old_font);
    }
    let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: lay.pad,
                top: lay.header_h - 1,
                right: lay.win_w - lay.pad,
                bottom: lay.header_h,
            },
            sep_brush,
        );
        let _ = DeleteObject(sep_brush);
    }

    // Separator above footer
    let footer_y = lay.win_h - lay.footer_h;
    let sep2_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: lay.pad,
                top: footer_y,
                right: lay.win_w - lay.pad,
                bottom: footer_y + 1,
            },
            sep2_brush,
        );
        let _ = DeleteObject(sep2_brush);
    }

    // ── "Theme" label ──────────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, body_font);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut label_wz = to_utf16_z("Theme");
    let mut label_rc = RECT {
        left: lay.pad,
        top: lay.combo_y,
        right: lay.pad + lay.label_w,
        bottom: lay.combo_y + 24,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut label_wz,
            &mut label_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Combo box surface ──────────────────────────────────────────
    // Draw the background of the combo box
    let combo_surface = theme.surface;
    let combo_bg = combo_surface.to_premultiplied_argb_pixel();

    // Draw combo background rect
    unsafe {
        let pixels =
            std::slice::from_raw_parts_mut(bits as *mut u32, (lay.win_w * lay.win_h) as usize);
        for y in lay.combo_y..lay.combo_y + COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32) {
            for x in lay.combo_x..lay.combo_x + lay.combo_w {
                let idx = (y * lay.win_w + x) as usize;
                if idx < pixels.len() {
                    // Blend combo bg over window bg
                    let a = (combo_surface.a as u32) as u32;
                    if a == 255 {
                        pixels[idx] = combo_bg;
                    } else if a > 0 {
                        let bg = pixels[idx];
                        let ba = (bg >> 24) & 0xFF;
                        let br = (bg >> 16) & 0xFF;
                        let bg_ = (bg >> 8) & 0xFF;
                        let bb = bg & 0xFF;

                        let ca = combo_surface.a as u32;
                        let cr = combo_surface.r as u32;
                        let cg = combo_surface.g as u32;
                        let cb_ = combo_surface.b as u32;

                        let out_a = ba + (ca * (255 - ba)) / 255;
                        let out_r = (br * (255 - ca) + cr * ca) / 255;
                        let out_g = (bg_ * (255 - ca) + cg * ca) / 255;
                        let out_b = (bb * (255 - ca) + cb_ * ca) / 255;

                        pixels[idx] = (out_a.min(255) << 24)
                            | (out_r.min(255) << 16)
                            | (out_g.min(255) << 8)
                            | out_b.min(255);
                    }
                }
            }
        }
    }

    // ── Combo text + arrow ─────────────────────────────────────────
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
        let _ = SelectObject(dib_dc, body_font);
    }

    // Selected theme name
    let sel_name = state
        .theme_names
        .get(state.theme_sel)
        .map(|s| s.as_str())
        .unwrap_or("built-in dark");
    let mut sel_wz = to_utf16_z(sel_name);
    let text_x = lay.combo_x + 8;
    let mut text_rc = RECT {
        left: text_x,
        top: lay.combo_y,
        right: lay.arrow_x - 4,
        bottom: lay.combo_y + 24,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut sel_wz,
            &mut text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    // Arrow ▼
    let arrow_brush = unsafe { CreateSolidBrush(theme.text_muted.to_colorref()) };
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: lay.arrow_x,
                top: lay.combo_y,
                right: lay.arrow_x + lay.arrow_w,
                bottom: lay.combo_y + COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32),
            },
            arrow_brush,
        );
        let _ = DeleteObject(arrow_brush);
    }

    // Draw arrow character on arrow background
    unsafe {
        let _ = SetTextColor(dib_dc, theme.background.to_colorref());
    }
    let mut arrow_wz = to_utf16_z("▼");
    let mut arrow_rc = RECT {
        left: lay.arrow_x,
        top: lay.combo_y,
        right: lay.arrow_x + lay.arrow_w,
        bottom: lay.combo_y + 24,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut arrow_wz,
            &mut arrow_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Buttons ────────────────────────────────────────────────────
    // Apply
    draw_button(
        dib_dc,
        bits,
        lay.win_w,
        lay.apply_x,
        lay.btn_y,
        lay.btn_w,
        lay.btn_h,
        "Apply",
        theme,
        body_font,
    );

    // Close
    draw_button(
        dib_dc,
        bits,
        lay.win_w,
        lay.close_x,
        lay.btn_y,
        lay.btn_w,
        lay.btn_h,
        "Close",
        theme,
        body_font,
    );

    // ── Footer status text ─────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, small_font);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let mut status_wz = to_utf16_z("Set the colour theme for mhd UI elements.");
    let mut status_rc = RECT {
        left: lay.pad,
        top: footer_y + (lay.footer_h - 12) / 2,
        right: lay.win_w - lay.pad,
        bottom: lay.win_h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut status_wz,
            &mut status_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Cleanup GDI objects ────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(body_font);
        let _ = DeleteObject(small_font);
    }

    // ── UpdateLayeredWindow ────────────────────────────────────────
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE {
        cx: lay.win_w,
        cy: lay.win_h,
    };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            None, // keep current position
            Some(&sz),
            dib_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    // ── Cleanup DIB ────────────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

/// Draw a rectangular button on the DIB.
fn draw_button(
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
    theme: &NativeTheme,
    font: HFONT,
) {
    // Button background (accent colour, opaque)
    unsafe {
        let btn_bg = CreateSolidBrush(theme.accent.to_colorref());
        let _ = FillRect(dib_dc, &RECT { left: x, top: y, right: x + w, bottom: y + h }, btn_bg);
        let _ = DeleteObject(btn_bg);
    }

    // Button text
    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut lbl_wz = to_utf16_z(label);
    let mut lbl_rc = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut lbl_wz,
            &mut lbl_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

// ── Window procedure ────────────────────────────────────────────────

unsafe extern "system" fn settings_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => LRESULT(0),

        WM_NCHITTEST => {
            // Get cursor position in client coordinates
            let screen_x = (lparam.0 as i16) as i32;
            let screen_y = ((lparam.0 >> 16) as i16) as i32;
            let mut pt = POINT { x: screen_x, y: screen_y };
            let _ = ScreenToClient(hwnd, &mut pt);

            let state_ptr =
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if state_ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let state = &*state_ptr;
            let lay = &state.layout;

            // Header → drag
            if pt.y < lay.header_h {
                return LRESULT(HTCAPTION as isize);
            }

            // Theme combo hit area
            let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32);
            if pt.y >= lay.combo_y && pt.y < lay.combo_y + combo_h {
                if pt.x >= lay.combo_x && pt.x < lay.combo_x + lay.combo_w {
                    // Check if arrow area
                    if pt.x >= lay.arrow_x {
                        return LRESULT(HT_THEME_ARROW);
                    }
                    return LRESULT(HT_THEME_COMBO);
                }
            }

            // Apply button
            if pt.x >= lay.apply_x
                && pt.x < lay.apply_x + lay.btn_w
                && pt.y >= lay.btn_y
                && pt.y < lay.btn_y + lay.btn_h
            {
                return LRESULT(HT_BTN_APPLY);
            }

            // Close button
            if pt.x >= lay.close_x
                && pt.x < lay.close_x + lay.btn_w
                && pt.y >= lay.btn_y
                && pt.y < lay.btn_y + lay.btn_h
            {
                return LRESULT(HT_BTN_CLOSE);
            }

            // Everything else → transparent (pass through)
            LRESULT(HTTRANSPARENT as isize)
        }

        WM_LBUTTONDOWN => {
            let x = (lparam.0 as i16) as i32;
            let y = ((lparam.0 >> 16) as i16) as i32;

            let state_ptr =
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if state_ptr.is_null() {
                return LRESULT(0);
            }
            let state = &mut *state_ptr;
            let lay = state.layout;

            let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32);

            // Theme combo click
            if y >= lay.combo_y && y < lay.combo_y + combo_h && x >= lay.combo_x
                && x < lay.combo_x + lay.combo_w
            {
                toggle_combo_popup(state);
                return LRESULT(0);
            }

            // Apply button
            if x >= lay.apply_x
                && x < lay.apply_x + lay.btn_w
                && y >= lay.btn_y
                && y < lay.btn_y + lay.btn_h
            {
                apply_settings(state);
                paint_settings(hwnd, state_ptr, &state.layout);
                return LRESULT(0);
            }

            // Close button
            if x >= lay.close_x
                && x < lay.close_x + lay.btn_w
                && y >= lay.btn_y
                && y < lay.btn_y + lay.btn_h
            {
                DestroyWindow(hwnd).ok();
                return LRESULT(0);
            }

            LRESULT(0)
        }

        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if !ptr.is_null() {
                close_combo_popup(&mut *ptr);
                let _ = Box::from_raw(ptr);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Combo popup ─────────────────────────────────────────────────────

unsafe extern "system" fn combo_item_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            // Settings window is: hwnd (item) → popup → settings
            let popup = GetParent(hwnd).unwrap_or_default();
            let settings_parent = GetParent(popup).unwrap_or_default();
            let state_ptr =
                GetWindowLongPtrW(settings_parent, GWLP_USERDATA) as *mut SettingsState;
            let theme = if !state_ptr.is_null() {
                &(*state_ptr).theme
            } else {
                &NativeTheme::default()
            };

            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);

            // Background
            let bg = CreateSolidBrush(theme.surface.to_colorref());
            let _ = FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg);

            // Item index from window user data
            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize;
            if idx > 0 && !state_ptr.is_null() {
                let state = &*state_ptr;
                if let Some(name) = state.theme_names.get(idx) {
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let font =
                        create_font(-(12.0 * state.layout.scale) as i32, false, "Segoe UI");
                    let old_font = SelectObject(hdc, font);
                    let _ = SetTextColor(hdc, theme.text.to_colorref());

                    let mut wz = to_utf16_z(name);
                    let _ = DrawTextW(
                        hdc,
                        &mut wz,
                        &mut rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );

                    let _ = SelectObject(hdc, old_font);
                    let _ = DeleteObject(font);
                }
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            // Notify parent of selection
            let popup = GetParent(hwnd).unwrap_or_default();
            let settings_parent = GetParent(popup).unwrap_or_default();

            let idx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize;
            if idx > 0 {
                let state_ptr =
                    GetWindowLongPtrW(settings_parent, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if idx < state.theme_names.len() {
                        state.theme_sel = idx;
                        // Apply immediately
                        apply_settings(state);
                        // Close popup
                        close_combo_popup(state);
                        // Repaint main window
                        paint_settings(settings_parent, state_ptr, &state.layout);
                    }
                }
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn toggle_combo_popup(state: &mut SettingsState) {
    if state.combo_open.load(Ordering::SeqCst) {
        close_combo_popup(state);
    } else {
        open_combo_popup(state);
    }
}

fn open_combo_popup(state: &mut SettingsState) {
    if state.combo_open.load(Ordering::SeqCst) {
        return;
    }

    let parent = state.hwnd;
    let lay = state.layout;

    // Compute position below the combo box
    let mut combo_pt = POINT { x: lay.combo_x, y: lay.combo_y };
    unsafe { let _ = ClientToScreen(parent, &mut combo_pt); }

    let popup_w = COMBO_POPUP_WIDTH.max((COMBO_POPUP_WIDTH as f32 * lay.scale) as i32);
    let item_h = COMBO_POPUP_ITEM_HEIGHT.max((COMBO_POPUP_ITEM_HEIGHT as f32 * lay.scale) as i32);
    let count = state.theme_names.len().min(COMBO_POPUP_MAX_VISIBLE as usize);
    let popup_h = (count as i32) * item_h + 2;

    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();
    let cls_name = to_utf16_z("mhd_combo_item_cls");

    // Create a layered popup for the dropdown list
    let popup = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            combo_pt.x,
            combo_pt.y + COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32),
            popup_w,
            popup_h,
            parent,
            HMENU::default(),
            hinstance,
            None,
        )
    };

    let Ok(popup) = popup else { return };

    // Create child item windows
    for i in 0..state.theme_names.len().min(COMBO_POPUP_MAX_VISIBLE as usize) {
        let item = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR::from_raw(cls_name.as_ptr()),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                (i as i32) * item_h,
                popup_w,
                item_h,
                popup,
                HMENU::default(),
                hinstance,
                None,
            )
        };
        if let Ok(hwnd_item) = item {
            // Store theme index in window text
            let idx_text = format!("{}\0", i);
            let wide: Vec<u16> = idx_text.encode_utf16().collect();
            unsafe {
                let _ = SetWindowTextW(hwnd_item, PCWSTR::from_raw(wide.as_ptr()));
            }
        }
    }

    // Paint the popup
    paint_combo_popup(popup, state);

    state.combo_popup = Some(popup);
    state.combo_open.store(true, Ordering::SeqCst);

    unsafe {
        let _ = ShowWindow(popup, SW_SHOWNA);
    }
}

fn close_combo_popup(state: &mut SettingsState) {
    if let Some(popup) = state.combo_popup.take() {
        unsafe { DestroyWindow(popup).ok(); }
    }
    state.combo_open.store(false, Ordering::SeqCst);
}

fn paint_combo_popup(hwnd: HWND, state: &SettingsState) {
    let theme = &state.theme;
    let mut rc = RECT::default();
    unsafe { let _ = GetClientRect(hwnd, &mut rc); }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;

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

    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(dib) = dib else {
        unsafe { let _ = ReleaseDC(None, screen_dc); }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    // Background
    let bg_pixel = theme.surface.to_premultiplied_argb_pixel();
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        for px in pixels.iter_mut() {
            *px = bg_pixel;
        }
    }

    // Border
    let border_color = theme.border.to_colorref();
    let border_brush = unsafe { CreateSolidBrush(border_color) };
    unsafe {
        let _ = FillRect(dib_dc, &RECT { left: 0, top: 0, right: w, bottom: 1 }, border_brush);
        let _ = FillRect(dib_dc, &RECT { left: 0, top: h - 1, right: w, bottom: h }, border_brush);
        let _ = FillRect(dib_dc, &RECT { left: 0, top: 0, right: 1, bottom: h }, border_brush);
        let _ = FillRect(dib_dc, &RECT { left: w - 1, top: 0, right: w, bottom: h }, border_brush);
        let _ = DeleteObject(border_brush);
    }

    // UpdateLayeredWindow
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
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
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

// ── Apply logic ─────────────────────────────────────────────────────

fn apply_settings(state: &mut SettingsState) {
    let theme_name = state
        .theme_names
        .get(state.theme_sel)
        .cloned()
        .unwrap_or_else(|| "built-in dark".to_string());

    let config_name = if theme_name == "built-in dark" {
        String::new()
    } else {
        // Find the file stem matching this theme display name
        let themes_dir = crate::native_theme::themes_dir();
        let mut found = String::new();
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(t) = load_theme_from_path(&path) {
                        if t.name == theme_name {
                            found = stem.to_string();
                            break;
                        }
                    }
                    if stem == theme_name {
                        found = stem.to_string();
                        break;
                    }
                }
            }
        }
        if found.is_empty() {
            theme_name.clone()
        } else {
            found
        }
    };

    // Write to config.toml
    if let Err(e) = set_config_theme(&state.handle.config_path, &config_name) {
        eprintln!("mhd: settings error: {e}");
        return;
    }

    // Reload config (also reloads theme)
    if let Err(e) = state.handle.reload_config() {
        eprintln!("mhd: settings reload error: {e}");
        return;
    }

    // Update local theme
    state.theme = state.handle.theme();
}

fn set_config_theme(config_path: &std::path::Path, theme_name: &str) -> Result<(), String> {
    let content =
        std::fs::read_to_string(config_path).map_err(|e| format!("cannot read config: {e}"))?;

    let theme_line = if theme_name.is_empty() {
        None
    } else {
        Some(format!("theme = \"{theme_name}\""))
    };

    let mut lines: Vec<&str> = content.lines().collect();
    let mut found = false;
    let mut insert_pos: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("theme ") || trimmed.starts_with("theme=") {
            if let Some(ref tl) = theme_line {
                lines[i] = tl;
            } else {
                lines[i] = "";
            }
            found = true;
            break;
        }
        if insert_pos.is_none() && !trimmed.starts_with('#') && !trimmed.is_empty() {
            insert_pos = Some(i);
        }
    }

    if !found {
        if let Some(ref tl) = theme_line {
            if let Some(pos) = insert_pos {
                lines.insert(pos, tl);
            } else {
                lines.push(tl);
            }
        }
    }

    let new_content = lines.join("\r\n");
    std::fs::write(config_path, new_content).map_err(|e| format!("cannot write config: {e}"))?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

fn create_font(h: i32, bold: bool, family: &str) -> HFONT {
    let name = to_utf16_z(family);
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
