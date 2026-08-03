//! CPU Power Plan overlay — switch power plans and edit parking/freq settings.
//!
//! Ephemeral thread pattern (like volume_mixer / power).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_EVENT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, FillRect, GetMonitorInfoW, HDC, HRGN, IntersectClipRect,
    InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, SelectClipRgn,
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
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos,
    GetDesktopWindow, GetWindowRect, IDC_ARROW, KillTimer, LoadCursorW, MSG,
    MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW, SW_HIDE,
    SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, WM_ACTIVATE, WM_CHAR, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
use windows::core::GUID;
use windows::core::PCWSTR;

use crate::constants::WM_MOUSELEAVE;
use crate::native_theme::{Argb, NativeTheme};
use crate::osd::{OsdHandle, to_utf16_z};

mod controller;
mod layout;
mod paint;
mod power;
mod telemetry;
use controller::msg_handler;
use layout::*;
use paint::{hit_hover_row, paint_panel, stress_button_rects};
pub(crate) use power::{
    GUID_COOLING_POLICY, GUID_MAX_PROC_STATE, GUID_MAX_PROC_STATE_CLASS1,
    GUID_MAX_PROC_STATE_CLASS2, GUID_PERF_BOOST_MODE, GUID_PROCESSOR_SUBGROUP, enumerate_schemes,
    get_active_scheme_guid, set_active_scheme, write_ac_value, write_dc_value,
};
use power::{read_current_plan_values, write_plan_values};
use telemetry::{
    PdhFreqSampler, PerfInfo, compute_loads, detect_core_topology,
    read_effective_processor_frequencies, read_parked_state, read_perf_info,
    read_processor_base_mhz,
};

// ── Layout (unscaled, 96 DPI) ────────────────────────────────────────
const W: i32 = 430;
const PAD: i32 = 14;
const HDR_H: i32 = 32;
const PLAN_H: i32 = 28;
const SEP_H: i32 = 1;
const SEC_H: i32 = 22; // section header height
const ROW_H: i32 = 26; // settings row height
const MON_HDR_H: i32 = 26; // monitor section header
const LOAD_ROW_H: i32 = 24; // stress load controls row
const BAR_H: i32 = 18; // per-core monitor row height
const CORE_BAR_H: i32 = 8; // compact load bar inside each monitor row
const BAR_GAP: i32 = 3; // gap between bars
const SUMMARY_H: i32 = 18; // group summary line
const BTN_W: i32 = 80; // apply button width
const BTN_H: i32 = 28; // apply button height
const BTN_ROW_H: i32 = 36; // apply button row height
const PAD_INNER: i32 = 8; // inner padding between columns

const RADIUS: i32 = 8;

// ── Timer IDs ────────────────────────────────────────────────────────
const TIMER_MONITOR: usize = 1;

// ── State types ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Field {
    AcMinCores,
    DcMinCores,
    AcMaxCores,
    DcMaxCores,
    AcMinFreq,
    DcMinFreq,
    AcMaxFreq,
    DcMaxFreq,
}

#[derive(Clone, Copy)]
struct PlanValues {
    /// 0–100: core parking minimum unparked cores (%)
    min_cores: u32,
    /// 0–100: core parking maximum unparked cores (%)
    max_cores: u32,
    /// 0–100: minimum processor performance state %
    min_freq: u32,
    /// 0–100: maximum processor performance state %
    max_freq: u32,
    /// Autonomous mode — CPU self-manages frequencies (GUID_PERF_AUTONOMOUS_MODE)
    autonomous_mode: bool,
    /// Turbo boost on/off
    turbo: bool,
    /// System cooling policy — 0=Passive, 1=Active
    cooling_policy: u32,
    /// Performance increase policy — 0=Ideal, 2=Rocket
    increase_policy: u32,
    /// Heterogeneous scheduling — 0=All, 2=PreferPerf, 4=PreferEff, 5=Auto
    hetero_policy: u32,
    /// Parked core performance state — 0=NoPref, 1=Deepest, 2=Lightest
    parked_perf: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum StressLevel {
    Off,
    Threads(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum HoverRow {
    PlanRow,
    MinCores,
    MaxCores,
    MinFreq,
    MaxFreq,
    AutonomousMode,
    TurboBoost,
    CoolingPolicy,
    IncreasePolicy,
    HeteroScheduling,
    ParkedPerf,
}

struct MonitorState {
    core_load: Vec<f32>,     // 0.0–1.0 per logical core
    core_parked: Vec<bool>,  // parked status per logical core
    core_freq_mhz: Vec<u32>, // current MHz per logical core when available
    base_mhz: u32,           // nominal (base) max frequency, 0 if unknown
    freq_scale_mhz: u32,     // bar full-scale freq (>= base, grows with turbo)
    freq_sampler: Option<PdhFreqSampler>,
    p_cores: Vec<usize>, // logical indices of P-cores
    e_cores: Vec<usize>, // logical indices of E-cores
    stress_level: StressLevel,
    stress_stop: Arc<AtomicBool>,
    /// Snapshot of previous perf info for delta computation (None = not yet sampled)
    prev_perf: Option<Vec<PerfInfo>>,
}

struct PanelState {
    theme: NativeTheme,
    schemes: Vec<(GUID, String)>,
    active_plan_name: String,
    ac: PlanValues,
    dc: PlanValues,
    /// Original values of the active plan when it was opened/switched to.
    /// Live edits are previewed immediately but reverted to this on an unsaved
    /// plan switch or unsaved close; Save promotes the current values here.
    baseline_ac: PlanValues,
    baseline_dc: PlanValues,
    dirty: bool,
    focused: Option<Field>,
    edit_text: String,
    edit_select_all: bool,
    pos: POINT,
    monitor: MonitorState,
    stress_handles: Vec<std::thread::JoinHandle<()>>,
    /// Collapsed state of the monitor core groups.
    p_collapsed: bool,
    e_collapsed: bool,
    /// Vertical scroll offset (px) of the monitor core list when it exceeds
    /// the available (screen-capped) height.
    scroll: i32,
    hover_row: Option<HoverRow>,
    hover_pos: POINT,
    /// When the current hover_row was first entered — used to delay the tooltip.
    hover_since: Option<std::time::Instant>,
}

// ── Thread control ───────────────────────────────────────────────────

static CTRL: Mutex<Option<Ctrl>> = Mutex::new(None);

#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct Ctrl {
    ev: SafeHandle,
    dying: Arc<AtomicBool>,
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        self.dying.store(true, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.ev.0);
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Show (or re‑show) the CPU Power Plan overlay.
pub fn show_panel(theme: NativeTheme) {
    let mut g = CTRL.lock().unwrap();
    if let Some(ref ctrl) = *g {
        unsafe {
            let _ = SetEvent(ctrl.ev.0);
        }
        return;
    }
    let ev = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(AtomicBool::new(false));
    let c = Ctrl {
        ev: SafeHandle(ev),
        dying: dying.clone(),
    };
    let cev = c.ev.clone();
    let cdy = c.dying.clone();
    *g = Some(c);
    drop(g);
    std::thread::Builder::new()
        .name("mhd-cpuplan".into())
        .spawn(move || thread_main(cev, cdy, theme))
        .ok();
}

/// Switch the active power plan and show OSD notification.
pub fn switch_plan(target: &str, plans: &[String], osd: &OsdHandle) {
    let schemes = enumerate_schemes();
    if schemes.is_empty() {
        return;
    }

    let new_guid = if target == "next" {
        let active_guid = get_active_scheme_guid();
        let order: Vec<GUID> = plans
            .iter()
            .filter_map(|n| schemes.iter().find(|(_, name)| name == n).map(|(g, _)| *g))
            .collect();
        if order.is_empty() {
            let active_pos = schemes
                .iter()
                .position(|(g, _)| *g == active_guid)
                .unwrap_or(0);
            let next = (active_pos + 1) % schemes.len();
            schemes[next].0
        } else {
            let cur_pos = order
                .iter()
                .position(|g| *g == active_guid)
                .unwrap_or(order.len().wrapping_sub(1));
            order[(cur_pos + 1) % order.len()]
        }
    } else {
        match schemes.iter().find(|(_, n)| n == target) {
            Some((g, _)) => *g,
            None => return,
        }
    };

    set_active_scheme(new_guid);
    let name = schemes
        .iter()
        .find(|(g, _)| *g == new_guid)
        .map(|(_, n)| n.as_str())
        .unwrap_or("Unknown");
    osd.show_notify(name, 2000);
}

/// Switch to the n-th enumerated scheme (for tray use).
pub fn switch_plan_by_index(index: usize, osd: &OsdHandle) {
    let schemes = enumerate_schemes();
    if let Some((guid, name)) = schemes.get(index) {
        set_active_scheme(*guid);
        osd.show_notify(name.as_str(), 2000);
    }
}

// ── Stress threads ─────────────────────────────────────────────────

fn spawn_stress_threads(count: usize, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let stp = stop.clone();
        let h = std::thread::Builder::new()
            .name("mhd-stress".into())
            .spawn(move || {
                while !stp.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
            .ok();
        if let Some(h) = h {
            handles.push(h);
        }
    }
    handles
}

fn stop_stress_threads(stop: &Arc<AtomicBool>, handles: &mut Vec<std::thread::JoinHandle<()>>) {
    stop.store(true, Ordering::Release);
    for h in handles.drain(..) {
        let _ = h.join();
    }
}

// ── Win32 Power helpers ──────────────────────────────────────────────

// ── Helpers ───────────────────────────────────────────────────────────

fn work_area() -> RECT {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let hm = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(hm, &mut mi);
        mi.rcWork
    }
}

fn sc_from_hwnd(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

// Unscaled height of everything above the monitor core groups (up to and
// including the PACKAGE line). The core list scrolls below this point.
fn top_block_h() -> i32 {
    HDR_H + PLAN_H
        + SEC_H + ROW_H * 2          // CORES
        + SEC_H + ROW_H * 2          // FREQUENCY
        + SEC_H + ROW_H * 4          // POWER FEATURES
        + SEC_H + ROW_H * 2          // CORE MANAGEMENT
        + BTN_ROW_H + SEP_H          // Save row + separator
        + MON_HDR_H + LOAD_ROW_H + SUMMARY_H // monitor header + load + PACKAGE
}

fn group_bars_h(count: usize) -> i32 {
    (BAR_H + BAR_GAP) * count as i32 - BAR_GAP
}

fn group_block_h(count: usize, collapsed: bool) -> i32 {
    if count == 0 {
        return 0;
    }
    SEC_H + if collapsed { 0 } else { group_bars_h(count) }
}

// Unscaled height of the scrollable core-list content.
fn list_content_h(p_count: usize, e_count: usize, p_col: bool, e_col: bool) -> i32 {
    if p_count == 0 && e_count == 0 {
        return ROW_H;
    }
    group_block_h(p_count, p_col) + group_block_h(e_count, e_col)
}

fn compute_total_h(sc: f32, p_count: usize, e_count: usize) -> i32 {
    let h = top_block_h() + list_content_h(p_count, e_count, false, false);
    (h as f32 * sc) as i32
}

// On-screen Y (scaled) where the monitor core list begins.
fn list_top(sc: f32) -> i32 {
    (top_block_h() as f32 * sc) as i32
}

// Actual window height: natural height capped to the work area so it always
// fits on screen; the core list scrolls when capped.
fn current_win_h(sc: f32, st: &PanelState) -> i32 {
    let p = st.monitor.p_cores.len();
    let e = st.monitor.e_cores.len();
    let natural = top_block_h() + list_content_h(p, e, st.p_collapsed, st.e_collapsed);
    let natural = (natural as f32 * sc) as i32;
    let wa = work_area();
    let max_h = (wa.bottom - wa.top) - (24.0 * sc) as i32;
    let min_h = list_top(sc) + (BAR_H as f32 * sc) as i32; // top block + ≥1 bar
    natural.min(max_h).max(min_h)
}

// Maximum scroll offset (scaled) for the given window height.
fn scroll_max(sc: f32, st: &PanelState, win_h: i32) -> i32 {
    let p = st.monitor.p_cores.len();
    let e = st.monitor.e_cores.len();
    let content = (list_content_h(p, e, st.p_collapsed, st.e_collapsed) as f32 * sc) as i32;
    let viewport = win_h - list_top(sc);
    (content - viewport).max(0)
}

// Recompute the (collapse-aware, screen-capped) window height, resize the
// window if it changed, clamp the scroll offset, then paint.
fn render_panel(hwnd: HWND, st: &mut PanelState, win_w: i32, sc: f32, win_h: &mut i32) {
    let new_h = current_win_h(sc, st);
    if new_h != *win_h {
        *win_h = new_h;
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                0,
                0,
                win_w,
                new_h,
                SWP_NOMOVE | SWP_NOZORDER,
            );
        }
    }
    st.scroll = st.scroll.clamp(0, scroll_max(sc, st, *win_h));
    paint_panel(hwnd, st, win_w, *win_h, sc);
}

// ── Thread entry point ───────────────────────────────────────────────

fn thread_main(hdl: SafeHandle, dying: Arc<AtomicBool>, theme: NativeTheme) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls = to_utf16_z("mhd_cpuplan_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(cpuplan_wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&wc);
    }

    // Detect core topology
    let (p_cores, e_cores) = detect_core_topology();
    let total_cores = p_cores.len() + e_cores.len();

    // Initial compute TOTAL_H using unscaled values (we'll use final win_h)
    let init_total = compute_total_h(1.0, p_cores.len(), e_cores.len());

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            W,
            init_total,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let sc = sc_from_hwnd(hwnd);
    let win_w = (W as f32 * sc) as i32;
    // Cap to the work area so many-core systems still fit on screen.
    let max_h = {
        let wa = work_area();
        (wa.bottom - wa.top) - (24.0 * sc) as i32
    };
    let mut win_h = compute_total_h(sc, p_cores.len(), e_cores.len()).min(max_h);

    let wa = work_area();
    let pos = POINT {
        x: wa.left + (wa.right - wa.left - win_w) / 2,
        y: wa.top + (wa.bottom - wa.top - win_h) / 2,
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            pos.x,
            pos.y,
            win_w,
            win_h,
            SWP_NOZORDER,
        );
    }

    let active_schemes = enumerate_schemes();
    let active_guid = get_active_scheme_guid();
    let active_plan_name = active_schemes
        .iter()
        .find(|(g, _)| *g == active_guid)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let (ac, dc) = read_current_plan_values();

    let stress_stop = Arc::new(AtomicBool::new(false));
    let base_mhz = read_processor_base_mhz(total_cores.max(1)).unwrap_or(0);
    let freq_sampler = (base_mhz > 0)
        .then(|| PdhFreqSampler::new(total_cores.max(1), base_mhz))
        .flatten();

    let mut st = PanelState {
        theme,
        schemes: active_schemes,
        active_plan_name,
        ac,
        dc,
        baseline_ac: ac,
        baseline_dc: dc,
        dirty: false,
        focused: None,
        edit_text: String::new(),
        edit_select_all: false,
        pos,
        monitor: MonitorState {
            core_load: vec![0.0; total_cores.max(1)],
            core_parked: vec![false; total_cores.max(1)],
            core_freq_mhz: vec![0; total_cores.max(1)],
            base_mhz,
            freq_scale_mhz: base_mhz.max(1),
            freq_sampler,
            p_cores,
            e_cores,
            stress_level: StressLevel::Off,
            stress_stop: stress_stop.clone(),
            prev_perf: None,
        },
        stress_handles: Vec::new(),
        p_collapsed: false,
        e_collapsed: false,
        scroll: 0,
        hover_row: None,
        hover_since: None,
        hover_pos: POINT::default(),
    };

    if dying.load(Ordering::Acquire) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    // Sample initial perf info
    if let Some(perf) = read_perf_info() {
        st.monitor.prev_perf = Some(perf);
    }
    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
    }

    render_panel(hwnd, &mut st, win_w, sc, &mut win_h);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // Start monitor timer
        let _ = SetTimer(hwnd, TIMER_MONITOR, 500, None);
    }

    let event = hdl.0;
    let mut hidden = false;
    let mut drag: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;

    loop {
        if dying.load(Ordering::Acquire) {
            break;
        }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        match res {
            WAIT_OBJECT_0 => {
                hidden = !hidden;
                if hidden {
                    // Hotkey-hide without Save discards live-preview edits.
                    flush_edit(&mut st);
                    revert_if_dirty(&mut st);
                    unsafe {
                        let _ = ReleaseCapture();
                        _ = ShowWindow(hwnd, SW_HIDE);
                        _ = InvalidateRect(hwnd, None, false);
                    }
                    // Kill timer when hidden
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_MONITOR);
                    }
                } else {
                    // Re-read current values when showing
                    let (ac_new, dc_new) = read_current_plan_values();
                    st.ac = ac_new;
                    st.dc = dc_new;
                    st.baseline_ac = ac_new;
                    st.baseline_dc = dc_new;
                    st.dirty = false;
                    st.focused = None;
                    st.edit_select_all = false;
                    st.schemes = enumerate_schemes();
                    let ag = get_active_scheme_guid();
                    st.active_plan_name = st
                        .schemes
                        .iter()
                        .find(|(g, _)| *g == ag)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "Unknown".to_string());

                    // Reset monitor state on show
                    if let Some(perf) = read_perf_info() {
                        st.monitor.prev_perf = Some(perf);
                    }
                    st.monitor.core_load.fill(0.0);
                    st.monitor.core_parked = read_parked_state();
                    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
                        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
                    }
                    update_freq_scale(&mut st.monitor);

                    render_panel(hwnd, &mut st, win_w, sc, &mut win_h);
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_SHOW);
                    }
                    // Restart timer
                    unsafe {
                        let _ = SetTimer(hwnd, TIMER_MONITOR, 500, None);
                    }
                }
            }
            WAIT_EVENT(1) => {
                let mut msg = MSG::default();
                let mut repaint = false;
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            break;
                        }
                        // Translate keyboard messages so WM_KEYDOWN → WM_CHAR for digit input
                        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                        if msg.hwnd == hwnd {
                            // Always repaint — including on MOUSEMOVE — so the
                            // hover tooltip / highlight and live monitoring data are
                            // visible even while the mouse is in motion over the panel.
                            repaint = true;
                            if (msg.message == WM_KEYDOWN || msg.message == WM_CHAR)
                                && handle_key(&msg, &mut st, &mut hidden)
                            {
                                repaint = true;
                            }
                            if msg.message == WM_MOUSEWHEEL && handle_wheel(&msg, &mut st, sc) {
                                repaint = true;
                            }
                            if msg.message == WM_TIMER && msg.wParam.0 == TIMER_MONITOR {
                                handle_monitor_timer(&mut st);
                                repaint = true;
                            }
                            if !msg_handler(
                                hwnd,
                                &msg,
                                &mut st,
                                &mut drag,
                                &mut mouse_tracked,
                                &mut hidden,
                                sc,
                            ) {
                                hidden = true;
                                repaint = false;
                            }
                        }
                    }
                }
                if repaint && !hidden {
                    render_panel(hwnd, &mut st, win_w, sc, &mut win_h);
                }
            }
            _ => break,
        }
    }

    // Stop stress threads on exit
    stop_stress_threads(&st.monitor.stress_stop, &mut st.stress_handles);
    unsafe {
        let _ = KillTimer(hwnd, TIMER_MONITOR);
        _ = DestroyWindow(hwnd);
    }
}

// ── Monitor timer handler ───────────────────────────────────────────

fn handle_monitor_timer(st: &mut PanelState) {
    // Read new perf info
    let curr_perf = match read_perf_info() {
        Some(p) => p,
        None => return,
    };

    // If we have a previous snapshot, compute loads
    if let Some(ref prev) = st.monitor.prev_perf {
        let loads = compute_loads(prev, &curr_perf);
        // Match lengths to our core vectors
        let n = st.monitor.core_load.len().min(loads.len());
        st.monitor.core_load[..n].copy_from_slice(&loads[..n]);
    }

    // Store current as previous for next tick
    st.monitor.prev_perf = Some(curr_perf);

    // Read parked state
    let parked = read_parked_state();
    update_vec_prefix(&mut st.monitor.core_parked, &parked);

    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
    }
    update_freq_scale(&mut st.monitor);
}

// Grow the bar full-scale so it always covers observed turbo peaks. Rounded up
// to the next 100 MHz; never shrinks within a session to keep bars stable.
fn update_freq_scale(mon: &mut MonitorState) {
    let peak = mon.core_freq_mhz.iter().copied().max().unwrap_or(0);
    let want = mon.base_mhz.max(peak);
    let want = want.div_ceil(100) * 100; // round up to 100 MHz
    if want > mon.freq_scale_mhz {
        mon.freq_scale_mhz = want;
    }
}

fn update_vec_prefix<T: Copy>(dst: &mut [T], src: &[T]) {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
}

// ── Key handling ─────────────────────────────────────────────────────

fn handle_key(msg: &MSG, st: &mut PanelState, hidden: &mut bool) -> bool {
    let vk = msg.wParam.0 as u32;
    match vk {
        0x1B => {
            // Escape — cancel: discard unsaved live-preview edits
            flush_edit(st);
            revert_if_dirty(st);
            *hidden = true;
            st.focused = None;
            st.edit_select_all = false;
            unsafe {
                let _ = ReleaseCapture();
            }
            return true;
        }
        0x0D => {
            // Enter — Save: commit live-preview edits as baseline
            flush_edit(st);
            apply_now(st);
            commit_baseline(st);
            st.focused = None;
            st.edit_select_all = false;
            unsafe {
                let _ = ReleaseCapture();
            }
            return true;
        }
        0x09 => {
            // Tab — cycle focus through editable fields
            flush_edit(st);
            let field = match st.focused {
                None | Some(Field::DcMaxFreq) => Field::AcMinCores,
                Some(Field::AcMinCores) => Field::DcMinCores,
                Some(Field::DcMinCores) => Field::AcMaxCores,
                Some(Field::AcMaxCores) => Field::DcMaxCores,
                Some(Field::DcMaxCores) => Field::AcMinFreq,
                Some(Field::AcMinFreq) => Field::DcMinFreq,
                Some(Field::DcMinFreq) => Field::AcMaxFreq,
                Some(Field::AcMaxFreq) => Field::DcMaxFreq,
            };
            focus_field(st, field);
            return true;
        }
        _ => {
            if st.focused.is_some() {
                // WM_CHAR: wParam is the character code
                if msg.message == WM_CHAR {
                    let ch = vk;
                    if ch >= '0' as u32 && ch <= '9' as u32 {
                        if st.edit_select_all {
                            st.edit_text.clear();
                            st.edit_select_all = false;
                        }
                        if st.edit_text.len() < 3 {
                            st.edit_text.push(char::from_u32(ch).unwrap_or('0'));
                            st.dirty = true;
                        }
                        return true;
                    }
                    if ch == 0x08 {
                        // Backspace as char
                        if st.edit_select_all {
                            st.edit_text.clear();
                            st.edit_select_all = false;
                            st.dirty = true;
                            return true;
                        }
                        if !st.edit_text.is_empty() {
                            st.edit_text.pop();
                            st.dirty = true;
                        }
                        return true;
                    }
                }
                // WM_KEYDOWN fallback for backspace
                if msg.message == WM_KEYDOWN && vk == 0x08 {
                    if st.edit_select_all {
                        st.edit_text.clear();
                        st.edit_select_all = false;
                    } else if !st.edit_text.is_empty() {
                        st.edit_text.pop();
                    }
                    st.dirty = true;
                    return true;
                }
            }
        }
    }
    false
}

// ── Wheel handling ──────────────────────────────────────────────────

fn handle_wheel(msg: &MSG, st: &mut PanelState, _sc: f32) -> bool {
    let delta = ((msg.wParam.0 >> 16) & 0xffff) as i16;
    let steps = delta / 120;
    if let Some(field) = st.focused {
        flush_edit(st);
        let val = match field {
            Field::AcMinCores => &mut st.ac.min_cores,
            Field::DcMinCores => &mut st.dc.min_cores,
            Field::AcMaxCores => &mut st.ac.max_cores,
            Field::DcMaxCores => &mut st.dc.max_cores,
            Field::AcMinFreq => &mut st.ac.min_freq,
            Field::DcMinFreq => &mut st.dc.min_freq,
            Field::AcMaxFreq => &mut st.ac.max_freq,
            Field::DcMaxFreq => &mut st.dc.max_freq,
        };
        if steps > 0 {
            *val = (*val + steps as u32).min(100);
        } else {
            *val = val.saturating_sub((-steps) as u32);
        }
        st.dirty = true;
        apply_now(st);
        return true;
    }
    // No field focused → scroll the monitor core list (~3 rows per notch).
    // render_panel clamps the offset to the valid range afterwards.
    let step = ((3 * (BAR_H + BAR_GAP)) as f32 * _sc) as i32;
    st.scroll = (st.scroll - steps as i32 * step).max(0);
    true
}

/// Get current value for a field (as u32)
fn get_field_value(st: &PanelState, field: Field) -> u32 {
    match field {
        Field::AcMinCores => st.ac.min_cores,
        Field::DcMinCores => st.dc.min_cores,
        Field::AcMaxCores => st.ac.max_cores,
        Field::DcMaxCores => st.dc.max_cores,
        Field::AcMinFreq => st.ac.min_freq,
        Field::DcMinFreq => st.dc.min_freq,
        Field::AcMaxFreq => st.ac.max_freq,
        Field::DcMaxFreq => st.dc.max_freq,
    }
}

fn focus_field(st: &mut PanelState, field: Field) {
    st.focused = Some(field);
    st.edit_text = get_field_value(st, field).to_string();
    st.edit_select_all = true;
}

fn clear_focus(st: &mut PanelState) {
    st.focused = None;
    st.edit_select_all = false;
}

fn flush_edit(st: &mut PanelState) {
    if st.edit_text.is_empty() || st.focused.is_none() {
        return;
    }
    let mut changed = false;
    if let Ok(v) = st.edit_text.parse::<u32>() {
        let v = v.min(100);
        let slot: &mut u32 = match st.focused.unwrap() {
            Field::AcMinCores => &mut st.ac.min_cores,
            Field::DcMinCores => &mut st.dc.min_cores,
            Field::AcMaxCores => &mut st.ac.max_cores,
            Field::DcMaxCores => &mut st.dc.max_cores,
            Field::AcMinFreq => &mut st.ac.min_freq,
            Field::DcMinFreq => &mut st.dc.min_freq,
            Field::AcMaxFreq => &mut st.ac.max_freq,
            Field::DcMaxFreq => &mut st.dc.max_freq,
        };
        if *slot != v {
            *slot = v;
            changed = true;
        }
    }
    st.edit_text.clear();
    st.edit_select_all = false;
    // Apply numeric edits to the live power scheme immediately (preview),
    // marking them unsaved until the user clicks Save.
    if changed {
        st.dirty = true;
        apply_now(st);
    }
}

// Write the current values to the active power scheme right away, so edits take
// effect live (visible in the monitor) without waiting for a Save click.
fn apply_now(st: &mut PanelState) {
    write_plan_values(&st.ac, &st.dc);
}

// Promote the current (live-previewed) values to the saved baseline.
fn commit_baseline(st: &mut PanelState) {
    st.baseline_ac = st.ac;
    st.baseline_dc = st.dc;
    st.dirty = false;
}

// Restore the active scheme to its pre-edit baseline. Used when the user
// switches plans or closes without saving. Writes to whatever scheme is
// currently active, so call this *before* changing the active scheme.
fn revert_if_dirty(st: &mut PanelState) {
    if !st.dirty {
        return;
    }
    st.ac = st.baseline_ac;
    st.dc = st.baseline_dc;
    write_plan_values(&st.ac, &st.dc);
    st.dirty = false;
}

// ── Layout constant re-exports for paint ─────────────────────────────

const LABEL_W: i32 = 150;
const VAL_W: i32 = 114;

extern "system" fn cpuplan_wndproc(_h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(_h, msg, wp, lp) }
}
