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
use crate::core::native_theme::{Argb, NativeTheme};

// ── Constants ────────────────────────────────────────────────────────

const WIN_W_BASE: i32 = 668;
const WIN_H_BASE: i32 = 560;
const PAD_BASE: i32 = 12;
const RADIUS_BASE: f32 = 10.0;
const HEADER_H_BASE: i32 = 28;
const ROW_H_BASE: i32 = 22;
const REFRESH_TIMER_ID: usize = 1;

// ── Safe wrapper for the event handle ────────────────────────────────

struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

// ── Toggle / show ───────────────────────────────────────────────────

pub fn show(theme: &NativeTheme) {
    // If a trace window already exists (e.g. minimized via the − button, which
    // minimizes it to the taskbar), restore that one instead of spawning a
    // duplicate thread + window and orphaning the existing instance.
    let cls_name = crate::osd::to_utf16_z("mhd_proxy_trace_cls");
    if let Ok(existing) = unsafe { FindWindowW(PCWSTR::from_raw(cls_name.as_ptr()), PCWSTR::null()) }
    {
        if !existing.is_invalid() {
            unsafe {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetWindowPos(
                    existing,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE,
                );
                let _ = SetForegroundWindow(existing);
            }
            return;
        }
    }

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
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_APPWINDOW,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_MINIMIZEBOX,
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

    // Set window icon for taskbar button and Alt+Tab
    let icon = crate::overlays::tray::load_tray_icon();
    unsafe {
        let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(0), LPARAM(icon.0 as isize));
    }
    unsafe {
        let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(1), LPARAM(icon.0 as isize));
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
    let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - close_btn_w;
    let debug_btn_x = minimize_btn_x - (4.0 * scale) as i32 - debug_btn_w;
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

    // Note button (left of Debug)
    let note_btn_w = (42.0 * scale) as i32;
    let note_btn_h = (20.0 * scale) as i32;
    let note_btn_x = debug_btn_x - (4.0 * scale) as i32 - note_btn_w;
    let note_btn_y = debug_btn_y;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        note_btn_x,
        note_btn_y,
        note_btn_w,
        note_btn_h,
        "Note",
        theme,
        hfont_small,
        false,
        ButtonStyle::Secondary,
    );

    // Minimize button (between debug and close)
    let minimize_btn_rect = RECT {
        left: minimize_btn_x,
        top: close_btn_y,
        right: minimize_btn_x + close_btn_w,
        bottom: close_btn_y + close_btn_w,
    };
    let minimize_brush =
        unsafe { CreateSolidBrush(theme.hover.blend_over(theme.background).to_colorref()) };
    unsafe {
        let _ = FillRect(dib_dc, &minimize_btn_rect, minimize_brush);
        let _ = DeleteObject(minimize_brush);
    }
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let mut min_wz = crate::osd::to_utf16_z("\u{2212}");
    let mut min_text_rc = RECT {
        left: minimize_btn_rect.left + 2,
        top: minimize_btn_rect.top,
        right: minimize_btn_rect.right + 4,
        bottom: minimize_btn_rect.bottom,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut min_wz,
            &mut min_text_rc,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

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

    // ── Fetch trace snapshot (needed for summary + rows) ─────────
    let trace = llm_proxy::get_trace();
    let total_cw = win_w - pad * 2;

    // ── Summary line ─────────────────────────────────────────────
    let summary_y = sep_y + (4.0 * scale) as i32;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    {
        let n = trace.len();
        // Trim avg: average saved% over entries that had trim applied with savings.
        let (trim_count, trim_sum) =
            trace.iter().fold((0usize, 0.0f64), |(cnt, sum), e| {
                if e.trim_applied && e.trim_tokens_before > 0 {
                    let saved = e.trim_tokens_before.saturating_sub(e.trim_tokens_after);
                    if saved > 0 {
                        let pct = saved as f64 / e.trim_tokens_before as f64 * 100.0;
                        (cnt + 1, sum + pct)
                    } else {
                        (cnt, sum)
                    }
                } else {
                    (cnt, sum)
                }
            });
        // Cache-hit avg: average cache_read/total_prompt% over sizeable requests.
        let (cache_count, cache_sum) =
            trace.iter().fold((0usize, 0.0f64), |(cnt, sum), e| {
                let total = e.input_tokens + e.cache_read_tokens + e.cache_creation_tokens;
                if total >= 1024 {
                    let ratio = e.cache_read_tokens as f64 / total as f64 * 100.0;
                    (cnt + 1, sum + ratio)
                } else {
                    (cnt, sum)
                }
            });
        let trim_avg = if trim_count > 0 {
            format!("{:.0}%", trim_sum / trim_count as f64)
        } else {
            "\u{2014}".to_string()
        };
        let cache_avg = if cache_count > 0 {
            format!("{:.0}%", cache_sum / cache_count as f64)
        } else {
            "\u{2014}".to_string()
        };
        let summary = format!(
            "trim avg {} \u{00B7} cache-hit {} \u{00B7} n={}",
            trim_avg, cache_avg, n
        );
        let mut sw = crate::osd::to_utf16_z(&summary);
        let mut summary_rc = RECT {
            left: pad,
            top: summary_y,
            right: win_w - pad,
            bottom: summary_y + row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut sw,
                &mut summary_rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }
    }

    // ── Column headers ───────────────────────────────────────────
    // Shift down one row to make room for the summary line.
    let col_y = summary_y + row_h;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }

    // Columns: #  Route  Target  In  Out  Cache  Trim
    let col_widths = [
        (32.0 * scale) as i32,  // # (~32px)
        (95.0 * scale) as i32,  // Route (~95px — fits "Sonnet→Haiku")
        total_cw - ((32.0 + 95.0 + 48.0 + 48.0 + 88.0 + 80.0) * scale) as i32, // Target = remainder
        (48.0 * scale) as i32,  // In (~48px)
        (48.0 * scale) as i32,  // Out (~48px)
        (88.0 * scale) as i32,  // Cache (~88px)
        (80.0 * scale) as i32,  // Trim (~80px)
    ];
    let col_headers = ["#", "Route", "Target", "In", "Out", "Cache", "Trim"];
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

        fn fmt_tokens(n: u64) -> String {
            if n == 0 {
                "\u{2014}".to_string() // em dash
            } else if n >= 1000 {
                format!("{:.1}k", n as f64 / 1000.0)
            } else {
                n.to_string()
            }
        }

        /// Convert HSV to Argb. h in degrees [0,360), s/v in [0,1].
        fn hsv_to_argb(h: f64, s: f64, v: f64) -> Argb {
            let c = v * s;
            let hh = h / 60.0;
            let x = c * (1.0 - (hh % 2.0 - 1.0).abs());
            let m = v - c;
            let (r1, g1, b1) = match hh as u8 % 6 {
                0 | 6 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            Argb::new(
                255,
                ((r1 + m) * 255.0) as u8,
                ((g1 + m) * 255.0) as u8,
                ((b1 + m) * 255.0) as u8,
            )
        }

        // total_prompt = fresh input + cache reads + cache writes.
        let total_prompt =
            entry.input_tokens + entry.cache_read_tokens + entry.cache_creation_tokens;
        let cache_hit = entry.cache_read_tokens > 0;
        let cache_miss = entry.cache_read_tokens == 0 && total_prompt >= 1024;
        let cache_ratio = if total_prompt > 0 {
            entry.cache_read_tokens as f64 / total_prompt as f64
        } else {
            0.0
        };

        // ── Route column (col 1) ─────────────────────────────────
        // Downgraded: "{tier}→{eff}" in accent; otherwise "{tier}" in muted.
        let (route_text, route_color) = if entry.downgraded {
            (
                format!("{:?}\u{2192}{:?}", entry.tier, entry.effective_tier),
                theme.accent,
            )
        } else {
            (format!("{:?}", entry.tier), theme.text_muted)
        };

        // ── Cache column (col 5) ─────────────────────────────────
        let cache_text = if cache_hit {
            let ratio_pct = cache_ratio * 100.0;
            if entry.cache_creation_tokens > 0 {
                format!(
                    "{:.0}% +{}",
                    ratio_pct,
                    fmt_tokens(entry.cache_creation_tokens)
                )
            } else {
                format!("{:.0}%", ratio_pct)
            }
        } else if cache_miss {
            "miss".to_string()
        } else {
            String::new()
        };
        let cache_color = if cache_hit {
            let t = cache_ratio.clamp(0.0, 1.0);
            hsv_to_argb(120.0 * t, 0.7, 0.9)
        } else if cache_miss {
            let severity = (total_prompt as f64 / 100_000.0).clamp(0.0, 1.0);
            hsv_to_argb(0.0, 0.3 + severity * 0.5, 0.6 + severity * 0.4)
        } else {
            theme.text
        };

        // ── Trim column (col 6) ──────────────────────────────────
        let (trim_text, trim_color) = if entry.trim_applied && entry.trim_tokens_before > 0 {
            let saved = entry.trim_tokens_before.saturating_sub(entry.trim_tokens_after);
            if saved > 0 {
                let pct = saved as f64 / entry.trim_tokens_before as f64 * 100.0;
                (
                    format!("\u{2212}{} ({:.0}%)", fmt_tokens(saved), pct),
                    theme.accent,
                )
            } else {
                (String::new(), theme.text)
            }
        } else {
            (String::new(), theme.text)
        };

        let values = [
            entry.seq.to_string(),
            route_text,
            entry.target.clone(),
            fmt_tokens(total_prompt),
            fmt_tokens(entry.output_tokens),
            cache_text,
            trim_text,
        ];
        // Per-column colors: # and Target/In/Out in theme.text; Route/Cache/Trim have own colors.
        let col_colors = [
            theme.text,
            route_color,
            theme.text,
            theme.text,
            theme.text,
            cache_color,
            trim_color,
        ];

        let mut col_x = pad;
        for (j, val) in values.iter().enumerate() {
            unsafe {
                let _ = SetTextColor(dib_dc, col_colors[j].to_colorref());
            }
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
                let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - btn_size;
                let debug_btn_x = minimize_btn_x - (4.0 * scale) as i32 - debug_btn_w;
                let debug_btn_y = pad + (close_btn_w - debug_btn_h) / 2;
                let note_btn_w = (42.0 * scale) as i32;
                let note_btn_h = (20.0 * scale) as i32;
                let note_btn_x = debug_btn_x - (4.0 * scale) as i32 - note_btn_w;
                let note_btn_y = debug_btn_y;
                // Check close button first
                let btn_left = win_w - pad - btn_size;
                if pt.x >= btn_left - 4
                    && pt.x < win_w - pad + 4
                    && pt.y >= pad - 4
                    && pt.y < pad + btn_size + 4
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Check minimize button
                let min_btn_left = minimize_btn_x;
                if pt.x >= min_btn_left - 4
                    && pt.x < min_btn_left + btn_size + 4
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
                // Check note button
                if pt.x >= note_btn_x
                    && pt.x < note_btn_x + note_btn_w
                    && pt.y >= note_btn_y
                    && pt.y < note_btn_y + note_btn_h
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
                let note_btn_w = (42.0 * scale) as i32;
                let note_btn_h = (20.0 * scale) as i32;
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let win_w = wr.right - wr.left;
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let close_btn_x = win_w - pad - btn_size;
                let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - btn_size;
                let debug_btn_x = minimize_btn_x - (4.0 * scale) as i32 - debug_btn_w;
                let debug_btn_y = pad + (btn_size - debug_btn_h) / 2;
                let note_btn_x = debug_btn_x - (4.0 * scale) as i32 - note_btn_w;
                let note_btn_y = debug_btn_y;
                // Check minimize button
                let min_btn_left = minimize_btn_x;
                if x >= min_btn_left - 4
                    && x < min_btn_left + btn_size + 4
                    && y >= pad - 4
                    && y < pad + btn_size + 4
                {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                    return LRESULT(0);
                }
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
                // Check note button — opens Quick Note writing to proxy.db
                if x >= note_btn_x
                    && x < note_btn_x + note_btn_w
                    && y >= note_btn_y
                    && y < note_btn_y + note_btn_h
                {
                    let state_ptr =
                        GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme;
                    if !state_ptr.is_null() {
                        let theme = (*state_ptr).clone();
                        crate::overlays::note::show(
                            theme,
                            crate::overlays::note::NoteSink::ProxyDb,
                            false,
                        );
                    }
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
            WM_ACTIVATE => {
                // When restored from minimized (taskbar click, Alt+Tab, etc.)
                // force a full repaint.  Layered WS_POPUP windows don't paint
                // themselves correctly after being minimized.
                paint_panel(hwnd, 0.0, 0, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
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
