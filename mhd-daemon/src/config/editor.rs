//! Styled native Win32 Settings panel (modal, tray thread).
//!
//! Facade over the editor sub-modules. The old single god file was split into
//! focused modules, each owning one responsibility:
//!
//! - `window` — window creation, lifecycle, and top-level paint dispatch;
//! - `events` — Win32 messages converted into editor events;
//! - `reducer` — editor events applied to `SettingsState`;
//! - `persistence` — apply and save configuration;
//! - `vision` — vision test execution and result updates;
//! - `dropdowns` — dropdown and popup coordination;
//! - `pages` — per-page layout helpers.
//!
//! Sub-modules currently import shared items through `super::*`; once the
//! split stabilises those should be replaced with explicit imports.

pub mod dropdowns;
pub mod events;
pub mod pages;
pub mod persistence;
pub mod reducer;
pub mod vision;
pub mod window;

// ── Shared constants ─────────────────────────────────────────────────
const TAB_NAMES: &[&str] = &["General", "Shortcuts", "LLM Proxy", "LLM Trim", "Advanced"];
const HEAD_TUNE_TIMER_ID: usize = 0xB0B0;
const WM_VISION_PROMPT_UPDATED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10;
const VISION_TEST_PROMPT: &str = "Describe this image in one short sentence.";
const VISION_TEST_ICON_PNG: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../icons/mHD_256.png"));

// ── Re-exports (kept for backward compatibility and sub-module use) ──
pub use crate::config::editor_head_tune;
pub use crate::config::editor_hittest::hit_test_settings;
pub use crate::config::editor_layout::{
    COMBO_HIT_HEIGHT, COMBO_POPUP_ITEM_HEIGHT, COMBO_POPUP_MAX_VISIBLE, FONT_BODY_SIZE,
    FONT_SMALL_SIZE, FONT_TITLE_SIZE, Layout, SECTION_HEADER_HEIGHT_BASE, WIN_HEIGHT_BASE,
    WIN_WIDTH_BASE, WM_MOUSELEAVE, WM_PARAM_EDIT_COMMIT, compute_layout,
};
pub use crate::config::editor_paint::{
    build_advanced_controls, build_general_controls, build_llm_proxy_controls,
    build_llm_trim_controls, build_shortcuts_controls, paint_page,
};
pub use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};
pub use crate::config::editor_state::{
    ButtonStyle, HEAD_SWEEP, HeadGroup, ParamEditCreateInfo, ProxyEditField, SettingsHit,
    SettingsPage, SettingsState, UIBinding, UiProvider, head_help_text,
};
pub use crate::config::editor_theme::draw_rounded_border_in_buffer;
pub use crate::config::editor_theme::{draw_button, draw_rounded_rect_in_buffer, to_utf16_z};

// Additional editor_layout items needed by the sub-modules. Keep these
// crate-visible until the split stabilises, then prefer explicit imports.
pub(crate) use crate::config::editor_layout::{
    EDITOR_ACTION_NAMES, ID_ACTION_BASE, editor_action_desc, editor_index_for_action_name,
};

// ── Public entry point ──────────────────────────────────────────────
pub use window::show_config_editor;

// ── Cross-module items ──────────────────────────────────────────────
pub(crate) use dropdowns::{
    close_combo_popup, close_kind_popup, draw_free_target_dropdown, draw_head_dropdown,
    draw_theme_dropdown, draw_vision_model_dropdown, toggle_combo_popup,
};
pub(crate) use events::{combo_popup_wndproc, param_edit_popup_wndproc, settings_wndproc};
pub(crate) use persistence::{apply_settings, load_ui_bindings};
pub(crate) use reducer::{
    cancel_inline_edit, finish_inline_edit, handle_list_click, open_kind_menu, select_free_target,
    select_head, select_vision_model, spawn_inline_edit,
};
pub(crate) use vision::run_vision_test;
pub(crate) use window::{
    browse_for_folder, build_theme_list, create_font, page_control_content_height, paint_settings,
};

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
        for (i, action_name) in EDITOR_ACTION_NAMES.iter().enumerate() {
            assert_eq!(editor_action_desc(i).name, *action_name);
        }
    }

    #[test]
    fn unknown_editor_action_falls_back_to_quit() {
        let idx = editor_index_for_action_name("does_not_exist");
        assert_eq!(editor_action_desc(idx).name, "quit");
    }
}
