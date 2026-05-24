//! Pomodoro Timer — persistent daemon + temporary overlay.
//!
//! Architecture
//! ────────────
//! • **Daemon** (background thread, lives forever):
//!   - Has a hidden HWND + message loop for `SetTimer(1000)` ticks.
//!   - Owns timer state (`Arc<Mutex<PomodoroState>>`).
//!   - Receives commands (start/pause/stop/extend) from overlay via PostMessage.
//!   - Posts `WM_POM_UPDATE` to the overlay HWND when state changes.
//! • **Overlay** (thread‑per‑invocation, like QuickNote):
//!   - Visible popup window with timer display + task name + buttons.
//!   - Registers with daemon on open, unregisters on close.
//!   - Sends commands to daemon hidden HWND.
//!   - Repaints on `WM_POM_UPDATE`.
//! • Second hotkey press closes the overlay window (daemon keeps running).
//! • On completion: flash + beep, archived in blackbox.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::SendHwnd;
use crate::core::native_theme::Argb;
use crate::win32::text_host::{TextHost, TextHostKind};

// ── Manual FFI ────────────────────────────────────────────────────────

#[link(name = "kernel32")]
unsafe extern "system" {
    fn Beep(dwfreq: u32, dwduration: u32) -> BOOL;
}

// ── Layout ───────────────────────────────────────────────────────────

const WIN_W: i32 = 400;
const WIN_H: i32 = 260;
const PAD: i32 = 12;
const HEADER_H: i32 = 34;
const INPUT_H: i32 = 28;
const BTN_H: i32 = 30;
const BTN_W: i32 = 72;
const DAEMON_CLS: &str = "mhd_pomodoro_daemon_cls";
const OVERLAY_CLS: &str = "mhd_pomodoro_overlay_cls";

const TIMER_ID: usize = 1;

const DEFAULT_WORK_SECS: u32 = 25 * 60;
const EXTEND_SECS: u32 = 5 * 60;

// ── Custom messages ──────────────────────────────────────────────────
// Overlay → Daemon
const WM_POM_START: u32 = WM_APP + 100;
const WM_POM_PAUSE: u32 = WM_APP + 101;
const WM_POM_STOP: u32 = WM_APP + 102;
const WM_POM_EXTEND: u32 = WM_APP + 103;
const WM_POM_SET_TASK: u32 = WM_APP + 104;  // wparam = overlay HWND, lparam = string ptr
const WM_POM_REGISTER_OVERLAY: u32 = WM_APP + 105;  // wparam = overlay HWND
const WM_POM_UNREGISTER_OVERLAY: u32 = WM_APP + 106;
const WM_POM_BREAK: u32 = WM_APP + 107;  // start a break timer

// Daemon → Overlay
const WM_POM_UPDATE: u32 = WM_APP + 200;  // wparam = remaining_secs, lparam = phase as u32

// ── Debug logging ────────────────────────────────────────────────────

static DEBUG_LOG: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn set_debug_logging(enabled: bool) {
    DEBUG_LOG.store(enabled, std::sync::atomic::Ordering::Release);
}

fn plog(msg: impl AsRef<str>) {
    if DEBUG_LOG.load(std::sync::atomic::Ordering::Acquire) {
        println!("[pomodoro] {}", msg.as_ref());
    }
}

// ── Phase ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Phase {
    Idle = 0,
    Running = 1,
    Paused = 2,
    Finished = 3,
}

// ── Mode (work / break) ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PomodoroMode {
    Work = 0,
    Break = 1,
}

// ── Shared state ─────────────────────────────────────────────────────

pub struct PomodoroState {
    pub phase: Phase,
    pub mode: PomodoroMode,
    pub remaining: u32,
    pub task_name: String,
    pub start_time: u64,
    pub overlay_hwnd: Option<SendHwnd>,
    pub last_theme: Option<crate::core::native_theme::NativeTheme>,
    pub bb: bool,
}

// ── Daemon handle ────────────────────────────────────────────────────

struct DaemonHandle {
    hwnd: SendHwnd,
    state: Arc<Mutex<PomodoroState>>,
}

static DAEMON: LazyLock<Mutex<Option<DaemonHandle>>> = LazyLock::new(|| Mutex::new(None));

// ── Public API ───────────────────────────────────────────────────────

/// Show the Pomodoro overlay (creates daemon lazily on first call).
pub fn show(theme: crate::core::native_theme::NativeTheme, bb: bool) {
    plog("show()");

    // Init daemon on first call
    let _daemon_hwnd = {
        let mut guard = DAEMON.lock().unwrap();
        if guard.is_none() {
            plog("initialising daemon");
            *guard = Some(spawn_daemon());
        }
        guard.as_ref().unwrap().hwnd.0
    };
    let state = {
        let guard = DAEMON.lock().unwrap();
        guard.as_ref().unwrap().state.clone()
    };

    // If overlay already open, close it
    {
        let st = state.lock().unwrap();
        if let Some(ref oh) = st.overlay_hwnd {
            plog("overlay already open, closing");
            let _ = unsafe { PostMessageW(oh.0, WM_CLOSE, WPARAM(0), LPARAM(0)) };
            return;
        }
    }

    // Spawn overlay thread
    let state_clone = state.clone();
    std::thread::Builder::new()
        .name("pomodoro-overlay".into())
        .spawn(move || {
            plog("overlay thread start");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_overlay(state_clone, theme, bb)
            }));
            if r.is_err() { plog("overlay thread panic caught"); }
            plog("overlay thread end");
        })
        .ok();
}

fn run_overlay(state: Arc<Mutex<PomodoroState>>, theme: crate::core::native_theme::NativeTheme, bb: bool) {
    let cls = to_utf16_z(OVERLAY_CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hi,
            hbrBackground: HBRUSH(2 as _),
            lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }

    let ex_style = if theme.background.a < 255 {
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED
    } else {
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW
    };

    let hwnd = match unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0, 0, WIN_W, WIN_H,
            None, None, hi, None,
        )
    } {
        Ok(h) => h,
        Err(e) => { plog(format!("CreateWindowEx failed: {e}")); return; }
    };

    if theme.background.a < 255 {
        unsafe { let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA); }
    }

    // ── Create EDIT via TextHost ──────────────────────────────────
    let brush_color = if theme.surface.a == 255 {
        theme.surface
    } else {
        theme.surface.blend_over(theme.background)
    };
    let text_host = TextHost::create(
        TextHostKind::Edit,
        hwnd,
        PAD, HEADER_H + PAD,
        WIN_W - 2 * PAD, INPUT_H,
        ES_AUTOHSCROLL as u32,
        edit_wndproc,
        brush_color,
    ).expect("TextHost::create failed");
    text_host.set_text("Task name (optional)");

    // Save theme + bb in daemon state
    {
        let mut st = state.lock().unwrap();
        st.overlay_hwnd = Some(SendHwnd(hwnd));
        st.last_theme = Some(theme.clone());
        st.bb = bb;
        // If daemon is running, update task name from overlay
        if !st.task_name.is_empty() {
            text_host.set_text(&st.task_name);
        }
    }

    // Centre
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - WIN_W) / 2;
    let y = wa.top + (wa.bottom - wa.top - WIN_H) / 2;
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), x, y, WIN_W, WIN_H, SWP_NOZORDER); }

    // Show + focus (before text_host is moved into state)
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        text_host.focus(hwnd);
    }

    // Store reference to shared state
    let state_box = Box::into_raw(Box::new(OverlayState {
        state: state.clone(),
        text_host,
        theme: theme.clone(),
        _bb: bb,
    }));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_box as isize); }

    // Register with daemon
    let daemon_hwnd = { DAEMON.lock().unwrap().as_ref().map(|d| d.hwnd.0).unwrap_or(HWND::default()) };
    if daemon_hwnd != HWND::default() {
        unsafe { let _ = PostMessageW(daemon_hwnd, WM_POM_REGISTER_OVERLAY, WPARAM(hwnd.0 as _), LPARAM(0)); }
    }

    // Message loop
    plog("overlay message loop start");
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // Unregister from daemon (daemon keeps running)
    if daemon_hwnd != HWND::default() {
        unsafe { let _ = PostMessageW(daemon_hwnd, WM_POM_UNREGISTER_OVERLAY, WPARAM(0), LPARAM(0)); }
    }

    // Clean up overlay_hwnd ref
    {
        let mut st = state.lock().unwrap();
        st.overlay_hwnd = None;
    }

    // Free overlay state
    unsafe {
        let _ = Box::from_raw(state_box);
    }

    plog("overlay message loop end");
}

struct OverlayState {
    state: Arc<Mutex<PomodoroState>>,
    text_host: TextHost,
    theme: crate::core::native_theme::NativeTheme,
    _bb: bool,
}

// ── Overlay window proc ──────────────────────────────────────────────

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
    if msg == WM_CREATE {
        return LRESULT(0);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let os = &mut *ptr;

    match msg {
        WM_NCHITTEST => {
            let x = (lparam.0 as i32) & 0xFFFF;
            let y = ((lparam.0 as i32) >> 16) & 0xFFFF;
            let mut pt = POINT { x, y };
            let _ = ScreenToClient(hwnd, &mut pt);
            if pt.y < HEADER_H {
                return LRESULT(HTCAPTION as _);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_overlay(hdc, hwnd, os);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as _);
            let surface = os.theme.surface.blend_over(os.theme.background);
            let _ = SetBkColor(hdc, surface.to_colorref());
            let text_color = surface.contrasting_text_color();
            let _ = SetTextColor(hdc, text_color.to_colorref());
            LRESULT(os.text_host.brush().0 as _)
        }

        WM_POM_UPDATE => {
            // State changed, repaint
            let _ = InvalidateRect(hwnd, None, TRUE);
            LRESULT(0)
        }

        WM_COMMAND => {
            let code = (wparam.0 >> 16) as u32;
            if code == EN_UPDATE {
                let daemon_hwnd = { DAEMON.lock().unwrap().as_ref().map(|d| d.hwnd.0).unwrap_or(HWND::default()) };
                let len = SendMessageW(os.text_host.hwnd(), WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as usize;
                if len > 0 && daemon_hwnd != HWND::default() {
                    let mut buf = vec![0u16; len + 1];
                    SendMessageW(os.text_host.hwnd(), WM_GETTEXT, WPARAM(buf.len()), LPARAM(buf.as_mut_ptr() as isize));
                    // Send task name to daemon as a heap-allocated string
                    let s = String::from_utf16_lossy(&buf[..len]).trim().to_string();
                    let s_box = Box::into_raw(Box::new(s));
                    let _ = PostMessageW(daemon_hwnd, WM_POM_SET_TASK, WPARAM(s_box as _), LPARAM(0));
                }
            }
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let x = (lparam.0 as i32) & 0xFFFF;
            let y = ((lparam.0 as i32) >> 16) & 0xFFFF;
            handle_overlay_click(os, hwnd, x, y);
            LRESULT(0)
        }

        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    }
}

// ── Overlay click ────────────────────────────────────────────────────

fn handle_overlay_click(os: &OverlayState, _hwnd: HWND, x: i32, y: i32) {
    let daemon_hwnd = { DAEMON.lock().unwrap().as_ref().map(|d| d.hwnd.0).unwrap_or(HWND::default()) };
    if daemon_hwnd == HWND::default() {
        return;
    }

    let btn_y = WIN_H - PAD - BTN_H;
    if y < btn_y || y > btn_y + BTN_H {
        return;
    }

    // Read current phase + mode to map buttons correctly
    let st = os.state.lock().unwrap();
    let is_finished = st.phase == Phase::Finished;
    let is_work = st.mode == PomodoroMode::Work;
    drop(st);

    let btn_total = BTN_W * 4 + PAD * 3;
    let btn_start_x = (WIN_W - btn_total) / 2;

    let btn_idx = if x >= btn_start_x && x < btn_start_x + BTN_W {
        0usize
    } else if x >= btn_start_x + BTN_W + PAD && x < btn_start_x + BTN_W * 2 + PAD {
        1
    } else if x >= btn_start_x + BTN_W * 2 + PAD * 2 && x < btn_start_x + BTN_W * 3 + PAD * 2 {
        2
    } else if x >= btn_start_x + BTN_W * 3 + PAD * 3 && x < btn_start_x + BTN_W * 4 + PAD * 3 {
        3
    } else {
        return;
    };

    let msg = if is_finished {
        match btn_idx {
            0 => WM_POM_EXTEND,          // +5m
            1 if is_work => WM_POM_BREAK, // Break 5m (work finished)
            1 => WM_POM_START,           // Work 25m (break finished)
            _ => return,                  // buttons 2,3 = empty
        }
    } else {
        match btn_idx {
            0 => WM_POM_START,
            1 => WM_POM_PAUSE,
            2 => WM_POM_STOP,
            3 => WM_POM_EXTEND,
            _ => return,
        }
    };
    unsafe { let _ = PostMessageW(daemon_hwnd, msg, WPARAM(0), LPARAM(0)); }
}

// ── Overlay painting ─────────────────────────────────────────────────

fn paint_overlay(hdc: HDC, _hwnd: HWND, os: &OverlayState) {
    let st = os.state.lock().unwrap();
    let theme = &os.theme;

    let bg = gdi_color(theme.background, Argb::new(255, 0, 0, 0));
    let bg_brush = unsafe { CreateSolidBrush(bg.to_colorref()) };
    let rc = RECT { left: 0, top: 0, right: WIN_W, bottom: WIN_H };
    unsafe {
        let _ = FillRect(hdc, &rc, bg_brush);
        let _ = DeleteObject(bg_brush);
    }

    // ── Header ────────────────────────────────────────────────────
    let mut hr = RECT { left: 0, top: 0, right: WIN_W, bottom: HEADER_H };
    unsafe {
        let header_brush = CreateSolidBrush(gdi_color(theme.surface, theme.background).to_colorref());
        let _ = FillRect(hdc, &hr, header_brush);
        let _ = DeleteObject(header_brush);

        let pen = CreatePen(PS_SOLID, 1, theme.border.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let _ = MoveToEx(hdc, 0, HEADER_H - 1, None);
        let _ = LineTo(hdc, WIN_W, HEADER_H - 1);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, theme.text.to_colorref());
        let mut title = to_utf16_z("🍅 Pomodoro");
        let _ = DrawTextW(hdc, &mut title, &mut hr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // ── Timer display ─────────────────────────────────────────────
    let timer_y = HEADER_H + PAD + INPUT_H + PAD;
    let remaining = st.remaining;
    let mins = remaining / 60;
    let secs = remaining % 60;
    let time_str = format!("{:02}:{:02}", mins, secs);

    unsafe {
        let mut time_wz = to_utf16_z(&time_str);
        let large_font = CreateFontW(
            42, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(to_utf16_z("Segoe UI").as_ptr()),
        );
        let old_font = SelectObject(hdc, large_font);
        let mut tr = RECT {
            left: PAD, top: timer_y,
            right: WIN_W - PAD, bottom: timer_y + 56,
        };
        let _ = SetTextColor(hdc, theme.text.to_colorref());
        let _ = DrawTextW(hdc, &mut time_wz, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(large_font);
    }

    // ── Phase label ───────────────────────────────────────────────
    let phase_label = match st.phase {
        Phase::Idle => "Press Start or Enter",
        Phase::Running if st.mode == PomodoroMode::Work => "Running",
        Phase::Running if st.mode == PomodoroMode::Break => "Break 🧘",
        Phase::Paused => "Paused",
        Phase::Finished if st.mode == PomodoroMode::Work => "Time's up! 🎉",
        Phase::Finished if st.mode == PomodoroMode::Break => "Break over! 🎯",
        _ => "",
    };
    unsafe {
        let mut pr = RECT {
            left: PAD, top: timer_y + 56,
            right: WIN_W - PAD, bottom: timer_y + 56 + 24,
        };
        let _ = SetTextColor(hdc, theme.text_muted.to_colorref());
        let mut label_wz = to_utf16_z(phase_label);
        let _ = DrawTextW(hdc, &mut label_wz, &mut pr, DT_CENTER | DT_TOP | DT_SINGLELINE);
    }

    // ── Buttons ───────────────────────────────────────────────────
    let btn_y = WIN_H - PAD - BTN_H;
    let btn_total = BTN_W * 4 + PAD * 3;
    let btn_start_x = (WIN_W - btn_total) / 2;
    let labels = if st.phase == Phase::Finished {
        if st.mode == PomodoroMode::Work {
            ["+5m", "Break 5m", "", ""]
        } else {
            ["+5m", "Work 25m", "", ""]
        }
    } else {
        ["Start", "Pause", "Stop", "+5m"]
    };
    for i in 0..4 {
        if labels[i].is_empty() { continue; }
        let bx = btn_start_x + i as i32 * (BTN_W + PAD);
        let br = RECT { left: bx, top: btn_y, right: bx + BTN_W, bottom: btn_y + BTN_H };
        unsafe {
            let border_pen = CreatePen(PS_SOLID, 1, theme.border.to_colorref());
            let old_pen = SelectObject(hdc, border_pen);
            let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(hdc, br.left, br.top, br.right, br.bottom);
            let _ = SelectObject(hdc, old_pen);
            let _ = SelectObject(hdc, old_brush);
            let _ = DeleteObject(border_pen);
        }
        unsafe {
            let _ = SetTextColor(hdc, theme.text.to_colorref());
            let mut label_wz = to_utf16_z(labels[i]);
            let mut lr = br;
            let _ = DrawTextW(hdc, &mut label_wz, &mut lr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
    }
    drop(st);
}

// ── EDIT subclass ────────────────────────────────────────────────────

unsafe extern "system" fn edit_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
    let old_proc = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old: WNDPROC = Some(std::mem::transmute::<isize, extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(old_proc));

    match msg {
        WM_KEYDOWN => {
            let vk = wparam.0 as u32;
            if vk == VK_RETURN.0 as u32 {
                let ctrl_down = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                if !ctrl_down {
                    if let Ok(_parent) = GetParent(hwnd) {
                        let daemon_hwnd = { DAEMON.lock().unwrap().as_ref().map(|d| d.hwnd.0).unwrap_or(HWND::default()) };
                        if daemon_hwnd != HWND::default() {
                            let _ = PostMessageW(daemon_hwnd, WM_POM_START, WPARAM(0), LPARAM(0));
                        }
                    }
                    return LRESULT(0);
                }
            }
            if vk == VK_ESCAPE.0 as u32 {
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            }
        }
        _ => {}
    }

    CallWindowProcW(old, hwnd, msg, wparam, lparam)
    }
}

// ── Daemon (background) ─────────────────────────────────────────────

fn spawn_daemon() -> DaemonHandle {
    let state = Arc::new(Mutex::new(PomodoroState {
        phase: Phase::Idle,
        mode: PomodoroMode::Work,
        remaining: DEFAULT_WORK_SECS,
        task_name: String::new(),
        start_time: 0,
        overlay_hwnd: None,
        last_theme: None,
        bb: false,
    }));

    let state_clone = state.clone();

    let (tx, rx) = std::sync::mpsc::channel::<SendHwnd>();

    std::thread::Builder::new()
        .name("pomodoro-daemon".into())
        .spawn(move || {
            plog("daemon thread start");
            let cls = to_utf16_z(DAEMON_CLS);
            let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

            unsafe {
                let wc = WNDCLASSW {
                    style: WNDCLASS_STYLES(0),
                    lpfnWndProc: Some(daemon_wndproc),
                    hInstance: hi,
                    lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
                    ..Default::default()
                };
                let _ = RegisterClassW(&wc);
            }

            let hwnd = match unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR::from_raw(cls.as_ptr()),
                    PCWSTR::null(),
                    WS_POPUP,
                    0, 0, 0, 0,
                    None, None, hi, None,
                )
            } {
                Ok(h) => h,
                Err(e) => { plog(format!("daemon CreateWindowEx failed: {e}")); return; }
            };

            // Store shared state in GWLP_USERDATA
            let state_box = Box::into_raw(Box::new(state_clone));
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_box as isize); }

            // Signal that daemon is ready
            let _ = tx.send(SendHwnd(hwnd));

            plog("daemon message loop start");
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }

            // Cleanup
            unsafe {
                let _ = Box::from_raw(state_box);
            }
            plog("daemon message loop end");
        })
        .ok();

    let hwnd = rx.recv().unwrap_or(SendHwnd(HWND::default()));
    DaemonHandle { hwnd, state }
}

unsafe extern "system" fn daemon_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
    if msg == WM_CREATE {
        return LRESULT(0);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Arc<Mutex<PomodoroState>>;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state_arc = &*ptr;
    let mut st = state_arc.lock().unwrap();

    match msg {
        WM_TIMER if wparam.0 == TIMER_ID => {
            if st.phase == Phase::Running {
                if st.remaining <= 1 {
                    st.phase = Phase::Finished;
                    st.remaining = 0;
                    let _ = KillTimer(hwnd, TIMER_ID);
                    log_blackbox_raw(&st, if st.mode == PomodoroMode::Work { "pomodoro_end" } else { "break_end" });
                    // Beep
                    let _ = Beep(800, 200);
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let _ = Beep(1000, 200);
                    // Auto-show overlay + flash if closed
                    if st.overlay_hwnd.is_none() {
                        if let Some(ref theme) = st.last_theme {
                            let state_arc2 = state_arc.clone();
                            let theme2 = theme.clone();
                            let bb2 = st.bb;
                            std::thread::Builder::new()
                                .name("pomodoro-overlay".into())
                                .spawn(move || {
                                    run_overlay(state_arc2, theme2, bb2);
                                })
                                .ok();
                        }
                    } else if let Some(ref oh) = st.overlay_hwnd {
                        let _ = FlashWindow(oh.0, TRUE);
                    }
                } else {
                    st.remaining -= 1;
                }
                // Notify overlay
                if let Some(ref oh) = st.overlay_hwnd {
                    let _ = PostMessageW(oh.0, WM_POM_UPDATE, WPARAM(st.remaining as _), LPARAM(st.phase as u32 as _));
                }
            }
            LRESULT(0)
        }

        WM_POM_START => {
            match st.phase {
                Phase::Idle | Phase::Finished => {
                    st.mode = PomodoroMode::Work;
                    st.phase = Phase::Running;
                    st.remaining = DEFAULT_WORK_SECS;
                    st.start_time = epoch_secs();
                    log_blackbox_raw(&st, "pomodoro_start");
                    let _ = SetTimer(hwnd, TIMER_ID, 1000, None);
                }
                Phase::Paused => {
                    st.phase = Phase::Running;
                    st.start_time = epoch_secs();
                    let _ = SetTimer(hwnd, TIMER_ID, 1000, None);
                }
                Phase::Running => {
                    // Running → pause
                    st.phase = Phase::Paused;
                    let _ = KillTimer(hwnd, TIMER_ID);
                }
            }
            notify_overlay(&st);
            LRESULT(0)
        }

        WM_POM_PAUSE => {
            if st.phase == Phase::Running {
                st.phase = Phase::Paused;
                let _ = KillTimer(hwnd, TIMER_ID);
                notify_overlay(&st);
            }
            LRESULT(0)
        }

        WM_POM_STOP => {
            if st.phase == Phase::Running || st.phase == Phase::Paused {
                log_blackbox_raw(&st, "pomodoro_abandon");
            }
            st.phase = Phase::Idle;
            st.remaining = DEFAULT_WORK_SECS;
            let _ = KillTimer(hwnd, TIMER_ID);
            notify_overlay(&st);
            LRESULT(0)
        }

        WM_POM_EXTEND => {
            match st.phase {
                Phase::Running => {
                    st.remaining = st.remaining.saturating_add(EXTEND_SECS);
                }
                Phase::Paused => {
                    st.remaining = st.remaining.saturating_add(EXTEND_SECS);
                }
                Phase::Finished => {
                    // Keep current mode (Work/Break)
                    st.phase = Phase::Running;
                    st.remaining = EXTEND_SECS;
                    st.start_time = epoch_secs();
                    let _ = SetTimer(hwnd, TIMER_ID, 1000, None);
                    log_blackbox_raw(&st, if st.mode == PomodoroMode::Work { "pomodoro_restart" } else { "break_restart" });
                }
                Phase::Idle => {
                    st.mode = PomodoroMode::Work;
                    st.phase = Phase::Running;
                    st.remaining = DEFAULT_WORK_SECS;
                    st.start_time = epoch_secs();
                    let _ = SetTimer(hwnd, TIMER_ID, 1000, None);
                    log_blackbox_raw(&st, "pomodoro_start");
                }
            }
            notify_overlay(&st);
            LRESULT(0)
        }

        WM_POM_BREAK => {
            st.mode = PomodoroMode::Break;
            st.phase = Phase::Running;
            st.remaining = 5 * 60; // 5 min break
            st.start_time = epoch_secs();
            let _ = SetTimer(hwnd, TIMER_ID, 1000, None);
            log_blackbox_raw(&st, "break_start");
            notify_overlay(&st);
            LRESULT(0)
        }

        WM_POM_SET_TASK => {
            let ptr = wparam.0 as *mut String;
            if !ptr.is_null() {
                let s = Box::from_raw(ptr);
                st.task_name = *s;
                notify_overlay(&st);
            }
            LRESULT(0)
        }

        WM_POM_REGISTER_OVERLAY => {
            let oh = HWND(wparam.0 as _);
            st.overlay_hwnd = Some(SendHwnd(oh));
            // Send current state to new overlay
            let _ = PostMessageW(oh, WM_POM_UPDATE, WPARAM(st.remaining as _), LPARAM(st.phase as u32 as _));
            LRESULT(0)
        }

        WM_POM_UNREGISTER_OVERLAY => {
            st.overlay_hwnd = None;
            LRESULT(0)
        }

        WM_CLOSE => {
            // Daemon hidden window should not be closed normally
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    }
}

fn notify_overlay(st: &PomodoroState) {
    if let Some(ref oh) = st.overlay_hwnd {
        let _ = unsafe { PostMessageW(oh.0, WM_POM_UPDATE, WPARAM(st.remaining as _), LPARAM(st.phase as u32 as _)) };
    }
}

// ── Blackbox logging ─────────────────────────────────────────────────

fn log_blackbox_raw(st: &PomodoroState, event: &str) {
    let _ = st;
    #[cfg(feature = "blackbox")]
    {
        let ts = epoch_secs();
        let duration = if st.start_time > 0 { ts - st.start_time } else { 0 };
        let task = if st.task_name.is_empty() { "_" } else { &st.task_name };
        crate::blackbox::send_event(crate::blackbox::BlackboxEvent::LogCustom {
            ts,
            event: event.to_string(),
            kv: vec![
                ("d".to_string(), duration.to_string()),
                ("t".to_string(), task.to_string()),
            ],
        });
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn gdi_color(color: Argb, background: Argb) -> Argb {
    if color.a == 255 { color } else { color.blend_over(background) }
}

fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn work_area() -> RECT {
    unsafe {
        let desktop = GetDesktopWindow();
        let hmon = MonitorFromWindow(desktop, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        info.rcWork
    }
}


fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
