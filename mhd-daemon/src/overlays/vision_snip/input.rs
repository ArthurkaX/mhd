//! Hit‑testing and input dispatch for Vision Snip.
//!
//! Matches the toolbar layout defined in [`paint`] and provides the
//! [`MouseAction`] enum for mouse event handling.

use crate::overlays::vision_snip::model::{AnnotationGeometry, DragState, Point, VisionSnipModel};
use crate::overlays::vision_snip::paint::{
    self, BTN_GAP, BTN_H, BTN_W, NUM_LEFT_ACTIONS, NUM_RIGHT_ACTIONS, NUM_SWATCHES, NUM_TOOL_BTNS,
    SEP_W, SWATCH_GAP, SWATCH_SIZE, TOOL_H, TOOL_Y, ToolbarAction,
};

// ── MouseAction ────────────────────────────────────────────────────────

/// What to do in response to a mouse event.
#[derive(Debug, Clone)]
pub enum MouseAction {
    /// No action.
    None,
    /// Start a drag operation (crop, arrow, rectangle).
    StartDrag(DragState),
    /// Update an active drag with the current cursor position.
    UpdateDrag(Point),
    /// End an active drag, returning the resulting geometry if valid.
    EndDrag(Option<AnnotationGeometry>),
    /// Clicked a toolbar button.
    ClickToolbar(ToolbarAction),
    /// Clicked on or near an existing annotation label badge.
    ClickAnnotation(char),
}

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
    let mut btn_idx: usize = 0;

    // ── Tool buttons (Crop, Marker, Arrow, Rectangle) ────────────────
    let bw = sc(BTN_W);
    for _ in 0..NUM_TOOL_BTNS {
        if x >= left && x < left + bw {
            return Some(match btn_idx {
                paint::IDX_CROP => ToolbarAction::CropTool,
                paint::IDX_MARKER => ToolbarAction::MarkerTool,
                paint::IDX_ARROW => ToolbarAction::ArrowTool,
                paint::IDX_RECTANGLE => ToolbarAction::RectangleTool,
                _ => ToolbarAction::CropReset,
            });
        }
        left += bw + gap;
        btn_idx += 1;
    }

    left += sep;

    // ── Color swatches ──────────────────────────────────────────────
    let sw = sc(SWATCH_SIZE);
    let sw_gap = sc(SWATCH_GAP);
    // Center swatches in the button row
    let _cby = tby + (tbh - sw) / 2;
    for ci in 0..NUM_SWATCHES {
        if x >= left && x < left + sw {
            return Some(ToolbarAction::SetColor(ci));
        }
        left += sw + sw_gap;
        btn_idx += 1;
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
        btn_idx += 1;
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
        btn_idx += 1;
    }

    None
}

/// Hit‑test specifically for color swatches.
///
/// Returns the swatch index (0..NUM_SWATCHES) if the point hits one,
/// or `None` otherwise.
pub fn hit_test_color(x: i32, y: i32, scale: f32, monitor_width: i32) -> Option<usize> {
    let sc = |v: i32| (v as f32 * scale) as i32;

    let tw = paint::toolbar_width(scale);
    let tx = (monitor_width - tw) / 2;
    let ty = sc(TOOL_Y);
    let th = sc(TOOL_H);
    let tbh = sc(BTN_H);
    let tby = ty + (th - tbh) / 2;

    // Quick reject – outside toolbar vertical band
    if y < ty || y >= ty + th || y < tby || y >= tby + tbh {
        return None;
    }

    let gap = sc(BTN_GAP);
    let sep = sc(SEP_W);
    let bw = sc(BTN_W);
    let sw = sc(SWATCH_SIZE);
    let sw_gap = sc(SWATCH_GAP);

    // Skip tool buttons
    let mut left = tx + gap + (bw + gap) * 4;
    left += sep;

    // Swatch zone
    for ci in 0..NUM_SWATCHES {
        if x >= left && x < left + sw {
            return Some(ci);
        }
        left += sw + sw_gap;
    }

    None
}
