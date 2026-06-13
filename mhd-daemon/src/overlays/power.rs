//! Power Control overlay — keep awake, sleep, shutdown, turn off screen.
//!
//! Ephemeral thread pattern (like volume_mixer / monitor_panel).

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use crate::constants::WM_MOUSELEAVE;

use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_EVENT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, FillRect, GetMonitorInfoW, HDC, InvalidateRect,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetSuspendState, SetThreadExecutionState,
};
use windows::Win32::System::Shutdown::{EWX_SHUTDOWN, ExitWindowsEx, SHUTDOWN_REASON};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetDesktopWindow, GetWindowRect, IDC_ARROW, KillTimer,
    LoadCursorW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostMessageW,
    QS_ALLINPUT, RegisterClassW, SW_HIDE, SW_SHOWNA, SWP_NOSIZE, SWP_NOZORDER, SetTimer,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WM_ACTIVATE, WM_APP, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WM_SYSCOMMAND, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::native_theme::{Argb, NativeTheme};
use crate::osd::to_utf16_z;

// ── Layout (unscaled, scale = 96 DPI) ─────────────────────────────────
const W: i32 = 380;
const PAD: i32 = 14;

// Y positions
const HDR_H: i32 = 44; // header
const AW_BTN_Y: i32 = 44; // awake buttons row
const AW_BTN_H: i32 = 26;
const STATUS_Y: i32 = 76;
const STATUS_H: i32 = 22;
const SEP1_Y: i32 = 102; // separator
const ACT_LBL_Y: i32 = 108;
const ACT_LBL_H: i32 = 18;
const ACT_BTN_Y: i32 = 128;
const ACT_BTN_H: i32 = 30;
const TMR_ROW1_Y: i32 = 162;
const TMR_ROW2_Y: i32 = 180;
const TMR_BTN_H: i32 = 18;
const PEND_Y: i32 = 204;
const PEND_H: i32 = 26;

const TOTAL_H: i32 = 234;

// Awake button sizes
const AW_BTN_W: i32 = 54;
const AW_BTN_GAP: i32 = 5;
const AW_COUNT: usize = 6;

// Action button sizes
const ACT_BTN_W: i32 = 110;
const ACT_BTN_GAP: i32 = 10;

// Timer buttons
const TMR_BTN_W: i32 = 48;
const TMR_BTN_GAP: i32 = 4;
const TMR_LABEL_W: i32 = 70;

const RADIUS: i32 = 8;
const TMR_ID: usize = 1; // 1-second tick
const CANCEL_MSG: u32 = WM_APP + 1;

const AWAKE_NAMES: [&str; AW_COUNT] = ["Off", "Forever", "30m", "1h", "2h", "4h"];

// ── State types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum AwakeMode {
    Off,
    Forever,
    Timed { end_sec: u64 }, // seconds since epoch
}

#[derive(Clone, Copy, PartialEq)]
enum PowerOp {
    Sleep,
    Shutdown,
    TurnOffScreen,
}

struct Pending {
    op: PowerOp,
    at_sec: u64, // execute at this second
}

struct State {
    theme: NativeTheme,
    awake: AwakeMode,
    pending: Option<Pending>,
    cd_hwnd: Option<HWND>,
    pos: POINT, // current window position
}

struct CdData {
    main_hwnd: HWND,
    op: PowerOp,
    secs: u32,
}

// ── Thread control (static) ───────────────────────────────────────────

static CTRL: Mutex<Option<Ctrl>> = Mutex::new(None);

#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct Ctrl {
    ev: SafeHandle,
    dying: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        self.dying.store(true, std::sync::atomic::Ordering::Release);
        unsafe {
            let _ = SetEvent(self.ev.0);
        }
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Show (or re‑show) the Power Control overlay.
/// The worker thread keeps running even when the window is hidden,
/// preserving keep‑awake state and pending timer actions.
pub fn show(theme: NativeTheme) {
    let mut g = CTRL.lock().unwrap();
    if let Some(ref ctrl) = *g {
        // Thread already alive — signal it to toggle visibility
        unsafe {
            let _ = SetEvent(ctrl.ev.0);
        }
        return;
    }
    let ev = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = Ctrl {
        ev: SafeHandle(ev),
        dying: dying.clone(),
    };
    let cev = c.ev.clone();
    let cdy = c.dying.clone();
    *g = Some(c);
    drop(g);
    std::thread::Builder::new()
        .name("mhd-power".into())
        .spawn(move || thread_main(cev, cdy, theme))
        .ok();
}

// ── Helpers ────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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

// ── Thread entry point ─────────────────────────────────────────────────

fn thread_main(hdl: SafeHandle, dying: Arc<std::sync::atomic::AtomicBool>, theme: NativeTheme) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls = to_utf16_z("mhd_power_cls");
    let cd_cls = to_utf16_z("mhd_power_cd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();

    for (n, wp) in [
        (&cls, Some(power_wndproc as _)),
        (&cd_cls, Some(cd_wndproc as _)),
    ] {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: wp,
            hInstance: hinst,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
            lpszClassName: PCWSTR::from_raw(n.as_ptr()),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&wc);
        }
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            W,
            TOTAL_H,
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
    let win_h = (TOTAL_H as f32 * sc) as i32;

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

    let mut st = State {
        theme,
        awake: AwakeMode::Off,
        pending: None,
        cd_hwnd: None,
        pos,
    };

    if dying.load(std::sync::atomic::Ordering::Acquire) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    paint_main(hwnd, &st, win_w, win_h, sc);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        let _ = SetTimer(hwnd, TMR_ID, 1000, None);
    }

    let event = hdl.0;
    let mut hidden = false; // true when window is hidden but thread alive
    let mut drag: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;

    loop {
        if dying.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        match res {
            WAIT_OBJECT_0 => {
                // show() called again → toggle visibility
                hidden = !hidden;
                if hidden {
                    unsafe {
                        let _ = ReleaseCapture();
                        _ = ShowWindow(hwnd, SW_HIDE);
                        _ = InvalidateRect(hwnd, None, false);
                    }
                    destroy_cd(&mut st);
                } else {
                    paint_main(hwnd, &st, win_w, win_h, sc);
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_SHOWNA);
                    }
                }
            }
            WAIT_EVENT(1) => {
                // messages
                let mut msg = MSG::default();
                let mut repaint = false;
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            break;
                        }
                        if msg.message == CANCEL_MSG && msg.hwnd == hwnd {
                            st.pending = None;
                            destroy_cd(&mut st);
                            repaint = true;
                            continue;
                        }
                        if msg.hwnd == hwnd {
                            if !matches!(msg.message, WM_MOUSEMOVE | WM_MOUSELEAVE) {
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
                        if let Some(cd) = st.cd_hwnd
                            && msg.hwnd == cd {
                                DispatchMessageW(&msg);
                            }
                    }
                }
                tick(&mut st, hwnd, sc);
                if repaint && !hidden {
                    paint_main(hwnd, &st, win_w, win_h, sc);
                }
            }
            _ => break,
        }
    }

    destroy_cd(&mut st);
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS);
        _ = DestroyWindow(hwnd);
    }
}

// ── Tick (called on timer) ─────────────────────────────────────────────

fn tick(st: &mut State, main_hwnd: HWND, sc: f32) {
    // Awake timed expiry
    if let AwakeMode::Timed { end_sec } = st.awake
        && now_secs() >= end_sec {
            st.awake = AwakeMode::Off;
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS);
            }
        }

    // Pending action
    if let Some(ref p) = st.pending {
        let remaining = p.at_sec.saturating_sub(now_secs());
        if remaining <= 10 && remaining > 0 && st.cd_hwnd.is_none() {
            st.cd_hwnd = Some(create_cd(main_hwnd, p.op, remaining as u32, sc));
        }
        if remaining == 0 {
            let op = p.op;
            st.pending = None;
            destroy_cd(st);
            exec(op);
        }
    }
}

fn exec(op: PowerOp) {
    match op {
        PowerOp::Sleep => unsafe {
            let _ = SetSuspendState(false, false, false);
        },
        PowerOp::Shutdown => unsafe {
            let _ = ExitWindowsEx(EWX_SHUTDOWN, SHUTDOWN_REASON(0));
        },
        PowerOp::TurnOffScreen => {
            // SendMessage(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2)
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    HWND(0xFFFF as *mut c_void),
                    WM_SYSCOMMAND,
                    WPARAM(0xF170),
                    LPARAM(2),
                );
            }
        }
    }
}

// ── Countdown window ──────────────────────────────────────────────────

fn create_cd(main_hwnd: HWND, op: PowerOp, secs: u32, sc: f32) -> HWND {
    let cls = to_utf16_z("mhd_power_cd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();
    let cw = (280.0 * sc) as i32;
    let ch = (130.0 * sc) as i32;
    let wr = work_area();
    let cx = wr.left + (wr.right - wr.left - cw) / 2;
    let cy = wr.top + (wr.bottom - wr.top - ch) / 2;

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            cx,
            cy,
            cw,
            ch,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return HWND::default(),
    };

    let data = Box::new(CdData {
        main_hwnd,
        op,
        secs,
    });
    // Need main HWND — we'll set it later via SetWindowLongPtrW from thread_main
    // Actually we know it from the caller; pass it via the data.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);
    }
    unsafe {
        let _ = SetTimer(hwnd, 1, 1000, None);
    }
    paint_cd(hwnd, op, secs, sc);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNA);
    }
    hwnd
}

fn destroy_cd(st: &mut State) {
    if let Some(h) = st.cd_hwnd.take() {
        unsafe {
            let ptr = SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            if ptr != 0 {
                let _ = Box::from_raw(ptr as *mut CdData);
            }
            let _ = KillTimer(h, 1);
            let _ = DestroyWindow(h);
        }
    }
}

// ── Window procs ───────────────────────────────────────────────────────

extern "system" fn power_wndproc(_h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(_h, msg, wp, lp) }
}

extern "system" fn cd_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr);
                if ptr != 0 {
                    let d = &mut *(ptr as *mut CdData);
                    if d.secs > 0 {
                        d.secs -= 1;
                    }
                    if d.secs == 0 {
                        let _ = Box::from_raw(ptr as *mut CdData);
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
                    let d = &*(ptr as *const CdData);
                    let sc2 = sc_from_hwnd(hwnd);
                    paint_cd(hwnd, d.op, d.secs, sc2);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = lp.0 as i32 & 0xFFFF;
                let y = (lp.0 as i32 >> 16) & 0xFFFF;
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if ptr != 0 {
                    let d = &mut *(ptr as *mut CdData);
                    let sc2 = sc_from_hwnd(hwnd);
                    let cw = (280.0 * sc2) as i32;
                    let bw = (80.0 * sc2) as i32;
                    let bh = (28.0 * sc2) as i32;
                    let bx = (cw - bw) / 2;
                    let by = (84.0 * sc2) as i32;
                    if x >= bx && x < bx + bw && y >= by && y < by + bh {
                        let _ = PostMessageW(d.main_hwnd, CANCEL_MSG, WPARAM(0), LPARAM(0));
                        let _ = Box::from_raw(ptr as *mut CdData);
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

// ── Message handler for main window ───────────────────────────────────

fn msg_handler(
    hwnd: HWND,
    msg: &MSG,
    st: &mut State,
    drag: &mut Option<(i32, i32)>,
    mouse_tracked: &mut bool,
    hidden: &mut bool,
    sc: f32,
) -> bool {
    if msg.message == WM_KEYDOWN && msg.wParam.0 as u32 == 0x1B {
        *hidden = true;
        destroy_cd(st);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            _ = ReleaseCapture();
        }
        return false;
    }
    if msg.message == WM_ACTIVATE && msg.wParam.0 as u32 == 0 {
        *hidden = true;
        destroy_cd(st);
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            _ = ReleaseCapture();
        }
        return false;
    }
    if msg.message == WM_TIMER && msg.wParam.0 == TMR_ID {
        return true; // tick() is called after the message loop
    }

    if msg.message == WM_LBUTTONDOWN {
        let x = (msg.lParam.0 as i32) & 0xFFFF;
        let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;

        // Header: drag or close
        if y < (HDR_H as f32 * sc) as i32 {
            let cx = (W as f32 * sc) as i32 - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
            if x >= cx && x <= cx + (20.0 * sc) as i32 {
                *hidden = true;
                destroy_cd(st);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    _ = ReleaseCapture();
                }
                return false;
            }
            *drag = Some((x, y));
            unsafe {
                let _ = SetCapture(hwnd);
            }
            return true;
        }

        // Awake buttons
        if let Some(mode) = hit_awake(x, y, sc) {
            st.awake = mode;
            match mode {
                AwakeMode::Off => unsafe {
                    let _ = SetThreadExecutionState(ES_CONTINUOUS);
                },
                AwakeMode::Forever => unsafe {
                    let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                },
                AwakeMode::Timed { end_sec } => {
                    unsafe {
                        let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                    }
                    let _ = end_sec;
                }
            }
            return true;
        }

        // Action buttons
        if let Some(op) = hit_act(x, y, sc) {
            match op {
                PowerOp::TurnOffScreen => exec(PowerOp::TurnOffScreen),
                _ => {
                    st.pending = Some(Pending {
                        op,
                        at_sec: now_secs() + 10,
                    })
                }
            }
            return true;
        }

        // Timer buttons
        if let Some((op, mins)) = hit_timer(x, y, sc) {
            st.pending = Some(Pending {
                op,
                at_sec: now_secs() + mins as u64 * 60,
            });
            return true;
        }

        // Cancel pending
        if st.pending.is_some() && hit_pcancel(x, y, sc) {
            st.pending = None;
            destroy_cd(st);
            return true;
        }

        return true;
    }

    if msg.message == WM_LBUTTONUP {
        if drag.is_some() {
            *drag = None;
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        return true;
    }

    if msg.message == WM_MOUSEMOVE {
        if let Some((grab_x, grab_y)) = *drag {
            let mut cursor = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut cursor);
            }
            let nx = cursor.x - grab_x;
            let ny = cursor.y - grab_y;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    nx,
                    ny,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER,
                );
            }
            st.pos = POINT { x: nx, y: ny };
        }
        if !*hidden && !*mouse_tracked {
            let mut tm = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                ..Default::default()
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tm);
            }
            *mouse_tracked = true;
        }
        return true;
    }

    if msg.message == WM_MOUSELEAVE {
        *mouse_tracked = false;
        return true;
    }

    true
}

// ── Hit tests ──────────────────────────────────────────────────────────

fn hit_awake(x: i32, y: i32, sc: f32) -> Option<AwakeMode> {
    let by = (AW_BTN_Y as f32 * sc) as i32;
    let bh = (AW_BTN_H as f32 * sc) as i32;
    if y < by || y >= by + bh {
        return None;
    }
    let bw = (AW_BTN_W as f32 * sc) as i32;
    let gap = (AW_BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;
    for i in 0..AW_COUNT {
        let bx = sx + i as i32 * (bw + gap);
        if x >= bx && x < bx + bw {
            let end_sec = now_secs()
                + match i {
                    0 => 0, // Off — handled separately below
                    1 => 0, // Forever — handled separately
                    2 => 30,
                    3 => 60,
                    4 => 120,
                    5 => 240,
                    _ => 0,
                } * 60;
            return Some(match i {
                0 => AwakeMode::Off,
                1 => AwakeMode::Forever,
                _ => AwakeMode::Timed { end_sec },
            });
        }
    }
    None
}

fn hit_act(x: i32, y: i32, sc: f32) -> Option<PowerOp> {
    let by = (ACT_BTN_Y as f32 * sc) as i32;
    let bh = (ACT_BTN_H as f32 * sc) as i32;
    if y < by || y >= by + bh {
        return None;
    }
    let bw = (ACT_BTN_W as f32 * sc) as i32;
    let gap = (ACT_BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;
    for i in 0..3 {
        let bx = sx + i * (bw + gap);
        if x >= bx && x < bx + bw {
            return Some(match i {
                0 => PowerOp::Sleep,
                1 => PowerOp::Shutdown,
                2 => PowerOp::TurnOffScreen,
                _ => unreachable!(),
            });
        }
    }
    None
}

fn hit_timer(x: i32, y: i32, sc: f32) -> Option<(PowerOp, u32)> {
    let bw = (TMR_BTN_W as f32 * sc) as i32;
    let bh = (TMR_BTN_H as f32 * sc) as i32;
    let gap = (TMR_BTN_GAP as f32 * sc) as i32;
    let vals = [5u32, 15, 30, 60];

    let r1 = (TMR_ROW1_Y as f32 * sc) as i32;
    if y >= r1 && y < r1 + bh {
        let sx = (PAD as f32 * sc + TMR_LABEL_W as f32 * sc) as i32;
        for i in 0..4 {
            let bx = sx + i as i32 * (bw + gap);
            if x >= bx && x < bx + bw {
                return Some((PowerOp::Sleep, vals[i]));
            }
        }
    }
    let r2 = (TMR_ROW2_Y as f32 * sc) as i32;
    if y >= r2 && y < r2 + bh {
        let sx = (PAD as f32 * sc + TMR_LABEL_W as f32 * sc) as i32;
        for i in 0..4 {
            let bx = sx + i as i32 * (bw + gap);
            if x >= bx && x < bx + bw {
                return Some((PowerOp::Shutdown, vals[i]));
            }
        }
    }
    None
}

fn hit_pcancel(x: i32, y: i32, sc: f32) -> bool {
    let py = (PEND_Y as f32 * sc) as i32;
    let ph = (PEND_H as f32 * sc) as i32;
    if y < py || y >= py + ph {
        return false;
    }
    let w = (W as f32 * sc) as i32;
    let bw = (60.0 * sc) as i32;
    let bh = (22.0 * sc) as i32;
    let bx = w - (PAD as f32 * sc) as i32 - bw;
    let by = py + (ph - bh) / 2;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

// ── Main window painting ──────────────────────────────────────────────

fn paint_main(hwnd: HWND, st: &State, w: i32, h: i32, sc: f32) {
    let mut frame = match crate::renderer::DibFrame::new(w, h) {
        Some(f) => f,
        None => return,
    };
    let pixels = frame.pixels_mut();
    crate::osd::draw_rounded_rect(
        pixels,
        w,
        h,
        (RADIUS as f32 * sc) as i32,
        st.theme.background,
    );
    let mem = frame.dc();
    let font = crate::osd::create_font(-(14.0 * sc) as i32, false, "Segoe UI");
    let _of = unsafe { SelectObject(mem, font) };

    let fg = st.theme.text;
    let accent = st.theme.accent;
    let dim = st.theme.text_muted;

    // ── Header ──
    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z("Power Control"),
        &mut rct(0, (10.0 * sc) as i32, w, (HDR_H as f32 * sc) as i32),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // Close ✕
    let cx = w - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
    let cx2 = cx + (20.0 * sc) as i32;
    tcol(mem, dim);
    dw(
        mem,
        &mut to_utf16_z("✕"),
        &mut rct(
            cx,
            (10.0 * sc) as i32,
            cx2,
            (HDR_H as f32 * sc) as i32 - (10.0 * sc) as i32,
        ),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // ── Awake buttons ──
    let by = (AW_BTN_Y as f32 * sc) as i32;
    let bw = (AW_BTN_W as f32 * sc) as i32;
    let bh = (AW_BTN_H as f32 * sc) as i32;
    let gap = (AW_BTN_GAP as f32 * sc) as i32;
    let sx = (PAD as f32 * sc) as i32;

    for i in 0..AW_COUNT {
        let bx = sx + i as i32 * (bw + gap);
        let (bc, tc) = (dim, dim.contrasting_text_color()); // no highlighting — user request
        fill_rect(mem, bx, by, bx + bw, by + bh, bc);
        tcol(mem, tc);
        dw(
            mem,
            &mut to_utf16_z(AWAKE_NAMES[i]),
            &mut rct(bx, by, bx + bw, by + bh),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Status ──
    let status = match st.awake {
        AwakeMode::Off => "○ System may sleep".to_string(),
        AwakeMode::Forever => "● Awake (forever)".to_string(),
        AwakeMode::Timed { end_sec } => {
            let rem = end_sec.saturating_sub(now_secs());
            if rem > 0 {
                format!("● Awake ({}m left)", rem / 60)
            } else {
                "○ System may sleep".to_string()
            }
        }
    };
    let mut sr = rct(
        (PAD as f32 * sc) as i32,
        (STATUS_Y as f32 * sc) as i32,
        w,
        (STATUS_Y as f32 * sc + STATUS_H as f32 * sc) as i32,
    );
    tcol(mem, accent);
    dw(
        mem,
        &mut to_utf16_z(&status),
        &mut sr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    // ── Separator ──
    let sep_y = (SEP1_Y as f32 * sc) as i32;
    fill_rect(mem, sx, sep_y, w - sx, sep_y + 1, dim);

    // ── Actions label ──
    tcol(mem, dim);
    dw(
        mem,
        &mut to_utf16_z("Actions"),
        &mut rct(
            sx,
            (ACT_LBL_Y as f32 * sc) as i32,
            w,
            (ACT_LBL_Y as f32 * sc + ACT_LBL_H as f32 * sc) as i32,
        ),
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    // ── Action buttons ──
    let aby = (ACT_BTN_Y as f32 * sc) as i32;
    let abw = (ACT_BTN_W as f32 * sc) as i32;
    let abh = (ACT_BTN_H as f32 * sc) as i32;
    let agap = (ACT_BTN_GAP as f32 * sc) as i32;
    let albls = ["Sleep", "Shutdown", "Screen off"];
    for i in 0..3 {
        let bx = sx + i as i32 * (abw + agap);
        fill_rect(mem, bx, aby, bx + abw, aby + abh, dim);
        tcol(mem, dim.contrasting_text_color());
        dw(
            mem,
            &mut to_utf16_z(albls[i]),
            &mut rct(bx, aby, bx + abw, aby + abh),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    // ── Timer rows ──
    let tbw = (TMR_BTN_W as f32 * sc) as i32;
    let tbh = (TMR_BTN_H as f32 * sc) as i32;
    let tgap = (TMR_BTN_GAP as f32 * sc) as i32;
    let tvals = ["5m", "15m", "30m", "1h"];
    let rows = [
        (TMR_ROW1_Y as f32 * sc, "Sleep:"),
        (TMR_ROW2_Y as f32 * sc, "Shutdown:"),
    ];

    for (ry, label) in rows {
        let ry = ry as i32;
        // Label
        tcol(mem, dim);
        dw(
            mem,
            &mut to_utf16_z(label),
            &mut rct(sx, ry, sx + (TMR_LABEL_W as f32 * sc) as i32, ry + tbh),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );

        // Timer buttons
        let tsx = sx + (TMR_LABEL_W as f32 * sc) as i32;
        for bi in 0..4 {
            let bx = tsx + bi as i32 * (tbw + tgap);
            fill_rect(mem, bx, ry, bx + tbw, ry + tbh, dim);
            tcol(mem, dim.contrasting_text_color());
            dw(
                mem,
                &mut to_utf16_z(tvals[bi]),
                &mut rct(bx, ry, bx + tbw, ry + tbh),
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // ── Pending row ──
    if let Some(ref p) = st.pending {
        let py = (PEND_Y as f32 * sc) as i32;
        let ph = (PEND_H as f32 * sc) as i32;
        let rem = p.at_sec.saturating_sub(now_secs());
        let opn = match p.op {
            PowerOp::Sleep => "Sleep",
            PowerOp::Shutdown => "Shutdown",
            PowerOp::TurnOffScreen => "Screen off",
        };
        let txt = if rem > 0 {
            format!("⏹ {} in {}:{:02}", opn, rem / 60, rem % 60)
        } else {
            format!("⏹ {} executing…", opn)
        };

        tcol(mem, accent);
        dw(
            mem,
            &mut to_utf16_z(&txt),
            &mut rct(sx, py, w - sx - (64.0 * sc) as i32, py + ph),
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );

        // Cancel button
        let cbw = (60.0 * sc) as i32;
        let cbh = (22.0 * sc) as i32;
        let cbx = w - (PAD as f32 * sc) as i32 - cbw;
        let cby = py + (ph - cbh) / 2;
        fill_rect(mem, cbx, cby, cbx + cbw, cby + cbh, dim);
        tcol(mem, dim.contrasting_text_color());
        dw(
            mem,
            &mut to_utf16_z("Cancel"),
            &mut rct(cbx, cby, cbx + cbw, cby + cbh),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
    }

    frame.fix_gdi_alpha(st.theme.background);

    // ── Blit ──
    unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        frame.present_layered(hwnd, wr.left, wr.top, 255);
    }

    unsafe {
        let _ = DeleteObject(font);
    }
}

// ── Countdown painting ────────────────────────────────────────────────

fn paint_cd(hwnd: HWND, op: PowerOp, secs: u32, sc: f32) {
    let w = (280.0 * sc) as i32;
    let h = (130.0 * sc) as i32;

    let mut frame = match crate::renderer::DibFrame::new(w, h) {
        Some(f) => f,
        None => return,
    };
    let bg = Argb {
        a: 230,
        r: 30,
        g: 30,
        b: 30,
    };
    crate::osd::draw_rounded_rect(frame.pixels_mut(), w, h, (RADIUS as f32 * sc) as i32, bg);
    let mem = frame.dc();
    let font = crate::osd::create_font(-(14.0 * sc) as i32, false, "Segoe UI");
    let _of = unsafe { SelectObject(mem, font) };

    let fg = Argb {
        a: 255,
        r: 220,
        g: 220,
        b: 220,
    };
    let accent = Argb {
        a: 255,
        r: 255,
        g: 100,
        b: 50,
    };
    let dim = Argb {
        a: 200,
        r: 80,
        g: 80,
        b: 80,
    };

    let opn = match op {
        PowerOp::Sleep => "Sleep",
        PowerOp::Shutdown => "Shutdown",
        PowerOp::TurnOffScreen => "Screen off",
    };

    tcol(mem, fg);
    dw(
        mem,
        &mut to_utf16_z(&format!("{} in", opn)),
        &mut rct(0, (12.0 * sc) as i32, w, (42.0 * sc) as i32),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    let tf = make_font(36, sc);
    let _otf = unsafe { SelectObject(mem, tf) };
    tcol(mem, accent);
    dw(
        mem,
        &mut to_utf16_z(&format!("0:{:02}", secs)),
        &mut rct(0, (42.0 * sc) as i32, w, (76.0 * sc) as i32),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    unsafe {
        let _ = DeleteObject(tf);
        _ = SelectObject(mem, font);
    }

    // Cancel
    let bw = (80.0 * sc) as i32;
    let bh = (28.0 * sc) as i32;
    let bx = (w - bw) / 2;
    let by = (84.0 * sc) as i32;
    fill_rect(mem, bx, by, bx + bw, by + bh, dim);
    tcol(mem, dim.contrasting_text_color());
    dw(
        mem,
        &mut to_utf16_z("Cancel"),
        &mut rct(bx, by, bx + bw, by + bh),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    frame.fix_gdi_alpha(bg);

    unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        frame.present_layered(hwnd, wr.left, wr.top, 255);
    }

    unsafe {
        let _ = DeleteObject(font);
    }
}

// ── Drawing helpers ────────────────────────────────────────────────────

fn make_font(size: i32, sc: f32) -> windows::Win32::Graphics::Gdi::HFONT {
    let h = (size as f32 * sc) as i32;
    crate::osd::create_font(-h, false, "Segoe UI")
}

fn fill_rect(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: Argb) {
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    let br = unsafe { CreateSolidBrush(color.to_colorref()) };
    let r = RECT {
        left: x1,
        top: y1,
        right: x2,
        bottom: y2,
    };
    unsafe {
        let _ = FillRect(dc, &r, br);
        _ = DeleteObject(br);
    }
}

fn tcol(dc: HDC, color: Argb) {
    unsafe {
        let _ = SetTextColor(dc, color.to_colorref());
        _ = SetBkMode(dc, TRANSPARENT);
    }
}

fn dw(dc: HDC, text: &mut Vec<u16>, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe {
        let _ = DrawTextW(dc, text, rc as *mut RECT, fmt);
    }
}

fn rct(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}
