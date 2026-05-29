//! Hit‑testing for the settings editor.
//!
//! Determines which UI element is under the cursor, given the current
//! editor state and layout geometry.

use crate::config::editor_layout::{
    COMBO_HIT_HEIGHT, SECTION_HEADER_HEIGHT_BASE,
    SECTION_GAP_BASE, ADVANCED_BUTTONS,
    editor_action_desc,
};
use crate::config::editor_state::{SettingsHit, SettingsPage, SettingsState};

// ── General page hit‑test ─────────────────────────────────────────

fn hit_test_general(state: &SettingsState, x: i32, y: i32) -> SettingsHit {
    let lay = &state.layout;
    let combo_h = (COMBO_HIT_HEIGHT as f32 * lay.scale) as i32;

    // Theme combo
    if y >= lay.combo_y
        && y < lay.combo_y + combo_h
        && x >= lay.combo_x
        && x < lay.combo_x + lay.combo_w
    {
        return SettingsHit::ThemeCombo;
    }

    // Autostart toggle
    if y >= lay.autostart_y
        && y < lay.autostart_y + (20.0 * lay.scale) as i32
        && x >= lay.combo_x
        && x < lay.combo_x + (36.0 * lay.scale) as i32
    {
        return SettingsHit::AutostartToggle;
    }

    // Quick Note browse button
    let notes_divider_y = lay.autostart_y + (20.0 * lay.scale) as i32 + (SECTION_GAP_BASE as f32 * lay.scale) as i32 / 2;
    let notes_header_y = notes_divider_y + (SECTION_GAP_BASE as f32 * lay.scale) as i32 / 2;
    let notes_path_y = notes_header_y + (SECTION_HEADER_HEIGHT_BASE as f32 * lay.scale) as i32;
    let notes_row_h = (28.0 * lay.scale) as i32;
    let browse_btn_w = (80.0 * lay.scale) as i32;
    let browse_x = lay.combo_x + lay.combo_w - browse_btn_w;
    if y >= notes_path_y && y < notes_path_y + notes_row_h && x >= browse_x && x < browse_x + browse_btn_w {
        return SettingsHit::NotesDirBrowseBtn;
    }

    SettingsHit::None
}

// ── Shortcuts page hit‑test ───────────────────────────────────────

fn hit_test_shortcuts(state: &SettingsState, x: i32, y: i32) -> SettingsHit {
    let lay = &state.layout;

    if y < lay.list_y || y >= lay.list_y + lay.list_h || x < lay.pad {
        return SettingsHit::None;
    }

    let content_h = state.bindings.len() as i32 * lay.row_h + if state.expanded_idx.is_some() { lay.accordion_h } else { 0 } + lay.row_h;

    // Scrollbar
    if content_h > lay.list_h {
        let scroll_w = (6.0 * lay.scale) as i32;
        let scroll_x = lay.win_w - lay.pad + (lay.pad - scroll_w) / 2;
        let scroll_left = scroll_x - 4;
        let scroll_right = scroll_x + scroll_w + 4;
        if x >= scroll_left && x < scroll_right {
            return SettingsHit::Scrollbar;
        }
    }

    // Binding rows — simplified to overview rows
    let mut row_y = lay.list_y - state.bindings_scroll_y;
    for i in 0..state.bindings.len() {
        if y >= row_y && y < row_y + lay.row_h {
            // Delete button (X) at the right edge
            if x >= lay.win_w - lay.pad - lay.del_w && x < lay.win_w - lay.pad {
                return SettingsHit::RowDelete(i);
            }
            // Click anywhere else on the row → open editor
            return SettingsHit::RowClick(i);
        }
        row_y += lay.row_h;

        // Accordion editor below expanded row
        if Some(i) == state.expanded_idx {
            let acc_hit = hit_test_accordion(state, x, y, lay.pad, row_y);
            if acc_hit != SettingsHit::None {
                return acc_hit;
            }
            row_y += lay.accordion_h;
        }
    }

    // Add binding row (click only on the button)
    if y >= row_y && y < row_y + lay.row_h {
        let btn_w = (80.0 * lay.scale) as i32;
        if x >= lay.pad && x < lay.pad + btn_w {
            return SettingsHit::AddBtn;
        }
    }

    SettingsHit::None
}

// ── Advanced page hit‑test ────────────────────────────────────────

fn hit_test_advanced(state: &SettingsState, x: i32, y: i32) -> SettingsHit {
    let lay = &state.layout;
    let pad = lay.pad;
    let mut cur_y = lay.shortcuts_y + (SECTION_HEADER_HEIGHT_BASE as f32 * lay.scale) as i32;
    let btn_h = (36.0 * lay.scale) as i32;
    let btn_gap = (8.0 * lay.scale) as i32;
    let btn_w = (260.0 * lay.scale) as i32;

    for i in 0..ADVANCED_BUTTONS.len() {
        if x >= pad && x < pad + btn_w && y >= cur_y && y < cur_y + btn_h {
            return SettingsHit::AdvancedButton(i);
        }
        cur_y += btn_h + btn_gap;
    }
    SettingsHit::None
}

// ── Accordion hit‑test ────────────────────────────────────────────

fn hit_test_accordion(state: &SettingsState, x: i32, y: i32, accordion_x: i32, accordion_y: i32) -> SettingsHit {
    let lay = &state.layout;
    if state.expanded_idx.is_none() {
        return SettingsHit::None;
    }
    let gap = (8.0 * lay.scale) as i32;
    let btn_h = (28.0 * lay.scale) as i32;
    let pw = lay.win_w - lay.pad * 2 - gap * 2;
    let panel_w = pw + gap * 2;
    let panel_h = lay.accordion_h;

    if x < accordion_x || x >= accordion_x + panel_w || y < accordion_y || y >= accordion_y + panel_h {
        return SettingsHit::None;
    }

    let field_x = accordion_x + gap;
    let mut cur_y = accordion_y + (12.0 * lay.scale) as i32;
    let trigger_field_w = (pw - btn_h - gap) * 2 / 5;
    let action_btn_w = pw - trigger_field_w - btn_h - gap * 2;

    // Row 1: trigger field, record btn, action btn
    if y >= cur_y && y < cur_y + btn_h {
        // Record button
        let record_x = field_x + trigger_field_w + gap;
        if x >= record_x && x < record_x + btn_h {
            return SettingsHit::AccordionRecordBtn;
        }
        // Action button
        let act_x = record_x + btn_h + gap;
        if x >= act_x && x < act_x + action_btn_w {
            return SettingsHit::AccordionActionBtn;
        }
        // Trigger field
        if x >= field_x && x < field_x + trigger_field_w {
            return SettingsHit::AccordionTriggerField;
        }
        return SettingsHit::AccordionTriggerField;
    }
    cur_y += btn_h + gap;

    // Row 2: parameter field
    if y >= cur_y && y < cur_y + btn_h {
        let desc = state.expanded_idx.and_then(|idx| {
            if idx < state.bindings.len() { Some(editor_action_desc(state.acc_kind_idx)) } else { None }
        });
        if desc.map(|d| d.param_schema) == Some(crate::action::ActionParamSchema::KeyMapping) {
            let param_field_w = pw - btn_h - gap;
            let param_record_x = field_x + param_field_w + gap;
            if x >= param_record_x && x < param_record_x + btn_h {
                return SettingsHit::AccordionParamRecordBtn;
            }
        }
        return SettingsHit::AccordionParamField;
    }
    cur_y += btn_h + gap;

    // Error message row
    if state.acc_save_error.is_some() {
        cur_y += (16.0 * lay.scale) as i32 + gap;
    }
    cur_y += gap;

    // Footer buttons
    if y >= cur_y && y < cur_y + btn_h {
        let btn_w = (70.0 * lay.scale) as i32;
        let btn_gap = (8.0 * lay.scale) as i32;
        let total = btn_w * 3 + btn_gap * 2;
        let start_x = accordion_x + (panel_w - total) / 2;

        let save_x = start_x;
        if x >= save_x && x < save_x + btn_w {
            return SettingsHit::AccordionSaveBtn;
        }
        let cancel_x = save_x + btn_w + btn_gap;
        if x >= cancel_x && x < cancel_x + btn_w {
            return SettingsHit::AccordionCancelBtn;
        }
        let delete_x = cancel_x + btn_w + btn_gap;
        if x >= delete_x && x < delete_x + btn_w {
            return SettingsHit::AccordionDeleteBtn;
        }
    }

    SettingsHit::None
}

// ── Central dispatch ──────────────────────────────────────────────

/// Central hit‑test for the settings window.
///
/// Dispatches to the correct page-specific handler based on the active
/// section, after checking footer buttons and tab bar.
pub fn hit_test_settings(state: &SettingsState, x: i32, y: i32) -> SettingsHit {
    let lay = &state.layout;

    // ── Footer buttons (always accessible) ──────────────────────
    if y >= lay.btn_y && y < lay.btn_y + lay.btn_h {
        if x >= lay.apply_x && x < lay.apply_x + lay.btn_w {
            return SettingsHit::ApplyBtn;
        }
        if x >= lay.close_x && x < lay.close_x + lay.btn_w {
            return SettingsHit::CloseBtn;
        }
    }

    // ── Tab bar (horizontal, below header separator) ──────────
    if y >= lay.tab_bar_y && y < lay.tab_bar_y + lay.tab_h {
        let tab_total_w = 3 * lay.tab_w + 2 * lay.tab_gap;
        let tab_start_x = (lay.win_w - tab_total_w) / 2;
        let idx = if x >= tab_start_x && x < tab_start_x + lay.tab_w {
            Some(0usize)
        } else if x >= tab_start_x + lay.tab_w + lay.tab_gap && x < tab_start_x + 2 * lay.tab_w + lay.tab_gap {
            Some(1usize)
        } else if x >= tab_start_x + 2 * (lay.tab_w + lay.tab_gap) && x < tab_start_x + tab_total_w {
            Some(2usize)
        } else {
            None
        };
        if let Some(i) = idx {
            return SettingsHit::Tab(i);
        }
    }

    // ── Page-specific controls ───────────────────────────────────
    match state.active_section {
        SettingsPage::General => hit_test_general(state, x, y),
        SettingsPage::Shortcuts => hit_test_shortcuts(state, x, y),
        SettingsPage::Advanced => hit_test_advanced(state, x, y),
    }
}
