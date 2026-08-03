//! GDI painting for the binding editor popup.
//!
//! The popup is a layered window drawn entirely into a `DibFrame` buffer:
//! header, trigger/key slots, action combo, parameter row (slot or text
//! field), description block, footer buttons, and the three dropdown
//! overlays (trigger keys, action kinds, param keys).

use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::editor_binding_popup::layout::{
    POPUP_BIND_BUTTON_WIDTH, POPUP_FIELD_HEIGHT, POPUP_FOOTER_HEIGHT_BASE,
    POPUP_HEADER_HEIGHT_BASE, POPUP_LABEL_WIDTH, POPUP_PADDING, POPUP_RADIUS_BASE,
    POPUP_ROW_HEIGHT, action_dropdown_visible_rows, key_dropdown_visible_rows, trigger_slot_rects,
};
use crate::config::editor_binding_popup::state::{BindingPopupHit, BindingPopupState};
use crate::config::editor_layout::{
    FONT_BODY_SIZE, FONT_SMALL_SIZE, FONT_TITLE_SIZE, editor_action_desc,
};
use crate::config::editor_state::ButtonStyle;
use crate::config::editor_theme::{
    draw_button, draw_plain_label, draw_rounded_border_in_buffer, draw_rounded_rect_in_buffer,
    to_utf16_z,
};
use crate::core::action::ActionParamSchema;
use crate::core::native_theme::Argb;

pub(crate) unsafe fn paint_binding_popup(hwnd: HWND, state_ptr: *mut BindingPopupState) {
    unsafe {
        let state = &*state_ptr;
        let theme = &state.theme;
        let w = state.win_w;
        let h = state.win_h;
        let scale = state.scale;

        let mut frame = match crate::renderer::DibFrame::new(w, h) {
            Some(f) => f,
            None => return,
        };
        let dib_dc = frame.dc();
        let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;

        // Popup background — use a slightly distinct shade from the
        // main window so the popup is visually distinguishable even in
        // themes where background and surface are near-identical.
        let popup_bg = theme.surface.blend_over(theme.background);
        let radius = (POPUP_RADIUS_BASE * scale) as i32;
        crate::osd::draw_rounded_rect(frame.pixels_mut(), w, h, radius, popup_bg);

        let _ = SetBkMode(dib_dc, TRANSPARENT);

        // Fonts
        let title_font = crate::renderer::create_font(
            -(FONT_TITLE_SIZE as f32 * scale) as i32,
            true,
            "Segoe UI Variable",
        );
        let body_font = crate::renderer::create_font(
            -(FONT_BODY_SIZE as f32 * scale) as i32,
            false,
            "Segoe UI Variable",
        );
        let small_font = crate::renderer::create_font(
            -(FONT_SMALL_SIZE as f32 * scale) as i32,
            false,
            "Segoe UI Variable",
        );
        let old_font = SelectObject(dib_dc, title_font);

        // ── Header title ──────────────────────────────────────────
        SetTextColor(dib_dc, theme.text.to_colorref());
        let header_h = (POPUP_HEADER_HEIGHT_BASE as f32 * scale) as i32;
        let pad = (POPUP_PADDING as f32 * scale) as i32;
        let mut title_wz = to_utf16_z("Edit Shortcut");
        let mut title_rc = RECT {
            left: pad,
            top: pad / 2,
            right: w - pad,
            bottom: pad / 2 + 18 + 4,
        };
        DrawTextW(
            dib_dc,
            &mut title_wz,
            &mut title_rc,
            DT_LEFT | DT_SINGLELINE,
        );

        // Separator under header
        let sep_brush = CreateSolidBrush(theme.border.to_colorref());
        FillRect(
            dib_dc,
            &RECT {
                left: pad,
                top: header_h - 1,
                right: w - pad,
                bottom: header_h,
            },
            sep_brush,
        );

        // ── Content layout ─────────────────────────────────────────
        let field_h = (POPUP_FIELD_HEIGHT as f32 * scale) as i32;
        let bind_btn_w = (POPUP_BIND_BUTTON_WIDTH as f32 * scale) as i32;
        let row_h = (POPUP_ROW_HEIGHT as f32 * scale) as i32;
        let label_w = (POPUP_LABEL_WIDTH as f32 * scale) as i32;
        let content_y = header_h + (8.0 * scale) as i32;

        // --- Trigger row ---
        let trigger_label_y = content_y;

        // Label
        SelectObject(dib_dc, body_font);
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: trigger_label_y,
                right: pad + label_w,
                bottom: trigger_label_y + row_h,
            },
            "Trigger",
            body_font,
            theme.text,
        );

        // Trigger combo slots
        let trigger_field_rect = RECT {
            left: pad + label_w + 8,
            top: trigger_label_y,
            right: w - pad - bind_btn_w - 8,
            bottom: trigger_label_y + field_h,
        };
        for (i, (slot, slot_rect)) in trigger_slot_rects(trigger_field_rect, scale)
            .iter()
            .enumerate()
        {
            let is_hovered = state.hovered_target == BindingPopupHit::TriggerSlot(*slot);
            let is_open = state.trigger_editor.open_slot == Some(*slot);
            let slot_bg = if is_hovered || is_open {
                theme
                    .hover
                    .blend_over(theme.surface.blend_over(theme.background))
            } else {
                theme.surface.blend_over(theme.background)
            };
            draw_rounded_rect_in_buffer(bits, w, h, *slot_rect, (4.0 * scale) as i32, slot_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                *slot_rect,
                (4.0 * scale) as i32,
                1,
                if is_open { theme.text } else { theme.border },
            );

            SelectObject(dib_dc, small_font);
            SetTextColor(dib_dc, theme.text.to_colorref());
            let mut label_wz = to_utf16_z(&state.trigger_editor.slot_label(*slot));
            let mut label_rect = RECT {
                left: slot_rect.left + (6.0 * scale) as i32,
                top: slot_rect.top,
                right: slot_rect.right - (6.0 * scale) as i32,
                bottom: slot_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut label_wz,
                &mut label_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            if i < 3 {
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut plus_wz = to_utf16_z("+");
                let next_left = trigger_slot_rects(trigger_field_rect, scale)[i + 1].1.left;
                let mut plus_rect = RECT {
                    left: slot_rect.right,
                    top: slot_rect.top,
                    right: next_left,
                    bottom: slot_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut plus_wz,
                    &mut plus_rect,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }

        // Record button (right of trigger field)
        let record_btn_rect = RECT {
            left: w - pad - bind_btn_w,
            top: trigger_label_y,
            right: w - pad,
            bottom: trigger_label_y + field_h,
        };
        let is_record_hovered = state.hovered_target == BindingPopupHit::RecordTrigger;
        let record_bg = if state.is_recording_trigger {
            Argb::new(255, 200, 50, 50)
        } else if is_record_hovered {
            theme
                .hover
                .blend_over(theme.surface.blend_over(theme.background))
        } else {
            theme.surface.blend_over(theme.background)
        };
        draw_rounded_rect_in_buffer(bits, w, h, record_btn_rect, (4.0 * scale) as i32, record_bg);
        SetTextColor(dib_dc, record_bg.contrasting_text_color().to_colorref());
        SelectObject(dib_dc, small_font);
        let mut rec_wz = to_utf16_z(if state.is_recording_trigger {
            "Listening"
        } else {
            "Bind"
        });
        let mut rec_rc = record_btn_rect;
        DrawTextW(
            dib_dc,
            &mut rec_wz,
            &mut rec_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
        draw_rounded_border_in_buffer(
            bits,
            w,
            h,
            record_btn_rect,
            (4.0 * scale) as i32,
            1,
            theme.border,
        );

        // --- Action row ---
        let action_label_y = trigger_label_y + row_h + (4.0 * scale) as i32;
        SelectObject(dib_dc, body_font);
        draw_plain_label(
            dib_dc,
            RECT {
                left: pad,
                top: action_label_y,
                right: pad + label_w,
                bottom: action_label_y + row_h,
            },
            "Action",
            body_font,
            theme.text,
        );

        // Action kind combo
        let combo_w = (200.0 * scale) as i32;
        let combo_rect = RECT {
            left: pad + label_w + 8,
            top: action_label_y,
            right: pad + label_w + 8 + combo_w,
            bottom: action_label_y + field_h,
        };
        let is_combo_hovered = state.hovered_target == BindingPopupHit::KindCombo;
        let combo_bg = if is_combo_hovered {
            theme
                .hover
                .blend_over(theme.surface.blend_over(theme.background))
        } else {
            theme.surface.blend_over(theme.background)
        };
        draw_rounded_rect_in_buffer(bits, w, h, combo_rect, (4.0 * scale) as i32, combo_bg);
        draw_rounded_border_in_buffer(
            bits,
            w,
            h,
            combo_rect,
            (4.0 * scale) as i32,
            1,
            if is_combo_hovered {
                theme.text
            } else {
                theme.border
            },
        );

        let kind_name = editor_action_desc(state.kind_idx).label;
        SelectObject(dib_dc, body_font);
        SetTextColor(dib_dc, theme.text.to_colorref());
        let mut kind_wz = to_utf16_z(kind_name);
        let mut kind_rc = RECT {
            left: combo_rect.left + (6.0 * scale) as i32,
            top: combo_rect.top,
            right: combo_rect.right - (6.0 * scale) as i32,
            bottom: combo_rect.bottom,
        };
        DrawTextW(
            dib_dc,
            &mut kind_wz,
            &mut kind_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );

        // Dropdown arrow
        SetTextColor(dib_dc, theme.text_muted.to_colorref());
        let mut arrow_wz = to_utf16_z("▼");
        let mut arrow_rc = RECT {
            left: combo_rect.right - field_h,
            top: combo_rect.top,
            right: combo_rect.right,
            bottom: combo_rect.bottom,
        };
        DrawTextW(
            dib_dc,
            &mut arrow_wz,
            &mut arrow_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );

        // --- Parameter row (only for actions with a parameter) ---
        let desc = editor_action_desc(state.kind_idx);
        let param_schema = desc.param_schema;
        let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;

        if desc.param_key.is_some() {
            SelectObject(dib_dc, body_font);
            draw_plain_label(
                dib_dc,
                RECT {
                    left: pad,
                    top: param_label_y,
                    right: pad + label_w,
                    bottom: param_label_y + row_h,
                },
                "Parameter",
                body_font,
                theme.text,
            );

            match param_schema {
                ActionParamSchema::KeyMapping => {
                    // ── KeyMapping: slot-based editor (like Trigger) ──
                    let param_key_field_rect = RECT {
                        left: pad + label_w + 8,
                        top: param_label_y,
                        right: w - pad - bind_btn_w - 8,
                        bottom: param_label_y + field_h,
                    };
                    for (i, (slot, slot_rect)) in trigger_slot_rects(param_key_field_rect, scale)
                        .iter()
                        .enumerate()
                    {
                        let is_hovered = state.hovered_target == BindingPopupHit::ParamSlot(*slot);
                        let is_open = state.param_editor.open_slot == Some(*slot);
                        let slot_bg = if is_hovered || is_open {
                            theme
                                .hover
                                .blend_over(theme.surface.blend_over(theme.background))
                        } else {
                            theme.surface.blend_over(theme.background)
                        };
                        draw_rounded_rect_in_buffer(
                            bits,
                            w,
                            h,
                            *slot_rect,
                            (4.0 * scale) as i32,
                            slot_bg,
                        );
                        draw_rounded_border_in_buffer(
                            bits,
                            w,
                            h,
                            *slot_rect,
                            (4.0 * scale) as i32,
                            1,
                            if is_open { theme.text } else { theme.border },
                        );

                        SelectObject(dib_dc, small_font);
                        SetTextColor(dib_dc, theme.text.to_colorref());
                        let mut label_wz = to_utf16_z(&state.param_editor.slot_label(*slot));
                        let mut label_rect = RECT {
                            left: slot_rect.left + (6.0 * scale) as i32,
                            top: slot_rect.top,
                            right: slot_rect.right - (6.0 * scale) as i32,
                            bottom: slot_rect.bottom,
                        };
                        DrawTextW(
                            dib_dc,
                            &mut label_wz,
                            &mut label_rect,
                            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );

                        if i < 3 {
                            SetTextColor(dib_dc, theme.text_muted.to_colorref());
                            let mut plus_wz = to_utf16_z("+");
                            let next_left = trigger_slot_rects(param_key_field_rect, scale)[i + 1]
                                .1
                                .left;
                            let mut plus_rect = RECT {
                                left: slot_rect.right,
                                top: slot_rect.top,
                                right: next_left,
                                bottom: slot_rect.bottom,
                            };
                            DrawTextW(
                                dib_dc,
                                &mut plus_wz,
                                &mut plus_rect,
                                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                            );
                        }
                    }

                    // Bind button (right of param key field)
                    let param_bind_rect = RECT {
                        left: w - pad - bind_btn_w,
                        top: param_label_y,
                        right: w - pad,
                        bottom: param_label_y + field_h,
                    };
                    let is_param_bind_hovered =
                        state.hovered_target == BindingPopupHit::RecordParam;
                    let param_bind_bg = if state.is_recording_param {
                        Argb::new(255, 200, 50, 50)
                    } else if is_param_bind_hovered {
                        theme
                            .hover
                            .blend_over(theme.surface.blend_over(theme.background))
                    } else {
                        theme.surface.blend_over(theme.background)
                    };
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        param_bind_rect,
                        (4.0 * scale) as i32,
                        param_bind_bg,
                    );
                    SetTextColor(dib_dc, param_bind_bg.contrasting_text_color().to_colorref());
                    SelectObject(dib_dc, small_font);
                    let mut bind_wz = to_utf16_z(if state.is_recording_param {
                        "Listening"
                    } else {
                        "Bind"
                    });
                    let mut bind_rc = param_bind_rect;
                    DrawTextW(
                        dib_dc,
                        &mut bind_wz,
                        &mut bind_rc,
                        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                    );
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        param_bind_rect,
                        (4.0 * scale) as i32,
                        1,
                        theme.border,
                    );
                }

                _ => {
                    // ── Non-KeyMapping: text-field editor ──
                    let has_side_btn = param_schema == ActionParamSchema::FilePath;
                    let param_field_rect = RECT {
                        left: pad + label_w + 8,
                        top: param_label_y,
                        right: if has_side_btn {
                            w - pad - bind_btn_w - 8
                        } else {
                            w - pad
                        },
                        bottom: param_label_y + field_h,
                    };
                    let is_editing = state.is_editing_param;
                    let is_param_hovered = state.hovered_target == BindingPopupHit::ParamField
                        || state.hovered_target == BindingPopupHit::BrowseParam;
                    let param_bg = if is_param_hovered {
                        theme
                            .hover
                            .blend_over(theme.surface.blend_over(theme.background))
                    } else {
                        theme.surface.blend_over(theme.background)
                    };
                    let param_border = if is_editing {
                        theme.accent
                    } else {
                        theme.border
                    };
                    let radius = (4.0 * scale) as i32;
                    draw_rounded_rect_in_buffer(bits, w, h, param_field_rect, radius, param_bg);
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        param_field_rect,
                        radius,
                        1,
                        param_border,
                    );

                    let param_text = if state.param.is_empty() && !is_editing {
                        match param_schema {
                            ActionParamSchema::FilePath => "(click Browse to select)",
                            _ => "",
                        }
                    } else {
                        &state.param
                    };
                    SelectObject(dib_dc, body_font);
                    SetTextColor(dib_dc, theme.text.to_colorref());
                    let inset = (6.0 * scale) as i32;
                    let mut text_rc = RECT {
                        left: param_field_rect.left + inset,
                        top: param_field_rect.top,
                        right: param_field_rect.right - inset,
                        bottom: param_field_rect.bottom,
                    };

                    if is_editing {
                        // Draw text suitable for editing (no ellipsis so the
                        // cursor can reach the far end)
                        let mut wz = to_utf16_z(param_text);
                        let _ = DrawTextW(
                            dib_dc,
                            &mut wz,
                            &mut text_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                        );

                        // Draw cursor vertical bar at the cursor position
                        let cursor_x = cursor_position_x(
                            dib_dc,
                            &state.param,
                            state.param_edit_cursor,
                            param_field_rect.left + inset,
                        );
                        if cursor_x < param_field_rect.right - 2 {
                            let cursor_h = (20.0 * scale) as i32;
                            let cy = param_field_rect.top
                                + (param_field_rect.bottom - param_field_rect.top - cursor_h) / 2;
                            let cursor_rc = RECT {
                                left: cursor_x,
                                top: cy,
                                right: cursor_x + ((1.0 * scale) as i32).max(1),
                                bottom: cy + cursor_h,
                            };
                            let cursor_brush = CreateSolidBrush(theme.text.to_colorref());
                            FillRect(dib_dc, &cursor_rc, cursor_brush);
                            let _ = DeleteObject(cursor_brush);
                        }
                    } else {
                        let mut wz = to_utf16_z(param_text);
                        let _ = DrawTextW(
                            dib_dc,
                            &mut wz,
                            &mut text_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                        );
                    }

                    // Browse button for FilePath
                    if has_side_btn {
                        let btn_rect = RECT {
                            left: w - pad - bind_btn_w,
                            top: param_label_y,
                            right: w - pad,
                            bottom: param_label_y + field_h,
                        };
                        let is_browse_hovered =
                            state.hovered_target == BindingPopupHit::BrowseParam;
                        let btn_bg = if is_browse_hovered {
                            theme
                                .hover
                                .blend_over(theme.surface.blend_over(theme.background))
                        } else {
                            theme.surface.blend_over(theme.background)
                        };
                        draw_rounded_rect_in_buffer(
                            bits,
                            w,
                            h,
                            btn_rect,
                            (4.0 * scale) as i32,
                            btn_bg,
                        );
                        SetTextColor(dib_dc, btn_bg.contrasting_text_color().to_colorref());
                        SelectObject(dib_dc, small_font);
                        let mut btn_wz = to_utf16_z("Browse\u{2026}");
                        let mut btn_rc = btn_rect;
                        DrawTextW(
                            dib_dc,
                            &mut btn_wz,
                            &mut btn_rc,
                            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                        );
                        draw_rounded_border_in_buffer(
                            bits,
                            w,
                            h,
                            btn_rect,
                            (4.0 * scale) as i32,
                            1,
                            theme.border,
                        );
                    }
                }
            }
        }

        // ── Description / info block ─────────────────────────────────
        if desc.param_key.is_some() || !desc.description.is_empty() {
            let desc_row_y = if desc.param_key.is_some() {
                param_label_y + row_h
            } else {
                action_label_y + row_h
            };
            let desc_y = desc_row_y + (12.0 * scale) as i32;
            let desc_h = (60.0 * scale) as i32;
            let desc_rect = RECT {
                left: pad,
                top: desc_y,
                right: w - pad,
                bottom: desc_y + desc_h,
            };

            // Background
            let desc_bg = theme.surface.blend_over(theme.background).with_alpha(80);
            draw_rounded_rect_in_buffer(bits, w, h, desc_rect, (6.0 * scale) as i32, desc_bg);

            SelectObject(dib_dc, small_font);
            let inset = (8.0 * scale) as i32;

            // Description line
            SetTextColor(dib_dc, theme.text_muted.to_colorref());
            let mut text_wz = to_utf16_z(desc.description);
            let mut text_rc = RECT {
                left: desc_rect.left + inset,
                top: desc_rect.top + (4.0 * scale) as i32,
                right: desc_rect.right - inset,
                bottom: desc_rect.top + (22.0 * scale) as i32,
            };
            let _ = DrawTextW(
                dib_dc,
                &mut text_wz,
                &mut text_rc,
                DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
            );

            // Schema-specific hint / error line
            let (hint_color, hint_text) = if let Some(ref err) = state.param_save_error {
                (Argb::new(255, 220, 60, 60), err.clone())
            } else {
                let text = match param_schema {
                    ActionParamSchema::Number { unit, min, max } => {
                        format!("Range: {}{} to {}{}", min, unit, max, unit)
                    }
                    ActionParamSchema::FilePath => {
                        "Click Browse to select a file or executable.".to_string()
                    }
                    ActionParamSchema::Text => {
                        if desc.name == "run_ps" {
                            "Example: Get-Process | Where-Object { $_.CPU -gt 10 }".to_string()
                        } else if desc.name == "switch_power_plan" {
                            "Enter the GUID or name of the power plan.".to_string()
                        } else {
                            String::new()
                        }
                    }
                    ActionParamSchema::KeyMapping => {
                        "Click Bind to record a key combination.".to_string()
                    }
                    ActionParamSchema::None | ActionParamSchema::PowerAction => String::new(),
                };
                (theme.text.with_alpha(180), text)
            };
            if !hint_text.is_empty() {
                SetTextColor(dib_dc, hint_color.to_colorref());
                let mut hint_wz = to_utf16_z(&hint_text);
                let mut hint_rc = RECT {
                    left: desc_rect.left + inset,
                    top: text_rc.bottom + (2.0 * scale) as i32,
                    right: desc_rect.right - inset,
                    bottom: desc_rect.bottom - (4.0 * scale) as i32,
                };
                let _ = DrawTextW(
                    dib_dc,
                    &mut hint_wz,
                    &mut hint_rc,
                    DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
                );
            }
        }

        // ── Footer separator ──────────────────────────────────────

        let footer_h = (POPUP_FOOTER_HEIGHT_BASE as f32 * scale) as i32;
        let footer_y = h - footer_h;
        FillRect(
            dib_dc,
            &RECT {
                left: pad,
                top: footer_y,
                right: w - pad,
                bottom: footer_y + 1,
            },
            sep_brush,
        );

        // ── Buttons (right to left: Cancel, Save) ─────────────────
        let btn_w = (90.0 * scale) as i32;
        let btn_hh = (30.0 * scale) as i32;
        let btn_y = footer_y + (footer_h - btn_hh) / 2;
        let save_x = w - pad - btn_w;
        let cancel_x = save_x - btn_w - (8.0 * scale) as i32;

        let is_cancel_hovered = state.hovered_target == BindingPopupHit::CancelBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            cancel_x,
            btn_y,
            btn_w,
            btn_hh,
            "Cancel",
            theme,
            body_font,
            is_cancel_hovered,
            ButtonStyle::Secondary,
        );

        let is_save_hovered = state.hovered_target == BindingPopupHit::SaveBtn;
        draw_button(
            dib_dc,
            bits,
            w,
            h,
            save_x,
            btn_y,
            btn_w,
            btn_hh,
            "Save",
            theme,
            body_font,
            is_save_hovered,
            ButtonStyle::Primary,
        );

        // ── Trigger key dropdown, drawn above rows underneath ─────────────
        if state.trigger_editor.dropdown.is_open
            && let Some(slot) = state.trigger_editor.open_slot
        {
            let slot_rect = trigger_slot_rects(trigger_field_rect, scale)
                .into_iter()
                .find(|(candidate, _)| *candidate == slot)
                .map(|(_, rect)| rect)
                .unwrap_or(trigger_field_rect);
            let key_items = state.trigger_editor.items_for_open_slot();
            let item_h = (24.0 * scale) as i32;
            let search_h = (30.0 * scale) as i32;
            let visible_rows = key_dropdown_visible_rows();
            let filtered_count = state.trigger_editor.dropdown.filtered_count(&key_items);
            let visible_items = state
                .trigger_editor
                .dropdown
                .visible_items(&key_items, visible_rows);
            let list_rows = visible_items.len().max(1);
            let popup_rect = RECT {
                left: slot_rect.left,
                top: slot_rect.bottom,
                right: (slot_rect.left + (220.0 * scale) as i32).min(w - pad),
                bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
            };
            let popup_bg = theme.surface.blend_over(theme.background);
            draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                popup_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            let search_rect = RECT {
                left: popup_rect.left + (6.0 * scale) as i32,
                top: popup_rect.top + (5.0 * scale) as i32,
                right: popup_rect.right - (6.0 * scale) as i32,
                bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                theme.background.blend_over(popup_bg),
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            SelectObject(dib_dc, small_font);
            let search_text = if state.trigger_editor.dropdown.filter.is_empty() {
                "Search keys..."
            } else {
                state.trigger_editor.dropdown.filter.as_str()
            };
            SetTextColor(
                dib_dc,
                if state.trigger_editor.dropdown.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }
                .to_colorref(),
            );
            let mut search_wz = to_utf16_z(search_text);
            let mut search_text_rect = RECT {
                left: search_rect.left + (7.0 * scale) as i32,
                top: search_rect.top,
                right: search_rect.right - (7.0 * scale) as i32,
                bottom: search_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut search_wz,
                &mut search_text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            let list_top = popup_rect.top + search_h;
            for (i, item) in visible_items.iter().enumerate() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top + i as i32 * item_h,
                    right: popup_rect.right,
                    bottom: list_top + (i as i32 + 1) * item_h,
                };
                let is_hovered = state.hovered_target == BindingPopupHit::TriggerKeyItem(item.id);
                if is_hovered {
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        item_rect,
                        0,
                        theme.hover.blend_over(popup_bg),
                    );
                }

                SetTextColor(dib_dc, theme.text.to_colorref());
                let mut item_wz = to_utf16_z(&item.label);
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if visible_items.is_empty() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top,
                    right: popup_rect.right,
                    bottom: list_top + item_h,
                };
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut item_wz = to_utf16_z("No keys found");
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if filtered_count > visible_rows {
                let scroll_w = (4.0 * scale).max(3.0) as i32;
                let track_rect = RECT {
                    left: popup_rect.right - scroll_w - 2,
                    top: list_top + 2,
                    right: popup_rect.right - 2,
                    bottom: popup_rect.bottom - 2,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    track_rect,
                    scroll_w / 2,
                    theme.border.with_alpha(60),
                );
                let travel = (track_rect.bottom - track_rect.top).max(1);
                let thumb_h =
                    ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                let thumb_h = thumb_h.max((16.0 * scale) as i32);
                let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                let thumb_y = track_rect.top
                    + ((state.trigger_editor.dropdown.scroll as f32 / max_scroll as f32)
                        * (travel - thumb_h).max(0) as f32) as i32;
                let thumb_rect = RECT {
                    left: track_rect.left,
                    top: thumb_y,
                    right: track_rect.right,
                    bottom: thumb_y + thumb_h,
                };
                draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
            }
        }

        // ── Action dropdown, drawn last so it sits above fields/footer ───
        if state.action_dropdown.is_open {
            let item_h = (24.0 * scale) as i32;
            let search_h = (30.0 * scale) as i32;
            let visible_rows = action_dropdown_visible_rows();
            let filtered_count = state.action_dropdown.filtered_count(&state.action_items);
            let visible_actions = state
                .action_dropdown
                .visible_items(&state.action_items, visible_rows);
            let list_rows = visible_actions.len().max(1);
            let popup_rect = RECT {
                left: combo_rect.left,
                top: combo_rect.bottom,
                right: (combo_rect.left + (260.0 * scale) as i32).min(w - pad),
                bottom: combo_rect.bottom + search_h + list_rows as i32 * item_h,
            };
            let popup_bg = theme.surface.blend_over(theme.background);
            draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                popup_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            let search_rect = RECT {
                left: popup_rect.left + (6.0 * scale) as i32,
                top: popup_rect.top + (5.0 * scale) as i32,
                right: popup_rect.right - (6.0 * scale) as i32,
                bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                theme.background.blend_over(popup_bg),
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            SelectObject(dib_dc, small_font);
            let search_text = if state.action_dropdown.filter.is_empty() {
                "Search actions..."
            } else {
                state.action_dropdown.filter.as_str()
            };
            SetTextColor(
                dib_dc,
                if state.action_dropdown.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }
                .to_colorref(),
            );
            let mut search_wz = to_utf16_z(search_text);
            let mut search_text_rect = RECT {
                left: search_rect.left + (7.0 * scale) as i32,
                top: search_rect.top,
                right: search_rect.right - (7.0 * scale) as i32,
                bottom: search_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut search_wz,
                &mut search_text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            let list_top = popup_rect.top + search_h;
            for (i, action) in visible_actions.iter().enumerate() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top + i as i32 * item_h,
                    right: popup_rect.right,
                    bottom: list_top + (i as i32 + 1) * item_h,
                };
                let is_hovered = state.hovered_target == BindingPopupHit::KindItem(action.id);
                let is_selected = state.kind_idx == action.id;
                if is_hovered || is_selected {
                    let bg = if is_hovered {
                        theme.hover.blend_over(popup_bg)
                    } else {
                        theme.selected.blend_over(popup_bg)
                    };
                    draw_rounded_rect_in_buffer(bits, w, h, item_rect, 0, bg);
                }

                SetTextColor(dib_dc, theme.text.to_colorref());
                let mut item_wz = to_utf16_z(&action.label);
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if visible_actions.is_empty() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top,
                    right: popup_rect.right,
                    bottom: list_top + item_h,
                };
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut item_wz = to_utf16_z("No actions found");
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if filtered_count > visible_rows {
                let scroll_w = (4.0 * scale).max(3.0) as i32;
                let track_rect = RECT {
                    left: popup_rect.right - scroll_w - 2,
                    top: list_top + 2,
                    right: popup_rect.right - 2,
                    bottom: popup_rect.bottom - 2,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    track_rect,
                    scroll_w / 2,
                    theme.border.with_alpha(60),
                );
                let travel = (track_rect.bottom - track_rect.top).max(1);
                let thumb_h =
                    ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                let thumb_h = thumb_h.max((16.0 * scale) as i32);
                let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                let thumb_y = track_rect.top
                    + ((state.action_dropdown.scroll as f32 / max_scroll as f32)
                        * (travel - thumb_h).max(0) as f32) as i32;
                let thumb_rect = RECT {
                    left: track_rect.left,
                    top: thumb_y,
                    right: track_rect.right,
                    bottom: thumb_y + thumb_h,
                };
                draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
            }

            let described_id = match state.hovered_target {
                BindingPopupHit::KindItem(id) => id,
                _ => state.kind_idx,
            };
            if let Some(action) = state
                .action_items
                .iter()
                .find(|item| item.id == described_id)
                .filter(|item| {
                    item.description
                        .as_deref()
                        .is_some_and(|text| !text.is_empty())
                })
            {
                let gap = (8.0 * scale) as i32;
                let desc_rect = RECT {
                    left: popup_rect.right + gap,
                    top: popup_rect.top,
                    right: (popup_rect.right + gap + (220.0 * scale) as i32).min(w - pad),
                    bottom: popup_rect.bottom,
                };
                if desc_rect.right > desc_rect.left + (80.0 * scale) as i32 {
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        desc_rect,
                        (4.0 * scale) as i32,
                        popup_bg,
                    );
                    draw_rounded_border_in_buffer(
                        bits,
                        w,
                        h,
                        desc_rect,
                        (4.0 * scale) as i32,
                        1,
                        theme.border,
                    );

                    let inset = (10.0 * scale) as i32;
                    SelectObject(dib_dc, small_font);
                    SetTextColor(dib_dc, theme.text.to_colorref());
                    let mut title_wz = to_utf16_z(&action.label);
                    let mut title_rect = RECT {
                        left: desc_rect.left + inset,
                        top: desc_rect.top + inset,
                        right: desc_rect.right - inset,
                        bottom: desc_rect.top + inset + (22.0 * scale) as i32,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut title_wz,
                        &mut title_rect,
                        DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );

                    SetTextColor(dib_dc, theme.text_muted.to_colorref());
                    let mut desc_wz = to_utf16_z(action.description.as_deref().unwrap_or(""));
                    let mut text_rect = RECT {
                        left: desc_rect.left + inset,
                        top: title_rect.bottom + (4.0 * scale) as i32,
                        right: desc_rect.right - inset,
                        bottom: desc_rect.bottom - inset,
                    };
                    DrawTextW(
                        dib_dc,
                        &mut desc_wz,
                        &mut text_rect,
                        DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
                    );
                }
            }
        }

        // ── Param editor key dropdown (for KeyMapping schema) ───────────
        if desc.param_key.is_some()
            && param_schema == ActionParamSchema::KeyMapping
            && state.param_editor.dropdown.is_open
            && let Some(slot) = state.param_editor.open_slot
        {
            let param_label_y = action_label_y + row_h + (4.0 * scale) as i32;
            let param_key_field_rect = RECT {
                left: pad + label_w + 8,
                top: param_label_y,
                right: w - pad - bind_btn_w - 8,
                bottom: param_label_y + field_h,
            };
            let slot_rect = {
                let rects = trigger_slot_rects(param_key_field_rect, scale);
                rects
                    .into_iter()
                    .find(|(candidate, _)| *candidate == slot)
                    .map(|(_, rect)| rect)
                    .unwrap_or(param_key_field_rect)
            };
            let key_items = state.param_editor.items_for_open_slot();
            let item_h = (24.0 * scale) as i32;
            let search_h = (30.0 * scale) as i32;
            let visible_rows = key_dropdown_visible_rows();
            let filtered_count = state.param_editor.dropdown.filtered_count(&key_items);
            let visible_items = state
                .param_editor
                .dropdown
                .visible_items(&key_items, visible_rows);
            let list_rows = visible_items.len().max(1);
            let popup_rect = RECT {
                left: slot_rect.left,
                top: slot_rect.bottom,
                right: (slot_rect.left + (220.0 * scale) as i32).min(w - pad),
                bottom: slot_rect.bottom + search_h + list_rows as i32 * item_h,
            };
            let popup_bg = theme.surface.blend_over(theme.background);
            draw_rounded_rect_in_buffer(bits, w, h, popup_rect, (4.0 * scale) as i32, popup_bg);
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                popup_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            let search_rect = RECT {
                left: popup_rect.left + (6.0 * scale) as i32,
                top: popup_rect.top + (5.0 * scale) as i32,
                right: popup_rect.right - (6.0 * scale) as i32,
                bottom: popup_rect.top + search_h - (5.0 * scale) as i32,
            };
            draw_rounded_rect_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                theme.background.blend_over(popup_bg),
            );
            draw_rounded_border_in_buffer(
                bits,
                w,
                h,
                search_rect,
                (4.0 * scale) as i32,
                1,
                theme.border,
            );

            SelectObject(dib_dc, small_font);
            let search_text = if state.param_editor.dropdown.filter.is_empty() {
                "Search keys..."
            } else {
                state.param_editor.dropdown.filter.as_str()
            };
            SetTextColor(
                dib_dc,
                if state.param_editor.dropdown.filter.is_empty() {
                    theme.text_muted
                } else {
                    theme.text
                }
                .to_colorref(),
            );
            let mut search_wz = to_utf16_z(search_text);
            let mut search_text_rect = RECT {
                left: search_rect.left + (7.0 * scale) as i32,
                top: search_rect.top,
                right: search_rect.right - (7.0 * scale) as i32,
                bottom: search_rect.bottom,
            };
            DrawTextW(
                dib_dc,
                &mut search_wz,
                &mut search_text_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );

            let list_top = popup_rect.top + search_h;
            for (i, item) in visible_items.iter().enumerate() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top + i as i32 * item_h,
                    right: popup_rect.right,
                    bottom: list_top + (i as i32 + 1) * item_h,
                };
                let is_hovered = state.hovered_target == BindingPopupHit::ParamKeyItem(item.id);
                if is_hovered {
                    draw_rounded_rect_in_buffer(
                        bits,
                        w,
                        h,
                        item_rect,
                        0,
                        theme.hover.blend_over(popup_bg),
                    );
                }

                SetTextColor(dib_dc, theme.text.to_colorref());
                let mut item_wz = to_utf16_z(&item.label);
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if visible_items.is_empty() {
                let item_rect = RECT {
                    left: popup_rect.left,
                    top: list_top,
                    right: popup_rect.right,
                    bottom: list_top + item_h,
                };
                SetTextColor(dib_dc, theme.text_muted.to_colorref());
                let mut item_wz = to_utf16_z("No keys found");
                let mut item_text_rect = RECT {
                    left: item_rect.left + (8.0 * scale) as i32,
                    top: item_rect.top,
                    right: item_rect.right - (8.0 * scale) as i32,
                    bottom: item_rect.bottom,
                };
                DrawTextW(
                    dib_dc,
                    &mut item_wz,
                    &mut item_text_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            if filtered_count > visible_rows {
                let scroll_w = (4.0 * scale).max(3.0) as i32;
                let track_rect = RECT {
                    left: popup_rect.right - scroll_w - 2,
                    top: list_top + 2,
                    right: popup_rect.right - 2,
                    bottom: popup_rect.bottom - 2,
                };
                draw_rounded_rect_in_buffer(
                    bits,
                    w,
                    h,
                    track_rect,
                    scroll_w / 2,
                    theme.border.with_alpha(60),
                );
                let travel = (track_rect.bottom - track_rect.top).max(1);
                let thumb_h =
                    ((visible_rows as f32 / filtered_count as f32) * travel as f32) as i32;
                let thumb_h = thumb_h.max((16.0 * scale) as i32);
                let max_scroll = filtered_count.saturating_sub(visible_rows).max(1);
                let thumb_y = track_rect.top
                    + ((state.param_editor.dropdown.scroll as f32 / max_scroll as f32)
                        * (travel - thumb_h).max(0) as f32) as i32;
                let thumb_rect = RECT {
                    left: track_rect.left,
                    top: thumb_y,
                    right: track_rect.right,
                    bottom: thumb_y + thumb_h,
                };
                draw_rounded_rect_in_buffer(bits, w, h, thumb_rect, scroll_w / 2, theme.hover);
            }
        }

        // ── Cleanup ────────────────────────────────────────────────
        let _ = DeleteObject(sep_brush);
        SelectObject(dib_dc, old_font);
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(body_font);
        let _ = DeleteObject(small_font);

        frame.fix_gdi_alpha(popup_bg);

        let cur_pos = {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            (wr.left, wr.top)
        };
        frame.present_layered(hwnd, cur_pos.0, cur_pos.1, 255);
    }
}

/// Compute the X coordinate of the cursor (insertion point) within the param
/// edit field, using the current font to measure text before the cursor.
fn cursor_position_x(dib_dc: HDC, text: &str, cursor: usize, field_left: i32) -> i32 {
    if cursor == 0 || text.is_empty() || cursor > text.len() {
        return field_left;
    }
    // cursor is a byte index; find the nearest char boundary just in case
    let bound = text.floor_char_boundary(cursor);
    let prefix = &text[..bound];
    let wz: Vec<u16> = prefix.encode_utf16().collect();
    unsafe {
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dib_dc, &wz, &mut sz);
        field_left + sz.cx
    }
}
