//! LLM Trim page layout helpers.

use crate::config::editor_layout::Layout;
use crate::config::editor_state::SettingsState;

/// Estimated total content height for the LLM Trim page.
pub fn content_height(_state: &SettingsState, lay: &Layout) -> i32 {
    lay.llm_trim.free_y + 2 * lay.llm_trim.row_h - lay.content_y()
}
