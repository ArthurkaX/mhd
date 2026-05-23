use std::ffi::c_void;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY,
    DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_NORMAL,
    HDC, OUT_DEFAULT_PRECIS, RGBQUAD, TRANSPARENT, AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};
use windows::core::PCWSTR;

use crate::native_theme::{Argb, NativeTheme};

pub fn paint_osd(
    hwnd: HWND,
    value: u32,
    monitor_name: &str,
    work: &RECT,
    width: i32,
    height: i32,
    scale: f32,
    theme: &NativeTheme,
) {
    let screen_dc = unsafe { GetDC(None) };

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let Ok(dib) = dib else {
        unsafe {
            let _ = ReleaseDC(None, screen_dc);
        }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    let radius = (14.0 * scale) as i32;
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        draw_rounded_rect(pixels, width, height, radius, theme.background);
    }

    let font_name = to_utf16_z("Segoe UI");
    let font_h = -(14.0 * scale) as i32;

    let hfont = unsafe {
        CreateFontW(
            font_h,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        )
    };

    let font_small_h = -(11.0 * scale) as i32;
    let hfont_small = unsafe {
        CreateFontW(
            font_small_h,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        )
    };

    let old_font = unsafe { SelectObject(dib_dc, hfont) };

    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }

    let pad = (20.0 * scale) as i32;

    // Monitor name
    let name_y = pad + 4;
    let mut name_rc = RECT {
        left: pad + radius / 2,
        top: name_y,
        right: width - pad,
        bottom: name_y + font_h.abs() * 3 / 2,
    };
    let mut name_wz = to_utf16_z(monitor_name);
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut name_wz,
            &mut name_rc,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // "Brightness" label
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let lbl_y = name_y + font_h.abs() + 12;
    let mut lbl_rc = RECT {
        left: pad + radius / 2,
        top: lbl_y,
        right: width - pad,
        bottom: lbl_y + font_small_h.abs() * 3 / 2 + 4,
    };
    let mut label_wide = to_utf16_z("Brightness");
    unsafe {
        let _ = DrawTextW(dib_dc, &mut label_wide, &mut lbl_rc, DT_LEFT | DT_SINGLELINE);
    }
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }

    // Progress bar
    let bar_y = lbl_y + font_small_h.abs() + 12;
    let bar_w = ((width - pad * 2) as f32 * 0.78) as i32;
    let bar_x = pad + radius / 2;
    let bar_h = ((6.0 * scale) as i32).max(2);

    // Track
    {
        let track_brush = unsafe { CreateSolidBrush(theme.bar_background.to_colorref()) };
        let track_rc = RECT {
            left: bar_x,
            top: bar_y,
            right: bar_x + bar_w,
            bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(dib_dc, &track_rc, track_brush);
            let _ = DeleteObject(track_brush);
        }
    }

    // Fill
    let fill_w = ((bar_w as f32) * (value.min(100) as f32 / 100.0)) as i32;
    if fill_w > 0 {
        let accent = unsafe { CreateSolidBrush(theme.accent.to_colorref()) };
        let fill_rc = RECT {
            left: bar_x,
            top: bar_y,
            right: bar_x + fill_w,
            bottom: bar_y + bar_h,
        };
        unsafe {
            let _ = FillRect(dib_dc, &fill_rc, accent);
            let _ = DeleteObject(accent);
        }
    }

    // Percentage label
    let pct = format!("{}%", value.min(100));
    let mut pct_wz = to_utf16_z(&pct);
    let mut pct_rc = RECT {
        left: bar_x + bar_w + 12,
        top: bar_y - 4,
        right: width - pad,
        bottom: bar_y + bar_h + 4,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut pct_wz,
            &mut pct_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    fix_gdi_alpha(bits, width, height, theme.background);

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE {
        cx: width,
        cy: height,
    };

    let pos_x = work.left + (work.right - work.left - width) / 2;
    let pos_y = work.top + (work.bottom - work.top - height) / 2;
    let pt_dst = POINT { x: pos_x, y: pos_y };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            Some(&pt_dst),
            Some(&sz),
            dib_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

pub fn fix_gdi_alpha(bits: *mut c_void, width: i32, height: i32, background: Argb) {
    if bits.is_null() || width <= 0 || height <= 0 {
        return;
    }

    let bg_px = background.to_premultiplied_argb_pixel();
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        for px in pixels.iter_mut() {
            if *px == 0 {
                continue;
            }
            if is_background_like_pixel(*px, bg_px, background.a) {
                continue;
            }

            // GDI text/brush calls often leave alpha at 0. Restore only those
            // pixels; custom drawing helpers already write meaningful alpha
            // for glass/hover/selection colors and must keep it.
            if (*px >> 24) == 0 {
                *px = 0xff00_0000 | (*px & 0x00ff_ffff);
            }
        }
    }
}

fn is_background_like_pixel(px: u32, bg_px: u32, bg_alpha: u8) -> bool {
    if px == bg_px {
        return true;
    }
    let a = ((px >> 24) & 0xff) as u8;
    let rgb = px & 0x00ff_ffff;
    let bg_rgb = bg_px & 0x00ff_ffff;
    rgb == bg_rgb && a <= bg_alpha
}

pub fn draw_rounded_rect(pixels: &mut [u32], width: i32, height: i32, r: i32, color: Argb) {
    let bg: u32 = color.to_premultiplied_argb_pixel();
    let transparent: u32 = 0x00000000;

    let cr = r;
    let tl_cx = cr;
    let tl_cy = cr;
    let tr_cx = width - cr - 1;
    let tr_cy = cr;
    let bl_cx = cr;
    let bl_cy = height - cr - 1;
    let br_cx = width - cr - 1;
    let br_cy = height - cr - 1;

    for y in 0..height {
        for x in 0..width {
            let (is_corner, cx, cy) = if x < cr && y < cr {
                (true, tl_cx, tl_cy)
            } else if x > tr_cx && y < cr {
                (true, tr_cx, tr_cy)
            } else if x < cr && y > bl_cy {
                (true, bl_cx, bl_cy)
            } else if x > br_cx && y > br_cy {
                (true, br_cx, br_cy)
            } else {
                (false, 0, 0)
            };

            let pixel = if is_corner {
                let dx = (x - cx) as f32;
                let dy = (y - cy) as f32;
                let dist = (dx * dx + dy * dy).sqrt();
                let falloff = 1.0 - (dist - cr as f32).clamp(0.0, 1.0);
                if falloff <= 0.0 {
                    transparent
                } else {
                    let ba = ((bg >> 24) & 0xFF) as f32;
                    let br = ((bg >> 16) & 0xFF) as f32;
                    let bg_ = ((bg >> 8) & 0xFF) as f32;
                    let bb = (bg & 0xFF) as f32;
                    let na = (ba * falloff) as u32;
                    let nr = (br * falloff) as u32;
                    let ng = (bg_ * falloff) as u32;
                    let nb = (bb * falloff) as u32;
                    (na.min(255) << 24) | (nr.min(255) << 16) | (ng.min(255) << 8) | nb.min(255)
                }
            } else {
                bg
            };

            pixels[(y * width + x) as usize] = pixel;
        }
    }
}

pub fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
