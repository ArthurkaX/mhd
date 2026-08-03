//! Dropdown and popup coordination for the Settings panel: open/close
//! helpers plus the search-dropdown overlay painters.

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

use super::*;

// ═══════════════════════════════════════════════════════════════════════
// Theme search dropdown
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn toggle_combo_popup(state: &mut SettingsState) {
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

pub(crate) fn close_combo_popup(state: &mut SettingsState) {
    if let Some(popup) = state.combo_popup.take() {
        unsafe {
            DestroyWindow(popup).ok();
        }
    }
    state.combo_open.store(false, Ordering::SeqCst);
    state.theme_dropdown.close();
    state.vision_model_dropdown.close();
}

pub(crate) fn close_kind_popup(_state: &mut SettingsState) {
    // No-op: native HMENU is self-dismissing.
}

// ═══════════════════════════════════════════════════════════════════════
// Theme search dropdown painting
// ═══════════════════════════════════════════════════════════════════════

/// Draw the theme search dropdown overlay.
pub(crate) fn draw_theme_dropdown(
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

pub(crate) fn draw_vision_model_dropdown(
    dib_dc: HDC,
    bits: *mut c_void,
    lay: &Layout,
    state: &SettingsState,
    body_font: HFONT,
    small_font: HFONT,
) {
    let theme = &state.theme;
    let scale = lay.scale();
    let section_h = (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let gap = (8.0 * scale) as i32;
    let combo_h = (30.0 * scale) as i32;
    let vision_y = lay.llm_proxy.vision_y;
    let combo_y = vision_y + section_h + gap;
    let combo_x = lay.pad();
    let combo_w = lay.llm_proxy.vision_combo_w;
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
        let search_text = if state.vision_model_dropdown.filter.is_empty() {
            "Search models\u{2026}"
        } else {
            state.vision_model_dropdown.filter.as_str()
        };
        let _ = SetTextColor(
            dib_dc,
            if state.vision_model_dropdown.filter.is_empty() {
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
        .vision_model_dropdown
        .visible_items(&state.vision_model_items, visible_rows);
    let list_top = dropdown_top + 2 + search_h;

    for (i, item) in visible_items.iter().enumerate() {
        let item_rect = RECT {
            left: combo_x + 4,
            top: list_top + i as i32 * item_h,
            right: combo_x + dropdown_w - 4,
            bottom: list_top + (i as i32 + 1) * item_h,
        };

        let is_selected = state
            .vision_model
            .as_ref()
            .map(|vm| format!("{} / {}", vm.provider, vm.model) == item.label)
            .unwrap_or(false);
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
            let mut empty_wz = to_utf16_z("No matching models");
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

pub(crate) fn draw_free_target_dropdown(
    dib_dc: HDC,
    bits: *mut c_void,
    lay: &Layout,
    state: &SettingsState,
    body_font: HFONT,
    small_font: HFONT,
) {
    let theme = &state.theme;
    let scale = lay.scale();
    let combo_h = (30.0 * scale) as i32;
    let combo_y = lay.llm_trim.free_y;
    let combo_x = lay.pad();
    let combo_w = lay.llm_trim.combo_w;
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
        let search_text = if state.trim_free_target_dropdown.filter.is_empty() {
            "Search models\u{2026}"
        } else {
            state.trim_free_target_dropdown.filter.as_str()
        };
        let _ = SetTextColor(
            dib_dc,
            if state.trim_free_target_dropdown.filter.is_empty() {
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
        .trim_free_target_dropdown
        .visible_items(&state.trim_free_target_items, visible_rows);
    let list_top = dropdown_top + 2 + search_h;

    for (i, item) in visible_items.iter().enumerate() {
        let item_rect = RECT {
            left: combo_x + 4,
            top: list_top + i as i32 * item_h,
            right: combo_x + dropdown_w - 4,
            bottom: list_top + (i as i32 + 1) * item_h,
        };

        let is_selected = state.trim_free_target == item.label;
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
            let mut empty_wz = to_utf16_z("No matching models");
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

pub(crate) fn draw_head_dropdown(
    dib_dc: HDC,
    bits: *mut c_void,
    lay: &Layout,
    state: &SettingsState,
    body_font: HFONT,
    small_font: HFONT,
) {
    let theme = &state.theme;
    let scale = lay.scale();
    let (combo_y, combo_w) = match state.head_open_group {
        Some(HeadGroup::NativeBig) => (lay.llm_trim.row_a_y, lay.llm_trim.combo_w),
        Some(HeadGroup::NativeHaiku) => (lay.llm_trim.row_b_y, lay.llm_trim.combo_w),
        Some(HeadGroup::Harness) => (lay.llm_trim.row_c_y, lay.llm_trim.combo_w),
        None => (0, 0),
    };
    let combo_x = lay.win_w() - lay.pad() - combo_w;
    let combo_h = lay.llm_trim.row_h;
    let item_h = (24.0 * scale) as i32;
    let search_h = (30.0 * scale) as i32;
    let visible_rows = 8;
    let filtered_count = state.head_dropdown.filtered_count(&state.head_items);
    let max_visible = filtered_count.min(visible_rows);
    let dropdown_h = search_h + (max_visible as i32) * item_h + 4;
    let dropdown_w = combo_w;
    // Flip the dropdown above the row when it would collide with the footer.
    let footer_y = lay.win_h() - lay.footer_h();
    let open_up = combo_y + combo_h + dropdown_h > footer_y;
    let dropdown_top = if open_up {
        combo_y - dropdown_h
    } else {
        combo_y + combo_h
    };

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
        let search_text = if state.head_dropdown.filter.is_empty() {
            "Search values\u{2026}"
        } else {
            state.head_dropdown.filter.as_str()
        };
        let _ = SetTextColor(
            dib_dc,
            if state.head_dropdown.filter.is_empty() {
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
        .head_dropdown
        .visible_items(&state.head_items, visible_rows);
    let list_top = dropdown_top + 2 + search_h;

    let group = state.head_open_group;
    let current = match group {
        Some(HeadGroup::NativeBig) => state.trim_toolresult_head,
        Some(HeadGroup::NativeHaiku) => state.trim_head_haiku,
        Some(HeadGroup::Harness) => state.trim_head_harness,
        None => 0,
    };
    // Column x-offsets inside the row (Tune-style: head / trim% / bar / tags).
    let inner = dropdown_w - 8;
    let head_w = inner * 22 / 100;
    let pct_w = inner * 24 / 100;
    let bar_w = inner * 30 / 100;
    for (i, item) in visible_items.iter().enumerate() {
        let item_rect = RECT {
            left: combo_x + 4,
            top: list_top + i as i32 * item_h,
            right: combo_x + dropdown_w - 4,
            bottom: list_top + (i as i32 + 1) * item_h,
        };

        let is_selected = item.label == format!("{}", current);
        if is_selected {
            draw_rounded_rect_in_buffer(
                bits,
                lay.win_w(),
                lay.win_h(),
                item_rect,
                (2.0 * scale) as i32,
                theme.selected,
            );
        }

        let head: usize = item.label.parse().unwrap_or(0);
        let view = group.and_then(|g| editor_head_tune::head_row_view(g, head, current));
        let col0 = item_rect.left + 4;

        unsafe {
            let _ = SelectObject(dib_dc, small_font);
            match &view {
                Some(v) => {
                    // head value, coloured by risk zone
                    let _ = SetTextColor(dib_dc, v.color.to_colorref());
                    let mut head_wz = to_utf16_z(&item.label);
                    let mut head_rc = RECT {
                        left: col0,
                        top: item_rect.top,
                        right: col0 + head_w,
                        bottom: item_rect.bottom,
                    };
                    let _ = DrawTextW(
                        dib_dc,
                        &mut head_wz,
                        &mut head_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );
                    // trim%
                    let mut pct_wz = to_utf16_z(&v.pct);
                    let mut pct_rc = RECT {
                        left: col0 + head_w,
                        top: item_rect.top,
                        right: col0 + head_w + pct_w,
                        bottom: item_rect.bottom,
                    };
                    let _ = DrawTextW(
                        dib_dc,
                        &mut pct_wz,
                        &mut pct_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );
                    // bar (squares)
                    let mut bar_wz = to_utf16_z(&v.bar);
                    let mut bar_rc = RECT {
                        left: col0 + head_w + pct_w,
                        top: item_rect.top,
                        right: col0 + head_w + pct_w + bar_w,
                        bottom: item_rect.bottom,
                    };
                    let _ = DrawTextW(
                        dib_dc,
                        &mut bar_wz,
                        &mut bar_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );
                    // tags (base / rec), muted
                    let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
                    let mut tag_wz = to_utf16_z(&v.tags);
                    let mut tag_rc = RECT {
                        left: col0 + head_w + pct_w + bar_w,
                        top: item_rect.top,
                        right: item_rect.right - 4,
                        bottom: item_rect.bottom,
                    };
                    let _ = DrawTextW(
                        dib_dc,
                        &mut tag_wz,
                        &mut tag_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );
                }
                None => {
                    // Unmeasured: head value + canned help text.
                    let _ = SetTextColor(dib_dc, theme.text.to_colorref());
                    let mut head_wz = to_utf16_z(&item.label);
                    let mut head_rc = RECT {
                        left: col0,
                        top: item_rect.top,
                        right: col0 + head_w,
                        bottom: item_rect.bottom,
                    };
                    let _ = DrawTextW(
                        dib_dc,
                        &mut head_wz,
                        &mut head_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                    );
                    if let Some(desc) = &item.description {
                        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
                        let mut desc_wz = to_utf16_z(desc);
                        let mut desc_rc = RECT {
                            left: col0 + head_w,
                            top: item_rect.top,
                            right: item_rect.right - 4,
                            bottom: item_rect.bottom,
                        };
                        let _ = DrawTextW(
                            dib_dc,
                            &mut desc_wz,
                            &mut desc_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                        );
                    }
                }
            }
        }
    }

    // No results message
    if visible_items.is_empty() {
        unsafe {
            let _ = SelectObject(dib_dc, small_font);
            let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
            let mut empty_wz = to_utf16_z("No matching values");
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

    // ── Description inset (left of the list) ─────────────────────────────
    // Explains the zone (green / amber / red) of the hovered — or selected —
    // head value, and shows its measured detail. Purely informational: it does
    // not participate in hit-testing, so list-click geometry is unaffected.
    let inset_w = (230.0 * scale) as i32;
    let inset_gap = (8.0 * scale) as i32;
    let inset_right = combo_x - inset_gap;
    let inset_left = (inset_right - inset_w).max(lay.pad());
    let inset_rect = RECT {
        left: inset_left,
        top: dropdown_top,
        right: inset_right,
        bottom: dropdown_top + dropdown_h,
    };
    draw_rounded_rect_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        inset_rect,
        (4.0 * scale) as i32,
        bg,
    );
    draw_rounded_border_in_buffer(
        bits,
        lay.win_w(),
        lay.win_h(),
        inset_rect,
        (4.0 * scale) as i32,
        1,
        theme.border,
    );

    // Which row does the inset describe: hovered row, else the selected value.
    let hover_head: usize = state
        .head_hover_idx
        .and_then(|i| visible_items.get(i))
        .map(|it| &it.label)
        .or_else(|| {
            visible_items
                .iter()
                .find(|it| it.label == format!("{}", current))
                .map(|it| &it.label)
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(current);

    let pad_in = (10.0 * scale) as i32;
    let mut ty = dropdown_top + pad_in;
    let line_h = (18.0 * scale) as i32;
    let zone_color = editor_head_tune::head_zone_color(hover_head);

    unsafe {
        // Title: "head — Zone" with a colour swatch dot.
        let swatch = RECT {
            left: inset_left + pad_in,
            top: ty + (line_h - (10.0 * scale) as i32) / 2,
            right: inset_left + pad_in + (10.0 * scale) as i32,
            bottom: ty + (line_h + (10.0 * scale) as i32) / 2,
        };
        draw_rounded_rect_in_buffer(
            bits,
            lay.win_w(),
            lay.win_h(),
            swatch,
            (2.0 * scale) as i32,
            zone_color,
        );
        let _ = SelectObject(dib_dc, body_font);
        let _ = SetTextColor(dib_dc, zone_color.to_colorref());
        let title = format!(
            "{}  {}",
            hover_head,
            editor_head_tune::head_zone_label(hover_head)
        );
        let mut title_wz = to_utf16_z(&title);
        let mut title_rc = RECT {
            left: inset_left + pad_in + (16.0 * scale) as i32,
            top: ty,
            right: inset_right - pad_in,
            bottom: ty + line_h,
        };
        let _ = DrawTextW(
            dib_dc,
            &mut title_wz,
            &mut title_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        ty += line_h + (2.0 * scale) as i32;

        // Zone note (wraps across the inset width).
        let _ = SelectObject(dib_dc, small_font);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
        let mut note_wz = to_utf16_z(editor_head_tune::head_zone_note(hover_head));
        let mut note_rc = RECT {
            left: inset_left + pad_in,
            top: ty,
            right: inset_right - pad_in,
            bottom: dropdown_top + dropdown_h - pad_in - line_h * 2,
        };
        let _ = DrawTextW(
            dib_dc,
            &mut note_wz,
            &mut note_rc,
            DT_LEFT | DT_WORDBREAK | DT_EDITCONTROL,
        );

        // Measured detail line at the bottom, if Calculate has run.
        let detail = match state
            .head_open_group
            .and_then(|g| editor_head_tune::head_row_view(g, hover_head, current))
        {
            Some(v) => format!("trim {}   {}", v.pct, v.tags),
            None => "Press Calculate for measured trim%.".to_string(),
        };
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
        let mut det_wz = to_utf16_z(&detail);
        let mut det_rc = RECT {
            left: inset_left + pad_in,
            top: dropdown_top + dropdown_h - pad_in - line_h,
            right: inset_right - pad_in,
            bottom: dropdown_top + dropdown_h - pad_in,
        };
        let _ = DrawTextW(
            dib_dc,
            &mut det_wz,
            &mut det_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
}
