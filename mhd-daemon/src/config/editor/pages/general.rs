//! General page layout helpers.

use crate::config::editor_layout::{Layout, SECTION_GAP_BASE, SECTION_HEADER_HEIGHT_BASE};
use crate::config::editor_state::SettingsState;

/// Estimated total content height for the General page.
pub fn content_height(_state: &SettingsState, lay: &Layout) -> i32 {
    // Match the exact layout from build_general_controls.
    let row_h = (28.0 * lay.scale()) as i32;
    let section_gap = (SECTION_GAP_BASE as f32 * lay.scale()) as i32;
    let keycast_divider_y = lay.general.draw_path_y + row_h + section_gap / 2;
    let keycast_header_y = keycast_divider_y + section_gap / 2;
    let keycast_y = keycast_header_y + (SECTION_HEADER_HEIGHT_BASE as f32 * lay.scale()) as i32;
    let pos_gap = (6.0 * lay.scale()) as i32;
    let duration_y = keycast_y + (row_h + pos_gap) * 2 + section_gap / 2;
    // Typing block controls (3 rows: toggle, width, duration)
    let typing_start_y = duration_y + row_h + section_gap;
    let last_y = typing_start_y + 3 * row_h + pos_gap;
    last_y + (20.0 * lay.scale()) as i32 - lay.content_y()
}
