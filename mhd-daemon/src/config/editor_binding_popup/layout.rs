//! Geometry and layout helpers for the binding editor popup.
//!
//! Owns the popup's layout constants (window size, header/footer, row and
//! field metrics) plus the pure helpers that compute the key-combo slot
//! rectangles and the visible-row counts for the dropdowns.

use windows::Win32::Foundation::RECT;

use crate::config::editor_key_combo::KeyComboSlot;

// ── Layout constants ───────────────────────────────────────────────────

pub(crate) const POPUP_WIDTH_BASE: i32 = 640;
pub(crate) const POPUP_HEIGHT_BASE: i32 = 380;
pub(crate) const POPUP_HEADER_HEIGHT_BASE: i32 = 48;
pub(crate) const POPUP_FOOTER_HEIGHT_BASE: i32 = 52;
pub(crate) const POPUP_RADIUS_BASE: f32 = 12.0;
pub(crate) const POPUP_PADDING: i32 = 20;
pub(crate) const POPUP_ROW_HEIGHT: i32 = 32;
pub(crate) const POPUP_LABEL_WIDTH: i32 = 80;
pub(crate) const POPUP_FIELD_HEIGHT: i32 = 30;
pub(crate) const POPUP_BIND_BUTTON_WIDTH: i32 = 76;

// ── Layout helpers ─────────────────────────────────────────────────────

/// Number of visible rows in the action (kind) dropdown.
pub(crate) fn action_dropdown_visible_rows() -> usize {
    8
}

/// Number of visible rows in the key-combo dropdowns.
pub(crate) fn key_dropdown_visible_rows() -> usize {
    8
}

/// Compute the four trigger/key slots within a field rect.
///
/// The first three slots are modifier slots (`Modifier(0..3)`), the last one
/// is the key slot. Returns `(slot, rect)` pairs in left-to-right order.
pub(crate) fn trigger_slot_rects(field_rect: RECT, scale: f32) -> Vec<(KeyComboSlot, RECT)> {
    let gap = (6.0 * scale) as i32;
    let plus_w = (10.0 * scale) as i32;
    let total_plus = plus_w * 3;
    let total_gap = gap * 6;
    let slot_w = ((field_rect.right - field_rect.left - total_plus - total_gap) / 4).max(32);
    let mut x = field_rect.left;
    let mut rects = Vec::with_capacity(4);
    for i in 0..4 {
        let slot = if i < 3 {
            KeyComboSlot::Modifier(i)
        } else {
            KeyComboSlot::Key
        };
        let left = x;
        let right = if i == 3 {
            field_rect.right
        } else {
            left + slot_w
        };
        rects.push((
            slot,
            RECT {
                left,
                top: field_rect.top,
                right,
                bottom: field_rect.bottom,
            },
        ));
        x = right + plus_w + gap * 2;
    }
    rects
}
