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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::{AppHandle, SendHwnd};
use crate::hook::WM_BINDING_CAPTURED;
use crate::native_theme::{Argb, NativeTheme, load_theme_from_path};
use crate::osd::{draw_rounded_rect, to_utf16_z};
use crate::trigger::{KeyCombo, Modifiers, PhysicalKey, keys_to_string};

// ── Layout constants (96 dpi base) ─────────────────────────────────

const WIN_WIDTH_BASE: i32 = 480;
const WIN_HEIGHT_BASE: i32 = 380;
const PADDING: i32 = 24;
const HEADER_HEIGHT_BASE: i32 = 64;
const FOOTER_HEIGHT_BASE: i32 = 52;
const ROW_HEIGHT_BASE: i32 = 32;
const LABEL_WIDTH_BASE: i32 = 80;
const BTN_WIDTH_BASE: i32 = 100;
const BTN_HEIGHT_BASE: i32 = 30;
const COMBO_HIT_HEIGHT: i32 = 24;
const ROUND_RADIUS_BASE: f32 = 14.0;

// ── Combo popup constants ──────────────────────────────────────────

const COMBO_POPUP_WIDTH: i32 = 260;
const COMBO_POPUP_ITEM_HEIGHT: i32 = 24;
const COMBO_POPUP_MAX_VISIBLE: i32 = 8;
const WM_MOUSELEAVE: u32 = 0x02A3;
const EM_SETSEL: u32 = 0x00B1;

// ── State ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UIActionKind {
    ReplaceKey,
    RunPs,
    SetBrightness,
    Quit,
}

impl UIActionKind {
    fn to_str(&self) -> &'static str {
        match self {
            UIActionKind::ReplaceKey => "Replace Key",
            UIActionKind::RunPs => "PowerShell",
            UIActionKind::SetBrightness => "Brightness",
            UIActionKind::Quit => "Quit",
        }
    }

    fn all() -> Vec<UIActionKind> {
        vec![
            UIActionKind::ReplaceKey,
            UIActionKind::RunPs,
            UIActionKind::SetBrightness,
            UIActionKind::Quit,
        ]
    }
}

#[derive(Debug, Clone)]
struct UIBinding {
    trigger: String,
    kind: UIActionKind,
    param: String,
    is_recording_trigger: bool,
    is_recording_param: bool,
}

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
    list_y: i32,
    list_h: i32,
    row_h: i32,
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
    /// Currently hovered index in the popup
    hover_sel: Option<usize>,
    /// Combo popup window (when open)
    combo_popup: Option<HWND>,
    /// Whether the combo popup is open
    combo_open: Arc<AtomicBool>,

    /// List of bindings being edited
    bindings: Vec<UIBinding>,
    /// Vertical scroll offset in pixels
    scroll_y: i32,
    /// Currently recording (binding_idx, is_trigger)
    recording_info: Option<(usize, bool)>,
    /// Active inline edit control
    edit_control: Option<HWND>,
    /// Index of binding being edited inline
    edit_idx: Option<usize>,
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
    let _ = unsafe { RegisterClassW(&wc) };

    // Combo popup class — regular popup, no child windows.
    let popup_cls = to_utf16_z("mhd_combo_popup_cls");
    let popup_wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(combo_popup_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(popup_cls.as_ptr()),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&popup_wc);
    }

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

    let bindings = load_ui_bindings(&handle);

    let state = Box::into_raw(Box::new(SettingsState {
        handle: handle.clone(),
        theme: theme.clone(),
        hwnd,
        layout,
        theme_names,
        theme_sel,
        hover_sel: None,
        combo_popup: None,
        combo_open,
        bindings,
        scroll_y: 0,
        recording_info: None,
        edit_control: None,
        edit_idx: None,
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

    let list_y = header_h + row_h + pad / 2;
    let list_h = (win_h - footer_h) - list_y - pad / 2;

    Layout {
        scale,
        win_w,
        win_h,
        pad,
        header_h,
        footer_h,
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
        list_y,
        list_h,
        row_h,
    }
}

fn load_ui_bindings(handle: &AppHandle) -> Vec<UIBinding> {
    use crate::action::Action;
    use crate::trigger::keys_to_string;

    let config = handle.config.lock().unwrap();
    config
        .active_bindings()
        .iter()
        .map(|b| {
            let (kind, param) = match &b.action {
                Action::ReplaceKey { keys } => (UIActionKind::ReplaceKey, keys_to_string(keys)),
                Action::RunPs { command } => (UIActionKind::RunPs, command.clone()),
                Action::SetBrightness { relative, value } => {
                    let s = if *relative {
                        format!("{:+}", value)
                    } else {
                        format!("{}", value)
                    };
                    (UIActionKind::SetBrightness, s)
                }
                Action::Quit => (UIActionKind::Quit, String::new()),
                _ => (UIActionKind::Quit, "Unsupported".to_string()),
            };

            UIBinding {
                trigger: b.trigger_name.clone(),
                kind,
                param,
                is_recording_trigger: false,
                is_recording_param: false,
            }
        })
        .collect()
}

// ── Theme list ──────────────────────────────────────────────────────

fn build_theme_list(_default_theme: &NativeTheme) -> Vec<String> {
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
        let _ = DrawTextW(
            dib_dc,
            &mut title_wz,
            &mut title_rc,
            DT_LEFT | DT_SINGLELINE,
        );
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
        for y in lay.combo_y
            ..lay.combo_y + COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32)
        {
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
                bottom: lay.combo_y
                    + COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32),
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

    // ── Bindings List ──────────────────────────────────────────────
    unsafe {
        let _ = IntersectClipRect(
            dib_dc,
            lay.pad,
            lay.list_y,
            lay.win_w - lay.pad,
            lay.list_y + lay.list_h,
        );
    }

    let mut row_y = lay.list_y - state.scroll_y;
    for (i, b) in state.bindings.iter().enumerate() {
        if row_y + lay.row_h >= lay.list_y && row_y < lay.list_y + lay.list_h {
            draw_binding_row(
                dib_dc, bits, i, b, row_y, state, theme, body_font, small_font,
            );
        }
        row_y += lay.row_h;
    }

    // "Add New" button
    if row_y + lay.row_h >= lay.list_y && row_y < lay.list_y + lay.list_h {
        draw_button(
            dib_dc,
            bits,
            lay.win_w,
            lay.pad,
            row_y + (lay.row_h - lay.btn_h) / 2,
            (80.0 * lay.scale) as i32,
            lay.btn_h,
            "+ Add",
            theme,
            small_font,
        );
    }

    unsafe {
        let rgn = CreateRectRgn(0, 0, lay.win_w, lay.win_h);
        SelectClipRgn(dib_dc, rgn);
        let _ = DeleteObject(rgn);
    }

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

    // GDI writes RGB into a 32-bit DIB but often leaves alpha as 0.
    // For a layered window that makes buttons/lines/text transparent holes.
    // Restore alpha for newly drawn RGB pixels while preserving the original
    // per-pixel alpha of the rounded background (glass theme stays glassy).
    fix_gdi_alpha(bits, lay.win_w, lay.win_h, theme.background);

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
fn fix_gdi_alpha(
    bits: *mut c_void,
    width: i32,
    height: i32,
    background: crate::native_theme::Argb,
) {
    if bits.is_null() || width <= 0 || height <= 0 {
        return;
    }

    let bg_px = background.to_premultiplied_argb_pixel();
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        for px in pixels.iter_mut() {
            let _a = (*px >> 24) & 0xff;
            let rgb = *px & 0x00ff_ffff;

            // Do not touch transparent outside corners.
            if *px == 0 {
                continue;
            }

            // Preserve the original glass background (including anti-aliased
            // rounded corners). Everything else is foreground UI drawn by GDI
            // and should be fully opaque.
            if is_background_like_pixel(*px, bg_px, background.a) {
                continue;
            }

            *px = 0xff00_0000 | rgb;
        }
    }
}

fn is_background_like_pixel(px: u32, bg_px: u32, bg_alpha: u8) -> bool {
    if px == bg_px {
        return true;
    }

    let a = ((px >> 24) & 0xff) as u8;
    let rgb = px & 0x00ff_ffff;
    let bg_rgb = bg_px & 0x00ff_ffff;

    // Common glass case: black background. Anti-aliased corners have the
    // same RGB but lower alpha; keep them translucent.
    rgb == bg_rgb && a <= bg_alpha
}

/// Returns true if white text has sufficient contrast on this background.
fn contrast_text_on(bg: Argb) -> bool {
    let r = bg.r as f32 / 255.0;
    let g = bg.g as f32 / 255.0;
    let b = bg.b as f32 / 255.0;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

fn draw_button(
    dib_dc: HDC,
    _bits: *mut c_void,
    _win_w: i32,
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
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            },
            btn_bg,
        );
        let _ = DeleteObject(btn_bg);
    }

    // Button text — pick contrasting colour based on accent luminance
    let btn_text_color = if contrast_text_on(theme.accent) {
        Argb::new(0xFF, 0xFF, 0xFF, 0xFF) // white
    } else {
        Argb::new(0xFF, 0x00, 0x00, 0x00) // black
    };
    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, btn_text_color.to_colorref());
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
    unsafe {
        match msg {
            WM_CREATE => LRESULT(0),

            WM_NCHITTEST => {
                // Get cursor position in client coordinates
                let screen_x = (lparam.0 as i16) as i32;
                let screen_y = ((lparam.0 >> 16) as i16) as i32;
                let mut pt = POINT {
                    x: screen_x,
                    y: screen_y,
                };
                let _ = ScreenToClient(hwnd, &mut pt);

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &*state_ptr;
                let lay = &state.layout;

                // Header → drag, but only outside control rows.
                if pt.y < lay.header_h {
                    return LRESULT(HTCAPTION as isize);
                }

                // Everything else is normal client area. Do NOT return custom
                // HT_* values here: Windows then sends WM_NCLBUTTONDOWN instead
                // of WM_LBUTTONDOWN, so our controls never receive clicks.
                LRESULT(HTCLIENT as isize)
            }

            WM_LBUTTONDOWN => {
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if state_ptr.is_null() {
                    return LRESULT(0);
                }
                let state = &mut *state_ptr;
                let lay = state.layout;

                let combo_h = (COMBO_HIT_HEIGHT as f32 * lay.scale) as i32;

                // Theme combo click
                if y >= lay.combo_y
                    && y < lay.combo_y + combo_h
                    && x >= lay.combo_x
                    && x < lay.combo_x + lay.combo_w
                {
                    toggle_combo_popup(state);
                    return LRESULT(0);
                }

                // ── Bindings list interaction ────────────────────────
                if y >= lay.list_y && y < lay.list_y + lay.list_h {
                    // Close any active edit if clicking elsewhere
                    finish_inline_edit(state);

                    let mut row_y = lay.list_y - state.scroll_y;
                    let mut clicked = false;

                    for i in 0..state.bindings.len() {
                        if y >= row_y && y < row_y + lay.row_h {
                            handle_list_click(state, i, x, y, row_y);
                            clicked = true;
                            break;
                        }
                        row_y += lay.row_h;
                    }

                    if !clicked && y >= row_y && y < row_y + lay.row_h {
                        state.bindings.push(UIBinding {
                            trigger: "none".to_string(),
                            kind: UIActionKind::ReplaceKey,
                            param: "".to_string(),
                            is_recording_trigger: false,
                            is_recording_param: false,
                        });
                        paint_settings(hwnd, state_ptr, &lay);
                        return LRESULT(0);
                    }
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

            WM_COMMAND => {
                let code = (wparam.0 as u32 >> 16) as u16;
                if code == EN_KILLFOCUS as u16 {
                    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                    if !state_ptr.is_null() {
                        let state = &mut *state_ptr;
                        finish_inline_edit(state);
                    }
                }
                LRESULT(0)
            }

            WM_MOUSEWHEEL => {
                let delta = (wparam.0 as i32 >> 16) as i16;
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let lay = state.layout;
                    let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
                    let max_scroll = (content_h - lay.list_h).max(0);
                    state.scroll_y =
                        (state.scroll_y - (delta as i32 / 120) * 40).clamp(0, max_scroll);
                    paint_settings(hwnd, state_ptr, &lay);
                }
                LRESULT(0)
            }

            WM_BINDING_CAPTURED => {
                let data = lparam.0 as usize;
                let mods = Modifiers((data & 0xFF) as u8);
                let key_type = (data >> 8) & 0xFF;
                let key_val = (data >> 16) & 0xFF;

                let key = if key_type == 0 {
                    PhysicalKey::Keyboard(key_val as u8)
                } else {
                    PhysicalKey::MouseButton(key_val as u8)
                };

                let trigger_str = keys_to_string(&KeyCombo {
                    modifiers: mods,
                    key: Some(key),
                });

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if let Some((idx, is_trigger)) = state.recording_info.take() {
                        if is_trigger {
                            state.bindings[idx].trigger = trigger_str;
                            state.bindings[idx].is_recording_trigger = false;
                        } else {
                            state.bindings[idx].param = trigger_str;
                            state.bindings[idx].is_recording_param = false;
                        }
                        *state.handle.recording_window.lock().unwrap() = None;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
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
}

// ── Combo popup ─────────────────────────────────────────────────────

unsafe extern "system" fn combo_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);

                // State pointer stored in the popup itself by open_combo_popup.
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;

                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                let (theme, scale) = if !state_ptr.is_null() {
                    (&(*state_ptr).theme, (*state_ptr).layout.scale)
                } else {
                    (&NativeTheme::default(), 1.0)
                };

                let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * scale) as i32;

                // Background — use main background colour, not surface.
                // Surface is often transparent/light and makes text unreadable.
                let bg = CreateSolidBrush(theme.background.to_colorref());
                let _ = FillRect(hdc, &rc, bg);
                let _ = DeleteObject(bg);

                // Border
                let border_brush = CreateSolidBrush(theme.border.to_colorref());
                let _ = FillRect(
                    hdc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: w,
                        bottom: 1,
                    },
                    border_brush,
                );
                let _ = FillRect(
                    hdc,
                    &RECT {
                        left: 0,
                        top: h - 1,
                        right: w,
                        bottom: h,
                    },
                    border_brush,
                );
                let _ = FillRect(
                    hdc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: 1,
                        bottom: h,
                    },
                    border_brush,
                );
                let _ = FillRect(
                    hdc,
                    &RECT {
                        left: w - 1,
                        top: 0,
                        right: w,
                        bottom: h,
                    },
                    border_brush,
                );
                let _ = DeleteObject(border_brush);

                // Draw each item
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let font_h = -(12.0 * state.layout.scale) as i32;
                    let font = create_font(font_h, false, "Segoe UI");
                    let old_font = SelectObject(hdc, font);

                    for i in 0..state
                        .theme_names
                        .len()
                        .min(COMBO_POPUP_MAX_VISIBLE as usize)
                    {
                        let item_y = (i as i32) * item_h;
                        let item_rc = RECT {
                            left: 2,
                            top: item_y,
                            right: w - 2,
                            bottom: item_y + item_h,
                        };

                        // Hover/selected highlight
                        let highlight = if i == state.theme_sel {
                            Some(theme.selected)
                        } else if state.hover_sel == Some(i) {
                            Some(theme.hover)
                        } else {
                            None
                        };

                        if let Some(c) = highlight {
                            let blended = c.blend_over(theme.background);
                            let sel_brush = CreateSolidBrush(blended.to_colorref());
                            let _ = FillRect(hdc, &item_rc, sel_brush);
                            let _ = DeleteObject(sel_brush);
                        }

                        let _ = SetTextColor(hdc, theme.text.to_colorref());
                        if let Some(name) = state.theme_names.get(i) {
                            let mut wz = to_utf16_z(name);
                            let _ = DrawTextW(
                                hdc,
                                &mut wz,
                                &mut RECT {
                                    left: 8,
                                    top: item_y,
                                    right: w - 8,
                                    bottom: item_y + item_h,
                                },
                                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                            );
                        }
                    }

                    let _ = SelectObject(hdc, old_font);
                    let _ = DeleteObject(font);
                }

                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                let y = ((lparam.0 >> 16) as i16) as i32;
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;

                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * state.layout.scale) as i32;
                    // y includes 1px top border
                    let inner_y = if y > 0 { y - 1 } else { 0 };
                    let idx = (inner_y / item_h) as usize;

                    let new_hover = if idx < state.theme_names.len() {
                        Some(idx)
                    } else {
                        None
                    };

                    if state.hover_sel != new_hover {
                        state.hover_sel = new_hover;
                        let _ = InvalidateRect(hwnd, None, false);

                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                LRESULT(0)
            }

            WM_MOUSELEAVE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.hover_sel.is_some() {
                        state.hover_sel = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                let y = ((lparam.0 >> 16) as i16) as i32;
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;

                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * state.layout.scale) as i32;
                    // y includes 1px top border
                    let inner_y = if y > 0 { y - 1 } else { 0 };
                    let idx = inner_y / item_h;
                    if idx >= 0 && (idx as usize) < state.theme_names.len() {
                        state.theme_sel = idx as usize;
                        apply_settings(state);
                        close_combo_popup(state);
                        paint_settings(state.hwnd, state_ptr, &state.layout);
                    }
                }
                LRESULT(0)
            }

            WM_ACTIVATE => {
                // If losing activation, close popup
                if loword(wparam.0 as u32) == 0 {
                    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                    if !state_ptr.is_null() {
                        close_combo_popup(&mut *state_ptr);
                    }
                }
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn loword(dw: u32) -> u16 {
    (dw & 0xffff) as u16
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
    let state_ptr = state as *mut SettingsState;

    // Compute position below the combo box
    let mut combo_pt = POINT {
        x: lay.combo_x,
        y: lay.combo_y,
    };
    unsafe {
        let _ = ClientToScreen(parent, &mut combo_pt);
    }

    let popup_w = COMBO_POPUP_WIDTH.max((COMBO_POPUP_WIDTH as f32 * lay.scale) as i32);
    let item_h = COMBO_POPUP_ITEM_HEIGHT.max((COMBO_POPUP_ITEM_HEIGHT as f32 * lay.scale) as i32);
    let count = state
        .theme_names
        .len()
        .min(COMBO_POPUP_MAX_VISIBLE as usize);
    let popup_h = (count as i32) * item_h + 2;

    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();
    let cls_name = to_utf16_z("mhd_combo_popup_cls");

    // Regular popup window (not layered), no child HWNDs.
    // The popup wndproc paints all items directly and handles hits by y.
    let popup = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
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

    // Store state pointer so the popup wndproc can read theme/item info.
    unsafe {
        let _ = SetWindowLongPtrW(popup, GWLP_USERDATA, state_ptr as isize);
    }

    state.combo_popup = Some(popup);
    state.combo_open.store(true, Ordering::SeqCst);

    unsafe {
        let _ = ShowWindow(popup, SW_SHOWNA);
    }
}

fn close_combo_popup(state: &mut SettingsState) {
    if let Some(popup) = state.combo_popup.take() {
        unsafe {
            DestroyWindow(popup).ok();
        }
    }
    state.combo_open.store(false, Ordering::SeqCst);
}

fn draw_binding_row(
    hdc: HDC,
    bits: *mut c_void,
    _idx: usize,
    binding: &UIBinding,
    y: i32,
    state: &SettingsState,
    theme: &NativeTheme,
    _font: HFONT,
    small_font: HFONT,
) {
    let lay = &state.layout;
    let row_rc = RECT {
        left: lay.pad,
        top: y,
        right: lay.win_w - lay.pad,
        bottom: y + lay.row_h,
    };

    // 1. Trigger button
    let trig_w = (120.0 * lay.scale) as i32;
    let trig_rc = RECT {
        left: row_rc.left,
        top: y + (lay.row_h - lay.btn_h) / 2,
        right: row_rc.left + trig_w,
        bottom: y + (lay.row_h - lay.btn_h) / 2 + lay.btn_h,
    };
    let trig_text = if binding.is_recording_trigger {
        "..."
    } else {
        &binding.trigger
    };
    draw_button(
        hdc,
        bits,
        lay.win_w,
        trig_rc.left,
        trig_rc.top,
        trig_w,
        lay.btn_h,
        trig_text,
        theme,
        small_font,
    );

    // 2. Action kind button
    let kind_x = trig_rc.right + (8.0 * lay.scale) as i32;
    let kind_w = (100.0 * lay.scale) as i32;
    draw_button(
        hdc,
        bits,
        lay.win_w,
        kind_x,
        trig_rc.top,
        kind_w,
        lay.btn_h,
        binding.kind.to_str(),
        theme,
        small_font,
    );

    // 3. Param area
    let param_x = kind_x + kind_w + (8.0 * lay.scale) as i32;
    let param_w = row_rc.right - param_x - (32.0 * lay.scale) as i32;
    let param_rc = RECT {
        left: param_x,
        top: trig_rc.top,
        right: param_x + param_w,
        bottom: trig_rc.bottom,
    };

    let param_bg = theme.surface.blend_over(theme.background);
    let brush = unsafe { CreateSolidBrush(param_bg.to_colorref()) };
    unsafe {
        let _ = FillRect(hdc, &param_rc, brush);
        let _ = DeleteObject(brush);

        let _ = SetTextColor(hdc, theme.text.to_colorref());
        let mut wz = to_utf16_z(&binding.param);
        let mut text_rc = RECT {
            left: param_rc.left + 4,
            ..param_rc
        };
        let _ = DrawTextW(
            hdc,
            &mut wz,
            &mut text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // 4. Delete button
    let del_w = (24.0 * lay.scale) as i32;
    let del_rc = RECT {
        left: row_rc.right - del_w,
        top: y + (lay.row_h - del_w) / 2,
        right: row_rc.right,
        bottom: y + (lay.row_h - del_w) / 2 + del_w,
    };
    draw_button(
        hdc,
        bits,
        lay.win_w,
        del_rc.left,
        del_rc.top,
        del_w,
        del_w,
        "X",
        theme,
        small_font,
    );
}

fn handle_list_click(state: &mut SettingsState, idx: usize, x: i32, y: i32, row_y: i32) {
    let lay = state.layout;

    // 1. Trigger button
    let trig_w = (120.0 * lay.scale) as i32;
    if x >= lay.pad
        && x < lay.pad + trig_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        // Toggle recording trigger
        let is_recording = !state.bindings[idx].is_recording_trigger;

        // Turn off all recording
        for b in state.bindings.iter_mut() {
            b.is_recording_trigger = false;
            b.is_recording_param = false;
        }

        if is_recording {
            state.bindings[idx].is_recording_trigger = true;
            state.recording_info = Some((idx, true));
            *state.handle.recording_window.lock().unwrap() = Some(SendHwnd(state.hwnd));
        } else {
            state.recording_info = None;
            *state.handle.recording_window.lock().unwrap() = None;
        }

        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }

    // 2. Kind button
    let kind_x = lay.pad + trig_w + (8.0 * lay.scale) as i32;
    let kind_w = (100.0 * lay.scale) as i32;
    if x >= kind_x
        && x < kind_x + kind_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        // Cycle kinds for now
        let kinds = UIActionKind::all();
        let cur_idx = kinds
            .iter()
            .position(|&k| k == state.bindings[idx].kind)
            .unwrap_or(0);
        state.bindings[idx].kind = kinds[(cur_idx + 1) % kinds.len()];
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }

    // 2b. Param
    let param_x = kind_x + kind_w + (8.0 * lay.scale) as i32;
    let param_w = lay.win_w - lay.pad - (32.0 * lay.scale) as i32 - param_x;
    if x >= param_x
        && x < param_x + param_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        if state.bindings[idx].kind == UIActionKind::ReplaceKey {
            let is_recording = !state.bindings[idx].is_recording_param;
            for b in state.bindings.iter_mut() {
                b.is_recording_trigger = false;
                b.is_recording_param = false;
            }
            if is_recording {
                state.bindings[idx].is_recording_param = true;
                state.recording_info = Some((idx, false));
                *state.handle.recording_window.lock().unwrap() = Some(SendHwnd(state.hwnd));
            } else {
                state.recording_info = None;
                *state.handle.recording_window.lock().unwrap() = None;
            }
        } else if state.bindings[idx].kind == UIActionKind::RunPs
            || state.bindings[idx].kind == UIActionKind::SetBrightness
        {
            let rc = RECT {
                left: param_x,
                top: row_y + (lay.row_h - lay.btn_h) / 2,
                right: param_x + param_w,
                bottom: row_y + (lay.row_h + lay.btn_h) / 2,
            };
            spawn_inline_edit(state, idx, rc);
        }
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }

    // 3. Delete button
    let del_w = (24.0 * lay.scale) as i32;
    if x >= lay.win_w - lay.pad - del_w
        && x < lay.win_w - lay.pad
        && y >= row_y + (lay.row_h - del_w) / 2
        && y < row_y + (lay.row_h + del_w) / 2
    {
        state.bindings.remove(idx);
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }
}

fn spawn_inline_edit(state: &mut SettingsState, idx: usize, rc: RECT) {
    if let Some(old) = state.edit_control.take() {
        unsafe {
            let _ = DestroyWindow(old);
        }
    }

    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let cls = to_utf16_z("EDIT");
    let text = to_utf16_z(&state.bindings[idx].param);

    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::from_raw(text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            state.hwnd,
            HMENU::default(),
            hinst,
            None,
        )
    };

    if let Ok(h) = hwnd {
        unsafe {
            let _ = SetFocus(h);
            let _ = SendMessageW(h, EM_SETSEL, WPARAM(0), LPARAM(-1));
        }
        state.edit_control = Some(h);
        state.edit_idx = Some(idx);
    }
}

fn finish_inline_edit(state: &mut SettingsState) {
    if let Some(h) = state.edit_control.take() {
        if let Some(idx) = state.edit_idx.take() {
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(h, &mut buf) };
            if len > 0 {
                state.bindings[idx].param = String::from_utf16_lossy(&buf[..len as usize]);
            } else {
                state.bindings[idx].param = "".to_string();
            }
        }
        unsafe {
            let _ = DestroyWindow(h);
        }
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
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
    if let Err(e) = save_config(
        &state.handle.config_path,
        &config_name,
        &state.bindings,
        &state.handle,
    ) {
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

fn save_config(
    path: &std::path::Path,
    theme: &str,
    bindings: &[UIBinding],
    handle: &AppHandle,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut toml_val: toml::Value = if content.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        toml::from_str(&content).map_err(|e| e.to_string())?
    };

    let active_scheme = handle.config.lock().unwrap().active_scheme().to_string();

    if let Some(table) = toml_val.as_table_mut() {
        // Update theme
        if theme.is_empty() {
            table.remove("theme");
        } else {
            table.insert("theme".to_string(), toml::Value::String(theme.to_string()));
        }

        // Update active_scheme
        table.insert(
            "active_scheme".to_string(),
            toml::Value::String(active_scheme),
        );

        // Update bindings
        let mut new_bindings = Vec::new();
        for b in bindings {
            let mut map = toml::value::Table::new();
            map.insert(
                "trigger".to_string(),
                toml::Value::String(b.trigger.clone()),
            );

            let (action, param_key) = match b.kind {
                UIActionKind::ReplaceKey => ("replace_key", "keys"),
                UIActionKind::RunPs => ("run_ps", "command"),
                UIActionKind::SetBrightness => ("set_brightness", "value"),
                UIActionKind::Quit => ("quit", ""),
            };

            map.insert(
                "action".to_string(),
                toml::Value::String(action.to_string()),
            );
            if !param_key.is_empty() {
                map.insert(param_key.to_string(), toml::Value::String(b.param.clone()));
            }

            new_bindings.push(toml::Value::Table(map));
        }
        table.insert("binding".to_string(), toml::Value::Array(new_bindings));
    }

    let new_content = toml::to_string_pretty(&toml_val).map_err(|e| e.to_string())?;
    std::fs::write(path, new_content).map_err(|e| e.to_string())?;
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────

fn create_font(h: i32, bold: bool, family: &str) -> HFONT {
    let name = to_utf16_z(family);
    unsafe {
        CreateFontW(
            h,
            0,
            0,
            0,
            if bold {
                FW_BOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
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
