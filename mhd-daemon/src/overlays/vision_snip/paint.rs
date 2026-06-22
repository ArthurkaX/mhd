//! Overlay composition for Vision Snip.
//!
//! Draws the frozen screenshot, dim overlay outside the crop rectangle,
//! annotation previews, and the toolbar onto a [`DibFrame`] for the
//! layered window.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::*;

use crate::overlays::vision_snip::model::{
    Annotation, AnnotationColor, AnnotationGeometry, Point, Rect, Tool, VisionSnipModel,
};
use crate::renderer::{DibFrame, to_utf16_z};
use crate::win32::screen_capture::CapturedImage;

// ── Toolbar constants ──────────────────────────────────────────────────

pub const TOOL_H: i32 = 40;
pub const TOOL_Y: i32 = 8;
pub const BTN_H: i32 = 28;
pub const BTN_W: i32 = 36;
pub const BTN_GAP: i32 = 4;
pub const SWATCH_SIZE: i32 = 18;
pub const SWATCH_GAP: i32 = 4;
pub const SEP_W: i32 = 6;
pub const BADGE_R: i32 = 18;
pub const DIM_ALPHA: u8 = 100;

/// Background alpha for the toolbar panel.
const TOOLBAR_BG_ALPHA: u8 = 180;

/// The six annotation colours in display order.
pub const SWATCH_COLORS: [AnnotationColor; 6] = AnnotationColor::ALL;

// ── Toolbar button indices (for hit testing) ───────────────────────────

/// The number of tool buttons before the first separator.
pub const NUM_TOOL_BTNS: usize = 4;
/// Total swatches.
pub const NUM_SWATCHES: usize = 6;
/// Action buttons before the final separator (Undo, Clear).
pub const NUM_LEFT_ACTIONS: usize = 2;
/// Action buttons after the final separator (Analyze, Close).
pub const NUM_RIGHT_ACTIONS: usize = 2;

// Tool button indices
pub const IDX_CROP: usize = 0;
pub const IDX_MARKER: usize = 1;
pub const IDX_ARROW: usize = 2;
pub const IDX_RECTANGLE: usize = 3;

// Left action indices (after swatches separator)
pub const IDX_UNDO: usize = 0;
pub const IDX_CLEAR: usize = 1;

// Right action indices (after final separator)
pub const IDX_ANALYZE: usize = 0;
pub const IDX_CLOSE: usize = 1;

// ── ToolbarAction ──────────────────────────────────────────────────────

/// Actions that can be triggered by clicking the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    CropTool,
    MarkerTool,
    ArrowTool,
    RectangleTool,
    SetColor(usize),
    Undo,
    Clear,
    Analyze,
    Close,
    CropReset,
}

// ── Main paint entry point ─────────────────────────────────────────────

/// Compose the full overlay frame.
///
/// * `frame`        – target DIB (sized to the monitor).
/// * `model`        – current model state (crop, annotations, tool…).
/// * `screenshot`   – captured monitor pixels (RGBA bytes).
/// * `scale`        – DPI scaling factor.
/// * `hovered_btn`  – optional toolbar button index for hover highlight.
pub fn paint_overlay(
    frame: &mut DibFrame,
    model: &VisionSnipModel,
    screenshot: &CapturedImage,
    scale: f32,
    hovered_btn: Option<usize>,
) {
    let w = frame.width();
    let h = frame.height();
    let pixels = frame.pixels_mut();

    // 1. Copy screenshot into the DIB (converting RGBA → BGRA u32).
    copy_screenshot(pixels, w, h, screenshot);

    // 2. Dim the area outside the crop rectangle.
    dim_outside_crop(pixels, w, h, &model.crop);

    // 3. Draw crop border.
    draw_crop_border(pixels, w, h, &model.crop, scale);

    // 4. Draw committed annotations.
    for ann in &model.annotations {
        render_annotation(pixels, w, h, ann, scale);
    }

    // 5. If there is a pending annotation, draw it as a rubber-band preview.
    if let Some(ref pending) = model.pending {
        render_geometry(pixels, w, h, &pending.geometry, &pending.color, scale);
    }

    // 6. Draw the toolbar on top.
    draw_toolbar(frame, model, scale, hovered_btn);
}

// ── Screenshot copy ────────────────────────────────────────────────────

fn copy_screenshot(pixels: &mut [u32], w: i32, _h: i32, screenshot: &CapturedImage) {
    let sw = screenshot.width as i32;
    let sh = screenshot.height as i32;
    let copy_w = w.min(sw);
    let copy_h = (_h).min(sh);

    for y in 0..copy_h {
        for x in 0..copy_w {
            let src_idx = (y as u32 * screenshot.width + x as u32) as usize * 4;
            if src_idx + 4 <= screenshot.rgba.len() {
                let r = screenshot.rgba[src_idx];
                let g = screenshot.rgba[src_idx + 1];
                let b = screenshot.rgba[src_idx + 2];
                let a = screenshot.rgba[src_idx + 3];
                // BGRA u32 for the DIB (little-endian: B, G, R, A as bytes)
                let bg = (a as u32) << 24 | (b as u32) << 16 | (g as u32) << 8 | r as u32;
                let dst_idx = (y * w + x) as usize;
                if dst_idx < pixels.len() {
                    pixels[dst_idx] = bg;
                }
            }
        }
    }
}

// ── Dimming ────────────────────────────────────────────────────────────

fn dim_outside_crop(pixels: &mut [u32], w: i32, h: i32, crop: &Rect) {
    let left = crop.left.max(0);
    let top = crop.top.max(0);
    let right = crop.right.min(w);
    let bottom = crop.bottom.min(h);

    // Top strip
    for y in 0..top.min(h) {
        for x in 0..w {
            if x < w {
                darken_pixel(pixels, y, w, x);
            }
        }
    }
    // Bottom strip
    for y in bottom.max(0)..h {
        for x in 0..w {
            if x < w {
                darken_pixel(pixels, y, w, x);
            }
        }
    }
    // Left strip (middle section, not overlapping top/bottom)
    for y in top.max(0)..bottom.min(h) {
        for x in 0..left.min(w) {
            if x < w {
                darken_pixel(pixels, y, w, x);
            }
        }
        // Right strip
        for x in right.max(0)..w {
            if x < w {
                darken_pixel(pixels, y, w, x);
            }
        }
    }
}

#[inline]
fn darken_pixel(pixels: &mut [u32], y: i32, w: i32, x: i32) {
    let idx = (y * w + x) as usize;
    if idx >= pixels.len() {
        return;
    }
    let px = pixels[idx];
    let r = ((px >> 16) & 0xFF) as u32;
    let g = ((px >> 8) & 0xFF) as u32;
    let b = (px & 0xFF) as u32;
    let a = (px >> 24) & 0xFF;
    // 50% darken: divide each channel by 2
    let nr = r / 2;
    let ng = g / 2;
    let nb = b / 2;
    pixels[idx] = (a << 24) | (nr << 16) | (ng << 8) | nb;
}

// ── Crop border ────────────────────────────────────────────────────────

fn draw_crop_border(pixels: &mut [u32], w: i32, h: i32, crop: &Rect, _scale: f32) {
    let left = crop.left.max(0);
    let top = crop.top.max(0);
    let right = crop.right.min(w);
    let bottom = crop.bottom.min(h);

    if right <= left || bottom <= top {
        return;
    }

    let color: u32 = 0xFFFFFFFF; // White, full alpha
    let dash: u32 = 6;

    // Top edge
    let mut i = 0;
    for x in left..right {
        if i % (dash * 2) < dash {
            set_pixel_raw(pixels, w, h, x, top, color);
            set_pixel_raw(pixels, w, h, x, top + 1, color);
        }
        i += 1;
    }
    // Bottom edge
    i = 0;
    for x in left..right {
        if i % (dash * 2) < dash {
            set_pixel_raw(pixels, w, h, x, bottom, color);
            set_pixel_raw(pixels, w, h, x, bottom - 1, color);
        }
        i += 1;
    }
    // Left edge
    i = 0;
    for y in top..bottom {
        if i % (dash * 2) < dash {
            set_pixel_raw(pixels, w, h, left, y, color);
            set_pixel_raw(pixels, w, h, left + 1, y, color);
        }
        i += 1;
    }
    // Right edge
    i = 0;
    for y in top..bottom {
        if i % (dash * 2) < dash {
            set_pixel_raw(pixels, w, h, right, y, color);
            set_pixel_raw(pixels, w, h, right - 1, y, color);
        }
        i += 1;
    }
}

// ── Annotation rendering on the overlay ────────────────────────────────

/// Render a single committed annotation onto the DIB.
fn render_annotation(pixels: &mut [u32], w: i32, h: i32, ann: &Annotation, scale: f32) {
    render_geometry(pixels, w, h, &ann.geometry, &ann.color, scale);
}

/// Render an annotation geometry (committed or pending).
fn render_geometry(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    geometry: &AnnotationGeometry,
    color: &AnnotationColor,
    scale: f32,
) {
    match geometry {
        AnnotationGeometry::Point {
            anchor,
            badge_origin,
        } => {
            render_point(pixels, w, h, anchor, badge_origin, color, scale);
        }
        AnnotationGeometry::Arrow { from, to } => {
            render_arrow(pixels, w, h, from, to, color, scale);
        }
        AnnotationGeometry::Rectangle { rect } => {
            render_rect(pixels, w, h, rect, color, scale);
        }
    }
}

fn render_point(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    anchor: &Point,
    badge_origin: &Point,
    color: &AnnotationColor,
    _scale: f32,
) {
    let (cr, cg, cb, _) = color.rgba();
    let main_col = argb_u32(255, cr, cg, cb);
    let outline_col = argb_u32(200, 0, 0, 0);

    // Anchor dot
    fill_circle(pixels, w, h, anchor.x, anchor.y, 4, outline_col);
    fill_circle(pixels, w, h, anchor.x, anchor.y, 3, main_col);

    // Leader line
    draw_bresenham_line(
        pixels,
        w,
        h,
        anchor.x,
        anchor.y,
        badge_origin.x,
        badge_origin.y,
        argb_u32(180, 0, 0, 0),
    );

    // Badge outline
    fill_circle(
        pixels,
        w,
        h,
        badge_origin.x,
        badge_origin.y,
        BADGE_R + 1,
        outline_col,
    );
    fill_circle(
        pixels,
        w,
        h,
        badge_origin.x,
        badge_origin.y,
        BADGE_R,
        main_col,
    );
}

fn render_arrow(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    from: &Point,
    to: &Point,
    color: &AnnotationColor,
    _scale: f32,
) {
    let (cr, cg, cb, _) = color.rgba();
    let main_col = argb_u32(255, cr, cg, cb);
    let outline_col = argb_u32(200, 0, 0, 0);

    // Thick line
    draw_thick_line(pixels, w, h, from.x, from.y, to.x, to.y, main_col, 3);

    // Arrowhead at `to`
    let dx = (to.x - from.x) as f64;
    let dy = (to.y - from.y) as f64;
    let len = dx.hypot(dy);
    if len > 1.0 {
        let ux = dx / len;
        let uy = dy / len;
        let head_len = 12.0;
        let spread = 0.4;
        let lx = to.x - (ux * head_len - uy * head_len * spread) as i32;
        let ly = to.y - (uy * head_len + ux * head_len * spread) as i32;
        let rx = to.x - (ux * head_len + uy * head_len * spread) as i32;
        let ry = to.y - (uy * head_len - ux * head_len * spread) as i32;

        draw_thick_line(pixels, w, h, to.x, to.y, lx, ly, outline_col, 2);
        draw_thick_line(pixels, w, h, to.x, to.y, rx, ry, outline_col, 2);
        draw_thick_line(pixels, w, h, lx, ly, rx, ry, outline_col, 2);
    }

    // Badge near tail
    let bx = from.x - 10;
    let by = from.y - 22;
    fill_circle(pixels, w, h, bx + 10, by + 14, 15, outline_col);
    fill_circle(pixels, w, h, bx + 10, by + 14, 14, main_col);
}

fn render_rect(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    rect: &Rect,
    color: &AnnotationColor,
    _scale: f32,
) {
    let (cr, cg, cb, _) = color.rgba();
    let main_col = argb_u32(255, cr, cg, cb);
    let outline_col = argb_u32(200, 0, 0, 0);

    // Outline border (3px)
    draw_rect_outline(pixels, w, h, rect, outline_col, 5);
    draw_rect_outline(pixels, w, h, rect, main_col, 3);

    // Badge at top-left
    let bx = rect.left - 8;
    let by = rect.top - 30;
    fill_circle(pixels, w, h, bx + 10, by + 14, 15, outline_col);
    fill_circle(pixels, w, h, bx + 10, by + 14, 14, main_col);
}

// ── Drawing primitives ─────────────────────────────────────────────────

#[inline]
fn argb_u32(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

#[inline]
fn set_pixel_raw(pixels: &mut [u32], w: i32, _h: i32, x: i32, y: i32, color: u32) {
    if x < 0 || x >= w || y < 0 {
        return;
    }
    let idx = (y * w + x) as usize;
    if idx < pixels.len() {
        pixels[idx] = color;
    }
}

fn fill_circle(pixels: &mut [u32], w: i32, h: i32, cx: i32, cy: i32, radius: i32, color: u32) {
    let r2 = radius * radius;
    for y in (cy - radius).max(0)..=(cy + radius).min(h - 1) {
        for x in (cx - radius).max(0)..=(cx + radius).min(w - 1) {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy <= r2 {
                let idx = (y * w + x) as usize;
                if idx < pixels.len() {
                    pixels[idx] = color;
                }
            }
        }
    }
}

fn draw_bresenham_line(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) {
    let mut x = x0;
    let mut y = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        set_pixel_raw(pixels, w, h, x, y, color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_thick_line(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
    thickness: i32,
) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    let half = thickness / 2;

    loop {
        for ty in (y - half)..=(y + half) {
            for tx in (x - half)..=(x + half) {
                set_pixel_raw(pixels, w, h, tx, ty, color);
            }
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_rect_outline(pixels: &mut [u32], w: i32, h: i32, rect: &Rect, color: u32, thickness: i32) {
    // Top
    draw_thick_line(
        pixels, w, h, rect.left, rect.top, rect.right, rect.top, color, thickness,
    );
    // Bottom
    draw_thick_line(
        pixels,
        w,
        h,
        rect.left,
        rect.bottom,
        rect.right,
        rect.bottom,
        color,
        thickness,
    );
    // Left
    draw_thick_line(
        pixels,
        w,
        h,
        rect.left,
        rect.top,
        rect.left,
        rect.bottom,
        color,
        thickness,
    );
    // Right
    draw_thick_line(
        pixels,
        w,
        h,
        rect.right,
        rect.top,
        rect.right,
        rect.bottom,
        color,
        thickness,
    );
}

// ── Toolbar drawing ────────────────────────────────────────────────────

/// Draw the toolbar into the DIB using GDI calls on the memory DC.
fn draw_toolbar(
    frame: &mut DibFrame,
    model: &VisionSnipModel,
    scale: f32,
    hovered_btn: Option<usize>,
) {
    let w = frame.width();
    let h = frame.height();
    let dc = frame.dc();

    let sc = |v: i32| (v as f32 * scale) as i32;

    let tw = toolbar_width(scale);
    let tx = (w - tw) / 2;
    let ty = sc(TOOL_Y);
    let th = sc(TOOL_H);
    let tbh = sc(BTN_H);
    let tby = ty + (th - tbh) / 2;
    let gap = sc(BTN_GAP);
    let sep = sc(SEP_W);

    // ── Toolbar background ───────────────────────────────────────────
    {
        let bg_color = argb_u32(TOOLBAR_BG_ALPHA, 0x11, 0x11, 0x11);
        // Fill the toolbar rect directly on the pixel buffer
        let pixels = frame.pixels_mut();
        for y in ty..(ty + th).min(h) {
            for x in tx..(tx + tw).min(w) {
                let idx = (y * w + x) as usize;
                if idx < pixels.len() {
                    pixels[idx] = bg_color;
                }
            }
        }
    }

    // ── Font for button labels ───────────────────────────────────────
    let font = crate::renderer::create_font(-(13.0 * scale) as i32, false, "Segoe UI");
    let _old_font = unsafe { SelectObject(dc, font) };
    unsafe {
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0x00CCCCCCu32));
    }

    let mut btn_index: usize = 0;
    let mut left = tx + gap;

    // Helper to record a button region and advance
    let mut add_tool =
        |label: &str, btn_w: i32, active: bool, left: &mut i32, btn_index: &mut usize| {
            let idx = *btn_index;
            let is_hovered = hovered_btn == Some(idx);
            *btn_index += 1;

            let cur_left = *left;

            if is_hovered {
                unsafe {
                    let hbr = CreateSolidBrush(COLORREF(0x33FFFFFFu32));
                    let rc = RECT {
                        left: cur_left,
                        top: tby,
                        right: cur_left + btn_w,
                        bottom: tby + tbh,
                    };
                    let _ = FillRect(dc, &rc, hbr);
                    let _ = DeleteObject(hbr);
                }
            } else if active {
                unsafe {
                    let hbr = CreateSolidBrush(COLORREF(0x00B49664u32));
                    let rc = RECT {
                        left: cur_left,
                        top: tby,
                        right: cur_left + btn_w,
                        bottom: tby + tbh,
                    };
                    let _ = FillRect(dc, &rc, hbr);
                    let _ = DeleteObject(hbr);
                }
            }

            if active || is_hovered {
                unsafe {
                    let _ = SetTextColor(dc, COLORREF(0x00FFFFFFu32));
                }
            } else {
                unsafe {
                    let _ = SetTextColor(dc, COLORREF(0x00AAAAAAu32));
                }
            }

            let mut text = to_utf16_z(label);
            let mut rc = RECT {
                left: cur_left,
                top: tby,
                right: cur_left + btn_w,
                bottom: tby + tbh,
            };
            unsafe {
                let _ = DrawTextW(
                    dc,
                    &mut text,
                    &mut rc as *mut RECT,
                    DT_CENTER | DT_SINGLELINE | DT_VCENTER,
                );
            }

            *left += btn_w + gap;
        };

    // ── Tool buttons (Crop, Marker, Arrow, Rectangle) ────────────────
    let tool_btns = [
        (IDX_CROP, "Crop", Tool::Crop),
        (IDX_MARKER, "M", Tool::Marker),
        (IDX_ARROW, "→", Tool::Arrow),
        (IDX_RECTANGLE, "□", Tool::Rectangle),
    ];
    for &(_idx, label, t) in &tool_btns {
        let active = model.active_tool == t;
        add_tool(label, sc(BTN_W), active, &mut left, &mut btn_index);
    }

    left += sep;

    // ── Color swatches ──────────────────────────────────────────────
    let sw = sc(SWATCH_SIZE);
    let sw_gap = sc(SWATCH_GAP);
    let cby = tby + (tbh - sw) / 2;

    for ci in 0..NUM_SWATCHES {
        let color = &SWATCH_COLORS[ci];
        let (cr, cg, cb, _) = color.rgba();
        let colorref = COLORREF((cr as u32) | ((cg as u32) << 8) | ((cb as u32) << 16));

        let is_selected = model.selected_color == *color;
        let is_hovered = hovered_btn == Some(btn_index);
        btn_index += 1;

        // Fill swatch
        unsafe {
            let hbr = CreateSolidBrush(colorref);
            let rc = RECT {
                left,
                top: cby,
                right: left + sw,
                bottom: cby + sw,
            };
            let _ = FillRect(dc, &rc, hbr);
            let _ = DeleteObject(hbr);
        }

        // Border: white if selected, dark gray otherwise
        let border = if is_selected {
            COLORREF(0x00FFFFFFu32)
        } else if is_hovered {
            COLORREF(0x00CCCCCCu32)
        } else {
            COLORREF(0x00666666u32)
        };
        unsafe {
            let hbr = CreateSolidBrush(border);
            let rc = RECT {
                left,
                top: cby,
                right: left + sw,
                bottom: cby + 1,
            };
            let _ = FillRect(dc, &rc, hbr);
            let rc = RECT {
                left,
                top: cby + sw - 1,
                right: left + sw,
                bottom: cby + sw,
            };
            let _ = FillRect(dc, &rc, hbr);
            let rc = RECT {
                left,
                top: cby,
                right: left + 1,
                bottom: cby + sw,
            };
            let _ = FillRect(dc, &rc, hbr);
            let rc = RECT {
                left: left + sw - 1,
                top: cby,
                right: left + sw,
                bottom: cby + sw,
            };
            let _ = FillRect(dc, &rc, hbr);
            let _ = DeleteObject(hbr);
        }

        left += sw + sw_gap;
    }

    left += sep;

    // Reset text color
    unsafe {
        let _ = SetTextColor(dc, COLORREF(0x00DDDDDDu32));
    }

    // ── Left action buttons (Undo, Clear) ───────────────────────────
    add_tool("↶", sc(BTN_W), false, &mut left, &mut btn_index); // Undo
    add_tool("✕", sc(BTN_W), false, &mut left, &mut btn_index); // Clear (different from Close)

    left += sep;

    // ── Right action buttons (Analyze, Close) ───────────────────────
    add_tool("▶", sc(BTN_W), false, &mut left, &mut btn_index); // Analyze
    unsafe {
        let _ = SetTextColor(dc, COLORREF(0x00FF6655u32));
    }
    add_tool("✕", sc(BTN_W), false, &mut left, &mut btn_index); // Close

    // ── Force alpha on toolbar pixels for hit-testing ────────────────
    {
        let pixels = frame.pixels_mut();
        for y in ty..(ty + th).min(h) {
            for x in tx..(tx + tw).min(w) {
                let idx = (y * w + x) as usize;
                if idx < pixels.len() {
                    let p = pixels[idx];
                    if (p >> 24) < 255 {
                        pixels[idx] = p | 0xFF000000;
                    }
                }
            }
        }
    }

    // Cleanup font
    unsafe {
        let _ = SelectObject(dc, _old_font);
        let _ = DeleteObject(font);
    }
}

/// Compute the full toolbar width at the given scale.
pub fn toolbar_width(scale: f32) -> i32 {
    let sc = |v: i32| (v as f32 * scale) as i32;
    let gap = sc(BTN_GAP);
    let sep = sc(SEP_W);
    let bw = sc(BTN_W);
    let sw = sc(SWATCH_SIZE);
    let sw_gap = sc(SWATCH_GAP);

    let mut w = gap; // left margin

    // Tool buttons
    w += bw * 4 + gap * 3; // 4 buttons, 3 gaps between them
    w += gap; // gap after last tool button

    w += sep;

    w += gap; // gap after separator
    // Color swatches
    w += sw * 6 + sw_gap * 5; // 6 swatches, 5 gaps
    w += sw_gap; // gap after last swatch

    w += sep;

    w += gap;
    // Left actions (Undo, Clear)
    w += bw * 2 + gap;
    w += gap;

    w += sep;

    w += gap;
    // Right actions (Analyze, Close)
    w += bw * 2 + gap;
    w += gap; // right margin

    w
}
