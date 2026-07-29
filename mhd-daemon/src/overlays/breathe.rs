//! Breathe — paced resonance breathing overlay with expanding/contracting sphere.
//!
//! Architecture
//! ────────────
//! - **Daemon** (background thread, lives forever):
//!   - Hidden HWND + message loop with `SetTimer(50ms)` for smooth animation.
//!   - Owns breathing state (`Arc<Mutex<BreatheState>>`).
//!   - Receives commands (start/pause/stop) from overlay via PostMessage.
//!   - Posts `WM_BREATHE_UPDATE` to overlay HWND each tick.
//! - **Overlay** (thread-per-invocation, like Pomodoro/QuickNote):
//!   - Visible popup window with sphere animation, phase label, progress, buttons.
//!   - Registers with daemon on open, unregisters on close.
//!   - Repaints on `WM_BREATHE_UPDATE`.
//! - Second hotkey press closes the overlay window (daemon keeps running).
//! - On completion: log to blackbox (silent — no audio cues).

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::SendHwnd;
use crate::core::native_theme::{Argb, NativeTheme};

// ── Layout ───────────────────────────────────────────────────────────

const WIN_W: i32 = 460;
const WIN_H: i32 = 380;
const PAD: i32 = 18;
const HEADER_H: i32 = 46;
const CLOSE_SIZE: i32 = 26;
const SPHERE_CX: i32 = WIN_W / 2;
const SPHERE_CY: i32 = 180;
const SPHERE_MIN_R: i32 = 24;
const SPHERE_MAX_R: i32 = 84;
const PHASE_Y: i32 = 52;
const PHASE_H: i32 = 28;
const SESSION_BAR_Y: i32 = 286;
const SESSION_BAR_H: i32 = 6;
const COUNTER_Y: i32 = 300;
const COUNTER_H: i32 = 20;
const BTN_H: i32 = 34;
const BTN_W: i32 = 92;
const BTN_GAP: i32 = 10;
const DAEMON_CLS: &str = "mhd_breathe_daemon_cls";
const OVERLAY_CLS: &str = "mhd_breathe_overlay_cls";

const TIMER_ID: usize = 2;
const TIMER_MS: u32 = 50; // ~20 Hz for smooth sphere animation

// ── Custom messages ──────────────────────────────────────────────────
// Overlay → Daemon
const WM_BR_START: u32 = WM_APP + 100;
const WM_BR_PAUSE: u32 = WM_APP + 101;
const WM_BR_STOP: u32 = WM_APP + 102;
const WM_BR_REGISTER: u32 = WM_APP + 103;
const WM_BR_UNREGISTER: u32 = WM_APP + 104;
const WM_BR_SELECT_PRESET: u32 = WM_APP + 105;

// Daemon → Overlay
const WM_BR_UPDATE: u32 = WM_APP + 200;

// ── Debug logging ────────────────────────────────────────────────────

static DEBUG_LOG: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn set_debug_logging(enabled: bool) {
    DEBUG_LOG.store(enabled, std::sync::atomic::Ordering::Release);
}

fn blog(msg: impl AsRef<str>) {
    if DEBUG_LOG.load(std::sync::atomic::Ordering::Acquire) {
        println!("[breathe] {}", msg.as_ref());
    }
}

// ── Phase ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Phase {
    Idle = 0,
    Inhale = 1,
    Exhale = 2,
    Paused = 3,
    Complete = 4,
}

// ── Breathing configuration ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BreatheConfig {
    pub duration_s: u32,
    pub inhale_s: u32,
    pub exhale_s: u32,
    pub preset_name: String,
}

impl BreatheConfig {
    pub fn ratio_str(&self) -> String {
        format!("{}-{}", self.inhale_s, self.exhale_s)
    }

    pub fn cycle_s(&self) -> u32 {
        self.inhale_s + self.exhale_s
    }

    pub fn total_breaths(&self) -> u32 {
        self.duration_s / self.cycle_s()
    }
}

/// Resolve a preset name to breathing parameters.
/// Returns None for unknown preset names.
pub fn preset_config(name: &str) -> Option<BreatheConfig> {
    let (duration_min, inhale, exhale) = match name {
        "balanced" => (10u32, 5, 5),
        "calm" => (15, 4, 6),
        "extended" => (20, 4, 6),
        _ => return None,
    };
    let cycle = inhale + exhale;
    let duration_s = (duration_min * 60).div_ceil(cycle) * cycle; // round up
    Some(BreatheConfig {
        duration_s,
        inhale_s: inhale,
        exhale_s: exhale,
        preset_name: name.to_string(),
    })
}

/// Auto-select preset by time of day (matching breathe-cli logic).
pub fn auto_preset() -> BreatheConfig {
    let hour = current_local_hour();
    let name = if hour < 12 {
        "balanced"
    } else if hour < 17 {
        "extended"
    } else {
        "calm"
    };
    preset_config(name).unwrap_or_else(|| preset_config("balanced").unwrap())
}

// ── Shared state ─────────────────────────────────────────────────────

pub struct BreatheState {
    pub phase: Phase,
    pub config: BreatheConfig,
    pub phase_start: Option<Instant>,
    pub breaths: u32,
    pub overlay_hwnd: Option<SendHwnd>,
    pub last_theme: Option<NativeTheme>,
    pub bb: bool,
    pub start_time: u64,
    /// Preset selected in the Idle screen (highlighted). Clicking the same
    /// preset again starts the session. Empty = nothing selected.
    pub selected_preset: String,
}

// ── Daemon handle ────────────────────────────────────────────────────

struct DaemonHandle {
    hwnd: SendHwnd,
    state: Arc<Mutex<BreatheState>>,
}

static DAEMON: LazyLock<Mutex<Option<DaemonHandle>>> = LazyLock::new(|| Mutex::new(None));

// ── Public API ───────────────────────────────────────────────────────

/// Show the Breathe overlay with the given config.
/// Creates daemon lazily on first call.
pub fn show(theme: NativeTheme, config: BreatheConfig, bb: bool) {
    blog("show()");

    // Init daemon on first call
    let _daemon_hwnd = {
        let mut guard = DAEMON.lock().unwrap();
        if guard.is_none() {
            blog("initialising daemon");
            *guard = Some(spawn_daemon());
        }
        guard.as_ref().unwrap().hwnd.0
    };
    let state = {
        let guard = DAEMON.lock().unwrap();
        guard.as_ref().unwrap().state.clone()
    };

    // If overlay already open, close it (toggle behaviour)
    {
        let st = state.lock().unwrap();
        if let Some(ref oh) = st.overlay_hwnd {
            blog("overlay already open, closing");
            let _ = unsafe { PostMessageW(oh.0, WM_CLOSE, WPARAM(0), LPARAM(0)) };
            return;
        }
    }

    // Set config in state before showing overlay
    {
        let mut st = state.lock().unwrap();
        st.config = config;
        st.phase = Phase::Idle;
        st.phase_start = None;
        st.breaths = 0;
        st.last_theme = Some(theme.clone());
        st.bb = bb;
        st.selected_preset.clear();
    }

    // Spawn overlay thread
    let state_clone = state.clone();
    std::thread::Builder::new()
        .name("breathe-overlay".into())
        .spawn(move || {
            blog("overlay thread start");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_overlay(state_clone, theme, bb)
            }));
            if r.is_err() {
                blog("overlay thread panic caught");
            }
            blog("overlay thread end");
        })
        .ok();
}

// ── Overlay window ───────────────────────────────────────────────────

fn run_overlay(state: Arc<Mutex<BreatheState>>, theme: NativeTheme, bb: bool) {
    let cls = to_utf16_z(OVERLAY_CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: hi,
            hbrBackground: HBRUSH(2 as _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
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
            0,
            0,
            WIN_W,
            WIN_H,
            None,
            None,
            hi,
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            blog(format!("CreateWindowEx failed: {e}"));
            return;
        }
    };

    if theme.background.a < 255 {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA);
        }
    }

    // Save theme + bb in daemon state
    {
        let mut st = state.lock().unwrap();
        st.overlay_hwnd = Some(SendHwnd(hwnd));
        st.last_theme = Some(theme.clone());
        st.bb = bb;
    }

    // Centre
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - WIN_W) / 2;
    let y = wa.top + (wa.bottom - wa.top - WIN_H) / 2;
    unsafe {
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, WIN_W, WIN_H, SWP_NOZORDER);
    }

    // Show
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }

    // Store reference to shared state
    let state_box = Box::into_raw(Box::new(OverlayState {
        state: state.clone(),
        theme: theme.clone(),
        _bb: bb,
    }));
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_box as isize);
    }

    // Register with daemon
    let daemon_hwnd = {
        DAEMON
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.hwnd.0)
            .unwrap_or_default()
    };
    if daemon_hwnd != HWND::default() {
        unsafe {
            let _ = PostMessageW(daemon_hwnd, WM_BR_REGISTER, WPARAM(hwnd.0 as _), LPARAM(0));
        }
    }

    // Don't auto-start: the user picks a preset first, then clicks again to start.

    // Message loop
    blog("overlay message loop start");
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // Unregister from daemon
    if daemon_hwnd != HWND::default() {
        unsafe {
            let _ = PostMessageW(daemon_hwnd, WM_BR_UNREGISTER, WPARAM(0), LPARAM(0));
        }
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

    blog("overlay message loop end");
}

struct OverlayState {
    state: Arc<Mutex<BreatheState>>,
    theme: NativeTheme,
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
                if point_in_rect(pt.x, pt.y, close_rect()) {
                    return LRESULT(HTCLIENT as _);
                }
                if pt.y < HEADER_H {
                    return LRESULT(HTCAPTION as _);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }

            WM_SETCURSOR => {
                let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
                let _ = SetCursor(cursor);
                LRESULT(1)
            }

            WM_KEYDOWN => {
                if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
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

            WM_BR_UPDATE => {
                let _ = InvalidateRect(hwnd, None, FALSE);
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                let x = (lparam.0 as i32) & 0xFFFF;
                let y = ((lparam.0 as i32) >> 16) & 0xFFFF;
                if point_in_rect(x, y, close_rect()) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
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
    let daemon_hwnd = {
        DAEMON
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.hwnd.0)
            .unwrap_or_default()
    };
    if daemon_hwnd == HWND::default() {
        return;
    }

    let btn_y = button_y();
    if y < btn_y || y > btn_y + BTN_H {
        return;
    }

    let st = os.state.lock().unwrap();
    let phase = st.phase;
    drop(st);

    let btn_total = BTN_W * 3 + BTN_GAP * 2;
    let btn_start_x = (WIN_W - btn_total) / 2;

    let btn_idx = if x >= btn_start_x && x < btn_start_x + BTN_W {
        0usize
    } else if x >= btn_start_x + BTN_W + BTN_GAP && x < btn_start_x + BTN_W * 2 + BTN_GAP {
        1
    } else if x >= btn_start_x + BTN_W * 2 + BTN_GAP * 2
        && x < btn_start_x + BTN_W * 3 + BTN_GAP * 2
    {
        2
    } else {
        return;
    };

    let (msg, wparam_val) = match phase {
        Phase::Complete => match btn_idx {
            2 => (WM_CLOSE, 0),
            _ => return,
        },
        Phase::Inhale | Phase::Exhale => match btn_idx {
            0 => (WM_BR_PAUSE, 0),
            2 => (WM_BR_STOP, 0),
            _ => return,
        },
        Phase::Paused => match btn_idx {
            0 => (WM_BR_START, 0), // Resume
            2 => (WM_BR_STOP, 0),
            _ => return,
        },
        Phase::Idle => (WM_BR_SELECT_PRESET, btn_idx),
    };

    unsafe {
        if msg == WM_CLOSE {
            let _ = PostMessageW(_hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        } else {
            let _ = PostMessageW(daemon_hwnd, msg, WPARAM(wparam_val), LPARAM(0));
            if msg == WM_BR_STOP {
                let _ = PostMessageW(_hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

// ── Overlay painting ─────────────────────────────────────────────────

fn paint_overlay(hdc: HDC, _hwnd: HWND, os: &OverlayState) {
    let st = os.state.lock().unwrap();
    let theme = &os.theme;

    let bg = gdi_color(theme.background, Argb::new(255, 0, 0, 0));
    fill_rect(
        hdc,
        RECT {
            left: 0,
            top: 0,
            right: WIN_W,
            bottom: WIN_H,
        },
        bg,
    );

    let surface = gdi_color(theme.surface, theme.background);
    let accent = theme.accent;
    let muted = theme.text_muted;

    // ── Header: preset · ratio · mm:ss + status + close ──────────
    draw_header(hdc, &st, theme, surface, muted);

    // ── Bottom separator ──────────────────────────────────────────
    draw_bottom_line(hdc, button_y() - 10, gdi_color(theme.border, bg));

    // ── Phase label ──────────────────────────────────────────────
    let (phase_text, phase_color) = phase_label(&st, accent, muted);
    draw_label(
        hdc,
        &phase_text,
        RECT {
            left: 0,
            top: PHASE_Y,
            right: WIN_W,
            bottom: PHASE_Y + PHASE_H,
        },
        phase_color,
        20,
        true,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // ── Sphere ────────────────────────────────────────────────────
    let progress = current_progress(&st);
    let radius = sphere_radius(st.phase, progress);
    let sphere_color = sphere_color(st.phase, progress, accent);
    draw_sphere(hdc, SPHERE_CX, SPHERE_CY, radius, sphere_color, theme);

    // ── Session progress bar ──────────────────────────────────────
    let total = st.config.duration_s.max(1);
    let elapsed = elapsed_secs(&st).min(total);
    let progress_w = WIN_W - PAD * 2;
    let filled_w = ((progress_w as u64 * elapsed as u64) / total as u64) as i32;
    let track = RECT {
        left: PAD,
        top: SESSION_BAR_Y,
        right: WIN_W - PAD,
        bottom: SESSION_BAR_Y + SESSION_BAR_H,
    };
    fill_round_rect(hdc, track, gdi_color(theme.border, bg), 4);
    if filled_w > 0 {
        fill_round_rect(
            hdc,
            RECT {
                left: PAD,
                top: SESSION_BAR_Y,
                right: PAD + filled_w,
                bottom: SESSION_BAR_Y + SESSION_BAR_H,
            },
            accent,
            4,
        );
    }

    // ── Breath counter ────────────────────────────────────────────
    let counter = format!("{} / {} breaths", st.breaths, st.config.total_breaths());
    draw_label(
        hdc,
        &counter,
        RECT {
            left: 0,
            top: COUNTER_Y,
            right: WIN_W,
            bottom: COUNTER_Y + COUNTER_H,
        },
        muted,
        12,
        false,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    // ── Buttons ───────────────────────────────────────────────────
    let labels = button_labels(&st);
    let btn_y = button_y();
    let btn_total = BTN_W * 3 + BTN_GAP * 2;
    let btn_start_x = (WIN_W - btn_total) / 2;
    for (i, label) in labels.iter().enumerate() {
        if label.is_empty() {
            continue;
        }
        let bx = btn_start_x + i as i32 * (BTN_W + BTN_GAP);
        let br = RECT {
            left: bx,
            top: btn_y,
            right: bx + BTN_W,
            bottom: btn_y + BTN_H,
        };
        let primary = if st.phase == Phase::Idle {
            // Highlight the selected preset button in Idle mode.
            let name = match i {
                0 => "balanced",
                1 => "calm",
                2 => "extended",
                _ => "",
            };
            st.selected_preset == name
        } else {
            i == 0 && (st.phase == Phase::Paused || st.phase == Phase::Complete)
        };
        draw_button(hdc, br, label, theme, primary);
    }

    // Close button
    draw_close_button(hdc, close_rect(), theme);
}

fn draw_header(hdc: HDC, st: &BreatheState, _theme: &NativeTheme, _surface: Argb, muted: Argb) {
    let status = status_indicator(st);
    let header = if st.phase == Phase::Idle {
        // Don't show a preset name before the user has chosen one.
        format!("Breathe   {}", status)
    } else {
        let remaining = remaining_secs(st);
        format!(
            "Breathe · {} · {} · {}   {}",
            st.config.preset_name,
            st.config.ratio_str(),
            format_mmss(remaining),
            status,
        )
    };
    draw_label(
        hdc,
        &header,
        RECT {
            left: PAD,
            top: 0,
            right: WIN_W - CLOSE_SIZE - PAD,
            bottom: HEADER_H,
        },
        muted,
        13,
        false,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

// ── Daemon (background) ─────────────────────────────────────────────

fn spawn_daemon() -> DaemonHandle {
    let state = Arc::new(Mutex::new(BreatheState {
        phase: Phase::Idle,
        config: preset_config("balanced").unwrap(),
        phase_start: None,
        breaths: 0,
        overlay_hwnd: None,
        last_theme: None,
        bb: false,
        start_time: 0,
        selected_preset: String::new(),
    }));

    let state_clone = state.clone();
    let (tx, rx) = std::sync::mpsc::channel::<SendHwnd>();

    std::thread::Builder::new()
        .name("breathe-daemon".into())
        .spawn(move || {
            blog("daemon thread start");
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
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    hi,
                    None,
                )
            } {
                Ok(h) => h,
                Err(e) => {
                    blog(format!("daemon CreateWindowEx failed: {e}"));
                    return;
                }
            };

            let state_box = Box::into_raw(Box::new(state_clone));
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_box as isize);
            }

            let _ = tx.send(SendHwnd(hwnd));

            blog("daemon message loop start");
            let mut msg = MSG::default();
            while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }

            unsafe {
                let _ = Box::from_raw(state_box);
            }
            blog("daemon message loop end");
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

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Arc<Mutex<BreatheState>>;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state_arc = &*ptr;
        let mut st = state_arc.lock().unwrap();

        match msg {
            WM_TIMER if wparam.0 == TIMER_ID => {
                tick(&mut st, hwnd);
                notify_overlay(&st);
                LRESULT(0)
            }

            WM_BR_START => {
                match st.phase {
                    Phase::Idle => {
                        st.phase = Phase::Inhale;
                        st.phase_start = Some(Instant::now());
                        st.breaths = 0;
                        st.start_time = epoch_secs();
                        log_blackbox_raw(&st, "breathe_start");
                        let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                    }
                    Phase::Paused => {
                        // Resume: snap back to beginning of Inhale
                        st.phase = Phase::Inhale;
                        st.phase_start = Some(Instant::now());
                        let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                    }
                    Phase::Inhale | Phase::Exhale => {
                        // Already running — treat as pause
                        st.phase = Phase::Paused;
                        st.phase_start = None;
                        let _ = KillTimer(hwnd, TIMER_ID);
                    }
                    Phase::Complete => {
                        // Restart with same config
                        st.phase = Phase::Inhale;
                        st.phase_start = Some(Instant::now());
                        st.breaths = 0;
                        st.start_time = epoch_secs();
                        log_blackbox_raw(&st, "breathe_restart");
                        let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                    }
                }
                notify_overlay(&st);
                LRESULT(0)
            }

            WM_BR_PAUSE => {
                if st.phase == Phase::Inhale || st.phase == Phase::Exhale {
                    st.phase = Phase::Paused;
                    st.phase_start = None;
                    let _ = KillTimer(hwnd, TIMER_ID);
                    notify_overlay(&st);
                }
                LRESULT(0)
            }

            WM_BR_STOP => {
                if st.phase != Phase::Idle {
                    log_blackbox_raw(&st, "breathe_abandon");
                }
                st.phase = Phase::Idle;
                st.phase_start = None;
                st.breaths = 0;
                st.selected_preset.clear();
                let _ = KillTimer(hwnd, TIMER_ID);
                notify_overlay(&st);
                LRESULT(0)
            }

            WM_BR_SELECT_PRESET => {
                // Only meaningful while Idle — select or start the preset.
                if st.phase == Phase::Idle {
                    let preset_idx = wparam.0;
                    let name = match preset_idx {
                        0 => "balanced",
                        1 => "calm",
                        2 => "extended",
                        _ => return LRESULT(0),
                    };
                    if st.selected_preset == name && !st.selected_preset.is_empty() {
                        // Same preset clicked again — start the session.
                        if let Some(cfg) = preset_config(name) {
                            st.config = cfg;
                            st.phase = Phase::Inhale;
                            st.phase_start = Some(Instant::now());
                            st.breaths = 0;
                            st.start_time = epoch_secs();
                            log_blackbox_raw(&st, "breathe_start");
                            let _ = SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
                        }
                    } else {
                        // First click — just select/highlight this preset.
                        st.selected_preset = name.to_string();
                    }
                    notify_overlay(&st);
                }
                LRESULT(0)
            }

            WM_BR_REGISTER => {
                let oh = HWND(wparam.0 as _);
                st.overlay_hwnd = Some(SendHwnd(oh));
                notify_overlay(&st);
                LRESULT(0)
            }

            WM_BR_UNREGISTER => {
                st.overlay_hwnd = None;
                LRESULT(0)
            }

            WM_CLOSE => LRESULT(0),

            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Advance the breathing state by one timer tick.
fn tick(st: &mut BreatheState, hwnd: HWND) {
    if st.phase != Phase::Inhale && st.phase != Phase::Exhale {
        return;
    }

    let phase_start = match st.phase_start {
        Some(t) => t,
        None => return,
    };

    let phase_dur = if st.phase == Phase::Inhale {
        st.config.inhale_s
    } else {
        st.config.exhale_s
    } as f32;
    let elapsed = phase_start.elapsed().as_secs_f32();
    let progress = elapsed / phase_dur;

    if progress >= 1.0 {
        if st.phase == Phase::Inhale {
            st.phase = Phase::Exhale;
            st.phase_start = Some(Instant::now());
        } else {
            // Exhale complete — full cycle done
            st.breaths += 1;
            let breathing_base = st.breaths * st.config.cycle_s();
            if breathing_base >= st.config.duration_s {
                st.phase = Phase::Complete;
                st.phase_start = None;
                let _ = unsafe { KillTimer(hwnd, TIMER_ID) };
                log_blackbox_raw(st, "breathe_complete");
            } else {
                st.phase = Phase::Inhale;
                st.phase_start = Some(Instant::now());
            }
        }
    }
}

fn notify_overlay(st: &BreatheState) {
    if let Some(ref oh) = st.overlay_hwnd {
        let _ =
            unsafe { PostMessageW(oh.0, WM_BR_UPDATE, WPARAM(0), LPARAM(st.phase as u32 as _)) };
    }
}

// ── Blackbox logging ─────────────────────────────────────────────────

fn log_blackbox_raw(st: &BreatheState, event: &str) {
    let _ = st;
    #[cfg(not(feature = "blackbox"))]
    let _ = event;
    #[cfg(feature = "blackbox")]
    {
        let ts = epoch_secs();
        let duration = if st.start_time > 0 {
            ts - st.start_time
        } else {
            0
        };
        crate::blackbox::send_event(crate::blackbox::BlackboxEvent::LogCustom {
            ts,
            event: event.to_string(),
            kv: vec![
                ("d".to_string(), duration.to_string()),
                ("preset".to_string(), st.config.preset_name.clone()),
                ("ratio".to_string(), st.config.ratio_str()),
                ("breaths".to_string(), st.breaths.to_string()),
            ],
        });
    }
}

// ── Sphere drawing ──────────────────────────────────────────────────

fn sphere_radius(phase: Phase, progress: f32) -> i32 {
    let p = ease_breath(progress.clamp(0.0, 1.0));
    match phase {
        Phase::Inhale => SPHERE_MIN_R + ((SPHERE_MAX_R - SPHERE_MIN_R) as f32 * p) as i32,
        Phase::Exhale => SPHERE_MAX_R - ((SPHERE_MAX_R - SPHERE_MIN_R) as f32 * p) as i32,
        Phase::Paused => SPHERE_MIN_R,
        Phase::Complete => SPHERE_MIN_R + 8,
        Phase::Idle => SPHERE_MIN_R,
    }
}

/// Exhale colour (darker tint of accent).
fn exhale_color(accent: Argb) -> Argb {
    Argb::new(
        accent.a,
        (accent.r as u32 * 7 / 10) as u8,
        (accent.g as u32 * 7 / 10) as u8,
        (accent.b as u32 * 7 / 10) as u8,
    )
}

/// Idle/paused/complete colour.
fn dim_color(accent: Argb) -> Argb {
    Argb::new(accent.a, 128, 128, 128)
}

fn sphere_color(phase: Phase, progress: f32, accent: Argb) -> Argb {
    let p = progress.clamp(0.0, 1.0);
    match phase {
        // Inhale: interpolate from exhale-colour (dim) at start to accent (bright) at end
        Phase::Inhale => lerp_argb(exhale_color(accent), accent, p),
        // Exhale: interpolate from accent (bright) at start to exhale-colour (dim) at end
        Phase::Exhale => lerp_argb(accent, exhale_color(accent), p),
        Phase::Paused => dim_color(accent),
        Phase::Complete => accent,
        Phase::Idle => dim_color(accent),
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp_argb(a: Argb, b: Argb, t: f32) -> Argb {
    Argb::new(
        a.a,
        lerp(a.r as f32, b.r as f32, t).round().clamp(0.0, 255.0) as u8,
        lerp(a.g as f32, b.g as f32, t).round().clamp(0.0, 255.0) as u8,
        lerp(a.b as f32, b.b as f32, t).round().clamp(0.0, 255.0) as u8,
    )
}

fn draw_sphere(hdc: HDC, cx: i32, cy: i32, r: i32, color: Argb, theme: &NativeTheme) {
    unsafe {
        // Subtle outer glow ring
        let glow_color = gdi_color(theme.border, theme.background);
        let glow_pen = CreatePen(PS_SOLID, 1, glow_color.to_colorref());
        let glow_brush = CreateSolidBrush(glow_color.to_colorref());
        let old_glow_pen = SelectObject(hdc, glow_pen);
        let old_glow_brush = SelectObject(hdc, glow_brush);
        let _ = Ellipse(hdc, cx - r - 4, cy - r - 4, cx + r + 4, cy + r + 4);
        let _ = SelectObject(hdc, old_glow_pen);
        let _ = SelectObject(hdc, old_glow_brush);
        let _ = DeleteObject(glow_pen);
        let _ = DeleteObject(glow_brush);

        // Main sphere
        let brush = CreateSolidBrush(color.to_colorref());
        let pen = CreatePen(PS_SOLID, 1, color.to_colorref());
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, pen);
        let _ = Ellipse(hdc, cx - r, cy - r, cx + r, cy + r);
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn ease_breath(t: f32) -> f32 {
    // Cosine-based easing: derivative is non-zero at endpoints,
    // so the sphere keeps moving at inhale/exhale transitions (no "freeze").
    0.5 - 0.5 * (std::f32::consts::PI * t).cos()
}

fn current_progress(st: &BreatheState) -> f32 {
    if st.phase != Phase::Inhale && st.phase != Phase::Exhale {
        return 0.0;
    }
    let phase_start = match st.phase_start {
        Some(t) => t,
        None => return 0.0,
    };
    let phase_dur = if st.phase == Phase::Inhale {
        st.config.inhale_s
    } else {
        st.config.exhale_s
    } as f32;
    if phase_dur <= 0.0 {
        return 0.0;
    }
    (phase_start.elapsed().as_secs_f32() / phase_dur).clamp(0.0, 1.0)
}

fn elapsed_secs(st: &BreatheState) -> u32 {
    let base = st.breaths * st.config.cycle_s();
    if st.phase == Phase::Inhale {
        base + st
            .phase_start
            .map(|t| t.elapsed().as_secs() as u32)
            .unwrap_or(0)
    } else if st.phase == Phase::Exhale {
        base + st.config.inhale_s
            + st.phase_start
                .map(|t| t.elapsed().as_secs() as u32)
                .unwrap_or(0)
    } else {
        base
    }
}

fn remaining_secs(st: &BreatheState) -> u32 {
    st.config.duration_s.saturating_sub(elapsed_secs(st))
}

fn format_mmss(seconds: u32) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn status_indicator(st: &BreatheState) -> &'static str {
    match st.phase {
        Phase::Inhale | Phase::Exhale => "●",
        Phase::Paused => "‖",
        Phase::Complete => "✓",
        Phase::Idle => "○",
    }
}

fn phase_label(st: &BreatheState, accent: Argb, muted: Argb) -> (String, Argb) {
    match st.phase {
        Phase::Inhale => ("INHALE".to_string(), accent),
        Phase::Exhale => ("EXHALE".to_string(), accent),
        Phase::Paused => ("PAUSED".to_string(), muted),
        Phase::Complete => ("Complete".to_string(), accent),
        Phase::Idle => {
            if st.selected_preset.is_empty() {
                ("Choose a preset".to_string(), muted)
            } else {
                ("Click again to start".to_string(), accent)
            }
        }
    }
}

fn button_labels(st: &BreatheState) -> [&'static str; 3] {
    match st.phase {
        Phase::Inhale | Phase::Exhale => ["Pause", "", "Quit"],
        Phase::Paused => ["Resume", "", "Quit"],
        Phase::Complete => ["", "", "Close"],
        Phase::Idle => ["Balanced", "Calm", "Extended"],
    }
}

fn button_y() -> i32 {
    WIN_H - PAD - BTN_H
}

fn close_rect() -> RECT {
    let top = (HEADER_H - CLOSE_SIZE) / 2;
    RECT {
        left: WIN_W - PAD - CLOSE_SIZE,
        top,
        right: WIN_W - PAD,
        bottom: top + CLOSE_SIZE,
    }
}

fn point_in_rect(x: i32, y: i32, rc: RECT) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

fn gdi_color(color: Argb, background: Argb) -> Argb {
    if color.a == 255 {
        color
    } else {
        color.blend_over(background)
    }
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

fn fill_rect(hdc: HDC, rc: RECT, color: Argb) {
    unsafe {
        let brush = CreateSolidBrush(color.to_colorref());
        let _ = FillRect(hdc, &rc, brush);
        let _ = DeleteObject(brush);
    }
}

fn fill_round_rect(hdc: HDC, rc: RECT, color: Argb, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(color.to_colorref());
        let pen = CreatePen(PS_SOLID, 1, color.to_colorref());
        let old_brush = SelectObject(hdc, brush);
        let old_pen = SelectObject(hdc, pen);
        // RoundRect uses inclusive right/bottom (unlike FillRect/DrawTextW
        // which are exclusive). Add 1 so the rounded rect matches the exact
        // pixel area of the passed RECT, keeping text centred correctly.
        let _ = RoundRect(
            hdc,
            rc.left,
            rc.top,
            rc.right + 1,
            rc.bottom + 1,
            radius,
            radius,
        );
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(brush);
        let _ = DeleteObject(pen);
    }
}

fn draw_bottom_line(hdc: HDC, y: i32, color: Argb) {
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, color.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let _ = MoveToEx(hdc, 0, y, None);
        let _ = LineTo(hdc, WIN_W, y);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);
    }
}

fn draw_label(
    hdc: HDC,
    text: &str,
    mut rc: RECT,
    color: Argb,
    size: i32,
    semibold: bool,
    fmt: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let face = to_utf16_z("Segoe UI");
        let font = CreateFontW(
            size,
            0,
            0,
            0,
            if semibold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(face.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, color.to_colorref());
        let mut wz = to_utf16_z(text);
        let _ = DrawTextW(hdc, &mut wz, &mut rc, fmt);
        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn draw_button(hdc: HDC, rc: RECT, label: &str, theme: &NativeTheme, primary: bool) {
    let fill = if primary {
        theme.accent
    } else {
        gdi_color(theme.surface, theme.background)
    };
    let border = if primary { theme.accent } else { theme.border };
    let text = if primary {
        theme.accent.contrasting_text_color()
    } else {
        theme.text
    };

    fill_round_rect(hdc, rc, fill, 8);
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, border.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, 8);
        let _ = SelectObject(hdc, old_pen);
        let _ = SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
    draw_label(
        hdc,
        label,
        rc,
        text,
        13,
        true,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

fn draw_close_button(hdc: HDC, rc: RECT, theme: &NativeTheme) {
    fill_round_rect(hdc, rc, gdi_color(theme.hover, theme.background), 8);
    draw_label(
        hdc,
        "X",
        rc,
        theme.text_muted,
        13,
        true,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

// ── chrono-free local time ──────────────────────────────────────────

/// Get current local hour (0-23) via Win32 GetLocalTime (no chrono).
fn current_local_hour() -> u32 {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    unsafe {
        let st = GetLocalTime();
        st.wHour as u32
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_presets_all_6_bpm() {
        for name in ["balanced", "calm", "extended"] {
            let c = preset_config(name).expect("preset exists");
            let cycle = c.cycle_s();
            let bpm = 60.0 / cycle as f32;
            assert_eq!(bpm, 6.0, "{} preset is {:.1} bpm, expected 6.0", name, bpm);
        }
    }

    #[test]
    fn test_presets_cycle_ge_8() {
        for name in ["balanced", "calm", "extended"] {
            let c = preset_config(name).unwrap();
            assert!(c.cycle_s() >= 8, "{} cycle too short", name);
        }
    }

    #[test]
    fn test_presets_duration_divides_evenly() {
        for name in ["balanced", "calm", "extended"] {
            let c = preset_config(name).unwrap();
            assert_eq!(
                c.duration_s % c.cycle_s(),
                0,
                "{} duration not divisible by cycle",
                name
            );
        }
    }

    #[test]
    fn test_unknown_preset_returns_none() {
        assert!(preset_config("nonexistent").is_none());
        assert!(preset_config("").is_none());
    }

    #[test]
    fn test_ratio_str_format() {
        let c = preset_config("balanced").unwrap();
        assert_eq!(c.ratio_str(), "5-5");
        let c = preset_config("calm").unwrap();
        assert_eq!(c.ratio_str(), "4-6");
    }

    #[test]
    fn test_total_breaths() {
        let c = preset_config("balanced").unwrap(); // 10 min = 600s, cycle 10s = 60 breaths
        assert_eq!(c.total_breaths(), 60);
        let c = preset_config("calm").unwrap(); // 15 min = 900s, cycle 10s = 90 breaths
        assert_eq!(c.total_breaths(), 90);
        let c = preset_config("extended").unwrap(); // 20 min = 1200s, cycle 10s = 120 breaths
        assert_eq!(c.total_breaths(), 120);
    }

    #[test]
    fn test_sphere_radius_inhale_grows() {
        let r0 = sphere_radius(Phase::Inhale, 0.0);
        let r1 = sphere_radius(Phase::Inhale, 0.5);
        let r2 = sphere_radius(Phase::Inhale, 1.0);
        assert_eq!(r0, SPHERE_MIN_R);
        assert!(r1 > r0, "midpoint should be larger than start");
        assert_eq!(r2, SPHERE_MAX_R);
    }

    #[test]
    fn test_sphere_radius_exhale_shrinks() {
        let r0 = sphere_radius(Phase::Exhale, 0.0);
        let r1 = sphere_radius(Phase::Exhale, 0.5);
        let r2 = sphere_radius(Phase::Exhale, 1.0);
        assert_eq!(r0, SPHERE_MAX_R);
        assert!(r1 < r0, "midpoint should be smaller than start");
        assert_eq!(r2, SPHERE_MIN_R);
    }

    #[test]
    fn test_sphere_radius_paused_frozen() {
        assert_eq!(sphere_radius(Phase::Paused, 0.0), SPHERE_MIN_R);
        assert_eq!(sphere_radius(Phase::Paused, 0.5), SPHERE_MIN_R);
        assert_eq!(sphere_radius(Phase::Paused, 1.0), SPHERE_MIN_R);
    }

    #[test]
    fn test_ease_breath_endpoints() {
        assert!((ease_breath(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_breath(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_ease_breath_monotonic() {
        let mut prev = -1.0;
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let v = ease_breath(t);
            assert!(v >= prev, "ease should be monotonic at t={}", t);
            prev = v;
        }
    }

    #[test]
    fn test_ease_breath_midpoint() {
        // At t=0.5 the cosine easing is exactly 0.5
        assert!((ease_breath(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_sphere_color_inhale_brightens() {
        let accent = Argb::new(255, 100, 60, 200);
        let c0 = sphere_color(Phase::Inhale, 0.0, accent);
        let c1 = sphere_color(Phase::Inhale, 1.0, accent);
        assert_eq!(c0, exhale_color(accent));
        assert_eq!(c1, accent);
    }

    #[test]
    fn test_sphere_color_exhale_dims() {
        let accent = Argb::new(255, 100, 60, 200);
        let c0 = sphere_color(Phase::Exhale, 0.0, accent);
        let c1 = sphere_color(Phase::Exhale, 1.0, accent);
        assert_eq!(c0, accent);
        assert_eq!(c1, exhale_color(accent));
    }

    #[test]
    fn test_sphere_color_midpoint_blend() {
        let accent = Argb::new(255, 100, 60, 200);
        let mid = sphere_color(Phase::Inhale, 0.5, accent);
        let expected = lerp_argb(exhale_color(accent), accent, 0.5);
        assert_eq!(mid, expected);
    }

    #[test]
    fn test_format_mmss() {
        assert_eq!(format_mmss(0), "00:00");
        assert_eq!(format_mmss(5), "00:05");
        assert_eq!(format_mmss(59), "00:59");
        assert_eq!(format_mmss(60), "01:00");
        assert_eq!(format_mmss(600), "10:00");
        assert_eq!(format_mmss(900), "15:00");
    }

    #[test]
    fn test_phase_label_text() {
        let accent = Argb::new(255, 0, 120, 200);
        let muted = Argb::new(255, 128, 128, 128);
        let mk = |phase: Phase| BreatheState {
            phase,
            config: preset_config("balanced").unwrap(),
            phase_start: None,
            breaths: 0,
            overlay_hwnd: None,
            last_theme: None,
            bb: false,
            start_time: 0,
            selected_preset: String::new(),
        };
        assert_eq!(phase_label(&mk(Phase::Inhale), accent, muted).0, "INHALE");
        assert_eq!(phase_label(&mk(Phase::Exhale), accent, muted).0, "EXHALE");
        assert_eq!(phase_label(&mk(Phase::Paused), accent, muted).0, "PAUSED");
        assert_eq!(
            phase_label(&mk(Phase::Complete), accent, muted).0,
            "Complete"
        );
        assert_eq!(
            phase_label(&mk(Phase::Idle), accent, muted).0,
            "Choose a preset"
        );
    }

    #[test]
    fn test_phase_label_selected_preset() {
        let accent = Argb::new(255, 0, 120, 200);
        let muted = Argb::new(255, 128, 128, 128);
        let mk = |phase: Phase, preset: &str| BreatheState {
            phase,
            config: preset_config("balanced").unwrap(),
            phase_start: None,
            breaths: 0,
            overlay_hwnd: None,
            last_theme: None,
            bb: false,
            start_time: 0,
            selected_preset: preset.to_string(),
        };
        assert_eq!(
            phase_label(&mk(Phase::Idle, "calm"), accent, muted).0,
            "Click again to start"
        );
    }

    #[test]
    fn test_button_labels_by_phase() {
        let mk = |phase: Phase| BreatheState {
            phase,
            config: preset_config("balanced").unwrap(),
            phase_start: None,
            breaths: 0,
            overlay_hwnd: None,
            last_theme: None,
            bb: false,
            start_time: 0,
            selected_preset: String::new(),
        };
        assert_eq!(button_labels(&mk(Phase::Inhale)), ["Pause", "", "Quit"]);
        assert_eq!(button_labels(&mk(Phase::Paused)), ["Resume", "", "Quit"]);
        assert_eq!(button_labels(&mk(Phase::Complete)), ["", "", "Close"]);
        assert_eq!(
            button_labels(&mk(Phase::Idle)),
            ["Balanced", "Calm", "Extended"]
        );
    }

    #[test]
    fn test_status_indicator() {
        let mk = |phase: Phase| BreatheState {
            phase,
            config: preset_config("balanced").unwrap(),
            phase_start: None,
            breaths: 0,
            overlay_hwnd: None,
            last_theme: None,
            bb: false,
            start_time: 0,
            selected_preset: String::new(),
        };
        assert_eq!(status_indicator(&mk(Phase::Inhale)), "●");
        assert_eq!(status_indicator(&mk(Phase::Paused)), "‖");
        assert_eq!(status_indicator(&mk(Phase::Complete)), "✓");
        assert_eq!(status_indicator(&mk(Phase::Idle)), "○");
    }
}
