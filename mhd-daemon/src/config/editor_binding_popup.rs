//! Modal popup for editing a single shortcut binding.
//!
//! Opens when the user clicks a shortcut row in the settings editor.
//! A small layered window with full GDI drawing — no child HWNDs.
//!
//! The Parameter row adapts to the selected action's param schema:
//!
//!   • ActionParamSchema::None        → row hidden entirely
//!   • ActionParamSchema::Text         → text field, no side button
//!   • ActionParamSchema::FilePath     → text field + [Browse…]
//!   • ActionParamSchema::KeyMapping   → text field + [Bind]
//!   • ActionParamSchema::Number       → text field, no side button
//!   • ActionParamSchema::PowerAction  → row hidden (no param_key)
//!
//! Layout (example with KeyMapping):
//! ┌─────────────────────────────────────┐
//! │  Edit Shortcut                      │
//! ├─────────────────────────────────────┤
//! │  Trigger              [Bind]        │
//! │  ┌───────────────────────────────┐  │
//! │  │ Ctrl+Shift+F                  │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Action              [▼]           │
//! │  ┌───────────────────────────────┐  │
//! │  │ show_volume_mixer             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │  Parameter            [Bind]        │
//! │  ┌───────────────────────────────┐  │
//! │  │ 5                             │  │
//! │  └───────────────────────────────┘  │
//! │                                     │
//! │       [Cancel]          [Save]      │
//! └─────────────────────────────────────┘
//!
//! Facade over the popup sub-modules. The old single god file was split into
//! focused modules, each owning one responsibility:
//!
//! - `window` — window class registration, creation, and the modal loop;
//! - `events` — Win32 messages converted into popup events (window procedure);
//! - `state` — popup state and editing transitions;
//! - `layout` — geometry constants and slot/dropdown layout helpers;
//! - `paint` — GDI painting of the popup surface;
//! - `hittest` — hit-testing rows, slots, dropdowns, and buttons;
//! - `params` — parameter-file selection and captured-key serialization.

pub mod events;
pub mod hittest;
pub mod layout;
pub mod paint;
pub mod params;
pub mod state;
pub mod window;

// ── Public entry point ────────────────────────────────────────────────

pub use window::open_binding_popup;

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use crate::config::editor_binding_popup::layout::{
        POPUP_HEIGHT_BASE, POPUP_WIDTH_BASE, trigger_slot_rects,
    };
    use crate::config::editor_binding_popup::params::{default_param_for_schema, key_to_string};
    use crate::config::editor_binding_popup::state::{
        BindingPopupHit, BindingPopupState, cancel_param_edit, commit_param_edit,
    };
    use crate::config::editor_key_combo::{KeyComboEditorState, KeyComboSlot};
    use crate::config::editor_layout::{editor_action_desc, editor_index_for_action_name};
    use crate::config::editor_search_dropdown::SearchDropdownState;
    use crate::core::native_theme::NativeTheme;
    use windows::Win32::Foundation::{HWND, RECT};

    fn test_popup_state(kind_idx: usize, param: &str) -> BindingPopupState {
        BindingPopupState {
            hwnd: HWND::default(),
            parent_hwnd: HWND::default(),
            parent_ptr: std::ptr::null_mut(),
            binding_idx: 0,
            trigger: String::new(),
            trigger_editor: KeyComboEditorState::from_trigger_string(""),
            kind_idx,
            param: param.to_string(),
            param_editor: KeyComboEditorState::from_trigger_string(""),
            theme: NativeTheme::default(),
            scale: 1.0,
            win_w: POPUP_WIDTH_BASE,
            win_h: POPUP_HEIGHT_BASE,
            is_recording_trigger: false,
            is_recording_param: false,
            is_editing_param: true,
            param_edit_cursor: 0,
            param_edit_old: String::new(),
            param_save_error: None,
            hovered_target: BindingPopupHit::None,
            action_items: Vec::new(),
            action_dropdown: SearchDropdownState::default(),
            should_close: false,
            saved: false,
        }
    }

    #[test]
    fn default_param_for_number_schema_is_five() {
        let desc = editor_action_desc(editor_index_for_action_name("brightness_up"));
        assert_eq!(default_param_for_schema(desc.param_schema), "5");
    }

    #[test]
    fn default_param_for_other_schemas_is_empty() {
        for name in ["replace_key", "run_ps", "quit"] {
            let desc = editor_action_desc(editor_index_for_action_name(name));
            assert_eq!(default_param_for_schema(desc.param_schema), "");
        }
    }

    #[test]
    fn commit_param_edit_clamps_number_to_range() {
        let idx = editor_index_for_action_name("brightness_up");

        let mut state = test_popup_state(idx, "150");
        commit_param_edit(&mut state);
        assert_eq!(state.param, "100");
        assert!(!state.is_editing_param);

        let mut state = test_popup_state(idx, "-3");
        commit_param_edit(&mut state);
        assert_eq!(state.param, "1");
        assert!(!state.is_editing_param);
    }

    #[test]
    fn cancel_param_edit_restores_old_value() {
        let idx = editor_index_for_action_name("run_ps");
        let mut state = test_popup_state(idx, "new");
        state.param_edit_old = "old".to_string();
        cancel_param_edit(&mut state);
        assert_eq!(state.param, "old");
        assert!(!state.is_editing_param);
    }

    #[test]
    fn trigger_slot_rects_layouts_four_slots() {
        let field = RECT {
            left: 108,
            top: 76,
            right: 556,
            bottom: 106,
        };
        let rects = trigger_slot_rects(field, 1.0);
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0].0, KeyComboSlot::Modifier(0));
        assert_eq!(rects[1].0, KeyComboSlot::Modifier(1));
        assert_eq!(rects[2].0, KeyComboSlot::Modifier(2));
        assert_eq!(rects[3].0, KeyComboSlot::Key);
        assert_eq!(rects[3].1.right, field.right);
        for pair in rects.windows(2) {
            assert!(pair[0].1.right <= pair[1].1.left);
        }
    }

    #[test]
    fn key_to_string_decodes_captured_data() {
        // mods=0, key_type=0 (keyboard), vk='A'
        assert_eq!(key_to_string(0x0041_0000), "a");
        // mods=ctrl(0x02), key_type=2 (wheel), key_val=0 (up)
        assert_eq!(key_to_string(0x0000_0202), "ctrl+wheel_up");
    }
}
