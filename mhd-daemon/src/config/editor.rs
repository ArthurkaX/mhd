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
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::{AppHandle, DaemonControl};
use crate::hook::WM_BINDING_CAPTURED;
use crate::native_theme::{Argb, NativeTheme, load_theme_from_path};
use crate::osd::to_utf16_z;
use crate::trigger::{KeyCombo, Modifiers, PhysicalKey, keys_to_string};

// ── Layout constants (96 dpi base) ─────────────────────────────────

const WIN_WIDTH_BASE: i32 = 750;
const WIN_HEIGHT_BASE: i32 = 600;
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

// ── State ───────────────────────────────────────────────────────────

/// Actions exposed in the settings editor, by stable TOML action name.
///
/// Do not store positions from `ALL_ACTIONS` here: adding/reordering actions in
/// the registry must not shift editor choices (that caused Quick Note to become
/// Quit mhd in saved configs).
const EDITOR_ACTION_NAMES: &[&str] = &[
    "replace_key",
    "run_program",
    "run_ps",
    "brightness_up",
    "brightness_down",
    "show_monitor_panel",
    "show_volume_mixer",
    "media_volume_up",
    "media_volume_down",
    "media_mute",
    "media_play_pause",
    "media_stop",
    "media_last_track",
    "media_next_track",
    "toggle_topmost",
    "power_actions",
    "quick_draw",
    "quick_note",
    "pomodoro",
    "quit",
];

fn editor_action_desc(editor_idx: usize) -> &'static crate::action::ActionDescriptor {
    let name = EDITOR_ACTION_NAMES
        .get(editor_idx)
        .copied()
        .unwrap_or("quit");
    crate::action::ALL_ACTIONS
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| crate::action::ALL_ACTIONS.iter().find(|d| d.name == "quit").unwrap())
}

fn editor_index_for_action_name(name: &str) -> usize {
    EDITOR_ACTION_NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| EDITOR_ACTION_NAMES.iter().position(|n| *n == "quit").unwrap())
}

#[derive(Debug, Clone)]
struct UIBinding {
    trigger: String,
    /// Index into [`EDITOR_ACTION_NAMES`].
    kind_idx: usize,
    param: String,
    is_recording_trigger: bool,
    is_recording_param: bool,
}

/// Active section in the settings editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Bindings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverTarget {
    None,
    Tab(usize),
    ThemeCombo,
    AutostartToggle,
    ApplyBtn,
    CloseBtn,
    AddBtn,
    RowTrigger(usize),
    RowKind(usize),
    RowParam(usize),
    RowDelete(usize),
    Scrollbar,
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

    // Tab strip
    tab_y: i32,
    tab_h: i32,
    tab_w: i32,

    // Sections
    appearance_y: i32,
    shortcuts_y: i32,

    // Appearance controls
    label_w: i32,
    combo_x: i32,
    combo_w: i32,
    combo_y: i32,
    autostart_y: i32,
    arrow_x: i32,
    arrow_w: i32,

    // Table columns
    trig_w: i32,
    kind_w: i32,
    del_w: i32,

    // Bindings list
    list_y: i32,
    list_h: i32,
    row_h: i32,

    // Footer buttons
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
    active_section: SettingsSection,
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

    /// Autostart at user logon (via scheduled task)
    autostart: bool,

    /// List of bindings being edited
    bindings: Vec<UIBinding>,
    /// Vertical scroll offset per section
    #[allow(dead_code)] // reserved for General tab scrolling
    general_scroll_y: i32,
    bindings_scroll_y: i32,
    /// Currently recording (binding_idx, is_trigger)
    recording_info: Option<(usize, bool)>,
    /// Index of binding being edited inline
    edit_idx: Option<usize>,
    /// Buffer for the inline-edited text (no HWND child control — layered window compat)
    edit_text: String,
    /// Cursor position within edit_text
    edit_cursor: usize,
    /// Start of selection for inline edit (None = no selection)
    edit_select_start: Option<usize>,
    /// Previous value to restore on Escape
    edit_old_value: String,

    /// Hovered interactive target
    hovered_target: HoverTarget,
    /// Scroll dragging state
    is_dragging_scroll: bool,
    /// Scroll drag starting mouse Y position
    scroll_drag_start_y: i32,
    /// Scroll drag starting scroll offset
    scroll_drag_start_offset: i32,
    // (kind_popup replaced by HMENU cascading menu)
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
        active_section: SettingsSection::General,
        autostart: crate::autostart::is_autostart_enabled(),
        bindings,
        general_scroll_y: 0,
        bindings_scroll_y: 0,
        recording_info: None,
        edit_idx: None,
        edit_text: String::new(),
        edit_cursor: 0,
        edit_select_start: None,
        edit_old_value: String::new(),
        hovered_target: HoverTarget::None,
        is_dragging_scroll: false,
        scroll_drag_start_y: 0,
        scroll_drag_start_offset: 0,

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
    let btn_h = (BTN_HEIGHT_BASE as f32 * scale) as i32;
    let btn_w = (BTN_WIDTH_BASE as f32 * scale) as i32;
    let win_w = (WIN_WIDTH_BASE as f32 * scale) as i32;
    let win_h = (WIN_HEIGHT_BASE as f32 * scale) as i32;

    // Tab strip below header
    // Tabs on same line as title to save vertical space
    let tab_h = (24.0 * scale) as i32;
    let tab_y = pad / 2;
    let tab_w = (100.0 * scale) as i32;

    let appearance_y = header_h + pad / 2;
    let label_w = (LABEL_WIDTH_BASE as f32 * scale) as i32;
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
    let combo_x = pad + label_w + 8;
    let combo_w = (COMBO_POPUP_WIDTH as f32 * scale) as i32;
    let combo_y = appearance_y + (30.0 * scale) as i32;

    let shortcuts_y = appearance_y;
    let autostart_y = combo_y + combo_h + (8.0 * scale) as i32;
    let list_y = shortcuts_y + (48.0 * scale) as i32;
    let list_h = (win_h - footer_h) - list_y - pad / 2;

    let trig_w = (140.0 * scale) as i32;
    let kind_w = (120.0 * scale) as i32;
    let del_w = (28.0 * scale) as i32;

    let btn_y = win_h - footer_h + (footer_h - btn_h) / 2;

    let radius = (ROUND_RADIUS_BASE * scale) as i32;

    Layout {
        scale,
        win_w,
        win_h,
        pad,
        header_h,
        footer_h,
        tab_y,
        tab_h,
        tab_w,
        appearance_y,
        shortcuts_y,
        autostart_y,
        label_w,
        combo_x,
        combo_w,
        combo_y,
        arrow_x: combo_x + combo_w - combo_h,
        arrow_w: combo_h,
        trig_w,
        kind_w,
        del_w,
        btn_h,
        btn_w,
        btn_y,
        apply_x: win_w - pad - btn_w,
        close_x: win_w - pad - btn_w * 2 - (8.0 * scale) as i32,
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
            // Map Action variant → editor action by stable TOML name.
            let kind_idx = editor_index_for_action_name(b.action.name());

            let param = match &b.action {
                Action::ReplaceKey { keys } => keys_to_string(keys),
                Action::RunPs { command } => command.clone(),
                Action::SetBrightness { relative, value } => {
                    if *relative {
                        format!("{:+}", value)
                    } else {
                        format!("{}", value)
                    }
                }
                Action::BrightnessUp { value } | Action::BrightnessDown { value } => {
                    value.to_string()
                }
                _ => String::new(),
            };

            UIBinding {
                trigger: b.trigger_name.clone(),
                kind_idx,
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

    let mut frame = match crate::renderer::DibFrame::new(lay.win_w, lay.win_h) {
        Some(f) => f,
        None => return,
    };
    let dib_dc = frame.dc();
    let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;

    // ── Background rounded rect ────────────────────────────────────
    crate::osd::draw_rounded_rect(frame.pixels_mut(), lay.win_w, lay.win_h, lay.radius, theme.background);

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
    }

    // ── Tab strip (right‑aligned, same row as title) ──────────────
    let tab_names = ["General", "Bindings"];
    let tab_count = tab_names.len() as i32;
    let total_tab_w = lay.tab_w * tab_count + (8 * (tab_count - 1));
    let tab_start_x = lay.win_w - lay.pad - total_tab_w;
    for (ti, &name) in tab_names.iter().enumerate() {
        let tx = tab_start_x + (ti as i32) * (lay.tab_w + 8);
        let ty = lay.tab_y;
        let tab_rect = RECT { left: tx, top: ty, right: tx + lay.tab_w, bottom: ty + lay.tab_h };
        let is_active = (ti == 0 && state.active_section == SettingsSection::General)
            || (ti == 1 && state.active_section == SettingsSection::Bindings);

        let bg = if is_active {
            theme.accent
        } else {
            theme.surface.blend_over(theme.background)
        };
        let fg = if is_active {
            if contrast_text_on(theme.accent) { Argb::new(255, 0, 0, 0) } else { Argb::new(255, 255, 255, 255) }
        } else {
            theme.text_muted
        };
        draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, tab_rect, (4.0 * lay.scale) as i32, bg);
        unsafe { let _ = SetTextColor(dib_dc, fg.to_colorref()); }
        unsafe { let _ = SelectObject(dib_dc, body_font); }
        let mut label = to_utf16_z(name);
        let mut label_rc = RECT { left: tx, top: ty, right: tx + lay.tab_w, bottom: ty + lay.tab_h };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut label, &mut label_rc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // ── Appearance Section (General tab) ───────────────────────────
    if state.active_section == SettingsSection::General {
        unsafe {
            let _ = SelectObject(dib_dc, title_font);
            let _ = SetTextColor(dib_dc, theme.text.to_colorref());
        }
        let mut app_wz = to_utf16_z("Appearance");
        let mut app_rc = RECT {
            left: lay.pad,
            top: lay.appearance_y,
            right: lay.win_w - lay.pad,
            bottom: lay.appearance_y + (24.0 * lay.scale) as i32,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut app_wz, &mut app_rc, DT_LEFT | DT_SINGLELINE);
        }

    // Theme label
    unsafe {
        let _ = SelectObject(dib_dc, body_font);
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
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * lay.scale) as i32);
    let combo_rc = RECT {
        left: lay.combo_x,
        top: lay.combo_y,
        right: lay.combo_x + lay.combo_w,
        bottom: lay.combo_y + combo_h,
    };
    let is_combo_hovered = state.hovered_target == HoverTarget::ThemeCombo;
    let mut combo_color = theme.surface;
    if is_combo_hovered {
        combo_color = theme.hover.blend_over(combo_color);
    }
    let combo_radius = (4.0 * lay.scale) as i32;
    draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, combo_rc, combo_radius, combo_color);

    // Draw subtle border for combo box
    let combo_border_color = if is_combo_hovered {
        theme.text
    } else {
        theme.border
    };
    draw_rounded_border_in_buffer(bits, lay.win_w, lay.win_h, combo_rc, combo_radius, 1, combo_border_color);

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
        bottom: lay.combo_y + combo_h,
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
    let arrow_color = if is_combo_hovered {
        theme.text
    } else {
        theme.text_muted
    };
    unsafe {
        let _ = SetTextColor(dib_dc, arrow_color.to_colorref());
    }
    let mut arrow_wz = to_utf16_z("▼");
    let mut arrow_rc = RECT {
        left: lay.arrow_x,
        top: lay.combo_y,
        right: lay.arrow_x + lay.arrow_w,
        bottom: lay.combo_y + combo_h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut arrow_wz,
            &mut arrow_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Help text next to theme selector
    unsafe {
        let _ = SelectObject(dib_dc, small_font);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let mut theme_help_wz = to_utf16_z("Set the colour theme for mhd UI elements.");
    let mut theme_help_rc = RECT {
        left: lay.combo_x + lay.combo_w + lay.pad,
        top: lay.combo_y,
        right: lay.win_w - lay.pad,
        bottom: lay.combo_y + combo_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut theme_help_wz, &mut theme_help_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // ── Autostart toggle ───────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, body_font);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut auto_label_wz = to_utf16_z("Autostart");
    let mut auto_label_rc = RECT {
        left: lay.pad,
        top: lay.autostart_y,
        right: lay.pad + lay.label_w,
        bottom: lay.autostart_y + (20.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut auto_label_wz, &mut auto_label_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Toggle switch (checkbox-like)
    let toggle_h = (18.0 * lay.scale) as i32;
    let toggle_w = (36.0 * lay.scale) as i32;
    let toggle_y = lay.autostart_y + ((20.0 * lay.scale) as i32 - toggle_h) / 2;
    let toggle_rc = RECT {
        left: lay.combo_x,
        top: toggle_y,
        right: lay.combo_x + toggle_w,
        bottom: toggle_y + toggle_h,
    };
    let is_auto_hovered = state.hovered_target == HoverTarget::AutostartToggle;
    let auto_on = state.autostart;

    let toggle_bg = if auto_on {
        theme.accent
    } else {
        theme.surface.blend_over(theme.background)
    };
    let toggle_bg2 = if is_auto_hovered {
        theme.hover.blend_over(toggle_bg)
    } else {
        toggle_bg
    };
    let toggle_radius = toggle_h / 2;
    draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, toggle_rc, toggle_radius, toggle_bg2);

    // Knob
    let knob_margin = (2.0 * lay.scale) as i32;
    let knob_diam = toggle_h - knob_margin * 2;
    let knob_left = if auto_on {
        toggle_rc.right - knob_diam - knob_margin
    } else {
        toggle_rc.left + knob_margin
    };
    let knob_color = if auto_on { theme.text } else { theme.text_muted };
    draw_rounded_rect_in_buffer(
        bits, lay.win_w, lay.win_h,
        RECT { left: knob_left, top: toggle_rc.top + knob_margin, right: knob_left + knob_diam, bottom: toggle_rc.bottom - knob_margin },
        knob_diam / 2,
        knob_color,
    );

    // Autostart help text
    unsafe {
        let _ = SelectObject(dib_dc, small_font);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let auto_status = if auto_on { "Enabled (runs at logon with highest privileges)" } else { "Start mhd automatically when you log on" };
    let mut auto_help_wz = to_utf16_z(auto_status);
    let mut auto_help_rc = RECT {
        left: lay.combo_x + toggle_w + lay.pad,
        top: lay.autostart_y,
        right: lay.win_w - lay.pad,
        bottom: lay.autostart_y + (20.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut auto_help_wz, &mut auto_help_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }
    } // end if General

    // ── Shortcuts Section (Bindings tab) ────────────────────────────
    if state.active_section == SettingsSection::Bindings {
        unsafe {
            let _ = SelectObject(dib_dc, title_font);
            let _ = SetTextColor(dib_dc, theme.text.to_colorref());
        }
        let mut short_wz = to_utf16_z("Shortcuts");
        let mut short_rc = RECT {
            left: lay.pad,
        top: lay.shortcuts_y,
        right: lay.win_w - lay.pad,
        bottom: lay.shortcuts_y + (24.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut short_wz, &mut short_rc, DT_LEFT | DT_SINGLELINE);
    }

    // ── Table Headers ──────────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, small_font);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let table_header_y = lay.list_y - (24.0 * lay.scale) as i32;
    let kind_x = lay.pad + lay.trig_w + (8.0 * lay.scale) as i32;
    let param_x = kind_x + lay.kind_w + (8.0 * lay.scale) as i32;

    // Trigger header
    let mut trig_header_wz = to_utf16_z("TRIGGER");
    let mut trig_header_rc = RECT {
        left: lay.pad + 6,
        top: table_header_y,
        right: lay.pad + lay.trig_w,
        bottom: table_header_y + (16.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut trig_header_wz,
            &mut trig_header_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Action header
    let mut action_header_wz = to_utf16_z("ACTION");
    let mut action_header_rc = RECT {
        left: kind_x + 6,
        top: table_header_y,
        right: kind_x + lay.kind_w,
        bottom: table_header_y + (16.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut action_header_wz,
            &mut action_header_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Parameter header
    let mut param_header_wz = to_utf16_z("PARAMETER");
    let mut param_header_rc = RECT {
        left: param_x + 6,
        top: table_header_y,
        right: lay.win_w - lay.pad - lay.del_w,
        bottom: table_header_y + (16.0 * lay.scale) as i32,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut param_header_wz,
            &mut param_header_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

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

    let mut row_y = lay.list_y - state.bindings_scroll_y;
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
        let is_add_hovered = state.hovered_target == HoverTarget::AddBtn;
        draw_button(
            dib_dc,
            bits,
            lay.win_w,
            lay.win_h,
            lay.pad,
            row_y + (lay.row_h - lay.btn_h) / 2,
            (80.0 * lay.scale) as i32,
            lay.btn_h,
            "+ Add",
            theme,
            small_font,
            is_add_hovered,
            ButtonStyle::Secondary,
        );
    }
    } // end of if active_section == Bindings

    unsafe {
        let rgn = CreateRectRgn(0, 0, lay.win_w, lay.win_h);
        SelectClipRgn(dib_dc, rgn);
        let _ = DeleteObject(rgn);
    }

    // Separator above footer
    let footer_y = lay.win_h - lay.footer_h;
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: lay.pad,
                top: footer_y,
                right: lay.win_w - lay.pad,
                bottom: footer_y + 1,
            },
            sep_brush,
        );
        let _ = DeleteObject(sep_brush);
    }

    // ── Buttons ────────────────────────────────────────────────────
    // Apply
    let is_apply_hovered = state.hovered_target == HoverTarget::ApplyBtn;
    draw_button(
        dib_dc,
        bits,
        lay.win_w,
        lay.win_h,
        lay.apply_x,
        lay.btn_y,
        lay.btn_w,
        lay.btn_h,
        "Apply",
        theme,
        body_font,
        is_apply_hovered,
        ButtonStyle::Primary,
    );

    // Close
    let is_close_hovered = state.hovered_target == HoverTarget::CloseBtn;
    draw_button(
        dib_dc,
        bits,
        lay.win_w,
        lay.win_h,
        lay.close_x,
        lay.btn_y,
        lay.btn_w,
        lay.btn_h,
        "Close",
        theme,
        body_font,
        is_close_hovered,
        ButtonStyle::Secondary,
    );

    // ── Scrollbar (Bindings only) ────────────────────────────────────
    if state.active_section == SettingsSection::Bindings {
        let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
        if content_h > lay.list_h {
        let scroll_w = (6.0 * lay.scale) as i32;
        let scroll_x = lay.win_w - lay.pad + (lay.pad - scroll_w) / 2;

        // Draw track
        let track_rect = RECT {
            left: scroll_x,
            top: lay.list_y,
            right: scroll_x + scroll_w,
            bottom: lay.list_y + lay.list_h,
        };
        draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, track_rect, scroll_w / 2, theme.border);

        // Draw thumb
        let thumb_h = ((lay.list_h as f32 / content_h as f32) * lay.list_h as f32) as i32;
        let thumb_h = thumb_h.max((30.0 * lay.scale) as i32);
        let max_scroll = content_h - lay.list_h;
        let thumb_y = lay.list_y + ((state.bindings_scroll_y as f32 / max_scroll as f32) * (lay.list_h - thumb_h) as f32) as i32;

        let is_thumb_active = state.hovered_target == HoverTarget::Scrollbar || state.is_dragging_scroll;
        let thumb_color = if is_thumb_active {
            theme.accent
        } else {
            theme.text_muted
        };

        let thumb_rect = RECT {
            left: scroll_x,
            top: thumb_y,
            right: scroll_x + scroll_w,
            bottom: thumb_y + thumb_h,
        };
        draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, thumb_rect, scroll_w / 2, thumb_color);
    }
    } // end if Bindings (scrollbar)

    // ── Cleanup GDI objects ────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(body_font);
        let _ = DeleteObject(small_font);
    }

    // GDI writes RGB into a 32-bit DIB but often leaves alpha as 0.
    frame.fix_gdi_alpha(theme.background);

    // ── UpdateLayeredWindow (via DibFrame) ─────────────────────────
    // Pass the current window position — UpdateLayeredWindow with a
    // non‑None position moves the window, so we must not clobber it.
    let cur_pos = unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        (wr.left, wr.top)
    };
    frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);
    // DibFrame::drop handles DC, DIB, and screen DC cleanup.
}

/// Draw a rectangular button on the DIB.
fn blend_pixels_premultiplied(original: u32, overlay: Argb, opacity: f32) -> u32 {
    let overlay_a = (overlay.a as f32 * opacity) as u32;
    if overlay_a == 0 {
        return original;
    }

    let dest_a = (original >> 24) & 0xFF;
    let dest_r = (original >> 16) & 0xFF;
    let dest_g = (original >> 8) & 0xFF;
    let dest_b = original & 0xFF;

    let src_r = overlay.r as u32;
    let src_g = overlay.g as u32;
    let src_b = overlay.b as u32;

    let out_a = overlay_a + (dest_a * (255 - overlay_a) + 127) / 255;
    let out_r = (src_r * overlay_a + dest_r * (255 - overlay_a) + 127) / 255;
    let out_g = (src_g * overlay_a + dest_g * (255 - overlay_a) + 127) / 255;
    let out_b = (src_b * overlay_a + dest_b * (255 - overlay_a) + 127) / 255;

    (out_a.min(255) << 24)
        | (out_r.min(255) << 16)
        | (out_g.min(255) << 8)
        | out_b.min(255)
}

fn draw_rounded_rect_in_buffer(
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    r: i32,
    color: Argb,
) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };

    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let cr = r.min(w / 2).min(h / 2);

    let tl_cx = rect.left + cr;
    let tl_cy = rect.top + cr;
    let tr_cx = rect.right - cr - 1;
    let tr_cy = rect.top + cr;
    let bl_cx = rect.left + cr;
    let bl_cy = rect.bottom - cr - 1;
    let br_cx = rect.right - cr - 1;
    let br_cy = rect.bottom - cr - 1;

    for y in y1..y2 {
        for x in x1..x2 {
            let (is_corner, cx, cy) = if x < rect.left + cr && y < rect.top + cr {
                (true, tl_cx, tl_cy)
            } else if x > tr_cx && y < rect.top + cr {
                (true, tr_cx, tr_cy)
            } else if x < rect.left + cr && y > bl_cy {
                (true, bl_cx, bl_cy)
            } else if x > br_cx && y > br_cy {
                (true, br_cx, br_cy)
            } else {
                (false, 0, 0)
            };

            let idx = (y * win_w + x) as usize;
            if idx >= pixels.len() {
                continue;
            }

            if is_corner {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                let dist = (dx * dx + dy * dy).sqrt();
                let falloff = 1.0 - (dist - cr as f32).clamp(0.0, 1.0);
                if falloff <= 0.0 {
                    // outside corner
                } else {
                    let original = pixels[idx];
                    pixels[idx] = blend_pixels_premultiplied(original, color, falloff);
                }
            } else {
                let original = pixels[idx];
                pixels[idx] = blend_pixels_premultiplied(original, color, 1.0);
            }
        }
    }
}

fn draw_rounded_border_in_buffer(
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    r: i32,
    border_width: i32,
    color: Argb,
) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };

    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let cr = r.min(w / 2).min(h / 2);

    let tl_cx = rect.left + cr;
    let tl_cy = rect.top + cr;
    let tr_cx = rect.right - cr - 1;
    let tr_cy = rect.top + cr;
    let bl_cx = rect.left + cr;
    let bl_cy = rect.bottom - cr - 1;
    let br_cx = rect.right - cr - 1;
    let br_cy = rect.bottom - cr - 1;

    let bw = border_width as f32;

    for y in y1..y2 {
        for x in x1..x2 {
            let (is_corner, cx, cy) = if x < rect.left + cr && y < rect.top + cr {
                (true, tl_cx, tl_cy)
            } else if x > tr_cx && y < rect.top + cr {
                (true, tr_cx, tr_cy)
            } else if x < rect.left + cr && y > bl_cy {
                (true, bl_cx, bl_cy)
            } else if x > br_cx && y > br_cy {
                (true, br_cx, br_cy)
            } else {
                (false, 0, 0)
            };

            let idx = (y * win_w + x) as usize;
            if idx >= pixels.len() {
                continue;
            }

            let mut opacity = 0.0;

            if is_corner {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                let dist = (dx * dx + dy * dy).sqrt();
                let dist_from_outer = cr as f32 - dist;
                let dist_from_inner = dist - (cr as f32 - bw);
                let edge_opacity = dist_from_outer.clamp(0.0, 1.0).min(dist_from_inner.clamp(0.0, 1.0));
                opacity = edge_opacity;
            } else {
                let dl = (x - rect.left) as f32;
                let dr = (rect.right - 1 - x) as f32;
                let dt = (y - rect.top) as f32;
                let db = (rect.bottom - 1 - y) as f32;
                let min_dist = dl.min(dr).min(dt).min(db);
                if min_dist < bw {
                    opacity = 1.0 - (bw - 1.0 - min_dist).clamp(0.0, 1.0);
                }
            }

            if opacity > 0.0 {
                let original = pixels[idx];
                pixels[idx] = blend_pixels_premultiplied(original, color, opacity);
            }
        }
    }
}

/// Returns true if white text has sufficient contrast on this background.
fn contrast_text_on(bg: Argb) -> bool {
    let r = bg.r as f32 / 255.0;
    let g = bg.g as f32 / 255.0;
    let b = bg.b as f32 / 255.0;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonStyle {
    Primary,
    Secondary,
    DangerGhost,
    TriggerPlate,
}

/// Fill a rectangle with fully transparent pixels (alpha=0).
/// Used when an inline EDIT child window is active — the layered window
/// composites the child control only where the parent bitmap has zero alpha.
#[allow(dead_code)]
fn clear_rect_in_buffer(bits: *mut c_void, win_w: i32, win_h: i32, rect: RECT) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };
    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);
    for y in y1..y2 {
        let row_start = (y * win_w) as usize;
        for x in x1..x2 {
            let idx = row_start + x as usize;
            if idx < pixels.len() {
                pixels[idx] = 0; // ARGB: fully transparent
            }
        }
    }
}

/// Draw a rounded button or interactive plate on the DIB.
fn draw_button(
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
    theme: &NativeTheme,
    font: HFONT,
    is_hovered: bool,
    style: ButtonStyle,
) {
    let rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };

    let (btn_color, text_color, border_color) = match style {
        ButtonStyle::Primary => {
            let mut bg = theme.accent;
            if is_hovered {
                bg = theme.hover.blend_over(bg);
            }
            let fg = if contrast_text_on(bg) {
                Argb::new(255, 255, 255, 255)
            } else {
                Argb::new(255, 0, 0, 0)
            };
            (bg, fg, theme.border)
        }
        ButtonStyle::Secondary => {
            let mut bg = theme.surface;
            if is_hovered {
                bg = theme.hover.blend_over(bg);
            }
            let fg = bg.contrasting_text_color();
            (bg, fg, theme.border)
        }
        ButtonStyle::DangerGhost => {
            let mut bg = Argb::new(0, 0, 0, 0);
            let mut fg = theme.text_muted;
            if is_hovered {
                bg = Argb::new(40, 255, 0, 0);
                fg = Argb::new(255, 255, 80, 80);
            }
            (bg, fg, Argb::new(0, 0, 0, 0))
        }
        ButtonStyle::TriggerPlate => {
            let mut bg = theme.surface.blend_over(theme.background);
            let mut border = theme.border;
            if is_hovered {
                bg = theme.hover.blend_over(bg);
                border = theme.text;
            }
            let fg = bg.contrasting_text_color();
            (bg, fg, border)
        }
    };

    // Radius scaled based on DPI width
    let radius = (5.0 * (win_w as f32 / WIN_WIDTH_BASE as f32)) as i32;
    draw_rounded_rect_in_buffer(bits, win_w, win_h, rect, radius, btn_color);

    if border_color.a > 0 {
        draw_rounded_border_in_buffer(bits, win_w, win_h, rect, radius, 1, border_color);
    }

    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.to_colorref());
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

                // Header → drag, but exclude the tab strip (which lives on the
                // same row as the title) so tabs remain clickable.
                if pt.y < lay.header_h {
                    if pt.y >= lay.tab_y && pt.y < lay.tab_y + lay.tab_h {
                        let total_tab_w = lay.tab_w * 2 + 8;
                        let tab_start_x = lay.win_w - lay.pad - total_tab_w;
                        let tx = pt.x - tab_start_x;
                        if tx >= 0 {
                            let ti = tx / (lay.tab_w + 8);
                            if ti < 2 {
                                return LRESULT(HTCLIENT as isize);
                            }
                        }
                    }
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

                // Tab click
                let total_tab_w = lay.tab_w * 2 + 8;
                let tab_start_x = lay.win_w - lay.pad - total_tab_w;
                if y >= lay.tab_y && y < lay.tab_y + lay.tab_h {
                    let tx = x - tab_start_x;
                    if tx >= 0 {
                        let ti = tx / (lay.tab_w + 8);
                        if ti < 2 {
                            close_combo_popup(state);
                            close_kind_popup(state);
                            let new_section = if ti == 0 { SettingsSection::General } else { SettingsSection::Bindings };
                            if state.active_section != new_section {
                                state.active_section = new_section;
                                paint_settings(hwnd, state_ptr, &lay);
                            }
                            return LRESULT(0);
                        }
                    }
                }

                // Scrollbar click / drag start
                let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
                if content_h > lay.list_h {
                    let scroll_w = (6.0 * lay.scale) as i32;
                    let scroll_x = lay.win_w - lay.pad + (lay.pad - scroll_w) / 2;
                    let scroll_left = scroll_x - 4;
                    let scroll_right = scroll_x + scroll_w + 4;
                    if x >= scroll_left && x < scroll_right && y >= lay.list_y && y < lay.list_y + lay.list_h {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        let thumb_h = ((lay.list_h as f32 / content_h as f32) * lay.list_h as f32) as i32;
                        let thumb_h = thumb_h.max((30.0 * lay.scale) as i32);
                        let max_scroll = content_h - lay.list_h;
                        let thumb_y = lay.list_y + ((state.bindings_scroll_y as f32 / max_scroll as f32) * (lay.list_h - thumb_h) as f32) as i32;

                        if y >= thumb_y && y < thumb_y + thumb_h {
                            state.is_dragging_scroll = true;
                            state.scroll_drag_start_y = y;
                            state.scroll_drag_start_offset = state.bindings_scroll_y;
                            let _ = SetCapture(hwnd);
                        } else {
                            let track_click_y = y - lay.list_y - thumb_h / 2;
                            let pct = track_click_y as f32 / (lay.list_h - thumb_h) as f32;
                            state.bindings_scroll_y = (pct * max_scroll as f32) as i32;
                            state.bindings_scroll_y = state.bindings_scroll_y.clamp(0, max_scroll);
                            
                            state.is_dragging_scroll = true;
                            state.scroll_drag_start_y = y;
                            state.scroll_drag_start_offset = state.bindings_scroll_y;
                            let _ = SetCapture(hwnd);
                            paint_settings(hwnd, state_ptr, &lay);
                        }
                        return LRESULT(0);
                    }
                }

                // Theme combo click
                if y >= lay.combo_y
                    && y < lay.combo_y + combo_h
                    && x >= lay.combo_x
                    && x < lay.combo_x + lay.combo_w
                {
                    toggle_combo_popup(state);
                    return LRESULT(0);
                }

                // Autostart toggle click
                if y >= lay.autostart_y
                    && y < lay.autostart_y + (20.0 * lay.scale) as i32
                    && x >= lay.combo_x
                    && x < lay.combo_x + (36.0 * lay.scale) as i32
                {
                    state.autostart = !state.autostart;
                    if state.autostart {
                        if let Err(e) = crate::autostart::install_autostart() {
                            eprintln!("mhd: failed to enable autostart: {e}");
                            state.autostart = false;
                        }
                    } else {
                        if let Err(e) = crate::autostart::remove_autostart() {
                            eprintln!("mhd: failed to disable autostart: {e}");
                        }
                    }
                    paint_settings(hwnd, state_ptr, &lay);
                    return LRESULT(0);
                }

                // ── Bindings list interaction ────────────────────────
                if y >= lay.list_y && y < lay.list_y + lay.list_h {
                    // Close any active edit if clicking elsewhere
                    finish_inline_edit(state);

                    let mut row_y = lay.list_y - state.bindings_scroll_y;
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
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.bindings.push(UIBinding {
                            trigger: "none".to_string(),
                            kind_idx: 0, // ReplaceKey
                            param: "".to_string(),
                            is_recording_trigger: false,
                            is_recording_param: false,
                        });
                        paint_settings(hwnd, state_ptr, &lay);
                        return LRESULT(0);
                    }
                    return LRESULT(0);
                }

                // Apply button
                if x >= lay.apply_x
                    && x < lay.apply_x + lay.btn_w
                    && y >= lay.btn_y
                    && y < lay.btn_y + lay.btn_h
                {
                    close_combo_popup(state);
                    close_kind_popup(state);
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

                close_combo_popup(state);
                close_kind_popup(state);
                LRESULT(0)
            }

            WM_LBUTTONUP => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.is_dragging_scroll {
                        state.is_dragging_scroll = false;
                        let _ = ReleaseCapture();
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                }
                LRESULT(0)
            }

            WM_MOUSEMOVE => {
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    let lay = state.layout;

                    if state.is_dragging_scroll {
                        let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
                        let thumb_h = ((lay.list_h as f32 / content_h as f32) * lay.list_h as f32) as i32;
                        let thumb_h = thumb_h.max((30.0 * lay.scale) as i32);
                        let max_scroll = content_h - lay.list_h;
                        let thumb_travel = lay.list_h - thumb_h;
                        if thumb_travel > 0 {
                            let dy = y - state.scroll_drag_start_y;
                            let scroll_delta = (dy as f32 / thumb_travel as f32 * max_scroll as f32) as i32;
                            state.bindings_scroll_y = (state.scroll_drag_start_offset + scroll_delta).clamp(0, max_scroll);
                            paint_settings(hwnd, state_ptr, &lay);
                        }
                    } else {
                        let mut target = HoverTarget::None;

                        let combo_h = (COMBO_HIT_HEIGHT as f32 * lay.scale) as i32;

                        // Tab strip
                        let total_tab_w = lay.tab_w * 2 + 8;
                        let tab_start_x = lay.win_w - lay.pad - total_tab_w;
                        if y >= lay.tab_y && y < lay.tab_y + lay.tab_h {
                            let tx = x - tab_start_x;
                            if tx >= 0 {
                                let ti = tx / (lay.tab_w + 8);
                                if ti < 2 {
                                    target = HoverTarget::Tab(ti as usize);
                                }
                            }
                        }
                        // Theme combo
                        else if y >= lay.combo_y
                            && y < lay.combo_y + combo_h
                            && x >= lay.combo_x
                            && x < lay.combo_x + lay.combo_w
                        {
                            target = HoverTarget::ThemeCombo;
                        }
                        // Autostart toggle
                        else if y >= lay.autostart_y
                            && y < lay.autostart_y + (20.0 * lay.scale) as i32
                            && x >= lay.combo_x
                            && x < lay.combo_x + (36.0 * lay.scale) as i32
                        {
                            target = HoverTarget::AutostartToggle;
                        }
                        // Apply button
                        else if x >= lay.apply_x
                            && x < lay.apply_x + lay.btn_w
                            && y >= lay.btn_y
                            && y < lay.btn_y + lay.btn_h
                        {
                            target = HoverTarget::ApplyBtn;
                        }
                        // Close button
                        else if x >= lay.close_x
                            && x < lay.close_x + lay.btn_w
                            && y >= lay.btn_y
                            && y < lay.btn_y + lay.btn_h
                        {
                            target = HoverTarget::CloseBtn;
                        }
                        // Bindings list area
                        else if y >= lay.list_y && y < lay.list_y + lay.list_h {
                            let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
                            let mut hit_scroll_track = false;

                            if content_h > lay.list_h {
                                let scroll_w = (6.0 * lay.scale) as i32;
                                let scroll_x = lay.win_w - lay.pad + (lay.pad - scroll_w) / 2;
                                let scroll_left = scroll_x - 4;
                                let scroll_right = scroll_x + scroll_w + 4;
                                if x >= scroll_left && x < scroll_right {
                                    target = HoverTarget::Scrollbar;
                                    hit_scroll_track = true;
                                }
                            }

                            if !hit_scroll_track {
                                let mut row_y = lay.list_y - state.bindings_scroll_y;
                                let mut found = false;

                                for i in 0..state.bindings.len() {
                                    if y >= row_y && y < row_y + lay.row_h {
                                        let kind_x = lay.pad + lay.trig_w + (8.0 * lay.scale) as i32;
                                        let desc = crate::action::ALL_ACTIONS.iter().find(|a| a.name == crate::config::editor::EDITOR_ACTION_NAMES.get(state.bindings[i].kind_idx).copied().unwrap_or("quit")).unwrap();

                                        if x >= lay.pad && x < lay.pad + lay.trig_w {
                                            target = HoverTarget::RowTrigger(i);
                                        } else if x >= kind_x && x < kind_x + lay.kind_w {
                                            target = HoverTarget::RowKind(i);
                                        } else if desc.param_key.is_some() {
                                            let param_x = kind_x + lay.kind_w + (8.0 * lay.scale) as i32;
                                            let param_w = lay.win_w - lay.pad - lay.del_w - (8.0 * lay.scale) as i32 - param_x;
                                            if x >= param_x && x < param_x + param_w {
                                                target = HoverTarget::RowParam(i);
                                            }
                                        }
                                        if target == HoverTarget::None {
                                            if x >= lay.win_w - lay.pad - lay.del_w && x < lay.win_w - lay.pad {
                                                target = HoverTarget::RowDelete(i);
                                            }
                                        }
                                        found = true;
                                        break;
                                    }
                                    row_y += lay.row_h;
                                }

                                // Add button
                                if !found && y >= row_y && y < row_y + lay.row_h {
                                    let add_w = (80.0 * lay.scale) as i32;
                                    if x >= lay.pad && x < lay.pad + add_w {
                                        target = HoverTarget::AddBtn;
                                    }
                                }
                            }
                        }

                        if state.hovered_target != target {
                            state.hovered_target = target;
                            paint_settings(hwnd, state_ptr, &lay);

                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            let _ = TrackMouseEvent(&mut tme);
                        }
                    }
                }
                LRESULT(0)
            }

            WM_MOUSELEAVE => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.hovered_target != HoverTarget::None {
                        state.hovered_target = HoverTarget::None;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
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
                    if state.active_section == SettingsSection::Bindings {
                        let lay = state.layout;
                        let content_h = (state.bindings.len() as i32 + 1) * lay.row_h;
                        let max_scroll = (content_h - lay.list_h).max(0);
                        state.bindings_scroll_y =
                            (state.bindings_scroll_y - (delta as i32 / 120) * 40).clamp(0, max_scroll);
                        paint_settings(hwnd, state_ptr, &lay);
                    }
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
                } else if key_type == 1 {
                    PhysicalKey::MouseButton(key_val as u8)
                } else {
                    // key_type 2 = wheel / tilt
                    match key_val {
                        0 => PhysicalKey::WheelUp,
                        1 => PhysicalKey::WheelDown,
                        2 => PhysicalKey::WheelLeft,
                        3 => PhysicalKey::WheelRight,
                        _ => return LRESULT(0),
                    }
                };

                let trigger_str = keys_to_string(&KeyCombo {
                    modifiers: mods,
                    key: Some(key),
                });

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if let Some((idx, is_trigger)) = state.recording_info.take() {
                        // Safety: idx must be valid after deletions/shifts.
                        if idx < state.bindings.len() {
                            if is_trigger {
                                state.bindings[idx].trigger = trigger_str;
                                state.bindings[idx].is_recording_trigger = false;
                            } else {
                                state.bindings[idx].param = trigger_str;
                                state.bindings[idx].is_recording_param = false;
                            }
                        }
                        crate::hook::set_recording_window(None);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                }
                LRESULT(0)
            }

            WM_KEYDOWN => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.edit_idx.is_some() {
                        let vk = wparam.0 as u32;
                        let ctrl_down = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
                        let shift_down = GetAsyncKeyState(VK_SHIFT.0 as i32) < 0;
                        let is_selected = state.edit_select_start.is_some()
                            && state.edit_select_start.unwrap() != state.edit_cursor;
                        let (sel_start, sel_end) = if let Some(sel) = state.edit_select_start {
                            (sel.min(state.edit_cursor), sel.max(state.edit_cursor))
                        } else {
                            (state.edit_cursor, state.edit_cursor)
                        };

                        match vk {
                            0x0D /* VK_RETURN */ => {
                                finish_inline_edit(state);
                            }
                            0x1B /* VK_ESCAPE */ => {
                                cancel_inline_edit(state);
                            }
                            // Ctrl+A — Select All
                            0x41 if ctrl_down => {
                                state.edit_select_start = Some(0);
                                state.edit_cursor = state.edit_text.len();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            // Ctrl+C — Copy
                            0x43 if ctrl_down => {
                                if is_selected {
                                    let text = &state.edit_text[sel_start..sel_end];
                                    if OpenClipboard(hwnd).is_ok() {
                                        let _ = EmptyClipboard();
                                        let wide: Vec<u16> = text.encode_utf16().collect();
                                        let size = (wide.len() + 1) * 2;
                                        if let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, size) {
                                            let ptr = GlobalLock(handle) as *mut u16;
                                            if !ptr.is_null() {
                                                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                                                ptr.add(wide.len()).write(0);
                                                let _ = GlobalUnlock(handle);
                                                let _ = SetClipboardData(13u32, windows::Win32::Foundation::HANDLE(handle.0));
                                            }
                                        }
                                        let _ = CloseClipboard();
                                    }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            // Ctrl+V — Paste
                            0x56 if ctrl_down => {
                                // Replace selection with pasted text
                                if is_selected {
                                    let (s, e) = (sel_start, sel_end);
                                    state.edit_text.drain(s..e);
                                    state.edit_cursor = s;
                                    state.edit_select_start = None;
                                }
                                if let Ok(_h) = OpenClipboard(hwnd) {
                                    if let Ok(handle) = GetClipboardData(13u32) {
                                        let ptr = GlobalLock(windows::Win32::Foundation::HGLOBAL(handle.0)) as *const u16;
                                        if !ptr.is_null() {
                                            let len = (0..).find(|&i| *ptr.add(i) == 0).unwrap_or(0);
                                            if len > 0 {
                                                let paste_str = String::from_utf16_lossy(
                                                    std::slice::from_raw_parts(ptr, len)
                                                );
                                                // Only paste printable characters
                                                let filtered: String = paste_str.chars()
                                                    .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
                                                    .collect();
                                                state.edit_text.insert_str(state.edit_cursor, &filtered);
                                                state.edit_cursor += filtered.len();
                                            }
                                            let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(handle.0));
                                        }
                                    }
                                    let _ = CloseClipboard();
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            // Ctrl+X — Cut
                            0x58 if ctrl_down => {
                                if is_selected {
                                    let text = &state.edit_text[sel_start..sel_end];
                                    if OpenClipboard(hwnd).is_ok() {
                                        let _ = EmptyClipboard();
                                        let wide: Vec<u16> = text.encode_utf16().collect();
                                        let size = (wide.len() + 1) * 2;
                                        if let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, size) {
                                            let ptr = GlobalLock(handle) as *mut u16;
                                            if !ptr.is_null() {
                                                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                                                ptr.add(wide.len()).write(0);
                                                let _ = GlobalUnlock(handle);
                                                let _ = SetClipboardData(13u32, windows::Win32::Foundation::HANDLE(handle.0));
                                            }
                                        }
                                        let _ = CloseClipboard();
                                    }
                                    // Then delete selection
                                    state.edit_text.drain(sel_start..sel_end);
                                    state.edit_cursor = sel_start;
                                    state.edit_select_start = None;
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x08 /* VK_BACK */ => {
                                if is_selected {
                                    state.edit_text.drain(sel_start..sel_end);
                                    state.edit_cursor = sel_start;
                                    state.edit_select_start = None;
                                } else if state.edit_cursor > 0 {
                                    state.edit_text.remove(state.edit_cursor - 1);
                                    state.edit_cursor = state.edit_cursor.saturating_sub(1);
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x2E /* VK_DELETE */ => {
                                if is_selected {
                                    state.edit_text.drain(sel_start..sel_end);
                                    state.edit_cursor = sel_start;
                                    state.edit_select_start = None;
                                } else if state.edit_cursor < state.edit_text.len() {
                                    state.edit_text.remove(state.edit_cursor);
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x25 /* VK_LEFT */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() {
                                        state.edit_select_start = Some(state.edit_cursor);
                                    }
                                    if state.edit_cursor > 0 {
                                        state.edit_cursor -= 1;
                                    }
                                } else {
                                    state.edit_select_start = None;
                                    if state.edit_cursor > 0 {
                                        state.edit_cursor -= 1;
                                    }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x27 /* VK_RIGHT */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() {
                                        state.edit_select_start = Some(state.edit_cursor);
                                    }
                                    if state.edit_cursor < state.edit_text.len() {
                                        state.edit_cursor += 1;
                                    }
                                } else {
                                    state.edit_select_start = None;
                                    if state.edit_cursor < state.edit_text.len() {
                                        state.edit_cursor += 1;
                                    }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x24 /* VK_HOME */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() {
                                        state.edit_select_start = Some(state.edit_cursor);
                                    }
                                    state.edit_cursor = 0;
                                } else {
                                    state.edit_select_start = None;
                                    state.edit_cursor = 0;
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x23 /* VK_END */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() {
                                        state.edit_select_start = Some(state.edit_cursor);
                                    }
                                    state.edit_cursor = state.edit_text.len();
                                } else {
                                    state.edit_select_start = None;
                                    state.edit_cursor = state.edit_text.len();
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            _ => {}
                        }
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            WM_CHAR => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    if state.edit_idx.is_some() {
                        let ch = (wparam.0 as u32) as u8 as char;
                        // Only insert printable characters
                        if ch.is_ascii_graphic() || ch == ' ' {
                            // Delete selection if any
                            if let Some(sel) = state.edit_select_start {
                                if sel != state.edit_cursor {
                                    let (s, e) = (sel.min(state.edit_cursor), sel.max(state.edit_cursor));
                                    state.edit_text.drain(s..e);
                                    state.edit_cursor = s;
                                    state.edit_select_start = None;
                                }
                            }
                            state.edit_text.insert(state.edit_cursor, ch);
                            state.edit_cursor += 1;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            WM_DESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !ptr.is_null() {
                    close_combo_popup(&mut *ptr);
                    close_kind_popup(&mut *ptr);
                    let _ = Box::from_raw(ptr);
                }
                crate::hook::set_recording_window(None);
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
    close_kind_popup(state);
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

fn open_kind_menu(state: &mut SettingsState, idx: usize) {
    const ID_ACTION_BASE: usize = 1000;

    unsafe {
        // Build a main popup menu with submenus grouped by category.
        let main_menu = CreatePopupMenu();
        let Ok(main_menu) = main_menu else { return };
        if main_menu == HMENU::default() {
            return;
        }

        // Collect unique categories in display order.
        let mut categories: Vec<&str> = Vec::new();
        for editor_idx in 0..EDITOR_ACTION_NAMES.len() {
            let cat = editor_action_desc(editor_idx).category;
            if !categories.contains(&cat) {
                categories.push(cat);
            }
        }

        for &cat in &categories {
            let sub = CreatePopupMenu();
            let Ok(sub) = sub else { continue };
            if sub == HMENU::default() {
                continue;
            }
            for editor_idx in 0..EDITOR_ACTION_NAMES.len() {
                let desc = editor_action_desc(editor_idx);
                if desc.category == cat {
                    let cmd = ID_ACTION_BASE + editor_idx;
                    let label = to_utf16_z(desc.label);
                    let _ = AppendMenuW(sub, MF_STRING, cmd, PCWSTR::from_raw(label.as_ptr()));
                }
            }

            let cat_label = to_utf16_z(cat);
            let _ = AppendMenuW(
                main_menu,
                MF_POPUP | MF_STRING,
                sub.0 as usize,
                PCWSTR::from_raw(cat_label.as_ptr()),
            );
        }

        // Position the menu at the kind button
        let lay = state.layout;
        let kind_x = lay.pad + lay.trig_w + (8.0 * lay.scale) as i32;
        let btn_y_in_row = state.layout.list_y - state.bindings_scroll_y + (idx as i32) * lay.row_h
            + (lay.row_h - lay.btn_h) / 2;
        let mut pt = POINT {
            x: kind_x,
            y: btn_y_in_row,
        };
        let _ = ClientToScreen(state.hwnd, &mut pt);

        // TrackPopupMenu blocks until dismissed. With TPM_RETURNCMD the
        // return value (i32) is the command ID of the selected item, or 0.
        let chosen = TrackPopupMenu(
            main_menu,
            TPM_RETURNCMD | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            0,
            state.hwnd,
            None,
        );
        let chosen = chosen.0 as usize;

        // Clean up menus
        let _ = DestroyMenu(main_menu);

        if chosen >= ID_ACTION_BASE {
            let selected = chosen - ID_ACTION_BASE;
            if selected < EDITOR_ACTION_NAMES.len() {
                if state.bindings[idx].kind_idx != selected {
                    state.bindings[idx].kind_idx = selected;
                    // Set default param 5 for parameterised actions
                    let desc = editor_action_desc(selected);
                    state.bindings[idx].param = if desc.param_key.is_some() {
                        "5".to_string()
                    } else {
                        String::new()
                    };
                }
                paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
            }
        }
    }
}

fn close_kind_popup(state: &mut SettingsState) {
    // No-op: native HMENU is self-dismissing.
    // Kept for compatibility with existing cleanup calls.
    let _ = state;
}

fn draw_binding_row(
    hdc: HDC,
    bits: *mut c_void,
    idx: usize,
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

    // 1. Trigger button (Plate style)
    let trig_rc = RECT {
        left: row_rc.left,
        top: y + (lay.row_h - lay.btn_h) / 2,
        right: row_rc.left + lay.trig_w,
        bottom: y + (lay.row_h + lay.btn_h) / 2,
    };
    let trig_text = if binding.is_recording_trigger {
        "..."
    } else {
        &binding.trigger
    };
    let is_trig_hovered = state.hovered_target == HoverTarget::RowTrigger(idx);
    draw_button(
        hdc,
        bits,
        lay.win_w,
        lay.win_h,
        trig_rc.left,
        trig_rc.top,
        lay.trig_w,
        lay.btn_h,
        trig_text,
        theme,
        small_font,
        is_trig_hovered,
        ButtonStyle::TriggerPlate,
    );

    // 2. Action kind button
    let kind_x = trig_rc.right + (8.0 * lay.scale) as i32;
    let is_kind_hovered = state.hovered_target == HoverTarget::RowKind(idx);
    let kind_rect = RECT {
        left: kind_x,
        top: trig_rc.top,
        right: kind_x + lay.kind_w,
        bottom: trig_rc.bottom,
    };

    let mut kind_btn_color = theme.surface;
    if is_kind_hovered {
        kind_btn_color = theme.hover.blend_over(kind_btn_color);
    }
    let radius = (4.0 * lay.scale) as i32;
    draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, kind_rect, radius, kind_btn_color);
    draw_rounded_border_in_buffer(bits, lay.win_w, lay.win_h, kind_rect, radius, 1, if is_kind_hovered { theme.text } else { theme.border });

    unsafe {
        let _ = SelectObject(hdc, small_font);
        // Use contrasting text colour on the kind button background
        let kind_text_color = kind_btn_color.contrasting_text_color();
        let _ = SetTextColor(hdc, kind_text_color.to_colorref());
    }

    // Left-aligned label text
    let desc = editor_action_desc(binding.kind_idx);
    let mut kind_wz = to_utf16_z(desc.label);
    let mut kind_text_rc = RECT {
        left: kind_x + 8,
        top: trig_rc.top,
        right: kind_x + lay.kind_w - 18,
        bottom: trig_rc.bottom,
    };
    unsafe {
        let _ = DrawTextW(
            hdc,
            &mut kind_wz,
            &mut kind_text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    // Arrow ▼
    let mut kind_arrow_wz = to_utf16_z("▼");
    let mut kind_arrow_rc = RECT {
        left: kind_x + lay.kind_w - 16,
        top: trig_rc.top,
        right: kind_x + lay.kind_w,
        bottom: trig_rc.bottom,
    };
    unsafe {
        let _ = DrawTextW(
            hdc,
            &mut kind_arrow_wz,
            &mut kind_arrow_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // 3. Param area — only drawn for actions that have a parameter
    let has_param = desc.param_key.is_some();
    if has_param {
        let param_x = kind_rect.right + (8.0 * lay.scale) as i32;
        let param_w = row_rc.right - param_x - lay.del_w - (8.0 * lay.scale) as i32;
        let param_rc = RECT {
            left: param_x,
            top: trig_rc.top,
            right: param_x + param_w,
            bottom: trig_rc.bottom,
        };

        let param_bg = theme.surface.blend_over(theme.background);
        let is_param_hovered = state.hovered_target == HoverTarget::RowParam(idx);
        let is_recording_param = binding.is_recording_param;
        let is_editing_param = state.edit_idx == Some(idx);
        let param_text_color = param_bg.contrasting_text_color();

        if is_editing_param {
            // Draw the editor background
            let param_radius = (4.0 * lay.scale) as i32;
            draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, param_rc, param_radius, param_bg);

            // Draw the edit text inline (no child control — layered window compat)
            unsafe {
                let _ = SelectObject(hdc, small_font);
            }

            let text_x = param_rc.left + 8;
            let text_y = param_rc.top;
            let text_h = param_rc.bottom - param_rc.top;
            let accent = theme.accent;

            // Determine selection range
            let sel = state.edit_select_start;
            let (sel_start, sel_end) = match sel {
                Some(s) if s != state.edit_cursor => (s.min(state.edit_cursor), s.max(state.edit_cursor)),
                _ => (state.edit_cursor, state.edit_cursor),
            };
            let has_selection = sel.is_some() && sel.unwrap() != state.edit_cursor;

            if has_selection {
                let full_text = &state.edit_text;
                let before = &full_text[..sel_start];
                let selected = &full_text[sel_start..sel_end];
                let after = &full_text[sel_end..];

                let wz_before = to_utf16_z(before);
                let wz_selected = to_utf16_z(selected);

                let mut before_size = SIZE::default();
                let mut selected_size = SIZE::default();
                unsafe {
                    let _ = GetTextExtentPoint32W(hdc, &wz_before, &mut before_size);
                    let _ = GetTextExtentPoint32W(hdc, &wz_selected, &mut selected_size);
                }

                let sel_rect_left = text_x + before_size.cx;
                let sel_rect_right = sel_rect_left + selected_size.cx;

                // Draw selection background
                draw_rounded_rect_in_buffer(
                    bits, lay.win_w, lay.win_h,
                    RECT { left: sel_rect_left, top: param_rc.top + 2, right: sel_rect_right, bottom: param_rc.bottom - 2 },
                    0, accent,
                );

                // Draw before (contrasting text)
                unsafe {
                    let _ = SetTextColor(hdc, param_text_color.to_colorref());
                    let mut rc = RECT { left: text_x, top: text_y, right: sel_rect_left, bottom: text_y + text_h };
                    let _ = DrawTextW(hdc, &mut (before.to_string() + "\0").encode_utf16().collect::<Vec<_>>(), &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
                }
                // Draw selected (white on accent bg)
                unsafe {
                    let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
                    let mut rc = RECT { left: sel_rect_left, top: text_y, right: sel_rect_right, bottom: text_y + text_h };
                    let _ = DrawTextW(hdc, &mut (selected.to_string() + "\0").encode_utf16().collect::<Vec<_>>(), &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
                }
                // Draw after (contrasting text)
                unsafe {
                    let _ = SetTextColor(hdc, param_text_color.to_colorref());
                    let mut rc = RECT { left: sel_rect_right, top: text_y, right: param_rc.right - 8, bottom: text_y + text_h };
                    let _ = DrawTextW(hdc, &mut (after.to_string() + "\0").encode_utf16().collect::<Vec<_>>(), &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
                }

                // Draw cursor at the "active" end of selection
                let cursor_x = if state.edit_cursor == sel_end { sel_rect_right } else { sel_rect_left };
                let buf_size = (lay.win_w * lay.win_h) as usize;
                let cursor_color = param_text_color.to_premultiplied_argb_pixel();
                for dy in (text_h / 4)..(text_h * 3 / 4) {
                    let px = cursor_x + text_y * lay.win_w + dy * lay.win_w;
                    if px >= 0 && (px as usize) < buf_size {
                        unsafe { *bits.add(px as usize).cast::<u32>() = cursor_color; }
                    }
                }
            } else {
                // No selection — draw text with cursor
                unsafe { let _ = SetTextColor(hdc, param_text_color.to_colorref()); }
                let display = if state.edit_cursor <= state.edit_text.len() {
                    let (before, after) = state.edit_text.split_at(state.edit_cursor);
                    format!("{}|{}", before, after)
                } else {
                    state.edit_text.clone()
                };
                let mut wz = to_utf16_z(&display);
                let mut text_rc = RECT { left: text_x, ..param_rc };
                unsafe {
                    let _ = DrawTextW(hdc, &mut wz, &mut text_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                }
            }
        } else {
            let mut current_bg = param_bg;
            if is_param_hovered {
                current_bg = theme.hover.blend_over(current_bg);
            }

            let param_radius = (4.0 * lay.scale) as i32;
            draw_rounded_rect_in_buffer(bits, lay.win_w, lay.win_h, param_rc, param_radius, current_bg);

            unsafe {
                let _ = SetTextColor(hdc, param_text_color.to_colorref());
                let mut wz = to_utf16_z(&binding.param);
                let mut text_rc = RECT {
                    left: param_rc.left + 8,
                    ..param_rc
                };
                let _ = DrawTextW(
                    hdc,
                    &mut wz,
                    &mut text_rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
        }

        let border_color = if is_recording_param || is_editing_param {
            theme.accent
        } else if is_param_hovered {
            theme.text
        } else {
            theme.border
        };
        let param_radius = (4.0 * lay.scale) as i32;
        draw_rounded_border_in_buffer(bits, lay.win_w, lay.win_h, param_rc, param_radius, 1, border_color);
    }

    // 4. Delete button (DangerGhost style)
    let del_rc = RECT {
        left: row_rc.right - lay.del_w,
        top: y + (lay.row_h - lay.del_w) / 2,
        right: row_rc.right,
        bottom: y + (lay.row_h + lay.del_w) / 2,
    };
    let is_del_hovered = state.hovered_target == HoverTarget::RowDelete(idx);
    draw_button(
        hdc,
        bits,
        lay.win_w,
        lay.win_h,
        del_rc.left,
        del_rc.top,
        lay.del_w,
        lay.del_w,
        "X",
        theme,
        small_font,
        is_del_hovered,
        ButtonStyle::DangerGhost,
    );
}

fn handle_list_click(state: &mut SettingsState, idx: usize, x: i32, y: i32, row_y: i32) {
    let lay = state.layout;

    // 1. Trigger button
    if x >= lay.pad
        && x < lay.pad + lay.trig_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        close_kind_popup(state);
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
            crate::hook::set_recording_window(Some(state.hwnd));
        } else {
            state.recording_info = None;
            crate::hook::set_recording_window(None);
        }

        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }

    // 2. Kind button → open cascading HMENU
    let kind_x = lay.pad + lay.trig_w + (8.0 * lay.scale) as i32;
    if x >= kind_x
        && x < kind_x + lay.kind_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        close_combo_popup(state);
        open_kind_menu(state, idx);
        return;
    }

    // 2b. Param
    let param_x = kind_x + lay.kind_w + (8.0 * lay.scale) as i32;
    let param_w = lay.win_w - lay.pad - lay.del_w - (8.0 * lay.scale) as i32 - param_x;
    if x >= param_x
        && x < param_x + param_w
        && y >= row_y + (lay.row_h - lay.btn_h) / 2
        && y < row_y + (lay.row_h + lay.btn_h) / 2
    {
        close_kind_popup(state);
        let desc = editor_action_desc(state.bindings[idx].kind_idx);
        let is_replace_key = desc.name == "replace_key";
        let has_params = desc.param_key.is_some();

        if is_replace_key {
            let is_recording = !state.bindings[idx].is_recording_param;
            for b in state.bindings.iter_mut() {
                b.is_recording_trigger = false;
                b.is_recording_param = false;
            }
            if is_recording {
                state.bindings[idx].is_recording_param = true;
                state.recording_info = Some((idx, false));
                crate::hook::set_recording_window(Some(state.hwnd));
            } else {
                state.recording_info = None;
                crate::hook::set_recording_window(None);
            }
        } else if has_params {
            if desc.name == "run_program" {
                // Open file dialog to pick .exe/.lnk/.bat
                if let Some(path) = pick_program_file(state.hwnd) {
                    state.bindings[idx].param = path;
                }
            } else {
                // Inline editing for other parameterised actions (run_ps, brightness, etc.)
                let rc = RECT {
                    left: param_x,
                    top: row_y + (lay.row_h - lay.btn_h) / 2,
                    right: param_x + param_w,
                    bottom: row_y + (lay.row_h + lay.btn_h) / 2,
                };
                spawn_inline_edit(state, idx, rc);
            }
        }
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }

    // 3. Delete button
    if x >= lay.win_w - lay.pad - lay.del_w
        && x < lay.win_w - lay.pad
        && y >= row_y + (lay.row_h - lay.del_w) / 2
        && y < row_y + (lay.row_h + lay.del_w) / 2
    {
        close_kind_popup(state);
        state.bindings.remove(idx);
        // Adjust recording_info if it referenced the removed binding or shifted.
        if let Some((ri_idx, is_trig)) = state.recording_info {
            if ri_idx == idx {
                // The recorded binding is gone.
                state.recording_info = None;
                crate::hook::set_recording_window(None);
            } else if ri_idx > idx {
                // Shift index down because elements before it were removed.
                state.recording_info = Some((ri_idx - 1, is_trig));
            }
        }
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
        return;
    }
}

fn spawn_inline_edit(state: &mut SettingsState, idx: usize, _rc: RECT) {
    // No child HWND — we render the text directly in paint_settings
    // to avoid layered‑window compositing issues (UpdateLayeredWindow +
    // child EDIT controls are invisible).  Keyboard input is handled
    // via WM_CHAR / WM_KEYDOWN in the main window procedure.
    state.edit_idx = Some(idx);
    state.edit_text = state.bindings[idx].param.clone();
    state.edit_cursor = state.bindings[idx].param.len();
    state.edit_select_start = None;
    state.edit_old_value = state.bindings[idx].param.clone();
}

fn finish_inline_edit(state: &mut SettingsState) {
    if let Some(idx) = state.edit_idx.take() {
        state.bindings[idx].param = std::mem::take(&mut state.edit_text);
        state.edit_cursor = 0;
        state.edit_select_start = None;
        state.edit_old_value.clear();
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

fn cancel_inline_edit(state: &mut SettingsState) {
    if let Some(idx) = state.edit_idx.take() {
        state.bindings[idx].param = std::mem::take(&mut state.edit_old_value);
        state.edit_text.clear();
        state.edit_cursor = 0;
        state.edit_select_start = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

// ── File dialog for Run Program ─────────────────────────────────────

fn pick_program_file(parent: HWND) -> Option<String> {
    use std::mem;
    unsafe {
        let mut ofn: windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW = mem::zeroed();
        let mut buf = [0u16; 1024];
        let filter: Vec<u16> = "Programs\0*.exe;*.lnk;*.bat\0All Files\0*.*\0\0"
            .encode_utf16()
            .collect();

        ofn.lStructSize = mem::size_of::<windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW>() as u32;
        ofn.hwndOwner = parent;
        ofn.lpstrFilter = windows::core::PCWSTR(filter.as_ptr());
        ofn.lpstrFile = windows::core::PWSTR(buf.as_mut_ptr());
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrTitle = windows::core::w!("Select Program");
        ofn.Flags = windows::Win32::UI::Controls::Dialogs::OFN_FILEMUSTEXIST
            | windows::Win32::UI::Controls::Dialogs::OFN_HIDEREADONLY
            | windows::Win32::UI::Controls::Dialogs::OFN_PATHMUSTEXIST;

        if windows::Win32::UI::Controls::Dialogs::GetOpenFileNameW(&mut ofn).as_bool() {
            let len = (0..buf.len()).find(|&i| buf[i] == 0).unwrap_or(0);
            if len > 0 {
                return Some(String::from_utf16_lossy(&buf[..len]));
            }
        }
    }
    None
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
        state.autostart,
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
    autostart: bool,
    handle: &AppHandle,
) -> Result<(), String> {
    // Validate no duplicate triggers within same scheme
    {
        let mut seen = std::collections::HashSet::new();
        for b in bindings {
            let lower = b.trigger.trim().to_lowercase();
            if !seen.insert(lower.clone()) {
                return Err(format!("Duplicate trigger '{}' — each trigger must be unique within the active scheme", b.trigger));
            }
        }
    }

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

        // Update autostart
        if autostart {
            table.insert("autostart".to_string(), toml::Value::Boolean(true));
        } else {
            table.remove("autostart");
        }

        // Update bindings
        let mut new_bindings = Vec::new();
        for b in bindings {
            let mut map = toml::value::Table::new();
            map.insert(
                "trigger".to_string(),
                toml::Value::String(b.trigger.clone()),
            );

            let desc = editor_action_desc(b.kind_idx);

            map.insert(
                "action".to_string(),
                toml::Value::String(desc.name.to_string()),
            );
            if let Some(param_key) = desc.param_key {
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
    crate::renderer::create_font(h, bold, family)
}

fn monitor_work_rect() -> RECT {
    crate::renderer::primary_monitor_work_rect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    #[test]
    fn quick_note_maps_to_quick_note_not_quit() {
        let idx = editor_index_for_action_name(Action::QuickNote.name());
        assert_eq!(editor_action_desc(idx).name, "quick_note");
        assert_eq!(editor_action_desc(idx).label, "Quick Note");
    }

    #[test]
    fn all_editor_actions_resolve_by_name() {
        for i in 0..EDITOR_ACTION_NAMES.len() {
            assert_eq!(editor_action_desc(i).name, EDITOR_ACTION_NAMES[i]);
        }
    }

    #[test]
    fn unknown_editor_action_falls_back_to_quit() {
        let idx = editor_index_for_action_name("does_not_exist");
        assert_eq!(editor_action_desc(idx).name, "quit");
    }
}
