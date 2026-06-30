//! Live trim-vs-quota measurement overlay.
//!
//! Opens a layered WS_POPUP window that drives
//! `llm_proxy::measure::run_measurement` on a background thread,
//! polls [`MeasureProgress`] on a 500ms timer, and renders the FSM
//! stages.  Launched from the [Bench] button in the Proxy Trace
//! window.

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
use crate::core::native_theme::{Argb, NativeTheme};

// ── Constants ────────────────────────────────────────────────────────

const WIN_W_BASE: i32 = 520;
const WIN_H_BASE: i32 = 420;
const PAD_BASE: i32 = 12;
const RADIUS_BASE: f32 = 10.0;
const HEADER_H_BASE: i32 = 28;
const ROW_H_BASE: i32 = 22;
const REFRESH_TIMER_ID: usize = 1;

// ── Module-level statics ────────────────────────────────────────────

static MEASURE_PROGRESS: std::sync::Mutex<Option<Arc<std::sync::Mutex<llm_proxy::measure::MeasureProgress>>>> =
    std::sync::Mutex::new(None);
static MEASURE_THREAD_RUNNING: AtomicBool = AtomicBool::new(false);
static CONFIRM_REQUESTED: AtomicBool = AtomicBool::new(false);
static CONFIRM_DECIDED: AtomicBool = AtomicBool::new(false);
static CONFIRM_PROCEED: AtomicBool = AtomicBool::new(false);

// ── Safe handle wrapper ─────────────────────────────────────────────

struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

// ── Show / singleton ────────────────────────────────────────────────

pub fn show(theme: &NativeTheme) {
    // If a measure panel already exists (e.g. minimised via the minus
    // button), restore + foreground it instead of spawning a duplicate.
    let cls_name = crate::osd::to_utf16_z("mhd_measure_panel_cls");
    if let Ok(existing) = unsafe { FindWindowW(PCWSTR::from_raw(cls_name.as_ptr()), PCWSTR::null()) } {
        if !existing.is_invalid() {
            unsafe {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetWindowPos(
                    existing,
                    HWND_TOPMOST,
                    0, 0, 0, 0,
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

    // Build fresh progress state before spawning the thread.
    let fresh_progress: Arc<std::sync::Mutex<llm_proxy::measure::MeasureProgress>> =
        Arc::new(std::sync::Mutex::new(llm_proxy::measure::MeasureProgress::new()));
    {
        let mut guard = MEASURE_PROGRESS.lock().unwrap();
        *guard = Some(fresh_progress);
    }
    // Reset confirm gates.
    CONFIRM_REQUESTED.store(false, Ordering::SeqCst);
    CONFIRM_DECIDED.store(false, Ordering::SeqCst);
    CONFIRM_PROCEED.store(false, Ordering::SeqCst);

    std::thread::Builder::new()
        .name("mhd-measure-panel".into())
        .spawn(move || {
            panel_thread(handle, dying, theme_clone);
        })
        .ok();
}

fn panel_thread(event: SafeHandle, dying: Arc<AtomicBool>, theme: NativeTheme) {
    let cls_name = crate::osd::to_utf16_z("mhd_measure_panel_cls");
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
            0, 0, win_w, win_h,
            None, None, hinstance, None,
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
        let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(1), LPARAM(icon.0 as isize));
    }

    // Center on primary monitor
    let work = primary_monitor_work_rect();
    let pos_x = work.left + (work.right - work.left - win_w) / 2;
    let pos_y = work.top + (work.bottom - work.top - win_h) / 2;

    unsafe {
        let _ = SetWindowPos(
            hwnd, HWND_TOPMOST,
            pos_x, pos_y, win_w, win_h,
            SWP_SHOWWINDOW,
        );
    }

    // Start 500ms refresh timer
    unsafe {
        let _ = SetTimer(hwnd, REFRESH_TIMER_ID, 500, None);
    }

    // Paint once so the user sees Idle immediately
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
    let _header_h = (HEADER_H_BASE as f32 * scale) as i32;
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
    let mut title_wz = crate::osd::to_utf16_z("Trim Quota Bench");
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

    // Minimize button (between title area and close)
    let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - close_btn_w;
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

    // Close x button
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

    // ── Read progress snapshot ─────────────────────────────────
    let prog: Option<llm_proxy::measure::MeasureProgress> = {
        MEASURE_PROGRESS.lock().unwrap().as_ref().map(|a| a.lock().unwrap().clone())
    };
    let prog = match prog {
        Some(p) => p,
        None => {
            // No progress object yet — finalize with bare panel and exit.
            unsafe {
                let _ = SelectObject(dib_dc, _old_font);
                let _ = DeleteObject(hfont);
                let _ = DeleteObject(hfont_small);
            }
            frame.fix_gdi_alpha(theme.background);
            let mut wr = RECT::default();
            unsafe { let _ = GetWindowRect(hwnd, &mut wr); }
            frame.present_layered(hwnd, wr.left, wr.top, 255);
            return;
        }
    };

    // ── Content area (below separator) ─────────────────────────
    let content_y = sep_y + (4.0 * scale) as i32;

    match prog.phase {
        llm_proxy::measure::MeasurePhase::Idle | llm_proxy::measure::MeasurePhase::AwaitConfirm => {
            render_idle_confirm(
                dib_dc, bits, hfont_small, theme, scale,
                win_w, win_h, pad, content_y, row_h, &prog,
            );
        }
        llm_proxy::measure::MeasurePhase::Preflight
        | llm_proxy::measure::MeasurePhase::Snapshot
        | llm_proxy::measure::MeasurePhase::RunOff
        | llm_proxy::measure::MeasurePhase::AggregateOff
        | llm_proxy::measure::MeasurePhase::RunOn
        | llm_proxy::measure::MeasurePhase::AggregateOn
        | llm_proxy::measure::MeasurePhase::Compare => {
            render_running(
                dib_dc, bits, hfont_small, theme, scale,
                win_w, win_h, pad, content_y, row_h, &prog,
            );
        }
        llm_proxy::measure::MeasurePhase::Done => {
            render_done(
                dib_dc, bits, hfont_small, theme, scale,
                win_w, win_h, pad, content_y, row_h, &prog,
            );
        }
        llm_proxy::measure::MeasurePhase::Error | llm_proxy::measure::MeasurePhase::Aborted => {
            render_error(
                dib_dc, bits, hfont_small, theme, scale,
                win_w, win_h, pad, content_y, row_h, &prog,
            );
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

// ── Screen renderers ────────────────────────────────────────────────

fn render_idle_confirm(
    dib_dc: HDC, bits: *mut core::ffi::c_void,
    hfont_small: HFONT, theme: &NativeTheme, scale: f32,
    win_w: i32, win_h: i32, pad: i32, y0: i32, row_h: i32,
    prog: &llm_proxy::measure::MeasureProgress,
) {
    let indent = pad + (8.0 * scale) as i32;

    // Description
    let desc_lines = [
        "Measures how much real 5h quota trim saves, by running the",
        "same workload twice — once with trim OFF, once ON.",
    ];
    unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
    for (i, line) in desc_lines.iter().enumerate() {
        let mut wz = crate::osd::to_utf16_z(line);
        let mut rc = RECT {
            left: indent,
            top: y0 + i as i32 * row_h,
            right: win_w - pad,
            bottom: y0 + (i + 1) as i32 * row_h,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // Info table
    let info_y = y0 + 2 * row_h + (4.0 * scale) as i32;
    let trim_engine_str = prog.trim_engine.as_deref().unwrap_or("unknown");
    let info_lines = [
        format!("  Trim engine:   {}", trim_engine_str),
        "  Method:        two warm-cache sessions, proxy-log usage".to_string(),
        "  Spends:        real quota (two agent sessions)".to_string(),
    ];
    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }
    for (i, line) in info_lines.iter().enumerate() {
        let mut wz = crate::osd::to_utf16_z(line);
        let mut rc = RECT {
            left: indent,
            top: info_y + i as i32 * row_h,
            right: win_w - pad,
            bottom: info_y + (i + 1) as i32 * row_h,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // Warning
    let warn_y = info_y + 5 * row_h + (4.0 * scale) as i32;
    unsafe { let _ = SetTextColor(dib_dc, Argb::new(255, 235, 185, 90).to_colorref()); }
    let warn_lines = [
        "\u{26A0} This SPENDS real quota. Do not use this account for",
        "   anything else while the bench runs.",
        "",
        "\u{2139} Cold-cache measurement: result is the upper bound of trim's",
        "   input saving; warm sessions may save less.",
    ];
    for (i, line) in warn_lines.iter().enumerate() {
        let mut wz = crate::osd::to_utf16_z(line);
        let mut rc = RECT {
            left: indent,
            top: warn_y + i as i32 * row_h,
            right: win_w - pad,
            bottom: warn_y + (i + 1) as i32 * row_h,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // Buttons: Start and Cancel
    let btn_w = (80.0 * scale) as i32;
    let btn_h = (24.0 * scale) as i32;
    let btn_y = warn_y + 6 * row_h + (4.0 * scale) as i32;
    let cancel_x = win_w / 2 - btn_w - (4.0 * scale) as i32;
    let start_x = win_w / 2 + (4.0 * scale) as i32;

    draw_button(
        dib_dc, bits, win_w, win_h,
        cancel_x, btn_y, btn_w, btn_h,
        "Cancel", theme, hfont_small, false, ButtonStyle::Secondary,
    );
    draw_button(
        dib_dc, bits, win_w, win_h,
        start_x, btn_y, btn_w, btn_h,
        "Start", theme, hfont_small, false, ButtonStyle::Success,
    );
}

fn render_running(
    dib_dc: HDC, bits: *mut core::ffi::c_void,
    hfont_small: HFONT, theme: &NativeTheme, scale: f32,
    win_w: i32, win_h: i32, pad: i32, y0: i32, row_h: i32,
    prog: &llm_proxy::measure::MeasureProgress,
) {
    let indent = pad + (8.0 * scale) as i32;

    // Subtitle
    let mut sub_wz = crate::osd::to_utf16_z("Trim Quota Bench — running");
    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }
    let mut sub_rc = RECT {
        left: indent, top: y0, right: win_w - pad,
        bottom: y0 + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut sub_wz, &mut sub_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Phase label
    let phase_label = phase_label_text(&prog.phase);
    let msg_y = y0 + row_h;
    unsafe { let _ = SetTextColor(dib_dc, theme.accent.to_colorref()); }
    let mut pl_wz = crate::osd::to_utf16_z(&phase_label);
    let mut pl_rc = RECT {
        left: indent, top: msg_y, right: win_w - pad,
        bottom: msg_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut pl_wz, &mut pl_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Message
    if !prog.message.is_empty() {
        unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
        let mut mw = crate::osd::to_utf16_z(&prog.message);
        let msg_indent = indent + (8.0 * scale) as i32;
        let mut mr = RECT {
            left: msg_indent, top: msg_y + row_h, right: win_w - pad,
            bottom: msg_y + 2 * row_h,
        };
        unsafe {
            let _ = DrawTextW(dib_dc, &mut mw, &mut mr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
        }
    }

    // Arm usage status (filled live from the requests-log aggregates)
    let arms_y = msg_y + 2 * row_h + (4.0 * scale) as i32;

    fn arm_summary(a: &Option<llm_proxy::measure::ArmAggregate>) -> String {
        match a {
            Some(a) => format!(
                "{} reqs \u{00B7} input {} \u{00B7} cache_read {}",
                a.n_requests,
                fmt_tokens(a.input_tokens),
                fmt_tokens(a.cache_read_tokens),
            ),
            None => "\u{2014}".to_string(),
        }
    }

    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }

    let off_label = format!("OFF session   {}", arm_summary(&prog.off));
    let mut ow = crate::osd::to_utf16_z(&off_label);
    let mut or = RECT {
        left: indent, top: arms_y, right: win_w - pad,
        bottom: arms_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut ow, &mut or, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
    }

    let on_label = format!("ON  session   {}", arm_summary(&prog.on));
    let mut nw = crate::osd::to_utf16_z(&on_label);
    let mut nr = RECT {
        left: indent, top: arms_y + row_h, right: win_w - pad,
        bottom: arms_y + 2 * row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut nw, &mut nr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS);
    }

    // Method note
    let spent_y = arms_y + 3 * row_h;
    unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
    let mut sw = crate::osd::to_utf16_z("Measuring real per-request usage from the proxy log.");
    let mut sr = RECT {
        left: indent, top: spent_y, right: win_w - pad,
        bottom: spent_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut sw, &mut sr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Warning
    let caution_y = spent_y + row_h;
    unsafe { let _ = SetTextColor(dib_dc, Argb::new(255, 235, 185, 90).to_colorref()); }
    let mut cw = crate::osd::to_utf16_z("\u{26A0} Don't touch this account while running.");
    let mut cr = RECT {
        left: indent, top: caution_y, right: win_w - pad,
        bottom: caution_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut cw, &mut cr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Close button
    let btn_w = (80.0 * scale) as i32;
    let btn_h = (24.0 * scale) as i32;
    let btn_y = caution_y + row_h + (4.0 * scale) as i32;
    let close_x = win_w / 2 - btn_w / 2;

    draw_button(
        dib_dc, bits, win_w, win_h,
        close_x, btn_y, btn_w, btn_h,
        "Close", theme, hfont_small, false, ButtonStyle::Secondary,
    );
}

fn render_done(
    dib_dc: HDC, bits: *mut core::ffi::c_void,
    hfont_small: HFONT, theme: &NativeTheme, scale: f32,
    win_w: i32, win_h: i32, pad: i32, y0: i32, row_h: i32,
    prog: &llm_proxy::measure::MeasureProgress,
) {
    let indent = pad + (8.0 * scale) as i32;

    // Subtitle
    let mut sub_wz = crate::osd::to_utf16_z("Trim Quota Bench — result");
    unsafe { let _ = SetTextColor(dib_dc, theme.text.to_colorref()); }
    let mut sub_rc = RECT {
        left: indent, top: y0, right: win_w - pad,
        bottom: y0 + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut sub_wz, &mut sub_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Result body — two-run usage comparison from the proxy log.
    let r = prog.result.as_ref();
    let col_off = indent + (180.0 * scale) as i32;
    let col_on  = indent + (300.0 * scale) as i32;

    // Helper to draw one "label  off  on" row.
    let mut row_i = 0i32;
    let table_y = y0 + row_h;
    let mut draw_row = |label: &str, off: String, on: String, color: Argb| {
        let ry = table_y + row_i * row_h;
        row_i += 1;
        unsafe { let _ = SetTextColor(dib_dc, color.to_colorref()); }
        for (text, x) in [(label, indent), (off.as_str(), col_off), (on.as_str(), col_on)] {
            let mut wz = crate::osd::to_utf16_z(text);
            let mut rc = RECT { left: x, top: ry, right: win_w - pad, bottom: ry + row_h };
            unsafe {
                let _ = DrawTextW(dib_dc, &mut wz, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            }
        }
    };

    draw_row("", "OFF".to_string(), "ON".to_string(), theme.text_muted);
    match r {
        Some(r) => {
            draw_row("requests", r.off.n_requests.to_string(), r.on.n_requests.to_string(), theme.text);
            draw_row("input tok", fmt_tokens(r.off.input_tokens), fmt_tokens(r.on.input_tokens), theme.text);
            draw_row("cache create", fmt_tokens(r.off.cache_creation_tokens), fmt_tokens(r.on.cache_creation_tokens), theme.text);
            draw_row("cache read", fmt_tokens(r.off.cache_read_tokens), fmt_tokens(r.on.cache_read_tokens), theme.text);
            draw_row("quota cost*", fmt_tokens(r.billed_input_off), fmt_tokens(r.billed_input_on), theme.text);
            draw_row("quota saved", format!("{:.1}%", r.input_saved_pct), String::new(), theme.accent);
            draw_row("trim raw (ON)", format!("{:.1}%", r.trim_raw_pct), String::new(), theme.accent);
        }
        None => {
            draw_row("requests", "\u{2014}".to_string(), "\u{2014}".to_string(), theme.text_muted);
        }
    }

    // Verdict
    let verdict_text = r.map(|r| r.verdict.as_str()).unwrap_or("n/a");
    let verdict_color = if verdict_text.starts_with("PROVEN") {
        theme.accent
    } else if verdict_text.starts_with("BACKWARDS") {
        Argb::new(255, 235, 100, 100)
    } else {
        theme.text_muted
    };
    let v_y = table_y + (row_i + 1) * row_h;
    unsafe { let _ = SetTextColor(dib_dc, verdict_color.to_colorref()); }
    let mut vw_wz = crate::osd::to_utf16_z(&format!("Verdict: {}", verdict_text));
    let mut vw_rc = RECT { left: indent, top: v_y, right: win_w - pad, bottom: v_y + row_h };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut vw_wz, &mut vw_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Info note
    let note_y = v_y + row_h;
    unsafe { let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref()); }
    let mut note_wz = crate::osd::to_utf16_z(
        "\u{2139} quota cost = input + 1.25\u{00d7}cache_creation + 0.1\u{00d7}cache_read. Saved to history.",
    );
    let mut note_rc = RECT { left: indent, top: note_y, right: win_w - pad, bottom: note_y + row_h };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut note_wz, &mut note_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Close button
    let btn_w = (80.0 * scale) as i32;
    let btn_h = (24.0 * scale) as i32;
    let btn_y = note_y + row_h + (4.0 * scale) as i32;
    let close_x = win_w / 2 - btn_w / 2;

    draw_button(
        dib_dc, bits, win_w, win_h,
        close_x, btn_y, btn_w, btn_h,
        "Close", theme, hfont_small, false, ButtonStyle::Secondary,
    );
}

fn render_error(
    dib_dc: HDC, bits: *mut core::ffi::c_void,
    hfont_small: HFONT, theme: &NativeTheme, scale: f32,
    win_w: i32, win_h: i32, pad: i32, y0: i32, row_h: i32,
    prog: &llm_proxy::measure::MeasureProgress,
) {
    let indent = pad + (8.0 * scale) as i32;
    let is_aborted = prog.phase == llm_proxy::measure::MeasurePhase::Aborted;
    let subtitle = if is_aborted { "Trim Quota Bench — aborted" } else { "Trim Quota Bench — error" };

    unsafe { let _ = SetTextColor(dib_dc, Argb::new(255, 235, 100, 100).to_colorref()); }
    let mut sub_wz = crate::osd::to_utf16_z(subtitle);
    let mut sub_rc = RECT {
        left: indent, top: y0, right: win_w - pad,
        bottom: y0 + row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut sub_wz, &mut sub_rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Error message with wrapping
    let err_text = if is_aborted {
        "The bench was cancelled by the user."
    } else {
        prog.error.as_deref().unwrap_or("An unknown error occurred.")
    };
    unsafe { let _ = SetTextColor(dib_dc, Argb::new(255, 235, 100, 100).to_colorref()); }
    let mut ew = crate::osd::to_utf16_z(err_text);
    let mut er = RECT {
        left: indent, top: y0 + row_h + (4.0 * scale) as i32,
        right: win_w - pad,
        bottom: y0 + 6 * row_h,
    };
    unsafe {
        let _ = DrawTextW(dib_dc, &mut ew, &mut er, DT_LEFT | DT_WORDBREAK);
    }

    // Close button
    let btn_w = (80.0 * scale) as i32;
    let btn_h = (24.0 * scale) as i32;
    let btn_y = y0 + 8 * row_h;
    let close_x = win_w / 2 - btn_w / 2;

    draw_button(
        dib_dc, bits, win_w, win_h,
        close_x, btn_y, btn_w, btn_h,
        "Close", theme, hfont_small, false, ButtonStyle::Secondary,
    );
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Human-readable label for each running-phase.
fn phase_label_text(phase: &llm_proxy::measure::MeasurePhase) -> String {
    match phase {
        llm_proxy::measure::MeasurePhase::Preflight => "Preflight".to_string(),
        llm_proxy::measure::MeasurePhase::Snapshot => "Freezing system corpus".to_string(),
        llm_proxy::measure::MeasurePhase::RunOff => "Running OFF session (trim disabled)".to_string(),
        llm_proxy::measure::MeasurePhase::AggregateOff => "Reading OFF usage".to_string(),
        llm_proxy::measure::MeasurePhase::RunOn => "Running ON session (trim enabled)".to_string(),
        llm_proxy::measure::MeasurePhase::AggregateOn => "Reading ON usage".to_string(),
        llm_proxy::measure::MeasurePhase::Compare => "Comparing arms".to_string(),
        _ => format!("{:?}", phase),
    }
}

/// Format a token count compactly (em-dash for 0, k/M tiers).
fn fmt_tokens(n: u64) -> String {
    if n == 0 {
        "\u{2014}".to_string()
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let win_w = wr.right - wr.left;
                let close_btn_x = win_w - pad - close_btn_w;
                let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - btn_size;

                // Close button zone
                let btn_left = win_w - pad - btn_size;
                if pt.x >= btn_left - 4
                    && pt.x < win_w - pad + 4
                    && pt.y >= pad - 4
                    && pt.y < pad + btn_size + 4
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Minimize button zone
                let min_btn_left = minimize_btn_x;
                if pt.x >= min_btn_left - 4
                    && pt.x < min_btn_left + btn_size + 4
                    && pt.y >= pad - 4
                    && pt.y < pad + btn_size + 4
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Draggable header
                let font_h = -(14.0 * scale) as i32;
                let header_bottom = pad + font_h.abs() + 8 + 4;
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
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                let win_w = wr.right - wr.left;
                let _win_h = wr.bottom - wr.top;
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                let close_btn_x = win_w - pad - btn_size;
                let minimize_btn_x = close_btn_x - (4.0 * scale) as i32 - btn_size;

                // Minimize button
                let min_btn_left = minimize_btn_x;
                if x >= min_btn_left - 4
                    && x < min_btn_left + btn_size + 4
                    && y >= pad - 4
                    && y < pad + btn_size + 4
                {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                    return LRESULT(0);
                }
                // Close header button
                let btn_left = win_w - pad - btn_size;
                if x >= btn_left - 4
                    && x < win_w - pad + 4
                    && y >= pad - 4
                    && y < pad + btn_size + 4
                {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }

                // ── Content-area buttons ──────────────────────────
                let row_h = (ROW_H_BASE as f32 * scale) as i32;
                let sep_y = pad + (14.0 * scale) as i32 + 8;
                let content_y = sep_y + (4.0 * scale) as i32;

                let phase = {
                    let guard = MEASURE_PROGRESS.lock().unwrap();
                    guard.as_ref().map(|a| a.lock().unwrap().phase.clone())
                };
                let phase = phase.unwrap_or(llm_proxy::measure::MeasurePhase::Idle);

                match phase {
                    llm_proxy::measure::MeasurePhase::Idle | llm_proxy::measure::MeasurePhase::AwaitConfirm => {
                        // Cancel / Start — match render_idle_confirm layout.
                        let info_y = content_y + 2 * row_h + (4.0 * scale) as i32;
                        let warn_y = info_y + 5 * row_h + (4.0 * scale) as i32;
                        let btn_y = warn_y + 6 * row_h + (4.0 * scale) as i32;
                        let btn_w = (80.0 * scale) as i32;
                        let btn_h = (24.0 * scale) as i32;
                        let cancel_x = win_w / 2 - btn_w - (4.0 * scale) as i32;
                        let start_x = win_w / 2 + (4.0 * scale) as i32;

                        // Cancel
                        if x >= cancel_x && x < cancel_x + btn_w
                            && y >= btn_y && y < btn_y + btn_h
                        {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        // Start
                        if x >= start_x && x < start_x + btn_w
                            && y >= btn_y && y < btn_y + btn_h
                        {
                            start_measurement();
                            return LRESULT(0);
                        }
                    }
                    llm_proxy::measure::MeasurePhase::Error | llm_proxy::measure::MeasurePhase::Aborted => {
                        let btn_w = (80.0 * scale) as i32;
                        let btn_h = (24.0 * scale) as i32;
                        let btn_y = content_y + 8 * row_h;
                        let close_x = win_w / 2 - btn_w / 2;
                        if x >= close_x && x < close_x + btn_w
                            && y >= btn_y && y < btn_y + btn_h
                        {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    }
                    llm_proxy::measure::MeasurePhase::Done => {
                        // Mirror render_done layout exactly: table_y = content_y + row_h;
                        // 8 table rows -> verdict at +9*row_h, note at +10*row_h, btn below.
                        let table_y = content_y + row_h;
                        let note_y = table_y + 10 * row_h;
                        let btn_y = note_y + row_h + (4.0 * scale) as i32;
                        let btn_w = (80.0 * scale) as i32;
                        let btn_h = (24.0 * scale) as i32;
                        let close_x = win_w / 2 - btn_w / 2;
                        if x >= close_x && x < close_x + btn_w
                            && y >= btn_y && y < btn_y + btn_h
                        {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    }
                    _ => {
                        // Running phases
                        let msg_y = content_y + row_h;
                        let bars_y = msg_y + 2 * row_h + (4.0 * scale) as i32;
                        let spent_y = bars_y + 3 * row_h;
                        let caution_y = spent_y + row_h;
                        let btn_y = caution_y + row_h + (4.0 * scale) as i32;
                        let btn_w = (80.0 * scale) as i32;
                        let btn_h = (24.0 * scale) as i32;
                        let close_x = win_w / 2 - btn_w / 2;
                        if x >= close_x && x < close_x + btn_w
                            && y >= btn_y && y < btn_y + btn_h
                        {
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                    }
                }

                LRESULT(0)
            }
            WM_ACTIVATE => {
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

// ── Start measurement thread ─────────────────────────────────────────

fn start_measurement() {
    if MEASURE_THREAD_RUNNING.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    let progress = { MEASURE_PROGRESS.lock().unwrap().clone() };
    let Some(progress) = progress else {
        MEASURE_THREAD_RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    CONFIRM_DECIDED.store(false, Ordering::SeqCst);
    CONFIRM_PROCEED.store(true, Ordering::SeqCst); // GUI Start means "proceed"

    std::thread::Builder::new().name("mhd-measure-run".into()).spawn(move || {
        let cfg = llm_proxy::measure::MeasureConfig {
            db_path: llm_proxy::config::config_dir().join("proxy.db"),
            dry_run: false,
        };
        // Confirm closure: GUI already confirmed by pressing Start -> return true once.
        let confirm: llm_proxy::measure::ConfirmFn = Box::new(|| true);
        let _ = llm_proxy::measure::run_measurement(&cfg, &progress, confirm);
        MEASURE_THREAD_RUNNING.store(false, Ordering::SeqCst);
    }).ok();
}
