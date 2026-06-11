//! Page paint dispatch for the settings editor.
//!
//! Each page has a `build_*_controls` function that returns a [`ControlList`]
//! without drawing.  A single [`paint_control`] function renders one control
//! by dispatching to the appropriate `draw_*` helper.  [`paint_page`]
//! iterates the list, calls `paint_control` for each entry, and returns
//! the derived hit regions.

use std::ffi::c_void;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;

use crate::action::ActionParamSchema;
use crate::config::editor_control::{Control, ControlList, FontChoice, HitRegion};
use crate::config::editor_layout::{
    editor_action_desc, ADVANCED_BUTTONS, COMBO_HIT_HEIGHT, SECTION_GAP_BASE,
    SECTION_HEADER_HEIGHT_BASE, Layout, WIN_WIDTH_BASE,
};
use crate::config::editor_state::{ButtonStyle, HoverTarget, SettingsHit, SettingsState};
use crate::config::editor_theme::{
    draw_button, draw_collapsed_combo_box, draw_plain_label, draw_readonly_text_field,
    draw_rounded_border_in_buffer, draw_rounded_rect_in_buffer, draw_section_header,
    draw_toggle_switch, to_utf16_z,
};
use crate::core::native_theme::{Argb, NativeTheme};

// ── Convenience: resolve a FontChoice to an HFONT ───────────────────

fn select_font(dib_dc: HDC, font: FontChoice, title_font: HFONT, body_font: HFONT, small_font: HFONT) -> HFONT {
    let h = match font {
        FontChoice::Title => title_font,
        FontChoice::Body => body_font,
        FontChoice::Small => small_font,
    };
    unsafe { let _ = SelectObject(dib_dc, h); }
    h
}

// ── Single‑control paint dispatch ───────────────────────────────────

/// Render a single [`Control`] into the DIB.
///
/// The caller is responsible for setting/restoring clip regions around
/// the page (clipping is page-level, not per-control).
pub fn paint_control(
    c: &Control,
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    theme: &NativeTheme,
    title_font: HFONT,
    body_font: HFONT,
    small_font: HFONT,
) {
    match c {
        Control::Header { rect, label, font, text_color } => {
            draw_section_header(dib_dc, *rect, label, select_font(dib_dc, *font, title_font, body_font, small_font), *text_color);
        }
        Control::Label { rect, label, font, text_color } => {
            draw_plain_label(dib_dc, *rect, label, select_font(dib_dc, *font, title_font, body_font, small_font), *text_color);
        }
        Control::Divider { rect } => {
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *rect, 0, theme.border);
        }
        Control::Combo { rect, arrow_width, selected, font, bg_color, border_color, text_color, arrow_color, .. } => {
            draw_collapsed_combo_box(
                dib_dc, bits, win_w, win_h, *rect, *arrow_width, selected,
                select_font(dib_dc, *font, title_font, body_font, small_font),
                *bg_color, *border_color, *text_color, *arrow_color,
            );
        }
        Control::Toggle { rect, is_on, is_hovered, .. } => {
            draw_toggle_switch(bits, win_w, win_h, *rect, *is_on, *is_hovered, theme);
        }
        Control::TextField { rect, text, placeholder, font, bg_color, border_color, text_color, .. } => {
            draw_readonly_text_field(
                dib_dc, bits, win_w, win_h, *rect, text, placeholder.as_deref(),
                select_font(dib_dc, *font, title_font, body_font, small_font),
                *bg_color, *border_color, *text_color,
            );
        }
        Control::Button { rect, label, font, is_hovered, style, .. } => {
            draw_button(
                dib_dc, bits, win_w, win_h,
                rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top,
                label, theme,
                select_font(dib_dc, *font, title_font, body_font, small_font),
                *is_hovered, *style,
            );
        }
        Control::BindingRow { rect, trigger, kind_label, param, is_selected, is_hovered, zebra, .. } => {
            // Background rounded rect
            let bg = if *is_selected {
                theme.selected
            } else if *is_hovered {
                theme.hover
            } else if *zebra {
                theme.surface.blend_over(theme.background)
            } else {
                return; // no background for even rows
            };
            let radius = (4.0 * (win_w as f32 / WIN_WIDTH_BASE as f32)) as i32;
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *rect, radius, bg);

            // Trigger text
            unsafe {
                let _ = SelectObject(dib_dc, small_font);
                let _ = SetTextColor(dib_dc, theme.text.to_colorref());
            }
            let mut trig_wz = to_utf16_z(trigger);
            let mut trig_rc = RECT {
                left: rect.left + 8,
                top: rect.top,
                right: rect.left + lay_trig_w(win_w),
                bottom: rect.bottom,
            };
            unsafe {
                let _ = DrawTextW(dib_dc, &mut trig_wz, &mut trig_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            }

            // Kind label
            unsafe {
                let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
            }
            let kind_x = rect.left + lay_trig_w(win_w) + (8.0 * (win_w as f32 / WIN_WIDTH_BASE as f32)) as i32;
            let mut kind_wz = to_utf16_z(kind_label);
            let mut kind_rc = RECT {
                left: kind_x + 8,
                top: rect.top,
                right: kind_x + lay_kind_w(win_w),
                bottom: rect.bottom,
            };
            unsafe {
                let _ = DrawTextW(dib_dc, &mut kind_wz, &mut kind_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            }

            // Param text
            let param_x = kind_x + lay_kind_w(win_w) + (8.0 * (win_w as f32 / WIN_WIDTH_BASE as f32)) as i32;
            let param_w = win_w - rect.left - lay_del_w(win_w) - param_x;
            if !param.is_empty() {
                let mut param_wz = to_utf16_z(param);
                let mut param_rc = RECT {
                    left: param_x + 4,
                    top: rect.top,
                    right: param_x + param_w,
                    bottom: rect.bottom,
                };
                unsafe {
                    let _ = DrawTextW(dib_dc, &mut param_wz, &mut param_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
                }
            }
        }
        Control::AccordionPanel { rect, bg_color, border_color, radius } => {
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *rect, *radius, *bg_color);
            if border_color.a > 0 {
                draw_rounded_border_in_buffer(bits, win_w, win_h, *rect, *radius, 1, *border_color);
            }
        }
        Control::AccordionField { rect, text, font, bg_color, text_color, .. } => {
            let scale = win_w as f32 / WIN_WIDTH_BASE as f32;
            let radius = (4.0 * scale) as i32;
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *rect, radius, *bg_color);
            unsafe {
                let _ = SelectObject(dib_dc, select_font(dib_dc, *font, title_font, body_font, small_font));
                let _ = SetTextColor(dib_dc, text_color.to_colorref());
            }
            let mut wz = to_utf16_z(text);
            let mut text_rc = RECT {
                left: rect.left + 4,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            };
            unsafe {
                let _ = DrawTextW(dib_dc, &mut wz, &mut text_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
            }
        }
        Control::AccordionHelp { rect, text, text_color } => {
            unsafe {
                let _ = SelectObject(dib_dc, small_font);
                let _ = SetTextColor(dib_dc, text_color.to_colorref());
            }
            let mut wz = to_utf16_z(text);
            let mut rc = *rect;
            unsafe {
                let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            }
        }
        Control::Scrollbar { rect: _, track_rect, thumb_rect, thumb_color, .. } => {
            let scroll_w = thumb_rect.right - thumb_rect.left;
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *track_rect, scroll_w / 2, theme.border);
            draw_rounded_rect_in_buffer(bits, win_w, win_h, *thumb_rect, scroll_w / 2, *thumb_color);
        }
        Control::ClipStart { rect } => {
            unsafe {
                let _ = IntersectClipRect(dib_dc, rect.left, rect.top, rect.right, rect.bottom);
            }
        }
        Control::ClipEnd => {
            unsafe {
                let rgn = CreateRectRgn(0, 0, win_w, win_h);
                let _ = SelectClipRgn(dib_dc, rgn);
                let _ = DeleteObject(rgn);
            }
        }
    }
}

/// Iterate a [`ControlList`], painting every entry, and return the derived
/// hit regions.
pub fn paint_page(
    controls: &ControlList,
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    theme: &NativeTheme,
    title_font: HFONT,
    body_font: HFONT,
    small_font: HFONT,
) -> Vec<HitRegion> {
    for c in &controls.controls {
        paint_control(c, dib_dc, bits, win_w, win_h, theme, title_font, body_font, small_font);
    }
    controls.regions()
}

// ── Helpers: layout accessors that only need win_w ─────────────────

fn lay_trig_w(win_w: i32) -> i32 {
    // trig_w = TRIG_WIDTH_BASE * scale = 160 * scale, scale = win_w / WIN_WIDTH_BASE
    (160.0 * win_w as f32 / WIN_WIDTH_BASE as f32) as i32
}

fn lay_kind_w(win_w: i32) -> i32 {
    (150.0 * win_w as f32 / WIN_WIDTH_BASE as f32) as i32
}

fn lay_del_w(win_w: i32) -> i32 {
    (28.0 * win_w as f32 / WIN_WIDTH_BASE as f32) as i32
}

// ── General page controls ────────────────────────────────────────────

/// Build the General page control list (no drawing).
pub fn build_general_controls(
    lay: &Layout,
    state: &SettingsState,
) -> ControlList {
    let mut ctls = ControlList::new();
    let scale = (lay.win_w() as f32) / WIN_WIDTH_BASE as f32;
    let section_header_h = (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let section_gap = (SECTION_GAP_BASE as f32 * scale) as i32;

    // Appearance section header
    ctls.push(Control::Header {
        rect: RECT {
            left: lay.pad(),
            top: lay.appearance_y(),
            right: lay.win_w() - lay.pad(),
            bottom: lay.appearance_y() + section_header_h,
        },
        label: "Appearance".into(),
        font: FontChoice::Title,
        text_color: theme_text(state),
    });

    // Theme label
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.pad(),
            top: lay.combo_y(),
            right: lay.pad() + lay.label_w(),
            bottom: lay.combo_y() + 24,
        },
        label: "Theme".into(),
        font: FontChoice::Body,
        text_color: theme_text(state),
    });

    // Combo box
    let combo_h = COMBO_HIT_HEIGHT.max((COMBO_HIT_HEIGHT as f32 * scale) as i32);
    let combo_rect = RECT {
        left: lay.combo_x(),
        top: lay.combo_y(),
        right: lay.combo_x() + lay.combo_w(),
        bottom: lay.combo_y() + combo_h,
    };
    let is_combo_hovered = state.hovered_target == HoverTarget::ThemeCombo;
    let combo_bg = if is_combo_hovered {
        theme_hover(state).blend_over(theme_surface(state))
    } else {
        theme_surface(state)
    };
    let combo_border = if is_combo_hovered {
        theme_text(state)
    } else {
        theme_border(state)
    };
    let arrow_color = if is_combo_hovered {
        theme_text(state)
    } else {
        theme_text_muted(state)
    };
    let sel_name = state
        .theme_names
        .get(state.theme_sel)
        .map(|s| s.as_str())
        .unwrap_or("built-in dark");
    ctls.push(Control::Combo {
        rect: combo_rect,
        arrow_width: lay.arrow_w(),
        selected: sel_name.into(),
        font: FontChoice::Body,
        bg_color: combo_bg,
        border_color: combo_border,
        text_color: theme_text(state),
        arrow_color,
        hit: SettingsHit::ThemeCombo,
    });

    // Theme help text
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.combo_x() + lay.combo_w() + lay.pad(),
            top: lay.combo_y(),
            right: lay.win_w() - lay.pad(),
            bottom: lay.combo_y() + combo_h,
        },
        label: "Set the colour theme for mhd UI elements.".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Section divider
    let divider_y = lay.autostart_y() - section_header_h - section_gap / 2;
    ctls.push(Control::Divider {
        rect: RECT {
            left: lay.pad(),
            top: divider_y,
            right: lay.win_w() - lay.pad(),
            bottom: divider_y + 1,
        },
    });

    // Startup section header
    ctls.push(Control::Header {
        rect: RECT {
            left: lay.pad(),
            top: divider_y + section_gap / 2,
            right: lay.win_w() - lay.pad(),
            bottom: lay.autostart_y(),
        },
        label: "Startup".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Autostart label
    let auto_row_h = (20.0 * scale) as i32;
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.pad(),
            top: lay.autostart_y(),
            right: lay.pad() + lay.label_w(),
            bottom: lay.autostart_y() + auto_row_h,
        },
        label: "Autostart".into(),
        font: FontChoice::Body,
        text_color: theme_text(state),
    });

    // Toggle switch
    let toggle_h = (18.0 * scale) as i32;
    let toggle_w = (36.0 * scale) as i32;
    let toggle_y = lay.autostart_y() + (auto_row_h - toggle_h) / 2;
    let toggle_rect = RECT {
        left: lay.combo_x(),
        top: toggle_y,
        right: lay.combo_x() + toggle_w,
        bottom: toggle_y + toggle_h,
    };
    let is_auto_hovered = state.hovered_target == HoverTarget::AutostartToggle;
    ctls.push(Control::Toggle {
        rect: toggle_rect,
        is_on: state.autostart,
        is_hovered: is_auto_hovered,
        hit: SettingsHit::AutostartToggle,
    });

    // Autostart help text
    let auto_help = if state.autostart {
        "Enabled (runs at logon with highest privileges)"
    } else {
        "Start mhd automatically when you log on"
    };
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.combo_x() + toggle_w + lay.pad(),
            top: lay.autostart_y(),
            right: lay.win_w() - lay.pad(),
            bottom: lay.autostart_y() + auto_row_h,
        },
        label: auto_help.into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Quick Note divider
    let notes_divider_y = lay.autostart_y() + auto_row_h + section_gap / 2;
    ctls.push(Control::Divider {
        rect: RECT {
            left: lay.pad(),
            top: notes_divider_y,
            right: lay.win_w() - lay.pad(),
            bottom: notes_divider_y + 1,
        },
    });

    // Quick Note section header
    let notes_header_y = notes_divider_y + section_gap / 2;
    ctls.push(Control::Header {
        rect: RECT {
            left: lay.pad(),
            top: notes_header_y,
            right: lay.win_w() - lay.pad(),
            bottom: notes_header_y + section_header_h,
        },
        label: "Quick Note".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Save-path label
    let notes_path_y = notes_header_y + section_header_h;
    let notes_row_h = (28.0 * scale) as i32;
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.pad(),
            top: notes_path_y,
            right: lay.pad() + lay.label_w(),
            bottom: notes_path_y + notes_row_h,
        },
        label: "Save path".into(),
        font: FontChoice::Body,
        text_color: theme_text(state),
    });

    // Read-only path text field
    let browse_btn_w = (80.0 * scale) as i32;
    let browse_x = lay.combo_x() + lay.combo_w() - browse_btn_w;
    let path_field_w = browse_x - lay.combo_x() - (4.0 * scale) as i32;

    ctls.push(Control::TextField {
        rect: RECT {
            left: lay.combo_x(),
            top: notes_path_y,
            right: lay.combo_x() + path_field_w,
            bottom: notes_path_y + notes_row_h,
        },
        text: state.notes_dir.to_string_lossy().to_string(),
        placeholder: None,
        font: FontChoice::Small,
        bg_color: theme_surface(state).blend_over(theme_background(state)),
        border_color: theme_border(state),
        text_color: theme_text(state),
        hit: SettingsHit::None,
    });

    // Browse button
    let is_browse_hovered = state.hovered_target == HoverTarget::NotesDirBrowseBtn;
    ctls.push(Control::Button {
        rect: RECT {
            left: browse_x,
            top: notes_path_y,
            right: browse_x + browse_btn_w,
            bottom: notes_path_y + notes_row_h,
        },
        label: "Browse\u{2026}".into(),
        font: FontChoice::Small,
        is_hovered: is_browse_hovered,
        style: ButtonStyle::Secondary,
        hit: SettingsHit::NotesDirBrowseBtn,
    });

    // Quick Draw divider
    let draw_divider_y = notes_path_y + notes_row_h + section_gap / 2;
    ctls.push(Control::Divider {
        rect: RECT {
            left: lay.pad(),
            top: draw_divider_y,
            right: lay.win_w() - lay.pad(),
            bottom: draw_divider_y + 1,
        },
    });

    // Quick Draw section header
    let draw_header_y = draw_divider_y + section_gap / 2;
    ctls.push(Control::Header {
        rect: RECT {
            left: lay.pad(),
            top: draw_header_y,
            right: lay.win_w() - lay.pad(),
            bottom: draw_header_y + section_header_h,
        },
        label: "Quick Draw".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Save-path label
    let draw_path_y = draw_header_y + section_header_h;
    let draw_row_h = (28.0 * scale) as i32;
    ctls.push(Control::Label {
        rect: RECT {
            left: lay.pad(),
            top: draw_path_y,
            right: lay.pad() + lay.label_w(),
            bottom: draw_path_y + draw_row_h,
        },
        label: "Save path".into(),
        font: FontChoice::Body,
        text_color: theme_text(state),
    });

    // Read-only path text field
    let draw_browse_btn_w = (80.0 * scale) as i32;
    let draw_browse_x = lay.combo_x() + lay.combo_w() - draw_browse_btn_w;
    let draw_path_field_w = draw_browse_x - lay.combo_x() - (4.0 * scale) as i32;

    ctls.push(Control::TextField {
        rect: RECT {
            left: lay.combo_x(),
            top: draw_path_y,
            right: lay.combo_x() + draw_path_field_w,
            bottom: draw_path_y + draw_row_h,
        },
        text: state.draw_dir.to_string_lossy().to_string(),
        placeholder: None,
        font: FontChoice::Small,
        bg_color: theme_surface(state).blend_over(theme_background(state)),
        border_color: theme_border(state),
        text_color: theme_text(state),
        hit: SettingsHit::None,
    });

    // Browse button
    let is_draw_browse_hovered = state.hovered_target == HoverTarget::DrawDirBrowseBtn;
    ctls.push(Control::Button {
        rect: RECT {
            left: draw_browse_x,
            top: draw_path_y,
            right: draw_browse_x + draw_browse_btn_w,
            bottom: draw_path_y + draw_row_h,
        },
        label: "Browse\u{2026}".into(),
        font: FontChoice::Small,
        is_hovered: is_draw_browse_hovered,
        style: ButtonStyle::Secondary,
        hit: SettingsHit::DrawDirBrowseBtn,
    });

    ctls
}

// ── Advanced page controls ───────────────────────────────────────────

/// Build the Advanced page control list (no drawing).
pub fn build_advanced_controls(
    lay: &Layout,
    state: &SettingsState,
) -> ControlList {
    let mut ctls = ControlList::new();
    let scale = (lay.win_w() as f32) / WIN_WIDTH_BASE as f32;
    let section_header_h = (SECTION_HEADER_HEIGHT_BASE as f32 * scale) as i32;

    // Title header
    ctls.push(Control::Header {
        rect: RECT {
            left: lay.pad(),
            top: lay.shortcuts_y(),
            right: lay.win_w() - lay.pad(),
            bottom: lay.shortcuts_y() + section_header_h,
        },
        label: "Advanced".into(),
        font: FontChoice::Title,
        text_color: theme_text(state),
    });

    // Action buttons
    let pad = lay.pad();
    let mut cur_y = lay.shortcuts_y() + section_header_h;
    let btn_h = (36.0 * scale) as i32;
    let btn_gap = (8.0 * scale) as i32;
    let btn_w = (260.0 * scale) as i32;

    for (i, &(label, _desc)) in ADVANCED_BUTTONS.iter().enumerate() {
        let is_hovered = state.hovered_target == HoverTarget::AdvancedBtn(i);
        let is_danger = i >= 4;
        let style = if is_danger {
            ButtonStyle::DangerGhost
        } else {
            ButtonStyle::Secondary
        };

        ctls.push(Control::Button {
            rect: RECT {
                left: pad,
                top: cur_y,
                right: pad + btn_w,
                bottom: cur_y + btn_h,
            },
            label: label.to_string(),
            font: FontChoice::Body,
            is_hovered,
            style,
            hit: SettingsHit::AdvancedButton(i),
        });

        cur_y += btn_h + btn_gap;
    }

    ctls
}

// ── Shortcuts page controls ───────────────────────────────────────────

/// Build the Shortcuts page control list (no drawing).
pub fn build_shortcuts_controls(
    lay: &Layout,
    state: &SettingsState,
) -> ControlList {
    let mut ctls = ControlList::new();
    let scale = (lay.win_w() as f32) / WIN_WIDTH_BASE as f32;
    let row_h = lay.row_h();
    let pad = lay.pad();
    let list_y = lay.list_y();
    let list_h = lay.list_h();
    let win_w_val = lay.win_w();

    // Title
    ctls.push(Control::Header {
        rect: RECT {
            left: pad,
            top: lay.shortcuts_y(),
            right: win_w_val - pad,
            bottom: lay.shortcuts_y() + (24.0 * scale) as i32,
        },
        label: "Shortcuts".into(),
        font: FontChoice::Title,
        text_color: theme_text(state),
    });

    // Table headers
    let table_header_y = list_y - (24.0 * scale) as i32;
    let kind_x = pad + lay.trig_w() + (8.0 * scale) as i32;
    let param_x = kind_x + lay.kind_w() + (8.0 * scale) as i32;
    let col_h = (16.0 * scale) as i32;

    ctls.push(Control::Label {
        rect: RECT {
            left: pad + 6,
            top: table_header_y,
            right: pad + lay.trig_w(),
            bottom: table_header_y + col_h,
        },
        label: "TRIGGER".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    ctls.push(Control::Label {
        rect: RECT {
            left: kind_x + 6,
            top: table_header_y,
            right: kind_x + lay.kind_w(),
            bottom: table_header_y + col_h,
        },
        label: "ACTION".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    ctls.push(Control::Label {
        rect: RECT {
            left: param_x + 6,
            top: table_header_y,
            right: win_w_val - pad - lay.del_w(),
            bottom: table_header_y + col_h,
        },
        label: "PARAMETER".into(),
        font: FontChoice::Small,
        text_color: theme_text_muted(state),
    });

    // Content height (for scrollbar and positioning)
    let content_h = state.bindings.len() as i32 * row_h
        + if state.expanded_idx.is_some() {
            lay.accordion_h()
        } else {
            0
        }
        + row_h;

    // Clipped list area begins here
    ctls.push(Control::ClipStart {
        rect: RECT {
            left: pad,
            top: list_y,
            right: win_w_val - pad,
            bottom: list_y + list_h,
        },
    });

    // Binding rows
    let mut row_y = list_y - state.bindings_scroll_y;
    for (i, b) in state.bindings.iter().enumerate() {
        let row_visible = row_y + row_h >= list_y && row_y < list_y + list_h;

        if row_visible {
            let desc = editor_action_desc(b.kind_idx);
            let param_text = if desc.param_key.is_some() && !b.param.is_empty() {
                b.param.clone()
            } else {
                String::new()
            };

            // Row body (pushed first in draw order; hit scan is back-to-front
            // so the delete-X button pushed next wins for overlaps).
            let is_row_hovered = state.hovered_target == HoverTarget::RowClick(i);
            let is_selected = state.expanded_idx == Some(i);
            let zebra_on = i % 2 == 1;
            ctls.push(Control::BindingRow {
                rect: RECT {
                    left: pad,
                    top: row_y,
                    right: win_w_val - pad,
                    bottom: row_y + row_h,
                },
                trigger: b.trigger.clone(),
                kind_label: desc.label.to_string(),
                param: param_text,
                is_selected,
                is_hovered: is_row_hovered,
                zebra: zebra_on,
                hit: SettingsHit::RowClick(i),
            });

            // Delete X button — drawn on top of the row body.  Because the
            // hit scan is back-to-front this narrower Button wins over the
            // wider BindingRow for clicks on the X.
            let del_x = win_w_val - pad - lay.del_w();
            let is_del_hovered = state.hovered_target == HoverTarget::RowDelete(i);
            ctls.push(Control::Button {
                rect: RECT {
                    left: del_x,
                    top: row_y + (row_h - lay.del_w()) / 2,
                    right: del_x + lay.del_w(),
                    bottom: row_y + (row_h + lay.del_w()) / 2,
                },
                label: "X".into(),
                font: FontChoice::Small,
                is_hovered: is_del_hovered,
                style: ButtonStyle::DangerGhost,
                hit: SettingsHit::RowDelete(i),
            });
        }

        row_y += row_h;

        // Accordion editor below the expanded row
        if Some(i) == state.expanded_idx && i < state.bindings.len() {
            let acc_visible = row_y + lay.accordion_h() >= list_y && row_y < list_y + list_h;

            if acc_visible {
                let gap = (8.0 * scale) as i32;
                let btn_h = (28.0 * scale) as i32;
                let pw = win_w_val - pad * 2 - gap * 2;
                let panel_w = pw + gap * 2;
                let panel_h = lay.accordion_h();
                let panel_bg = theme_surface(state).blend_over(theme_background(state));
                let field_x = pad + gap;

                // Panel background
                ctls.push(Control::AccordionPanel {
                    rect: RECT {
                        left: pad,
                        top: row_y,
                        right: pad + panel_w,
                        bottom: row_y + panel_h,
                    },
                    bg_color: panel_bg,
                    border_color: theme_border(state),
                    radius: (6.0 * scale) as i32,
                });

                let mut acc_cur_y = row_y + (12.0 * scale) as i32;
                let trigger_field_w = (pw - btn_h - gap) * 2 / 5;
                let action_btn_w = pw - trigger_field_w - btn_h - gap * 2;

                // ── Row 1: trigger field, record btn, action btn ──
                let is_tf_hovered = state.hovered_target == HoverTarget::AccordionTriggerField;
                let tf_bg = if is_tf_hovered {
                    theme_hover(state).blend_over(panel_bg)
                } else {
                    theme_background(state).blend_over(panel_bg)
                };
                let trigger_text = if state.acc_is_recording {
                    "...".to_string()
                } else {
                    state.acc_trigger.clone()
                };

                ctls.push(Control::AccordionField {
                    rect: RECT {
                        left: field_x,
                        top: acc_cur_y,
                        right: field_x + trigger_field_w,
                        bottom: acc_cur_y + btn_h,
                    },
                    text: trigger_text,
                    font: FontChoice::Small,
                    bg_color: tf_bg,
                    text_color: theme_text(state),
                    hit: SettingsHit::AccordionTriggerField,
                });

                let record_x = field_x + trigger_field_w + gap;
                let is_rec_hovered = state.hovered_target == HoverTarget::AccordionRecordBtn;
                ctls.push(Control::Button {
                    rect: RECT {
                        left: record_x,
                        top: acc_cur_y,
                        right: record_x + btn_h,
                        bottom: acc_cur_y + btn_h,
                    },
                    label: if state.acc_is_recording { "Rec".into() } else { "Bind".into() },
                    font: FontChoice::Small,
                    is_hovered: is_rec_hovered,
                    style: ButtonStyle::Primary,
                    hit: SettingsHit::AccordionRecordBtn,
                });

                let act_x = record_x + btn_h + gap;
                let is_act_hovered = state.hovered_target == HoverTarget::AccordionActionBtn;
                let desc = editor_action_desc(state.acc_kind_idx);
                ctls.push(Control::Button {
                    rect: RECT {
                        left: act_x,
                        top: acc_cur_y,
                        right: act_x + action_btn_w,
                        bottom: acc_cur_y + btn_h,
                    },
                    label: desc.label.to_string(),
                    font: FontChoice::Small,
                    is_hovered: is_act_hovered,
                    style: ButtonStyle::Secondary,
                    hit: SettingsHit::AccordionActionBtn,
                });

                acc_cur_y += btn_h + gap;

                // ── Row 2: parameter field ──
                let is_key_mapping = desc.param_schema == ActionParamSchema::KeyMapping;
                let param_field_w = if is_key_mapping {
                    pw - btn_h - gap
                } else {
                    pw
                };

                match desc.param_schema {
                    ActionParamSchema::None | ActionParamSchema::PowerAction => {
                        ctls.push(Control::AccordionHelp {
                            rect: RECT {
                                left: field_x,
                                top: acc_cur_y,
                                right: field_x + pw,
                                bottom: acc_cur_y + btn_h,
                            },
                            text: "No options required".into(),
                            text_color: theme_text_muted(state),
                        });
                    }
                    _ => {
                        let param_text = if state.acc_param.is_empty() {
                            "<not set>".to_string()
                        } else {
                            state.acc_param.clone()
                        };
                        let is_param_hovered = state.hovered_target == HoverTarget::AccordionParamField;
                        let p_bg = if is_param_hovered {
                            theme_hover(state).blend_over(panel_bg)
                        } else {
                            theme_background(state).blend_over(panel_bg)
                        };
                        ctls.push(Control::AccordionField {
                            rect: RECT {
                                left: field_x,
                                top: acc_cur_y,
                                right: field_x + param_field_w,
                                bottom: acc_cur_y + btn_h,
                            },
                            text: param_text,
                            font: FontChoice::Small,
                            bg_color: p_bg,
                            text_color: theme_text(state),
                            hit: SettingsHit::AccordionParamField,
                        });

                        if is_key_mapping {
                            let param_record_x = field_x + param_field_w + gap;
                            let is_param_rec_hovered = state.hovered_target == HoverTarget::AccordionParamRecordBtn;
                            ctls.push(Control::Button {
                                rect: RECT {
                                    left: param_record_x,
                                    top: acc_cur_y,
                                    right: param_record_x + btn_h,
                                    bottom: acc_cur_y + btn_h,
                                },
                                label: if state.acc_is_recording_param { "Rec".into() } else { "Bind".into() },
                                font: FontChoice::Small,
                                is_hovered: is_param_rec_hovered,
                                style: ButtonStyle::Primary,
                                hit: SettingsHit::AccordionParamRecordBtn,
                            });
                        }
                    }
                }

                acc_cur_y += btn_h + gap;

                // Error message row
                if let Some(ref err) = state.acc_save_error {
                    ctls.push(Control::AccordionHelp {
                        rect: RECT {
                            left: field_x,
                            top: acc_cur_y,
                            right: field_x + pw,
                            bottom: acc_cur_y + (16.0 * scale) as i32,
                        },
                        text: err.clone(),
                        text_color: Argb::new(255, 220, 60, 60),
                    });
                    acc_cur_y += (16.0 * scale) as i32 + gap;
                }
                acc_cur_y += gap;

                // ── Footer buttons: Save / Cancel / Delete ──
                let fbtn_w = (70.0 * scale) as i32;
                let fbtn_gap = (8.0 * scale) as i32;
                let total = fbtn_w * 3 + fbtn_gap * 2;
                let start_x = pad + (panel_w - total) / 2;

                let is_save_hovered = state.hovered_target == HoverTarget::AccordionSaveBtn;
                let is_cancel_hovered = state.hovered_target == HoverTarget::AccordionCancelBtn;
                let is_delete_hovered = state.hovered_target == HoverTarget::AccordionDeleteBtn;

                ctls.push(Control::Button {
                    rect: RECT {
                        left: start_x,
                        top: acc_cur_y,
                        right: start_x + fbtn_w,
                        bottom: acc_cur_y + btn_h,
                    },
                    label: "Save".into(),
                    font: FontChoice::Small,
                    is_hovered: is_save_hovered,
                    style: ButtonStyle::Primary,
                    hit: SettingsHit::AccordionSaveBtn,
                });
                ctls.push(Control::Button {
                    rect: RECT {
                        left: start_x + fbtn_w + fbtn_gap,
                        top: acc_cur_y,
                        right: start_x + fbtn_w + fbtn_gap + fbtn_w,
                        bottom: acc_cur_y + btn_h,
                    },
                    label: "Cancel".into(),
                    font: FontChoice::Small,
                    is_hovered: is_cancel_hovered,
                    style: ButtonStyle::Secondary,
                    hit: SettingsHit::AccordionCancelBtn,
                });
                ctls.push(Control::Button {
                    rect: RECT {
                        left: start_x + (fbtn_w + fbtn_gap) * 2,
                        top: acc_cur_y,
                        right: start_x + (fbtn_w + fbtn_gap) * 2 + fbtn_w,
                        bottom: acc_cur_y + btn_h,
                    },
                    label: "Delete".into(),
                    font: FontChoice::Small,
                    is_hovered: is_delete_hovered,
                    style: ButtonStyle::DangerGhost,
                    hit: SettingsHit::AccordionDeleteBtn,
                });
            }

            row_y += lay.accordion_h();
        }
    }

    // ── "Add New" button ──
    let add_btn_visible = row_y + row_h >= list_y && row_y < list_y + list_h;
    if add_btn_visible {
        let add_btn_w = (80.0 * scale) as i32;
        let is_add_hovered = state.hovered_target == HoverTarget::AddBtn;
        ctls.push(Control::Button {
            rect: RECT {
                left: pad,
                top: row_y + (row_h - lay.btn_h()) / 2,
                right: pad + add_btn_w,
                bottom: row_y + (row_h + lay.btn_h()) / 2,
            },
            label: "+ Add".into(),
            font: FontChoice::Small,
            is_hovered: is_add_hovered,
            style: ButtonStyle::Secondary,
            hit: SettingsHit::AddBtn,
        });
    }

    // End clipped list area
    ctls.push(Control::ClipEnd);

    // ── Scrollbar ──
    if content_h > list_h {
        let scroll_w = (6.0 * scale) as i32;
        let scroll_x = win_w_val - pad + (pad - scroll_w) / 2;
        let track_rect = RECT {
            left: scroll_x,
            top: list_y,
            right: scroll_x + scroll_w,
            bottom: list_y + list_h,
        };

        let thumb_h = ((list_h as f32 / content_h as f32) * list_h as f32) as i32;
        let thumb_h = thumb_h.max((30.0 * scale) as i32);
        let max_scroll = content_h - list_h;
        let thumb_y = list_y
            + ((state.bindings_scroll_y as f32 / max_scroll as f32) * (list_h - thumb_h) as f32)
                as i32;
        let thumb_rect = RECT {
            left: scroll_x,
            top: thumb_y,
            right: scroll_x + scroll_w,
            bottom: thumb_y + thumb_h,
        };

        let is_thumb_active =
            state.hovered_target == HoverTarget::Scrollbar || state.is_dragging_scroll;
        let thumb_color = if is_thumb_active {
            theme_accent(state)
        } else {
            theme_text_muted(state)
        };

        ctls.push(Control::Scrollbar {
            rect: RECT {
                left: scroll_x - 4,
                top: list_y,
                right: scroll_x + scroll_w + 4,
                bottom: list_y + list_h,
            },
            track_rect,
            thumb_rect,
            thumb_color,
            hit: SettingsHit::Scrollbar,
        });
    }

    ctls
}

// ── Theme accessor shims (we don't have a &NativeTheme in build fns) ─

fn theme_text(state: &SettingsState) -> Argb { state.theme.text }
fn theme_text_muted(state: &SettingsState) -> Argb { state.theme.text_muted }
fn theme_border(state: &SettingsState) -> Argb { state.theme.border }
fn theme_surface(state: &SettingsState) -> Argb { state.theme.surface }
fn theme_hover(state: &SettingsState) -> Argb { state.theme.hover }
fn theme_accent(state: &SettingsState) -> Argb { state.theme.accent }
fn theme_background(state: &SettingsState) -> Argb { state.theme.background }
