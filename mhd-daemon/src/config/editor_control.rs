//! Control enum and hit-region model for the settings editor.
//!
//! Each [`Control`] variant carries everything needed to draw itself —
//! its `rect`, pre-resolved colors/states, and an optional `SettingsHit`
//! for interactive elements.  A page's controls are built by the
//! `build_*_controls` functions in `editor_paint`; the caller renders
//! them via `paint_control` and derives hit regions from controls that
//! carry a non-`None` hit.

use windows::Win32::Foundation::RECT;

use crate::config::editor_state::{ButtonStyle, SettingsHit};
use crate::core::native_theme::Argb;

/// Which of the three editor fonts to use when rendering a control.
#[derive(Clone, Copy)]
pub enum FontChoice {
    Title,
    Body,
    Small,
}

/// One drawable control on a settings page.
///
/// Each variant stores its bounding rect and all visual data needed for a
/// single `paint_control` call.  Interactive variants carry a `hit` field;
/// the hit-region list is derived from those controls.
pub enum Control {
    /// Section header (left-aligned, single-line, not vertically centred).
    Header {
        rect: RECT,
        label: String,
        font: FontChoice,
        text_color: Argb,
    },

    /// Plain label (left-aligned, single-line, vertically centred).
    Label {
        rect: RECT,
        label: String,
        font: FontChoice,
        text_color: Argb,
    },

    /// Horizontal 1 px divider line.
    Divider { rect: RECT },

    /// Collapsed combo box with dropdown arrow at the right edge.
    Combo {
        rect: RECT,
        arrow_width: i32,
        selected: String,
        font: FontChoice,
        bg_color: Argb,
        border_color: Argb,
        text_color: Argb,
        arrow_color: Argb,
        hit: SettingsHit,
    },

    /// Pill-shaped toggle switch (track + knob).
    Toggle {
        rect: RECT,
        is_on: bool,
        is_hovered: bool,
        hit: SettingsHit,
    },

    /// Single-line read-only text field (rounded background + border + text).
    TextField {
        rect: RECT,
        text: String,
        placeholder: Option<String>,
        font: FontChoice,
        bg_color: Argb,
        border_color: Argb,
        text_color: Argb,
        hit: SettingsHit,
    },

    /// A button rendered via `draw_button`.
    Button {
        rect: RECT,
        label: String,
        font: FontChoice,
        is_hovered: bool,
        style: ButtonStyle,
        hit: SettingsHit,
    },

    // ── Shortcuts‑page controls ─────────────────────────────────

    /// A binding‑row body (background rounded rect + trigger/action/param
    /// text).  The delete‑X button is a separate `Button` pushed immediately
    /// after so it wins the first‑match region scan.
    BindingRow {
        rect: RECT,
        trigger: String,
        kind_label: String,
        param: String,
        is_selected: bool,
        is_hovered: bool,
        zebra: bool,
        hit: SettingsHit,
    },

    /// Accordion editor panel background + border.
    AccordionPanel {
        rect: RECT,
        bg_color: Argb,
        border_color: Argb,
        radius: i32,
    },

    /// A rounded field inside the accordion (trigger or param).
    AccordionField {
        rect: RECT,
        text: String,
        font: FontChoice,
        bg_color: Argb,
        text_color: Argb,
        hit: SettingsHit,
    },

    /// Help / placeholder / error text inside the accordion (no background).
    AccordionHelp {
        rect: RECT,
        text: String,
        text_color: Argb,
    },

    /// Scrollbar track + thumb.
    Scrollbar {
        rect: RECT,         // hit‑test area
        track_rect: RECT,   // visual track
        thumb_rect: RECT,   // visual thumb
        thumb_color: Argb,
        hit: SettingsHit,
    },

    // ── Meta‑controls (clipping) ─────────────────────────────

    /// Begin a clipped rendering region.  All subsequent controls until
    /// [`Control::ClipEnd`] are clipped to `rect`.
    ClipStart { rect: RECT },
    /// End the clipped region opened by the most recent [`Control::ClipStart`].
    ClipEnd,
}

impl Control {
    /// The bounding rectangle of this control (used for layout and hit‑test).
    pub fn rect(&self) -> RECT {
        match self {
            Control::Header { rect, .. }
            | Control::Label { rect, .. }
            | Control::Divider { rect, .. }
            | Control::Combo { rect, .. }
            | Control::Toggle { rect, .. }
            | Control::TextField { rect, .. }
            | Control::Button { rect, .. }
            | Control::BindingRow { rect, .. }
            | Control::AccordionPanel { rect, .. }
            | Control::AccordionField { rect, .. }
            | Control::AccordionHelp { rect, .. } => *rect,
            Control::Scrollbar { rect, .. } => *rect,
            Control::ClipStart { rect } => *rect,
            Control::ClipEnd => RECT { left: 0, top: 0, right: 0, bottom: 0 }, // never hit-tested
        }
    }

    /// The `SettingsHit` for interactive controls, or `None`.
    pub fn hit(&self) -> Option<SettingsHit> {
        match self {
            Control::Header { .. }
            | Control::Label { .. }
            | Control::Divider { .. }
            | Control::AccordionPanel { .. }
            | Control::AccordionHelp { .. } => None,
            Control::Combo { hit, .. }
            | Control::Toggle { hit, .. }
            | Control::TextField { hit, .. }
            | Control::Button { hit, .. }
            | Control::BindingRow { hit, .. }
            | Control::AccordionField { hit, .. }
            | Control::Scrollbar { hit, .. } => Some(*hit),
            Control::ClipStart { .. } | Control::ClipEnd => None,
        }
    }
}

/// One hit-test rectangle paired with the `SettingsHit` it resolves to.
#[derive(Clone, Copy)]
pub struct HitRegion {
    pub rect: RECT,
    pub hit: SettingsHit,
}

impl HitRegion {
    pub fn new(rect: RECT, hit: SettingsHit) -> Self {
        Self { rect, hit }
    }

    /// True if (x, y) falls inside this region.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.rect.left
            && x < self.rect.right
            && y >= self.rect.top
            && y < self.rect.bottom
    }
}

/// A page's worth of controls.
///
/// Built by `build_*_controls` functions, then rendered by `paint_control` /
/// `paint_page`.  Hit regions are derived from controls that carry a
/// non-`None` `SettingsHit`.
pub struct ControlList {
    pub controls: Vec<Control>,
}

impl ControlList {
    pub fn new() -> Self {
        Self {
            controls: Vec::new(),
        }
    }

    pub fn push(&mut self, c: Control) {
        self.controls.push(c);
    }

    /// Derive hit regions from every control that has an interactive hit,
    /// intersecting each region with the active clip rect (set by
    /// [`Control::ClipStart`] / [`Control::ClipEnd`]).  This ensures that
    /// controls (or parts of them) clipped out of view — e.g. a binding row
    /// whose scrolled rect extends above the list band — are not hittable,
    /// matching the old per‑page hit‑test guards.
    pub fn regions(&self) -> Vec<HitRegion> {
        let mut r = Vec::with_capacity(self.controls.len());
        // Stack of active clip rects.  Top of stack is the current clip;
        // `None` means no clipping (full window).
        let mut clip_stack: Vec<Option<RECT>> = vec![None];

        for c in &self.controls {
            match c {
                Control::ClipStart { rect } => {
                    let current = clip_stack.last().copied().unwrap_or(None);
                    let new_clip = match current {
                        None => Some(*rect),
                        Some(curr) => {
                            let left = curr.left.max(rect.left);
                            let top = curr.top.max(rect.top);
                            let right = curr.right.min(rect.right);
                            let bottom = curr.bottom.min(rect.bottom);
                            if left < right && top < bottom {
                                Some(RECT { left, top, right, bottom })
                            } else {
                                None // fully clipped away
                            }
                        }
                    };
                    clip_stack.push(new_clip);
                }
                Control::ClipEnd => {
                    // Restore previous clip state (never pop the initial None).
                    if clip_stack.len() > 1 {
                        clip_stack.pop();
                    }
                }
                _ => {
                    if let Some(hit) = c.hit() {
                        let mut cr = c.rect();
                        if let Some(clip) = clip_stack.last().copied().unwrap_or(None) {
                            // Intersect control rect with the active clip rect.
                            let left = cr.left.max(clip.left);
                            let top = cr.top.max(clip.top);
                            let right = cr.right.min(clip.right);
                            let bottom = cr.bottom.min(clip.bottom);
                            if left < right && top < bottom {
                                cr = RECT { left, top, right, bottom };
                                r.push(HitRegion::new(cr, hit));
                            }
                            // else: fully clipped → no region
                        } else {
                            r.push(HitRegion::new(cr, hit));
                        }
                    }
                }
            }
        }
        r
    }
}
