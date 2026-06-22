//! Final image construction for Vision Snip.
//!
//! Crops the source screenshot, renders annotations (borders, arrows, badges)
//! directly onto the pixel buffer, and builds the structured prompt metadata.
//!
//! This module has no Win32 dependencies and can be fully unit tested
//! (pixel-level assertions).

use crate::overlays::vision_snip::draw::{self, Canvas};
use crate::overlays::vision_snip::model::{Annotation, AnnotationGeometry, Rect};
use crate::win32::screen_capture::CapturedImage;

// ── Canvas over the cropped RGBA image ──────────────────────────────────

impl Canvas for CapturedImage {
    /// Alpha-blend a source colour over the destination RGBA pixel.
    fn blend(&mut self, x: i32, y: i32, (r, g, b, a): (u8, u8, u8, u8)) {
        if x < 0 || x >= self.width as i32 || y < 0 || y >= self.height as i32 {
            return;
        }
        let idx = (y as u32 * self.width + x as u32) as usize * 4;
        if idx + 4 > self.rgba.len() {
            return;
        }
        if a == 255 {
            self.rgba[idx] = r;
            self.rgba[idx + 1] = g;
            self.rgba[idx + 2] = b;
            self.rgba[idx + 3] = 255;
            return;
        }
        let src_a = a as u32;
        let dst_a = self.rgba[idx + 3] as u32;
        let out_a = src_a + (dst_a * (255 - src_a)) / 255;
        if out_a > 0 {
            self.rgba[idx] = ((r as u32 * src_a + self.rgba[idx] as u32 * (255 - src_a)) / 255) as u8;
            self.rgba[idx + 1] =
                ((g as u32 * src_a + self.rgba[idx + 1] as u32 * (255 - src_a)) / 255) as u8;
            self.rgba[idx + 2] =
                ((b as u32 * src_a + self.rgba[idx + 2] as u32 * (255 - src_a)) / 255) as u8;
        }
        self.rgba[idx + 3] = out_a.min(255) as u8;
    }
}

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
/// `annotations` must already be in crop-local coordinates.  Rendering order
/// is arrows, then rectangles, then points (so point badges land on top).
/// The actual drawing is shared with the live overlay via [`draw`].
pub fn render_annotations(
    image: &mut CapturedImage,
    annotations: &[Annotation],
) -> Result<(), String> {
    let mut sorted = annotations.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|a| match a.geometry {
        AnnotationGeometry::Arrow { .. } => 0,
        AnnotationGeometry::Rectangle { .. } => 1,
        AnnotationGeometry::Point { .. } => 2,
    });

    for ann in sorted {
        draw::render_annotation(image, ann);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlays::vision_snip::model::{AnnotationColor, AnnotationGeometry, Point};

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
