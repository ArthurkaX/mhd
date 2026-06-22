//! Final image construction for Vision Snip.
//!
//! Crops the source screenshot, renders annotations (borders, arrows, badges)
//! directly onto the pixel buffer, and builds the structured prompt metadata.
//!
//! This module has no Win32 dependencies and can be fully unit tested
//! (pixel-level assertions).

use crate::overlays::vision_snip::model::{Annotation, AnnotationGeometry, Point, Rect};
use crate::win32::screen_capture::CapturedImage;

/// Crop a `CapturedImage` to the given rect.
///
/// `crop` is in image-local coordinates (top-left origin).
/// The output dimensions match the crop dimensions.
pub fn crop_image(image: &CapturedImage, crop: Rect) -> Result<CapturedImage, String> {
    let cw = crop.width() as usize;
    let ch = crop.height() as usize;

    if cw == 0 || ch == 0 {
        return Err("Crop has zero dimensions".to_string());
    }

    let src_w = image.width as i32;
    let src_h = image.height as i32;

    // Clamp crop to image bounds
    let left = crop.left.max(0).min(src_w - 1);
    let top = crop.top.max(0).min(src_h - 1);
    let right = crop.right.max(left + 1).min(src_w);
    let bottom = crop.bottom.max(top + 1).min(src_h);

    let actual_w = (right - left) as usize;
    let actual_h = (bottom - top) as usize;

    let mut rgba = Vec::with_capacity(actual_w * actual_h * 4);

    for y in top..bottom {
        for x in left..right {
            let src_idx = (y as u32 * image.width + x as u32) as usize * 4;
            if src_idx + 4 <= image.rgba.len() {
                rgba.extend_from_slice(&image.rgba[src_idx..src_idx + 4]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }

    Ok(CapturedImage {
        width: actual_w as u32,
        height: actual_h as u32,
        rgba,
    })
}

/// Render annotations onto the image buffer in-place.
///
/// `annotations` must already be in crop-local coordinates.
/// Rendering order: arrows, rectangles, then points (so points are on top).
pub fn render_annotations(
    image: &mut CapturedImage,
    annotations: &[Annotation],
) -> Result<(), String> {
    let w = image.width as i32;
    let h = image.height as i32;

    // Render in order: arrows, rectangles, points
    let mut sorted = annotations.iter().enumerate().collect::<Vec<_>>();
    sorted.sort_by_key(|(_, a)| match a.geometry {
        AnnotationGeometry::Arrow { .. } => 0,
        AnnotationGeometry::Rectangle { .. } => 1,
        AnnotationGeometry::Point { .. } => 2,
    });

    for (_, ann) in &sorted {
        match &ann.geometry {
            AnnotationGeometry::Point {
                anchor,
                badge_origin,
            } => {
                render_point_badge(image, w, h, ann, *anchor, *badge_origin);
            }
            AnnotationGeometry::Arrow { from, to } => {
                render_arrow(image, w, h, ann, *from, *to);
            }
            AnnotationGeometry::Rectangle { rect } => {
                render_rect_border(image, w, h, ann, *rect);
            }
        }
    }

    Ok(())
}

// ── Drawing helpers ─────────────────────────────────────────────────────

fn set_pixel(
    image: &mut CapturedImage,
    w: i32,
    _h: i32,
    x: i32,
    y: i32,
    r: u8,
    g: u8,
    b: u8,
    a: u8,
) {
    if x < 0 || x >= w || y < 0 || y >= image.height as i32 {
        return;
    }
    let idx = (y as u32 * image.width + x as u32) as usize * 4;
    if idx + 4 <= image.rgba.len() {
        if a == 255 {
            image.rgba[idx] = r;
            image.rgba[idx + 1] = g;
            image.rgba[idx + 2] = b;
            image.rgba[idx + 3] = 255;
        } else {
            // Alpha blend
            let src_a = a as u32;
            let dst_a = image.rgba[idx + 3] as u32;
            let out_a = src_a + (dst_a * (255 - src_a as u32)) / 255;
            if out_a > 0 {
                image.rgba[idx] =
                    ((r as u32 * src_a + image.rgba[idx] as u32 * (255 - src_a)) / 255) as u8;
                image.rgba[idx + 1] =
                    ((g as u32 * src_a + image.rgba[idx + 1] as u32 * (255 - src_a)) / 255) as u8;
                image.rgba[idx + 2] =
                    ((b as u32 * src_a + image.rgba[idx + 2] as u32 * (255 - src_a)) / 255) as u8;
            }
            image.rgba[idx + 3] = out_a.min(255) as u8;
        }
    }
}

fn set_pixel_rgba(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    rgba: (u8, u8, u8, u8),
) {
    set_pixel(image, w, h, x, y, rgba.0, rgba.1, rgba.2, rgba.3);
}

fn draw_line(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
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

    loop {
        for ty in y - thickness / 2..=y + thickness / 2 {
            for tx in x - thickness / 2..=x + thickness / 2 {
                set_pixel_rgba(image, w, h, tx, ty, color);
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

fn draw_rect(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    rect: Rect,
    color: (u8, u8, u8, u8),
    thickness: i32,
) {
    // Top edge
    for x in rect.left..=rect.right {
        for t in 0..thickness {
            set_pixel_rgba(image, w, h, x, rect.top + t, color);
            set_pixel_rgba(image, w, h, x, rect.bottom - t, color);
        }
    }
    // Left and right edges (excluding corners already drawn)
    for y in rect.top + thickness..=rect.bottom - thickness {
        for t in 0..thickness {
            set_pixel_rgba(image, w, h, rect.left + t, y, color);
            set_pixel_rgba(image, w, h, rect.right - t, y, color);
        }
    }
}

fn draw_filled_circle(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    cx: i32,
    cy: i32,
    radius: i32,
    color: (u8, u8, u8, u8),
) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                set_pixel_rgba(image, w, h, cx + dx, cy + dy, color);
            }
        }
    }
}

fn draw_char(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    ch: char,
    color: (u8, u8, u8, u8),
    size: i32,
) {
    // Simple 5x7 bitmap font for A-Z and 0-9
    let idx = if ch >= 'A' && ch <= 'Z' {
        (ch as u8 - b'A') as usize
    } else if ch >= '0' && ch <= '9' {
        (ch as u8 - b'0' + 26) as usize
    } else {
        return;
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

    if idx >= FONT.len() {
        return;
    }

    let glyph = FONT[idx];
    let scale = size.max(3) / 5; // scale factor, minimum 1

    for col in 0..5usize {
        let mut mask = glyph[col];
        for row in 0..7usize {
            if (mask & 0b0100000) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        set_pixel_rgba(
                            image,
                            w,
                            h,
                            x + col as i32 * scale + sx,
                            y + row as i32 * scale + sy,
                            color,
                        );
                    }
                }
            }
            mask <<= 1;
        }
    }
}

/// Render a point marker badge.
fn render_point_badge(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    ann: &Annotation,
    anchor: Point,
    badge_origin: Point,
) {
    let (cr, cg, cb, _) = ann.color.rgba();
    let color = (cr, cg, cb, 255);
    let (tr, tg, tb, _) = ann.color.text_rgba();
    let text_color = (tr, tg, tb, 255);
    let badge_r = 16;

    // Draw anchor dot
    draw_filled_circle(image, w, h, anchor.x, anchor.y, 4, (0, 0, 0, 255));
    draw_filled_circle(image, w, h, anchor.x, anchor.y, 3, color);

    // Draw leader line from anchor to badge
    draw_line(
        image,
        w,
        h,
        anchor.x,
        anchor.y,
        badge_origin.x,
        badge_origin.y,
        (0, 0, 0, 180),
        1,
    );

    // Draw badge circle with dark outline
    let outline_color = (0, 0, 0, 200);
    draw_filled_circle(
        image,
        w,
        h,
        badge_origin.x,
        badge_origin.y,
        badge_r + 1,
        outline_color,
    );
    draw_filled_circle(image, w, h, badge_origin.x, badge_origin.y, badge_r, color);

    // Draw label letter
    let label_str = ann.label.to_string();
    if let Some(ch) = label_str.chars().next() {
        draw_char(
            image,
            w,
            h,
            badge_origin.x - 7,
            badge_origin.y - 10,
            ch,
            text_color,
            12,
        );
    }
}

/// Render an arrow annotation.
fn render_arrow(
    image: &mut CapturedImage,
    w: i32,
    h: i32,
    ann: &Annotation,
    from: Point,
    to: Point,
) {
    let (cr, cg, cb, _) = ann.color.rgba();
    let color = (cr, cg, cb, 255);
    let (tr, tg, tb, _) = ann.color.text_rgba();
    let text_color = (tr, tg, tb, 255);
    let thickness = 3;

    // Draw the line with outline for contrast
    draw_line(
        image,
        w,
        h,
        from.x,
        from.y,
        to.x,
        to.y,
        (0, 0, 0, 200),
        thickness + 2,
    );
    draw_line(image, w, h, from.x, from.y, to.x, to.y, color, thickness);

    // Draw arrowhead at `to`
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = ((dx * dx + dy * dy) as f64).sqrt();
    if len > 0.0 {
        let ux = dx as f64 / len;
        let uy = dy as f64 / len;
        let ah_size = 10.0;

        let p1x = to.x - (ux * ah_size - uy * ah_size * 0.5) as i32;
        let p1y = to.y - (uy * ah_size + ux * ah_size * 0.5) as i32;
        let p2x = to.x - (ux * ah_size + uy * ah_size * 0.5) as i32;
        let p2y = to.y - (uy * ah_size - ux * ah_size * 0.5) as i32;

        // Fill arrowhead
        let ah_color = (cr / 2 + 64, cg / 2 + 64, cb / 2 + 64, 255);
        draw_line(image, w, h, to.x, to.y, p1x, p1y, (0, 0, 0, 200), 3);
        draw_line(image, w, h, to.x, to.y, p2x, p2y, (0, 0, 0, 200), 3);
        draw_line(image, w, h, to.x, to.y, p1x, p1y, ah_color, 1);
        draw_line(image, w, h, to.x, to.y, p2x, p2y, ah_color, 1);
        draw_line(image, w, h, p1x, p1y, p2x, p2y, (0, 0, 0, 200), 2);
    }

    // Draw label badge near the tail
    let badge_x = from.x - 8;
    let badge_y = from.y - 20;
    draw_filled_circle(image, w, h, badge_x + 8, badge_y + 12, 14, (0, 0, 0, 200));
    draw_filled_circle(image, w, h, badge_x + 8, badge_y + 12, 13, color);
    let label_str = ann.label.to_string();
    if let Some(ch) = label_str.chars().next() {
        draw_char(image, w, h, badge_x + 2, badge_y + 3, ch, text_color, 12);
    }
}

/// Render a rectangle annotation border.
fn render_rect_border(image: &mut CapturedImage, w: i32, h: i32, ann: &Annotation, rect: Rect) {
    let (cr, cg, cb, _) = ann.color.rgba();
    let color = (cr, cg, cb, 255);
    let (tr, tg, tb, _) = ann.color.text_rgba();
    let text_color = (tr, tg, tb, 255);
    let thickness = 3;

    // Draw outline border
    draw_rect(image, w, h, rect, (0, 0, 0, 200), thickness + 2);
    draw_rect(image, w, h, rect, color, thickness);

    // Draw label badge at top-left
    let badge_x = rect.left - 8;
    let badge_y = rect.top - 28;
    draw_filled_circle(image, w, h, badge_x + 8, badge_y + 12, 14, (0, 0, 0, 200));
    draw_filled_circle(image, w, h, badge_x + 8, badge_y + 12, 13, color);
    let label_str = ann.label.to_string();
    if let Some(ch) = label_str.chars().next() {
        draw_char(image, w, h, badge_x + 2, badge_y + 3, ch, text_color, 12);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlays::vision_snip::model::{AnnotationColor, AnnotationGeometry};

    fn make_test_image(w: u32, h: u32, fill: u8) -> CapturedImage {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.push(fill);
            rgba.push(fill);
            rgba.push(fill);
            rgba.push(255);
        }
        CapturedImage {
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn test_crop_full_image() {
        let img = make_test_image(100, 100, 128);
        let crop = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let cropped = crop_image(&img, crop).unwrap();
        assert_eq!(cropped.width, 100);
        assert_eq!(cropped.height, 100);
    }

    #[test]
    fn test_crop_subregion() {
        let img = make_test_image(100, 100, 128);
        let crop = Rect {
            left: 10,
            top: 10,
            right: 50,
            bottom: 50,
        };
        let cropped = crop_image(&img, crop).unwrap();
        assert_eq!(cropped.width, 40);
        assert_eq!(cropped.height, 40);
    }

    #[test]
    fn test_crop_zero_size() {
        let img = make_test_image(100, 100, 128);
        let crop = Rect {
            left: 50,
            top: 50,
            right: 50,
            bottom: 50,
        };
        assert!(crop_image(&img, crop).is_err());
    }

    #[test]
    fn test_render_point_marker() {
        let mut img = make_test_image(200, 200, 200);
        let ann = Annotation {
            label: 'A',
            geometry: AnnotationGeometry::Point {
                anchor: Point { x: 100, y: 100 },
                badge_origin: Point { x: 120, y: 80 },
            },
            color: AnnotationColor::Red,
            description: "test".into(),
        };
        render_annotations(&mut img, &[ann]).unwrap();
        // Check that some pixels were modified around the badge
        let idx = (80 * 200 + 120) as usize * 4;
        assert!(img.rgba[idx] > 210 || img.rgba[idx] < 200); // Red badge pixel
    }

    #[test]
    fn test_render_clips_at_edges() {
        let mut img = make_test_image(50, 50, 200);
        let ann = Annotation {
            label: 'A',
            geometry: AnnotationGeometry::Point {
                anchor: Point { x: 0, y: 0 },
                badge_origin: Point { x: 0, y: 0 },
            },
            color: AnnotationColor::Blue,
            description: "corner".into(),
        };
        // Should not panic
        render_annotations(&mut img, &[ann]).unwrap();
    }
}
