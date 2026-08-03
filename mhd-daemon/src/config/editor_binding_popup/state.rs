//! Popup state and editing transitions for the binding editor popup.
//!
//! Owns `BindingPopupState` (the modal popup's editable copy of one shortcut
//! binding) and the pure state transitions that mutate it: committing or
//! cancelling a parameter text edit and opening the action dropdown.

use windows::Win32::Foundation::HWND;

use crate::config::editor_binding_popup::layout::action_dropdown_visible_rows;
use crate::config::editor_key_combo::{KeyComboEditorState, KeyComboSlot};
use crate::config::editor_layout::editor_action_desc;
use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
use crate::config::editor_state::SettingsState;
use crate::core::action::ActionParamSchema;
use crate::core::native_theme::NativeTheme;

// ── Popup state ────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct BindingPopupState {
    pub hwnd: HWND,
    pub parent_hwnd: HWND,
    pub parent_ptr: *mut SettingsState,
    pub binding_idx: usize,

    // Edited data (cloned from parent on open)
    pub trigger: String,
    pub trigger_editor: KeyComboEditorState,
    pub kind_idx: usize,
    pub param: String,

    // Key-combo editor used when schema is ActionParamSchema::KeyMapping
    pub param_editor: KeyComboEditorState,

    // UI state
    pub theme: NativeTheme,
    pub scale: f32,
    pub win_w: i32,
    pub win_h: i32,
    pub is_recording_trigger: bool,
    pub is_recording_param: bool,
    pub is_editing_param: bool,
    pub param_edit_cursor: usize,
    pub param_edit_old: String,
    pub param_save_error: Option<String>,
    pub hovered_target: BindingPopupHit,
    pub action_items: Vec<SearchDropdownItem>,

    // Combo popup
    pub action_dropdown: SearchDropdownState,

    // Signal the modal loop to exit
    pub should_close: bool,
    pub saved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPopupHit {
    None,
    TriggerSlot(KeyComboSlot),
    TriggerKeyItem(usize),
    RecordTrigger,
    KindCombo,
    ParamSlot(KeyComboSlot),
    ParamKeyItem(usize),
    ParamField,
    RecordParam,
    BrowseParam,
    SaveBtn,
    CancelBtn,
    KindItem(usize),
}

// ── Editing transitions ────────────────────────────────────────────────

/// Commit any in-progress param text edit, clearing the editing state.
/// Validates number ranges and clamps them on commit.
pub(crate) fn commit_param_edit(state: &mut BindingPopupState) {
    state.is_editing_param = false;
    state.param_edit_cursor = state.param.len();
    state.param_save_error = None;

    let schema = editor_action_desc(state.kind_idx).param_schema;
    if let ActionParamSchema::Number { min, max, .. } = schema {
        let val: i32 = state.param.trim().parse().unwrap_or(min);
        if val < min || val > max {
            if val < min {
                state.param = min.to_string();
            } else {
                state.param = max.to_string();
            }
        } else {
            state.param = val.to_string();
        }
    }
}

/// Cancel any in-progress param text edit, restoring the old value.
pub(crate) fn cancel_param_edit(state: &mut BindingPopupState) {
    if state.is_editing_param {
        state.param = std::mem::take(&mut state.param_edit_old);
        state.is_editing_param = false;
        state.param_edit_cursor = state.param.len();
    }
    state.param_save_error = None;
}

/// Open the action (kind) dropdown, closing any key-combo dropdown first.
pub(crate) fn open_kind_dropdown(state: &mut BindingPopupState) {
    state.trigger_editor.close_dropdown();
    state.action_dropdown.open(
        &state.action_items,
        state.kind_idx,
        action_dropdown_visible_rows(),
    );
}
