//! Hit‑testing and input dispatch for Vision Snip.
//!
//! Matches the toolbar layout defined in [`paint`].

use crate::overlays::vision_snip::model::VisionSnipModel;
use crate::overlays::vision_snip::paint::{
    self, BTN_GAP, BTN_H, BTN_W, NUM_LEFT_ACTIONS, NUM_RIGHT_ACTIONS, NUM_SWATCHES, NUM_TOOL_BTNS,
    SEP_W, SWATCH_GAP, SWATCH_SIZE, TOOL_H, TOOL_Y, ToolbarAction,
};

// ── Hit‑testing ────────────────────────────────────────────────────────

/// Hit‑test the toolbar at the given client coordinates.
///
/// Returns `Some(ToolbarAction)` if a toolbar button was clicked,
/// or `None` if the click fell outside any button.
pub fn hit_test(model: &VisionSnipModel, x: i32, y: i32, scale: f32) -> Option<ToolbarAction> {
    let sc = |v: i32| (v as f32 * scale) as i32;

    let tw = paint::toolbar_width(scale);
    let w = model.monitor_width as i32;
    let tx = (w - tw) / 2;
    let ty = sc(TOOL_Y);
    let th = sc(TOOL_H);

    // Quick reject – outside toolbar vertical band
    if y < ty || y >= ty + th {
        return None;
    }

    let tbh = sc(BTN_H);
    let tby = ty + (th - tbh) / 2;

    // Quick reject – outside toolbar vertical button band
    if y < tby || y >= tby + tbh {
        return None;
    }

    let gap = sc(BTN_GAP);
    let sep = sc(SEP_W);

    let mut left = tx + gap;

    // ── Tool buttons (Crop, Marker, Arrow, Rectangle) ────────────────
    let bw = sc(BTN_W);
    for i in 0..NUM_TOOL_BTNS {
        if x >= left && x < left + bw {
            return Some(match i {
                paint::IDX_CROP => ToolbarAction::CropTool,
                paint::IDX_MARKER => ToolbarAction::MarkerTool,
                paint::IDX_ARROW => ToolbarAction::ArrowTool,
                paint::IDX_RECTANGLE => ToolbarAction::RectangleTool,
                _ => ToolbarAction::CropReset,
            });
        }
        left += bw + gap;
    }

    left += sep;

    // ── Color swatches ──────────────────────────────────────────────
    let sw = sc(SWATCH_SIZE);
    let sw_gap = sc(SWATCH_GAP);
    for ci in 0..NUM_SWATCHES {
        if x >= left && x < left + sw {
            return Some(ToolbarAction::SetColor(ci));
        }
        left += sw + sw_gap;
    }

    left += sep;

    // ── Left action buttons (Undo, Clear) ───────────────────────────
    for ai in 0..NUM_LEFT_ACTIONS {
        if x >= left && x < left + bw {
            return Some(match ai {
                paint::IDX_UNDO => ToolbarAction::Undo,
                paint::IDX_CLEAR => ToolbarAction::Clear,
                _ => ToolbarAction::Undo,
            });
        }
        left += bw + gap;
    }

    left += sep;

    // ── Right action buttons (Analyze, Close) ─────────────────────────
    for ai in 0..NUM_RIGHT_ACTIONS {
        if x >= left && x < left + bw {
            return Some(match ai {
                paint::IDX_ANALYZE => ToolbarAction::Analyze,
                paint::IDX_CLOSE => ToolbarAction::Close,
                _ => ToolbarAction::Close,
            });
        }
        left += bw + gap;
    }

    None
}
