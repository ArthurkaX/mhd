//! Win32 window procedures: native messages converted into editor events.
//!
//! Owns the four top-level procs (`settings_wndproc`, `combo_popup_wndproc`,
//! `param_edit_popup_wndproc`, `param_edit_rich_edit_subclass`) plus the private
//! `loword` helper. The `editor` facade re-exports the procs for the rest of the
//! config editor.

use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Controls::RichEdit::{
    CFE_EFFECTS, CFM_COLOR, CHARFORMATW, EM_SETBKGNDCOLOR, EM_SETCHARFORMAT, SCF_ALL, SCF_DEFAULT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::*;
use crate::config::text_cursor;
use crate::core::action::ActionParamSchema;
use crate::core::native_theme::NativeTheme;
use crate::core::trigger::{KeyCombo, Modifiers, PhysicalKey, keys_to_string};
use crate::hook::WM_BINDING_CAPTURED;
use crate::overlays::keycast::KeycastPosition;

pub(crate) unsafe extern "system" fn settings_wndproc(
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

                // ── Vision model search dropdown hit test ───────────
                if state.vision_model_dropdown.is_open
                    && state.active_section == SettingsPage::LlmProxy
                {
                    let scale = state.layout.scale();
                    let vision_y = state.layout.llm_proxy.vision_y;
                    let section_h = (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;
                    let gap = (8.0 * scale) as i32;
                    let combo_h = (30.0 * scale) as i32;
                    let combo_y = vision_y + section_h + gap;
                    let combo_x = state.layout.pad();
                    let combo_w = state.layout.llm_proxy.vision_combo_w;
                    let dropdown_top = combo_y + combo_h;
                    let item_h = (24.0 * scale) as i32;
                    let search_h = (30.0 * scale) as i32;
                    let visible_rows = 8;
                    let filtered_count = state
                        .vision_model_dropdown
                        .filtered_count(&state.vision_model_items);
                    let max_visible = filtered_count.min(visible_rows);
                    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
                    let dropdown_w = combo_w;

                    let on_combo_button = y >= combo_y
                        && y < combo_y + combo_h
                        && x >= combo_x
                        && x < combo_x + combo_w;

                    let on_dropdown = y >= dropdown_top
                        && y < dropdown_top + dropdown_h
                        && x >= combo_x
                        && x < combo_x + dropdown_w;

                    if on_combo_button {
                        // fall through to VisionModelCombo handler (toggles close)
                    } else if on_dropdown {
                        if y >= dropdown_top + search_h {
                            let item_idx = (y - (dropdown_top + search_h)) / item_h;
                            let visible_items = state
                                .vision_model_dropdown
                                .visible_items(&state.vision_model_items, visible_rows);
                            if (item_idx as usize) < visible_items.len() {
                                let selected = visible_items[item_idx as usize];
                                select_vision_model(state, selected.id);
                                state.vision_model_dropdown.close();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                        }
                        return LRESULT(0);
                    } else {
                        state.vision_model_dropdown.close();
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }
                }

                // ── Free/cheap trim target search dropdown hit test ───
                if state.trim_free_target_dropdown.is_open
                    && state.active_section == SettingsPage::LlmTrim
                {
                    let scale = state.layout.scale();
                    let combo_y = state.layout.llm_trim.free_y;
                    let combo_x = state.layout.pad();
                    let combo_w = state.layout.llm_trim.combo_w;
                    let combo_h = (30.0 * scale) as i32;
                    let dropdown_top = combo_y + combo_h;
                    let item_h = (24.0 * scale) as i32;
                    let search_h = (30.0 * scale) as i32;
                    let visible_rows = 8;
                    let filtered_count = state
                        .trim_free_target_dropdown
                        .filtered_count(&state.trim_free_target_items);
                    let max_visible = filtered_count.min(visible_rows);
                    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
                    let dropdown_w = combo_w;

                    let on_combo_button = y >= combo_y
                        && y < combo_y + combo_h
                        && x >= combo_x
                        && x < combo_x + combo_w;

                    let on_dropdown = y >= dropdown_top
                        && y < dropdown_top + dropdown_h
                        && x >= combo_x
                        && x < combo_x + dropdown_w;

                    if on_combo_button {
                        // fall through to TrimFreeTargetCombo handler (toggles close)
                    } else if on_dropdown {
                        if y >= dropdown_top + search_h {
                            let item_idx = (y - (dropdown_top + search_h)) / item_h;
                            let visible_items = state
                                .trim_free_target_dropdown
                                .visible_items(&state.trim_free_target_items, visible_rows);
                            if (item_idx as usize) < visible_items.len() {
                                let selected = visible_items[item_idx as usize];
                                select_free_target(state, selected.id);
                                state.trim_free_target_dropdown.close();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                        }
                        return LRESULT(0);
                    } else {
                        state.trim_free_target_dropdown.close();
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }
                }

                // ── Head budget search dropdown hit test ─────────────────────
                if state.head_dropdown.is_open && state.active_section == SettingsPage::LlmTrim {
                    let scale = state.layout.scale();
                    let (combo_y, combo_w) = match state.head_open_group {
                        Some(HeadGroup::NativeBig) => {
                            (state.layout.llm_trim.row_a_y, state.layout.llm_trim.combo_w)
                        }
                        Some(HeadGroup::NativeHaiku) => {
                            (state.layout.llm_trim.row_b_y, state.layout.llm_trim.combo_w)
                        }
                        Some(HeadGroup::Harness) => {
                            (state.layout.llm_trim.row_c_y, state.layout.llm_trim.combo_w)
                        }
                        None => (0, 0),
                    };
                    let combo_x = state.layout.win_w() - state.layout.pad() - combo_w;
                    let combo_h = state.layout.llm_trim.row_h;
                    let item_h = (24.0 * scale) as i32;
                    let search_h = (30.0 * scale) as i32;
                    let visible_rows = 8;
                    let filtered_count = state.head_dropdown.filtered_count(&state.head_items);
                    let max_visible = filtered_count.min(visible_rows);
                    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
                    let dropdown_w = combo_w;
                    let footer_y = state.layout.win_h() - state.layout.footer_h();
                    let open_up = combo_y + combo_h + dropdown_h > footer_y;
                    let dropdown_top = if open_up {
                        combo_y - dropdown_h
                    } else {
                        combo_y + combo_h
                    };

                    let on_combo_button = y >= combo_y
                        && y < combo_y + combo_h
                        && x >= combo_x
                        && x < combo_x + combo_w;

                    let on_dropdown = y >= dropdown_top
                        && y < dropdown_top + dropdown_h
                        && x >= combo_x
                        && x < combo_x + dropdown_w;

                    if on_combo_button {
                        // fall through to HeadArrow handler (toggles close)
                    } else if on_dropdown {
                        if y >= dropdown_top + search_h {
                            let item_idx = (y - (dropdown_top + search_h)) / item_h;
                            let visible_items = state
                                .head_dropdown
                                .visible_items(&state.head_items, visible_rows);
                            if (item_idx as usize) < visible_items.len() {
                                let selected = visible_items[item_idx as usize];
                                select_head(state, selected.id);
                                state.head_dropdown.close();
                                state.head_open_group = None;
                                state.head_hover_idx = None;
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                        }
                        return LRESULT(0);
                    } else {
                        state.head_dropdown.close();
                        state.head_open_group = None;
                        state.head_hover_idx = None;
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
                            3 => SettingsPage::LlmTrim,
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
                    SettingsHit::KeycastPositionBtn(idx) => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if let Some(position) = KeycastPosition::all().get(idx).copied() {
                            state.keycast_position = position;
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                    }
                    SettingsHit::KeycastDurationDown => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_duration_ms =
                            state.keycast_duration_ms.saturating_sub(250).max(250);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastDurationUp => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_duration_ms = (state.keycast_duration_ms + 250).min(5000);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastShowTypingToggle => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_show_typing = !state.keycast_show_typing;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastTypingWidthDown => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_typing_width_chars =
                            state.keycast_typing_width_chars.saturating_sub(1).max(4);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastTypingWidthUp => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_typing_width_chars =
                            (state.keycast_typing_width_chars + 1).min(80);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastTypingDurationDown => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_typing_duration_ms = state
                            .keycast_typing_duration_ms
                            .saturating_sub(250)
                            .max(250);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::KeycastTypingDurationUp => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        state.keycast_typing_duration_ms =
                            (state.keycast_typing_duration_ms + 250).min(5000);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    // ── LLM Proxy: global settings ────────────────────
                    SettingsHit::ProxyAnthropicKeyField => {
                        finish_inline_edit(state);
                        state.edit_text = state.anthropic_key.clone();
                        state.edit_cursor = state.anthropic_key.len();
                        state.edit_old_value = state.anthropic_key.clone();
                        state.proxy_editing_field = Some(ProxyEditField::AnthropicKey);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::ProxyBindAddressField => {
                        finish_inline_edit(state);
                        state.edit_text = state.proxy_bind_address.clone();
                        state.edit_cursor = state.proxy_bind_address.len();
                        state.edit_old_value = state.proxy_bind_address.clone();
                        state.proxy_editing_field = Some(ProxyEditField::BindAddress);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::ProxyOpusDowngradeToggle => {
                        state.opus_downgrade_enabled = !state.opus_downgrade_enabled;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::ProxySonnetDowngradeToggle => {
                        state.sonnet_downgrade_enabled = !state.sonnet_downgrade_enabled;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::TrimClaudeCodeToggle => {
                        state.trim_enabled = !state.trim_enabled;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::TrimOpenAiToggle => {
                        state.trim_openai_enabled = !state.trim_openai_enabled;
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::HeadArrowNativeBig
                    | SettingsHit::HeadArrowHaiku
                    | SettingsHit::HeadArrowHarness => {
                        close_kind_popup(state);
                        let group = match hit {
                            SettingsHit::HeadArrowNativeBig => HeadGroup::NativeBig,
                            SettingsHit::HeadArrowHaiku => HeadGroup::NativeHaiku,
                            _ => HeadGroup::Harness,
                        };
                        state.head_open_group = Some(group);
                        // Build items from HEAD_SWEEP. The dropdown draw renders
                        // measured Tune columns per row; the canned description is a
                        // keyword/fallback only.
                        let mut items: Vec<SearchDropdownItem> = Vec::new();
                        for (i, &v) in HEAD_SWEEP.iter().enumerate() {
                            items.push(
                                SearchDropdownItem::new(i, format!("{v}"), vec![format!("{v}")])
                                    .with_description(head_help_text(group, v)),
                            );
                        }
                        state.head_items = items;
                        // Determine selected ID
                        let cur_val = match group {
                            HeadGroup::NativeBig => state.trim_toolresult_head,
                            HeadGroup::NativeHaiku => state.trim_head_haiku,
                            HeadGroup::Harness => state.trim_head_harness,
                        };
                        let selected_id =
                            HEAD_SWEEP.iter().position(|&v| v == cur_val).unwrap_or(4);
                        state.head_dropdown.open(&state.head_items, selected_id, 8);
                        state.head_hover_idx = None;
                        // Close other dropdowns
                        state.combo_open.store(false, Ordering::SeqCst);
                        if let Some(popup) = state.combo_popup.take() {
                            let _ = DestroyWindow(popup);
                        }
                        state.theme_dropdown.close();
                        state.vision_model_dropdown.close();
                        state.trim_free_target_dropdown.close();
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }

                    SettingsHit::HeadCalculateBtn => {
                        editor_head_tune::start();
                        let _ = SetTimer(hwnd, HEAD_TUNE_TIMER_ID, 400, None);
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }

                    // ── Vision model ───────────────────────────────────
                    SettingsHit::VisionModelCombo => {
                        close_kind_popup(state);
                        // Build items: all provider/model pairs
                        let mut items = Vec::new();
                        items.push(SearchDropdownItem::new(
                            0,
                            "Not configured",
                            vec!["none".to_string(), "not configured".to_string()],
                        ));
                        let mut id = 1;
                        for p in &state.providers {
                            for m in &p.models {
                                let display_name = if m.contains('/') {
                                    m.rsplit('/').next().unwrap_or(m).to_string()
                                } else {
                                    m.clone()
                                };
                                let label = format!("{} / {}", p.name, display_name);
                                items.push(SearchDropdownItem::new(
                                    id,
                                    label,
                                    vec![
                                        p.name.to_lowercase(),
                                        display_name.to_lowercase(),
                                        m.to_lowercase(),
                                    ],
                                ));
                                id += 1;
                            }
                        }
                        state.vision_model_items = items;

                        // Determine selected ID
                        let selected_id = state
                            .vision_model
                            .as_ref()
                            .and_then(|vm| {
                                let target_label = format!("{} / {}", vm.provider, vm.model);
                                state
                                    .vision_model_items
                                    .iter()
                                    .position(|item| item.label == target_label)
                            })
                            .unwrap_or(0);

                        state
                            .vision_model_dropdown
                            .open(&state.vision_model_items, selected_id, 8);
                        // Close the other combo if it was open
                        state.combo_open.store(false, Ordering::SeqCst);
                        if let Some(popup) = state.combo_popup.take() {
                            let _ = DestroyWindow(popup);
                        }
                        state.theme_dropdown.close();
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::TrimFreeTargetCombo => {
                        close_kind_popup(state);
                        // Build items: "Off" + all provider model IDs
                        let mut items = Vec::new();
                        items.push(SearchDropdownItem::new(
                            0,
                            "Off",
                            vec!["off".into(), "none".into()],
                        ));
                        let mut id = 1;
                        for p in &state.providers {
                            for m in &p.models {
                                items.push(SearchDropdownItem::new(
                                    id,
                                    m.clone(),
                                    vec![p.name.to_lowercase(), m.to_lowercase()],
                                ));
                                id += 1;
                            }
                        }
                        state.trim_free_target_items = items;

                        // Determine selected ID
                        let selected_id = state
                            .trim_free_target_items
                            .iter()
                            .position(|item| item.label == state.trim_free_target)
                            .unwrap_or(0);

                        state.trim_free_target_dropdown.open(
                            &state.trim_free_target_items,
                            selected_id,
                            8,
                        );
                        // Close the other combos
                        state.combo_open.store(false, Ordering::SeqCst);
                        if let Some(popup) = state.combo_popup.take() {
                            let _ = DestroyWindow(popup);
                        }
                        state.theme_dropdown.close();
                        state.vision_model_dropdown.close();
                        paint_settings(hwnd, state_ptr, &state.layout);
                    }
                    SettingsHit::VisionPromptBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        let theme = state.theme.clone();
                        let prompt = state.vision_prompt.clone();
                        crate::overlays::vision_prompt::show(
                            theme,
                            prompt,
                            Some((crate::app::SendHwnd(hwnd), WM_VISION_PROMPT_UPDATED)),
                        );
                    }
                    SettingsHit::VisionTestBtn => {
                        close_combo_popup(state);
                        close_kind_popup(state);
                        if !state.vision_test_running {
                            state.vision_test_running = true;
                            state.vision_test_status = String::new();
                            paint_settings(hwnd, state_ptr, &state.layout);

                            // Run test on a background thread
                            // Use raw usize to pass values across thread boundary safely
                            let hwnd_val = hwnd.0 as usize;
                            let state_ptr_val = state_ptr as usize;
                            let vision_model = state.vision_model.clone();
                            let providers = state.providers.clone();
                            std::thread::spawn(move || {
                                let result = run_vision_test(&vision_model, &providers);
                                // SAFETY: hwnd_val and state_ptr_val are valid for the
                                // lifetime of the editor window
                                let s = &mut *(state_ptr_val as *mut SettingsState);
                                s.vision_test_running = false;
                                s.vision_test_status = match &result {
                                    Ok(()) => "Passed".to_string(),
                                    Err(e) => format!("Failed: {}", e),
                                };
                                let hwnd_copy = HWND(hwnd_val as *mut std::ffi::c_void);
                                let _ = InvalidateRect(hwnd_copy, None, false);
                            });
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
                                    &state.handle,
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
                        // Head dropdown: track hovered row so the left description inset
                        // updates as the cursor moves. Uses the hit-test geometry exactly.
                        if state.head_dropdown.is_open
                            && state.active_section == SettingsPage::LlmTrim
                        {
                            let hscale = lay.scale();
                            let combo_y = match state.head_open_group {
                                Some(HeadGroup::NativeBig) => lay.llm_trim.row_a_y,
                                Some(HeadGroup::NativeHaiku) => lay.llm_trim.row_b_y,
                                Some(HeadGroup::Harness) => lay.llm_trim.row_c_y,
                                None => 0,
                            };
                            let combo_w = lay.llm_trim.combo_w;
                            let combo_x = lay.win_w() - lay.pad() - combo_w;
                            let combo_h = lay.llm_trim.row_h;
                            let item_h = (24.0 * hscale) as i32;
                            let search_h = (30.0 * hscale) as i32;
                            let vis = state
                                .head_dropdown
                                .visible_items(&state.head_items, 8)
                                .len();
                            let dropdown_h = search_h + (vis as i32) * item_h + 4;
                            let footer_y = lay.win_h() - lay.footer_h();
                            let open_up = combo_y + combo_h + dropdown_h > footer_y;
                            let dropdown_top = if open_up {
                                combo_y - dropdown_h
                            } else {
                                combo_y + combo_h
                            };
                            let list_top = dropdown_top + search_h;
                            let new_idx = if x >= combo_x
                                && x < combo_x + combo_w
                                && y >= list_top
                                && y < list_top + (vis as i32) * item_h
                            {
                                let ci = ((y - list_top) / item_h) as usize;
                                if ci < vis { Some(ci) } else { None }
                            } else {
                                None
                            };
                            if state.head_hover_idx != new_idx {
                                state.head_hover_idx = new_idx;
                                paint_settings(hwnd, state_ptr, &lay);
                            }
                        }
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

                    // Vision model search dropdown scroll
                    if state.vision_model_dropdown.is_open
                        && state.active_section == SettingsPage::LlmProxy
                    {
                        let delta_rows = if delta > 0 { -3 } else { 3 };
                        state.vision_model_dropdown.scroll_by(
                            delta_rows,
                            &state.vision_model_items,
                            8,
                        );
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }

                    // Free/cheap trim target search dropdown scroll
                    if state.trim_free_target_dropdown.is_open
                        && state.active_section == SettingsPage::LlmTrim
                    {
                        let delta_rows = if delta > 0 { -3 } else { 3 };
                        state.trim_free_target_dropdown.scroll_by(
                            delta_rows,
                            &state.trim_free_target_items,
                            8,
                        );
                        paint_settings(hwnd, state_ptr, &state.layout);
                        return LRESULT(0);
                    }

                    // Head budget search dropdown scroll
                    if state.head_dropdown.is_open && state.active_section == SettingsPage::LlmTrim
                    {
                        let delta_rows = if delta > 0 { -3 } else { 3 };
                        state
                            .head_dropdown
                            .scroll_by(delta_rows, &state.head_items, 8);
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
                    if state.edit_idx.is_some() || state.proxy_editing_field.is_some() {
                        let vk = wparam.0 as u32;
                        let ctrl_down = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
                        let shift_down = GetAsyncKeyState(VK_SHIFT.0 as i32) < 0;
                        let is_selected = state.edit_select_start.is_some()
                            && state.edit_select_start.unwrap() != state.edit_cursor;
                        let (sel_start, sel_end) = if let Some(sel) = state.edit_select_start {
                            // Clamp: every consumer below slices or drains with these.
                            let a = text_cursor::clamp(&state.edit_text, sel);
                            let b = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                            (a.min(b), a.max(b))
                        } else {
                            let c = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                            (c, c)
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
                                } else {
                                    let start = text_cursor::prev(&state.edit_text, state.edit_cursor);
                                    let end = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                                    if start < end {
                                        state.edit_text.drain(start..end);
                                    }
                                    state.edit_cursor = start;
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x2E /* VK_DELETE */ => {
                                if is_selected {
                                    state.edit_text.drain(sel_start..sel_end);
                                    state.edit_cursor = sel_start;
                                    state.edit_select_start = None;
                                } else {
                                    let start = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                                    let end = text_cursor::next(&state.edit_text, start);
                                    if start < end {
                                        state.edit_text.drain(start..end);
                                    }
                                    state.edit_cursor = start;
                                }
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x25 /* VK_LEFT */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
                                } else {
                                    state.edit_select_start = None;
                                }
                                state.edit_cursor = text_cursor::prev(&state.edit_text, state.edit_cursor);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x27 /* VK_RIGHT */ => {
                                if shift_down {
                                    if state.edit_select_start.is_none() { state.edit_select_start = Some(state.edit_cursor); }
                                } else {
                                    state.edit_select_start = None;
                                }
                                state.edit_cursor = text_cursor::next(&state.edit_text, state.edit_cursor);
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
                                if let Some(first) = visible.first()
                                    && first.id < state.theme_names.len()
                                {
                                    state.theme_sel = first.id;
                                    apply_settings(state);
                                    close_combo_popup(state);
                                    paint_settings(hwnd, state_ptr, &state.layout);
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

                    // Vision model search dropdown keyboard handling
                    if state.vision_model_dropdown.is_open
                        && state.active_section == SettingsPage::LlmProxy
                    {
                        let vk = wparam.0 as u32;
                        match vk {
                            0x0D => {
                                let visible = state
                                    .vision_model_dropdown
                                    .visible_items(&state.vision_model_items, 8);
                                if let Some(first) = visible.first() {
                                    select_vision_model(state, first.id);
                                    state.vision_model_dropdown.close();
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                            0x1B => {
                                state.vision_model_dropdown.close();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x08 => {
                                state
                                    .vision_model_dropdown
                                    .backspace(&state.vision_model_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x26 => {
                                state.vision_model_dropdown.scroll_by(
                                    -1,
                                    &state.vision_model_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x28 => {
                                state.vision_model_dropdown.scroll_by(
                                    1,
                                    &state.vision_model_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x21 => {
                                state.vision_model_dropdown.scroll_by(
                                    -8,
                                    &state.vision_model_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x22 => {
                                state.vision_model_dropdown.scroll_by(
                                    8,
                                    &state.vision_model_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            _ => {}
                        }
                        return LRESULT(0);
                    }

                    // Free/cheap trim target search dropdown keyboard handling

                    // Head budget search dropdown keyboard handling
                    if state.head_dropdown.is_open && state.active_section == SettingsPage::LlmTrim
                    {
                        let vk = wparam.0 as u32;
                        match vk {
                            0x0D => {
                                let visible =
                                    state.head_dropdown.visible_items(&state.head_items, 8);
                                if let Some(first) = visible.first() {
                                    select_head(state, first.id);
                                    state.head_dropdown.close();
                                    state.head_open_group = None;
                                    state.head_hover_idx = None;
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                            0x1B => {
                                state.head_dropdown.close();
                                state.head_open_group = None;
                                state.head_hover_idx = None;
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x08 => {
                                state.head_dropdown.backspace(&state.head_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x26 => {
                                state.head_dropdown.scroll_by(-1, &state.head_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x28 => {
                                state.head_dropdown.scroll_by(1, &state.head_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x21 => {
                                state.head_dropdown.scroll_by(-8, &state.head_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x22 => {
                                state.head_dropdown.scroll_by(8, &state.head_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            _ => {}
                        }
                        return LRESULT(0);
                    }
                    if state.trim_free_target_dropdown.is_open
                        && state.active_section == SettingsPage::LlmTrim
                    {
                        let vk = wparam.0 as u32;
                        match vk {
                            0x0D => {
                                let visible = state
                                    .trim_free_target_dropdown
                                    .visible_items(&state.trim_free_target_items, 8);
                                if let Some(first) = visible.first() {
                                    select_free_target(state, first.id);
                                    state.trim_free_target_dropdown.close();
                                    paint_settings(hwnd, state_ptr, &state.layout);
                                }
                            }
                            0x1B => {
                                state.trim_free_target_dropdown.close();
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x08 => {
                                state
                                    .trim_free_target_dropdown
                                    .backspace(&state.trim_free_target_items, 8);
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x26 => {
                                state.trim_free_target_dropdown.scroll_by(
                                    -1,
                                    &state.trim_free_target_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x28 => {
                                state.trim_free_target_dropdown.scroll_by(
                                    1,
                                    &state.trim_free_target_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x21 => {
                                state.trim_free_target_dropdown.scroll_by(
                                    -8,
                                    &state.trim_free_target_items,
                                    8,
                                );
                                paint_settings(hwnd, state_ptr, &state.layout);
                            }
                            0x22 => {
                                state.trim_free_target_dropdown.scroll_by(
                                    8,
                                    &state.trim_free_target_items,
                                    8,
                                );
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
                    if state.edit_idx.is_some() || state.proxy_editing_field.is_some() {
                        let ch = (wparam.0 as u32) as u8 as char;
                        if ch.is_ascii_graphic() || ch == ' ' {
                            if let Some(sel) = state.edit_select_start
                                && sel != state.edit_cursor
                            {
                                let (s, e) = {
                                    let a = text_cursor::clamp(&state.edit_text, sel);
                                    let b = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                                    (a.min(b), a.max(b))
                                };
                                state.edit_text.drain(s..e);
                                state.edit_cursor = s;
                                state.edit_select_start = None;
                            }
                            let at = text_cursor::clamp(&state.edit_text, state.edit_cursor);
                            state.edit_text.insert(at, ch);
                            state.edit_cursor = at + ch.len_utf8();
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

                    // Vision model search dropdown character input
                    if state.vision_model_dropdown.is_open
                        && state.active_section == SettingsPage::LlmProxy
                    {
                        let ch = (wparam.0 as u32) as u8 as char;
                        if ch.is_ascii_graphic() || ch == ' ' {
                            state.vision_model_dropdown.input_char(
                                ch,
                                &state.vision_model_items,
                                8,
                            );
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                        return LRESULT(0);
                    }

                    // Free/cheap trim target search dropdown character input
                    if state.trim_free_target_dropdown.is_open
                        && state.active_section == SettingsPage::LlmTrim
                    {
                        let ch = (wparam.0 as u32) as u8 as char;
                        if ch.is_ascii_graphic() || ch == ' ' {
                            state.trim_free_target_dropdown.input_char(
                                ch,
                                &state.trim_free_target_items,
                                8,
                            );
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                        return LRESULT(0);
                    }

                    // Head budget search dropdown character input
                    if state.head_dropdown.is_open && state.active_section == SettingsPage::LlmTrim
                    {
                        let ch = (wparam.0 as u32) as u8 as char;
                        if ch.is_ascii_graphic() || ch == ' ' {
                            state.head_dropdown.input_char(ch, &state.head_items, 8);
                            paint_settings(hwnd, state_ptr, &state.layout);
                        }
                        return LRESULT(0);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            WM_VISION_PROMPT_UPDATED => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &mut *state_ptr;
                    state.vision_prompt = llm_proxy::config::load_settings()
                        .ok()
                        .map(|s| s.vision_prompt)
                        .unwrap_or_else(|| llm_proxy::vision::DEFAULT_VISION_PROMPT.to_string());
                    paint_settings(hwnd, state_ptr, &state.layout);
                }
                LRESULT(0)
            }

            WM_TIMER if wparam.0 == HEAD_TUNE_TIMER_ID => {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    paint_settings(hwnd, state_ptr, &state.layout);
                    if !editor_head_tune::is_running() {
                        let _ = KillTimer(hwnd, HEAD_TUNE_TIMER_ID);
                    }
                }
                LRESULT(0)
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

pub(crate) unsafe extern "system" fn combo_popup_wndproc(
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

pub(crate) unsafe extern "system" fn param_edit_rich_edit_subclass(
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

pub(crate) unsafe extern "system" fn param_edit_popup_wndproc(
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
