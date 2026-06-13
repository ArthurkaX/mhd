//! Real-time proxy trace overlay.
//!
//! Shows recent routing decisions made by the embedded LLM proxy in a small
//! transparent window. Each row is one request: original tier, effective tier,
//! target model, and a "downgraded" badge.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::core::native_theme::NativeTheme;

// ── Constants ────────────────────────────────────────────────────────

const WIN_W_BASE: i32 = 520;
const WIN_H_BASE: i32 = 400;
const PAD_BASE: i32 = 12;
const RADIUS_BASE: f32 = 10.0;
const HEADER_H_BASE: i32 = 28;
const ROW_H_BASE: i32 = 22;
const HIDE_TIMEOUT_MS: u32 = 60_000;
const HIDE_TIMER_ID: usize = 1;
const REFRESH_TIMER_ID: usize = 2;

// ── Safe wrapper for the event handle ────────────────────────────────

struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

// ── Toggle / show ───────────────────────────────────────────────────

pub fn show(theme: &NativeTheme) {
    let event = unsafe { CreateEventW(None, false, false, None).unwrap() };
    let handle = SafeHandle(event);
    let dying = Arc::new(AtomicBool::new(false));
    let theme_clone = theme.clone();

    std::thread::Builder::new()
        .name("mhd-proxy-trace".into())
        .spawn(move || {
            panel_thread(handle, dying, theme_clone);
        })
        .ok();
}

fn panel_thread(event: SafeHandle, dying: Arc<AtomicBool>, theme: NativeTheme) {
    let cls_name = crate::osd::to_utf16_z("mhd_proxy_trace_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(panel_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    let dpi = unsafe { GetDpiForWindow(GetDesktopWindow()) as f32 };
    let scale = dpi / 96.0;

    let win_w = (WIN_W_BASE as f32 * scale) as i32;
    let win_h = (WIN_H_BASE as f32 * scale) as i32;

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            win_w,
            win_h,
            None,
            None,
            hinstance,
            None,
        )
    }
    .ok();

    let hwnd = match hwnd {
        Some(h) => h,
        None => return,
    };

    unsafe {
        let _ = SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Box::into_raw(Box::new(theme.clone())) as isize,
        );
    }

    // Position at right side of primary monitor
    let work = primary_monitor_work_rect();
    let pos_x = work.right - win_w - (PAD_BASE as f32 * scale) as i32;
    let pos_y = work.top + (PAD_BASE as f32 * scale) as i32;

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            pos_x,
            pos_y,
            win_w,
            win_h,
            SWP_SHOWWINDOW,
        );
    }

    // Start auto-refresh timer (1 second)
    unsafe {
        let _ = SetTimer(hwnd, REFRESH_TIMER_ID, 1000, None);
    }

    // Paint
    paint_panel(hwnd, scale, win_w, win_h);

    // Message loop
    let mut msg = MSG::default();
    loop {
        if dying.load(Ordering::Acquire) {
            break;
        }
        let wait = [event.0];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, 200, QS_ALLINPUT) };
        if res == WAIT_OBJECT_0 {
            // Toggle requested — close
            break;
        }
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

// ── Paint ────────────────────────────────────────────────────────────

fn primary_monitor_work_rect() -> RECT {
    crate::renderer::primary_monitor_work_rect()
}

fn paint_panel(hwnd: HWND, mut scale: f32, mut win_w: i32, mut win_h: i32) {
    if win_w == 0 || win_h == 0 {
        let mut wr = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut wr);
        }
        win_w = wr.right - wr.left;
        win_h = wr.bottom - wr.top;
        if scale == 0.0 {
            let dpi = unsafe { GetDpiForWindow(hwnd) as f32 };
            scale = dpi / 96.0;
        }
    }
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme };
    if state_ptr.is_null() {
        return;
    }
    let theme = unsafe { &*state_ptr };

    let mut frame = match crate::renderer::DibFrame::new(win_w, win_h) {
        Some(f) => f,
        None => return,
    };
    let dib_dc = frame.dc();
    let bits = frame.pixels_mut().as_mut_ptr() as *mut c_void;
    let _ = bits;

    let radius = (RADIUS_BASE * scale) as i32;
    crate::osd::draw_rounded_rect(frame.pixels_mut(), win_w, win_h, radius, theme.background);

    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h = -(14.0 * scale) as i32;
    let font_small_h = -(12.0 * scale) as i32;
    let header_h = (HEADER_H_BASE as f32 * scale) as i32;
    let row_h = (ROW_H_BASE as f32 * scale) as i32;

    let hfont = crate::osd::create_font(font_h, false, "Segoe UI");
    let hfont_small = crate::osd::create_font(font_small_h, false, "Segoe UI");
    let _old_font = unsafe { SelectObject(dib_dc, hfont) };
    unsafe {
        let _ = SetBkMode(dib_dc, TRANSPARENT);
    }

    // ── Header ──────────────────────────────────────────────────
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut title_wz = crate::osd::to_utf16_z("Proxy Trace");
    let mut title_rc = RECT {
        left: pad,
        top: pad,
        right: win_w - pad,
        bottom: pad + font_h.abs() + 4,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut title_wz,
            &mut title_rc,
            DT_LEFT | DT_SINGLELINE,
        );
    }

    // Status (right-aligned)
    let running = crate::llm_proxy::is_running();
    let status = if running { "proxy: on" } else { "proxy: off" };
    let status_color = if running {
        theme.accent
    } else {
        theme.text_muted
    };
    unsafe {
        let _ = SetTextColor(dib_dc, status_color.to_colorref());
    }
    let mut status_wz = crate::osd::to_utf16_z(status);
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut status_wz,
            &mut title_rc,
            DT_RIGHT | DT_SINGLELINE,
        );
    }

    // Separator
    let sep_y = pad + font_h.abs() + 8;
    let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
    unsafe {
        let _ = FillRect(
            dib_dc,
            &RECT {
                left: pad,
                top: sep_y,
                right: win_w - pad,
                bottom: sep_y + 1,
            },
            sep_brush,
        );
        let _ = DeleteObject(sep_brush);
    }

    // ── Column headers ───────────────────────────────────────────
    let col_y = sep_y + (4.0 * scale) as i32;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
    }
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }

    let col_w = (win_w - pad * 2) / 5;
    let col_labels = ["Seq", "Tier", "Effective", "Target", "Reason"];
    for (i, label) in col_labels.iter().enumerate() {
        let mut lw = crate::osd::to_utf16_z(label);
        let mut rc = RECT {
            left: pad + i as i32 * col_w,
            top: col_y,
            right: pad + (i as i32 + 1) * col_w,
            bottom: col_y + header_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut lw,
                &mut rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }
    }

    // ── Trace rows ───────────────────────────────────────────────
    let trace = crate::llm_proxy::get_trace();
    let list_top = col_y + header_h;

    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
    }

    for (i, entry) in trace.iter().rev().take(50).enumerate() {
        let ry = list_top + i as i32 * row_h;
        if ry + row_h > win_h - pad {
            break;
        }

        let is_downgraded = entry.downgraded;

        // Row background (zebra)
        if i % 2 == 1 {
            let bg = theme.surface.blend_over(theme.background);
            let brush = unsafe { CreateSolidBrush(bg.to_colorref()) };
            unsafe {
                let _ = FillRect(
                    dib_dc,
                    &RECT {
                        left: pad,
                        top: ry,
                        right: win_w - pad,
                        bottom: ry + row_h,
                    },
                    brush,
                );
                let _ = DeleteObject(brush);
            }
        }

        // Dimmed if not downgraded
        let text_color = if is_downgraded {
            theme.text
        } else {
            theme.text_muted
        };
        unsafe {
            let _ = SetTextColor(dib_dc, text_color.to_colorref());
        }

        let values = [
            entry.seq.to_string(),
            format!("{:?}", entry.tier),
            format!("{:?}", entry.effective_tier),
            entry.target.clone(),
            if entry.downgraded {
                format!("⚠ {}", entry.reason)
            } else {
                String::new()
            },
        ];

        for (j, val) in values.iter().enumerate() {
            let mut vw = crate::osd::to_utf16_z(val);
            let mut rc = RECT {
                left: pad + j as i32 * col_w,
                top: ry,
                right: pad + (j as i32 + 1) * col_w,
                bottom: ry + row_h,
            };
            unsafe {
                let _ = DrawTextW(
                    dib_dc,
                    &mut vw,
                    &mut rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
        }
    }

    // ── Finalize ──────────────────────────────────────────────────
    unsafe {
        let _ = SelectObject(dib_dc, _old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    frame.fix_gdi_alpha(theme.background);

    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
    }
    frame.present_layered(hwnd, wr.left, wr.top, 255);
}

// ── Window procedure ─────────────────────────────────────────────────

unsafe extern "system" fn panel_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let scale = { let dpi = GetDpiForWindow(hwnd) as f32; dpi / 96.0 };
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            paint_panel(hwnd, scale, wr.right - wr.left, wr.bottom - wr.top);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) == 0 {
                // Lost focus — hide after timeout
                let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
            } else {
                let _ = KillTimer(hwnd, HIDE_TIMER_ID);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER if wparam.0 == HIDE_TIMER_ID => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == REFRESH_TIMER_ID => {
            // Auto-refresh trace every second
            paint_panel(hwnd, 0.0, 0, 0);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u32 == 0x1B /* VK_ESCAPE */ => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme;
            if !ptr.is_null() {
                let _ = Box::from_raw(ptr);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
