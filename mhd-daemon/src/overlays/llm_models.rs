//! LLM model selector overlay.
//!
//! Shows the three Claude tiers (Opus / Sonnet / Haiku). For each tier you pick
//! a routing target: **Anthropic native**, or any model from the shared pool
//! configured in `[llm_proxy]`. Clicking a row calls the proxy's `/set_model`
//! endpoint; the switch takes effect on the proxy's next request (in-flight work
//! is never interrupted).
//!
//! Structurally modelled on `monitor.rs` (layered popup window on its own
//! thread, sections + clickable rows, auto-hide, wheel scroll).

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_EVENT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteObject,
    DrawTextW, FillRect, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetWindowRect, IDC_ARROW, KillTimer, LoadCursorW, MSG, MsgWaitForMultipleObjects,
    PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW, SW_HIDE, SW_SHOWNA, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SetCursor, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    WM_ACTIVATE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::config::LlmProxyConfig;
use crate::constants::WM_MOUSELEAVE;
use crate::native_theme::NativeTheme;

// ── Constants ───────────────────────────────────────────────────────────

const PANEL_WIDTH_BASE: i32 = 420;
const PANEL_MIN_HEIGHT_BASE: i32 = 100;
const ROW_HEIGHT_BASE: i32 = 34;
const TIER_HEADER_HEIGHT_BASE: i32 = 28;
const PAD_BASE: i32 = 16;
const RADIUS_BASE: i32 = 14;
const HIDE_TIMEOUT_MS: u32 = 4000;
const LEAVE_HIDE_TIMEOUT_MS: u32 = 1200;
const HIDE_TIMER_ID: usize = 2;
const MAX_HEIGHT_RATIO: f32 = 0.85;

/// Thread-safe wrapper around `HANDLE`.
#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct PanelThreadControl {
    event: SafeHandle,
    dying: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for PanelThreadControl {
    fn drop(&mut self) {
        self.dying.store(true, std::sync::atomic::Ordering::Release);
        unsafe {
            let _ = SetEvent(self.event.0);
        }
    }
}

static PANEL_STATE: Mutex<Option<PanelThreadControl>> = Mutex::new(None);

/// Show the model selector overlay (non-blocking).
pub fn show(theme: NativeTheme, cfg: LlmProxyConfig) {
    let mut guard = PANEL_STATE.lock().unwrap();
    *guard = None; // kill previous

    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ctrl = PanelThreadControl {
        event: SafeHandle(event),
        dying: dying.clone(),
    };
    let show_event = ctrl.event.clone();
    let show_dying = ctrl.dying.clone();
    *guard = Some(ctrl);
    drop(guard);

    std::thread::Builder::new()
        .name("mhd-llm-models".into())
        .spawn(move || {
            panel_thread(show_event, show_dying, theme, cfg);
        })
        .ok();
}

// ── State ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Opt {
    label: String,
    target: String,
}

#[derive(Clone)]
struct TierRow {
    name: &'static str,
    slot: &'static str,
    options: Vec<Opt>,
    selected: usize,
}

struct ScrollState {
    y: i32,
    max: i32,
}

struct PanelState {
    tiers: Vec<TierRow>,
    theme: NativeTheme,
    window_pos: Option<POINT>,
    scroll: ScrollState,
    running: bool,
}

fn build_tiers(cfg: &LlmProxyConfig) -> Vec<TierRow> {
    let mut options = vec![Opt {
        label: "Anthropic native".to_string(),
        target: "native".to_string(),
    }];
    for m in &cfg.models {
        let label = if m.display_name.is_empty() {
            m.id.clone()
        } else {
            m.display_name.clone()
        };
        options.push(Opt {
            label,
            target: m.id.clone(),
        });
    }

    let mk = |name: &'static str, slot: &'static str| TierRow {
        name,
        slot,
        options: options.clone(),
        selected: 0,
    };

    let mut free_options = vec![Opt {
        label: "Off".to_string(),
        target: String::new(),
    }];
    free_options.extend(options.iter().cloned());

    let mut tiers = vec![
        mk("Opus", "opus"),
        mk("Sonnet", "sonnet"),
        mk("Haiku", "haiku"),
    ];
    tiers.push(TierRow {
        name: "Free tier (light trim)",
        slot: "free_target",
        options: free_options,
        selected: 0,
    });
    tiers
}

/// Mark the selected option for each tier from the proxy's live config.
fn refresh_selection(state: &mut PanelState) {
    state.running = crate::llm_proxy::is_running();
    if let Some((opus, sonnet, haiku, _fable)) = crate::llm_proxy::get_targets() {
        let targets = [opus, sonnet, haiku];
        for (tier, current) in state.tiers.iter_mut().zip(targets.iter()) {
            tier.selected = tier
                .options
                .iter()
                .position(|o| &o.target == current)
                .unwrap_or(0);
        }
    }
    if let Some(free_target) = crate::llm_proxy::get_trim_free_target()
        && let Some(tier) = state.tiers.iter_mut().find(|t| t.slot == "free_target")
    {
        tier.selected = tier
            .options
            .iter()
            .position(|o| o.target == free_target)
            .unwrap_or(0);
    }
}

// ── Thread entry point ──────────────────────────────────────────────────

fn panel_thread(
    hdl: SafeHandle,
    dying: Arc<std::sync::atomic::AtomicBool>,
    theme: NativeTheme,
    cfg: LlmProxyConfig,
) {
    let event = hdl.0;
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = crate::osd::to_utf16_z("mhd_llm_models_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: windows::Win32::Foundation::HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(panel_wndproc),
        hInstance: hinstance,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&wc);
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            PANEL_WIDTH_BASE,
            PANEL_MIN_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;
    let panel_w = (PANEL_WIDTH_BASE as f32 * scale) as i32;

    let mut state = PanelState {
        tiers: build_tiers(&cfg),
        theme,
        window_pos: None,
        scroll: ScrollState { y: 0, max: 0 },
        running: false,
    };
    refresh_selection(&mut state);

    let work = monitor_work_rect();
    let mut dragging_window: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;
    let mut want_exit = false;

    {
        paint_panel(hwnd, &mut state, &work, panel_w, scale);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
            let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
        }
    }

    loop {
        if want_exit || dying.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }

        let wait_handles = [event];
        let res =
            unsafe { MsgWaitForMultipleObjects(Some(&wait_handles), false, INFINITE, QS_ALLINPUT) };
        const MSG_ARRIVED: WAIT_EVENT = WAIT_EVENT(1);

        match res {
            WAIT_OBJECT_0 => {
                want_exit = true;
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                }
            }
            MSG_ARRIVED => {
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            want_exit = true;
                            break;
                        }
                        if msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == 0x1B {
                            want_exit = true;
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                            break;
                        }
                        if msg.message == WM_TIMER && msg.wParam.0 == HIDE_TIMER_ID {
                            want_exit = true;
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                            break;
                        }

                        if msg.hwnd == hwnd {
                            match msg.message {
                                WM_ACTIVATE => {
                                    if msg.wParam.0 as u32 == 0 {
                                        want_exit = true;
                                        let _ = ShowWindow(hwnd, SW_HIDE);
                                        let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                        break;
                                    }
                                }
                                WM_LBUTTONDOWN => {
                                    let (x, y) = point_from_lparam(msg.lParam);
                                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                    if hit_test_header(y, scale) {
                                        dragging_window = Some((x, y));
                                        let _ = SetCapture(hwnd);
                                        continue;
                                    }
                                    let y_adj = y + state.scroll.y;
                                    if let Some((ti, oi)) =
                                        hit_test_row(&state, x, y_adj, panel_w, scale)
                                    {
                                        apply_selection(&mut state, ti, oi);
                                        paint_panel(hwnd, &mut state, &work, panel_w, scale);
                                        let _ =
                                            SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                                        continue;
                                    }
                                }
                                WM_MOUSEMOVE => {
                                    begin_mouse_tracking(hwnd, &mut mouse_tracked);
                                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                    if let Some((grab_x, grab_y)) = dragging_window {
                                        let (x, y) = point_from_lparam(msg.lParam);
                                        move_panel_window(hwnd, &mut state, x - grab_x, y - grab_y);
                                        continue;
                                    }
                                }
                                WM_MOUSELEAVE => {
                                    mouse_tracked = false;
                                    if dragging_window.is_none() {
                                        let _ = SetTimer(
                                            hwnd,
                                            HIDE_TIMER_ID,
                                            LEAVE_HIDE_TIMEOUT_MS,
                                            None,
                                        );
                                    }
                                    continue;
                                }
                                WM_LBUTTONUP => {
                                    if dragging_window.take().is_some() {
                                        let _ = ReleaseCapture();
                                        continue;
                                    }
                                }
                                WM_MOUSEWHEEL => {
                                    begin_mouse_tracking(hwnd, &mut mouse_tracked);
                                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                    let delta = wheel_delta_from_wparam(msg.wParam);
                                    let scroll_step = (ROW_HEIGHT_BASE as f32 * scale) as i32;
                                    let new_scroll =
                                        state.scroll.y - (delta as i32 / 120) * scroll_step;
                                    state.scroll.y = new_scroll.clamp(0, state.scroll.max);
                                    paint_panel(hwnd, &mut state, &work, panel_w, scale);
                                    continue;
                                }
                                _ => {}
                            }
                        }

                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            _ => continue,
        }
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

// ── Apply a selection ───────────────────────────────────────────────────

fn apply_selection(state: &mut PanelState, ti: usize, oi: usize) {
    let Some(tier) = state.tiers.get_mut(ti) else {
        return;
    };
    let Some(opt) = tier.options.get(oi).cloned() else {
        return;
    };
    tier.selected = oi;
    let slot = tier.slot;
    let ok = if slot == "free_target" {
        crate::llm_proxy::set_free_target(&opt.target)
    } else {
        crate::llm_proxy::set_target(slot, &opt.target)
    };
    if !ok {
        // Proxy not running — reflect that in the status line.
        state.running = crate::llm_proxy::is_running();
    }
}

// ── Layout / painting ───────────────────────────────────────────────────

fn monitor_work_rect() -> RECT {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}

fn content_top(scale: f32) -> i32 {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    pad + font_h_abs + 8 + 8 // header + separator + gap
}

fn total_content_height(state: &PanelState, scale: f32) -> i32 {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let tier_h = (TIER_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let mut total = content_top(scale);
    for tier in &state.tiers {
        total += tier_h;
        total += tier.options.len() as i32 * row_h;
        total += (8.0 * scale) as i32;
    }
    total + pad
}

fn paint_panel(hwnd: HWND, state: &mut PanelState, work: &RECT, width: i32, scale: f32) {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h = -(14.0 * scale) as i32;
    let font_small_h = -(13.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let tier_h = (TIER_HEADER_HEIGHT_BASE as f32 * scale) as i32;

    let content_h = total_content_height(state, scale);
    let max_h = ((work.bottom - work.top) as f32 * MAX_HEIGHT_RATIO) as i32;
    let total_h = content_h
        .max((PANEL_MIN_HEIGHT_BASE as f32 * scale) as i32)
        .min(max_h);
    state.scroll.max = (content_h - total_h).max(0);

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, width, total_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    let mut frame = match crate::renderer::DibFrame::new(width, total_h) {
        Some(f) => f,
        None => return,
    };
    let theme = &state.theme;
    let radius = (RADIUS_BASE as f32 * scale) as i32;
    crate::osd::draw_rounded_rect(frame.pixels_mut(), width, total_h, radius, theme.background);

    let hfont = crate::osd::create_font(font_h, false, "Segoe UI");
    let hfont_small = crate::osd::create_font(font_small_h, false, "Segoe UI");
    let old_font = unsafe { SelectObject(frame.dc(), hfont) };
    unsafe {
        let _ = SetBkMode(frame.dc(), TRANSPARENT);
        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
    }

    // Header
    let header_y = pad;
    let mut header_rc = RECT {
        left: pad,
        top: header_y,
        right: width - pad,
        bottom: header_y + font_h.abs() + 4,
    };
    let mut header_wz = crate::osd::to_utf16_z("LLM Models");
    unsafe {
        let _ = DrawTextW(
            frame.dc(),
            &mut header_wz,
            &mut header_rc,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
    // Status (right-aligned)
    let status = if state.running {
        "proxy: on"
    } else {
        "proxy: off"
    };
    let status_color = if state.running {
        theme.accent
    } else {
        theme.text_muted
    };
    unsafe {
        let _ = SetTextColor(frame.dc(), status_color.to_colorref());
    }
    let mut status_wz = crate::osd::to_utf16_z(status);
    unsafe {
        let _ = DrawTextW(
            frame.dc(),
            &mut status_wz,
            &mut header_rc,
            DT_RIGHT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // Separator
    let sep_y = header_y + font_h.abs() + 8;
    {
        let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
        let sep_rc = RECT {
            left: pad,
            top: sep_y,
            right: width - pad,
            bottom: sep_y + 1,
        };
        unsafe {
            let _ = FillRect(frame.dc(), &sep_rc, sep_brush);
            let _ = DeleteObject(sep_brush);
        }
    }

    let clip_top = pad;
    let clip_bot = total_h - pad;
    let mut current_y = content_top(scale) - state.scroll.y;

    for tier in &state.tiers {
        // Tier header
        if current_y + tier_h >= clip_top && current_y < clip_bot {
            unsafe {
                let _ = SelectObject(frame.dc(), hfont);
                let _ = SetTextColor(frame.dc(), theme.accent.to_colorref());
            }
            let mut name_rc = RECT {
                left: pad,
                top: current_y,
                right: width - pad,
                bottom: current_y + tier_h,
            };
            let mut name_wz = crate::osd::to_utf16_z(tier.name);
            unsafe {
                let _ = DrawTextW(
                    frame.dc(),
                    &mut name_wz,
                    &mut name_rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
        }
        current_y += tier_h;

        // Option rows
        unsafe {
            let _ = SelectObject(frame.dc(), hfont_small);
        }
        for (oi, opt) in tier.options.iter().enumerate() {
            if current_y + row_h >= clip_top && current_y < clip_bot {
                let selected = oi == tier.selected;
                let row_rc = RECT {
                    left: pad,
                    top: current_y + 2,
                    right: width - pad,
                    bottom: current_y + row_h - 2,
                };

                if selected {
                    // Filled accent pill behind the selected row.
                    let bg = theme.accent.blend_over(theme.background);
                    let brush = unsafe { CreateSolidBrush(bg.to_colorref()) };
                    unsafe {
                        let _ = FillRect(frame.dc(), &row_rc, brush);
                        let _ = DeleteObject(brush);
                    }
                }

                // Marker + label
                let marker = if selected { "● " } else { "○ " };
                let text = format!("{marker}{}", opt.label);
                let text_color = if selected {
                    theme.text
                } else {
                    theme.text_muted
                };
                unsafe {
                    let _ = SetTextColor(frame.dc(), text_color.to_colorref());
                }
                let mut label_rc = RECT {
                    left: pad + (10.0 * scale) as i32,
                    top: current_y,
                    right: width - pad - (8.0 * scale) as i32,
                    bottom: current_y + row_h,
                };
                let mut label_wz = crate::osd::to_utf16_z(&text);
                unsafe {
                    let _ = DrawTextW(
                        frame.dc(),
                        &mut label_wz,
                        &mut label_rc,
                        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                    );
                }
            }
            current_y += row_h;
        }
        current_y += (8.0 * scale) as i32;
    }

    unsafe {
        let _ = SelectObject(frame.dc(), old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    frame.fix_gdi_alpha(theme.background);

    let pt_dst = *state.window_pos.get_or_insert_with(|| POINT {
        x: work.left + (work.right - work.left - width) / 2,
        y: work.top + (work.bottom - work.top - total_h) / 2,
    });
    frame.present_layered(hwnd, pt_dst.x, pt_dst.y, 255);
}

// ── Hit testing ─────────────────────────────────────────────────────────

/// Returns (tier_idx, option_idx) for a click at client (x, y) with scroll
/// already applied to y.
fn hit_test_row(
    state: &PanelState,
    x: i32,
    y: i32,
    width: i32,
    scale: f32,
) -> Option<(usize, usize)> {
    let pad = (PAD_BASE as f32 * scale) as i32;
    if x < pad || x > width - pad {
        return None;
    }
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let tier_h = (TIER_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let mut current_y = content_top(scale);

    for (ti, tier) in state.tiers.iter().enumerate() {
        current_y += tier_h;
        for oi in 0..tier.options.len() {
            if y >= current_y && y < current_y + row_h {
                return Some((ti, oi));
            }
            current_y += row_h;
        }
        current_y += (8.0 * scale) as i32;
    }
    None
}

fn hit_test_header(y: i32, scale: f32) -> bool {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    let sep_y = pad + font_h_abs + 8;
    y >= 0 && y < sep_y
}

// ── Misc helpers (shared idiom with monitor.rs) ─────────────────────────

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xffff) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    (x, y)
}

fn begin_mouse_tracking(hwnd: HWND, tracked: &mut bool) {
    if *tracked {
        return;
    }
    let mut tme = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
        *tracked = true;
    }
}

fn move_panel_window(hwnd: HWND, state: &mut PanelState, delta_x: i32, delta_y: i32) {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
        return;
    }
    let pos = POINT {
        x: rc.left + delta_x,
        y: rc.top + delta_y,
    };
    state.window_pos = Some(pos);
    unsafe {
        let _ = SetWindowPos(hwnd, None, pos.x, pos.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
}

fn wheel_delta_from_wparam(wparam: WPARAM) -> i16 {
    ((wparam.0 >> 16) & 0xffff) as i16
}

extern "system" fn panel_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_SETCURSOR
        && let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) }
    {
        unsafe { SetCursor(cursor) };
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
