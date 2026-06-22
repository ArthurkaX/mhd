//! Real-time proxy trace overlay.
//!
//! Shows recent routing decisions made by the embedded LLM proxy in a small
//! transparent window. Each row is one request: original tier, effective tier,
//! target model, and a "downgraded" badge.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::config::editor_state::ButtonStyle;
use crate::config::editor_theme::draw_button;
use crate::core::llm_proxy;
use crate::core::native_theme::NativeTheme;

// ── Constants ────────────────────────────────────────────────────────

const WIN_W_BASE: i32 = 520;
const WIN_H_BASE: i32 = 560;
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
    let bits = frame.pixels_mut().as_mut_ptr() as *mut core::ffi::c_void;

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
    let close_btn_w = (20.0 * scale) as i32;
    let close_btn_x = win_w - pad - close_btn_w;
    let close_btn_y = pad;

    unsafe {
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut title_wz = crate::osd::to_utf16_z("Proxy Trace");
    let mut title_rc = RECT {
        left: pad,
        top: pad,
        right: win_w - pad - close_btn_w - (4.0 * scale) as i32,
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

    // Status (right-aligned, before close button)
    // Status text — occupies space left of the close button
    let mut status_rc = RECT {
        left: title_rc.right + (4.0 * scale) as i32,
        top: pad,
        right: close_btn_x - (4.0 * scale) as i32,
        bottom: pad + font_h.abs() + 4,
    };
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
            &mut status_rc,
            DT_RIGHT | DT_SINGLELINE,
        );
    }

    // Debug button
    let debug_btn_w = (42.0 * scale) as i32;
    let debug_btn_h = (20.0 * scale) as i32;
    let debug_btn_x = close_btn_x - (4.0 * scale) as i32 - debug_btn_w;
    let debug_btn_y = close_btn_y + (close_btn_w - debug_btn_h) / 2;
    let debug_on = crate::llm_proxy::is_debug_logging();
    let debug_style = if debug_on {
        ButtonStyle::Success
    } else {
        ButtonStyle::Secondary
    };
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        debug_btn_x,
        debug_btn_y,
        debug_btn_w,
        debug_btn_h,
        "Debug",
        theme,
        hfont_small,
        false,
        debug_style,
    );

    // Draw close × button
    let close_btn_rect = RECT {
        left: close_btn_x,
        top: close_btn_y,
        right: close_btn_x + close_btn_w,
        bottom: close_btn_y + close_btn_w,
    };
    let close_brush =
        unsafe { CreateSolidBrush(theme.hover.blend_over(theme.background).to_colorref()) };
    unsafe {
        let _ = FillRect(dib_dc, &close_btn_rect, close_brush);
        let _ = DeleteObject(close_brush);
    }
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut close_wz = crate::osd::to_utf16_z("\u{00D7}");
    let mut close_text_rc = RECT {
        left: close_btn_rect.left + 2,
        top: close_btn_rect.top,
        right: close_btn_rect.right + 4,
        bottom: close_btn_rect.bottom,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut close_wz,
            &mut close_text_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
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

    let total_cw = win_w - pad * 2;
    // Column widths: fixed reasonable sizes so Tier/Eff fit "Sonnet" comfortably
    let col_widths = [
        (32.0 * scale) as i32, // # (~32px)
        (60.0 * scale) as i32, // Tier (~60px)
        (60.0 * scale) as i32, // Eff (~60px)
        total_cw - ((32.0 + 60.0 + 60.0 + 48.0 + 48.0 + 90.0) * scale) as i32, // Target = remainder
        (48.0 * scale) as i32, // In (~48px)
        (48.0 * scale) as i32, // Out (~48px)
        (90.0 * scale) as i32, // Reason (~90px)
    ];
    let col_headers = ["#", "Tier", "Eff", "Target", "In", "Out", "Reason"];
    let mut col_x = pad;
    for (i, label) in col_headers.iter().enumerate() {
        let mut lw = crate::osd::to_utf16_z(label);
        let cw = col_widths[i];
        let mut rc = RECT {
            left: col_x,
            top: col_y,
            right: col_x + cw,
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
        col_x += cw;
    }

    // ── Trace rows ───────────────────────────────────────────────
    let trace = llm_proxy::get_trace();
    let vision_trace = llm_proxy::get_vision_trace();
    let list_top = col_y + header_h;
    let max_rows: i32 = if vision_trace.is_empty() { 50 } else { 32 };

    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
    }

    for (i, entry) in trace.iter().rev().take(max_rows as usize).enumerate() {
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

        fn fmt_tokens(n: u64) -> String {
            if n == 0 {
                "\u{2014}".to_string() // em dash
            } else if n >= 1000 {
                format!("{:.1}k", n as f64 / 1000.0)
            } else {
                n.to_string()
            }
        }

        let values = [
            entry.seq.to_string(),
            format!("{:?}", entry.tier),
            format!("{:?}", entry.effective_tier),
            entry.target.clone(),
            fmt_tokens(entry.input_tokens),
            fmt_tokens(entry.output_tokens),
            if entry.downgraded {
                format!("\u{26A0} {}", entry.reason)
            } else {
                String::new()
            },
        ];

        let mut col_x = pad;
        for (j, val) in values.iter().enumerate() {
            let mut vw = crate::osd::to_utf16_z(val);
            let cw = col_widths[j];
            let mut rc = RECT {
                left: col_x,
                top: ry,
                right: col_x + cw,
                bottom: ry + row_h,
            };
            col_x += cw;
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

    // ── Vision trace section ─────────────────────────────────────
    if !vision_trace.is_empty() {
        let vision_top = list_top + max_rows * row_h + (8.0 * scale) as i32;
        if vision_top + row_h <= win_h - pad {
            // Separator + title
            let sep_y = vision_top - (4.0 * scale) as i32;
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

            unsafe {
                let _ = SetTextColor(dib_dc, theme.text.to_colorref());
            }
            let mut title_wz = crate::osd::to_utf16_z("Vision");
            let mut title_rc = RECT {
                left: pad,
                top: vision_top,
                right: win_w - pad,
                bottom: vision_top + row_h,
            };
            unsafe {
                let _ = DrawTextW(
                    dib_dc,
                    &mut title_wz,
                    &mut title_rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                );
            }

            unsafe {
                let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
            }
            let mut col_x = pad;
            let v_col_headers = ["#", "Status", "Provider / Model", "Endpoint"];
            let v_col_widths = [
                (32.0 * scale) as i32,
                (50.0 * scale) as i32,
                (160.0 * scale) as i32,
                total_cw - ((32.0 + 50.0 + 160.0) * scale) as i32,
            ];
            let hdr_y = vision_top + row_h;
            for (i, label) in v_col_headers.iter().enumerate() {
                let mut lw = crate::osd::to_utf16_z(label);
                let cw = v_col_widths[i];
                let mut rc = RECT {
                    left: col_x,
                    top: hdr_y,
                    right: col_x + cw,
                    bottom: hdr_y + header_h,
                };
                unsafe {
                    let _ = DrawTextW(
                        dib_dc,
                        &mut lw,
                        &mut rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                    );
                }
                col_x += cw;
            }

            let v_list_top = hdr_y + header_h;
            for (i, entry) in vision_trace.iter().rev().take(5).enumerate() {
                let ry = v_list_top + i as i32 * row_h;
                if ry + row_h > win_h - pad {
                    break;
                }

                let status_text = match entry.status {
                    Some(s) => s.to_string(),
                    None => "…".to_string(),
                };
                let status_color = match entry.status {
                    Some(200..=299) => theme.accent,
                    _ => theme.text_muted,
                };
                let model_text = format!("{} / {}", entry.provider, entry.model);
                let endpoint_text = if let Some(ref err) = entry.error {
                    format!("{} — {}", entry.endpoint, err)
                } else {
                    entry.endpoint.clone()
                };

                let values = [
                    entry.seq.to_string(),
                    status_text,
                    model_text,
                    endpoint_text,
                ];

                let mut col_x = pad;
                for (j, val) in values.iter().enumerate() {
                    let color = if j == 1 {
                        status_color
                    } else {
                        theme.text_muted
                    };
                    unsafe {
                        let _ = SetTextColor(dib_dc, color.to_colorref());
                    }
                    let mut vw = crate::osd::to_utf16_z(val);
                    let cw = v_col_widths[j];
                    let mut rc = RECT {
                        left: col_x,
                        top: ry,
                        right: col_x + cw,
                        bottom: ry + row_h,
                    };
                    col_x += cw;
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
    unsafe {
        match msg {
            WM_NCHITTEST => {
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let mut pt = POINT { x, y };
                let _ = ScreenToClient(hwnd, &mut pt);
                let scale = {
                    let dpi = GetDpiForWindow(hwnd) as f32;
                    dpi / 96.0
                };
                let pad = (PAD_BASE as f32 * scale) as i32;
                let btn_size = (20.0 * scale) as i32;
                let close_btn_w = btn_size;
                let debug_btn_w = (42.0 * scale) as i32;
                let debug_btn_h = (20.0 * scale) as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let win_w = wr.right - wr.left;
                let close_btn_x = win_w - pad - close_btn_w;
                let debug_btn_x = close_btn_x - (4.0 * scale) as i32 - debug_btn_w;
                let debug_btn_y = pad + (close_btn_w - debug_btn_h) / 2;
                // Check close button first
                let btn_left = win_w - pad - btn_size;
                if pt.x >= btn_left - 4
                    && pt.x < win_w - pad + 4
                    && pt.y >= pad - 4
                    && pt.y < pad + btn_size + 4
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Check debug button
                if pt.x >= debug_btn_x
                    && pt.x < debug_btn_x + debug_btn_w
                    && pt.y >= debug_btn_y
                    && pt.y < debug_btn_y + debug_btn_h
                {
                    return LRESULT(HTCLIENT as isize);
                }
                let font_h = -(14.0 * scale) as i32;
                let header_bottom = pad + font_h.abs() + 8 + 4 + (28.0 * scale) as i32;
                if pt.y < header_bottom {
                    return LRESULT(HTCAPTION as isize);
                }
                LRESULT(HTCLIENT as isize)
            }
            WM_SETCURSOR => {
                if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                    let _ = SetCursor(LoadCursorW(None, IDC_ARROW).unwrap_or_default());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                let scale = {
                    let dpi = GetDpiForWindow(hwnd) as f32;
                    dpi / 96.0
                };
                let pad = (PAD_BASE as f32 * scale) as i32;
                let btn_size = (20.0 * scale) as i32;
                let debug_btn_w = (42.0 * scale) as i32;
                let debug_btn_h = (20.0 * scale) as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let win_w = wr.right - wr.left;
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let close_btn_x = win_w - pad - btn_size;
                let debug_btn_x = close_btn_x - (4.0 * scale) as i32 - debug_btn_w;
                let debug_btn_y = pad + (btn_size - debug_btn_h) / 2;
                // Check debug button
                if x >= debug_btn_x
                    && x < debug_btn_x + debug_btn_w
                    && y >= debug_btn_y
                    && y < debug_btn_y + debug_btn_h
                {
                    crate::llm_proxy::toggle_debug_logging();
                    paint_panel(hwnd, 0.0, 0, 0);
                    return LRESULT(0);
                }
                // Check close button
                let btn_left = win_w - pad - btn_size;
                if x >= btn_left - 4
                    && x < win_w - pad + 4
                    && y >= pad - 4
                    && y < pad + btn_size + 4
                {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let scale = {
                    let dpi = GetDpiForWindow(hwnd) as f32;
                    dpi / 96.0
                };
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                paint_panel(hwnd, scale, wr.right - wr.left, wr.bottom - wr.top);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) == 0 {
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
                paint_panel(hwnd, 0.0, 0, 0);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 as u32 == 0x1B => {
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
}
