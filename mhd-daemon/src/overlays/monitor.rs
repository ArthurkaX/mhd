//! Interactive monitor control panel overlay.
//!
//! Shows all physical monitors and their adjustable parameters (brightness,
//! contrast, audio volume, input source) — similar to Volume Mixer.
//! Detects supported features via DDC/CI capabilities string.
//! Interactive: click/drag on sliders, wheel, scrollable.

use std::sync::{Arc, Mutex};


use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_EVENT, WAIT_OBJECT_0,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetMonitorInfoW, MonitorFromPoint,
    SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetWindowRect, KillTimer, LoadCursorW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassW, SetCursor, SetTimer, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_SHOWNA,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, WM_ACTIVATE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_SETCURSOR,
    WM_TIMER,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WNDCLASSW, MSG,
};

use crate::ddc::{self, PhysicalMonitorHandle, PhysicalMonitorInfo, VcpValue};
use crate::native_theme::NativeTheme;

// ── Constants ───────────────────────────────────────────────────────────

const PANEL_WIDTH_BASE: i32 = 440;
const PANEL_MIN_HEIGHT_BASE: i32 = 100;
const ROW_HEIGHT_BASE: i32 = 40;
const HEADER_HEIGHT_BASE: i32 = 44;
const MONITOR_HEADER_HEIGHT_BASE: i32 = 28;
const PAD_BASE: i32 = 16;
const BAR_HEIGHT_BASE: i32 = 8;
const HIDE_TIMEOUT_MS: u32 = 2000;
const LEAVE_HIDE_TIMEOUT_MS: u32 = 1000;
const HIDE_TIMER_ID: usize = 2;
const RADIUS_BASE: i32 = 14;
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Maximum panel height as a fraction of the work area height.
const MAX_HEIGHT_RATIO: f32 = 0.85;

// VCP codes
const VCP_BRIGHTNESS: u8 = 0x10;
const VCP_CONTRAST: u8 = 0x12;
const VCP_AUDIO_VOLUME: u8 = 0x62;
const VCP_INPUT_SOURCE: u8 = 0x60;

/// Thread-safe wrapper around `HANDLE`.
#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

/// Thread control shared between `show()` and the panel thread.
struct PanelThreadControl {
    /// Auto-reset event to wake the thread.
    event: SafeHandle,
    /// When `true` the thread should exit at its next opportunity.
    dying: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for PanelThreadControl {
    fn drop(&mut self) {
        self.dying.store(true, std::sync::atomic::Ordering::Release);
        unsafe { let _ = SetEvent(self.event.0); }
    }
}

static PANEL_STATE: Mutex<Option<PanelThreadControl>> = Mutex::new(None);

/// Show the monitor control panel overlay (non-blocking).
/// Spawns a fresh thread each time; if a previous thread is still running
/// it is signalled to exit before creating the new one.
pub fn show(theme: NativeTheme) {
    let mut guard = PANEL_STATE.lock().unwrap();

    // Kill any previous thread first
    *guard = None; // Drop triggers dying=true + SetEvent

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
        .name("mhd-monitor-panel".into())
        .spawn(move || {
            panel_thread(show_event, show_dying, theme);
        })
        .ok();
}

/// A detected controllable parameter on a monitor.
#[derive(Debug, Clone)]
enum ParamKind {
    Brightness {
        current: u32,
        #[allow(dead_code)]
        min: u32,
        max: u32,
    },
    Contrast {
        current: u32,
        #[allow(dead_code)]
        min: u32,
        max: u32,
    },
    AudioVolume {
        current: u32,
        #[allow(dead_code)]
        min: u32,
        max: u32,
    },
    InputSource {
        current: u32,
        values: Vec<(u32, String)>,
    },
}

#[derive(Debug, Clone)]
struct ParamInfo {
    kind: ParamKind,
}

/// Data about a single monitor (refreshed on each show).
#[derive(Debug, Clone)]
struct MonitorData {
    name: String,
    handle: PhysicalMonitorHandle,
    params: Vec<ParamInfo>,
}

// ── Scroll state ───────────────────────────────────────────────────────

struct ScrollState {
    y: i32,
    max: i32,
}

// ── Panel state ─────────────────────────────────────────────────────────

struct PanelState {
    monitors: Vec<MonitorData>,
    theme: NativeTheme,
    window_pos: Option<POINT>,
    visible: bool,
    scroll: ScrollState,
}

// ── Thread entry point ──────────────────────────────────────────────────

fn panel_thread(hdl: SafeHandle, dying: Arc<std::sync::atomic::AtomicBool>, theme: NativeTheme) {
    let event = hdl.0;
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = crate::osd::to_utf16_z("mhd_monitor_panel_cls");
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
        monitors: Vec::new(),
        theme,
        window_pos: None,
        visible: false,
        scroll: ScrollState { y: 0, max: 0 },
    };

    let work = monitor_work_rect();
    let mut dragging_row: Option<(usize, usize)> = None; // (monitor_idx, param_idx)
    let mut dragging_window: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;

    let mut want_exit = false;

    // Show window immediately
    {
        refresh_monitors(&mut state);
        paint_panel(hwnd, &mut state, &work, panel_w, scale);
        state.visible = true;
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
        let res = unsafe {
            MsgWaitForMultipleObjects(Some(&wait_handles), false, INFINITE, QS_ALLINPUT)
        };

        const MSG_ARRIVED: WAIT_EVENT = WAIT_EVENT(1);

        match res {
            WAIT_OBJECT_0 => {
                // Signal from show() — toggle off and exit
                dragging_row = None;
                dragging_window = None;
                mouse_tracked = false;
                state.scroll.y = 0;
                let _ = unsafe { ReleaseCapture() };
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
                            state.scroll.y = 0;
                            break;
                        }
                        if msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == 0x1B {
                            want_exit = true;
                            state.scroll.y = 0;
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                            break;
                        }
                        if msg.message == WM_TIMER && msg.wParam.0 == HIDE_TIMER_ID {
                            dragging_row = None;
                            dragging_window = None;
                            mouse_tracked = false;
                            state.scroll.y = 0;
                            let _ = ReleaseCapture();
                            want_exit = true;
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                            break;
                        }

                        if msg.hwnd == hwnd {
                            match msg.message {
                                WM_ACTIVATE => {
                                    // Window lost focus — close immediately
                                    if msg.wParam.0 as u32 == 0 /* WA_INACTIVE */ {
                                        dragging_row = None;
                                        dragging_window = None;
                                        mouse_tracked = false;
                                        state.scroll.y = 0;
                                        let _ = ReleaseCapture();
                                        want_exit = true;
                                        let _ = ShowWindow(hwnd, SW_HIDE);
                                        let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                        break;
                                    }
                                }
                                WM_LBUTTONDOWN => {
                                    let (x, y) = point_from_lparam(msg.lParam);
                                    let y_adj = y + state.scroll.y;
                                    if let Some((mi, pi)) = hit_test_param_slider(
                                        &state, x, y_adj, panel_w, scale,
                                    ) {
                                        dragging_row = Some((mi, pi));
                                        dragging_window = None;
                                        let _ = SetCapture(hwnd);
                                        let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                        let volume = slider_value_from_x(
                                            x, panel_w, scale,
                                        );
                                        set_param_value(
                                            &mut state, mi, pi, volume,
                                        );
                                        paint_panel(hwnd, &mut state, &work, panel_w, scale);
                                        continue;
                                    }
                                    if hit_test_header(y, scale) {
                                        dragging_window = Some((x, y));
                                        dragging_row = None;
                                        let _ = SetCapture(hwnd);
                                        let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                        continue;
                                    }
                                    // Input source cycle
                                    if let Some((mi, pi)) = hit_test_input_source(
                                        &state, x, y_adj, panel_w, scale,
                                    ) {
                                        cycle_input_source(&mut state, mi, pi);
                                        paint_panel(hwnd, &mut state, &work, panel_w, scale);
                                        let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                        continue;
                                    }
                                }
                                WM_MOUSEMOVE => {
                                    begin_mouse_tracking(hwnd, &mut mouse_tracked);
                                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);

                                    if let Some((grab_x, grab_y)) = dragging_window {
                                        let (x, y) = point_from_lparam(msg.lParam);
                                        move_panel_window(
                                            hwnd, &mut state, x - grab_x, y - grab_y,
                                        );
                                        continue;
                                    }
                                    if let Some((mi, pi)) = dragging_row {
                                        let (x, _) = point_from_lparam(msg.lParam);
                                        let volume = slider_value_from_x(x, panel_w, scale);
                                        set_param_value(&mut state, mi, pi, volume);
                                        paint_panel(hwnd, &mut state, &work, panel_w, scale);
                                        continue;
                                    }
                                }
                                WM_MOUSELEAVE => {
                                    mouse_tracked = false;
                                    if dragging_row.is_none() && dragging_window.is_none() {
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
                                    if dragging_row.take().is_some()
                                        || dragging_window.take().is_some()
                                    {
                                        let _ = ReleaseCapture();
                                        continue;
                                    }
                                }
                                WM_MOUSEWHEEL => {
                                    begin_mouse_tracking(hwnd, &mut mouse_tracked);
                                    let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                                    let pt = screen_point_to_client(
                                        hwnd,
                                        point_from_lparam(msg.lParam),
                                    );
                                    let y_adj = pt.1 + state.scroll.y;

                                    // First check if we're on a slider row → adjust value
                                    if let Some((mi, pi)) = hit_test_slider_row(
                                        &state, pt.0, y_adj, panel_w, scale,
                                    ) {
                                        let delta = wheel_delta_from_wparam(msg.wParam);
                                        let step = (delta as f32 / 120.0) * 0.02;
                                        if let Some(param) = state.monitors[mi].params.get(pi) {
                                            if let Some(current) = param_current_value(param) {
                                                let new_val = (current as f32 + step * 100.0)
                                                    .clamp(0.0, 100.0);
                                                set_param_value(
                                                    &mut state,
                                                    mi,
                                                    pi,
                                                    new_val / 100.0,
                                                );
                                                paint_panel(
                                                    hwnd,
                                                    &mut state,
                                                    &work,
                                                    panel_w,
                                                    scale,
                                                );
                                                continue;
                                            }
                                        }
                                    }

                                    // Otherwise → scroll
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

    // Cleanup
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

// ── Monitor data refresh ────────────────────────────────────────────────

fn refresh_monitors(state: &mut PanelState) {
    state.monitors.clear();

    let monitors = match ddc::enumerate_cursor_monitor() {
        Ok(m) => m,
        Err(_) => return,
    };

    for monitor_info in monitors {
        let params = detect_features(&monitor_info);
        state.monitors.push(MonitorData {
            name: monitor_info.name,
            handle: monitor_info.handle,
            params,
        });
    }
}

/// Detect supported features for a monitor. Tries capabilities string first,
/// falls back to probing individual VCP features.
fn detect_features(m: &PhysicalMonitorInfo) -> Vec<ParamInfo> {
    // First try capabilities string
    let supported_codes = m
        .capabilities()
        .ok()
        .and_then(|caps| {
            let parsed = ddc::parse_capabilities_vcp(&caps);
            if parsed.is_empty() {
                None
            } else {
                Some(parsed)
            }
        });

    let mut params: Vec<ParamInfo> = Vec::new();

    // Helper: try to add a VCP-based feature
    let try_add_vcp = |code: u8, supported: &Option<Vec<ddc::SupportedVcp>>| {
        if let Some(list) = supported {
            // Check if this code is in the capabilities list
            if list.iter().any(|sv| sv.code == code) {
                return try_query_vcp(m, code);
            }
            return None;
        }
        // Fallback: probe directly
        try_query_vcp(m, code)
    };

    let supported_ref = &supported_codes;

    // Brightness (0x10)
    if let Some(v) = try_query_brightness(m) {
        params.push(ParamInfo { kind: v });
    }

    // Contrast (0x12)
    if let Some(v) = try_add_vcp(VCP_CONTRAST, supported_ref) {
        params.push(ParamInfo { kind: ParamKind::Contrast {
            current: v.current,
            min: 0,
            max: v.max,
        }});
    }

    // Audio Volume (0x62)
    if let Some(v) = try_add_vcp(VCP_AUDIO_VOLUME, supported_ref) {
        params.push(ParamInfo { kind: ParamKind::AudioVolume {
            current: v.current,
            min: 0,
            max: v.max,
        }});
    }

    // Input Source (0x60)
    if let Some(list) = supported_ref {
        if let Some(sv) = list.iter().find(|sv| sv.code == VCP_INPUT_SOURCE) {
            let current = m.get_vcp(VCP_INPUT_SOURCE).ok().map(|v| v.current).unwrap_or(0);
            let values = if let Some(ref vals) = sv.values {
                vals.iter().map(|&code| {
                    (code, input_source_name(code))
                }).collect::<Vec<_>>()
            } else {
                // Fallback: common input source values
                vec![
                    (0x01, "HDMI 1".into()),
                    (0x02, "HDMI 2".into()),
                    (0x03, "HDMI 3".into()),
                    (0x04, "DP 1".into()),
                    (0x05, "DP 2".into()),
                    (0x06, "USB-C".into()),
                    (0x0F, "DVI-D".into()),
                    (0x10, "VGA".into()),
                ]
            };
            params.push(ParamInfo { kind: ParamKind::InputSource {
                current,
                values,
            }});
        }
    }

    params
}

fn try_query_brightness(m: &PhysicalMonitorInfo) -> Option<ParamKind> {
    m.get_brightness().ok().map(|(cur, min, max)| {
        ParamKind::Brightness { current: cur, min, max }
    })
}

fn try_query_vcp(m: &PhysicalMonitorInfo, code: u8) -> Option<VcpValue> {
    m.get_vcp(code).ok()
}

// ── Input source name helpers ───────────────────────────────────────────

fn input_source_name(code: u32) -> String {
    match code {
        0x01 => "HDMI 1",
        0x02 => "HDMI 2",
        0x03 => "HDMI 3",
        0x04 => "DP 1",
        0x05 => "DP 2",
        0x06 => "USB-C",
        0x07 => "USB-C 2",
        0x0F => "DVI-D",
        0x10 => "VGA",
        0x11 => "VGA 2",
        0x12 => "S-Video",
        0x13 => "Component",
        0x14 => "DisplayPort",
        0x18 => "Thunderbolt",
        _ => return format!("Input 0x{:02X}", code),
    }
    .to_string()
}

// ── Param helpers ───────────────────────────────────────────────────────

fn param_current_value(param: &ParamInfo) -> Option<u32> {
    match &param.kind {
        ParamKind::Brightness { current, .. } => Some(*current),
        ParamKind::Contrast { current, .. } => Some(*current),
        ParamKind::AudioVolume { current, .. } => Some(*current),
        ParamKind::InputSource { current, .. } => Some(*current),
    }
}

fn param_label(param: &ParamInfo) -> &'static str {
    match &param.kind {
        ParamKind::Brightness { .. } => "Brightness",
        ParamKind::Contrast { .. } => "Contrast",
        ParamKind::AudioVolume { .. } => "Volume",
        ParamKind::InputSource { .. } => "Input Source",
    }
}

fn param_is_slider(param: &ParamInfo) -> bool {
    matches!(
        &param.kind,
        ParamKind::Brightness { .. }
            | ParamKind::Contrast { .. }
            | ParamKind::AudioVolume { .. }
    )
}

fn param_max_value(param: &ParamInfo) -> u32 {
    match &param.kind {
        ParamKind::Brightness { max, .. } => *max,
        ParamKind::Contrast { max, .. } => *max,
        ParamKind::AudioVolume { max, .. } => *max,
        ParamKind::InputSource { .. } => 0,
    }
}

fn param_display_value(param: &ParamInfo) -> String {
    match &param.kind {
        ParamKind::Brightness { current, max, .. } => {
            if *max > 0 {
                format!("{}%", (current * 100 / max))
            } else {
                format!("{}", current)
            }
        }
        ParamKind::Contrast { current, max, .. } => {
            if *max > 0 {
                format!("{}%", (current * 100 / max))
            } else {
                format!("{}", current)
            }
        }
        ParamKind::AudioVolume { current, max, .. } => {
            if *max > 0 {
                format!("{}%", (current * 100 / max))
            } else {
                format!("{}", current)
            }
        }
        ParamKind::InputSource { current, values } => {
            values
                .iter()
                .find(|(code, _)| *code == *current)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| format!("0x{:02X}", current))
        }
    }
}

// ── Monitor work rect ──────────────────────────────────────────────────

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

// ── Painting ───────────────────────────────────────────────────────────

fn paint_panel(
    hwnd: HWND,
    state: &mut PanelState,
    work: &RECT,
    width: i32,
    scale: f32,
) {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h = -(14.0 * scale) as i32;
    let font_small_h = -(12.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let header_h = (HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let mon_header_h = (MONITOR_HEADER_HEIGHT_BASE as f32 * scale) as i32;

    // Calculate total content height
    let content_h = total_content_height(state, pad, header_h, mon_header_h, row_h, scale);
    let max_h = ((work.bottom - work.top) as f32 * MAX_HEIGHT_RATIO) as i32;
    let total_h = content_h.max((PANEL_MIN_HEIGHT_BASE as f32 * scale) as i32);
    let total_h = total_h.min(max_h);

    // Update scroll max
    state.scroll.max = (content_h - total_h).max(0);

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, width, total_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    // DibFrame handles DIB creation, cleanup, and presenting.
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

    // ── Header ──
    let header_y = pad;
    let mut header_rc = RECT {
        left: pad,
        top: header_y,
        right: width - pad,
        bottom: header_y + font_h.abs() + 4,
    };
    let mut header_wz = crate::osd::to_utf16_z("Monitor Control");
    unsafe {
        let _ = DrawTextW(
            frame.dc(), &mut header_wz, &mut header_rc,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // Count
    let count_str = format!("{} monitors", state.monitors.len());
    unsafe {
        let _ = SetTextColor(frame.dc(), theme.text_muted.to_colorref());
    }
    let mut count_wz = crate::osd::to_utf16_z(&count_str);
    unsafe {
        let _ = DrawTextW(
            frame.dc(), &mut count_wz, &mut header_rc,
            DT_RIGHT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }

    // Separator under header
    let sep_y = header_y + font_h.abs() + 8;
    {
        let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
        let sep_rc = RECT {
            left: pad, top: sep_y, right: width - pad, bottom: sep_y + 1,
        };
        unsafe {
            let _ = FillRect(frame.dc(), &sep_rc, sep_brush);
            let _ = DeleteObject(sep_brush);
        }
    }

    // ── Rows (scroll offset applied to Y) ──
    unsafe {
        let _ = SelectObject(frame.dc(), hfont_small);
    }

    let bar_h = (BAR_HEIGHT_BASE as f32 * scale).max(3.0) as i32;
    let label_w = (100.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let bar_max_w = width - bar_x - pad - 50 - pad;

    let scroll_y = state.scroll.y;
    let clip_top = pad;
    let clip_bot = total_h - pad;

    let mut current_y = sep_y + 8 - scroll_y;

    for (_mi, monitor) in state.monitors.iter().enumerate() {
        // Monitor header
        let mon_hdr_y = current_y;
        let mon_hdr_rc = RECT {
            left: pad,
            top: mon_hdr_y,
            right: width - pad,
            bottom: mon_hdr_y + mon_header_h,
        };

        // Only draw if within clip region
        if mon_hdr_y + mon_header_h >= clip_top && mon_hdr_y < clip_bot {
            unsafe {
                // Section header with accent colour
                let _ = SetTextColor(frame.dc(), theme.accent.to_colorref());
            }
            let mut name_wz = crate::osd::to_utf16_z(&monitor.name);
            let mut name_rc = mon_hdr_rc;
            unsafe {
                let _ = DrawTextW(
                    frame.dc(), &mut name_wz, &mut name_rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }

            // Subtle separator line under monitor name
            let sep_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
            let sep_rc = RECT {
                left: pad,
                top: mon_hdr_y + mon_header_h - 1,
                right: width - pad,
                bottom: mon_hdr_y + mon_header_h,
            };
            unsafe {
                let _ = FillRect(frame.dc(), &sep_rc, sep_brush);
                let _ = DeleteObject(sep_brush);
            }
        }

        current_y += mon_header_h;

        // Parameters for this monitor
        for (_pi, param) in monitor.params.iter().enumerate() {
            let row_y = current_y;
            let _row_rc = RECT {
                left: pad,
                top: row_y,
                right: width - pad,
                bottom: row_y + row_h,
            };

            // Only draw if within clip region
            if row_y + row_h >= clip_top && row_y < clip_bot {
                if param_is_slider(param) {
                    // ── Slider row ──
                    unsafe {
                        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
                    }

                    // Label
                    let mut label_rc = RECT {
                        left: pad,
                        top: row_y,
                        right: pad + label_w,
                        bottom: row_y + row_h,
                    };
                    let mut label_wz = crate::osd::to_utf16_z(param_label(param));
                    unsafe {
                        let _ = DrawTextW(
                            frame.dc(), &mut label_wz, &mut label_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );
                    }

                    let mid_y = row_y + row_h / 2;
                    let bar_y = mid_y - bar_h / 2;

                    // Bar track
                    let track_brush = unsafe { CreateSolidBrush(theme.bar_background.to_colorref()) };
                    let track_rc = RECT {
                        left: bar_x, top: bar_y, right: bar_x + bar_max_w, bottom: bar_y + bar_h,
                    };
                    unsafe {
                        let _ = FillRect(frame.dc(), &track_rc, track_brush);
                        let _ = DeleteObject(track_brush);
                    }

                    // Bar fill
                    let current = param_current_value(param).unwrap_or(0);
                    let max_val = param_max_value(param).max(1);
                    let fill_ratio = current as f32 / max_val as f32;
                    let fill_w = (bar_max_w as f32 * fill_ratio).max(1.0) as i32;

                    let fill_brush = unsafe { CreateSolidBrush(theme.accent.to_colorref()) };
                    let fill_rc = RECT {
                        left: bar_x, top: bar_y, right: bar_x + fill_w, bottom: bar_y + bar_h,
                    };
                    unsafe {
                        let _ = FillRect(frame.dc(), &fill_rc, fill_brush);
                        let _ = DeleteObject(fill_brush);
                    }

                    // Percentage
                    let pct = param_display_value(param);
                    let pct_x = bar_x + bar_max_w + 8;
                    unsafe {
                        let _ = SetTextColor(frame.dc(), theme.text_muted.to_colorref());
                    }
                    let mut pct_rc = RECT {
                        left: pct_x, top: row_y, right: pct_x + 44, bottom: row_y + row_h,
                    };
                    let mut pct_wz = crate::osd::to_utf16_z(&pct);
                    unsafe {
                        let _ = DrawTextW(
                            frame.dc(), &mut pct_wz, &mut pct_rc,
                            DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
                        );
                    }
                } else {
                    // ── Non-slider (Input Source selector) ──
                    unsafe {
                        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
                    }

                    // Label
                    let mut label_rc = RECT {
                        left: pad,
                        top: row_y,
                        right: pad + label_w,
                        bottom: row_y + row_h,
                    };
                    let mut label_wz = crate::osd::to_utf16_z(param_label(param));
                    unsafe {
                        let _ = DrawTextW(
                            frame.dc(), &mut label_wz, &mut label_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );
                    }

                    // Value (clickable pill)
                    let display = param_display_value(param);
                    let val_rc = RECT {
                        left: bar_x,
                        top: row_y + 4,
                        right: bar_x + (120.0 * scale) as i32,
                        bottom: row_y + row_h - 4,
                    };
                    // Draw pill background
                    let pill_brush = unsafe {
                        CreateSolidBrush(theme.surface.blend_over(theme.background).to_colorref())
                    };
                    unsafe {
                        let _ = FillRect(frame.dc(), &val_rc, pill_brush);
                        let _ = DeleteObject(pill_brush);
                    }
                    // Border
                    let border_brush = unsafe { CreateSolidBrush(theme.border.to_colorref()) };
                    unsafe {
                        let _ = FillRect(
                            frame.dc(),
                            &RECT {
                                left: val_rc.left,
                                top: val_rc.top,
                                right: val_rc.left + 1,
                                bottom: val_rc.bottom,
                            },
                            border_brush,
                        );
                        let _ = FillRect(
                            frame.dc(),
                            &RECT {
                                left: val_rc.right - 1,
                                top: val_rc.top,
                                right: val_rc.right,
                                bottom: val_rc.bottom,
                            },
                            border_brush,
                        );
                        let _ = FillRect(
                            frame.dc(),
                            &RECT {
                                left: val_rc.left,
                                top: val_rc.top,
                                right: val_rc.right,
                                bottom: val_rc.top + 1,
                            },
                            border_brush,
                        );
                        let _ = FillRect(
                            frame.dc(),
                            &RECT {
                                left: val_rc.left,
                                top: val_rc.bottom - 1,
                                right: val_rc.right,
                                bottom: val_rc.bottom,
                            },
                            border_brush,
                        );
                        let _ = DeleteObject(border_brush);
                    }

                    // Text
                    unsafe {
                        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
                    }
                    let mut val_wz = crate::osd::to_utf16_z(&display);
                    let mut text_rc = val_rc;
                    unsafe {
                        let _ = DrawTextW(
                            frame.dc(), &mut val_wz, &mut text_rc,
                            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                        );
                    }

                    // → arrow for click hint
                    let mut arrow_rc = RECT {
                        left: val_rc.right + 4,
                        top: row_y,
                        right: val_rc.right + 20,
                        bottom: row_y + row_h,
                    };
                    unsafe {
                        let _ = SetTextColor(frame.dc(), theme.text_muted.to_colorref());
                    }
                    let mut arrow_wz = crate::osd::to_utf16_z("▶");
                    unsafe {
                        let _ = DrawTextW(
                            frame.dc(), &mut arrow_wz, &mut arrow_rc,
                            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
                        );
                    }
                }
            }

            current_y += row_h;
        }

        // Add spacing between monitors
        current_y += (8.0 * scale) as i32;
    }

    unsafe {
        let _ = SelectObject(frame.dc(), old_font);
        let _ = DeleteObject(hfont);
        let _ = DeleteObject(hfont_small);
    }

    // ── Scrollbar indicator ──
    if state.scroll.max > 0 {
        let sb_width = (4.0 * scale) as i32;
        let sb_x = width - pad / 2 - sb_width / 2;
        let sb_height = total_h - pad * 2;
        let visible_ratio = total_h as f32 / (total_h as f32 + state.scroll.max as f32);
        let thumb_h = (sb_height as f32 * visible_ratio).max(10.0) as i32;
        let thumb_pos = (state.scroll.y as f32 / state.scroll.max as f32
            * (sb_height - thumb_h) as f32) as i32;
        let thumb_y = pad + thumb_pos;

        let sb_color = crate::native_theme::Argb { a: 80, r: theme.text_muted.r, g: theme.text_muted.g, b: theme.text_muted.b };
        let sb_brush = unsafe { CreateSolidBrush(sb_color.to_colorref()) };
        let sb_rc = RECT {
            left: sb_x,
            top: thumb_y,
            right: sb_x + sb_width,
            bottom: thumb_y + thumb_h,
        };
        unsafe {
            let _ = FillRect(frame.dc(), &sb_rc, sb_brush);
            let _ = DeleteObject(sb_brush);
        }
    }

    frame.fix_gdi_alpha(theme.background);

    let pt_dst = *state.window_pos.get_or_insert_with(|| POINT {
        x: work.left + (work.right - work.left - width) / 2,
        y: work.top + (work.bottom - work.top - total_h) / 2,
    });

    frame.present_layered(hwnd, pt_dst.x, pt_dst.y, 255);
}

fn total_content_height(
    state: &PanelState,
    pad: i32,
    _header_h: i32,
    mon_header_h: i32,
    row_h: i32,
    scale: f32,
) -> i32 {
    let font_h_abs = (14.0 * scale) as i32;
    let sep_y = pad + font_h_abs + 8;
    let content_y = sep_y + 8;
    let mut total = content_y;

    for (_mi, monitor) in state.monitors.iter().enumerate() {
        total += mon_header_h; // monitor name header
        total += monitor.params.len() as i32 * row_h;
        total += (8.0 * scale) as i32; // spacing after monitor
    }

    total + pad - content_y + pad
}

// ── Interaction ─────────────────────────────────────────────────────────

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

fn hit_test_header(y: i32, scale: f32) -> bool {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    let sep_y = pad + font_h_abs + 8;
    y >= 0 && y < sep_y
}

fn screen_point_to_client(hwnd: HWND, point: (i32, i32)) -> (i32, i32) {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_ok() {
        (point.0 - rc.left, point.1 - rc.top)
    } else {
        point
    }
}

/// Hit-test a slider bar on a parameter row.
/// Returns (monitor_idx, param_idx) if x is within the bar area.
fn hit_test_param_slider(
    state: &PanelState,
    x: i32,
    y: i32,
    width: i32,
    scale: f32,
) -> Option<(usize, usize)> {
    let (bar_x, bar_max_w) = slider_bar_bounds(width, scale);
    if x < bar_x || x > bar_x + bar_max_w {
        return None;
    }
    hit_test_slider_row(state, x, y, width, scale)
}

/// Hit-test any parameter row.
fn hit_test_slider_row(
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

    let pad_s = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let mon_header_h = (MONITOR_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let sep_y = pad_s + font_h_abs + 8;
    let mut current_y = sep_y + 8;

    for (mi, monitor) in state.monitors.iter().enumerate() {
        // Monitor header
        current_y += mon_header_h;

        // Parameters
        for (pi, _param) in monitor.params.iter().enumerate() {
            if y >= current_y && y < current_y + row_h {
                return Some((mi, pi));
            }
            current_y += row_h;
        }

        current_y += (8.0 * scale) as i32; // spacing
    }

    None
}

/// Hit-test an input source selector.
fn hit_test_input_source(
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

    let pad_s = (PAD_BASE as f32 * scale) as i32;
    let font_h_abs = (14.0 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let mon_header_h = (MONITOR_HEADER_HEIGHT_BASE as f32 * scale) as i32;
    let sep_y = pad_s + font_h_abs + 8;
    let mut current_y = sep_y + 8;

    let label_w = (100.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let _bar_max_w = width - bar_x - pad - 50 - pad;

    for (mi, monitor) in state.monitors.iter().enumerate() {
        current_y += mon_header_h;

        for (pi, param) in monitor.params.iter().enumerate() {
            if !param_is_slider(param) {
                // Input source: check if within the pill + arrow area
                let pill_x = bar_x;
                let pill_w = (120.0 * scale) as i32;
                let pill_right = pill_x + pill_w + 20; // include arrow
                if y >= current_y && y < current_y + row_h
                    && x >= pill_x && x < pill_right
                {
                    return Some((mi, pi));
                }
            }
            current_y += row_h;
        }

        current_y += (8.0 * scale) as i32;
    }

    None
}

fn wheel_delta_from_wparam(wparam: WPARAM) -> i16 {
    ((wparam.0 >> 16) & 0xffff) as i16
}

fn slider_bar_bounds(width: i32, scale: f32) -> (i32, i32) {
    let pad = (PAD_BASE as f32 * scale) as i32;
    let label_w = (100.0 * scale) as i32;
    let bar_x = pad + label_w + pad;
    let bar_max_w = width - bar_x - pad - 50 - pad;
    (bar_x, bar_max_w)
}

fn slider_value_from_x(x: i32, width: i32, scale: f32) -> f32 {
    let (bar_x, bar_max_w) = slider_bar_bounds(width, scale);
    ((x - bar_x) as f32 / bar_max_w as f32).clamp(0.0, 1.0)
}

fn set_param_value(state: &mut PanelState, mi: usize, pi: usize, ratio: f32) {
    let Some(monitor) = state.monitors.get(mi) else { return };
    let Some(param) = monitor.params.get(pi) else { return };

    let handle = monitor.handle;
    let max_val = param_max_value(param).max(1);
    let new_val = (ratio * max_val as f32).round().clamp(0.0, max_val as f32) as u32;

    let vcp_code = match &param.kind {
        ParamKind::Brightness { .. } => VCP_BRIGHTNESS,
        ParamKind::Contrast { .. } => VCP_CONTRAST,
        ParamKind::AudioVolume { .. } => VCP_AUDIO_VOLUME,
        ParamKind::InputSource { .. } => return, // not a slider
    };

    // Use PhysicalMonitorInfo's set_vcp (but we don't have it here — reconstruct)
    // Actually we can call the low-level set_vcp on the handle directly.
    if let Err(e) = set_vcp_direct(handle, vcp_code, new_val) {
        eprintln!("mhd: monitor set error: {e}");
        return;
    }

    // Update state
    let param = &mut state.monitors[mi].params[pi];
    match &mut param.kind {
        ParamKind::Brightness { current, .. } => *current = new_val,
        ParamKind::Contrast { current, .. } => *current = new_val,
        ParamKind::AudioVolume { current, .. } => *current = new_val,
        ParamKind::InputSource { .. } => {}
    }
}

fn cycle_input_source(state: &mut PanelState, mi: usize, pi: usize) {
    let Some(monitor) = state.monitors.get(mi) else { return };
    let Some(param) = monitor.params.get(pi) else { return };

    let (current, values) = match &param.kind {
        ParamKind::InputSource { current, values } => (*current, values.clone()),
        _ => return,
    };

    if values.is_empty() {
        return;
    }

    // Find current index and cycle to next
    let current_idx = values.iter().position(|(code, _)| *code == current);
    let next_idx = match current_idx {
        Some(i) => (i + 1) % values.len(),
        None => 0,
    };
    let (next_code, _) = values[next_idx];

    let handle = monitor.handle;
    if let Err(e) = set_vcp_direct(handle, VCP_INPUT_SOURCE, next_code) {
        eprintln!("mhd: monitor input source error: {e}");
        return;
    }

    // Update state
    let param = &mut state.monitors[mi].params[pi];
    if let ParamKind::InputSource { current, .. } = &mut param.kind {
        *current = next_code;
    }
}

/// Set a VCP feature directly using the handle.
fn set_vcp_direct(handle: PhysicalMonitorHandle, code: u8, value: u32) -> Result<(), String> {
    // For input source, we need to use the monitor module's VCP set.
    // We can create a temporary PhysicalMonitorInfo for this.
    let mi = PhysicalMonitorInfo { handle, name: String::new() };
    mi.set_vcp(code, value)
}

// ── Window procedure ───────────────────────────────────────────────────

extern "system" fn panel_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_SETCURSOR {
        if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) } {
            unsafe { SetCursor(cursor) };
            return LRESULT(1);
        }
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
