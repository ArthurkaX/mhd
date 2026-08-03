//! LLM Proxy page layout helpers.

use crate::config::editor_layout::Layout;
use crate::config::editor_state::SettingsState;

/// Estimated total content height for the LLM Proxy page.
pub fn content_height(state: &SettingsState, lay: &Layout) -> i32 {
    let n = state.providers.len() as i32;
    let row_h = lay.provider_row_h();
    let section_h = (20.0 * lay.scale()) as i32;
    let gap = (6.0 * lay.scale()) as i32;
    let col_h = (16.0 * lay.scale()) as i32;
    let table_header_h = col_h + gap;
    // Providers section (last): section header + table headers + rows + add button
    let providers_end =
        lay.llm_proxy.providers_header_y + section_h + table_header_h + n * row_h + row_h;
    (providers_end + (16.0 * lay.scale()) as i32 - lay.content_y()).max(
        lay.llm_proxy.providers_header_y + section_h + table_header_h + row_h - lay.content_y(),
    )
}
