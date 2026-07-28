//! Shared annotation rasterization for Vision Snip.
//!
//! Both the live overlay (a layered-window DIB) and the final PNG render the
//! exact same annotation geometry and letter badges.  To avoid two diverging
//! copies of the drawing code (which previously caused the label and
//! glyph-rotation bugs to need fixing twice), all primitives and annotation
//! renderers live here and are generic over a [`Canvas`].
//!
//! Each target implements [`Canvas`] with its own pixel encoding and blending
//! conventions; the geometry, badge sizing, and glyphs are defined once.

use crate::overlays::vision_snip::model::{
    Annotation, AnnotationColor, AnnotationGeometry, Point, Rect,
};

/// Radius of the capital-letter badge circle, in physical pixels.
pub const BADGE_R: i32 = 15;

/// A pixel target that annotation drawing can render into.
///
/// Coordinates are in the target's own pixel space (origin top-left).  Out-of
/// bounds writes must be ignored.  `blend` applies one source colour at a
/// pixel; how it combines with the existing pixel (overwrite vs alpha blend)
/// is the implementor's choice.
pub trait Canvas {
    fn blend(&mut self, x: i32, y: i32, rgba: (u8, u8, u8, u8));
}

// ── Annotation renderers ────────────────────────────────────────────────

/// Render a committed annotation: geometry, badge circle, and letter.
pub fn render_annotation<C: Canvas>(c: &mut C, ann: &Annotation) {
    render_geometry(c, &ann.geometry, ann.color);
    let center = badge_center(&ann.geometry);
    let (tr, tg, tb, _) = ann.color.text_rgba();
    draw_label(c, center.x, center.y, ann.label, (tr, tg, tb, 255));
}

/// Render annotation geometry and its badge circle, without a letter.
///
/// Used for the rubber-band preview of a pending annotation (no label yet).
pub fn render_geometry<C: Canvas>(c: &mut C, geom: &AnnotationGeometry, color: AnnotationColor) {
    match geom {
        AnnotationGeometry::Point {
            anchor,
            badge_origin,
        } => render_point(c, *anchor, *badge_origin, color),
        AnnotationGeometry::Arrow { from, to } => render_arrow(c, *from, *to, color),
        AnnotationGeometry::Rectangle { rect } => render_rect(c, *rect, color),
    }
}

/// The badge centre for each geometry, matching the circle drawn by the
/// `render_*` helpers.  Used for label placement and badge hit-testing.
pub fn badge_center(geom: &AnnotationGeometry) -> Point {
    match geom {
        AnnotationGeometry::Point { badge_origin, .. } => *badge_origin,
        AnnotationGeometry::Arrow { from, .. } => Point {
            x: from.x,
            y: from.y - 8,
        },
        AnnotationGeometry::Rectangle { rect } => Point {
            x: rect.left,
            y: rect.top - 16,
        },
    }
}

fn render_point<C: Canvas>(c: &mut C, anchor: Point, badge_origin: Point, color: AnnotationColor) {
    let (cr, cg, cb, _) = color.rgba();
    let fill = (cr, cg, cb, 255);

    // Anchor dot with dark outline.
    draw_filled_circle(c, anchor.x, anchor.y, 4, (0, 0, 0, 255));
    draw_filled_circle(c, anchor.x, anchor.y, 3, fill);

    // Leader line to the badge.
    draw_line(
        c,
        anchor.x,
        anchor.y,
        badge_origin.x,
        badge_origin.y,
        (0, 0, 0, 180),
        1,
    );

    // Badge circle with dark outline.
    draw_filled_circle(
        c,
        badge_origin.x,
        badge_origin.y,
        BADGE_R + 1,
        (0, 0, 0, 200),
    );
    draw_filled_circle(c, badge_origin.x, badge_origin.y, BADGE_R, fill);
}

fn render_arrow<C: Canvas>(c: &mut C, from: Point, to: Point, color: AnnotationColor) {
    let (cr, cg, cb, _) = color.rgba();
    let fill = (cr, cg, cb, 255);

    // Shaft: dark outline then colour for contrast.
    draw_line(c, from.x, from.y, to.x, to.y, (0, 0, 0, 200), 5);
    draw_line(c, from.x, from.y, to.x, to.y, fill, 3);

    // Arrowhead at `to`.
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = ((dx * dx + dy * dy) as f64).sqrt();
    if len > 0.0 {
        let ux = dx as f64 / len;
        let uy = dy as f64 / len;
        let ah = 10.0;
        let p1x = to.x - (ux * ah - uy * ah * 0.5) as i32;
        let p1y = to.y - (uy * ah + ux * ah * 0.5) as i32;
        let p2x = to.x - (ux * ah + uy * ah * 0.5) as i32;
        let p2y = to.y - (uy * ah - ux * ah * 0.5) as i32;

        draw_line(c, to.x, to.y, p1x, p1y, (0, 0, 0, 200), 3);
        draw_line(c, to.x, to.y, p2x, p2y, (0, 0, 0, 200), 3);
        let ah_color = (cr / 2 + 64, cg / 2 + 64, cb / 2 + 64, 255);
        draw_line(c, to.x, to.y, p1x, p1y, ah_color, 1);
        draw_line(c, to.x, to.y, p2x, p2y, ah_color, 1);
        draw_line(c, p1x, p1y, p2x, p2y, (0, 0, 0, 200), 2);
    }

    // Badge near the tail.
    let bc = badge_center(&AnnotationGeometry::Arrow { from, to });
    draw_filled_circle(c, bc.x, bc.y, BADGE_R + 1, (0, 0, 0, 200));
    draw_filled_circle(c, bc.x, bc.y, BADGE_R, fill);
}

fn render_rect<C: Canvas>(c: &mut C, rect: Rect, color: AnnotationColor) {
    let (cr, cg, cb, _) = color.rgba();
    let fill = (cr, cg, cb, 255);

    // Border: dark outline then colour.
    draw_rect(c, rect, (0, 0, 0, 200), 5);
    draw_rect(c, rect, fill, 3);

    // Badge at top-left.
    let bc = badge_center(&AnnotationGeometry::Rectangle { rect });
    draw_filled_circle(c, bc.x, bc.y, BADGE_R + 1, (0, 0, 0, 200));
    draw_filled_circle(c, bc.x, bc.y, BADGE_R, fill);
}

// ── Primitives ──────────────────────────────────────────────────────────

/// Draw a thick line via Bresenham, stamping a `thickness`×`thickness` box.
pub fn draw_line<C: Canvas>(
    c: &mut C,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: (u8, u8, u8, u8),
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
                c.blend(tx, ty, color);
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

/// Draw the border of a rectangle with the given thickness.
fn draw_rect<C: Canvas>(c: &mut C, rect: Rect, color: (u8, u8, u8, u8), thickness: i32) {
    for x in rect.left..=rect.right {
        for t in 0..thickness {
            c.blend(x, rect.top + t, color);
            c.blend(x, rect.bottom - t, color);
        }
    }
    for y in rect.top + thickness..=rect.bottom - thickness {
        for t in 0..thickness {
            c.blend(rect.left + t, y, color);
            c.blend(rect.right - t, y, color);
        }
    }
}

fn draw_filled_circle<C: Canvas>(
    c: &mut C,
    cx: i32,
    cy: i32,
    radius: i32,
    color: (u8, u8, u8, u8),
) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                c.blend(cx + dx, cy + dy, color);
            }
        }
    }
}

/// Draw a label glyph centred at `(cx, cy)`.
fn draw_label<C: Canvas>(c: &mut C, cx: i32, cy: i32, ch: char, color: (u8, u8, u8, u8)) {
    const SIZE: i32 = 12;
    let scale = (SIZE / 5).max(1);
    let gw = 5 * scale;
    let gh = 5 * scale;
    draw_char(c, cx - gw / 2, cy - gh / 2, ch, color, SIZE);
}

/// Draw a single character with its top-left at `(x, y)`.
fn draw_char<C: Canvas>(c: &mut C, x: i32, y: i32, ch: char, color: (u8, u8, u8, u8), size: i32) {
    let glyph = match glyph5x7(ch) {
        Some(g) => g,
        None => return,
    };
    let scale = (size / 5).max(1);
    // Glyph is 5 rows of 5 bits each; bit 4 (0b10000) is the leftmost column.
    for row in 0..5usize {
        let bits = glyph[row];
        for col in 0..5usize {
            if (bits & (1 << (4 - col))) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        c.blend(
                            x + col as i32 * scale + sx,
                            y + row as i32 * scale + sy,
                            color,
                        );
                    }
                }
            }
        }
    }
}

/// The 5×5 bitmap glyph for an uppercase letter or digit (bit 4 = leftmost).
fn glyph5x7(ch: char) -> Option<[u8; 5]> {
    let idx = if ch.is_ascii_uppercase() {
        (ch as u8 - b'A') as usize
    } else if ch.is_ascii_digit() {
        (ch as u8 - b'0') as usize + 26
    } else {
        return None;
    };

    const FONT: [[u8; 5]; 36] = [
        // A-Z
        [0b01110, 0b10001, 0b11111, 0b10001, 0b10001], // A
        [0b11110, 0b10001, 0b11110, 0b10001, 0b11110], // B
        [0b01110, 0b10001, 0b10000, 0b10001, 0b01110], // C
        [0b11110, 0b10001, 0b10001, 0b10001, 0b11110], // D
        [0b11111, 0b10000, 0b11110, 0b10000, 0b11111], // E
        [0b11111, 0b10000, 0b11110, 0b10000, 0b10000], // F
        [0b01110, 0b10001, 0b10111, 0b10001, 0b01110], // G
        [0b10001, 0b10001, 0b11111, 0b10001, 0b10001], // H
        [0b01110, 0b00100, 0b00100, 0b00100, 0b01110], // I
        [0b00111, 0b00010, 0b00010, 0b10010, 0b01100], // J
        [0b10001, 0b10010, 0b11100, 0b10010, 0b10001], // K
        [0b10000, 0b10000, 0b10000, 0b10000, 0b11111], // L
        [0b10001, 0b11011, 0b10101, 0b10001, 0b10001], // M
        [0b10001, 0b11001, 0b10101, 0b10011, 0b10001], // N
        [0b01110, 0b10001, 0b10001, 0b10001, 0b01110], // O
        [0b11110, 0b10001, 0b11110, 0b10000, 0b10000], // P
        [0b01110, 0b10001, 0b10101, 0b10010, 0b01101], // Q
        [0b11110, 0b10001, 0b11110, 0b10010, 0b10001], // R
        [0b01110, 0b10000, 0b01110, 0b00001, 0b01110], // S
        [0b11111, 0b00100, 0b00100, 0b00100, 0b00100], // T
        [0b10001, 0b10001, 0b10001, 0b10001, 0b01110], // U
        [0b10001, 0b10001, 0b10001, 0b01010, 0b00100], // V
        [0b10001, 0b10001, 0b10101, 0b11011, 0b10001], // W
        [0b10001, 0b01010, 0b00100, 0b01010, 0b10001], // X
        [0b10001, 0b01010, 0b00100, 0b00100, 0b00100], // Y
        [0b11111, 0b00010, 0b00100, 0b01000, 0b11111], // Z
        // 0-9
        [0b01110, 0b10011, 0b10101, 0b11001, 0b01110], // 0
        [0b00100, 0b01100, 0b00100, 0b00100, 0b01110], // 1
        [0b01110, 0b00001, 0b00110, 0b01000, 0b11111], // 2
        [0b01110, 0b00001, 0b00110, 0b00001, 0b01110], // 3
        [0b00010, 0b00110, 0b01010, 0b11111, 0b00010], // 4
        [0b11111, 0b10000, 0b11110, 0b00001, 0b11110], // 5
        [0b01110, 0b10000, 0b11110, 0b10001, 0b01110], // 6
        [0b11111, 0b00001, 0b00010, 0b00100, 0b00100], // 7
        [0b01110, 0b10001, 0b01110, 0b10001, 0b01110], // 8
        [0b01110, 0b10001, 0b01111, 0b00001, 0b01110], // 9
    ];

    FONT.get(idx).copied()
}
