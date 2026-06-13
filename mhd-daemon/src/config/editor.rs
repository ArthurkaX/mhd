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
//!
//! This module is the **routing & dispatch** layer. Sub‑modules hold
//! layout constants, state types, hit‑testing, and theme primitives.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::FOS_PICKFOLDERS;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR};

// RichEdit constants used by the param edit popup.
use windows::Win32::UI::Controls::RichEdit::{
    CFE_EFFECTS, CFM_COLOR, CHARFORMATW, EM_SETBKGNDCOLOR, EM_SETCHARFORMAT, SCF_ALL, SCF_DEFAULT,
};

use crate::app::{AppHandle, DaemonControl};
use crate::core::action::ActionParamSchema;
use crate::core::native_theme::{Argb, NativeTheme, load_theme_from_path};
use crate::core::trigger::{KeyCombo, Modifiers, PhysicalKey, keys_to_string};
use crate::hook::WM_BINDING_CAPTURED;

// Import editor layout constants for local use
use crate::config::editor_layout::{
    ADVANCED_BUTTONS, EDITOR_ACTION_NAMES, ID_ACTION_BASE, editor_action_desc,
    editor_index_for_action_name,
};
// Re‑exports for backward compatibility (used by other modules)
pub use crate::config::editor_hittest::hit_test_settings;
pub use crate::config::editor_layout::{
    COMBO_HIT_HEIGHT, COMBO_POPUP_ITEM_HEIGHT, COMBO_POPUP_MAX_VISIBLE, FONT_BODY_SIZE,
    FONT_SMALL_SIZE, FONT_TITLE_SIZE, Layout, WIN_HEIGHT_BASE, WIN_WIDTH_BASE, WM_MOUSELEAVE,
    WM_PARAM_EDIT_COMMIT, compute_layout,
};
pub use crate::config::editor_paint::{
    build_advanced_controls, build_general_controls, build_llm_proxy_controls,
    build_shortcuts_controls, paint_page,
};
pub use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
use crate::config::editor_state::{
    ButtonStyle, ParamEditCreateInfo, SettingsHit, SettingsPage, SettingsState, UIBinding,
    UiProvider,
};
pub use crate::config::editor_theme::draw_rounded_border_in_buffer;
pub use crate::config::editor_theme::{draw_button, draw_rounded_rect_in_buffer, to_utf16_z};

const TAB_NAMES: &[&str] = &["General", "Shortcuts", "LLM Proxy", "Advanced"];

// ═══════════════════════════════════════════════════════════════════════
// Folder browser (IFileOpenDialog, Vista+)
// ═══════════════════════════════════════════════════════════════════════

fn browse_for_folder(hwnd: HWND) -> Option<std::path::PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        let _ = dialog.SetOptions(opts | FOS_PICKFOLDERS);
        let title: Vec<u16> = "Select save folder\0".encode_utf16().collect();
        let _ = dialog.SetTitle(PCWSTR::from_raw(title.as_ptr()));
        dialog.Show(hwnd).ok()?;
        let item = dialog.GetResult().ok()?;
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = pwstr.to_string().ok()?;
        Some(std::path::PathBuf::from(s))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Public API
// ═══════════════════════════════════════════════════════════════════════

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

    // Param edit popup class — regular popup with a RichEdit child.
    let edit_popup_cls = to_utf16_z("mhd_param_edit_popup_cls");
    let edit_popup_wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(param_edit_popup_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(edit_popup_cls.as_ptr()),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&edit_popup_wc);
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
            WS_EX_LAYERED | WS_EX_APPWINDOW,
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

    // Read both save paths up front, each lock released before the next, so the
    // struct literal below doesn't hold two MutexGuards on handle.config at once
    // (temporaries live to the end of the statement → that would deadlock).
    let notes_dir = handle
        .config
        .lock()
        .unwrap()
        .quicknote_config()
        .notes_dir
        .clone();
    let draw_dir = handle.config.lock().unwrap().draw_dir().clone();

    let state = Box::into_raw(Box::new(SettingsState {
        handle: handle.clone(),
        theme: theme.clone(),
        hwnd,
        layout,
        theme_names: theme_names.clone(),
        theme_sel,
        hover_sel: None,
        combo_popup: None,
        combo_open,
        theme_search_items: build_theme_search_items(&theme_names),
        theme_dropdown: SearchDropdownState::default(),
        active_section: SettingsPage::General,
        autostart: crate::autostart::is_autostart_enabled(),
        notes_dir,
        draw_dir,
        bindings,
        providers: {
            // Load providers from the proxy JSON files.
            // Also load secrets for the API key (first provider gets the upstream key).
            let upstream_key = llm_proxy::config::load_secrets()
                .map(|s| s.upstream_key)
                .unwrap_or_default();

            // Load models and group them by provider name.
            let models_by_provider: std::collections::HashMap<String, Vec<String>> = {
                let mut map: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                if let Ok(models) = llm_proxy::config::load_models() {
                    for m in models {
                        map.entry(m.provider).or_default().push(m.id);
                    }
                }
                map
            };

            match llm_proxy::config::load_providers() {
                Ok(list) => list
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let api_key = if i == 0 {
                            upstream_key.clone()
                        } else {
                            String::new()
                        };
                        let models = models_by_provider.get(&p.name).cloned().unwrap_or_default();
                        UiProvider {
                            name: p.name,
                            endpoint: p.endpoint,
                            api_key,
                            models,
                        }
                    })
                    .collect(),
                Err(_) => vec![UiProvider {
                    name: "Default".into(),
                    endpoint: "http://89.22.226.188:8080/v1".into(),
                    api_key: upstream_key,
                    models: Vec::new(),
                }],
            }
        },
        content_scroll_y: 0,
        recording_info: None,
        expanded_idx: None,
        acc_trigger: String::new(),
        acc_kind_idx: 0,
        acc_param: String::new(),
        acc_is_recording: false,
        acc_is_recording_param: false,
        acc_save_error: None,
        edit_idx: None,
        edit_text: String::new(),
        edit_cursor: 0,
        edit_select_start: None,
        edit_old_value: String::new(),
        hovered_target: SettingsHit::None,
        is_dragging_scroll: false,
        scroll_drag_start_y: 0,
        scroll_drag_start_offset: 0,
        param_edit_popup: None,
        param_edit_idx: None,
        tab_titles: TAB_NAMES.to_vec(),
        hit_regions: Vec::new(),
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
        let _ = ShowWindow(hwnd, SW_SHOW);
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

    // State is freed inside WM_DESTROY (via Box::from_raw).
    // Do NOT free it again here — that would be a double-free.
}

// ═══════════════════════════════════════════════════════════════════════
// Theme list & bindings loading
// ═══════════════════════════════════════════════════════════════════════

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
            if let Some(_stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(t) = load_theme_from_path(&path)
                && !names.contains(&t.name)
            {
                names.push(t.name.clone());
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

fn build_theme_search_items(names: &[String]) -> Vec<SearchDropdownItem> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| SearchDropdownItem::new(i, name.clone(), vec![name.to_lowercase()]))
        .collect()
}

fn load_ui_bindings(handle: &AppHandle) -> Vec<UIBinding> {
    use crate::action::Action;

    let config = handle.config.lock().unwrap();
    config
        .active_bindings()
        .iter()
        .map(|b| {
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

// ═══════════════════════════════════════════════════════════════════════
// Top‑level paint dispatch
// ═══════════════════════════════════════════════════════════════════════

fn paint_settings(hwnd: HWND, state_ptr: *mut SettingsState, layout: &Layout) {
    let state = unsafe { &*state_ptr };
    let theme = &state.theme;
    let lay = layout;

    let mut frame = match crate::renderer::DibFrame::new(lay.win_w(), lay.win_h()) {
        Some(f) => f,
        None => return,
    };
    let dib_dc = frame.dc();
    let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;

    // ── Background rounded rect ────────────────────────────────────
    crate::osd::draw_rounded_rect(
        frame.pixels_mut(),
        lay.win_w(),
        lay.win_h(),
        lay.radius(),
        theme.background,
    );

    // ── GDI painting helpers ───────────────────────────────────────
    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
    }

    let title_font = create_font(
        -(FONT_TITLE_SIZE as f32 * lay.scale()) as i32,
        true,
        "Segoe UI Variable",
    );
    let body_font = create_font(
        -(FONT_BODY_SIZE as f32 * lay.scale()) as i32,
        false,
        "Segoe UI Variable",
    );
    let small_font = create_font(
        -(FONT_SMALL_SIZE as f32 * lay.scale()) as i32,
        false,
        "Segoe UI Variable",
    );

    // ── Header: title ──────────────────────────────────────────────
    let old_font = unsafe { SelectObject(dib_dc, title_font) };
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut title_wz = to_utf16_z("mhd Settings");
    let mut title_rc = RECT {
        left: lay.pad(),
        top: lay.pad() / 2,
        right: lay.win_w() - lay.pad(),
        bottom: lay.pad() / 2 + 18 + 6,
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
                left: lay.pad(),
                top: lay.header_h() - 1,
                right: lay.win_w() - lay.pad(),
                bottom: lay.header_h(),
            },
            sep_brush,
        );
    }

    // ── Tab bar inside header (right-aligned, same row as title) ──
    let tab_names: &[&str] = TAB_NAMES;
    let n = tab_names.len() as i32;
    let tab_total_w = n * lay.tab_w() + (n - 1) * lay.tab_gap();
    let tab_start_x = lay.win_w() - lay.pad() - tab_total_w;

    for (ti, &name) in tab_names.iter().enumerate() {
        let tx = tab_start_x + ti as i32 * (lay.tab_w() + lay.tab_gap());
        let is_active = (ti == 0 && state.active_section == SettingsPage::General)
            || (ti == 1 && state.active_section == SettingsPage::Shortcuts)
            || (ti == 2 && state.active_section == SettingsPage::LlmProxy)
            || (ti == 3 && state.active_section == SettingsPage::Advanced);
        let is_hovered = match state.hovered_target {
            SettingsHit::Tab(i) => i == ti,
            _ => false,
        };

        let tab_rect = RECT {
            left: tx,
            top: lay.tab_bar_y(),
            right: tx + lay.tab_w(),
            bottom: lay.tab_bar_y() + lay.tab_h(),
        };
        let bg = if is_active {
            theme.accent
        } else if is_hovered {
            theme.hover.blend_over(theme.background)
        } else {
            theme.surface.blend_over(theme.background)
        };
        draw_rounded_rect_in_buffer(
            bits,
            lay.win_w(),
            lay.win_h(),
            tab_rect,
            (4.0 * lay.scale()) as i32,
            bg,
        );

        let fg = if is_active {
            if contrast_text_on(theme.accent) {
                Argb::new(255, 0, 0, 0)
            } else {
                Argb::new(255, 255, 255, 255)
            }
        } else if is_hovered {
            theme.text
        } else {
            theme.text_muted
        };
        unsafe {
            let _ = SetTextColor(dib_dc, fg.to_colorref());
            let _ = SetBkColor(dib_dc, Argb::new(0, 0, 0, 0).to_colorref());
            let _ = SelectObject(dib_dc, small_font);
        }
        let mut label = to_utf16_z(name);
        let mut label_rc = RECT {
            left: tx,
            top: lay.tab_bar_y(),
            right: tx + lay.tab_w(),
            bottom: lay.tab_bar_y() + lay.tab_h(),
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut label,
                &mut label_rc,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // ── Scrollable content area with clip region ──────────────────
    let page_regions: Vec<crate::config::editor_control::HitRegion>;
    unsafe {
        let content_clip = CreateRectRgn(
            lay.pad(),
            lay.content_y(),
            lay.win_w() - lay.pad(),
            lay.content_y() + lay.content_visible_h(),
        );
        SelectClipRgn(dib_dc, content_clip);
        let _ = DeleteObject(content_clip);

        let mut old_org = POINT { x: 0, y: 0 };
        let _ = SetViewportOrgEx(dib_dc, 0, -state.content_scroll_y, Some(&mut old_org));

        match state.active_section {
            SettingsPage::General => {
                let ctls = build_general_controls(lay, state);
                page_regions = paint_page(
                    &ctls,
                    dib_dc,
                    bits,
                    lay.win_w(),
                    lay.win_h(),
                    state.content_scroll_y,
                    theme,
                    title_font,
                    body_font,
                    small_font,
                );
            }
            SettingsPage::Shortcuts => {
                let ctls = build_shortcuts_controls(lay, state);
                page_regions = paint_page(
                    &ctls,
                    dib_dc,
                    bits,
                    lay.win_w(),
                    lay.win_h(),
                    state.content_scroll_y,
                    theme,
                    title_font,
                    body_font,
                    small_font,
                );
            }
            SettingsPage::Advanced => {
                let ctls = build_advanced_controls(lay, state);
                page_regions = paint_page(
                    &ctls,
                    dib_dc,
                    bits,
                    lay.win_w(),
                    lay.win_h(),
                    state.content_scroll_y,
                    theme,
                    title_font,
                    body_font,
                    small_font,
                );
            }
            SettingsPage::LlmProxy => {
                let ctls = build_llm_proxy_controls(lay, state);
                page_regions = paint_page(
                    &ctls,
                    dib_dc,
                    bits,
                    lay.win_w(),
                    lay.win_h(),
                    state.content_scroll_y,
                    theme,
                    title_font,
                    body_font,
                    small_font,
                );
            }
        }

        // Restore viewport origin
        let _ = SetViewportOrgEx(dib_dc, old_org.x, old_org.y, None);
    }

    // ── Content scrollbar ──────────────────────────────────────────
    let content_total_h = page_control_content_height(state, lay);
    if content_total_h > lay.content_visible_h() {
        let max_scroll = content_total_h - lay.content_visible_h();
        let scrollbar_x = lay.win_w() - lay.pad() - lay.scrollbar_w();
        let scrollbar_h = lay.content_visible_h();
        let thumb_h = ((scrollbar_h as f32 / content_total_h as f32) * scrollbar_h as f32) as i32;
        let thumb_h = thumb_h.max((16.0 * lay.scale()) as i32);
        let thumb_travel = scrollbar_h - thumb_h;
        let thumb_y = lay.content_y()
            + ((state.content_scroll_y as f32 / max_scroll as f32) * thumb_travel as f32) as i32;

        // Track background
        let track_rect = RECT {
            left: scrollbar_x,
            top: lay.content_y(),
            right: scrollbar_x + lay.scrollbar_w(),
            bottom: lay.content_y() + scrollbar_h,
        };
        draw_rounded_rect_in_buffer(bits, lay.win_w(), lay.win_h(), track_rect, 0, theme.surface);

        // Thumb
        let thumb_rect = RECT {
            left: scrollbar_x,
            top: thumb_y,
            right: scrollbar_x + lay.scrollbar_w(),
            bottom: thumb_y + thumb_h,
        };
        draw_rounded_rect_in_buffer(
            bits,
            lay.win_w(),
            lay.win_h(),
            thumb_rect,
            (2.0 * lay.scale()) as i32,
            theme.hover.blend_over(theme.surface),
        );
    }

    unsafe {
        let rgn = CreateRectRgn(0, 0, lay.win_w(), lay.win_h());
        SelectClipRgn(dib_dc, rgn);
        let _ = DeleteObject(rgn);
    }

    // ── Theme search dropdown overlay (on top of page content) ───────
    if state.active_section == SettingsPage::General && state.theme_dropdown.is_open {
        draw_theme_dropdown(dib_dc, bits, lay, state, body_font, small_font);
    }

    // Separator above footer
    let footer_y = lay.win_h() - lay.footer_h();
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: lay.pad(),
                top: footer_y,
                right: lay.win_w() - lay.pad(),
                bottom: footer_y + 1,
            },
            sep_brush,
        );
        let _ = DeleteObject(sep_brush);
    }

    // ── Buttons (right to left: Close, Apply, Save) ────────────────
    let is_close_hovered = state.hovered_target == SettingsHit::CloseBtn;
    draw_button(
        dib_dc,
        bits,
        lay.win_w(),
        lay.win_h(),
        lay.close_x(),
        lay.btn_y(),
        lay.btn_w(),
        lay.btn_h(),
        "Close",
        theme,
        body_font,
        is_close_hovered,
        ButtonStyle::Secondary,
    );

    let is_apply_hovered = state.hovered_target == SettingsHit::ApplyBtn;
    draw_button(
        dib_dc,
        bits,
        lay.win_w(),
        lay.win_h(),
        lay.apply_x(),
        lay.btn_y(),
        lay.btn_w(),
        lay.btn_h(),
        "Apply",
        theme,
        body_font,
        is_apply_hovered,
        ButtonStyle::Secondary,
    );

    let is_save_hovered = state.hovered_target == SettingsHit::SaveBtn;
    draw_button(
        dib_dc,
        bits,
        lay.win_w(),
        lay.win_h(),
        lay.save_x(),
        lay.btn_y(),
        lay.btn_w(),
        lay.btn_h(),
        "Save",
        theme,
        body_font,
        is_save_hovered,
        ButtonStyle::Primary,
    );

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
    let cur_pos = unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        (wr.left, wr.top)
    };
    frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);

    // Store hit regions for the active page so hit_test_settings can find
    // them via linear region search instead of geometry-duplicating code.
    unsafe {
        (*state_ptr).hit_regions = page_regions;
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Contrast helper (inline to keep paint dispatch self‑contained)
// ═══════════════════════════════════════════════════════════════════════

fn contrast_text_on(bg: Argb) -> bool {
    let r = bg.r as f32 / 255.0;
    let g = bg.g as f32 / 255.0;
    let b = bg.b as f32 / 255.0;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

// ═══════════════════════════════════════════════════════════════════════

/// Paint the LLM Proxy stub page — "coming soon" centered text.
/// Return the estimated total content height for the active page.
/// Used to determine whether a scrollbar is needed and how far to scroll.
fn page_control_content_height(state: &SettingsState, lay: &Layout) -> i32 {
    match state.active_section {
        SettingsPage::General => {
            // Last control is Draw Path browse button row.
            let last_y = lay.general.draw_path_y + (36.0 * lay.scale()) as i32;
            last_y + (20.0 * lay.scale()) as i32 - lay.content_y()
        }
        SettingsPage::Shortcuts => {
            // Dynamic: binding rows + accordion + add button.
            let n = state.bindings.len() as i32;
            let accordion_h = if state.expanded_idx.is_some() {
                lay.accordion_h()
            } else {
                0
            };
            let last_y = lay.list_y() + n * lay.row_h() + accordion_h + lay.row_h();
            last_y + (16.0 * lay.scale()) as i32 - lay.content_y()
        }
        SettingsPage::Advanced => {
            let btn_count = ADVANCED_BUTTONS.len() as i32;
            let last_y =
                lay.advanced.top_y + btn_count * (lay.advanced.btn_h + lay.advanced.btn_gap);
            last_y + (16.0 * lay.scale()) as i32 - lay.content_y()
        }
        SettingsPage::LlmProxy => {
            let n = state.providers.len() as i32;
            let last_y = lay.provider_list_y() + n * lay.provider_row_h() + lay.provider_row_h();
            last_y + (16.0 * lay.scale()) as i32 - lay.content_y()
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Inline editing helpers
// ═══════════════════════════════════════════════════════════════════════

#[allow(dead_code)] // kept for editor panel param editing
fn spawn_inline_edit(state: &mut SettingsState, idx: usize, rc: RECT) {
    if let Some(old) = state.param_edit_popup.take() {
        unsafe {
            let _ = DestroyWindow(old);
        }
    }

    let mut pt = POINT {
        x: rc.left,
        y: rc.top,
    };
    unsafe {
        let _ = ClientToScreen(state.hwnd, &mut pt);
    }

    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;

    let initial_text = state.bindings[idx].param.clone();
    let mut text_buf = [0u16; 1024];
    let len = initial_text.encode_utf16().count().min(1023);
    for (i, c) in initial_text.encode_utf16().take(len).enumerate() {
        text_buf[i] = c;
    }
    text_buf[len] = 0;

    let param_bg = state.theme.surface.blend_over(state.theme.background);
    let text_color = param_bg.contrasting_text_color();

    let info = ParamEditCreateInfo {
        state_ptr: state as *mut SettingsState,
        idx,
        width: w,
        height: h,
        initial_text: text_buf,
        text_color,
        brush_color: param_bg,
    };

    let cls_name = to_utf16_z("mhd_param_edit_popup_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let popup = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_BORDER,
            pt.x,
            pt.y,
            w,
            h,
            state.hwnd,
            HMENU::default(),
            hinstance,
            Some(&info as *const _ as *const c_void),
        )
    };

    match popup {
        Ok(h) => {
            state.param_edit_popup = Some(h);
            state.param_edit_idx = Some(idx);
            unsafe {
                let _ = ShowWindow(h, SW_SHOW);
            }
        }
        Err(_) => {
            state.edit_idx = Some(idx);
            state.edit_text = state.bindings[idx].param.clone();
            state.edit_cursor = state.bindings[idx].param.len();
            state.edit_select_start = None;
            state.edit_old_value = state.bindings[idx].param.clone();
        }
    }
}

fn finish_inline_edit(state: &mut SettingsState) {
    if let Some(idx) = state.edit_idx.take() {
        if state.expanded_idx == Some(idx) {
            state.acc_param = std::mem::take(&mut state.edit_text);
        } else {
            state.bindings[idx].param = std::mem::take(&mut state.edit_text);
        }
        state.edit_cursor = 0;
        state.edit_select_start = None;
        state.edit_old_value.clear();
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

fn cancel_inline_edit(state: &mut SettingsState) {
    if let Some(idx) = state.edit_idx.take() {
        if state.expanded_idx == Some(idx) {
            state.acc_param = std::mem::take(&mut state.edit_old_value);
        } else {
            state.bindings[idx].param = std::mem::take(&mut state.edit_old_value);
        }
        state.edit_text.clear();
        state.edit_cursor = 0;
        state.edit_select_start = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Apply & Save
// ═══════════════════════════════════════════════════════════════════════

fn apply_settings(state: &mut SettingsState) {
    let theme_name = state
        .theme_names
        .get(state.theme_sel)
        .cloned()
        .unwrap_or_else(|| "built-in dark".to_string());

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
                    if let Ok(t) = load_theme_from_path(&path)
                        && t.name == theme_name
                    {
                        found = stem.to_string();
                        break;
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

    if let Err(e) = save_config(
        &state.handle.config_path,
        &config_name,
        &state.bindings,
        state.autostart,
        &state.notes_dir,
        &state.draw_dir,
        &state.handle,
    ) {
        eprintln!("mhd: settings error: {e}");
        return;
    }

    // Persist providers and models to the proxy config
    {
        let providers: Vec<llm_proxy::config::Provider> = state
            .providers
            .iter()
            .map(|p| llm_proxy::config::Provider {
                name: p.name.clone(),
                endpoint: p.endpoint.clone(),
            })
            .collect();

        if let Err(e) = llm_proxy::config::save_providers(&providers) {
            eprintln!("mhd: failed to save providers: {e}");
        }

        // Models: each UiProvider model becomes a Model tied to that provider.
        let models: Vec<llm_proxy::config::Model> = state
            .providers
            .iter()
            .flat_map(|p| {
                p.models.iter().map(|m| llm_proxy::config::Model {
                    provider: p.name.clone(),
                    id: m.clone(),
                    display_name: String::new(),
                    tags: vec![],
                })
            })
            .collect();

        if let Err(e) = llm_proxy::config::save_models(&models) {
            eprintln!("mhd: failed to save models: {e}");
        }

        // Save the API key from the first provider as upstream_key.
        // Preserve the existing anthropic_key from secrets.json if present.
        let existing_secrets = llm_proxy::config::load_secrets().ok();
        let api_key = state.providers.first().and_then(|p| {
            if p.api_key.is_empty() {
                None
            } else {
                Some(p.api_key.clone())
            }
        });
        if api_key.is_some() || existing_secrets.is_some() {
            let secrets = llm_proxy::config::Secrets {
                anthropic_key: existing_secrets
                    .as_ref()
                    .map(|s| s.anthropic_key.clone())
                    .unwrap_or_default(),
                upstream_key: api_key.unwrap_or_default(),
            };
            if let Err(e) = llm_proxy::config::save_secrets(&secrets) {
                eprintln!("mhd: failed to save secrets: {e}");
            }
        }
    }

    if let Err(e) = state.handle.reload_config() {
        eprintln!("mhd: settings reload error: {e}");
        return;
    }

    state.theme = state.handle.theme();
}

fn save_config(
    path: &std::path::Path,
    theme: &str,
    bindings: &[UIBinding],
    autostart: bool,
    notes_dir: &std::path::Path,
    draw_dir: &std::path::Path,
    handle: &AppHandle,
) -> Result<(), String> {
    {
        let mut seen = std::collections::HashSet::new();
        for b in bindings {
            // Canonicalize so alias spellings of the same physical key
            // (e.g. "0x13" vs "pause") collapse to one entry.
            let key = match crate::core::trigger::parse_trigger(&b.trigger) {
                Ok(pt) => crate::core::trigger::keys_to_string(&crate::core::trigger::KeyCombo {
                    modifiers: pt.trigger.modifiers,
                    key: Some(pt.trigger.key),
                }),
                Err(_) => b.trigger.trim().to_lowercase(),
            };
            if !seen.insert(key) {
                return Err(format!(
                    "Duplicate trigger '{}' — each trigger must be unique within the active scheme",
                    b.trigger
                ));
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
        if theme.is_empty() {
            table.remove("theme");
        } else {
            table.insert("theme".to_string(), toml::Value::String(theme.to_string()));
        }
        table.insert(
            "active_scheme".to_string(),
            toml::Value::String(active_scheme),
        );

        if autostart {
            table.insert("autostart".to_string(), toml::Value::Boolean(true));
        } else {
            table.remove("autostart");
        }

        {
            let qn = table
                .entry("quicknote".to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(qn_table) = qn.as_table_mut() {
                qn_table.insert(
                    "notes_dir".to_string(),
                    toml::Value::String(notes_dir.to_string_lossy().into_owned()),
                );
            }
        }

        {
            let qd = table
                .entry("quickdraw".to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(qd_table) = qd.as_table_mut() {
                qd_table.insert(
                    "draw_dir".to_string(),
                    toml::Value::String(draw_dir.to_string_lossy().into_owned()),
                );
            }
        }

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

// ═══════════════════════════════════════════════════════════════════════
// Window procedure — event routing
// ═══════════════════════════════════════════════════════════════════════

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

                // Tab area inside header → HTCLIENT, not HTCAPTION,
                // so clicks on tabs work instead of dragging the window.
                if pt.y < lay.header_h() {
                    if pt.y >= lay.tab_bar_y() && pt.y < lay.tab_bar_y() + lay.tab_h() {
                        let n = state.tab_titles.len() as i32;
                        let tab_total_w = n * lay.tab_w() + (n - 1) * lay.tab_gap();
                        let tab_start_x = lay.win_w() - lay.pad() - tab_total_w;
                        for i in 0..n {
                            let tx = tab_start_x + i * (lay.tab_w() + lay.tab_gap());
                            if pt.x >= tx && pt.x < tx + lay.tab_w() {
                                return LRESULT(HTCLIENT as isize);
                            }
                        }
                    }
                    return LRESULT(HTCAPTION as isize);
                }
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

                // ── Theme search dropdown hit test (manual overlay) ───
                if state.theme_dropdown.is_open && state.active_section == SettingsPage::General {
                    let scale = state.layout.scale();
                    let combo_x = state.layout.combo_x();
                    let combo_y = state.layout.combo_y();
                    let combo_w = state.layout.combo_w();
                    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
                    let dropdown_top = combo_y + combo_h;
                    let item_h = (24.0 * scale) as i32;
                    let search_h = (30.0 * scale) as i32;
                    let visible_rows = 8;
                    let filtered_count = state
                        .theme_dropdown
                        .filtered_count(&state.theme_search_items);
                    let max_visible = filtered_count.min(visible_rows);
                    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
                    let dropdown_w = combo_w;

                    // Click on the combo button itself → let it reach normal ThemeCombo handler
                    let on_combo_button = y >= combo_y
                        && y < combo_y + combo_h
                        && x >= combo_x
                        && x < combo_x + combo_w;

                    // Click inside the dropdown popup area
                    let on_dropdown = y >= dropdown_top
                        && y < dropdown_top + dropdown_h
                        && x >= combo_x
                        && x < combo_x + dropdown_w;

                    if on_combo_button {
                        // Fall through to normal hit test (triggers toggle)
                    } else if on_dropdown {
                        if y >= dropdown_top + search_h {
                            // Click on a list item
                            let item_idx = (y - (dropdown_top + search_h)) / item_h;
                            let visible_items = state
                                .theme_dropdown
                                .visible_items(&state.theme_search_items, visible_rows);
                            if (item_idx as usize) < visible_items.len() {
                                let selected_id = visible_items[item_idx as usize].id;
                                if selected_id < state.theme_names.len() {
                                    state.theme_sel = selected_id;
                                    apply_settings(state);
                                    close_combo_popup(state);
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                        }
                        return LRESULT(0);
                    } else {
                        // Click outside dropdown → close it and consume the click
                        close_combo_popup(state);
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }
                }

                let hit = hit_test_settings(state, x, y);
                match hit {
                    SettingsHit::Tab(ti) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        let new_section = match ti {
                            0 => SettingsPage::General,
                            1 => SettingsPage::Shortcuts,
                            2 => SettingsPage::LlmProxy,
                            _ => SettingsPage::Advanced,
                        };
                        if state.active_section != new_section {
                            state.active_section = new_section;
                            state.content_scroll_y = 0;
                            state.content_scroll_y = 0;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    SettingsHit::ThemeCombo => {
                        toggle_combo_popup(state);
                    }
                    SettingsHit::AutostartToggle => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.autostart = !state.autostart;
                        if state.autostart {
                            if let Err(e) = crate::autostart::install_autostart() {
                                let msg = format!("Failed to enable autostart:\n{e}");
                                let wz = to_utf16_z(&msg);
                                let title = to_utf16_z("mhd Autostart Error");
                                let _ = MessageBoxW(
                                    hwnd,
                                    PCWSTR::from_raw(wz.as_ptr()),
                                    PCWSTR::from_raw(title.as_ptr()),
                                    MB_OK | MB_ICONERROR,
                                );
                                state.autostart = false;
                            }
                        } else {
                            if let Err(e) = crate::autostart::remove_autostart() {
                                let msg = format!("Failed to disable autostart:\n{e}");
                                let wz = to_utf16_z(&msg);
                                let title = to_utf16_z("mhd Autostart Error");
                                let _ = MessageBoxW(
                                    hwnd,
                                    PCWSTR::from_raw(wz.as_ptr()),
                                    PCWSTR::from_raw(title.as_ptr()),
                                    MB_OK | MB_ICONERROR,
                                );
                            }
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::Scrollbar => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        let lay = state.layout;
                        let content_h = page_control_content_height(state, &lay);
                        let max_scroll = (content_h - lay.content_visible_h()).max(0);
                        let scrollbar_h = lay.content_visible_h();
                        let thumb_h =
                            ((scrollbar_h as f32 / content_h as f32) * scrollbar_h as f32) as i32;
                        let thumb_h = thumb_h.max((16.0 * lay.scale()) as i32);
                        let thumb_travel = (scrollbar_h - thumb_h).max(1);
                        let thumb_y = lay.content_y()
                            + ((state.content_scroll_y as f32 / max_scroll as f32)
                                * thumb_travel as f32) as i32;

                        if y >= thumb_y && y < thumb_y + thumb_h {
                            state.is_dragging_scroll = true;
                            state.scroll_drag_start_y = y;
                            state.scroll_drag_start_offset = state.content_scroll_y;
                        } else {
                            let track_click_y = y - lay.content_y() - thumb_h / 2;
                            let pct = track_click_y as f32 / thumb_travel as f32;
                            state.content_scroll_y = (pct * max_scroll as f32) as i32;
                            state.content_scroll_y = state.content_scroll_y.clamp(0, max_scroll);
                            state.is_dragging_scroll = true;
                            state.scroll_drag_start_y = y;
                            state.scroll_drag_start_offset = state.content_scroll_y;
                            paint_settings(hwnd, state_ptr, &lay);
                        }
                        let _ = SetCapture(hwnd);
                    }
                    SettingsHit::RowClick(i) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        // Open modal binding editor popup instead of inline accordion.
                        crate::config::editor_binding_popup::open_binding_popup(hwnd, state_ptr, i);
                        // On return, redraw the settings window to reflect any changes.
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::RowDelete(i) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        finish_inline_edit(state);
                        let row_y = state.layout.list_y() - state.content_scroll_y
                            + (i as i32) * state.layout.row_h();
                        handle_list_click(state, i, x, y, row_y);
                    }
                    SettingsHit::AddBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        let idx = state.bindings.len();
                        state.bindings.push(UIBinding {
                            trigger: "none".to_string(),
                            kind_idx: 0,
                            param: "".to_string(),
                            is_recording_trigger: false,
                            is_recording_param: false,
                        });
                        state.acc_is_recording = false;
                        state.acc_is_recording_param = false;
                        state.acc_save_error = None;
                        state.expanded_idx = None;
                        let saved = crate::config::editor_binding_popup::open_binding_popup(
                            hwnd, state_ptr, idx,
                        );
                        if !saved && idx < state.bindings.len() {
                            state.bindings.remove(idx);
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::NotesDirBrowseBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(path) = browse_for_folder(hwnd) {
                            state.notes_dir = path;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    SettingsHit::DrawDirBrowseBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(path) = browse_for_folder(hwnd) {
                            state.draw_dir = path;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    // ── LLM Proxy: provider list ─────────────────────
                    SettingsHit::ProviderRow(i) | SettingsHit::ProviderEditBtn(i) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if i < state.providers.len() {
                            let p = state.providers[i].clone();
                            if let Some(updated) =
                                crate::config::editor_provider_popup::open_provider_popup(
                                    hwnd,
                                    state_ptr,
                                    Some(p),
                                )
                            {
                                state.providers[i] = updated;
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                        }
                    }
                    SettingsHit::ProviderModelsBtn(i) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if i < state.providers.len() {
                            let models = state.providers[i].models.clone();
                            let endpoint = state.providers[i].endpoint.clone();
                            let api_key = state.providers[i].api_key.clone();
                            if let Some(updated_models) =
                                crate::config::editor_provider_models_popup::open_models_popup(
                                    hwnd,
                                    &state.theme,
                                    state.layout.scale(),
                                    models,
                                    &endpoint,
                                    &api_key,
                                )
                            {
                                state.providers[i].models = updated_models;
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                        }
                    }
                    SettingsHit::ProviderDelete(i) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if i < state.providers.len() {
                            state.providers.remove(i);
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    SettingsHit::ProviderAddBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(new_p) =
                            crate::config::editor_provider_popup::open_provider_popup(
                                hwnd, state_ptr, None,
                            )
                        {
                            state.providers.push(new_p);
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    SettingsHit::SaveBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        apply_settings(state);
                        DestroyWindow(hwnd).ok();
                    }
                    SettingsHit::ApplyBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        apply_settings(state);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::CloseBtn => {
                        DestroyWindow(hwnd).ok();
                    }
                    SettingsHit::AdvancedButton(idx) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        match idx {
                            0 => {
                                let path = state.handle.config_path.to_string_lossy().to_string();
                                let wz = to_utf16_z(&path);
                                let _ = ShellExecuteW(
                                    None,
                                    None,
                                    PCWSTR::from_raw(wz.as_ptr()),
                                    None,
                                    None,
                                    SW_SHOW,
                                );
                            }
                            1 => {
                                if let Some(parent) = state.handle.config_path.parent() {
                                    let path = parent.to_string_lossy().to_string();
                                    let wz = to_utf16_z(&path);
                                    let _ = ShellExecuteW(
                                        None,
                                        None,
                                        PCWSTR::from_raw(wz.as_ptr()),
                                        None,
                                        None,
                                        SW_SHOW,
                                    );
                                }
                            }
                            2 => {
                                let dir = state.handle.config_path.parent().map_or_else(
                                    || std::path::PathBuf::from("."),
                                    |p| p.join("blackbox"),
                                );
                                let path = dir.to_string_lossy().to_string();
                                let wz = to_utf16_z(&path);
                                let _ = ShellExecuteW(
                                    None,
                                    None,
                                    PCWSTR::from_raw(wz.as_ptr()),
                                    None,
                                    None,
                                    SW_SHOW,
                                );
                            }
                            3 => {
                                let crash_path = state.handle.config_path.parent().map_or_else(
                                    || std::path::PathBuf::from("crash.log"),
                                    |p| p.join("crash.log"),
                                );
                                let path = crash_path.to_string_lossy().to_string();
                                let wz = to_utf16_z(&path);
                                let _ = ShellExecuteW(
                                    None,
                                    None,
                                    PCWSTR::from_raw(wz.as_ptr()),
                                    None,
                                    None,
                                    SW_SHOW,
                                );
                            }
                            4 => {
                                if let Ok(new) =
                                    crate::config::AppConfig::parse("", &state.handle.config_path)
                                {
                                    *state.handle.config.lock().unwrap() = new;
                                    state.bindings = load_ui_bindings(&state.handle);
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                            5 => {
                                if let Ok(new) =
                                    crate::config::AppConfig::parse("", &state.handle.config_path)
                                {
                                    *state.handle.config.lock().unwrap() = new;
                                    state.bindings = load_ui_bindings(&state.handle);
                                    state.theme = NativeTheme::default();
                                    state.theme_names = build_theme_list(&state.theme);
                                    state.theme_sel = 0;
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                            _ => {}
                        }
                    }
                    SettingsHit::AccordionSaveBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(idx) = state.expanded_idx
                            && idx < state.bindings.len()
                        {
                            state.acc_is_recording = false;
                            crate::hook::set_recording_window(None);
                            let trigger = state.acc_trigger.trim().to_lowercase();
                            if !trigger.is_empty() {
                                // Compare resolved triggers, not raw strings,
                                // so aliases of the same physical key (e.g.
                                // "0x13" and "pause") are caught as dupes.
                                let dup = match crate::core::trigger::parse_trigger(&trigger) {
                                    Ok(new_pt) => {
                                        state.bindings.iter().enumerate().any(|(j, b)| {
                                            j != idx
                                                && crate::core::trigger::parse_trigger(&b.trigger)
                                                    .map(|pt| pt.trigger == new_pt.trigger)
                                                    .unwrap_or(false)
                                        })
                                    }
                                    // Unparseable: fall back to string compare.
                                    Err(_) => state.bindings.iter().enumerate().any(|(j, b)| {
                                        j != idx && b.trigger.trim().to_lowercase() == trigger
                                    }),
                                };
                                if dup {
                                    state.acc_save_error = Some("Duplicate trigger – each shortcut needs a unique key combination.".into());
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                    return LRESULT(0);
                                }
                            }
                            state.bindings[idx].trigger = state.acc_trigger.clone();
                            state.bindings[idx].kind_idx = state.acc_kind_idx;
                            state.bindings[idx].param = state.acc_param.clone();
                            state.bindings[idx].is_recording_trigger = false;
                            state.bindings[idx].is_recording_param = false;
                        }
                        state.expanded_idx = None;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionCancelBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.acc_is_recording = false;
                        state.acc_is_recording_param = false;
                        crate::hook::set_recording_window(None);
                        state.expanded_idx = None;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionDeleteBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(idx) = state.expanded_idx
                            && idx < state.bindings.len()
                        {
                            state.bindings.remove(idx);
                            if let Some((ri_idx, is_trig)) = state.recording_info {
                                if ri_idx == idx {
                                    state.recording_info = None;
                                    crate::hook::set_recording_window(None);
                                } else if ri_idx > idx {
                                    state.recording_info = Some((ri_idx - 1, is_trig));
                                }
                            }
                        }
                        state.acc_is_recording = false;
                        state.acc_is_recording_param = false;
                        crate::hook::set_recording_window(None);
                        state.expanded_idx = None;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionRecordBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.acc_is_recording = !state.acc_is_recording;
                        if state.acc_is_recording {
                            state.acc_is_recording_param = false;
                            crate::hook::set_recording_window(Some(state.hwnd));
                        } else {
                            crate::hook::set_recording_window(None);
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionActionBtn => {
                        close_combo_popup(state);
                        if let Some(idx) = state.expanded_idx {
                            state.bindings[idx].trigger = state.acc_trigger.clone();
                            state.bindings[idx].kind_idx = state.acc_kind_idx;
                            state.bindings[idx].param = state.acc_param.clone();
                            open_kind_menu(state, idx);
                            if idx < state.bindings.len() {
                                state.acc_kind_idx = state.bindings[idx].kind_idx;
                                state.acc_param = state.bindings[idx].param.clone();
                                let new_desc = editor_action_desc(state.acc_kind_idx);
                                if new_desc.param_schema != ActionParamSchema::KeyMapping {
                                    state.acc_is_recording_param = false;
                                }
                            }
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionTriggerField => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.acc_is_recording = !state.acc_is_recording;
                        if state.acc_is_recording {
                            state.acc_is_recording_param = false;
                            crate::hook::set_recording_window(Some(state.hwnd));
                        } else {
                            crate::hook::set_recording_window(None);
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::AccordionParamField => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(idx) = state.expanded_idx {
                            let gap = (8.0 * state.layout.scale()) as i32;
                            let btn_h = (28.0 * state.layout.scale()) as i32;
                            let desc = editor_action_desc(state.acc_kind_idx);
                            let pw_adjust = if desc.param_schema == ActionParamSchema::KeyMapping {
                                btn_h + gap
                            } else {
                                0
                            };
                            let pw =
                                state.layout.win_w() - state.layout.pad() * 2 - gap * 2 - pw_adjust;
                            let accordion_y = state.layout.list_y() - state.content_scroll_y
                                + (idx as i32 + 1) * state.layout.row_h()
                                + (12.0 * state.layout.scale()) as i32;
                            let field_rect = RECT {
                                left: state.layout.pad() + gap,
                                top: accordion_y + btn_h + gap,
                                right: state.layout.pad() + gap + pw,
                                bottom: accordion_y + btn_h + gap + btn_h,
                            };
                            spawn_inline_edit(state, idx, field_rect);
                        }
                    }
                    SettingsHit::AccordionParamRecordBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(old) = state.param_edit_popup.take() {
                            let _ = DestroyWindow(old);
                            state.param_edit_idx = None;
                        }
                        state.acc_is_recording_param = !state.acc_is_recording_param;
                        if state.acc_is_recording_param {
                            state.acc_is_recording = false;
                            crate::hook::set_recording_window(Some(state.hwnd));
                        } else {
                            crate::hook::set_recording_window(None);
                        }
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::None => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                    }
                }
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
                        let content_h = page_control_content_height(state, &lay);
                        let max_scroll = (content_h - lay.content_visible_h()).max(0);
                        let scrollbar_h = lay.content_visible_h();
                        let thumb_h =
                            ((scrollbar_h as f32 / content_h as f32) * scrollbar_h as f32) as i32;
                        let thumb_h = thumb_h.max((16.0 * lay.scale()) as i32);
                        let thumb_travel = (scrollbar_h - thumb_h).max(1);
                        let dy = y - state.scroll_drag_start_y;
                        let scroll_delta =
                            (dy as f32 / thumb_travel as f32 * max_scroll as f32) as i32;
                        state.content_scroll_y =
                            (state.scroll_drag_start_offset + scroll_delta).clamp(0, max_scroll);
                        paint_settings(hwnd, state_ptr, &lay);
                    } else {
                        let target = hit_test_settings(state, x, y);

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
                    if state.hovered_target != SettingsHit::None {
                        state.hovered_target = SettingsHit::None;
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
                let screen_x = (lparam.0 as i16) as i32;
                let screen_y = ((lparam.0 >> 16) as i16) as i32;
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;

                    // Theme search dropdown scroll
                    if state.theme_dropdown.is_open && state.active_section == SettingsPage::General
                    {
                        let delta_rows = if delta > 0 { -3 } else { 3 };
                        state
                            .theme_dropdown
                            .scroll_by(delta_rows, &state.theme_search_items, 8);
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }

                    let lay = state.layout;
                    let content_h = page_control_content_height(state, &lay);
                    let max_scroll = (content_h - lay.content_visible_h()).max(0);
                    state.content_scroll_y =
                        (state.content_scroll_y - (delta as i32 / 120) * 40).clamp(0, max_scroll);
                    paint_settings(hwnd, state_ptr, &lay);

                    // Recompute hover at the cursor position from the message
                    // so the highlight follows scrolling immediately.
                    let mut pt = POINT {
                        x: screen_x,
                        y: screen_y,
                    };
                    let _ = ScreenToClient(hwnd, &mut pt);
                    let new_target = hit_test_settings(state, pt.x, pt.y);
                    if state.hovered_target != new_target {
                        state.hovered_target = new_target;
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
                    if state.acc_is_recording {
                        state.acc_trigger = trigger_str;
                        state.acc_is_recording = false;
                        state.recording_info = None;
                        crate::hook::set_recording_window(None);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    } else if state.acc_is_recording_param {
                        state.acc_param = trigger_str;
                        state.acc_is_recording_param = false;
                        state.recording_info = None;
                        crate::hook::set_recording_window(None);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    } else if let Some((idx, is_trigger)) = state.recording_info.take() {
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
                            0x0D /* VK_RETURN */ => { finish_inline_edit(state); }
                            0x1B /* VK_ESCAPE */ => { cancel_inline_edit(state); }
                            0x41 if ctrl_down => { // Ctrl+A
                                state.edit_select_start = Some(0);
                                state.edit_cursor = state.edit_text.len();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x43 if ctrl_down => { // Ctrl+C
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
                                                let _ = SetClipboardData(13u32, HANDLE(handle.0));
                                            }
                                        }
                                        let _ = CloseClipboard();
                                    }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x56 if ctrl_down => { // Ctrl+V
                                if is_selected {
                                    let (s, e) = (sel_start, sel_end);
                                    state.edit_text.drain(s..e);
                                    state.edit_cursor = s;
                                    state.edit_select_start = None;
                                }
                                if OpenClipboard(hwnd).is_ok() {
                                    if let Ok(handle) = GetClipboardData(13u32) {
                                        let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
                                        if !ptr.is_null() {
                                            let len = (0..).find(|&i| *ptr.add(i) == 0).unwrap_or(0);
                                            if len > 0 {
                                                let paste_str = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                                                let filtered: String = paste_str.chars().filter(|ch| ch.is_ascii_graphic() || *ch == ' ').collect();
                                                state.edit_text.insert_str(state.edit_cursor, &filtered);
                                                state.edit_cursor += filtered.len();
                                            }
                                            let _ = GlobalUnlock(HGLOBAL(handle.0));
                                        }
                                    }
                                    let _ = CloseClipboard();
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x58 if ctrl_down => { // Ctrl+X
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
                                                let _ = SetClipboardData(13u32, HANDLE(handle.0));
                                            }
                                        }
                                        let _ = CloseClipboard();
                                    }
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
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
                                    if state.edit_cursor > 0 { state.edit_cursor -= 1; }
                                } else {
                                    state.edit_select_start = None;
                                    if state.edit_cursor > 0 { state.edit_cursor -= 1; }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x27 /* VK_RIGHT */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
                                    if state.edit_cursor < state.edit_text.len() { state.edit_cursor += 1; }
                                } else {
                                    state.edit_select_start = None;
                                    if state.edit_cursor < state.edit_text.len() { state.edit_cursor += 1; }
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x24 /* VK_HOME */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
                                    state.edit_cursor = 0;
                                } else {
                                    state.edit_select_start = None;
                                    state.edit_cursor = 0;
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x23 /* VK_END */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
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

                    // Theme search dropdown keyboard handling
                    if state.theme_dropdown.is_open {
                        let vk = wparam.0 as u32;
                        match vk {
                            0x0D /* VK_RETURN */ => {
                                // Select first visible item (or current selection)
                                let visible = state.theme_dropdown.visible_items(
                                    &state.theme_search_items,
                                    8,
                                );
                                if let Some(first) = visible.first() {
                                    if first.id < state.theme_names.len() {
                                        state.theme_sel = first.id;
                                        apply_settings(state);
                                        close_combo_popup(state);
                                        paint_settings(hwnd, state_ptr, &state.layout);
                                    }
                                }
                            }
                            0x1B /* VK_ESCAPE */ => {
                                close_combo_popup(state);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x08 /* VK_BACK */ => {
                                state.theme_dropdown.backspace(&state.theme_search_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x26 /* VK_UP */ => {
                                state.theme_dropdown.scroll_by(-1, &state.theme_search_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x28 /* VK_DOWN */ => {
                                state.theme_dropdown.scroll_by(1, &state.theme_search_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x21 /* VK_PRIOR */ => {
                                state.theme_dropdown.scroll_by(-8, &state.theme_search_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x22 /* VK_NEXT */ => {
                                state.theme_dropdown.scroll_by(8, &state.theme_search_items, 8);
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
                        if ch.is_ascii_graphic() || ch == ' ' {
                            if let Some(sel) = state.edit_select_start
                                && sel != state.edit_cursor
                            {
                                let (s, e) =
                                    (sel.min(state.edit_cursor), sel.max(state.edit_cursor));
                                state.edit_text.drain(s..e);
                                state.edit_cursor = s;
                                state.edit_select_start = None;
                            }
                            state.edit_text.insert(state.edit_cursor, ch);
                            state.edit_cursor += 1;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                        return LRESULT(0);
                    }

                    // Theme search dropdown character input
                    if state.theme_dropdown.is_open {
                        let ch = (wparam.0 as u32) as u8 as char;
                        if ch.is_ascii_graphic() || ch == ' ' {
                            state
                                .theme_dropdown
                                .input_char(ch, &state.theme_search_items, 8);
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

// ═══════════════════════════════════════════════════════════════════════
// Combo popup (theme selector)
// ═══════════════════════════════════════════════════════════════════════

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

                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                let (theme, scale) = if !state_ptr.is_null() {
                    (&(*state_ptr).theme, (*state_ptr).layout.scale())
                } else {
                    (&NativeTheme::default(), 1.0)
                };

                let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * scale) as i32;

                let bg = CreateSolidBrush(theme.background.to_colorref());
                let _ = FillRect(hdc, &rc, bg);
                let _ = DeleteObject(bg);

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

                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    let font_h = -(12.0 * state.layout.scale()) as i32;
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
                        let highlight = if i == state.theme_sel {
                            Some(theme.selected)
                        } else if state.hover_sel == Some(i) {
                            Some(theme.hover)
                        } else {
                            None
                        };
                        if let Some(c) = highlight {
                            let blended: crate::core::native_theme::Argb =
                                c.blend_over(theme.background);
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
                    let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * state.layout.scale()) as i32;
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
                    let item_h = (COMBO_POPUP_ITEM_HEIGHT as f32 * state.layout.scale()) as i32;
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

// ═══════════════════════════════════════════════════════════════════════
// Param edit popup (RichEdit inline editor)
// ═══════════════════════════════════════════════════════════════════════

unsafe extern "system" fn param_edit_rich_edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CHAR {
        let ch = (wparam.0 as u16) as u8 as char;
        if ch == '\r' {
            if let Ok(parent) = unsafe { GetParent(hwnd) } {
                unsafe {
                    let _ = SendMessageW(parent, WM_PARAM_EDIT_COMMIT, WPARAM(0), LPARAM(0));
                }
            }
            return LRESULT(0);
        }
        if ch == '\x1b' {
            if let Ok(parent) = unsafe { GetParent(hwnd) } {
                unsafe {
                    let _ = DestroyWindow(parent);
                }
            }
            return LRESULT(0);
        }
    }
    let old_proc = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if old_proc != 0 {
        let proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            unsafe { std::mem::transmute(old_proc) };
        unsafe { proc(hwnd, msg, wparam, lparam) }
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

unsafe extern "system" fn param_edit_popup_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            let info = unsafe { &*(cs.lpCreateParams as *const ParamEditCreateInfo) };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, info.state_ptr as isize);
            }

            unsafe {
                let _ = LoadLibraryW(windows::core::w!("msftedit.dll"));
            }

            if let Ok(edit) = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::w!("RICHEDIT50W"),
                    PCWSTR::null(),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | ES_AUTOHSCROLL as u32),
                    0,
                    0,
                    info.width,
                    info.height,
                    hwnd,
                    HMENU::default(),
                    GetModuleHandleW(None).unwrap_or_default(),
                    None,
                )
            } {
                unsafe {
                    let _ = SendMessageW(
                        edit,
                        WM_SETFONT,
                        WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as _),
                        LPARAM(1),
                    );
                    let old_proc = SetWindowLongPtrW(
                        edit,
                        GWLP_WNDPROC,
                        param_edit_rich_edit_subclass as *const () as isize,
                    );
                    SetWindowLongPtrW(edit, GWLP_USERDATA, old_proc);
                    let brush = CreateSolidBrush(info.brush_color.to_colorref());
                    let _ = SetPropW(
                        edit,
                        windows::core::w!("EDIT_BRUSH"),
                        HANDLE(brush.0 as *mut _),
                    );
                    let _ = SendMessageW(
                        edit,
                        EM_SETBKGNDCOLOR,
                        WPARAM(0),
                        LPARAM(info.brush_color.to_colorref().0 as isize),
                    );
                    let wz = &info.initial_text;
                    let len = wz
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(info.initial_text.len());
                    if len > 0 {
                        let _ =
                            SendMessageW(edit, WM_SETTEXT, WPARAM(0), LPARAM(wz.as_ptr() as isize));
                    }
                    let cf = CHARFORMATW {
                        cbSize: std::mem::size_of::<CHARFORMATW>() as u32,
                        dwMask: CFM_COLOR,
                        dwEffects: CFE_EFFECTS::default(),
                        crTextColor: info.text_color.to_colorref(),
                        ..Default::default()
                    };
                    let _ = SendMessageW(
                        edit,
                        EM_SETCHARFORMAT,
                        WPARAM((SCF_DEFAULT | SCF_ALL) as usize),
                        LPARAM(&cf as *const _ as isize),
                    );
                    let _ = SetWindowPos(edit, None, 0, 0, info.width, info.height, SWP_NOZORDER);
                    let _ = SetFocus(edit);
                }
            }
            LRESULT(0)
        }

        WM_SIZE => {
            let w = (lparam.0 as i16) as i32;
            let h = ((lparam.0 >> 16) as i16) as i32;
            if let Ok(edit) = unsafe { GetWindow(hwnd, GW_CHILD) } {
                unsafe {
                    let _ = SetWindowPos(edit, None, 0, 0, w, h, SWP_NOZORDER);
                }
            }
            LRESULT(0)
        }

        WM_PARAM_EDIT_COMMIT => {
            if let Ok(edit) = unsafe { GetWindow(hwnd, GW_CHILD) } {
                let text_len = unsafe { SendMessageW(edit, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)) }
                    .0 as usize;
                let mut buf = vec![0u16; text_len + 1];
                unsafe {
                    SendMessageW(
                        edit,
                        WM_GETTEXT,
                        WPARAM(buf.len() as _),
                        LPARAM(buf.as_mut_ptr() as isize),
                    );
                }
                let new_text = String::from_utf16_lossy(&buf[..text_len]);

                let state_ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState };
                if !state_ptr.is_null() {
                    let state = unsafe { &mut *state_ptr };
                    if let Some(idx) = state.param_edit_idx
                        && idx < state.bindings.len()
                    {
                        if state.expanded_idx == Some(idx) {
                            state.acc_param = new_text;
                        } else {
                            state.bindings[idx].param = new_text;
                        }
                        paint_settings(state.hwnd, state_ptr, &state.layout);
                    }
                    state.param_edit_popup = None;
                    state.param_edit_idx = None;
                }
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_ACTIVATE => {
            if loword(wparam.0 as u32) == 0 {
                let state_ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState };
                if !state_ptr.is_null() {
                    let state = unsafe { &mut *state_ptr };
                    state.param_edit_popup = None;
                    state.param_edit_idx = None;
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            if let Ok(edit) = unsafe { GetWindow(hwnd, GW_CHILD) } {
                let brush_handle = unsafe { GetPropW(edit, windows::core::w!("EDIT_BRUSH")) };
                if brush_handle.0 as usize != 0 {
                    unsafe {
                        let _ = DeleteObject(HBRUSH(brush_handle.0 as _));
                        let _ = RemovePropW(edit, windows::core::w!("EDIT_BRUSH"));
                    }
                }
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn loword(dw: u32) -> u16 {
    (dw & 0xffff) as u16
}

// ═══════════════════════════════════════════════════════════════════════
// Theme search dropdown
// ═══════════════════════════════════════════════════════════════════════

fn toggle_combo_popup(state: &mut SettingsState) {
    close_kind_popup(state);
    if state.combo_open.load(Ordering::SeqCst) {
        close_combo_popup(state);
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    } else {
        // Destroy old HWND popup if any (legacy), then open search dropdown
        if let Some(popup) = state.combo_popup.take() {
            unsafe {
                DestroyWindow(popup).ok();
            }
        }
        state.combo_open.store(true, Ordering::SeqCst);
        state
            .theme_dropdown
            .open(&state.theme_search_items, state.theme_sel, 8);
        paint_settings(state.hwnd, state as *mut SettingsState, &state.layout);
    }
}

fn close_combo_popup(state: &mut SettingsState) {
    if let Some(popup) = state.combo_popup.take() {
        unsafe {
            DestroyWindow(popup).ok();
        }
    }
    state.combo_open.store(false, Ordering::SeqCst);
    state.theme_dropdown.close();
}

fn close_kind_popup(_state: &mut SettingsState) {
    // No-op: native HMENU is self-dismissing.
}

// ═══════════════════════════════════════════════════════════════════════
// Action kind menu (cascading popup)
// ═══════════════════════════════════════════════════════════════════════

fn open_kind_menu(state: &mut SettingsState, idx: usize) {
    unsafe {
        let main_menu = CreatePopupMenu();
        let Ok(main_menu) = main_menu else { return };
        if main_menu == HMENU::default() {
            return;
        }

        use crate::core::action::ActionCategory;
        let mut categories: Vec<ActionCategory> = Vec::new();
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
            let cat_label = to_utf16_z(cat.label());
            let _ = AppendMenuW(
                main_menu,
                MF_POPUP | MF_STRING,
                sub.0 as usize,
                PCWSTR::from_raw(cat_label.as_ptr()),
            );
        }

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

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
        let _ = DestroyMenu(main_menu);

        if chosen >= ID_ACTION_BASE {
            let selected = chosen - ID_ACTION_BASE;
            if selected < EDITOR_ACTION_NAMES.len() {
                if state.bindings[idx].kind_idx != selected {
                    state.bindings[idx].kind_idx = selected;
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

// ═══════════════════════════════════════════════════════════════════════
// Row click handler
// ═══════════════════════════════════════════════════════════════════════

fn handle_list_click(state: &mut SettingsState, idx: usize, x: i32, y: i32, row_y: i32) {
    let lay = state.layout;
    if x >= lay.win_w() - lay.pad() - lay.del_w()
        && x < lay.win_w() - lay.pad()
        && y >= row_y + (lay.row_h() - lay.del_w()) / 2
        && y < row_y + (lay.row_h() + lay.del_w()) / 2
    {
        close_kind_popup(state);
        state.bindings.remove(idx);
        if let Some((ri_idx, is_trig)) = state.recording_info {
            if ri_idx == idx {
                state.recording_info = None;
                crate::hook::set_recording_window(None);
            } else if ri_idx > idx {
                state.recording_info = Some((ri_idx - 1, is_trig));
            }
        }
        state.expanded_idx = None;
        paint_settings(state.hwnd, state as *mut SettingsState, &lay);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// File dialog (Run Program)
// ═══════════════════════════════════════════════════════════════════════

#[allow(dead_code)] // kept for editor panel FilePath param
fn pick_program_file(parent: HWND) -> Option<String> {
    use std::mem;
    unsafe {
        let mut ofn: windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW = mem::zeroed();
        let mut buf = [0u16; 1024];
        let filter: Vec<u16> = "Programs\0*.exe;*.lnk;*.bat\0All Files\0*.*\0\0"
            .encode_utf16()
            .collect();

        ofn.lStructSize =
            mem::size_of::<windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW>() as u32;
        ofn.hwndOwner = parent;
        ofn.lpstrFilter = PCWSTR(filter.as_ptr());
        ofn.lpstrFile = PWSTR(buf.as_mut_ptr());
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

// ═══════════════════════════════════════════════════════════════════════
// Theme search dropdown painting
// ═══════════════════════════════════════════════════════════════════════

/// Draw the theme search dropdown overlay.
fn draw_theme_dropdown(
    dib_dc: HDC,
    bits: *mut c_void,
    lay: &Layout,
    state: &SettingsState,
    body_font: HFONT,
    small_font: HFONT,
) {
    let theme = &state.theme;
    let scale = lay.scale();
    let combo_x = lay.combo_x();
    let combo_y = lay.combo_y();
    let combo_w = lay.combo_w();
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
    let dropdown_top = combo_y + combo_h;
    let item_h = (24.0 * scale) as i32;
    let search_h = (30.0 * scale) as i32;
    let visible_rows = 8;
    let filtered_count = state
        .theme_dropdown
        .filtered_count(&state.theme_search_items);
    let max_visible = filtered_count.min(visible_rows);
    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
    let dropdown_w = combo_w;

    let dropdown_rect = RECT {
        left: combo_x,
        top: dropdown_top,
        right: combo_x + dropdown_w,
        bottom: dropdown_top + dropdown_h,
    };

    // Background
    let bg = theme.surface.blend_over(theme.background);
    draw_rounded_rect_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        dropdown_rect,
        (4.0 * scale) as i32,
        bg,
    );
    draw_rounded_border_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        dropdown_rect,
        (4.0 * scale) as i32,
        1,
        theme.border,
    );

    // Search field
    let search_rect = RECT {
        left: combo_x + 4,
        top: dropdown_top + 2,
        right: combo_x + dropdown_w - 4,
        bottom: dropdown_top + 2 + search_h,
    };
    let search_bg = theme.background;
    draw_rounded_rect_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        search_rect,
        (4.0 * scale) as i32,
        search_bg,
    );
    draw_rounded_border_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        search_rect,
        (4.0 * scale) as i32,
        1,
        theme.border,
    );

    unsafe {
        let _ = SelectObject(dib_dc, body_font);
        let search_text = if state.theme_dropdown.filter.is_empty() {
            "Search themes…"
        } else {
            state.theme_dropdown.filter.as_str()
        };
        let _ = SetTextColor(
            dib_dc,
            if state.theme_dropdown.filter.is_empty() {
                theme.text_muted
            } else {
                theme.text
            }
            .to_colorref(),
        );
        let mut search_wz = to_utf16_z(search_text);
        let mut search_text_rc = RECT {
            left: search_rect.left + 4,
            top: search_rect.top,
            right: search_rect.right - 4,
            bottom: search_rect.bottom,
        };
        let _ = DrawTextW(
            dib_dc,
            &mut search_wz,
            &mut search_text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Items list
    let visible_items = state
        .theme_dropdown
        .visible_items(&state.theme_search_items, visible_rows);
    let list_top = dropdown_top + 2 + search_h;

    for (i, item) in visible_items.iter().enumerate() {
        let item_rect = RECT {
            left: combo_x + 4,
            top: list_top + i as i32 * item_h,
            right: combo_x + dropdown_w - 4,
            bottom: list_top + (i as i32 + 1) * item_h,
        };

        let is_selected = state.theme_sel == item.id;
        let highlight = if is_selected {
            Some(theme.selected)
        } else {
            None
        };
        if let Some(c) = highlight {
            draw_rounded_rect_in_buffer(
                bits,
                lay.win_w(),
                lay.win_h(),
                item_rect,
                (2.0 * scale) as i32,
                c,
            );
        }

        unsafe {
            let _ = SelectObject(dib_dc, small_font);
            let _ = SetTextColor(dib_dc, theme.text.to_colorref());
            let mut label_wz = to_utf16_z(&item.label);
            let mut label_rc = RECT {
                left: item_rect.left + 4,
                top: item_rect.top,
                right: item_rect.right - 4,
                bottom: item_rect.bottom,
            };
            let _ = DrawTextW(
                dib_dc,
                &mut label_wz,
                &mut label_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // No results message
    if visible_items.is_empty() {
        unsafe {
            let _ = SelectObject(dib_dc, small_font);
            let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
            let mut empty_wz = to_utf16_z("No matching themes");
            let mut empty_rc = RECT {
                left: combo_x + 4,
                top: list_top,
                right: combo_x + dropdown_w - 4,
                bottom: list_top + item_h,
            };
            let _ = DrawTextW(
                dib_dc,
                &mut empty_wz,
                &mut empty_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

fn create_font(h: i32, bold: bool, family: &str) -> HFONT {
    crate::renderer::create_font(h, bold, family)
}

fn monitor_work_rect() -> RECT {
    crate::renderer::primary_monitor_work_rect()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::action::Action;

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
