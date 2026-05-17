//! Styled native Win32 config‑file editor (modal, tray‑thread).
//!
//! Displays a dark‑theme popup with a multiline `EDIT` control for
//! modifying `config.toml`.  Supports Save, Save & Reload, TOML
//! validation, and dirty‑tracking with close confirmation.
//
//  Design note
//  ───────────
//  The window uses a normal popup with a rounded region rather than
//  a per‑pixel layered window, because native child `EDIT` controls
//  do not work reliably under `UpdateLayeredWindow` parents.

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
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;

use crate::app::AppHandle;
use crate::native_theme::NativeTheme;
use crate::osd::to_utf16_z;

// ── Control IDs ──────────────────────────────────────────────────────

const IDC_EDIT: usize = 1001;
const IDC_SAVE: usize = 1002;
const IDC_SAVE_RELOAD: usize = 1003;
const IDC_CANCEL: usize = 1004;

const ROUND_RADIUS_BASE: f32 = 14.0;
const PADDING: i32 = 20;
const HEADER_HEIGHT_BASE: i32 = 72;
const FOOTER_HEIGHT_BASE: i32 = 56;
const BUTTON_HEIGHT_BASE: i32 = 30;
const BUTTON_WIDTH_BASE: i32 = 120;

// ── Window dimensions (96 dpi base) ─────────────────────────────────

const WIN_WIDTH_BASE: i32 = 780;
const WIN_HEIGHT_BASE: i32 = 560;

// ── State ───────────────────────────────────────────────────────────

struct ConfigEditorState {
    handle: AppHandle,
    theme: NativeTheme,
    hwnd: HWND,
    edit: HWND,
    dirty: bool,
    loading: bool,
    status_text: String,
    surface_brush: HBRUSH,
    background_brush: HBRUSH,
}

// ── Public API ──────────────────────────────────────────────────────

/// Open the config editor on the current (tray) thread.
/// Blocks until the user dismisses the window.
pub fn show_config_editor(handle: AppHandle) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = to_utf16_z("mhd_config_editor_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(editor_wndproc),
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

    // Create state and attach to window
    let theme = handle.theme();
    let surface_brush = unsafe { CreateSolidBrush(theme.surface.to_colorref()) };
    let background_brush = unsafe { CreateSolidBrush(theme.background.to_colorref()) };

    let state = Box::into_raw(Box::new(ConfigEditorState {
        handle: handle.clone(),
        theme: theme.clone(),
        hwnd,
        edit: HWND::default(),
        dirty: false,
        loading: true,
        status_text: "Ready".into(),
        surface_brush,
        background_brush,
    }));
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }

    // Create controls
    create_controls(hwnd, hinstance, scale, &theme);

    // Read file content into edit control
    let content = std::fs::read_to_string(&handle.config_path).unwrap_or_default();
    let wide: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let state_ref = &mut *(state);
        state_ref.loading = true;
        let _ = SetWindowTextW(state_ref.edit, PCWSTR::from_raw(wide.as_ptr()));
        state_ref.loading = false;
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

    // Nested message loop
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !ret.as_bool() {
            break;
        }

        // Intercept keyboard shortcuts before TranslateMessage
        if msg.message == WM_KEYDOWN {
            let ctrl = (unsafe { GetKeyState(0x11) } as u16 & 0x8000) != 0; // VK_CONTROL
            let vkey = msg.wParam.0 as u32;

            if ctrl && vkey == 'S' as u32 {
                // Ctrl+S → Save
                let state_ref = unsafe { &mut *(state) };
                let _ = do_save(state_ref);
                update_theme_and_repaint(state_ref, hwnd);
                continue;
            }
            if ctrl && vkey == 0x0D /* VK_RETURN */ {
                // Ctrl+Enter → Save & Reload
                let state_ref = unsafe { &mut *(state) };
                let _ = do_save_and_reload(state_ref);
                update_theme_and_repaint(state_ref, hwnd);
                continue;
            }
        }

        if msg.message == WM_KEYDOWN && msg.wParam.0 == 0x1B /* VK_ESCAPE */ {
            let state_ref = unsafe { &mut *(state) };
            if state_ref.dirty {
                let ans = unsafe {
                    MessageBoxW(
                        hwnd,
                        PCWSTR::from_raw(
                            "Discard unsaved changes?\0".encode_utf16()
                                .chain(std::iter::once(0))
                                .collect::<Vec<u16>>()
                                .as_ptr(),
                        ),
                        PCWSTR::from_raw(
                            "mhd Config\0".encode_utf16()
                                .chain(std::iter::once(0))
                                .collect::<Vec<u16>>()
                                .as_ptr(),
                        ),
                        MB_YESNO | MB_ICONWARNING,
                    )
                };
                if ans == IDYES {
                    unsafe { DestroyWindow(hwnd).ok(); }
                }
            } else {
                unsafe { DestroyWindow(hwnd).ok(); }
            }
            continue;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup state
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
        if !ptr.is_null() {
            let _ = Box::from_raw(ptr);
        }
    }
}

fn update_theme_and_repaint(_state: &ConfigEditorState, hwnd: HWND) {
    unsafe { let _ = InvalidateRect(hwnd, None, true); }
}

// ── Control creation ────────────────────────────────────────────────

fn create_controls(parent: HWND, hinstance: HINSTANCE, scale: f32, _theme: &NativeTheme) {
    let pad = (PADDING as f32 * scale) as i32;
    let btn_h = (BUTTON_HEIGHT_BASE as f32 * scale) as i32;
    let btn_w = (BUTTON_WIDTH_BASE as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let footer_h = (FOOTER_HEIGHT_BASE as f32 * scale) as i32;

    let mut rc = RECT::default();
    unsafe { let _ = GetClientRect(parent, &mut rc); }
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;

    // Style helpers: cast i32 constants to WINDOW_STYLE (u32)
    fn ws(v: i32) -> WINDOW_STYLE {
        WINDOW_STYLE(v as u32)
    }

    // EDIT control
    let edit_y = header_h;
    let edit_h = win_h - header_h - footer_h;
    let edit = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw("EDIT\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL
                | ws(ES_LEFT) | ws(ES_MULTILINE) | ws(ES_AUTOVSCROLL) | ws(ES_AUTOHSCROLL) | ws(ES_WANTRETURN),
            pad,
            edit_y,
            win_w - pad * 2,
            edit_h,
            parent,
            HMENU(IDC_EDIT as isize as *mut c_void),
            hinstance,
            None,
        )
    };
    if let Ok(hwnd_edit) = edit {
        // Set font to Consolas
        let font_name = to_utf16_z("Consolas");
        let font_h = -(12.0 * scale) as i32;
        let hfont = unsafe {
            CreateFontW(
                font_h, 0, 0, 0,
                FW_NORMAL.0 as i32,
                0, 0, 0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                DEFAULT_QUALITY.0 as u32,
                FF_DONTCARE.0 as u32,
                PCWSTR::from_raw(font_name.as_ptr()),
            )
        };
        unsafe {
            let _ = SendMessageW(hwnd_edit, WM_SETFONT, WPARAM(hfont.0 as usize), LPARAM(1));
        }

        // Store edit HWND in state
        unsafe {
            let state_ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut ConfigEditorState;
            if !state_ptr.is_null() {
                (*state_ptr).edit = hwnd_edit;
            }
        }
    }

    fn btn_style() -> WINDOW_STYLE {
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32)
    }

    // Buttons
    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;

    // Cancel
    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw("BUTTON\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            PCWSTR::from_raw("Cancel\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            btn_style(),
            win_w - pad - btn_w,
            btn_y,
            btn_w,
            btn_h,
            parent,
            HMENU(IDC_CANCEL as isize as *mut c_void),
            hinstance,
            None,
        );
    }
    // Save & Reload
    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw("BUTTON\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            PCWSTR::from_raw("Save && Reload\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            btn_style(),
            win_w - pad - btn_w,
            btn_y,
            btn_w,
            btn_h,
            parent,
            HMENU(IDC_SAVE_RELOAD as isize as *mut c_void),
            hinstance,
            None,
        );
    }
    // Save
    let save_x = win_w - pad * 2 - btn_w * 2;
    unsafe {
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw("BUTTON\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            PCWSTR::from_raw("Save\0".encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>().as_ptr()),
            btn_style(),
            save_x,
            btn_y,
            btn_w,
            btn_h,
            parent,
            HMENU(IDC_SAVE as isize as *mut c_void),
            hinstance,
            None,
        );
    }
}

// ── Window procedure ────────────────────────────────────────────────

unsafe extern "system" fn editor_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            return LRESULT(0);
        }
        WM_COMMAND => {
            let hi = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let id = (wparam.0 & 0xFFFF) as usize;

            // EN_CHANGE from edit control
            if hi == 0x0300 /* EN_CHANGE */ && id == IDC_EDIT {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if !state.loading {
                        state.dirty = true;
                        state.status_text = "Unsaved changes".into();
                        let _ = InvalidateRect(hwnd, None, true);
                    }
                }
                return LRESULT(0);
            }

            // Button clicks
            if hi == 0 /* BN_CLICKED */ {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
                if state_ptr.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let state = &mut *state_ptr;

                match id {
                    IDC_SAVE => {
                        let _ = do_save(state);
                        let _ = InvalidateRect(hwnd, None, true);
                    }
                    IDC_SAVE_RELOAD => {
                        let _ = do_save_and_reload(state);
                        let _ = InvalidateRect(hwnd, None, true);
                    }
                    IDC_CANCEL => {
                        if state.dirty {
                            let ans = MessageBoxW(
                                hwnd,
                                PCWSTR::from_raw(
                                    "Discard unsaved changes?\0".encode_utf16()
                                        .chain(std::iter::once(0))
                                        .collect::<Vec<u16>>()
                                        .as_ptr(),
                                ),
                                PCWSTR::from_raw(
                                    "mhd Config\0".encode_utf16()
                                        .chain(std::iter::once(0))
                                        .collect::<Vec<u16>>()
                                        .as_ptr(),
                                ),
                                MB_YESNO | MB_ICONWARNING,
                            );
                            if ans == IDYES {
                                DestroyWindow(hwnd).ok();
                            }
                        } else {
                            DestroyWindow(hwnd).ok();
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
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

            // Background
            let bg_brush = CreateSolidBrush(theme.background.to_colorref());
            let _ = FillRect(hdc, &rc, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Header background
            let header_rc = RECT { left: 0, top: 0, right: w, bottom: header_h };
            let hdr_brush = CreateSolidBrush(theme.background.to_colorref());
            let _ = FillRect(hdc, &header_rc, hdr_brush);
            let _ = DeleteObject(hdr_brush);

            // Title
            let _ = SetBkMode(hdc, TRANSPARENT);
            let title_font = create_font(-(18.0 * scale) as i32, true, "Segoe UI");
            let old_font = SelectObject(hdc, title_font);
            let _ = SetTextColor(hdc, theme.text.to_colorref());

            let mut title_wz = to_utf16_z("mhd Config");
            let mut title_rc = RECT {
                left: pad,
                top: pad,
                right: w - pad,
                bottom: pad + 18 + 8,
            };
            let _ = DrawTextW(hdc, &mut title_wz, &mut title_rc, DT_LEFT | DT_SINGLELINE);

            // Config path
            let path_font = create_font(-(10.0 * scale) as i32, false, "Segoe UI");
            let _ = SelectObject(hdc, path_font);
            let _ = SetTextColor(hdc, theme.text_muted.to_colorref());

            let path_str = if !state_ptr.is_null() {
                format!("{}", (*state_ptr).handle.config_path.display())
            } else {
                String::new()
            };
            let mut path_wz = to_utf16_z(&path_str);
            let mut path_rc = RECT {
                left: pad,
                top: title_rc.bottom + 2,
                right: w - pad,
                bottom: title_rc.bottom + 2 + 12 + 4,
            };
            let _ = DrawTextW(hdc, &mut path_wz, &mut path_rc, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);

            let _ = SelectObject(hdc, old_font);
            let _ = DeleteObject(title_font);
            let _ = DeleteObject(path_font);

            // Separator under header
            let sep_brush = CreateSolidBrush(theme.border.to_colorref());
            let sep_rc = RECT { left: pad, top: header_h - 1, right: w - pad, bottom: header_h };
            let _ = FillRect(hdc, &sep_rc, sep_brush);

            // Separator above footer
            let footer_y = h - footer_h;
            let sep2_rc = RECT { left: pad, top: footer_y, right: w - pad, bottom: footer_y + 1 };
            let _ = FillRect(hdc, &sep2_rc, sep_brush);
            let _ = DeleteObject(sep_brush);

            // Status text (footer)
            let status_font = create_font(-(11.0 * scale) as i32, false, "Segoe UI");
            let _ = SelectObject(hdc, status_font);
            let _ = SetTextColor(hdc, theme.text_muted.to_colorref());

            let status_text = if !state_ptr.is_null() {
                (*state_ptr).status_text.clone()
            } else {
                "Ready".into()
            };
            let mut st_wz = to_utf16_z(&status_text);
            let mut st_rc = RECT {
                left: pad,
                top: footer_y + (footer_h - 11) / 2,
                right: w - pad,
                bottom: h,
            };
            let _ = DrawTextW(hdc, &mut st_wz, &mut st_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

            let _ = SelectObject(hdc, old_font);
            let _ = DeleteObject(status_font);

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC => {
            // hdc is in wparam as a HDC (pointer)
            let hdc = HDC(wparam.0 as *mut c_void);
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let _ = SetTextColor(hdc, state.theme.text.to_colorref());
                let _ = SetBkColor(hdc, state.theme.surface.to_colorref());
                return LRESULT(state.surface_brush.0 as isize);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // Free state
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ConfigEditorState;
            if !ptr.is_null() {
                let state = Box::from_raw(ptr);
                let _ = DeleteObject(state.surface_brush);
                let _ = DeleteObject(state.background_brush);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Save logic ──────────────────────────────────────────────────────

fn get_edit_text(hwnd_edit: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd_edit) } as usize;
    if len == 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len + 1];
    unsafe {
        let _ = GetWindowTextW(hwnd_edit, &mut buf);
    }
    buf.truncate(len);
    String::from_utf16_lossy(&buf)
}

fn do_save(state: &mut ConfigEditorState) -> Result<(), String> {
    let text = get_edit_text(state.edit);

    // Validate TOML
    if let Err(e) = crate::config::AppConfig::parse(&text, &state.handle.config_path) {
        state.status_text = format!("TOML error: {e}");
        return Err(state.status_text.clone());
    }

    // Write file
    if let Err(e) = std::fs::write(&state.handle.config_path, &text) {
        state.status_text = format!("Write error: {e}");
        return Err(state.status_text.clone());
    }

    state.dirty = false;
    state.status_text = "Saved".into();
    Ok(())
}

fn do_save_and_reload(state: &mut ConfigEditorState) -> Result<(), String> {
    do_save(state)?;
    if let Err(e) = state.handle.reload_config() {
        state.status_text = format!("Reload error: {e}");
        return Err(state.status_text.clone());
    }
    // Apply updated theme
    state.theme = state.handle.theme();
    state.status_text = "Saved and reloaded".into();
    // Update theme brushes
    let new_surface = unsafe { CreateSolidBrush(state.theme.surface.to_colorref()) };
    let new_bg = unsafe { CreateSolidBrush(state.theme.background.to_colorref()) };
    unsafe {
        let _ = DeleteObject(state.surface_brush);
        let _ = DeleteObject(state.background_brush);
    }
    state.surface_brush = new_surface;
    state.background_brush = new_bg;
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
