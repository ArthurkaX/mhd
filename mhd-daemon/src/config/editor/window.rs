//! Window creation, lifecycle, and top-level paint dispatch for the
//! Settings panel. Unsafe Win32 boundary code lives here (and in `events`).

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use windows::Win32::Foundation::{HINSTANCE, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::FOS_PICKFOLDERS;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR};

use super::*;
use crate::app::AppHandle;
use crate::app::DaemonControl;
use crate::core::native_theme::{Argb, NativeTheme, load_theme_from_path};

// ═══════════════════════════════════════════════════════════════════════
// Folder browser (IFileOpenDialog, Vista+)
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn browse_for_folder(hwnd: HWND) -> Option<std::path::PathBuf> {
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
    let keycast_config = handle.keycast_config();

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
        keycast_position: keycast_config.position,
        keycast_duration_ms: keycast_config.duration_ms,
        keycast_show_typing: keycast_config.show_typing,
        keycast_typing_width_chars: keycast_config.typing_width_chars,
        keycast_typing_duration_ms: keycast_config.typing_duration_ms,
        bindings,
        // Load global proxy settings: anthropic key + bind address
        anthropic_key: llm_proxy::config::load_secrets()
            .map(|s| s.anthropic_key)
            .unwrap_or_default(),
        proxy_bind_address: llm_proxy::config::load_settings()
            .map(|s| format!("{}:{}", s.bind_ip, s.port))
            .unwrap_or_else(|_| "127.0.0.1:8317".to_string()),
        opus_downgrade_enabled: llm_proxy::config::load_settings()
            .map(|s| s.opus_downgrade_enabled)
            .unwrap_or(false),
        sonnet_downgrade_enabled: llm_proxy::config::load_settings()
            .map(|s| s.sonnet_downgrade_enabled)
            .unwrap_or(false),
        trim_enabled: llm_proxy::config::load_settings()
            .map(|s| s.trim_enabled)
            .unwrap_or(false),
        trim_openai_enabled: llm_proxy::config::load_settings()
            .map(|s| s.trim_openai())
            .unwrap_or(false),
        trim_codex_enabled: llm_proxy::config::load_settings()
            .map(|s| s.trim_codex_enabled)
            .unwrap_or(false),
        trim_tool_desc_chars: llm_proxy::config::load_settings()
            .map(|s| s.trim_tool_desc_chars)
            .unwrap_or(150),
        trim_toolresult_head: llm_proxy::config::load_settings()
            .map(|s| s.trim_toolresult_head)
            .unwrap_or(3000),
        trim_toolresult_tail: llm_proxy::config::load_settings()
            .map(|s| s.trim_toolresult_tail)
            .unwrap_or(1000),
        trim_ws_enabled: llm_proxy::config::load_settings()
            .map(|s| s.trim_ws_enabled)
            .unwrap_or(false),
        trim_strip_thinking: llm_proxy::config::load_settings()
            .map(|s| s.trim_strip_thinking)
            .unwrap_or(false),
        trim_free_target: llm_proxy::config::load_settings()
            .map(|s| s.trim_free_target)
            .unwrap_or_default(),
        trim_head_haiku: llm_proxy::config::load_settings()
            .map(|s| s.trim_head_haiku)
            .unwrap_or(3000),
        trim_head_harness: llm_proxy::config::load_settings()
            .map(|s| s.trim_head_harness)
            .unwrap_or(3000),
        head_items: Vec::new(),
        head_dropdown: SearchDropdownState::default(),
        head_open_group: None,
        head_hover_idx: None,
        vision_model: llm_proxy::config::load_settings()
            .ok()
            .and_then(|s| s.vision_model),
        vision_prompt: llm_proxy::config::load_settings()
            .ok()
            .map(|s| s.vision_prompt)
            .unwrap_or_else(|| llm_proxy::vision::DEFAULT_VISION_PROMPT.to_string()),
        vision_model_items: Vec::new(),
        vision_model_dropdown: SearchDropdownState::default(),
        trim_free_target_items: Vec::new(),
        trim_free_target_dropdown: SearchDropdownState::default(),
        vision_test_status: String::new(),
        vision_test_running: false,
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
        proxy_editing_field: None,

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
    unsafe {
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    // WM_QUIT — the window was already destroyed,
                    // just exit the nested loop.
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // Check if the daemon is shutting down
            if !handle.status() {
                let _ = DestroyWindow(hwnd);
                return;
            }

            // Wait for new messages with a 200ms timeout so we can
            // detect daemon shutdown within a reasonable time.
            let _ = MsgWaitForMultipleObjects(None, false, 200, QS_ALLINPUT);
        }
    }

    // State is freed inside WM_DESTROY (via Box::from_raw).
    // Do NOT free it again here — that would be a double-free.
}

// ═══════════════════════════════════════════════════════════════════════
// Theme list & search items
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn build_theme_list(_default_theme: &NativeTheme) -> Vec<String> {
    let mut names = Vec::new();
    names.push("Code".to_string());

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
        if a == "Code" {
            std::cmp::Ordering::Less
        } else if b == "Code" {
            std::cmp::Ordering::Greater
        } else {
            a.to_lowercase().cmp(&b.to_lowercase())
        }
    });
    names.dedup();
    names
}

pub(crate) fn build_theme_search_items(names: &[String]) -> Vec<SearchDropdownItem> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| SearchDropdownItem::new(i, name.clone(), vec![name.to_lowercase()]))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Top-level paint dispatch
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn paint_settings(hwnd: HWND, state_ptr: *mut SettingsState, layout: &Layout) {
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
            || (ti == 3 && state.active_section == SettingsPage::LlmTrim)
            || (ti == 4 && state.active_section == SettingsPage::Advanced);
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

        // Direct pixel-buffer draws bypass the GDI clip region, so constrain
        // them to the content viewport too (device coords, fixed by scroll).
        let prev_clip = crate::config::editor_theme::set_buffer_clip_y(Some((
            lay.content_y(),
            lay.content_y() + lay.content_visible_h(),
        )));

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
            SettingsPage::LlmTrim => {
                let ctls = build_llm_trim_controls(lay, state);
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

        // Restore viewport origin and the buffer clip band.
        let _ = SetViewportOrgEx(dib_dc, old_org.x, old_org.y, None);
        crate::config::editor_theme::set_buffer_clip_y(prev_clip);
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
        draw_rounded_rect_in_buffer(bits, lay.win_w(), lay.win_h(), track_rect, 0, theme.border);

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
            (4.0 * lay.scale()) as i32,
            theme.accent,
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

    // ── Vision model search dropdown overlay ──────────────────────────
    if state.active_section == SettingsPage::LlmProxy && state.vision_model_dropdown.is_open {
        draw_vision_model_dropdown(dib_dc, bits, lay, state, body_font, small_font);
    }

    // ── Free/cheap trim target search dropdown overlay ────────────
    if state.active_section == SettingsPage::LlmTrim && state.trim_free_target_dropdown.is_open {
        draw_free_target_dropdown(dib_dc, bits, lay, state, body_font, small_font);
    }

    // ── Head budget search dropdown overlay ─────────────────────────
    if state.active_section == SettingsPage::LlmTrim && state.head_dropdown.is_open {
        draw_head_dropdown(dib_dc, bits, lay, state, body_font, small_font);
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
// Contrast helper (inline to keep paint dispatch self-contained)
// ═══════════════════════════════════════════════════════════════════════

fn contrast_text_on(bg: Argb) -> bool {
    let r = bg.r as f32 / 255.0;
    let g = bg.g as f32 / 255.0;
    let b = bg.b as f32 / 255.0;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

// ═══════════════════════════════════════════════════════════════════════

/// Return the estimated total content height for the active page.
/// Used to determine whether a scrollbar is needed and how far to scroll.
pub(crate) fn page_control_content_height(state: &SettingsState, lay: &Layout) -> i32 {
    match state.active_section {
        SettingsPage::General => super::pages::general::content_height(state, lay),
        SettingsPage::Shortcuts => super::pages::shortcuts::content_height(state, lay),
        SettingsPage::Advanced => super::pages::advanced::content_height(state, lay),
        SettingsPage::LlmProxy => super::pages::llm_proxy::content_height(state, lay),
        SettingsPage::LlmTrim => super::pages::llm_trim::content_height(state, lay),
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
// Helpers
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn create_font(h: i32, bold: bool, family: &str) -> HFONT {
    crate::renderer::create_font(h, bold, family)
}

fn monitor_work_rect() -> RECT {
    crate::renderer::primary_monitor_work_rect()
}
