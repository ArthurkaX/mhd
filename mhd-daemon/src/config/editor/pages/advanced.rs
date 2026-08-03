//! Advanced page layout helpers.

use crate::config::editor_layout::{ADVANCED_BUTTONS, Layout};
use crate::config::editor_state::SettingsState;

/// Estimated total content height for the Advanced page.
pub fn content_height(_state: &SettingsState, lay: &Layout) -> i32 {
    let btn_count = ADVANCED_BUTTONS.len() as i32;
    let last_y = lay.advanced.top_y + btn_count * (lay.advanced.btn_h + lay.advanced.btn_gap);
    last_y + (16.0 * lay.scale()) as i32 - lay.content_y()
}
