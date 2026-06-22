//! Windows screen capture — reusable GDI monitor capture.
//!
//! Provides a single function [`capture_foreground_monitor`] that captures the
//! monitor containing the foreground window (falling back to the primary monitor)
//! and returns BGRA pixels converted to RGBA.
//!
//! This replaces inline capture code that existed in the Quick Draw overlay.

use windows::Win32::Foundation::{BOOL, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDC, MONITOR_DEFAULTTONEAREST, MonitorFromWindow, RGBQUAD, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Captured image data in RGBA format.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A resolved monitor target ready for capture.
#[derive(Debug, Clone)]
pub struct CaptureTarget {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Errors that can occur during screen capture.
#[derive(Debug)]
pub enum CaptureError {
    NoMonitor,
    NoDeviceContext,
    DibCreationFailed,
    BitBltFailed,
    NoForegroundWindow,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMonitor => write!(f, "no monitor found"),
            Self::NoDeviceContext => write!(f, "failed to get device context"),
            Self::DibCreationFailed => write!(f, "failed to create DIB section"),
            Self::BitBltFailed => write!(f, "BitBlt failed"),
            Self::NoForegroundWindow => write!(f, "no foreground window"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Get the monitor rect for the monitor containing the foreground window.
/// Falls back to the primary monitor if no foreground window or its monitor
/// cannot be resolved.
fn get_target_monitor_rect() -> Result<(RECT, i32, i32), CaptureError> {
    unsafe {
        // Try foreground window first
        let fg = GetForegroundWindow();
        let hmon = if !fg.is_invalid() {
            MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST)
        } else {
            MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTONEAREST)
        };

        if hmon.is_invalid() {
            return Err(CaptureError::NoMonitor);
        }

        let mut mi: windows::Win32::Graphics::Gdi::MONITORINFOEXW = std::mem::zeroed();
        mi.monitorInfo.cbSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFOEXW>() as u32;
        let mi_ptr = &mi.monitorInfo as *const windows::Win32::Graphics::Gdi::MONITORINFO
            as *mut windows::Win32::Graphics::Gdi::MONITORINFO;
        if windows::Win32::Graphics::Gdi::GetMonitorInfoW(hmon, mi_ptr) == BOOL(0) {
            return Err(CaptureError::NoMonitor);
        }

        let r = mi.monitorInfo.rcMonitor;
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        Ok((r, w, h))
    }
}

/// Resolve the foreground monitor into a [`CaptureTarget`] without capturing.
///
/// Falls back to the primary monitor if no foreground window or its monitor
/// cannot be resolved.
pub fn resolve_foreground_monitor() -> Result<CaptureTarget, CaptureError> {
    let (rect, width, height) = get_target_monitor_rect()?;
    if width <= 0 || height <= 0 {
        return Err(CaptureError::NoMonitor);
    }
    Ok(CaptureTarget {
        left: rect.left,
        top: rect.top,
        width: width as u32,
        height: height as u32,
    })
}

/// Capture a specific monitor rect. The rect is obtained from [`resolve_foreground_monitor`].
pub fn capture_target(target: &CaptureTarget) -> Result<CapturedImage, CaptureError> {
    let w = target.width;
    let h = target.height;

    if w == 0 || h == 0 {
        return Err(CaptureError::NoMonitor);
    }

    unsafe {
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.is_invalid() {
            return Err(CaptureError::NoDeviceContext);
        }

        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            let _ = ReleaseDC(HWND::default(), hdc_screen);
            return Err(CaptureError::NoDeviceContext);
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => dib,
            Err(_) => {
                let _ = DeleteDC(hdc_mem);
                let _ = ReleaseDC(HWND::default(), hdc_screen);
                return Err(CaptureError::DibCreationFailed);
            }
        };

        let _old_obj = SelectObject(hdc_mem, dib);

        let result = BitBlt(
            hdc_mem,
            0,
            0,
            w as i32,
            h as i32,
            hdc_screen,
            target.left,
            target.top,
            SRCCOPY,
        );

        if result.is_err() {
            let _ = SelectObject(hdc_mem, _old_obj);
            let _ = DeleteObject(dib);
            let _ = DeleteDC(hdc_mem);
            let _ = ReleaseDC(HWND::default(), hdc_screen);
            return Err(CaptureError::BitBltFailed);
        }

        let bgra_slice = std::slice::from_raw_parts(bits as *const u32, (w * h) as usize);

        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for &pixel in bgra_slice {
            let b = (pixel & 0xFF) as u8;
            let g = ((pixel >> 8) & 0xFF) as u8;
            let r = ((pixel >> 16) & 0xFF) as u8;
            let a = ((pixel >> 24) & 0xFF) as u8;
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(a);
        }

        let _ = SelectObject(hdc_mem, _old_obj);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(hdc_mem);
        let _ = ReleaseDC(HWND::default(), hdc_screen);

        Ok(CapturedImage {
            width: w,
            height: h,
            rgba,
        })
    }
}

/// Capture the full monitor containing the foreground window.
///
/// Returns the image as RGBA pixels. Uses GDI `BitBlt` to capture the screen
/// into a DIB section and then converts BGRA (GDI native) to RGBA.
pub fn capture_foreground_monitor() -> Result<CapturedImage, CaptureError> {
    let (monitor_rect, width, height) = get_target_monitor_rect()?;

    if width <= 0 || height <= 0 {
        return Err(CaptureError::NoMonitor);
    }

    let w = width as u32;
    let h = height as u32;

    unsafe {
        let hdc_screen = GetDC(HWND::default());
        if hdc_screen.is_invalid() {
            return Err(CaptureError::NoDeviceContext);
        }

        let hdc_mem = CreateCompatibleDC(hdc_screen);
        if hdc_mem.is_invalid() {
            let _ = ReleaseDC(HWND::default(), hdc_screen);
            return Err(CaptureError::NoDeviceContext);
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down bitmap
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(dib) => dib,
            Err(_) => {
                let _ = DeleteDC(hdc_mem);
                let _ = ReleaseDC(HWND::default(), hdc_screen);
                return Err(CaptureError::DibCreationFailed);
            }
        };

        let _old_obj = SelectObject(hdc_mem, dib);

        let result = BitBlt(
            hdc_mem,
            0,
            0,
            w as i32,
            h as i32,
            hdc_screen,
            monitor_rect.left,
            monitor_rect.top,
            SRCCOPY,
        );

        if result.is_err() {
            let _ = SelectObject(hdc_mem, _old_obj);
            let _ = DeleteObject(dib);
            let _ = DeleteDC(hdc_mem);
            let _ = ReleaseDC(HWND::default(), hdc_screen);
            return Err(CaptureError::BitBltFailed);
        }

        // Read BGRA pixels from DIB
        let bgra_slice = std::slice::from_raw_parts(bits as *const u32, (w * h) as usize);

        // Convert BGRA to RGBA
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for &pixel in bgra_slice {
            let b = (pixel & 0xFF) as u8;
            let g = ((pixel >> 8) & 0xFF) as u8;
            let r = ((pixel >> 16) & 0xFF) as u8;
            let a = ((pixel >> 24) & 0xFF) as u8;
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(a);
        }

        // Cleanup
        let _ = SelectObject(hdc_mem, _old_obj);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(hdc_mem);
        let _ = ReleaseDC(HWND::default(), hdc_screen);

        Ok(CapturedImage {
            width: w,
            height: h,
            rgba,
        })
    }
}
