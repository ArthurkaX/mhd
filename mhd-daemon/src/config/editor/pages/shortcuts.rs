//! Shortcuts page layout helpers.

use crate::config::editor_layout::Layout;
use crate::config::editor_state::SettingsState;

/// Estimated total content height for the Shortcuts page.
pub fn content_height(state: &SettingsState, lay: &Layout) -> i32 {
    // Dynamic: binding rows + accordion + add button.
    let n = state.bindings.len() as i32;
    let accordion_h = if state.expanded_idx.is_some() {
        lay.accordion_h()
    } else {
        0
    };
    let last_y = lay.list_y() + n * lay.row_h() + accordion_h + lay.row_h();
    last_y + (16.0 * lay.scale()) as i32 - lay.content_y()
}
