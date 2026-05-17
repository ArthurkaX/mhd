//! Styled native Win32 Settings panel (modal, tray‑thread).
//!
//! Provides a structured settings UI one control at a time, starting
//! with theme selection.  The window is a dark popup with rounded
//! region, styled consistently with the OSD and About dialog.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::app::AppHandle;
use crate::native_theme::NativeTheme;
use crate::osd::to_utf16_z;

// ── Combo box raw constants (not in windows-rs 0.58) ────────────────

const CBS_DROPDOWNLIST: u32 = 0x0003;
const CBS_HASSTRINGS: u32 = 0x0200;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const CB_GETLBTEXT: u32 = 0x0148;
const CB_ERR: i32 = -1;

// ── Control IDs ──────────────────────────────────────────────────────

const IDC_THEME_COMBO: usize = 1001;
const IDC_APPLY: usize = 1002;
const IDC_CLOSE: usize = 1003;

const ROUND_RADIUS_BASE: f32 = 14.0;
const PADDING: i32 = 24;
const HEADER_HEIGHT_BASE: i32 = 64;
const FOOTER_HEIGHT_BASE: i32 = 56;
const ROW_HEIGHT_BASE: i32 = 28;
const LABEL_WIDTH_BASE: i32 = 80;

// ── Window dimensions (96 dpi base) ─────────────────────────────────

const WIN_WIDTH_BASE: i32 = 480;
const WIN_HEIGHT_BASE: i32 = 320;

// ── State ───────────────────────────────────────────────────────────

struct SettingsState {
    handle: AppHandle,
    theme: NativeTheme,
    hwnd: HWND,
    theme_combo: HWND,
    // ── theme bookmark ──────────────────────────────────────────
    // ⋮ Add new fields here for future settings controls.
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

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
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

    // DPI scale
    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;

    let w = (WIN_WIDTH_BASE as f32 * scale) as i32;
    let h = (WIN_HEIGHT_BASE as f32 * scale) as i32;
    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);
    }

    // Create state
    let theme = handle.theme();
    let state = Box::into_raw(Box::new(SettingsState {
        handle: handle.clone(),
        theme: theme.clone(),
        hwnd,
        theme_combo: HWND::default(),
    }));
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }

    // Create controls
    create_controls(hwnd, hinstance, scale);

    // Populate theme combo (now that state has the combo HWND)
    unsafe {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
        if !state_ptr.is_null() {
            populate_theme_combo(&mut *state_ptr);
        }
    }

    // Apply rounded region
    let radius = (ROUND_RADIUS_BASE * scale) as i32;
    unsafe {
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, radius * 2, radius * 2);
        let _ = SetWindowRgn(hwnd, rgn, true);
    }

    // Center on primary monitor work area
    let work = monitor_work_rect();
    let pos_x = work.left + (work.right - work.left - w) / 2;
    let pos_y = work.top + (work.bottom - work.top - h) / 2;
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            pos_x,
            pos_y,
            w,
            h,
            SWP_NOZORDER | SWP_NOSIZE,
        );
    }

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    // ── theme bookmark ─────────────────────────────────────────────
    // Nested message loop – add new message intercepts here.

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
            let _ = Box::from_raw(ptr);
        }
    }
}

// ── Control creation ────────────────────────────────────────────────

fn create_controls(parent: HWND, hinstance: HINSTANCE, scale: f32) {
    let pad = (PADDING as f32 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;
    let btn_h = 28i32.max((28.0 * scale) as i32);
    let btn_w = (100.0 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;

    let mut rc = RECT::default();
    unsafe { let _ = GetClientRect(parent, &mut rc); }
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;

    // ── Theme row: combo box ───────────────────────────────────────
    let combo_x = pad + label_w + 8;
    let combo_w = win_w - pad * 2 - label_w - 8;
    let combo_h = 22i32.max((22.0 * scale) as i32);
    let combo_y = header_h + (row_h - combo_h) / 2;

    unsafe {
        let edit_cls = to_utf16_z("COMBOBOX");
        let combo = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(edit_cls.as_ptr()),
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE(CBS_DROPDOWNLIST | CBS_HASSTRINGS),
            combo_x,
            combo_y,
            combo_w,
            200, // drop-down height
            parent,
            HMENU(IDC_THEME_COMBO as isize as *mut c_void),
            hinstance,
            None,
        );

        if let Ok(hwnd_combo) = combo {
            let state_ptr =
                GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut SettingsState;
            if !state_ptr.is_null() {
                (*state_ptr).theme_combo = hwnd_combo;
            }
        }
    }

    // ── Buttons ───────────────────────────────────────────────────
    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;
    let btn_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32);
    let btn_cls = to_utf16_z("BUTTON");

    // Close
    unsafe {
        let label = to_utf16_z("Close");
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(btn_cls.as_ptr()),
            PCWSTR::from_raw(label.as_ptr()),
            btn_style,
            win_w - pad - btn_w,
            btn_y,
            btn_w,
            btn_h,
            parent,
            HMENU(IDC_CLOSE as isize as *mut c_void),
            hinstance,
            None,
        );
    }

    // Apply
    unsafe {
        let label = to_utf16_z("Apply");
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(btn_cls.as_ptr()),
            PCWSTR::from_raw(label.as_ptr()),
            btn_style,
            win_w - pad * 2 - btn_w * 2,
            btn_y,
            btn_w,
            btn_h,
            parent,
            HMENU(IDC_APPLY as isize as *mut c_void),
            hinstance,
            None,
        );
    }
}

/// Scan the themes directory for `.json` files and populate the combo box.
fn populate_theme_combo(state: &mut SettingsState) {
    let current_theme = state.theme.name.clone();
    let combo = state.theme_combo;

    // Read available themes from disk
    let themes_dir = crate::native_theme::themes_dir();
    let mut names: Vec<String> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&themes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                // Try to load the theme to get its display name
                if let Ok(t) = crate::native_theme::load_theme_from_path(&path) {
                    names.push(t.name.clone());
                } else {
                    names.push(stem.to_string());
                }
            }
        }
    }

    // Always offer the built-in dark theme first
    names.insert(0, "built-in dark".into());

    // Remove duplicates
    names.sort();
    names.dedup();

    unsafe {
        for (i, name) in names.iter().enumerate() {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = SendMessageW(
                combo,
                CB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );

            // Select if matches current theme
            if *name == current_theme {
                let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(i), LPARAM(0));
            }
        }

        // If nothing selected, pick first
        let cur = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0));
        if cur.0 as i32 == CB_ERR {
            let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));
        }
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

        WM_COMMAND => {
            let hi = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let id = (wparam.0 & 0xFFFF) as usize;

            let state_ptr =
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;

            // Button clicks
            if hi == 0 /* BN_CLICKED */ {
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &mut *state_ptr;

                match id {
                    IDC_APPLY => {
                        apply_settings(state);
                        InvalidateRect(hwnd, None, true);
                    }
                    IDC_CLOSE => {
                        DestroyWindow(hwnd).ok();
                    }
                    _ => {}
                }
                return LRESULT(0);
            }

            // ── theme bookmark ─────────────────────────────────────
            // Add new control message handling here.

            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let state_ptr =
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            let theme = if !state_ptr.is_null() {
                &(*state_ptr).theme
            } else {
                &crate::native_theme::NativeTheme::default()
            };

            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;

            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let pad = (PADDING as f32 * scale) as i32;
            let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
            let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;
            let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
            let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;

            // Background
            let bg_brush = CreateSolidBrush(theme.background.to_colorref());
            let _ = FillRect(hdc, &rc, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Header
            let _ = SetBkMode(hdc, TRANSPARENT);
            let old_font = SelectObject(
                hdc,
                create_font(-(18.0 * scale) as i32, true, "Segoe UI"),
            );
            let _ = SetTextColor(hdc, theme.text.to_colorref());

            let mut title_wz = to_utf16_z("mhd Settings");
            let mut title_rc = RECT {
                left: pad,
                top: pad / 2,
                right: w - pad,
                bottom: pad / 2 + 18 + 6,
            };
            let _ = DrawTextW(hdc, &mut title_wz, &mut title_rc, DT_LEFT | DT_SINGLELINE);

            // Separator under header
            let _ = SelectObject(hdc, old_font);
            let sep_brush = CreateSolidBrush(theme.border.to_colorref());
            let sep_rc = RECT {
                left: pad,
                top: header_h - 1,
                right: w - pad,
                bottom: header_h,
            };
            let _ = FillRect(hdc, &sep_rc, sep_brush);
            // Separator above footer
            let footer_y = h - footer_h;
            let sep2_rc = RECT {
                left: pad,
                top: footer_y,
                right: w - pad,
                bottom: footer_y + 1,
            };
            let _ = FillRect(hdc, &sep2_rc, sep_brush);
            let _ = DeleteObject(sep_brush);

            // ── Theme row ──────────────────────────────────────────
            let label_y = header_h + (row_h - 12) / 2;

            let _ = SelectObject(
                hdc,
                create_font(-(12.0 * scale) as i32, false, "Segoe UI"),
            );
            let _ = SetTextColor(hdc, theme.text.to_colorref());
            let mut label_wz = to_utf16_z("Theme");
            let mut label_rc = RECT {
                left: pad,
                top: label_y,
                right: pad + label_w,
                bottom: label_y + 16,
            };
            let _ = DrawTextW(
                hdc,
                &mut label_wz,
                &mut label_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );

            // ── theme bookmark ─────────────────────────────────────
            // Add new setting rows here – copy the pattern above.

            // Status / info text in footer
            let _ = SetTextColor(hdc, theme.text_muted.to_colorref());
            let status_text = "Set the colour theme for mhd UI elements.";

            let mut st_wz = to_utf16_z(status_text);
            let mut st_rc = RECT {
                left: pad,
                top: footer_y + (footer_h - 12) / 2,
                right: w - pad,
                bottom: h,
            };
            let _ = DrawTextW(
                hdc,
                &mut st_wz,
                &mut st_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut c_void);
            let state_ptr =
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let _ = SetTextColor(hdc, state.theme.text.to_colorref());
                let _ = SetBkColor(hdc, state.theme.surface.to_colorref());
                let brush = CreateSolidBrush(state.theme.surface.to_colorref());
                return LRESULT(brush.0 as isize);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if !ptr.is_null() {
                let _ = Box::from_raw(ptr);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Apply logic ─────────────────────────────────────────────────────

/// Gather settings from all controls and apply them.
fn apply_settings(state: &mut SettingsState) {
    let combo = state.theme_combo;

    // Get selected item index
    let sel = unsafe { SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)) };
    if sel.0 as i32 == CB_ERR {
        return;
    }

    // Get text of selected item
    let mut buf = vec![0u16; 256];
    let len = unsafe {
        SendMessageW(
            combo,
            CB_GETLBTEXT,
            WPARAM(sel.0 as usize),
            LPARAM(buf.as_mut_ptr() as isize),
        )
    };
    if len.0 as i32 == CB_ERR || len.0 == 0 {
        return;
    }
    buf.truncate(len.0 as usize);
    let theme_name = String::from_utf16_lossy(&buf);

    // Map display name → config file stem
    let config_name = if theme_name == "built-in dark" {
        String::new()
    } else {
        let themes_dir = crate::native_theme::themes_dir();
        let mut found = String::new();
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    // Try display name match
                    if let Ok(t) = crate::native_theme::load_theme_from_path(&path) {
                        if t.name == theme_name {
                            found = stem.to_string();
                            break;
                        }
                    }
                    // Fallback: stem match
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

    // Reload config (which also reloads theme)
    if let Err(e) = state.handle.reload_config() {
        eprintln!("mhd: settings reload error: {e}");
        return;
    }

    // Update local theme
    state.theme = state.handle.theme();
}

/// Write/update the `theme = "..."` line in config.toml.
fn set_config_theme(
    config_path: &std::path::Path,
    theme_name: &str,
) -> Result<(), String> {
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
        // Remember first non-comment line position for insertion
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
