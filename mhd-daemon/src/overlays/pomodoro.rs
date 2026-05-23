//! Pomodoro Timer — hotkey-driven focus timer overlay.
//!
//! Small popup with a task name input, countdown display, and
//! Start / Pause / Stop / Extend controls.
//! Second hotkey press closes the window.
//! On completion: optional beep, flash, and blackbox log.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

// Manual FFI — Beep is in kernel32 (not gated behind debug-dump feature).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn Beep(dwfreq: u32, dwduration: u32) -> BOOL;
}

use crate::app::SendHwnd;
use crate::core::native_theme::Argb;

// ── Constants ─────────────────────────────────────────────────────────

const WIN_W: i32 = 400;
const WIN_H: i32 = 260;
const PAD: i32 = 12;
const HEADER_H: i32 = 34;
const INPUT_H: i32 = 28;
const BTN_H: i32 = 30;
const BTN_W: i32 = 72;
const CLS: &str = "mhd_pomodoro_cls";
const EDIT_ID: usize = 100;
const TIMER_ID: usize = 1;
const WM_APP_START: u32 = WM_APP;
const WM_APP_PAUSE: u32 = WM_APP + 1;
const WM_APP_STOP: u32 = WM_APP + 2;
const WM_APP_EXTEND: u32 = WM_APP + 3;

const DEFAULT_WORK_SECS: u32 = 25 * 60;
const EXTEND_SECS: u32 = 5 * 60;

// ── Static ────────────────────────────────────────────────────────────

static CTRL: Mutex<Option<SendHwnd>> = Mutex::new(None);
static DEBUG_LOG: AtomicBool = AtomicBool::new(false);

pub fn set_debug_logging(enabled: bool) {
    DEBUG_LOG.store(enabled, Ordering::Release);
}

fn plog(msg: impl AsRef<str>) {
    if DEBUG_LOG.load(Ordering::Acquire) {
        println!("[pomodoro] {}", msg.as_ref());
    }
}

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(theme: crate::core::native_theme::NativeTheme, bb: bool) {
    plog(format!("show() theme={}", theme.name));
    let Ok(mut guard) = CTRL.lock() else { plog("show(): CTRL lock poisoned"); return; };
    if let Some(sh) = guard.as_ref() {
        plog("show(): already open, posting WM_CLOSE");
        unsafe { let _ = PostMessageW(sh.0, WM_CLOSE, WPARAM(0), LPARAM(0)); }
        return;
    }
    *guard = Some(SendHwnd(HWND::default()));
    drop(guard);

    std::thread::Builder::new()
        .name("pomodoro".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(theme, bb)));
            if r.is_err() { plog("thread panic caught"); }
        })
        .ok();
}

// ── Phase ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Running { remaining: u32 },
    Paused { remaining: u32 },
    Finished,
}

impl Phase {
    fn remaining(&self) -> u32 {
        match *self {
            Phase::Idle => DEFAULT_WORK_SECS,
            Phase::Running { remaining } | Phase::Paused { remaining } => remaining,
            Phase::Finished => 0,
        }
    }
    fn is_ticking(&self) -> bool {
        matches!(self, Phase::Running { .. })
    }
}

// ── State ─────────────────────────────────────────────────────────────

struct WndState {
    edit_hwnd: HWND,
    edit_brush: HBRUSH,
    theme: crate::core::native_theme::NativeTheme,
    phase: Phase,
    task_name: String,
    bb: bool,
    start_time: u64,
}

// ── Window thread ─────────────────────────────────────────────────────

fn run(theme: crate::core::native_theme::NativeTheme, bb: bool) {
    plog("run(): registering class");
    let cls = to_utf16_z(CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
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
        Err(e) => { plog(format!("CreateWindowEx failed: {e}")); if let Ok(mut g) = CTRL.lock() { *g = None; } return; }
    };

    if theme.background.a < 255 {
        unsafe { let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA); }
    }

    // ── Create EDIT ──────────────────────────────────────────────
    let edit_hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP
                | WINDOW_STYLE((ES_AUTOHSCROLL) as u32),
            PAD, HEADER_H + PAD,
            WIN_W - 2 * PAD, INPUT_H,
            hwnd,
            HMENU(EDIT_ID as _),
            hi,
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            plog(format!("create EDIT failed: {e}"));
            unsafe { let _ = DestroyWindow(hwnd); }
            if let Ok(mut g) = CTRL.lock() { *g = None; }
            return;
        }
    };
    unsafe {
        let _ = SendMessageW(edit_hwnd, WM_SETFONT, WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as _), LPARAM(1));
        let placeholder = to_utf16_z("Task name (optional)");
        let _ = SendMessageW(edit_hwnd, WM_SETTEXT, WPARAM(0), LPARAM(placeholder.as_ptr() as isize));
    }

    // Subclass EDIT for Enter / Escape
    let old_edit_proc = unsafe { SetWindowLongPtrW(edit_hwnd, GWLP_WNDPROC, edit_wndproc as *const () as isize) };
    unsafe { SetWindowLongPtrW(edit_hwnd, GWLP_USERDATA, old_edit_proc); }

    // ── State ────────────────────────────────────────────────────
    let edit_brush = unsafe { CreateSolidBrush(gdi_color(theme.surface, theme.background).to_colorref()) };
    let mut st = WndState {
        edit_hwnd,
        edit_brush,
        theme,
        phase: Phase::Idle,
        task_name: String::new(),
        bb,
        start_time: 0,
    };
    let state_ptr: *mut WndState = &mut st;
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

    // ── Publish HWND ─────────────────────────────────────────────
    plog("publishing hwnd");
    if let Ok(mut g) = CTRL.lock() { *g = Some(SendHwnd(hwnd)); }

    // Centre
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - WIN_W) / 2;
    let y = wa.top + (wa.bottom - wa.top - WIN_H) / 2;
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), x, y, WIN_W, WIN_H, SWP_NOZORDER); }

    // Show + focus
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        steal_focus(hwnd, edit_hwnd);
    }

    // ── Message loop ─────────────────────────────────────────────
    plog("entering message loop");
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    plog("clearing CTRL");
    if let Ok(mut g) = CTRL.lock() { *g = None; }
}

// ── Window proc ───────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        return LRESULT(0);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let st = &mut *ptr;

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
            paint_window(hdc, hwnd, st);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as _);
            let _ = SetBkColor(hdc, gdi_color(st.theme.surface, st.theme.background).to_colorref());
            let _ = SetTextColor(hdc, st.theme.text.to_colorref());
            LRESULT(st.edit_brush.0 as _)
        }

        WM_TIMER if wparam.0 == TIMER_ID => {
            tick(st, hwnd);
            LRESULT(0)
        }

        WM_COMMAND => {
            let code = (wparam.0 >> 16) as u32;
            if code == EN_UPDATE {
                let len = SendMessageW(st.edit_hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as usize;
                if len > 0 {
                    let mut buf = vec![0u16; len + 1];
                    SendMessageW(st.edit_hwnd, WM_GETTEXT, WPARAM(buf.len()), LPARAM(buf.as_mut_ptr() as isize));
                    st.task_name = String::from_utf16_lossy(&buf[..len]).trim().to_string();
                } else {
                    st.task_name.clear();
                }
            }
            LRESULT(0)
        }

        WM_APP_START => {
            start_timer(st, hwnd);
            unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
            LRESULT(0)
        }

        WM_APP_PAUSE => {
            if st.phase.is_ticking() {
                st.phase = Phase::Paused { remaining: st.phase.remaining() };
                let _ = KillTimer(hwnd, TIMER_ID);
                unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
            }
            LRESULT(0)
        }

        WM_APP_STOP => {
            stop_timer(st, hwnd);
            unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
            LRESULT(0)
        }

        WM_APP_EXTEND => {
            extend_timer(st, hwnd);
            unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
            LRESULT(0)
        }

        WM_CLOSE => {
            if st.phase.is_ticking() || matches!(st.phase, Phase::Paused { .. }) {
                log_blackbox(st, "pomodoro_abandon");
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            let x = (lparam.0 as i32) & 0xFFFF;
            let y = ((lparam.0 as i32) >> 16) & 0xFFFF;
            handle_click(st, hwnd, x, y);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── EDIT subclass ─────────────────────────────────────────────────────

unsafe extern "system" fn edit_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let old_proc = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    let old: WNDPROC = Some(std::mem::transmute::<isize, extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(old_proc));

    match msg {
        WM_KEYDOWN => {
            let vk = wparam.0 as u32;
            if vk == VK_RETURN.0 as u32 {
                let ctrl_down = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                if !ctrl_down {
                    // Enter (no Ctrl) → Start
                    if let Ok(parent) = GetParent(hwnd) {
                        let _ = PostMessageW(parent, WM_APP_START, WPARAM(0), LPARAM(0));
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

// ── Timer logic ───────────────────────────────────────────────────────

fn start_timer(st: &mut WndState, hwnd: HWND) {
    match st.phase {
        Phase::Idle | Phase::Finished => {
            let remaining = DEFAULT_WORK_SECS;
            st.phase = Phase::Running { remaining };
            st.start_time = epoch_secs();
            log_blackbox(st, "pomodoro_start");
            unsafe { let _ = SetTimer(hwnd, TIMER_ID, 1000, None); }
        }
        Phase::Paused { remaining } => {
            st.phase = Phase::Running { remaining };
            st.start_time = epoch_secs();
            unsafe { let _ = SetTimer(hwnd, TIMER_ID, 1000, None); }
        }
        Phase::Running { .. } => {
            // Running → pause
            st.phase = Phase::Paused { remaining: st.phase.remaining() };
            unsafe { let _ = KillTimer(hwnd, TIMER_ID); }
        }
    }
}

fn tick(st: &mut WndState, hwnd: HWND) {
    if let Phase::Running { remaining } = st.phase {
        if remaining <= 1 {
            st.phase = Phase::Finished;
            unsafe { let _ = KillTimer(hwnd, TIMER_ID); }
            log_blackbox(st, "pomodoro_end");
            unsafe {
                let _ = FlashWindow(hwnd, TRUE);
            }
            unsafe { let _ = Beep(800, 200); }
        } else {
            st.phase = Phase::Running { remaining: remaining - 1 };
        }
        unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
    }
}

fn stop_timer(st: &mut WndState, hwnd: HWND) {
    if st.phase.is_ticking() || matches!(st.phase, Phase::Paused { .. }) {
        log_blackbox(st, "pomodoro_abandon");
    }
    unsafe { let _ = KillTimer(hwnd, TIMER_ID); }
    st.phase = Phase::Idle;
}

fn extend_timer(st: &mut WndState, hwnd: HWND) {
    match st.phase {
        Phase::Running { remaining } => {
            st.phase = Phase::Running { remaining: remaining + EXTEND_SECS };
        }
        Phase::Paused { remaining } => {
            st.phase = Phase::Paused { remaining: remaining + EXTEND_SECS };
        }
        Phase::Finished => {
            st.phase = Phase::Running { remaining: EXTEND_SECS };
            st.start_time = epoch_secs();
            unsafe { let _ = SetTimer(hwnd, TIMER_ID, 1000, None); }
            log_blackbox(st, "pomodoro_restart");
        }
        Phase::Idle => {
            st.phase = Phase::Running { remaining: DEFAULT_WORK_SECS };
            st.start_time = epoch_secs();
            unsafe { let _ = SetTimer(hwnd, TIMER_ID, 1000, None); }
            log_blackbox(st, "pomodoro_start");
        }
    }
    unsafe { let _ = InvalidateRect(hwnd, None, TRUE); }
}

// ── Click handling ────────────────────────────────────────────────────

fn handle_click(st: &WndState, hwnd: HWND, x: i32, y: i32) {
    let btn_y = WIN_H - PAD - BTN_H;
    if y < btn_y || y > btn_y + BTN_H {
        return;
    }
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

    let msg = match btn_idx {
        0 => WM_APP_START,
        1 => WM_APP_PAUSE,
        2 => WM_APP_STOP,
        3 => WM_APP_EXTEND,
        _ => return,
    };
    unsafe { let _ = PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0)); }
    let _ = st;
}

// ── Painting ──────────────────────────────────────────────────────────

fn paint_window(hdc: HDC, _hwnd: HWND, st: &WndState) {
    let theme = &st.theme;
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
    let remaining = st.phase.remaining();
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
        Phase::Running { .. } => "Running",
        Phase::Paused { .. } => "Paused",
        Phase::Finished => "Time's up! 🎉",
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
    let labels = ["Start", "Pause", "Stop", "+5m"];
    for i in 0..4 {
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
}

// ── Blackbox logging ─────────────────────────────────────────────────

fn log_blackbox(st: &WndState, event: &str) {
    #[cfg(feature = "blackbox")]
    {
        if !st.bb { return; }
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
    let _ = st;
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

fn steal_focus(parent: HWND, child: HWND) {
    unsafe {
        let our_tid = GetWindowThreadProcessId(parent, None);
        let fore_tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        if our_tid != fore_tid {
            let _ = AttachThreadInput(our_tid, fore_tid, TRUE);
            let _ = SetForegroundWindow(parent);
            let _ = AttachThreadInput(our_tid, fore_tid, FALSE);
        }
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(child);
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
