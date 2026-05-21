//! Power Control overlay — keep awake, sleep, shutdown, turn off screen.
//!
//! Ephemeral thread pattern (like volume_mixer / monitor_panel).

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_EVENT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, Ellipse, FillRect, GetDC, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DIB_RGB_COLORS,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_NORMAL, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    OUT_DEFAULT_PRECIS, RGBQUAD, TRANSPARENT, AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetSuspendState, SetThreadExecutionState};
use windows::Win32::System::Shutdown::{ExitWindowsEx, EWX_SHUTDOWN, SHUTDOWN_REASON};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDesktopWindow,
    GetWindowRect, KillTimer, LoadCursorW, MsgWaitForMultipleObjects,
    PeekMessageW, PostMessageW, RegisterClassW, SetTimer, ShowWindow,
    UpdateLayeredWindow,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_SHOWNA,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos,
    GWLP_USERDATA, ULW_ALPHA, WM_ACTIVATE, WM_APP, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_QUIT,
    WM_TIMER, WM_SYSCOMMAND,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WNDCLASSW, MSG,
};
use windows::core::PCWSTR;

use crate::native_theme::{Argb, NativeTheme};
use crate::osd::to_utf16_z;

// ── Constants ──────────────────────────────────────────────────────────

const WIDTH_BASE: i32 = 340;
const PAD: i32 = 14;
const HEADER_H: i32 = 44;
const BTN_GAP: i32 = 4;

const AWAKE_BTN_H: i32 = 28;
const AWAKE_BTN_W: i32 = 46;
const AWAKE_BTN_Y: i32 = HEADER_H;
const AWAKE_COUNT: usize = 6;

const STATUS_H: i32 = 24;
const STATUS_Y: i32 = AWAKE_BTN_Y + AWAKE_BTN_H + 2;

const ACTIONS_LABEL_H: i32 = 18;
const ACTIONS_LABEL_Y: i32 = STATUS_Y + STATUS_H + 6;
const ACT_BTN_H: i32 = 32;
const ACT_BTN_W: i32 = 100;
const ACT_BTN_Y: i32 = ACTIONS_LABEL_Y + ACTIONS_LABEL_H;
const ACT_BTN_GAP: i32 = 10;

const TIMER_LABEL_H: i32 = 16;
const TIMER_BTN_W: i32 = 48;
const TIMER_BTN_H: i32 = 20;
const TIMER_ROW1_Y: i32 = ACT_BTN_Y + ACT_BTN_H + 4;
const TIMER_ROW2_Y: i32 = TIMER_ROW1_Y + TIMER_LABEL_H + 2;

const PENDING_H: i32 = 28;
const PENDING_Y: i32 = TIMER_ROW2_Y + TIMER_LABEL_H + 4;
const PENDING_BTN_W: i32 = 60;

const HEIGHT_BASE: i32 = PENDING_Y + PENDING_H + PAD;

const RADIUS_BASE: i32 = 8;
const WM_MOUSELEAVE: u32 = 0x02A3;
const POWER_TIMER_ID: usize = 1;
const CANCEL_CD_MSG: u32 = WM_APP + 1;

const AWAKE_LABELS: [&str; AWAKE_COUNT] = ["Off", "Forever", "30m", "1h", "2h", "4h"];

// ── Data types ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum AwakeMode {
    Off,
    Forever,
    Timed { end_min: u64 },
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PowerOp {
    Sleep,
    Shutdown,
    TurnOffScreen,
}

#[derive(Clone)]
struct PendingAction {
    op: PowerOp,
    execute_at: u64,
}

struct PowerState {
    theme: NativeTheme,
    awake: AwakeMode,
    pending: Option<PendingAction>,
    countdown_hwnd: Option<HWND>,
    window_pos: Option<POINT>,
    visible: bool,
}

struct CountdownData {
    main_hwnd: HWND,
    op: PowerOp,
    remaining_secs: u32,
}

// ── Thread control ─────────────────────────────────────────────────────

static POWER_STATE: Mutex<Option<ThreadControl>> = Mutex::new(None);

#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct ThreadControl {
    event: SafeHandle,
    dying: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ThreadControl {
    fn drop(&mut self) {
        self.dying.store(true, std::sync::atomic::Ordering::Release);
        unsafe { let _ = SetEvent(self.event.0); }
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Show (or re‑show) the Power Control overlay.
pub fn show(theme: NativeTheme) {
    let mut guard = POWER_STATE.lock().unwrap();
    *guard = None;

    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let ctrl = ThreadControl { event: SafeHandle(event), dying: dying.clone() };
    let ev = ctrl.event.clone();
    let dy = ctrl.dying.clone();
    *guard = Some(ctrl);
    drop(guard);

    std::thread::Builder::new()
        .name("mhd-power".into())
        .spawn(move || thread_main(ev, dy, theme))
        .ok();
}

// ── Helpers ────────────────────────────────────────────────────────────

fn now_min() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() / 60
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn work_rect() -> RECT {
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

// ── Thread entry point ─────────────────────────────────────────────────

fn thread_main(hdl: SafeHandle, dying: Arc<std::sync::atomic::AtomicBool>, theme: NativeTheme) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls = to_utf16_z("mhd_power_cls");
    let cd_cls = to_utf16_z("mhd_power_cd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();

    for (name, wndproc) in [(&cls, Some(power_wndproc as _)), (&cd_cls, Some(cd_wndproc as _))] {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: wndproc,
            hInstance: hinst,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
            lpszClassName: PCWSTR::from_raw(name.as_ptr()),
            ..Default::default()
        };
        unsafe { let _ = RegisterClassW(&wc); }
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP, 0, 0, WIDTH_BASE, HEIGHT_BASE,
            None, None, hinst, None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let sc = dpi / 96.0;
    let w = (WIDTH_BASE as f32 * sc) as i32;
    let h = (HEIGHT_BASE as f32 * sc) as i32;

    let mut st = PowerState {
        theme,
        awake: AwakeMode::Off,
        pending: None,
        countdown_hwnd: None,
        window_pos: None,
        visible: false,
    };

    let work = work_rect();
    let mut drag: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;

    // Paint & show
    paint_main(hwnd, &mut st, &work, w, h, sc);
    st.visible = true;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        let _ = SetTimer(hwnd, POWER_TIMER_ID, 1000, None);
    }

    let event = hdl.0;
    let mut want_exit = false;

    loop {
        if want_exit || dying.load(std::sync::atomic::Ordering::Acquire) { break; }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        const MSG: WAIT_EVENT = WAIT_EVENT(1);

        match res {
            WAIT_OBJECT_0 => {
                drag = None; mouse_tracked = false;
                let _ = unsafe { ReleaseCapture() };
                want_exit = true;
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); let _ = KillTimer(hwnd, POWER_TIMER_ID); }
                destroy_cd(&mut st);
            }
            MSG => {
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT { want_exit = true; break; }
                        if msg.message == CANCEL_CD_MSG && msg.hwnd == hwnd {
                            st.pending = None;
                            destroy_cd(&mut st);
                            paint_main(hwnd, &mut st, &work, w, h, sc);
                            continue;
                        }
                        if msg.hwnd == hwnd {
                            if !handle_msg(hwnd, &msg, &mut st, &work, w, h, sc,
                                           &mut drag, &mut mouse_tracked, &mut want_exit) { break; }
                        }
                        if let Some(cd) = st.countdown_hwnd {
                            if msg.hwnd == cd { DispatchMessageW(&msg); }
                        }
                    }
                }
                tick(hwnd, &mut st, &work, w, h, sc);
            }
            _ => break,
        }
    }

    destroy_cd(&mut st);
    unsafe { let _ = SetThreadExecutionState(ES_CONTINUOUS); }
    unsafe { let _ = DestroyWindow(hwnd); }
}

// ── Timer tick ─────────────────────────────────────────────────────────

fn tick(main: HWND, st: &mut PowerState, work: &RECT, w: i32, h: i32, sc: f32) {
    let now = now_min();

    if let AwakeMode::Timed { end_min } = st.awake {
        if now >= end_min {
            st.awake = AwakeMode::Off;
            unsafe { let _ = SetThreadExecutionState(ES_CONTINUOUS); }
        }
    }

    if let Some(ref p) = st.pending.clone() {
        let remaining = p.execute_at.saturating_sub(now_secs());
        if remaining <= 10 && remaining > 0 && st.countdown_hwnd.is_none() {
            let cd = create_cd(main, p.op, remaining as u32, sc);
            st.countdown_hwnd = Some(cd);
        }
        if remaining == 0 {
            let op = p.op;
            st.pending = None;
            destroy_cd(st);
            exec(op, main);
        }
    }

    if st.visible { paint_main(main, st, work, w, h, sc); }
}

fn exec(op: PowerOp, _main: HWND) {
    match op {
        PowerOp::Sleep => unsafe { let _ = SetSuspendState(false, false, false); },
        PowerOp::Shutdown => unsafe { let _ = ExitWindowsEx(EWX_SHUTDOWN, SHUTDOWN_REASON(0)); },
        PowerOp::TurnOffScreen => {
            const SC_MONITORPOWER: usize = 0xF170;
            // HWND_BROADCAST = (HWND)0xFFFF
            let hwnd_broadcast = HWND(0xFFFF as *mut std::ffi::c_void);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    hwnd_broadcast,
                    WM_SYSCOMMAND, WPARAM(SC_MONITORPOWER), LPARAM(2),
                );
            }
        }
    }
}

// ── Countdown window management ────────────────────────────────────────

fn create_cd(main: HWND, op: PowerOp, secs: u32, sc: f32) -> HWND {
    let cd_cls = to_utf16_z("mhd_power_cd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();
    let cd_w = (280.0 * sc) as i32;
    let cd_h = (130.0 * sc) as i32;
    let wr = work_rect();
    let cx = wr.left + (wr.right - wr.left - cd_w) / 2;
    let cy = wr.top + (wr.bottom - wr.top - cd_h) / 2;

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cd_cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP, cx, cy, cd_w, cd_h,
            None, None, hinst, None,
        )
    } {
        Ok(h) => h,
        Err(_) => return HWND::default(),
    };

    let data = Box::new(CountdownData { main_hwnd: main, op, remaining_secs: secs });
    let ptr = Box::into_raw(data) as isize;
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr); }
    unsafe { let _ = SetTimer(hwnd, 1, 1000, None); }
    paint_cd(hwnd, op, secs, sc);
    unsafe { let _ = ShowWindow(hwnd, SW_SHOWNA); }
    hwnd
}

fn destroy_cd(st: &mut PowerState) {
    if let Some(h) = st.countdown_hwnd.take() {
        unsafe {
            let ptr = SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            if ptr != 0 { let _ = Box::from_raw(ptr as *mut CountdownData); }
            let _ = KillTimer(h, 1);
            let _ = DestroyWindow(h);
        }
    }
}

// ── Window procedures ─────────────────────────────────────────────────

extern "system" fn power_wndproc(_hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(_hwnd, msg, wp, lp) }
}

extern "system" fn cd_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr);
                if ptr != 0 {
                    let data = &mut *(ptr as *mut CountdownData);
                    if data.remaining_secs > 0 { data.remaining_secs -= 1; }
                    if data.remaining_secs == 0 {
                        let _ = Box::from_raw(ptr as *mut CountdownData);
                        let _ = KillTimer(hwnd, 1);
                        let _ = DestroyWindow(hwnd);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                        return LRESULT(0);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr);
                if ptr != 0 {
                    let data = &*(ptr as *const CountdownData);
                    let dpi = GetDpiForWindow(hwnd) as f32;
                    paint_cd(hwnd, data.op, data.remaining_secs, dpi / 96.0);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (lp.0 as i32) & 0xFFFF;
                let y = ((lp.0 as i32) >> 16) & 0xFFFF;
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if ptr != 0 {
                    let data = &mut *(ptr as *mut CountdownData);
                    let dpi = GetDpiForWindow(hwnd) as f32;
                    let sc = dpi / 96.0;
                    let cw = (280.0 * sc) as i32;
                    let bw = (80.0 * sc) as i32;
                    let bh = (28.0 * sc) as i32;
                    let bx = (cw - bw) / 2;
                    let by = (84.0 * sc) as i32;
                    if x >= bx && x < bx + bw && y >= by && y < by + bh {
                        let _ = PostMessageW(data.main_hwnd, CANCEL_CD_MSG, WPARAM(0), LPARAM(0));
                        let _ = Box::from_raw(ptr as *mut CountdownData);
                        let _ = KillTimer(hwnd, 1);
                        let _ = DestroyWindow(hwnd);
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    } else {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr);
                    }
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ── Main window message handling ──────────────────────────────────────

fn handle_msg(
    hwnd: HWND, msg: &MSG, st: &mut PowerState, work: &RECT,
    w: i32, h: i32, sc: f32,
    drag: &mut Option<(i32, i32)>, mouse_tracked: &mut bool,
    want_exit: &mut bool,
) -> bool {
    match msg.message {
        WM_KEYDOWN if msg.wParam.0 as u32 == 0x1B => {
            *want_exit = true;
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); let _ = KillTimer(hwnd, POWER_TIMER_ID); }
            return false;
        }
        WM_ACTIVATE if msg.wParam.0 as u32 == 0 => {
            *want_exit = true;
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); let _ = KillTimer(hwnd, POWER_TIMER_ID); }
            return false;
        }
        WM_LBUTTONDOWN => {
            let x = (msg.lParam.0 as i32) & 0xFFFF;
            let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
            let hdr_end = (HEADER_H as f32 * sc) as i32;

            if y < hdr_end {
                let close_x = w - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
                if x >= close_x && x <= close_x + (20.0 * sc) as i32 {
                    *want_exit = true;
                    unsafe { let _ = ShowWindow(hwnd, SW_HIDE); let _ = KillTimer(hwnd, POWER_TIMER_ID); }
                    return false;
                }
                *drag = Some((x, y));
                unsafe { let _ = SetCapture(hwnd); }
                return true;
            }

            if let Some(mode) = hit_awake(x, y, sc) {
                st.awake = mode;
                match mode {
                    AwakeMode::Off => unsafe { let _ = SetThreadExecutionState(ES_CONTINUOUS); },
                    AwakeMode::Forever => unsafe { let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED); },
                    AwakeMode::Timed { .. } => unsafe { let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED); },
                }
                paint_main(hwnd, st, work, w, h, sc);
                return true;
            }

            if let Some(op) = hit_act(x, y, sc) {
                match op {
                    PowerOp::TurnOffScreen => exec(PowerOp::TurnOffScreen, hwnd),
                    _ => st.pending = Some(PendingAction { op, execute_at: now_secs() + 10 }),
                }
                paint_main(hwnd, st, work, w, h, sc);
                return true;
            }

            if let Some((op, mins)) = hit_timer(x, y, sc) {
                st.pending = Some(PendingAction { op, execute_at: now_secs() + mins as u64 * 60 });
                paint_main(hwnd, st, work, w, h, sc);
                return true;
            }

            if st.pending.is_some() && hit_pcancel(x, y, w, sc) {
                st.pending = None;
                destroy_cd(st);
                paint_main(hwnd, st, work, w, h, sc);
                return true;
            }
        }
        WM_LBUTTONUP => {
            if drag.is_some() { *drag = None; unsafe { let _ = ReleaseCapture(); } }
        }
        WM_MOUSEMOVE => {
            if let Some((sx, sy)) = *drag {
                let cx = (msg.lParam.0 as i32) & 0xFFFF;
                let cy = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
                let mut r = RECT::default();
                unsafe { let _ = GetWindowRect(hwnd, &mut r); }
                unsafe {
                    let _ = SetWindowPos(hwnd, HWND::default(),
                        r.left + cx - sx, r.top + cy - sy,
                        0, 0, SWP_NOSIZE | SWP_NOZORDER);
                }
                *drag = Some((cx, cy));
            }
            if !*mouse_tracked {
                let mut tm = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE, hwndTrack: hwnd,
                    ..Default::default()
                };
                unsafe { let _ = TrackMouseEvent(&mut tm); }
                *mouse_tracked = true;
            }
        }
        WM_MOUSELEAVE => *mouse_tracked = false,
        _ => {}
    }
    true
}

// ── Hit testing ────────────────────────────────────────────────────────

fn hit_awake(x: i32, y: i32, sc: f32) -> Option<AwakeMode> {
    let by = (AWAKE_BTN_Y as f32 * sc) as i32;
    let bh = (AWAKE_BTN_H as f32 * sc) as i32;
    if y < by || y >= by + bh { return None; }
    let bw = (AWAKE_BTN_W as f32 * sc) as i32;
    let gap = (BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;
    for i in 0..AWAKE_COUNT {
        let bx = sx + i as i32 * (bw + gap);
        if x >= bx && x < bx + bw {
            return Some(match i {
                0 => AwakeMode::Off,
                1 => AwakeMode::Forever,
                2 => AwakeMode::Timed { end_min: now_min() + 30 },
                3 => AwakeMode::Timed { end_min: now_min() + 60 },
                4 => AwakeMode::Timed { end_min: now_min() + 120 },
                5 => AwakeMode::Timed { end_min: now_min() + 240 },
                _ => unreachable!(),
            });
        }
    }
    None
}

fn hit_act(x: i32, y: i32, sc: f32) -> Option<PowerOp> {
    let by = (ACT_BTN_Y as f32 * sc) as i32;
    let bh = (ACT_BTN_H as f32 * sc) as i32;
    if y < by || y >= by + bh { return None; }
    let bw = (ACT_BTN_W as f32 * sc) as i32;
    let gap = (ACT_BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;
    for i in 0..3 {
        let bx = sx + i as i32 * (bw + gap);
        if x >= bx && x < bx + bw {
            return Some(match i { 0 => PowerOp::Sleep, 1 => PowerOp::Shutdown, 2 => PowerOp::TurnOffScreen, _ => unreachable!() });
        }
    }
    None
}

fn hit_timer(x: i32, y: i32, sc: f32) -> Option<(PowerOp, u32)> {
    let bw = (TIMER_BTN_W as f32 * sc) as i32;
    let bh = (TIMER_BTN_H as f32 * sc) as i32;
    let gap = (BTN_GAP as f32 * sc) as i32;
    let timers = [5u32, 15, 30, 60];

    let r1 = (TIMER_ROW1_Y as f32 * sc) as i32;
    if y >= r1 && y < r1 + bh {
        let sx = (PAD as f32 * sc + 50.0 * sc) as i32;
        for i in 0..4 {
            let bx = sx + i as i32 * (bw + gap);
            if x >= bx && x < bx + bw { return Some((PowerOp::Sleep, timers[i])); }
        }
    }
    let r2 = (TIMER_ROW2_Y as f32 * sc) as i32;
    if y >= r2 && y < r2 + bh {
        let sx = (PAD as f32 * sc + 50.0 * sc) as i32;
        for i in 0..4 {
            let bx = sx + i as i32 * (bw + gap);
            if x >= bx && x < bx + bw { return Some((PowerOp::Shutdown, timers[i])); }
        }
    }
    None
}

fn hit_pcancel(x: i32, y: i32, w: i32, sc: f32) -> bool {
    let py = (PENDING_Y as f32 * sc) as i32;
    let ph = (PENDING_H as f32 * sc) as i32;
    if y < py || y >= py + ph { return false; }
    let bw = (PENDING_BTN_W as f32 * sc) as i32;
    let bh = (22.0 * sc) as i32;
    let bx = w - (PAD as f32 * sc) as i32 - bw;
    let by = py + (ph - bh) / 2;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

// ── Painting ───────────────────────────────────────────────────────────

fn paint_main(hwnd: HWND, st: &PowerState, work: &RECT, w: i32, h: i32, sc: f32) {
    let pos = st.window_pos.unwrap_or_else(|| POINT {
        x: work.right - (WIDTH_BASE as f32 * sc) as i32 - (16.0 * sc) as i32,
        y: work.top + (48.0 * sc) as i32,
    });
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), pos.x, pos.y, w, h, SWP_NOZORDER); }

    let dc = unsafe { GetDC(hwnd) };
    if dc.is_invalid() { return; }
    let mem = unsafe { CreateCompatibleDC(dc) };
    if mem.is_invalid() { unsafe { let _ = ReleaseDC(hwnd, dc); } return; }

    let bmp = make_dib(mem, w, h);
    let _old_bmp = unsafe { SelectObject(mem, bmp) };

    let font = make_font(14, sc);
    let _old_font = unsafe { SelectObject(mem, font) };

    let bg = st.theme.background;
    let fg = st.theme.text;
    let accent = st.theme.accent;
    let dim = st.theme.text_muted;

    // Clear (transparent)
    fill_rect(mem, 0, 0, w, h, Argb { a: 0, r: 0, g: 0, b: 0 });

    // Background
    rr_fill(mem, 0, 0, w, h, (RADIUS_BASE as f32 * sc) as i32, bg);

    // ── Header ──
    let r = rect((PAD as f32 * sc) as i32, (10.0 * sc) as i32, w - (PAD as f32 * sc) as i32, (HEADER_H as f32 * sc) as i32);
    set_tc(mem, fg);
    dw(mem, &mut to_utf16_z("⏻ Power Control"), &mut rect_clone(&r), DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // Close button
    let cx = w - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
    let cr = rect(cx, (10.0 * sc) as i32, cx + (20.0 * sc) as i32, (HEADER_H as f32 * sc) as i32 - (10.0 * sc) as i32);
    set_tc(mem, dim);
    dw(mem, &mut to_utf16_z("✕"), &mut rect_clone(&cr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // ── Keep Awake label ──
    let lbl_y = (AWAKE_BTN_Y as f32 * sc - 16.0 * sc) as i32;
    let lr = rect((PAD as f32 * sc) as i32, lbl_y, w, (AWAKE_BTN_Y as f32 * sc) as i32);
    set_tc(mem, dim);
    dw(mem, &mut to_utf16_z("⚡ Keep Awake"), &mut rect_clone(&lr), DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // ── Awake buttons ──
    let by = (AWAKE_BTN_Y as f32 * sc) as i32;
    let bw = (AWAKE_BTN_W as f32 * sc) as i32;
    let bh = (AWAKE_BTN_H as f32 * sc) as i32;
    let gap = (BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;

    for i in 0..AWAKE_COUNT {
        let bx = sx + i as i32 * (bw + gap);
        let sel = match st.awake {
            AwakeMode::Off => i == 0,
            AwakeMode::Forever => i == 1,
            AwakeMode::Timed { .. } => i >= 2,
        };
        let (btnc, txtc) = if sel { (accent, st.theme.background) } else { (dim, fg) };
        rr_fill(mem, bx, by, bx + bw, by + bh, (4.0 * sc) as i32, btnc);
        let tr = rect(bx, by, bx + bw, by + bh);
        set_tc(mem, txtc);
        dw(mem, &mut to_utf16_z(AWAKE_LABELS[i]), &mut rect_clone(&tr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    // ── Status line ──
    let status = match st.awake {
        AwakeMode::Off => "○  System may sleep".to_string(),
        AwakeMode::Forever => "●  Awake (forever)".to_string(),
        AwakeMode::Timed { end_min } => {
            let rem = end_min.saturating_sub(now_min());
            if rem > 0 { format!("●  Awake ({}h {:02}m left)", rem / 60, rem % 60) }
            else { "○  System may sleep".to_string() }
        }
    };
    let sr = rect((PAD as f32 * sc) as i32, (STATUS_Y as f32 * sc) as i32, w, (STATUS_Y as f32 * sc + STATUS_H as f32 * sc) as i32);
    set_tc(mem, accent);
    dw(mem, &mut to_utf16_z(&status), &mut rect_clone(&sr), DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // ── Separator ──
    let sep_y = (106.0 * sc) as i32;
    rr_fill(mem, (PAD as f32 * sc) as i32, sep_y, w - (PAD as f32 * sc) as i32, sep_y + (2.0 * sc) as i32, 0, dim);

    // ── Actions label ──
    let ar = rect((PAD as f32 * sc) as i32, (ACTIONS_LABEL_Y as f32 * sc) as i32, w, (ACTIONS_LABEL_Y as f32 * sc + ACTIONS_LABEL_H as f32 * sc) as i32);
    set_tc(mem, dim);
    dw(mem, &mut to_utf16_z("Actions"), &mut rect_clone(&ar), DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // ── Action buttons ──
    let aby = (ACT_BTN_Y as f32 * sc) as i32;
    let abw = (ACT_BTN_W as f32 * sc) as i32;
    let abh = (ACT_BTN_H as f32 * sc) as i32;
    let agap = (ACT_BTN_GAP as f32 * sc) as i32;
    let act_lbls = ["💤 Sleep", "⏻ Shutdown", "💡 Screen off"];
    for i in 0..3 {
        let bx = sx + i as i32 * (abw + agap);
        rr_fill(mem, bx, aby, bx + abw, aby + abh, (6.0 * sc) as i32, dim);
        let tr = rect(bx, aby, bx + abw, aby + abh);
        set_tc(mem, fg);
        dw(mem, &mut to_utf16_z(act_lbls[i]), &mut rect_clone(&tr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    // ── Timer rows ──
    let tbw = (TIMER_BTN_W as f32 * sc) as i32;
    let tbh = (TIMER_BTN_H as f32 * sc) as i32;
    let tvals = ["5m", "15m", "30m", "1h"];
    let tlabels = ["Sleep:", "Shutdown:"];
    let rows_y = [(TIMER_ROW1_Y as f32 * sc) as i32, (TIMER_ROW2_Y as f32 * sc) as i32];
    for (ri, label) in tlabels.iter().enumerate() {
        let ry = rows_y[ri];
        let lr = rect(sx, ry, sx + (50.0 * sc) as i32, ry + tbh);
        set_tc(mem, dim);
        dw(mem, &mut to_utf16_z(label), &mut rect_clone(&lr), DT_LEFT | DT_SINGLELINE | DT_VCENTER);

        let tsx = sx + (50.0 * sc) as i32 + gap;
        for bi in 0..4 {
            let bx = tsx + bi as i32 * (tbw + gap);
            rr_fill(mem, bx, ry, bx + tbw, ry + tbh, (3.0 * sc) as i32, dim);
            let tr = rect(bx, ry, bx + tbw, ry + tbh);
            set_tc(mem, fg);
            dw(mem, &mut to_utf16_z(tvals[bi]), &mut rect_clone(&tr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        }
    }

    // ── Pending row ──
    if let Some(ref p) = st.pending {
        let py = (PENDING_Y as f32 * sc) as i32;
        let ph = (PENDING_H as f32 * sc) as i32;
        let op_txt = match p.op { PowerOp::Sleep => "Sleep", PowerOp::Shutdown => "Shutdown", PowerOp::TurnOffScreen => "Screen off" };
        let remaining = p.execute_at.saturating_sub(now_secs());
        let m = remaining / 60; let s = remaining % 60;
        let txt = if remaining > 0 { format!("⏹  {} in {}:{:02}", op_txt, m, s) } else { format!("⏹  {} executing…", op_txt) };

        let tr = rect((PAD as f32 * sc) as i32, py, w - (PAD as f32 * sc) as i32 - (PENDING_BTN_W as f32 * sc + 4.0 * sc) as i32, py + ph);
        set_tc(mem, accent);
        dw(mem, &mut to_utf16_z(&txt), &mut rect_clone(&tr), DT_LEFT | DT_SINGLELINE | DT_VCENTER);

        let cbw = (PENDING_BTN_W as f32 * sc) as i32;
        let cbh = (22.0 * sc) as i32;
        let cbx = w - (PAD as f32 * sc) as i32 - cbw;
        let cby = py + (ph - cbh) / 2;
        rr_fill(mem, cbx, cby, cbx + cbw, cby + cbh, (4.0 * sc) as i32, dim);
        let cr2 = rect(cbx, cby, cbx + cbw, cby + cbh);
        set_tc(mem, fg);
        dw(mem, &mut to_utf16_z("Cancel"), &mut rect_clone(&cr2), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    // Blit
    unsafe {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE { cx: w, cy: h };
        let pt_dst = pos;
        let _ = UpdateLayeredWindow(
            hwnd, HDC::default(),
            Some(&pt_dst), Some(&sz),
            mem, Some(&pt_src),
            COLORREF(0), Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe { let _ = DeleteObject(font); _ = DeleteObject(bmp); _ = DeleteDC(mem); _ = ReleaseDC(hwnd, dc); }
}

// ── Countdown painting ────────────────────────────────────────────────

fn paint_cd(hwnd: HWND, op: PowerOp, secs: u32, sc: f32) {
    let w = (280.0 * sc) as i32;
    let h = (130.0 * sc) as i32;

    let dc = unsafe { GetDC(hwnd) };
    if dc.is_invalid() { return; }
    let mem = unsafe { CreateCompatibleDC(dc) };
    if mem.is_invalid() { unsafe { let _ = ReleaseDC(hwnd, dc); } return; }

    let bmp = make_dib(mem, w, h);
    let _old_bmp = unsafe { SelectObject(mem, bmp) };

    let font = make_font(14, sc);
    let _old_font = unsafe { SelectObject(mem, font) };

    let bg = Argb { a: 230, r: 30, g: 30, b: 30 };
    let fg = Argb { a: 255, r: 220, g: 220, b: 220 };
    let accent2 = Argb { a: 255, r: 255, g: 100, b: 50 };
    let dim2 = Argb { a: 200, r: 80, g: 80, b: 80 };

    fill_rect(mem, 0, 0, w, h, Argb { a: 0, r: 0, g: 0, b: 0 });
    rr_fill(mem, 0, 0, w, h, (RADIUS_BASE as f32 * sc) as i32, bg);

    let op_txt = match op { PowerOp::Sleep => "Sleep", PowerOp::Shutdown => "Shutdown", PowerOp::TurnOffScreen => "Screen off" };

    // Warning
    let wr = rect(0, (12.0 * sc) as i32, w, (42.0 * sc) as i32);
    let warn = format!("{} in", op_txt);
    set_tc(mem, fg);
    dw(mem, &mut to_utf16_z(&warn), &mut rect_clone(&wr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // Timer (larger font)
    let tf = make_font(36, sc);
    let _old_tf = unsafe { SelectObject(mem, tf) };
    let tr = rect(0, (42.0 * sc) as i32, w, (76.0 * sc) as i32);
    let ttxt = format!("0:{:02}", secs);
    set_tc(mem, accent2);
    dw(mem, &mut to_utf16_z(&ttxt), &mut rect_clone(&tr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    unsafe { let _ = DeleteObject(tf); }
    unsafe { let _ = SelectObject(mem, font); }

    // Cancel button
    let bw = (80.0 * sc) as i32;
    let bh = (28.0 * sc) as i32;
    let bx = (w - bw) / 2;
    let by = (84.0 * sc) as i32;
    rr_fill(mem, bx, by, bx + bw, by + bh, (4.0 * sc) as i32, dim2);
    let cr = rect(bx, by, bx + bw, by + bh);
    set_tc(mem, fg);
    dw(mem, &mut to_utf16_z("Cancel"), &mut rect_clone(&cr), DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // Blit
    unsafe {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let mut wrr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wrr);
        let pt_dst = POINT { x: wrr.left, y: wrr.top };
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE { cx: w, cy: h };
        let _ = UpdateLayeredWindow(
            hwnd, HDC::default(),
            Some(&pt_dst), Some(&sz),
            mem, Some(&pt_src),
            COLORREF(0), Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe { let _ = DeleteObject(font); _ = DeleteObject(bmp); _ = DeleteDC(mem); _ = ReleaseDC(hwnd, dc); }
}

// ── Drawing helpers ────────────────────────────────────────────────────

fn make_dib(dc: HDC, w: i32, h: i32) -> windows::Win32::Graphics::Gdi::HBITMAP {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }; 1],
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    unsafe { CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default() }
}

fn make_font(size: i32, sc: f32) -> windows::Win32::Graphics::Gdi::HFONT {
    let h = (size as f32 * sc) as i32;
    let name = to_utf16_z("Segoe UI");
    unsafe {
        CreateFontW(
            -h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(name.as_ptr()),
        )
    }
}

fn fill_rect(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: Argb) {
    if x2 <= x1 || y2 <= y1 { return; }
    let br = unsafe { CreateSolidBrush(color.to_colorref()) };
    let r = RECT { left: x1, top: y1, right: x2, bottom: y2 };
    unsafe { let _ = FillRect(dc, &r, br); let _ = DeleteObject(br); }
}

fn rr_fill(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, radius: i32, color: Argb) {
    if x2 <= x1 || y2 <= y1 { return; }
    let br = unsafe { CreateSolidBrush(color.to_colorref()) };
    if radius == 0 {
        unsafe { let _ = FillRect(dc, &RECT { left: x1, top: y1, right: x2, bottom: y2 }, br); let _ = DeleteObject(br); }
        return;
    }
    let r = radius.min((x2 - x1) / 2).min((y2 - y1) / 2);
    unsafe {
        FillRect(dc, &RECT { left: x1, top: y1 + r, right: x2, bottom: y2 - r }, br);
        FillRect(dc, &RECT { left: x1 + r, top: y1, right: x2 - r, bottom: y1 + r }, br);
        FillRect(dc, &RECT { left: x1 + r, top: y2 - r, right: x2 - r, bottom: y2 }, br);
        let _ = SelectObject(dc, br);
        let _ = Ellipse(dc, x1, y1, x1 + r * 2, y1 + r * 2);
        let _ = Ellipse(dc, x2 - r * 2, y1, x2, y1 + r * 2);
        let _ = Ellipse(dc, x1, y2 - r * 2, x1 + r * 2, y2);
        let _ = Ellipse(dc, x2 - r * 2, y2 - r * 2, x2, y2);
        let _ = DeleteObject(br);
    }
}

fn set_tc(dc: HDC, color: Argb) {
    unsafe { let _ = SetTextColor(dc, color.to_colorref()); let _ = SetBkMode(dc, TRANSPARENT); }
}

fn dw(dc: HDC, text: &mut Vec<u16>, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe { let _ = DrawTextW(dc, text, rc as *mut RECT as *mut _, fmt); }
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

fn rect_clone(src: &RECT) -> RECT {
    RECT { left: src.left, top: src.top, right: src.right, bottom: src.bottom }
}
