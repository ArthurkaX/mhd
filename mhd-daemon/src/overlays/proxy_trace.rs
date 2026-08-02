//! Real-time proxy trace overlay.
//!
//! Shows recent routing decisions made by the embedded LLM proxy in a small
//! transparent window. Each row is one request: client family (Claude Code /
//! Codex / OpenAI), original tier, effective tier, target model, and a
//! "downgraded" badge. A filter chip on the summary line narrows the table to
//! one client.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

// NOTE: leading `::` reaches the external `llm_proxy` crate; the bare
// `llm_proxy` name in this file is the daemon's wrapper module (see main.rs).
use ::llm_proxy::state::{ClientKind, TraceEntry, WireApi};

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
/// Anthropic prompt-cache TTL in seconds. A cache miss with a longer preceding
/// idle gap is classified as COLD (expected) rather than a real MISS.
const CACHE_TTL_SECS: u64 = 360;

/// Vertical scroll offset for the trace list, in collapsed visual rows from the
/// top (newest). 0 = pinned to newest (default). Clamped in paint_panel.
static TRACE_SCROLL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Click hit-map for trace rows: (y_top, y_bottom, seq), rebuilt every paint.
/// Lets WM_LBUTTONDOWN map a Y coordinate back to the request under it.
static ROW_HITS: Mutex<Vec<(i32, i32, u64)>> = Mutex::new(Vec::new());

/// Active client filter for the trace list: -1 = all clients, otherwise the
/// index into `ClientKind::all()` (0 = Claude Code, 1 = Codex, 2 = OpenAI).
/// Cycled by the filter chip on the summary row; persists across paints.
static CLIENT_FILTER: AtomicI32 = AtomicI32::new(-1);

/// Click hit-rect for the client-filter chip, rebuilt every paint. Lets the
/// click handlers map a click to the filter toggle without recomputing the
/// whole header layout (mirrors ROW_HITS).
static FILTER_CHIP_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

fn fmt_tokens(n: u64) -> String {
    if n == 0 {
        "\u{2014}".to_string() // em dash
    } else if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Whether a row has reported any token usage at all. `input_tokens` and
/// `output_tokens` are 0 until the response lands and `cache_read_tokens` is
/// `None` on routes that report no cache field, so a row with all three empty
/// is either still streaming (Codex rows close only at end-of-stream) or came
/// from a route that reports no counts — either way rendering "0" would state
/// a fact we do not have, so the In/Out/Cache cells show "—" instead.
fn no_reported_usage(
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
) -> bool {
    input_tokens == 0 && output_tokens == 0 && cache_read_tokens.is_none()
}

/// Whether the four-state cache classifier (COLD / EXPIRED / MISS) may run for
/// a row. It needs `cache_read == 0` and a sizeable prompt, and it is built on
/// `prefix_hash` plus the prefix-seen-before history — which mHD only computes
/// for Anthropic traffic. The Responses wire API never gets a prefix hash (the
/// native Codex path does not parse the body), so its `prefix_hash` is
/// structurally 0 and the `h != 0` guard would fall through to the
/// inter-request gap heuristic, labeling the row cold/expired/miss on pure
/// timing evidence — a guess presented as a fact. Excluded here so those rows
/// stay unclassified instead.
fn cache_classifier_applies(
    wire_api: WireApi,
    cache_read_tokens: Option<u64>,
    total_prompt: u64,
) -> bool {
    cache_read_tokens.is_some_and(|n| n == 0)
        && total_prompt >= 1024
        && wire_api != WireApi::Responses
}

/// Render the Cache column text for a row. Pure: the caller already computed
/// `no_usage`, the hit flag and ratio from `cache_read_tokens`, and the
/// four-state classification booleans. Returns "" when a reported `cache_read
/// == 0` gets no classifier label (the excluded-wire and small-prompt cases),
/// matching how the row renders an empty cell rather than a guess.
fn cache_cell_text(
    no_usage: bool,
    cache_hit: bool,
    cache_ratio: f64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    is_cold: bool,
    is_expired: bool,
    gap_secs: Option<u64>,
    is_miss: bool,
) -> String {
    if no_usage {
        "\u{2014}".to_string()
    } else if cache_hit {
        let ratio_pct = cache_ratio * 100.0;
        match cache_creation {
            Some(cc) if cc > 0 => format!("{:.0}% +{}", ratio_pct, fmt_tokens(cc)),
            _ => format!("{:.0}%", ratio_pct),
        }
    } else if cache_read.is_none() {
        "\u{2014}".to_string()
    } else if is_cold {
        "cold".to_string()
    } else if is_expired {
        match gap_secs {
            Some(g) => format!("expired {}m", g / 60),
            None => "expired".to_string(),
        }
    } else if is_miss {
        "miss".to_string()
    } else {
        String::new()
    }
}

/// Route-column text. Claude Code rows render their tier — "Tier" when plain,
/// "Tier→Tier" when downgraded. Rows from non-Claude clients (Codex, OpenAI
/// passthrough) have no Claude tier at all; `None` must never render a
/// fabricated tier (upstream used to map unknown models to "Sonnet"), so they
/// show the routing target instead — an empty cell is better than a wrong one.
fn route_text(e: &TraceEntry) -> String {
    if e.downgraded {
        match (e.tier, e.effective_tier) {
            (Some(from), Some(to)) => format!("{:?}\u{2192}{:?}", from, to),
            _ => e.target.clone(),
        }
    } else {
        match e.tier {
            Some(tier) => format!("{:?}", tier),
            None => e.target.clone(),
        }
    }
}

/// Target-column text. The routing marker `native` does not tell which model
/// handled the request, so native rows show the model requested by the client.
/// Provider routes already store the effective upstream model in `target`.
fn target_text(e: &TraceEntry) -> String {
    if e.target == "native" && !e.model.is_empty() {
        e.model.clone()
    } else {
        e.target.clone()
    }
}

/// Spawn the standalone request inspector for `(run_id, seq)`. Best-effort:
/// logs on failure, never blocks or panics the UI thread.
fn launch_inspector(run_id: u64, seq: u64) {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mhd-inspector.exe")));
    let Some(exe) = exe else { return };
    let _ = std::process::Command::new(exe)
        .arg("--run-id")
        .arg(run_id.to_string())
        .arg("--seq")
        .arg(seq.to_string())
        .spawn();
}

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
    if let Ok(existing) =
        unsafe { FindWindowW(PCWSTR::from_raw(cls_name.as_ptr()), PCWSTR::null()) }
        && !existing.is_invalid()
    {
        unsafe {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetWindowPos(existing, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetForegroundWindow(existing);
        }
        return;
    }

    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(e) => {
            eprintln!("mhd: proxy-trace: CreateEventW failed: {e}");
            return;
        }
    };
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

    // Tune button (left of Note)
    let tune_btn_w = (44.0 * scale) as i32;
    let tune_btn_h = (20.0 * scale) as i32;
    let tune_btn_x = note_btn_x
        - (4.0 * scale) as i32
        - (48.0 * scale) as i32
        - (4.0 * scale) as i32
        - tune_btn_w;
    let tune_btn_y = debug_btn_y;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        tune_btn_x,
        tune_btn_y,
        tune_btn_w,
        tune_btn_h,
        "Tune",
        theme,
        hfont_small,
        false,
        ButtonStyle::Secondary,
    );

    // Bench button (left of Note)
    let bench_btn_w = (48.0 * scale) as i32;
    let bench_btn_h = (20.0 * scale) as i32;
    let bench_btn_x = note_btn_x - (4.0 * scale) as i32 - bench_btn_w;
    let bench_btn_y = debug_btn_y;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        bench_btn_x,
        bench_btn_y,
        bench_btn_w,
        bench_btn_h,
        "Bench",
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

    // Client filter: which client family the table shows. -1 = all; otherwise
    // an index into ClientKind::all(). Applied to the whole snapshot before the
    // summary stats and the newest-max_rows window, so a Codex filter shows
    // Codex numbers alone (trim/cache averages included).
    let filter_kind = CLIENT_FILTER.load(Ordering::Relaxed);
    let filtered_trace: Vec<_> = trace
        .iter()
        .filter(|e| {
            filter_kind < 0
                || ClientKind::all()
                    .get(filter_kind as usize)
                    .is_some_and(|k| *k == e.client)
        })
        .collect();

    // ── Summary line ─────────────────────────────────────────────
    let summary_y = sep_y + (4.0 * scale) as i32;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }
    {
        let n = filtered_trace.len();
        // Trim avg: average saved% over entries that had trim applied with savings.
        let (trim_count, trim_sum) =
            filtered_trace
                .iter()
                .fold((0usize, 0.0f64), |(cnt, sum), e| {
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
        // Requests whose route reported no cache field are skipped entirely —
        // averaging them in as 0% would drag the number down with non-evidence.
        let (cache_count, cache_sum) =
            filtered_trace
                .iter()
                .fold((0usize, 0.0f64), |(cnt, sum), e| {
                    let Some(cr) = e.cache_read_tokens else {
                        return (cnt, sum);
                    };
                    let total = e.input_tokens + cr + e.cache_creation_tokens.unwrap_or(0);
                    if total >= 1024 {
                        let ratio = cr as f64 / total as f64 * 100.0;
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

    // ── Client filter chip ────────────────────────────────────────
    // A small cycle button on the right of the summary line. Clicking it steps
    // All → Claude Code → Codex → OpenAI so the table can be narrowed to one
    // client (e.g. Codex traffic alone). The hit rect is stashed for the click
    // handlers, mirroring ROW_HITS; the button sits in the caption band, so
    // WM_NCHITTEST exempts it explicitly.
    let chip_label = if filter_kind < 0 {
        "All".to_string()
    } else {
        ClientKind::all()
            .get(filter_kind as usize)
            .map(|k| k.label().to_string())
            .unwrap_or_else(|| "All".to_string())
    };
    let chip_w = (((chip_label.chars().count() as f32) * 6.5 + 18.0) * scale) as i32;
    let chip_h = (20.0 * scale) as i32;
    let chip_x = win_w - pad - chip_w;
    let chip_y = summary_y + (row_h - chip_h) / 2;
    draw_button(
        dib_dc,
        bits,
        win_w,
        win_h,
        chip_x,
        chip_y,
        chip_w,
        chip_h,
        &chip_label,
        theme,
        hfont_small,
        false,
        if filter_kind < 0 {
            ButtonStyle::Secondary
        } else {
            ButtonStyle::Primary
        },
    );
    if let Ok(mut r) = FILTER_CHIP_RECT.lock() {
        *r = Some((chip_x, chip_y, chip_w, chip_h));
    }

    // ── Column headers ───────────────────────────────────────────
    let col_y = summary_y + row_h;
    unsafe {
        let _ = SelectObject(dib_dc, hfont_small);
        let _ = SetTextColor(dib_dc, theme.text_muted.to_colorref());
    }

    // Columns: #  Client  Route  Target  In  Out  Cache  Trim
    let col_widths = [
        (32.0 * scale) as i32, // # (~32px)
        (52.0 * scale) as i32, // Client (~52px — fits "OpenAI"; "CC" for Claude Code)
        (95.0 * scale) as i32, // Route (~95px — fits "Sonnet→Haiku")
        total_cw - ((32.0 + 52.0 + 95.0 + 48.0 + 48.0 + 88.0 + 80.0) * scale) as i32, // Target = remainder
        (48.0 * scale) as i32,                                                        // In (~48px)
        (48.0 * scale) as i32,                                                        // Out (~48px)
        (88.0 * scale) as i32, // Cache (~88px)
        (80.0 * scale) as i32, // Trim (~80px)
    ];
    let col_headers = [
        "#", "Client", "Route", "Target", "In", "Out", "Cache", "Trim",
    ];
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

    // Collect newest-first so we can index [i+1] for the gap-to-previous calc.
    // The client filter is applied upstream, so this is the filtered window.
    let display_entries: Vec<_> = filtered_trace
        .iter()
        .rev()
        .take(max_rows as usize)
        .collect();

    if let Ok(mut hits) = ROW_HITS.lock() {
        hits.clear();
    }

    // Pre-compute whether each entry's prefix_hash was seen in an OLDER entry
    // (i.e. appeared earlier in time = later in the newest-first vector).
    // Strategy: iterate oldest→newest (reverse of display order), maintaining a
    // HashSet of hashes seen so far. An entry has "prefix_seen_before" = true iff
    // its non-zero hash is already in the set when we reach it.
    // We exclude hash==0 (unknown) from matching to avoid false COLD suppression.
    let prefix_seen_before: Vec<bool> = {
        let n = display_entries.len();
        let mut seen_before = vec![false; n];
        let mut seen_set = std::collections::HashSet::<u64>::new();
        // Iterate oldest first (reverse of display_entries which is newest-first)
        for idx in (0..n).rev() {
            let h = display_entries[idx].prefix_hash;
            if h != 0 {
                seen_before[idx] = seen_set.contains(&h);
                seen_set.insert(h);
            }
            // hash==0: seen_before stays false (unknown, handled in classification)
        }
        seen_before
    };

    // Collapse consecutive error rows sharing the same HTTP status into one
    // summary line (e.g. a 32× burst of 429s). `run_len[i]` = how many rows the
    // run starting at i covers; `skip[i]` marks the tail rows of a run (drawn as
    // part of the head's summary). display_entries is newest-first and already
    // time-ordered, so "consecutive" = adjacent indices.
    let n_disp = display_entries.len();
    let mut run_len = vec![1usize; n_disp];
    let mut skip = vec![false; n_disp];
    {
        let mut i = 0usize;
        while i < n_disp {
            // Probe rows collapse together regardless of their exact status
            // (a probe salvo may mix 200 and 429). They render as a quiet gray
            // "xN probe" line instead of a red error burst.
            if display_entries[i].is_probe {
                let mut j = i + 1;
                while j < n_disp && display_entries[j].is_probe {
                    j += 1;
                }
                run_len[i] = j - i;
                for item in skip.iter_mut().take(j).skip(i + 1) {
                    *item = true;
                }
                i = j;
                continue;
            }
            let code = display_entries[i].status.filter(|&s| s >= 400);
            if let Some(code) = code {
                let mut j = i + 1;
                while j < n_disp
                    && !display_entries[j].is_probe
                    && display_entries[j].status == Some(code)
                {
                    j += 1;
                }
                run_len[i] = j - i;
                for item in skip.iter_mut().take(j).skip(i + 1) {
                    *item = true;
                }
                i = j;
            } else {
                i += 1;
            }
        }
    }

    // Output row index: advances only when a row is actually drawn, so collapsed
    // bursts reclaim screen space instead of leaving blank gaps.
    let mut out_row: i32 = 0;
    let mut visual_idx: i32 = 0;
    // Total collapsed (visible) rows and how many fit on screen.
    let total_visual: i32 = skip.iter().filter(|s| !**s).count() as i32;
    let avail_h: i32 = (win_h - pad) - list_top;
    let visible_rows: i32 = (avail_h / row_h).max(1);
    let max_scroll: i32 = (total_visual - visible_rows).max(0);
    let mut scroll = TRACE_SCROLL.load(std::sync::atomic::Ordering::Relaxed);
    if scroll < 0 {
        scroll = 0;
    }
    if scroll > max_scroll {
        scroll = max_scroll;
    }
    TRACE_SCROLL.store(scroll, std::sync::atomic::Ordering::Relaxed);
    for (i, entry) in display_entries.iter().enumerate() {
        if skip[i] {
            continue;
        }
        // Skip visual rows scrolled off the top; stop once the viewport is full.
        if visual_idx < scroll {
            visual_idx += 1;
            continue;
        }
        visual_idx += 1;
        let ry = list_top + out_row * row_h;
        if ry + row_h > win_h - pad {
            break;
        }

        if let Ok(mut hits) = ROW_HITS.lock() {
            hits.push((ry, ry + row_h, entry.seq));
        }

        // Row background (zebra)
        if out_row % 2 == 1 {
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

        // Probe row (single or collapsed salvo): render one quiet gray summary
        // line instead of the red error burst a 429 probe would otherwise be.
        // Probes that 429 are harmless background noise (zero quota), so they
        // should not look like lost work.
        if entry.is_probe {
            let text = if run_len[i] > 1 {
                format!("\u{00d7}{} probe", run_len[i])
            } else {
                "probe".to_string()
            };
            let probe_color = theme.text_muted;
            unsafe {
                let _ = SetTextColor(dib_dc, probe_color.to_colorref());
            }
            let mut pw = crate::osd::to_utf16_z(&text);
            let mut prc = RECT {
                left: pad,
                top: ry,
                right: win_w - pad,
                bottom: ry + row_h,
            };
            unsafe {
                let _ = DrawTextW(
                    dib_dc,
                    &mut pw,
                    &mut prc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
            out_row += 1;
            continue;
        }

        // Error row (single or collapsed burst): render one red summary line
        // instead of the normal 7 columns, then move to the next row.
        if let Some(code) = entry.status.filter(|&s| s >= 400) {
            let label = match code {
                429 => "rate_limit",
                529 => "overloaded",
                503 => "unavailable",
                500..=599 => "server_error",
                401 | 403 => "auth",
                400..=499 => "client_error",
                _ => "error",
            };
            let text = if run_len[i] > 1 {
                format!("\u{00d7}{}  HTTP {}  {}", run_len[i], code, label)
            } else {
                format!("HTTP {}  {}", code, label)
            };
            let err_color = Argb::new(255, 220, 80, 80);
            unsafe {
                let _ = SetTextColor(dib_dc, err_color.to_colorref());
            }
            let mut ew = crate::osd::to_utf16_z(&text);
            let mut erc = RECT {
                left: pad,
                top: ry,
                right: win_w - pad,
                bottom: ry + row_h,
            };
            unsafe {
                let _ = DrawTextW(
                    dib_dc,
                    &mut ew,
                    &mut erc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
            out_row += 1;
            continue;
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

        // total_prompt = fresh input + cache reads + cache writes. Unreported
        // cache counts contribute nothing rather than being read as zero.
        let cache_read = entry.cache_read_tokens;
        let total_prompt =
            entry.input_tokens + cache_read.unwrap_or(0) + entry.cache_creation_tokens.unwrap_or(0);
        let cache_hit = cache_read.is_some_and(|n| n > 0);
        let cache_ratio = match (cache_read, total_prompt) {
            (Some(n), t) if t > 0 => n as f64 / t as f64,
            _ => 0.0,
        };
        // Compute elapsed time (secs) between this entry and the chronologically-
        // previous one. display_entries is newest-first, so the predecessor is [i+1].
        let gap_secs_opt: Option<u64> = if entry.started_ms == 0 {
            None
        } else {
            display_entries.get(i + 1).and_then(|prev| {
                if prev.started_ms == 0 || prev.started_ms > entry.started_ms {
                    None
                } else {
                    Some((entry.started_ms - prev.started_ms) / 1000)
                }
            })
        };
        // Four-state cache classification when the route reported cache_read == 0:
        //   COLD    — prefix hash never seen before (first fill, new project/session)
        //   EXPIRED — prefix was seen before but gap > TTL (cache aged out)
        //   MISS    — prefix was seen before and gap <= TTL (warm but missed → investigate)
        // HIT is handled separately (cache_read > 0).
        // hash==0 (unknown): never treated as COLD; falls to gap logic instead.
        //
        // Two routes are excluded from all four, staying unclassified instead:
        // a route that reported NO cache field at all (cache_read == None) — we
        // know nothing, so calling it a MISS would be inventing a fact — and the
        // Responses wire API, which never gets a prefix hash (the native Codex
        // path does not parse the body), so its structurally-0 hash would fall
        // through to the gap heuristic and label cold/expired/miss on pure
        // timing evidence. Both render a non-classified cell rather than a guess.
        let cache_miss_candidate =
            cache_classifier_applies(entry.wire_api, cache_read, total_prompt);
        let seen_before = prefix_seen_before[i];
        let h = entry.prefix_hash;
        #[derive(PartialEq)]
        enum CacheState {
            Cold,
            Expired,
            Miss,
        }
        let cache_state: Option<CacheState> = if cache_miss_candidate {
            if h != 0 && !seen_before {
                // Genuine first fill: this prefix has never appeared in an older entry.
                Some(CacheState::Cold)
            } else {
                // Prefix was seen before (or hash is unknown): classify by gap.
                let expired = gap_secs_opt.is_none_or(|g| g > CACHE_TTL_SECS);
                if expired {
                    Some(CacheState::Expired)
                } else {
                    Some(CacheState::Miss)
                }
            }
        } else {
            None
        };
        let is_cold = cache_state.as_ref().is_some_and(|s| *s == CacheState::Cold);
        let is_expired = cache_state
            .as_ref()
            .is_some_and(|s| *s == CacheState::Expired);
        let is_miss = cache_state.as_ref().is_some_and(|s| *s == CacheState::Miss);

        // ── Route column (col 2) ─────────────────────────────────
        // Claude Code: "{tier}" in muted, "{tier}→{eff}" in accent when
        // downgraded. Codex / OpenAI passthrough have no Claude tier — show the
        // routing target instead; never a fabricated tier.
        let (route_cell, route_color) = if entry.downgraded {
            (route_text(entry), theme.accent)
        } else {
            (route_text(entry), theme.text_muted)
        };

        // A row with nothing reported is either still streaming (Codex rows
        // close only at end-of-stream, so usage is legitimately absent while in
        // flight) or came from a route that reports no counts. Rendering "0"
        // would state a fact we do not have, so In/Out/Cache show "—" instead.
        // (Codex rows that have closed DO carry real counts via the usage tap,
        // which this rule now lets through — the old blanket Responses
        // exemption is gone.)
        let no_usage = no_reported_usage(
            entry.input_tokens,
            entry.output_tokens,
            entry.cache_read_tokens,
        );

        // ── Cache column (col 5) ─────────────────────────────────
        let cache_text = cache_cell_text(
            no_usage,
            cache_hit,
            cache_ratio,
            cache_read,
            entry.cache_creation_tokens,
            is_cold,
            is_expired,
            gap_secs_opt,
            is_miss,
        );
        let cache_color = if cache_hit {
            let t = cache_ratio.clamp(0.0, 1.0);
            hsv_to_argb(120.0 * t, 0.7, 0.9)
        } else if is_cold {
            // COLD: neutral cyan — first fill for a new prefix, not an error.
            Argb::new(255, 80, 200, 210)
        } else if is_expired {
            // EXPIRED: muted — cache aged out, expected after idle.
            theme.text_muted
        } else if is_miss {
            // MISS: red — warm prefix that still missed → investigate.
            let severity = (total_prompt as f64 / 100_000.0).clamp(0.0, 1.0);
            hsv_to_argb(0.0, 0.3 + severity * 0.5, 0.6 + severity * 0.4)
        } else {
            theme.text
        };

        // ── Trim column (col 6) ──────────────────────────────────
        let (trim_text, trim_color) = if entry.trim_applied && entry.trim_tokens_before > 0 {
            let saved = entry
                .trim_tokens_before
                .saturating_sub(entry.trim_tokens_after);
            if saved > 0 {
                let pct = saved as f64 / entry.trim_tokens_before as f64 * 100.0;
                (
                    if entry.target == "native" {
                        format!("\u{2212}{} ({:.0}%)", fmt_tokens(saved), pct)
                    } else {
                        format!("{:.0}%", pct)
                    },
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
            entry.client.short_label().to_string(),
            route_cell,
            target_text(entry),
            // No usage reported — render "—" (see `no_reported_usage` above).
            if no_usage {
                "\u{2014}".to_string()
            } else {
                fmt_tokens(total_prompt)
            },
            if no_usage {
                "\u{2014}".to_string()
            } else {
                fmt_tokens(entry.output_tokens)
            },
            cache_text,
            trim_text,
        ];
        // Per-column colors: #, Client, Target, In and Out in theme.text;
        // Route/Cache/Trim have own colors.
        let col_colors = [
            theme.text,
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
        out_row += 1;
    }

    // Thin scrollbar on the right edge when the list overflows the viewport.
    if total_visual > visible_rows {
        let sb_w = (4.0 * scale).max(3.0) as i32;
        let sb_x = win_w - pad - sb_w;
        let track_top = list_top;
        let track_h = visible_rows * row_h;
        // Track
        let track_brush =
            unsafe { CreateSolidBrush(theme.surface.blend_over(theme.background).to_colorref()) };
        unsafe {
            let _ = FillRect(
                dib_dc,
                &RECT {
                    left: sb_x,
                    top: track_top,
                    right: sb_x + sb_w,
                    bottom: track_top + track_h,
                },
                track_brush,
            );
            let _ = DeleteObject(track_brush);
        }
        // Thumb: height proportional to visible/total, position proportional to scroll/max_scroll.
        let thumb_h = ((visible_rows as f32 / total_visual as f32) * track_h as f32)
            .max(row_h as f32 * 0.75) as i32;
        let thumb_travel = (track_h - thumb_h).max(0);
        let thumb_frac = if max_scroll > 0 {
            scroll as f32 / max_scroll as f32
        } else {
            0.0
        };
        let thumb_top = track_top + (thumb_frac * thumb_travel as f32) as i32;
        let thumb_brush = unsafe { CreateSolidBrush(theme.text_muted.to_colorref()) };
        unsafe {
            let _ = FillRect(
                dib_dc,
                &RECT {
                    left: sb_x,
                    top: thumb_top,
                    right: sb_x + sb_w,
                    bottom: thumb_top + thumb_h,
                },
                thumb_brush,
            );
            let _ = DeleteObject(thumb_brush);
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
                // Check tune button
                let tune_btn_w = (44.0 * scale) as i32;
                let tune_btn_h = (20.0 * scale) as i32;
                let tune_btn_x = note_btn_x
                    - (4.0 * scale) as i32
                    - (48.0 * scale) as i32
                    - (4.0 * scale) as i32
                    - tune_btn_w;
                let tune_btn_y = debug_btn_y;
                if pt.x >= tune_btn_x
                    && pt.x < tune_btn_x + tune_btn_w
                    && pt.y >= tune_btn_y
                    && pt.y < tune_btn_y + tune_btn_h
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Check bench button
                let bench_btn_w = (48.0 * scale) as i32;
                let bench_btn_h = (20.0 * scale) as i32;
                let bench_btn_x = note_btn_x - (4.0 * scale) as i32 - bench_btn_w;
                let bench_btn_y = debug_btn_y;
                if pt.x >= bench_btn_x
                    && pt.x < bench_btn_x + bench_btn_w
                    && pt.y >= bench_btn_y
                    && pt.y < bench_btn_y + bench_btn_h
                {
                    return LRESULT(HTCLIENT as isize);
                }
                // Client-filter chip sits in the caption band, so exempt it so
                // clicks reach WM_LBUTTONDOWN instead of dragging the window.
                if let Ok(rect) = FILTER_CHIP_RECT.lock()
                    && let Some((fx, fy, fw, fh)) = *rect
                    && pt.x >= fx
                    && pt.x < fx + fw
                    && pt.y >= fy
                    && pt.y < fy + fh
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
                    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme;
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
                // Check tune button — opens tune panel overlay
                let tune_btn_w = (44.0 * scale) as i32;
                let tune_btn_h = (20.0 * scale) as i32;
                let tune_btn_x = note_btn_x
                    - (4.0 * scale) as i32
                    - (48.0 * scale) as i32
                    - (4.0 * scale) as i32
                    - tune_btn_w;
                let tune_btn_y = debug_btn_y;
                if x >= tune_btn_x
                    && x < tune_btn_x + tune_btn_w
                    && y >= tune_btn_y
                    && y < tune_btn_y + tune_btn_h
                {
                    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme;
                    if !state_ptr.is_null() {
                        let theme = (*state_ptr).clone();
                        crate::overlays::tune_panel::show(&theme);
                    }
                    return LRESULT(0);
                }
                // Check bench button — opens measure panel overlay
                let bench_btn_w = (48.0 * scale) as i32;
                let bench_btn_h = (20.0 * scale) as i32;
                let bench_btn_x = note_btn_x - (4.0 * scale) as i32 - bench_btn_w;
                let bench_btn_y = debug_btn_y;
                if x >= bench_btn_x
                    && x < bench_btn_x + bench_btn_w
                    && y >= bench_btn_y
                    && y < bench_btn_y + bench_btn_h
                {
                    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut NativeTheme;
                    if !state_ptr.is_null() {
                        let theme = (*state_ptr).clone();
                        crate::overlays::measure_panel::show(&theme);
                    }
                    return LRESULT(0);
                }
                // Check client-filter chip — cycles All → Claude Code → Codex → OpenAI.
                let filter_chip_clicked = FILTER_CHIP_RECT
                    .lock()
                    .ok()
                    .and_then(|rect| *rect)
                    .is_some_and(|(fx, fy, fw, fh)| {
                        x >= fx && x < fx + fw && y >= fy && y < fy + fh
                    });
                if filter_chip_clicked {
                    let cur = CLIENT_FILTER.load(Ordering::Relaxed);
                    // -1 (All) → 0 (Claude Code) → 1 (Codex) → 2 (OpenAI) → -1.
                    CLIENT_FILTER.store(if cur >= 2 { -1 } else { cur + 1 }, Ordering::Relaxed);
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
                // Row click -> open the inspector for that request.
                {
                    let hit_seq = ROW_HITS.lock().ok().and_then(|hits| {
                        hits.iter()
                            .find(|(t, b, _)| y >= *t && y < *b)
                            .map(|(_, _, s)| *s)
                    });
                    if let Some(seq) = hit_seq {
                        if let Some(run_id) = crate::core::llm_proxy::get_run_id() {
                            launch_inspector(run_id, seq);
                        }
                        return LRESULT(0);
                    }
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
            WM_MOUSEWHEEL => {
                // wheel delta is the high word of wparam, in multiples of 120.
                let delta = ((wparam.0 >> 16) as i16) as i32 / 120;
                // Wheel up (delta > 0) scrolls toward newer (offset decreases).
                let cur = TRACE_SCROLL.load(Ordering::Relaxed);
                TRACE_SCROLL.store(cur - delta, Ordering::Relaxed);
                // Clamping happens in paint_panel. Repaint now.
                paint_panel(hwnd, 0.0, 0, 0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The helpers under test take exactly the fields the render path feeds
    // them, so no TraceEntry scaffolding (or window/server) is needed — these
    // are the pure decisions that drive the In/Out/Cache cells.

    #[test]
    fn codex_row_with_reported_usage_renders_values_not_dashes() {
        // A Codex row closed by the usage tap: input is fresh prompt tokens
        // (Responses total minus cached), cache_read carries the served count.
        let no_usage = no_reported_usage(200, 50, Some(800));
        assert!(!no_usage, "reported usage must not be blanked");
        // cache_creation is always None on Codex, so total = fresh + cached.
        let total_prompt = 200 + 800;
        assert_eq!(fmt_tokens(total_prompt), "1.0k"); // In column
        assert_eq!(fmt_tokens(50), "50"); // Out column
        // Cache column: reported hit renders the percentage.
        assert_eq!(
            cache_cell_text(
                false,
                true,
                800.0 / total_prompt as f64,
                Some(800),
                None,
                false,
                false,
                None,
                false
            ),
            "80%"
        );
    }

    #[test]
    fn codex_row_in_flight_renders_dashes() {
        // Still streaming: the tap has not seen `response.completed` yet, so
        // nothing is reported and every cell stays honest as "—".
        let no_usage = no_reported_usage(0, 0, None);
        assert!(no_usage);
        assert_eq!(fmt_tokens(0), "\u{2014}");
        assert_eq!(
            cache_cell_text(true, false, 0.0, None, None, false, false, None, false),
            "\u{2014}"
        );
    }

    #[test]
    fn codex_reported_zero_cache_is_not_classified() {
        // cache_read == 0 with a large prompt would normally be a
        // cold/expired/miss candidate, but the Responses wire API never gets a
        // prefix hash — the classifier would label on timing evidence alone.
        let total_prompt = 50_000;
        assert!(!cache_classifier_applies(
            WireApi::Responses,
            Some(0),
            total_prompt
        ));
        // Unclassified, the cell renders empty (as a small Claude prompt does)
        // rather than a guessed label.
        assert_eq!(
            cache_cell_text(false, false, 0.0, Some(0), None, false, false, None, false),
            ""
        );
    }

    #[test]
    fn claude_reported_zero_cache_still_classified() {
        // A real Claude row reports a prefix hash, so a reported zero cache
        // with a large prompt is exactly the case the classifier exists for.
        let total_prompt = 50_000;
        assert!(cache_classifier_applies(
            WireApi::AnthropicMessages,
            Some(0),
            total_prompt
        ));
        // A genuine first fill still renders as COLD.
        assert_eq!(
            cache_cell_text(false, false, 0.0, Some(0), None, true, false, None, false),
            "cold"
        );
    }
}
