//! Tune advisor overlay — sweeps tool_result_head across three risk
//! buckets on your logged traffic and lets you apply a new head value.
//!
//! Opens a layered WS_POPUP window that drives
//! `llm_proxy::tune::run_bucket_tune` on a background thread,
//! polls [`TuneProgress`] on a 500ms timer, and renders the sweep
//! tables + apply controls. Launched from the [Tune] button in the
//! Proxy Trace window.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
const WIN_H_BASE: i32 = 920;
const PAD_BASE: i32 = 12;
const RADIUS_BASE: f32 = 10.0;
const HEADER_H_BASE: i32 = 28;
const ROW_H_BASE: i32 = 22;
const REFRESH_TIMER_ID: usize = 1;
const SWEEP_VALUES: [usize; 9] = [100, 200, 300, 500, 1000, 2000, 3000, 5000, 8000];
const BUCKET_LABELS: [&str; 3] = ["Native", "CcGateway", "OtherOpenai"];

// ── Module-level statics ────────────────────────────────────────────

static TUNE_PROGRESS: std::sync::Mutex<Option<Arc<std::sync::Mutex<TuneProgress>>>> =
    std::sync::Mutex::new(None);
static TUNE_THREAD_RUNNING: AtomicBool = AtomicBool::new(false);
static SELECTED_HEAD: AtomicUsize = AtomicUsize::new(0);
static APPLIED_HEAD: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

// ── Data types ──────────────────────────────────────────────────────

#[derive(Clone)]
struct PanelSweepPoint {
    desc_chars: usize,
    avg_trim_pct: f64,
    /// Computed for the tune panel; not yet rendered.
    #[allow(dead_code)]
    n_trimmed: usize,
    fail_open_ok: bool,
}

#[derive(Clone)]
struct PanelBucketResult {
    points: Vec<PanelSweepPoint>,
    baseline_desc_chars: usize,
    /// Computed for the tune panel; not yet rendered.
    #[allow(dead_code)]
    baseline_trim_pct: f64,
    recommended: usize,
    /// Computed for the tune panel; not yet rendered.
    #[allow(dead_code)]
    recommended_trim_pct: f64,
    verdict: String,
    /// Computed for the tune panel; not yet rendered.
    #[allow(dead_code)]
    n_bodies: usize,
    /// Computed for the tune panel; not yet rendered.
    #[allow(dead_code)]
    elapsed_ms: u64,
}

#[derive(Clone)]
struct TuneProgress {
    running: bool,
    error: Option<String>,
    current_bucket: usize,
    bucket_done: usize,
    bucket_total: usize,
    results: [Option<PanelBucketResult>; 3],
}

impl TuneProgress {
    fn new() -> Self {
        Self {
            running: false,
            error: None,
            current_bucket: 0,
            bucket_done: 0,
            bucket_total: SWEEP_VALUES.len(),
            results: [None, None, None],
        }
    }
}

// ── Safe handle wrapper ─────────────────────────────────────────────

struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

// ── Show / singleton ────────────────────────────────────────────────

pub fn show(theme: &NativeTheme) {
    let cls_name = crate::osd::to_utf16_z("mhd_tune_panel_cls");
    if let Ok(existing) =
        unsafe { FindWindowW(PCWSTR::from_raw(cls_name.as_ptr()), PCWSTR::null()) }
    {
        if !existing.is_invalid() {
            unsafe {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetWindowPos(existing, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                let _ = SetForegroundWindow(existing);
            }
            return;
        }
    }

    // Init selected head from current settings
    if let Ok(settings) = llm_proxy::config::load_settings() {
        SELECTED_HEAD.store(settings.trim_toolresult_head, Ordering::Relaxed);
    }
    *APPLIED_HEAD.lock().unwrap() = None;

    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(e) => {
            eprintln!("mhd: tune-panel: CreateEventW failed: {e}");
            return;
        }
    };
    let handle = SafeHandle(event);
    let dying = Arc::new(AtomicBool::new(false));
    let theme_clone = theme.clone();

    let fresh_progress: Arc<std::sync::Mutex<TuneProgress>> =
        Arc::new(std::sync::Mutex::new(TuneProgress::new()));
    {
        let mut guard = TUNE_PROGRESS.lock().unwrap();
        *guard = Some(fresh_progress);
    }

    std::thread::Builder::new()
        .name("mhd-tune-panel".into())
        .spawn(move || {
            panel_thread(handle, dying, theme_clone);
        })
        .ok();
}

// ── Panel thread ───────────────────────────────────────────────────

fn panel_thread(event: SafeHandle, dying: Arc<AtomicBool>, theme: NativeTheme) {
    let cls_name = crate::osd::to_utf16_z("mhd_tune_panel_cls");
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

    let icon = crate::overlays::tray::load_tray_icon();
    unsafe {
        let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(0), LPARAM(icon.0 as isize));
        let _ = SendMessageW(hwnd, WM_SETICON, WPARAM(1), LPARAM(icon.0 as isize));
    }

    let work = primary_monitor_work_rect();
    let pos_x = work.left + (work.right - work.left - win_w) / 2;
    let pos_y = work.top + (work.bottom - work.top - win_h) / 2;

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

    unsafe {
        let _ = SetTimer(hwnd, REFRESH_TIMER_ID, 500, None);
    }

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

// ── Paint helpers ──────────────────────────────────────────────────

fn primary_monitor_work_rect() -> RECT {
    crate::renderer::primary_monitor_work_rect()
}

/// Scale trim% (0..~50%) to a 5-segment ▰▱ bar.
fn trim_bar(pct: f64) -> String {
    let f = ((pct / 50.0) * 5.0).floor() as i32;
    let f = f.clamp(0, 5);
    let mut s = String::with_capacity(5);
    for _ in 0..f {
        s.push('\u{25B0}');
    }
    for _ in f..5 {
        s.push('\u{25B1}');
    }
    s
}

/// Risk-zone colour for a head value: green >=2000, amber 500-1999, red <500.
fn head_zone_color(head: usize) -> Argb {
    if head >= 2000 {
        Argb::new(255, 80, 200, 120)
    } else if head >= 500 {
        Argb::new(255, 235, 185, 90)
    } else {
        Argb::new(255, 235, 100, 100)
    }
}

fn tune_result_to_panel(r: &llm_proxy::tune::TuneResult) -> PanelBucketResult {
    PanelBucketResult {
        points: r
            .points
            .iter()
            .map(|p| PanelSweepPoint {
                desc_chars: p.desc_chars,
                avg_trim_pct: p.avg_trim_pct,
                n_trimmed: p.n_trimmed,
                fail_open_ok: p.fail_open_ok,
            })
            .collect(),
        baseline_desc_chars: r.baseline_desc_chars,
        baseline_trim_pct: r.baseline_trim_pct,
        recommended: r.recommended,
        recommended_trim_pct: r.recommended_trim_pct,
        verdict: format!("{:?}", r.verdict),
        n_bodies: r.n_bodies,
        elapsed_ms: r.elapsed_ms,
    }
}

// ── Paint ────────────────────────────────────────────────────────────

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
    let mut title_wz = crate::osd::to_utf16_z("Tune Tool Result Head");
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

    // Minimize button
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
    let prog: Option<TuneProgress> = {
        TUNE_PROGRESS
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.lock().unwrap().clone())
    };
    let prog = match prog {
        Some(p) => p,
        None => {
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
            return;
        }
    };

    // ── Content area (below separator) ─────────────────────────
    let content_y = sep_y + (4.0 * scale) as i32;

    if prog.running {
        render_running(
            dib_dc,
            hfont_small,
            theme,
            scale,
            win_w,
            pad,
            content_y,
            row_h,
            &prog,
        );
    } else if prog.results.iter().any(|r| r.is_some()) {
        render_done(
            dib_dc,
            bits,
            hfont_small,
            theme,
            scale,
            win_w,
            win_h,
            pad,
            content_y,
            row_h,
            &prog,
        );
    } else if prog.error.is_some() {
        // Error state
        unsafe {
            let _ = SelectObject(dib_dc, hfont_small);
            let _ = SetTextColor(dib_dc, Argb::new(255, 235, 100, 100).to_colorref());
        }
        let err_text = format!("Error: {}", prog.error.as_deref().unwrap_or("unknown"));
        let mut ew = crate::osd::to_utf16_z(&err_text);
        let mut erc = RECT {
            left: pad,
            top: content_y,
            right: win_w - pad,
            bottom: content_y + row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut ew,
                &mut erc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    } else {
        render_idle(
            dib_dc,
            bits,
            hfont_small,
            theme,
            scale,
            win_w,
            win_h,
            pad,
            content_y,
            row_h,
        );
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

fn render_idle(
    dib_dc: HDC,
    bits: *mut core::ffi::c_void,
    hfont_small: HFONT,
    theme: &NativeTheme,
    scale: f32,
    win_w: i32,
    win_h: i32,
    pad: i32,
    y0: i32,
    row_h: i32,
) {
    let indent = pad + (8.0 * scale) as i32;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }

    let desc_lines = [
        "Sweeps tool_result_head across three risk buckets",
        "on your logged traffic.  ~85s total.",
    ];
    for (i, line) in desc_lines.iter().enumerate() {
        let mut wz = crate::osd::to_utf16_z(line);
        let mut rc = RECT {
            left: indent,
            top: y0 + i as i32 * row_h,
            right: win_w - pad,
            bottom: y0 + (i + 1) as i32 * row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut wz,
                &mut rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // Run button
    let btn_w = (80.0 * scale) as i32;
    let btn_h = (24.0 * scale) as i32;
    let btn_x = win_w / 2 - btn_w / 2;
    let btn_y = y0 + 2 * row_h + (8.0 * scale) as i32;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        btn_x,
        btn_y,
        btn_w,
        btn_h,
        "Run",
        theme,
        hfont_small,
        false,
        ButtonStyle::Primary,
    );
}

fn render_running(
    dib_dc: HDC,
    hfont_small: HFONT,
    theme: &NativeTheme,
    scale: f32,
    win_w: i32,
    pad: i32,
    y0: i32,
    row_h: i32,
    prog: &TuneProgress,
) {
    let indent = pad + (8.0 * scale) as i32;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }

    let mut run_wz = crate::osd::to_utf16_z("Running Tune...");
    let mut run_rc = RECT {
        left: indent,
        top: y0,
        right: win_w - pad,
        bottom: y0 + row_h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut run_wz,
            &mut run_rc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    for bi in 0..3 {
        let line_y = y0 + (1 + bi) as i32 * row_h;
        let text = if prog.results[bi].is_some() {
            format!("{} completed", BUCKET_LABELS[bi])
        } else if bi == prog.current_bucket {
            format!(
                "{} {}/{}",
                BUCKET_LABELS[bi], prog.bucket_done, prog.bucket_total
            )
        } else {
            format!("{} pending", BUCKET_LABELS[bi])
        };

        let color = if prog.results[bi].is_some() {
            theme.accent
        } else if bi == prog.current_bucket {
            theme.text
        } else {
            theme.text_muted
        };
        unsafe {
            let _ = SetTextColor(dib_dc, color.to_colorref());
        }
        let mut wz = crate::osd::to_utf16_z(&text);
        let mut rc = RECT {
            left: indent,
            top: line_y,
            right: win_w - pad,
            bottom: line_y + row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut wz,
                &mut rc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }
}

fn render_done(
    dib_dc: HDC,
    bits: *mut core::ffi::c_void,
    hfont_small: HFONT,
    theme: &NativeTheme,
    scale: f32,
    win_w: i32,
    win_h: i32,
    pad: i32,
    y0: i32,
    row_h: i32,
    prog: &TuneProgress,
) {
    let indent = pad + (4.0 * scale) as i32;
    let _data_indent = indent + (8.0 * scale) as i32;

    // Column widths for each table
    let head_w = (48.0 * scale) as i32;
    let pct_w = (56.0 * scale) as i32;
    let bar_w = (80.0 * scale) as i32;
    let tags_w = (60.0 * scale) as i32;
    let total_data_w = head_w + pct_w + bar_w + tags_w;
    let data_x = win_w - pad - total_data_w;

    let mut section_y = y0;

    for bi in 0..3 {
        let Some(ref result) = prog.results[bi] else {
            continue;
        };

        // Section header: bucket name + verdict
        let verdict_color = match result.verdict.as_str() {
            "Worthwhile" => Argb::new(255, 80, 200, 120),
            "Marginal" => Argb::new(255, 235, 185, 90),
            _ => theme.text_muted,
        };
        let section_text = format!("{}  {}", BUCKET_LABELS[bi], result.verdict);
        unsafe {
            let _ = SelectObject(dib_dc, hfont_small);
            let _ = SetTextColor(dib_dc, verdict_color.to_colorref());
        }
        let mut sw = crate::osd::to_utf16_z(&section_text);
        let mut src = RECT {
            left: indent,
            top: section_y,
            right: win_w - pad,
            bottom: section_y + row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut sw,
                &mut src,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }

        // Sub-header: col labels
        let sub_hdr_y = section_y + row_h;
        unsafe {
            let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
        }
        let col_labels = ["head", "trim%", "bar", "tags"];
        let col_widths = [head_w, pct_w, bar_w, tags_w];
        let mut col_x = data_x;
        for (ci, label) in col_labels.iter().enumerate() {
            let mut lw = crate::osd::to_utf16_z(label);
            let cw = col_widths[ci];
            let mut rc = RECT {
                left: col_x,
                top: sub_hdr_y,
                right: col_x + cw,
                bottom: sub_hdr_y + row_h,
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

        // Data rows
        let data_start_y = sub_hdr_y + row_h;
        for (ri, pt) in result.points.iter().enumerate() {
            let ry = data_start_y + ri as i32 * row_h;
            if ry + row_h > win_h - pad - (60.0 * scale) as i32 {
                // Reserve bottom space for controls
                break;
            }

            let color = head_zone_color(pt.desc_chars);
            unsafe {
                let _ = SetTextColor(dib_dc, color.to_colorref());
            }

            let pct_str = format!("{:.1}%", pt.avg_trim_pct);
            let bar_str = if pt.fail_open_ok {
                trim_bar(pt.avg_trim_pct)
            } else {
                "!".to_string()
            };

            // Build tags: ‹base and/or ‹rec
            let mut tags = String::new();
            if pt.desc_chars == result.baseline_desc_chars {
                tags.push_str("\u{2039}base");
            }
            if pt.desc_chars == result.recommended {
                if !tags.is_empty() {
                    tags.push(' ');
                }
                tags.push_str("\u{2039}rec");
            }

            let values = [pt.desc_chars.to_string(), pct_str, bar_str, tags];
            let mut col_x = data_x;
            for (ci, val) in values.iter().enumerate() {
                let cw = col_widths[ci];
                let mut vw = crate::osd::to_utf16_z(val);
                let mut rc = RECT {
                    left: col_x,
                    top: ry,
                    right: col_x + cw,
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
                col_x += cw;
            }
        }

        section_y = data_start_y + result.points.len() as i32 * row_h + (4.0 * scale) as i32;
    }

    // ── Head selection controls ─────────────────────────────────
    let sel_head = SELECTED_HEAD.load(Ordering::Relaxed);
    let sel_y = section_y + (4.0 * scale) as i32;

    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text.to_colorref());
    }
    let sel_text = format!("Selected head: {}", sel_head);
    let mut stw = crate::osd::to_utf16_z(&sel_text);
    let mut strc = RECT {
        left: indent,
        top: sel_y,
        right: win_w - pad,
        bottom: sel_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut stw,
            &mut strc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // Preset buttons (9 values in a row)
    let preset_w = (44.0 * scale) as i32;
    let preset_gap = (4.0 * scale) as i32;
    let preset_h = (22.0 * scale) as i32;
    let presets_total = 9 * preset_w + 8 * preset_gap;
    let presets_x = (win_w - presets_total) / 2;
    let presets_y = sel_y + row_h + (4.0 * scale) as i32;

    for (pi, &val) in SWEEP_VALUES.iter().enumerate() {
        let px = presets_x + pi as i32 * (preset_w + preset_gap);
        let is_selected = val == sel_head;
        let style = if is_selected {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Secondary
        };
        draw_button(
            dib_dc,
            bits,
            win_w,
            win_h,
            px,
            presets_y,
            preset_w,
            preset_h,
            &val.to_string(),
            theme,
            hfont_small,
            false,
            style,
        );
    }

    // Apply button
    let apply_w = (80.0 * scale) as i32;
    let apply_h = (24.0 * scale) as i32;
    let apply_x = win_w / 2 - apply_w / 2;
    let apply_y = presets_y + preset_h + (8.0 * scale) as i32;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        apply_x,
        apply_y,
        apply_w,
        apply_h,
        "Apply",
        theme,
        hfont_small,
        false,
        ButtonStyle::Primary,
    );

    // Applied confirmation line.
    // NB: copy the value out and release the lock immediately. std::sync::Mutex
    // is NOT reentrant — holding this guard while the same mutex is locked again
    // below (for the MVP-note layout) self-deadlocks the UI thread.
    let applied: Option<usize> = *APPLIED_HEAD.lock().unwrap();
    if let Some(applied_val) = applied {
        let confirm_y = apply_y + apply_h + (4.0 * scale) as i32;
        unsafe {
            let _ = SetTextColor(dib_dc, theme.accent.to_colorref());
        }
        let confirm_text = format!("Applied head={} — live via file watcher", applied_val);
        let mut cw = crate::osd::to_utf16_z(&confirm_text);
        let mut crc = RECT {
            left: indent,
            top: confirm_y,
            right: win_w - pad,
            bottom: confirm_y + row_h,
        };
        unsafe {
            let _ = DrawTextW(
                dib_dc,
                &mut cw,
                &mut crc,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // MVP note
    let note_y = if applied.is_some() {
        sel_y
            + row_h
            + (4.0 * scale) as i32
            + preset_h
            + (8.0 * scale) as i32
            + apply_h
            + (4.0 * scale) as i32
            + row_h
            + (2.0 * scale) as i32
    } else {
        apply_y + apply_h + (4.0 * scale) as i32 + (2.0 * scale) as i32
    };
    unsafe {
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    let mut nw = crate::osd::to_utf16_z("MVP: applies one global head to all buckets.");
    let mut nrc = RECT {
        left: indent,
        top: note_y,
        right: win_w - pad,
        bottom: note_y + row_h,
    };
    unsafe {
        let _ = DrawTextW(
            dib_dc,
            &mut nw,
            &mut nrc,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
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

                // Read progress to determine state
                let (running, error, results) = {
                    let guard = TUNE_PROGRESS.lock().unwrap();
                    guard
                        .as_ref()
                        .map(|a| {
                            let p = a.lock().unwrap();
                            (
                                p.running,
                                p.error.clone(),
                                [
                                    p.results[0].is_some(),
                                    p.results[1].is_some(),
                                    p.results[2].is_some(),
                                ],
                            )
                        })
                        .unwrap_or((false, None, [false, false, false]))
                };

                if !running && !results.iter().any(|r| *r) && error.is_none() {
                    // Idle state — Run button hit-test
                    let btn_w = (80.0 * scale) as i32;
                    let btn_h = (24.0 * scale) as i32;
                    let btn_x = win_w / 2 - btn_w / 2;
                    let btn_y = content_y + 2 * row_h + (8.0 * scale) as i32;
                    if x >= btn_x && x < btn_x + btn_w && y >= btn_y && y < btn_y + btn_h {
                        start_tune_run(hwnd);
                        return LRESULT(0);
                    }
                }

                if !running && results.iter().any(|r| *r) {
                    // Done state — preset + Apply buttons
                    // Recalculate layout to match render_done.
                    // Scan through bucket sections to find y after all tables.
                    let mut scan_y = content_y;
                    for bi in 0..3 {
                        if results[bi] {
                            // bucket section: header + sub-header + data rows
                            let n_points = {
                                let guard = TUNE_PROGRESS.lock().unwrap();
                                guard
                                    .as_ref()
                                    .map(|a| {
                                        let p = a.lock().unwrap();
                                        p.results[bi].as_ref().map(|r| r.points.len()).unwrap_or(0)
                                    })
                                    .unwrap_or(0)
                            };
                            scan_y += (2 + n_points) as i32 * row_h + (4.0 * scale) as i32;
                        }
                    }

                    let sel_y = scan_y + (4.0 * scale) as i32;

                    // Preset buttons
                    let preset_w = (44.0 * scale) as i32;
                    let preset_gap = (4.0 * scale) as i32;
                    let preset_h = (22.0 * scale) as i32;
                    let presets_total = 9 * preset_w + 8 * preset_gap;
                    let presets_x = (win_w - presets_total) / 2;
                    let presets_y = sel_y + row_h + (4.0 * scale) as i32;

                    for (pi, &val) in SWEEP_VALUES.iter().enumerate() {
                        let px = presets_x + pi as i32 * (preset_w + preset_gap);
                        if x >= px
                            && x < px + preset_w
                            && y >= presets_y
                            && y < presets_y + preset_h
                        {
                            let sel_head = val;
                            SELECTED_HEAD.store(sel_head, Ordering::Relaxed);
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                    }

                    // Apply button
                    let apply_w = (80.0 * scale) as i32;
                    let apply_h = (24.0 * scale) as i32;
                    let apply_x = win_w / 2 - apply_w / 2;
                    let apply_y = presets_y + preset_h + (8.0 * scale) as i32;
                    if x >= apply_x
                        && x < apply_x + apply_w
                        && y >= apply_y
                        && y < apply_y + apply_h
                    {
                        let head = SELECTED_HEAD.load(Ordering::Relaxed);
                        if let Ok(mut settings) = llm_proxy::config::load_settings() {
                            settings.trim_toolresult_head = head;
                            let _ = llm_proxy::config::save_settings(&settings);
                            *APPLIED_HEAD.lock().unwrap() = Some(head);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
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

// ── Start tune run ───────────────────────────────────────────────────

/// Resets the running flags on ANY thread exit — including a panic — so the
/// panel can never freeze forever on a partial state. Drops at end of the
/// work closure; the 500ms refresh timer then repaints the final state.
struct RunGuard {
    progress: std::sync::Arc<std::sync::Mutex<TuneProgress>>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut p) = self.progress.lock() {
            p.running = false;
        }
        TUNE_THREAD_RUNNING.store(false, Ordering::SeqCst);
    }
}

fn start_tune_run(hwnd: HWND) {
    if TUNE_THREAD_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }

    let progress = { TUNE_PROGRESS.lock().unwrap().clone() };
    let Some(progress) = progress else {
        TUNE_THREAD_RUNNING.store(false, Ordering::SeqCst);
        return;
    };

    // Reset progress
    {
        let mut p = progress.lock().unwrap();
        p.running = true;
        p.error = None;
        p.results = [None, None, None];
        p.current_bucket = 0;
        p.bucket_done = 0;
        p.bucket_total = SWEEP_VALUES.len();
    }

    let db_path = llm_proxy::config::config_dir().join("proxy.db");
    let base = llm_proxy::native_trim::NativeKnobs::default();
    let sweep: Vec<usize> = SWEEP_VALUES.to_vec();
    let floor: usize = 200;
    let max_bodies: usize = 120;
    let knob = llm_proxy::tune::SweepKnob::ToolResultHead;
    let buckets = [
        llm_proxy::tune::Bucket::Native,
        llm_proxy::tune::Bucket::CcGateway,
        llm_proxy::tune::Bucket::OtherOpenai,
    ];

    // Force an immediate repaint so "Running Tune..." appears before DB open.
    paint_panel(hwnd, 0.0, 0, 0);

    std::thread::Builder::new()
        .name("mhd-tune-run".into())
        .spawn(move || {
            let _guard = RunGuard {
                progress: progress.clone(),
            };
            for (idx, &bucket) in buckets.iter().enumerate() {
                {
                    let mut p = progress.lock().unwrap();
                    p.current_bucket = idx;
                    p.bucket_done = 0;
                    p.bucket_total = sweep.len();
                }

                let p2 = progress.clone();
                let callback = move |done: usize, _total: usize| {
                    let mut p = p2.lock().unwrap();
                    p.bucket_done = done;
                };

                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    llm_proxy::tune::run_bucket_tune(
                        &db_path, &base, &sweep, floor, max_bodies, bucket, knob, callback,
                    )
                }));
                match outcome {
                    Ok(Ok(result)) => {
                        let panel = match result {
                            Some(r) => tune_result_to_panel(&r),
                            None => PanelBucketResult {
                                points: Vec::new(),
                                baseline_desc_chars: 0,
                                baseline_trim_pct: 0.0,
                                recommended: 0,
                                recommended_trim_pct: 0.0,
                                verdict: "NoData".to_string(),
                                n_bodies: 0,
                                elapsed_ms: 0,
                            },
                        };
                        let mut p = progress.lock().unwrap();
                        p.results[idx] = Some(panel);
                    }
                    Ok(Err(e)) => {
                        let mut p = progress.lock().unwrap();
                        p.error = Some(e);
                        break;
                    }
                    Err(panic_payload) => {
                        let msg = panic_payload
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic in run_bucket_tune".to_string());
                        let mut p = progress.lock().unwrap();
                        p.error = Some(format!("panic (bucket {}): {}", BUCKET_LABELS[idx], msg));
                        break;
                    }
                }
            }
        })
        .ok();
}
