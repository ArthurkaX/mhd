//! Hit‑testing for the settings editor.
//!
//! Determines which UI element is under the cursor, given the current
//! editor state and layout geometry.
//!
//! Footer buttons and the tab bar are handled by direct geometry checks.
//! All page‑specific controls are resolved via the [`HitRegion`] list that
//! `paint_*` stores in `state.hit_regions`.

use crate::config::editor_state::{SettingsHit, SettingsState};

// ── Central dispatch ──────────────────────────────────────────────

/// Central hit‑test for the settings window.
///
/// Footer and tab‑bar areas are checked first via direct layout geometry.
/// Everything else (all page controls) is resolved by a **back‑to‑front**
/// linear scan of `state.hit_regions`.  Controls are pushed in draw order
/// (background first, foreground on top), so scanning from the end finds
/// the topmost (last‑drawn) interactive element under the cursor — the one
/// the user actually sees and clicks.
pub fn hit_test_settings(state: &SettingsState, x: i32, y: i32) -> SettingsHit {
    let lay = &state.layout;

    // ── Footer buttons (always accessible) ──────────────────────
    if y >= lay.btn_y() && y < lay.btn_y() + lay.btn_h() {
        if x >= lay.apply_x() && x < lay.apply_x() + lay.btn_w() {
            return SettingsHit::ApplyBtn;
        }
        if x >= lay.close_x() && x < lay.close_x() + lay.btn_w() {
            return SettingsHit::CloseBtn;
        }
    }

    // ── Tab bar inside header (right-aligned) ──────────
    if y >= lay.tab_bar_y() && y < lay.tab_bar_y() + lay.tab_h() {
        let n = state.tab_titles.len() as i32;
        let tab_total_w = n * lay.tab_w() + (n - 1) * lay.tab_gap();
        let tab_start_x = lay.win_w() - lay.pad() - tab_total_w;
        for i in 0..n {
            let tx = tab_start_x + i * (lay.tab_w() + lay.tab_gap());
            if x >= tx && x < tx + lay.tab_w() {
                return SettingsHit::Tab(i as usize);
            }
        }
    }

    // ── Page-specific controls (resolved via region list) ────────
    // Controls are pushed in draw order (back-to-front).  Scanning
    // backwards finds the topmost (last-drawn) control first.
    for region in state.hit_regions.iter().rev() {
        if region.contains(x, y) {
            return region.hit;
        }
    }
    SettingsHit::None
}
