//! Native Win32 layered OSD for brightness display.
//!
//! Architecture:
//! - Dedicated thread with blocking message loop (`MsgWaitForMultipleObjects`)
//! - Command queue protected by `Mutex` + Win32 auto-reset event for wake-up
//! - Layered popup window with per-pixel alpha (`UpdateLayeredWindow`)
//! - Pure GDI rendering (no eframe, no GDI+, no OpenGL)
//! - 0 % CPU when idle

use std::sync::{Arc, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_EVENT, WAIT_OBJECT_0, WPARAM};

use crate::native_theme::NativeTheme;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

// ── Send/Sync wrapper for HANDLE ───────────────────────────────────────

/// WIN32 `HANDLE` is a raw pointer; the crate does not auto‑impl `Send`/`Sync`.
/// Kernel handles *are* thread‑safe, so we explicitly opt in.
#[derive(Copy, Clone)]
struct ThreadHandle(HANDLE);
unsafe impl Send for ThreadHandle {}
unsafe impl Sync for ThreadHandle {}

impl ThreadHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

// ── public API ────────────────────────────────────────────────────────

/// Handle for posting OSD commands from any thread.
#[derive(Clone)]
pub struct OsdHandle {
    inner: Arc<Mutex<OsdInner>>,
}

struct OsdInner {
    queue: Vec<OsdCommand>,
    event: ThreadHandle,
}

enum OsdCommand {
    Show { value: u32, monitor_name: String },
    SetTheme(NativeTheme),
    Shutdown,
}

impl OsdHandle {
    /// Trigger the brightness OSD with a new value and monitor name.
    /// If the OSD is already visible, the timer is reset.
    pub fn show_brightness(&self, value: u32, monitor_name: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::Show { value, monitor_name });
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }

    /// Update the theme used by the OSD thread.
    pub fn set_theme(&self, theme: NativeTheme) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::SetTheme(theme));
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }

    /// Stop the OSD thread and destroy the window.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::Shutdown);
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }
}

/// Spawn the OSD thread and return a handle.
/// Must be called once during daemon startup.
pub fn start_osd() -> OsdHandle {
    let event = unsafe {
        CreateEventW(None, false, false, None).expect("CreateEventW for OSD")
    };

    let inner = Arc::new(Mutex::new(OsdInner {
        queue: Vec::new(),
        event: ThreadHandle(event),
    }));

    let inner2 = Arc::clone(&inner);
    std::thread::Builder::new()
        .name("mhd-osd".into())
        .spawn(move || {
            osd_thread(inner2);
        })
        .expect("spawn OSD thread");

    OsdHandle { inner }
}

// ── constants ─────────────────────────────────────────────────────────

/// Base width at 96 dpi — scaled to actual DPI at runtime.
const OSD_WIDTH_BASE: i32 = 380;
/// Base height at 96 dpi.
const OSD_HEIGHT_BASE: i32 = 112;
/// Milliseconds the OSD stays visible after the last update.
const HIDE_TIMEOUT_MS: u32 = 1200;
const HIDE_TIMER_ID: usize = 1;
const PADDING: i32 = 20;
const BAR_HEIGHT: i32 = 6;
const ROUND_RADIUS_BASE: f32 = 14.0;
/// `MsgWaitForMultipleObjects` returns `WAIT_OBJECT_0` for handles and
/// `WAIT_OBJECT_0 + 1` when message input arrives. This constant is for
/// match patterns that don't allow arithmetic expressions.
const MSG_ARRIVED: WAIT_EVENT = WAIT_EVENT(1);

// ── thread entry ─────────────────────────────────────────────────────

fn osd_thread(inner: Arc<Mutex<OsdInner>>) {
    let event = { inner.lock().unwrap().event.raw() };

    // Per-monitor DPI v2 awareness (available on Win10 1703+)
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // ---- register window class ----
    let cls_name = to_utf16_z("mhd_osd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(osd_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc); }

    // ---- create layered popup (hidden) ----
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED
                | WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_TRANSPARENT,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            OSD_WIDTH_BASE,
            OSD_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    // ---- compute DPI scale ----
    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;
    let osd_w = (OSD_WIDTH_BASE as f32 * scale) as i32;
    let osd_h = (OSD_HEIGHT_BASE as f32 * scale) as i32;

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, osd_w, osd_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    // ---- OSD live state (owned by this thread) ----
    let mut _value: u32 = 50;
    let mut _monitor_name: String = String::new();
    let mut theme: NativeTheme = NativeTheme::default();

    // Work area of primary monitor
    let work = monitor_work_rect(None);

    // ---- message loop ----
    loop {
        // Block until event is signaled or a queued Windows message arrives.
        let res = unsafe {
            MsgWaitForMultipleObjects(
                Some(&[event]),
                false, // wait for any (only one handle anyway)
                INFINITE,
                QS_ALLINPUT,
            )
        };

        match res {
            WAIT_OBJECT_0 => {
                // Drain the command queue.
                let mut guard = inner.lock().unwrap();
                while let Some(cmd) = guard.queue.pop() {
                    match cmd {
                        OsdCommand::Show { value: v, monitor_name: n } => {
                            _value = v;
                            _monitor_name = n;
                            // Reset hide timer.
                            unsafe {
                                let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                            }
                            // Paint immediately and show.
                            paint_osd(
                                hwnd,
                                _value,
                                &_monitor_name,
                                &work,
                                osd_w,
                                osd_h,
                                scale,
                                &theme,
                            );
                            unsafe {
                                let _ = ShowWindow(hwnd, SW_SHOWNA);
                            }
                        }
                        OsdCommand::SetTheme(t) => {
                            theme = t;
                        }
                        OsdCommand::Shutdown => {
                            drop(guard);
                            unsafe {
                                DestroyWindow(hwnd).ok();
                            }
                            return;
                        }
                    }
                }
            }
            MSG_ARRIVED => {
                // Windows messages are available — pump them all.
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            return;
                        }
                        if msg.message == WM_TIMER && msg.wParam.0 == HIDE_TIMER_ID {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            _ => break,
        }
    }
}

// ── window procedure (minimal) ───────────────────────────────────────

extern "system" fn osd_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ── painting ─────────────────────────────────────────────────────────

fn paint_osd(
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

    // ---- 32-bit ARGB DIB section ----
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // negative = top‑down DIB
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD::default(); 1],
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = unsafe {
        CreateDIBSection(screen_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
    };
    let Ok(dib) = dib else {
        unsafe { let _ = ReleaseDC(None, screen_dc); }
        return;
    };

    let dib_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let old_bmp = unsafe { SelectObject(dib_dc, dib) };

    // ---- fill pixel buffer with rounded-rect background ----
    let radius = (ROUND_RADIUS_BASE * scale) as i32;
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (width * height) as usize);
        draw_rounded_rect(pixels, width, height, radius, theme.background);
    }

    // ---- create fonts ----
    let font_name = to_utf16_z("Segoe UI");
    let font_h = -(14.0 * scale) as i32; // character height

    let hfont = unsafe {
        CreateFontW(
            font_h, 0, 0, 0,
            FW_NORMAL.0 as i32,
            0, 0, 0,
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
            font_small_h, 0, 0, 0,
            FW_NORMAL.0 as i32,
            0, 0, 0,
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

    let pad = (PADDING as f32 * scale) as i32;

    // ---- monitor name ----
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

    // ---- "Brightness" label ----
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
        let _ = DrawTextW(
            dib_dc,
            &mut label_wide,
            &mut lbl_rc,
            DT_LEFT | DT_SINGLELINE,
        );
    }
    // Restore primary text colour for subsequent text
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }

    // ---- progress bar ----
    let bar_y = lbl_y + font_small_h.abs() + 12;
    let bar_w = ((width - pad * 2) as f32 * 0.78) as i32;
    let bar_x = pad + radius / 2;
    let bar_h = ((BAR_HEIGHT as f32 * scale) as i32).max(2);

    // track
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

    // fill
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

    // ---- percentage label ----
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

    // ---- clean up GDI objects (keep DIB selected) ----
    unsafe {
        let _ = SelectObject(dib_dc, old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    // GDI often writes RGB into a 32-bit DIB without a valid alpha channel.
    // Keep the rounded background translucent, but make foreground UI
    // (text/progress bar) fully opaque.
    fix_gdi_alpha(bits, width, height, theme.background);

    // ---- UpdateLayeredWindow ----
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let pt_src = POINT { x: 0, y: 0 };
    let sz = SIZE { cx: width, cy: height };

    // Center on primary monitor work area.
    let pos_x = work.left + (work.right - work.left - width) / 2;
    let pos_y = work.top + (work.bottom - work.top - height) / 2;
    let pt_dst = POINT { x: pos_x, y: pos_y };

    unsafe {
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),  // hdcDst: screen DC (use default = NULL)
            Some(&pt_dst),
            Some(&sz),
            dib_dc,          // hdcSrc: our DIB DC (pass directly, not Some)
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    // ---- cleanup DIB ----
    unsafe {
        let _ = SelectObject(dib_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(dib_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn fix_gdi_alpha(bits: *mut std::ffi::c_void, width: i32, height: i32, background: crate::native_theme::Argb) {
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
            *px = 0xff00_0000 | (*px & 0x00ff_ffff);
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

/// Fill a 32-bit ARGB pixel buffer with a rounded rectangle.
/// Corner edges are anti‑aliased over a 1‑pixel falloff for smoothness.
pub fn draw_rounded_rect(pixels: &mut [u32], width: i32, height: i32, r: i32, color: crate::native_theme::Argb) {
    let bg: u32 = color.to_premultiplied_argb_pixel();
    let transparent: u32 = 0x00000000;

    // Corner circle centres
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
            // Determine which corner this pixel belongs to (if any)
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
                // Smoothstep: 1.0 inside circle, 0.0 outside, linear falloff over 1px
                let falloff = 1.0 - (dist - cr as f32).clamp(0.0, 1.0);
                if falloff <= 0.0 {
                    transparent
                } else {
                    // Extract original ARGB from premultiplied bg pixel
                    let ba = ((bg >> 24) & 0xFF) as f32;
                    let br = ((bg >> 16) & 0xFF) as f32;
                    let bg_ = ((bg >> 8) & 0xFF) as f32;
                    let bb = (bg & 0xFF) as f32;
                    // Scale by falloff
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

/// Get the work area rect of the primary monitor (taskbar‑aware).
fn monitor_work_rect(_hwnd: Option<HWND>) -> RECT {
    unsafe {
        let desktop = GetDesktopWindow();
        let hmon = MonitorFromWindow(desktop, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}

/// Encode `&str` as UTF‑16 with trailing NUL.
pub fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
