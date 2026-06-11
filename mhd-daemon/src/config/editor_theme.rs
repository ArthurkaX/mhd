//! Theme‑aware drawing helpers for the settings editor.
//!
//! Provides pixel‑buffer operations for rounded rects, buttons, and
//! text styling used by all editor pages.

use std::ffi::c_void;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;

use crate::config::editor_layout::WIN_WIDTH_BASE;
use crate::config::editor_state::ButtonStyle;
use crate::core::native_theme::{Argb, NativeTheme};

// ── Colour blending ───────────────────────────────────────────────

/// Blend a colour onto a destination pixel using premultiplied alpha.
fn blend_pixels_premultiplied(original: u32, overlay: Argb, opacity: f32) -> u32 {
    let overlay_a = (overlay.a as f32 * opacity) as u32;
    if overlay_a == 0 {
        return original;
    }

    let dest_a = (original >> 24) & 0xFF;
    let dest_r = (original >> 16) & 0xFF;
    let dest_g = (original >> 8) & 0xFF;
    let dest_b = original & 0xFF;

    let src_r = overlay.r as u32;
    let src_g = overlay.g as u32;
    let src_b = overlay.b as u32;

    let out_a = overlay_a + (dest_a * (255 - overlay_a) + 127) / 255;
    let out_r = (src_r * overlay_a + dest_r * (255 - overlay_a) + 127) / 255;
    let out_g = (src_g * overlay_a + dest_g * (255 - overlay_a) + 127) / 255;
    let out_b = (src_b * overlay_a + dest_b * (255 - overlay_a) + 127) / 255;

    (out_a.min(255) << 24) | (out_r.min(255) << 16) | (out_g.min(255) << 8) | out_b.min(255)
}

// ── Rounded rectangle fill ────────────────────────────────────────

/// Draw a filled rounded rectangle directly into a 32-bit pixel buffer.
pub fn draw_rounded_rect_in_buffer(
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    r: i32,
    color: Argb,
) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };

    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let cr = r.min(w / 2).min(h / 2);

    let tl_cx = rect.left + cr;
    let tl_cy = rect.top + cr;
    let tr_cx = rect.right - cr - 1;
    let tr_cy = rect.top + cr;
    let bl_cx = rect.left + cr;
    let bl_cy = rect.bottom - cr - 1;
    let br_cx = rect.right - cr - 1;
    let br_cy = rect.bottom - cr - 1;

    for y in y1..y2 {
        for x in x1..x2 {
            let (is_corner, cx, cy) = if x < rect.left + cr && y < rect.top + cr {
                (true, tl_cx, tl_cy)
            } else if x > tr_cx && y < rect.top + cr {
                (true, tr_cx, tr_cy)
            } else if x < rect.left + cr && y > bl_cy {
                (true, bl_cx, bl_cy)
            } else if x > br_cx && y > br_cy {
                (true, br_cx, br_cy)
            } else {
                (false, 0, 0)
            };

            let idx = (y * win_w + x) as usize;
            if idx >= pixels.len() {
                continue;
            }

            if is_corner {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                let dist = (dx * dx + dy * dy).sqrt();
                let falloff = 1.0 - (dist - cr as f32).clamp(0.0, 1.0);
                if falloff <= 0.0 {
                    // outside corner
                } else {
                    let original = pixels[idx];
                    pixels[idx] = blend_pixels_premultiplied(original, color, falloff);
                }
            } else {
                let original = pixels[idx];
                pixels[idx] = blend_pixels_premultiplied(original, color, 1.0);
            }
        }
    }
}

// ── Rounded border ────────────────────────────────────────────────

/// Draw an anti‑aliased rounded border into a 32-bit pixel buffer.
pub fn draw_rounded_border_in_buffer(
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    r: i32,
    border_width: i32,
    color: Argb,
) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };

    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);

    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return;
    }

    let cr = r.min(w / 2).min(h / 2);

    let tl_cx = rect.left + cr;
    let tl_cy = rect.top + cr;
    let tr_cx = rect.right - cr - 1;
    let tr_cy = rect.top + cr;
    let bl_cx = rect.left + cr;
    let bl_cy = rect.bottom - cr - 1;
    let br_cx = rect.right - cr - 1;
    let br_cy = rect.bottom - cr - 1;

    let bw = border_width as f32;

    for y in y1..y2 {
        for x in x1..x2 {
            let (is_corner, cx, cy) = if x < rect.left + cr && y < rect.top + cr {
                (true, tl_cx, tl_cy)
            } else if x > tr_cx && y < rect.top + cr {
                (true, tr_cx, tr_cy)
            } else if x < rect.left + cr && y > bl_cy {
                (true, bl_cx, bl_cy)
            } else if x > br_cx && y > br_cy {
                (true, br_cx, br_cy)
            } else {
                (false, 0, 0)
            };

            let idx = (y * win_w + x) as usize;
            if idx >= pixels.len() {
                continue;
            }

            let mut opacity = 0.0;

            if is_corner {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                let dist = (dx * dx + dy * dy).sqrt();
                let dist_from_outer = cr as f32 - dist;
                let dist_from_inner = dist - (cr as f32 - bw);
                let edge_opacity = dist_from_outer
                    .clamp(0.0, 1.0)
                    .min(dist_from_inner.clamp(0.0, 1.0));
                opacity = edge_opacity;
            } else {
                let dl = (x - rect.left) as f32;
                let dr = (rect.right - 1 - x) as f32;
                let dt = (y - rect.top) as f32;
                let db = (rect.bottom - 1 - y) as f32;
                let min_dist = dl.min(dr).min(dt).min(db);
                if min_dist < bw {
                    opacity = 1.0 - (bw - 1.0 - min_dist).clamp(0.0, 1.0);
                }
            }

            if opacity > 0.0 {
                let original = pixels[idx];
                pixels[idx] = blend_pixels_premultiplied(original, color, opacity);
            }
        }
    }
}

// ── Contrast helper ───────────────────────────────────────────────

/// Returns true if white text has sufficient contrast on this background.
fn contrast_text_on(bg: Argb) -> bool {
    let r = bg.r as f32 / 255.0;
    let g = bg.g as f32 / 255.0;
    let b = bg.b as f32 / 255.0;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    lum < 0.5
}

// ── Clear rect ────────────────────────────────────────────────────

/// Fill a rectangle with fully transparent pixels (alpha=0).
/// Used when an inline EDIT child window is active — the layered window
/// composites the child control only where the parent bitmap has zero alpha.
#[allow(dead_code)]
pub fn clear_rect_in_buffer(bits: *mut c_void, win_w: i32, win_h: i32, rect: RECT) {
    if bits.is_null() || win_w <= 0 || win_h <= 0 {
        return;
    }
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, (win_w * win_h) as usize) };
    let x1 = rect.left.clamp(0, win_w);
    let x2 = rect.right.clamp(0, win_w);
    let y1 = rect.top.clamp(0, win_h);
    let y2 = rect.bottom.clamp(0, win_h);
    for y in y1..y2 {
        let row_start = (y * win_w) as usize;
        for x in x1..x2 {
            let idx = row_start + x as usize;
            if idx < pixels.len() {
                pixels[idx] = 0; // ARGB: fully transparent
            }
        }
    }
}

// ── Button drawing ────────────────────────────────────────────────

/// Draw a rounded button or interactive plate on the DIB.
pub fn draw_button(
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
    theme: &NativeTheme,
    font: HFONT,
    is_hovered: bool,
    style: ButtonStyle,
) {
    let rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };

    let (btn_color, text_color, border_color) = match style {
        ButtonStyle::Primary => {
            let mut bg = theme.accent;
            if is_hovered {
                bg = theme.hover.blend_over(bg);
            }
            let fg = if contrast_text_on(bg) {
                Argb::new(255, 255, 255, 255)
            } else {
                Argb::new(255, 0, 0, 0)
            };
            (bg, fg, theme.border)
        }
        ButtonStyle::Secondary => {
            let mut bg = theme.surface.blend_over(theme.background);
            if is_hovered {
                bg = theme.hover.blend_over(bg);
            }
            let fg = bg.contrasting_text_color();
            (bg, fg, theme.border)
        }
        ButtonStyle::DangerGhost => {
            let mut bg = Argb::new(0, 0, 0, 0);
            let mut fg = theme.text_muted.with_alpha(255);
            if is_hovered {
                bg = Argb::new(40, 255, 0, 0);
                fg = Argb::new(255, 255, 80, 80);
            }
            (bg, fg, Argb::new(0, 0, 0, 0))
        }
        ButtonStyle::TriggerPlate => {
            let mut bg = theme.surface.blend_over(theme.background);
            let mut border = theme.border;
            if is_hovered {
                bg = theme.hover.blend_over(bg);
                border = theme.text;
            }
            let fg = bg.contrasting_text_color();
            (bg, fg, border)
        }
    };

    // Radius scaled based on DPI width
    let radius = (5.0 * (win_w as f32 / WIN_WIDTH_BASE as f32)) as i32;
    draw_rounded_rect_in_buffer(bits, win_w, win_h, rect, radius, btn_color);

    if border_color.a > 0 {
        draw_rounded_border_in_buffer(bits, win_w, win_h, rect, radius, 1, border_color);
    }

    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.with_alpha(255).to_colorref());
    }
    let mut lbl_wz = to_utf16_z(label);
    let mut lbl_rc = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut lbl_wz,
            &mut lbl_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

// ── UTF-16 helper ─────────────────────────────────────────────────

/// Convert a `&str` to a null‑terminated UTF‑16 buffer.
pub fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── Section header ────────────────────────────────────────────────────

/// Draw a section header label (left aligned, single line, no vertical
/// centering — matches "Appearance", "Shortcuts", "Advanced", "Startup",
/// and "Quick Note" headers in the editor).
pub fn draw_section_header(
    dib_dc: HDC,
    rect: RECT,
    label: &str,
    font: HFONT,
    text_color: Argb,
) {
    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.to_colorref());
    }
    let mut wz = to_utf16_z(label);
    let mut rc = rect;
    unsafe {
        let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE);
    }
}

// ── Plain label ───────────────────────────────────────────────────────

/// Draw a single‑line plain label with vertical centering and left alignment
/// (used for "Theme", "Autostart", "Save path", etc.).
pub fn draw_plain_label(
    dib_dc: HDC,
    rect: RECT,
    label: &str,
    font: HFONT,
    text_color: Argb,
) {
    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.to_colorref());
    }
    let mut wz = to_utf16_z(label);
    let mut rc = rect;
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut wz,
            &mut rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

// ── Read‑only text field ──────────────────────────────────────────────

/// Draw a single‑line read‑only text field with a rounded background,
/// a 1‑px border, and clipped text.  When `text` is empty and `placeholder`
/// is `Some`, the placeholder is drawn in a muted colour.
///
/// The caller supplies the fully resolved background, border, and text colours
/// (including any hover / open state).
pub fn draw_readonly_text_field(
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    text: &str,
    placeholder: Option<&str>,
    font: HFONT,
    bg_color: Argb,
    border_color: Argb,
    text_color: Argb,
) {
    let scale = win_w as f32 / WIN_WIDTH_BASE as f32;
    let radius = (4.0 * scale) as i32;

    draw_rounded_rect_in_buffer(bits, win_w, win_h, rect, radius, bg_color);

    if border_color.a > 0 {
        draw_rounded_border_in_buffer(bits, win_w, win_h, rect, radius, 1, border_color);
    }

    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.to_colorref());
    }
    let display = if text.is_empty() {
        placeholder.unwrap_or("")
    } else {
        text
    };
    let inset = (6.0 * scale) as i32;
    let mut text_rc = RECT {
        left: rect.left + inset,
        top: rect.top,
        right: rect.right - inset,
        bottom: rect.bottom,
    };
    let mut wz = to_utf16_z(display);
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut wz,
            &mut text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
}

// ── Collapsed combo box ───────────────────────────────────────────────

/// Draw a collapsed (unopened) combo box: rounded background + border,
/// selected text, and a **▼** dropdown arrow at the right edge.
///
/// `arrow_width` controls the square arrow hit area width; pass
/// `max(rect height, …)` for a typical square arrow button.
pub fn draw_collapsed_combo_box(
    dib_dc: HDC,
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    arrow_width: i32,
    selected_text: &str,
    font: HFONT,
    bg_color: Argb,
    border_color: Argb,
    text_color: Argb,
    arrow_color: Argb,
) {
    let scale = win_w as f32 / WIN_WIDTH_BASE as f32;
    let radius = (4.0 * scale) as i32;

    // Background + border
    draw_rounded_rect_in_buffer(bits, win_w, win_h, rect, radius, bg_color);
    if border_color.a > 0 {
        draw_rounded_border_in_buffer(bits, win_w, win_h, rect, radius, 1, border_color);
    }

    // Selected text (left side, before arrow)
    unsafe {
        let _ = SelectObject(dib_dc, font);
        let _ = SetTextColor(dib_dc, text_color.to_colorref());
    }
    let inset = (8.0 * scale) as i32;
    let mut text_rc = RECT {
        left: rect.left + inset,
        top: rect.top,
        right: rect.right - arrow_width,
        bottom: rect.bottom,
    };
    let mut sel_wz = to_utf16_z(selected_text);
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut sel_wz,
            &mut text_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    // Dropdown arrow (right edge, vertically centred)
    unsafe {
        let _ = SetTextColor(dib_dc, arrow_color.to_colorref());
    }
    let mut arrow_wz = to_utf16_z("\u{25BC}"); // ▼
    let mut arrow_rc = RECT {
        left: rect.right - arrow_width,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut arrow_wz,
            &mut arrow_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }
}

// ── Toggle switch ─────────────────────────────────────────────────────

/// Draw a pill‑shaped toggle switch with a circular knob.
///
/// * `is_on` — knob slides to the right and the track uses `theme.accent`.
/// * `is_hovered` — applies a hover overlay on the track.
///
/// The caller positions `rect` (expected aspect ≈ 2 : 1, e.g. 36 × 18 at 96 dpi).
pub fn draw_toggle_switch(
    bits: *mut c_void,
    win_w: i32,
    win_h: i32,
    rect: RECT,
    is_on: bool,
    is_hovered: bool,
    theme: &NativeTheme,
) {
    let scale = win_w as f32 / WIN_WIDTH_BASE as f32;
    let h = rect.bottom - rect.top;
    let toggle_radius = h / 2;

    // Track background
    let mut track = if is_on {
        theme.accent
    } else {
        theme.surface.blend_over(theme.background)
    };
    if is_hovered {
        track = theme.hover.blend_over(track);
    }
    draw_rounded_rect_in_buffer(bits, win_w, win_h, rect, toggle_radius, track);

    // Knob
    let knob_margin = (2.0 * scale) as i32;
    let knob_diam = h - knob_margin * 2;
    let knob_left = if is_on {
        rect.right - knob_diam - knob_margin
    } else {
        rect.left + knob_margin
    };
    let knob_color = if is_on { theme.text } else { theme.text_muted };
    let knob_rect = RECT {
        left: knob_left,
        top: rect.top + knob_margin,
        right: knob_left + knob_diam,
        bottom: rect.bottom - knob_margin,
    };
    draw_rounded_rect_in_buffer(bits, win_w, win_h, knob_rect, knob_diam / 2, knob_color);
}
