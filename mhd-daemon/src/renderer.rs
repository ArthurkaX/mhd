//! Shared GDI DIB renderer for layered overlay windows.
//!
//! Provides [`DibFrame`] (RAII wrapper around DIB + memory DC) and
//! shared drawing primitives used by all overlay windows.
//!
//! Architecture
//! ────────────
//! ```
//! DibFrame::new(w, h)          // allocate DIB + DC
//!   .pixels_mut()              // direct pixel access (rounded rect, alpha fix)
//!   .dc()                      // GDI HDC for DrawTextW, FillRect, …
//!   .present_layered(hwnd, x, y)  // UpdateLayeredWindow + auto cleanup
//! // drop → SelectObject(old), DeleteObject(dib), DeleteDC(dib_dc), ReleaseDC(screen)
//! ```

use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DIB_RGB_COLORS,
    DRAW_TEXT_FORMAT, FF_DONTCARE, FW_BOLD, FW_NORMAL, HDC, OUT_DEFAULT_PRECIS, RGBQUAD,
    TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::core::native_theme::{Argb, NativeTheme};

// ── BLENDFUNCTION constant re-export ─────────────────────────────────

pub use windows::Win32::Graphics::Gdi::{AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION};

// ── DibFrame ──────────────────────────────────────────────────────────

/// RAII wrapper for a 32-bit DIB section and its compatible memory DC.
///
/// # Lifecycle
///
/// 1. [`new`](DibFrame::new) — allocate DIB + compatible DC.
/// 2. Draw using [`pixels_mut`](DibFrame::pixels_mut) (direct pixel ops)
///    and/or [`dc`](DibFrame::dc) (GDI calls).
/// 3. Call [`fix_gdi_alpha`](DibFrame::fix_gdi_alpha) to fix alpha
///    channel after GDI text/brush drawing.
/// 4. Call [`present_layered`](DibFrame::present_layered) to show via
///    `UpdateLayeredWindow`.  This does **not** consume the frame; you
///    can repaint and present again.
/// 5. Drop — restores the old bitmap, deletes the DIB, DC, and screen DC.
pub struct DibFrame {
    width: i32,
    height: i32,
    screen_dc: HDC,
    dib_dc: HDC,
    dib: windows::Win32::Graphics::Gdi::HBITMAP,
    old_bmp: windows::Win32::Graphics::Gdi::HGDIOBJ,
    bits: *mut u32,
}

// SAFETY: DibFrame owns raw Win32 handles but never sends them across
// threads. It is !Send + !Sync by default because of raw pointers.
// Threads that create DibFrame own them entirely within one window's
// paint function.
unsafe impl Send for DibFrame {}
unsafe impl Sync for DibFrame {}

impl DibFrame {
    /// Allocate a 32-bit top-down DIB and a memory DC compatible with
    /// the screen.  Returns `None` on allocation failure.
    pub fn new(width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }

        // SAFETY: GetDC(None) returns a screen DC; it must be released.
        let screen_dc = unsafe { GetDC(None) };

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
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
        // SAFETY: CreateDIBSection allocates a GDI bitmap.  We check the result.
        let dib = unsafe { CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
        let Ok(dib) = dib else {
            // SAFETY: ReleaseDC is safe even if the DC is the screen DC.
            unsafe {
                let _ = ReleaseDC(None, screen_dc);
            }
            return None;
        };

        // SAFETY: CreateCompatibleDC creates a 1x1 monochrome DC by default;
        // selecting a DIB into it gives it colour depth.
        let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
        if dib_dc.is_invalid() {
            unsafe {
                let _ = DeleteObject(dib);
                let _ = ReleaseDC(None, screen_dc);
            }
            return None;
        }

        // SAFETY: SelectObject returns the previous bitmap; we must restore it on drop.
        let old_bmp = unsafe { SelectObject(dib_dc, dib) };

        Some(DibFrame {
            width,
            height,
            screen_dc,
            dib_dc,
            dib,
            old_bmp,
            bits: bits as *mut u32,
        })
    }

    /// DIB width in pixels.
    #[inline]
    pub fn width(&self) -> i32 {
        self.width
    }

    /// DIB height in pixels.
    #[inline]
    pub fn height(&self) -> i32 {
        self.height
    }

    /// Returns a mutable slice of all pixels (BGRA, 0xAARRGGBB).
    ///
    /// Pixels are in premultiplied alpha form when drawn with
    /// [`draw_rounded_rect`] or set via pixel ops.
    #[inline]
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let len = (self.width * self.height) as usize;
        // SAFETY: `bits` points to valid memory for `width * height` u32s
        // as allocated by CreateDIBSection. We hold the DibFrame mutably.
        unsafe { std::slice::from_raw_parts_mut(self.bits, len) }
    }

    /// Returns the memory DC for GDI drawing calls (DrawTextW, FillRect, …).
    ///
    /// The DC is pre‑selected with the DIB, so all drawing is done
    /// directly into the pixel buffer.
    #[inline]
    pub fn dc(&self) -> HDC {
        self.dib_dc
    }

    /// Fix alpha channel for pixels drawn by GDI text/brush functions,
    /// which leave alpha zero.  Must be called **after** all GDI drawing
    /// and **before** [`present_layered`](DibFrame::present_layered).
    ///
    /// `background` is the rounded‑rect background colour (used to detect
    /// which pixels are background vs. foreground).
    pub fn fix_gdi_alpha(&mut self, background: Argb) {
        fix_gdi_alpha(
            self.bits as *mut c_void,
            self.width,
            self.height,
            background,
        );
    }

    /// Present the DIB as a layered window via `UpdateLayeredWindow`.
    ///
    /// The window is positioned at `(x, y)` and sized to `(width, height)`.
    /// `alpha` is the per‑window constant alpha (usually 255).
    ///
    /// This call does **not** take ownership; you can draw again and
    /// call `present_layered` multiple times.
    pub fn present_layered(&self, hwnd: HWND, x: i32, y: i32, alpha: u8) {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let pt_dst = POINT { x, y };

        // SAFETY: UpdateLayeredWindow is safe with valid HWND, DC, and size.
        unsafe {
            let _ = UpdateLayeredWindow(
                hwnd,
                HDC::default(),
                Some(&pt_dst),
                Some(&sz),
                self.dib_dc,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for DibFrame {
    fn drop(&mut self) {
        // SAFETY: Restore old bitmap, delete the DIB, delete the DC,
        // release the screen DC.  All handles are valid at this point.
        unsafe {
            let _ = SelectObject(self.dib_dc, self.old_bmp);
            let _ = DeleteObject(self.dib);
            let _ = DeleteDC(self.dib_dc);
            let _ = ReleaseDC(None, self.screen_dc);
        }
    }
}

// ── Drawing primitives ────────────────────────────────────────────────

/// Fill the DIB with a rounded‑rectangle background.
///
/// Pixels inside the corners are set to `color` (premultiplied ARGB).
/// Pixels outside (the four cut corners) are set to 0x00000000 (transparent).
/// Corner edges are anti‑aliased with a 1‑pixel falloff.
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

/// Fix alpha channel for pixels drawn by standard GDI text/brush
/// functions.  GDI functions always write alpha = 0; this restores it
/// to 0xFF for non‑background pixels while leaving custom‑drawn pixels
/// (rounded corners, buttons) untouched.
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

            // GDI text/brush calls often leave alpha at 0. Restore only
            // those pixels; custom drawing helpers already write
            // meaningful alpha for glass/hover/selection colors and
            // must keep it.
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

// ── Font helpers ──────────────────────────────────────────────────────

/// Create a GDI font with the given height, weight, and family name.
///
/// `height` should be **negative** for character‑height‑based sizing
/// (the standard practice for overlays: `-(pt * dpi / 72)` or simply
/// `-(desired_px)`).
/// `bold` controls FW_NORMAL vs FW_BOLD.
pub fn create_font(height: i32, bold: bool, family: &str) -> windows::Win32::Graphics::Gdi::HFONT {
    let name = to_utf16_z(family);
    // SAFETY: CreateFontW is safe; parameters are well-formed.
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            if bold {
                FW_BOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(name.as_ptr()),
        )
    }
}

// ── String helpers ────────────────────────────────────────────────────

/// Tell the OS it can reclaim physical RAM pages that the process has
/// freed but is no longer actively touching.
///
/// Call after a blocking UI operation (e.g. settings window, overlay
/// popup) that temporarily allocated significant memory, so the process
/// working set returns to its baseline.
pub fn trim_working_set() {
    #[cfg(windows)]
    unsafe {
        let process = windows::Win32::System::Threading::GetCurrentProcess();
        let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(process);
    }
}

/// Convert a Rust `&str` to a null‑terminated UTF‑16 vector for use
/// with Win32 `PCWSTR` parameters.
pub fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── Shell renderer ─────────────────────────────────────────────────────

/// High‑level helper that wraps [`DibFrame`] with a theme, DPI scale, and
/// convenience methods for common overlay window elements:
/// background, separator lines, text, and presenting.
///
/// # Lifecycle
///
/// 1. [`ShellRenderer::new`] — allocates the DIB and draws the rounded
///    background.
/// 2. Call draw helpers (`draw_separator`, or use [`dc`] / [`frame`] for
///    custom GDI drawing).
/// 3. [`ShellRenderer::present`] — calls `fix_gdi_alpha` + `present_layered`.
///
/// Drop restores and cleans up GDI resources via `DibFrame`.
pub struct ShellRenderer {
    frame: DibFrame,
    theme: NativeTheme,
    scale: f32,
}

impl ShellRenderer {
    /// Allocate a new DIB, draw the rounded‑rect background, and store
    /// the theme and scale.  Returns `None` on allocation failure.
    ///
    /// `radius` is the corner rounding radius in unscaled pixels
    /// (it is multiplied by `scale` internally).
    pub fn new(
        width: i32,
        height: i32,
        theme: &NativeTheme,
        scale: f32,
        radius: i32,
    ) -> Option<Self> {
        let mut frame = DibFrame::new(width, height)?;
        let r = (radius as f32 * scale) as i32;
        draw_rounded_rect(frame.pixels_mut(), width, height, r, theme.background);
        Some(ShellRenderer {
            frame,
            theme: theme.clone(),
            scale,
        })
    }

    /// Access the underlying `DibFrame` for pixel ops.
    #[inline]
    pub fn frame(&mut self) -> &mut DibFrame {
        &mut self.frame
    }

    /// GDI device context of the DIB.
    #[inline]
    pub fn dc(&self) -> HDC {
        self.frame.dc()
    }

    /// DPI scale factor (dpi / 96.0).
    #[inline]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The window theme.
    #[inline]
    pub fn theme(&self) -> &NativeTheme {
        &self.theme
    }

    /// DIB width.
    #[inline]
    pub fn width(&self) -> i32 {
        self.frame.width()
    }

    /// DIB height.
    #[inline]
    pub fn height(&self) -> i32 {
        self.frame.height()
    }

    /// Draw a horizontal separator line from `x1` to `x2` at `y`.
    /// The line colour comes from `theme.border`.
    pub fn draw_separator(&mut self, y: i32, x1: i32, x2: i32) {
        let h = (1.0_f32 * self.scale).max(1.0) as i32;
        // SAFETY: GDI calls within a DC that is selected with our DIB.
        let brush = unsafe { CreateSolidBrush(self.theme.border.to_colorref()) };
        let rc = RECT {
            left: x1,
            top: y,
            right: x2,
            bottom: y + h,
        };
        unsafe {
            let _ = FillRect(self.dc(), &rc, brush);
            let _ = DeleteObject(brush);
        }
    }

    /// Draw single‑line text with GDI `DrawTextW`.
    ///
    /// `font` should be selected **before** calling this (via
    /// `SelectObject`).  The font itself is **not** owned by this
    /// method and must be deleted by the caller.
    pub fn draw_text(&mut self, text: &str, rect: &RECT, color: Argb, format: DRAW_TEXT_FORMAT) {
        unsafe {
            let _ = SetTextColor(self.dc(), color.to_colorref());
            let _ = SetBkMode(self.dc(), TRANSPARENT);
        }
        let mut wz = to_utf16_z(text);
        let mut rc = *rect;
        unsafe {
            let _ = DrawTextW(self.dc(), &mut wz, &mut rc, format);
        }
    }

    /// Fix GDI alpha, present via `UpdateLayeredWindow`, and consume
    /// the shell renderer (the DIB is still valid until drop).
    pub fn present(mut self, hwnd: HWND, x: i32, y: i32, alpha: u8) {
        self.frame.fix_gdi_alpha(self.theme.background);
        self.frame.present_layered(hwnd, x, y, alpha);
    }
}

/// Return the work area rectangle of the primary monitor.
pub fn primary_monitor_work_rect() -> RECT {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO};
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
    // SAFETY: Standard GDI calls.
    unsafe {
        let desktop = GetDesktopWindow();
        let hmon = MonitorFromWindow(
            desktop,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}

/// Compute centered position for a `(width, height)` window on the
/// primary monitor's work area.
pub fn centered_position(width: i32, height: i32) -> (i32, i32) {
    let work = primary_monitor_work_rect();
    let x = work.left + (work.right - work.left - width) / 2;
    let y = work.top + (work.bottom - work.top - height) / 2;
    (x, y)
}
