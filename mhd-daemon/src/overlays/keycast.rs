//! KeyCast overlay — shows pressed keys for recordings and streams.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, GetKeyboardState, ToUnicodeEx,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, GetWindowThreadProcessId, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
    SW_HIDE, SW_SHOWNA, ShowWindow, TranslateMessage, WM_QUIT, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::native_theme::NativeTheme;
use crate::renderer::{DibFrame, create_font, primary_monitor_work_rect, to_utf16_z};
use crate::trigger::{PhysicalKey, Trigger};

const WIDTH_BASE: i32 = 760;
const ROW_HEIGHT_BASE: i32 = 50;
const PILL_GAP_BASE: i32 = 8;
const MAX_TOASTS: usize = 12;
const HEIGHT_BASE: i32 = ROW_HEIGHT_BASE;
const MARGIN_BASE: i32 = 28;
const DEFAULT_DURATION_MS: u64 = 1200;
const ENTER_MS: u64 = 240;
const EXIT_MS: u64 = 280;
const DEFAULT_SHOW_TYPING: bool = false;
const DEFAULT_TYPING_WIDTH_CHARS: u32 = 22;
const DEFAULT_TYPING_DURATION_MS: u64 = 2500;
const TYPING_ENTER_MS: u64 = 140;
const TYPING_EXIT_MS: u64 = 220;
const TYPING_BLOCK_GAP_BASE: i32 = 10;
const TYPING_BLOCK_HEIGHT_BASE: i32 = 50;

static ENABLED: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<mpsc::Sender<KeycastCommand>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeycastPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl KeycastPosition {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "top_left" | "top-left" => Some(Self::TopLeft),
            "top_center" | "top-center" => Some(Self::TopCenter),
            "top_right" | "top-right" => Some(Self::TopRight),
            "bottom_left" | "bottom-left" => Some(Self::BottomLeft),
            "bottom_center" | "bottom-center" => Some(Self::BottomCenter),
            "bottom_right" | "bottom-right" => Some(Self::BottomRight),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => "Top Left",
            Self::TopCenter => "Top Center",
            Self::TopRight => "Top Right",
            Self::BottomLeft => "Bottom Left",
            Self::BottomCenter => "Bottom Center",
            Self::BottomRight => "Bottom Right",
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            Self::TopLeft => "top_left",
            Self::TopCenter => "top_center",
            Self::TopRight => "top_right",
            Self::BottomLeft => "bottom_left",
            Self::BottomCenter => "bottom_center",
            Self::BottomRight => "bottom_right",
        }
    }

    pub fn all() -> [Self; 6] {
        [
            Self::TopLeft,
            Self::TopCenter,
            Self::TopRight,
            Self::BottomLeft,
            Self::BottomCenter,
            Self::BottomRight,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeycastConfig {
    pub position: KeycastPosition,
    pub duration_ms: u64,
    /// Show single printable keystrokes in a fixed typing block.
    pub show_typing: bool,
    /// Width of the typing block in characters.
    pub typing_width_chars: u32,
    /// How long a typed character stays visible.
    pub typing_duration_ms: u64,
}

impl Default for KeycastConfig {
    fn default() -> Self {
        Self {
            position: KeycastPosition::BottomCenter,
            duration_ms: DEFAULT_DURATION_MS,
            show_typing: DEFAULT_SHOW_TYPING,
            typing_width_chars: DEFAULT_TYPING_WIDTH_CHARS,
            typing_duration_ms: DEFAULT_TYPING_DURATION_MS,
        }
    }
}

enum KeycastCommand {
    Show(Trigger),
    ShowLabel(String),
    /// Printable single keystroke resolved via current keyboard layout.
    ShowKey(String),
    SetTheme(NativeTheme),
    SetConfig(KeycastConfig),
    Hide,
}

struct KeyToast {
    label: String,
    created_at: Instant,
}

/// One character in the typing block carousel.
struct TypingChar {
    ch: String,
    created_at: Instant,
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn toggle(theme: NativeTheme, config: KeycastConfig) -> bool {
    ensure_thread();
    let new = !ENABLED.load(Ordering::Acquire);
    ENABLED.store(new, Ordering::Release);

    sync_config(theme, config);

    if let Some(tx) = TX.get()
        && !new
    {
        let _ = tx.send(KeycastCommand::Hide);
    }

    new
}

pub fn sync_config(theme: NativeTheme, config: KeycastConfig) {
    if !is_enabled() {
        return;
    }

    ensure_thread();
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeycastCommand::SetTheme(theme));
        let _ = tx.send(KeycastCommand::SetConfig(config));
    }
}

pub fn show_trigger(trigger: Trigger) {
    if !is_enabled() {
        return;
    }

    ensure_thread();
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeycastCommand::Show(trigger));
    }
}

/// Push a single printable character (already resolved via the active
/// keyboard layout) into the typing-block carousel.
pub fn show_key(ch: String) {
    if !is_enabled() || ch.is_empty() {
        return;
    }

    ensure_thread();
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeycastCommand::ShowKey(ch));
    }
}

pub fn show_mouse_button(label: &str, modifiers: crate::trigger::Modifiers) {
    if !is_enabled() {
        return;
    }

    ensure_thread();
    if let Some(tx) = TX.get() {
        let _ = tx.send(KeycastCommand::ShowLabel(modified_label(modifiers, label)));
    }
}

/// Resolve a virtual key code to a Unicode character using the current
/// keyboard layout. Returns `None` for non-printable keys (controls,
/// function keys, dead keys, etc.).
///
/// This is what makes KeyCast show `й` instead of `Q` when the Russian
/// layout is active — `vk_to_name` only knows physical key identities.
pub fn resolve_vk_to_char(vk: u8) -> Option<String> {
    // Only treat these vk ranges as potentially printable: A–Z, 0–9, OEM,
    // space, and numpad keys. Everything else (F-keys, arrows, Esc, etc.)
    // has no useful character and is handled by `show_trigger` instead.
    let printable = matches!(
        vk,
        0x30..=0x39 | 0x41..=0x5A | 0x20 | 0xBD | 0xBB | 0xBC | 0xBE | 0xBF | 0xBA | 0xDE | 0xDC
            | 0xDB | 0xDD | 0xC0 | 0x60..=0x69 | 0x6A | 0x6B | 0x6D | 0x6F | 0x6E
    );
    if !printable {
        return None;
    }

    unsafe {
        // Get the keyboard layout of the **foreground window's thread**.
        // Each thread has its own layout — the user switches layouts only
        // for the active thread, so we must match that thread, not our own.
        let foreground = GetForegroundWindow();
        let hkl = if foreground.0.is_null() {
            GetKeyboardLayout(0)
        } else {
            let tid = GetWindowThreadProcessId(foreground, None);
            if tid != 0 {
                GetKeyboardLayout(tid)
            } else {
                GetKeyboardLayout(0)
            }
        };

        // GetKeyboardState may lag in a low-level hook proc (the OS hasn't
        // updated it for the current key yet). Fix modifiers via
        // GetAsyncKeyState which works reliably from any thread.
        let mut key_state = [0u8; 256];
        let _ = GetKeyboardState(&mut key_state);
        // Overwrite modifier states with async snapshot.
        let mod_vks = [
            0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5,
        ];
        for &vk_mod in &mod_vks {
            let down = GetAsyncKeyState(vk_mod) < 0;
            key_state[vk_mod as usize] = if down { 0x80 } else { 0 };
        }

        let mut buf = [0u16; 8];
        let n = ToUnicodeEx(vk as u32, 0, &key_state, &mut buf, 0, hkl);
        if n > 0 {
            let s = String::from_utf16_lossy(&buf[..n as usize]);
            // Filter out control characters; keep printable + space only.
            if s.chars().any(|c| !c.is_control() || c == ' ') {
                return Some(s);
            }
        }
    }
    None
}

fn ensure_thread() {
    if TX.get().is_some() {
        return;
    }

    let (tx, rx) = mpsc::channel();
    if TX.set(tx).is_err() {
        return;
    }

    let _ = std::thread::Builder::new()
        .name("keycast-overlay".into())
        .spawn(move || run_overlay(rx));
}

fn run_overlay(rx: mpsc::Receiver<KeycastCommand>) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = to_utf16_z("mhd_keycast_overlay_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }

    let ex_style =
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let hwnd = match unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            WIDTH_BASE,
            HEIGHT_BASE,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    // Second window: fixed-width typing block. Reuses the same window class.
    let typing_cls_name = to_utf16_z("mhd_keycast_typing_cls");
    let typing_wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: PCWSTR::from_raw(typing_cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&typing_wc);
    }
    let typing_hwnd = match unsafe {
        CreateWindowExW(
            ex_style,
            PCWSTR::from_raw(typing_cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            WIDTH_BASE,
            TYPING_BLOCK_HEIGHT_BASE,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let width = (WIDTH_BASE as f32 * scale) as i32;
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let pill_gap = (PILL_GAP_BASE as f32 * scale) as i32;
    let height = row_h;
    let margin = (MARGIN_BASE as f32 * scale) as i32;
    let block_h = (TYPING_BLOCK_HEIGHT_BASE as f32 * scale) as i32;
    let mut theme = NativeTheme::default();
    let mut config = KeycastConfig::default();
    let mut toasts: Vec<KeyToast> = Vec::new();
    let mut typing: Vec<TypingChar> = Vec::new();

    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                KeycastCommand::Show(trigger) => {
                    let label = trigger_label(trigger);
                    push_toast(&mut toasts, label);
                }
                KeycastCommand::ShowLabel(label) => {
                    push_toast(&mut toasts, label);
                }
                KeycastCommand::ShowKey(ch) => {
                    push_typing_char(&mut typing, ch);
                }
                KeycastCommand::SetTheme(next) => {
                    theme = next;
                }
                KeycastCommand::SetConfig(next) => {
                    config = next;
                }
                KeycastCommand::Hide => {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        let _ = ShowWindow(typing_hwnd, SW_HIDE);
                    }
                    toasts.clear();
                    typing.clear();
                }
            }
        }

        let now = Instant::now();
        let total_ms = total_lifetime_ms(config.duration_ms);
        toasts.retain(|toast| toast_age_ms(toast, now) < total_ms);

        let typing_total_ms = TYPING_ENTER_MS + config.typing_duration_ms + TYPING_EXIT_MS;
        typing.retain(|c| {
            (now.saturating_duration_since(c.created_at).as_millis() as u64) < typing_total_ms
        });

        let pills_visible = !toasts.is_empty();
        let typing_visible = config.show_typing && !typing.is_empty();

        if pills_visible {
            paint_pills(
                hwnd, &toasts, now, width, height, row_h, pill_gap, margin, scale, &theme, &config,
            );
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
            }
        } else {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }

        if typing_visible {
            paint_typing_block(
                typing_hwnd,
                &typing,
                now,
                scale,
                block_h,
                &theme,
                &config,
                pills_visible,
            );
            unsafe {
                let _ = ShowWindow(typing_hwnd, SW_SHOWNA);
            }
        } else {
            unsafe {
                let _ = ShowWindow(typing_hwnd, SW_HIDE);
            }
        }

        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    let _ = DestroyWindow(hwnd);
                    let _ = DestroyWindow(typing_hwnd);
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        std::thread::sleep(Duration::from_millis(16));
    }
}

fn paint_pills(
    hwnd: HWND,
    toasts: &[KeyToast],
    now: Instant,
    width: i32,
    height: i32,
    row_h: i32,
    pill_gap: i32,
    margin: i32,
    scale: f32,
    theme: &NativeTheme,
    config: &KeycastConfig,
) {
    let mut frame = match DibFrame::new(width, height) {
        Some(f) => f,
        None => return,
    };

    frame.pixels_mut().fill(0);

    let font = create_font(-(17.0 * scale) as i32, true, "Segoe UI");
    let old_font = unsafe { SelectObject(frame.dc(), font) };
    unsafe {
        let _ = SetBkMode(frame.dc(), TRANSPARENT);
        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
    }

    let pill_radius = (14.0 * scale) as i32;
    let text_pad = (18.0 * scale) as i32;
    let text_overhang = (8.0 * scale) as i32;
    let min_pill_w = (54.0 * scale) as i32;
    let max_pill_w = (480.0 * scale) as i32;

    let visible: Vec<(usize, &KeyToast)> =
        toasts.iter().rev().take(MAX_TOASTS).enumerate().collect();
    let pill_widths: Vec<i32> = visible
        .iter()
        .map(|(_, toast)| {
            (measure_text_width(frame.dc(), &toast.label) + text_pad * 2 + text_overhang)
                .clamp(min_pill_w, max_pill_w)
        })
        .collect();
    let target_xs = queue_target_xs(width, &pill_widths, pill_gap, config.position);
    let y = (height - row_h) / 2;

    for (draw_idx, (_, toast)) in visible.iter().enumerate().rev() {
        let age_ms = toast_age_ms(toast, now);
        let motion = toast_motion(age_ms, config.duration_ms);
        let pill_w = pill_widths[draw_idx];
        let target_x = target_xs[draw_idx];
        let x = lerp_i32(width, target_x, motion.enter_t);
        let x = lerp_i32(x, -pill_w - margin, motion.exit_t);

        let mut bg = theme.background;
        bg.a = ((bg.a as f32) * motion.alpha).clamp(0.0, 255.0) as u8;
        draw_pill(
            frame.pixels_mut(),
            width,
            height,
            x,
            y,
            pill_w,
            row_h,
            pill_radius,
            bg,
        );

        let text_w = pill_w - text_pad * 2;
        let clamped = pill_w >= max_pill_w;
        let mut rc = RECT {
            left: x + text_pad,
            top: y,
            right: x + text_pad + text_w,
            bottom: y + row_h,
        };
        let mut wz = to_utf16_z(&toast.label);
        let flags = if clamped {
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS
        } else {
            DT_CENTER | DT_SINGLELINE | DT_VCENTER
        };
        unsafe {
            let _ = DrawTextW(frame.dc(), &mut wz, &mut rc, flags);
        }
    }

    unsafe {
        let _ = SelectObject(frame.dc(), old_font);
        let _ = DeleteObject(font);
    }

    frame.fix_gdi_alpha(theme.background);
    let work = primary_monitor_work_rect();
    let (x, y) = overlay_position(&work, width, height, margin, config.position);
    frame.present_layered(hwnd, x, y, 245);
}

fn measure_text_width(hdc: windows::Win32::Graphics::Gdi::HDC, text: &str) -> i32 {
    let wz = to_utf16_z(text);
    let text_slice = &wz[..wz.len().saturating_sub(1)];
    let mut size = SIZE::default();
    unsafe {
        let _ = GetTextExtentPoint32W(hdc, text_slice, &mut size);
    }
    size.cx
}

/// Paint the fixed-width typing block.
///
/// The block is a single rounded pill of constant width (sized to fit
/// `typing_width_chars` average characters). Inside it, characters arrive
/// from the right, settle, and slide out to the left on their own timers —
/// a mini-carousel mirroring the shortcut pills.
///
/// `pills_visible` shifts the block away from the shortcut carousel so the
/// two never overlap.
fn paint_typing_block(
    hwnd: HWND,
    typing: &[TypingChar],
    now: Instant,
    scale: f32,
    block_h: i32,
    theme: &NativeTheme,
    config: &KeycastConfig,
    pills_visible: bool,
) {
    // Size the block to fit `typing_width_chars` of an average glyph width.
    let char_w = (12.0 * scale) as i32;
    let text_pad = (18.0 * scale) as i32;
    let block_w = (char_w * config.typing_width_chars as i32) + text_pad * 2;
    let block_gap = (TYPING_BLOCK_GAP_BASE as f32 * scale) as i32;
    let margin = (MARGIN_BASE as f32 * scale) as i32;

    let mut frame = match DibFrame::new(block_w, block_h) {
        Some(f) => f,
        None => return,
    };
    frame.pixels_mut().fill(0);

    let font = create_font(-(17.0 * scale) as i32, true, "Segoe UI");
    let old_font = unsafe { SelectObject(frame.dc(), font) };
    unsafe {
        let _ = SetBkMode(frame.dc(), TRANSPARENT);
        let _ = SetTextColor(frame.dc(), theme.text.to_colorref());
    }

    let pill_radius = (14.0 * scale) as i32;

    // Draw the container pill.
    let mut bg = theme.background;
    bg.a = 255;
    draw_pill(
        frame.pixels_mut(),
        block_w,
        block_h,
        0,
        0,
        block_w,
        block_h,
        pill_radius,
        bg,
    );

    // Visible chars: most recent first, capped to what fits in the block width.
    let inner_w = block_w - text_pad * 2;
    let max_chars = (inner_w / char_w).max(1) as usize;
    let visible: Vec<&TypingChar> = typing.iter().rev().take(max_chars).collect();

    // Measure actual glyph widths so we can pack them right-to-left inside the block.
    let widths: Vec<i32> = visible
        .iter()
        .map(|c| measure_text_width(frame.dc(), &c.ch).max(char_w / 2))
        .collect();
    let char_gap = (2.0 * scale) as i32;

    // Right edge of the typing region (inside padding).
    let right_edge = block_w - text_pad;
    let mut cursor_x = right_edge;

    // Draw from most-recent (right) to oldest (left).
    for (i, c) in visible.iter().enumerate() {
        let age_ms = now.saturating_duration_since(c.created_at).as_millis() as u64;
        let motion = typing_motion(age_ms, config.typing_duration_ms);
        let w = widths[i];

        // Target resting position: packed to the left of newer chars.
        let target_x = cursor_x - w;
        // Enter from the right edge, exit to the left edge.
        let enter_from = right_edge;
        let exit_to = -w;
        let x = lerp_i32(enter_from, target_x, motion.enter_t);
        let x = lerp_i32(x, exit_to, motion.exit_t);

        // Draw the glyph (no per-char background — it lives inside the container pill).
        let mut rc = RECT {
            left: x,
            top: 0,
            right: x + w,
            bottom: block_h,
        };
        // Apply per-char alpha by drawing into a temporary would be expensive;
        // instead we fade by skipping draw once alpha drops below a threshold.
        if motion.alpha > 0.02 {
            let mut wz = to_utf16_z(&c.ch);
            unsafe {
                let _ = DrawTextW(
                    frame.dc(),
                    &mut wz,
                    &mut rc,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
        }

        cursor_x = target_x - char_gap;
        if cursor_x < text_pad {
            break;
        }
    }

    unsafe {
        let _ = SelectObject(frame.dc(), old_font);
        let _ = DeleteObject(font);
    }

    frame.fix_gdi_alpha(theme.background);
    let work = primary_monitor_work_rect();

    // Position the typing block next to the shortcut carousel. When pills are
    // visible, the block sits one gap below (bottom positions) or above (top
    // positions) the carousel so they stack neatly. When alone, it takes the
    // carousel's place.
    let row_h = (ROW_HEIGHT_BASE as f32 * scale) as i32;
    let (base_x, base_y) = overlay_position(
        &work,
        (WIDTH_BASE as f32 * scale) as i32,
        row_h,
        margin,
        config.position,
    );

    // Horizontal: align to the same anchor as the carousel for left/right,
    // center the fixed block for center positions.
    let x = match config.position {
        KeycastPosition::TopLeft | KeycastPosition::BottomLeft => base_x,
        KeycastPosition::TopRight | KeycastPosition::BottomRight => {
            base_x + ((WIDTH_BASE as f32 * scale) as i32) - block_w
        }
        KeycastPosition::TopCenter | KeycastPosition::BottomCenter => {
            base_x + ((WIDTH_BASE as f32 * scale) as i32 - block_w) / 2
        }
    };

    let y = if pills_visible {
        match config.position {
            // Bottom positions: block goes below the carousel.
            KeycastPosition::BottomLeft
            | KeycastPosition::BottomCenter
            | KeycastPosition::BottomRight => base_y + row_h + block_gap,
            // Top positions: block goes above the carousel.
            KeycastPosition::TopLeft | KeycastPosition::TopCenter | KeycastPosition::TopRight => {
                base_y - block_h - block_gap
            }
        }
    } else {
        base_y
    };

    frame.present_layered(hwnd, x, y, 245);
}

/// Motion curve for a typing-block character. Mirrors `toast_motion` but with
/// its own (shorter) enter/exit timings so typing feels snappier.
fn typing_motion(age_ms: u64, hold_ms: u64) -> ToastMotion {
    let enter_raw = (age_ms as f32 / TYPING_ENTER_MS as f32).clamp(0.0, 1.0);
    let exit_start = TYPING_ENTER_MS + hold_ms;
    let exit_raw = if age_ms <= exit_start {
        0.0
    } else {
        ((age_ms - exit_start) as f32 / TYPING_EXIT_MS as f32).clamp(0.0, 1.0)
    };
    let enter_t = smootherstep(enter_raw);
    let exit_t = smootherstep(exit_raw);
    let alpha = (enter_t * (1.0 - exit_t)).clamp(0.0, 1.0);
    ToastMotion {
        enter_t,
        exit_t,
        alpha,
    }
}

fn queue_target_xs(
    width: i32,
    pill_widths: &[i32],
    gap: i32,
    position: KeycastPosition,
) -> Vec<i32> {
    let mut xs = Vec::with_capacity(pill_widths.len());
    let queue_w =
        pill_widths.iter().sum::<i32>() + gap * (pill_widths.len().saturating_sub(1) as i32);
    let visible_queue_w = queue_w.min(width);
    let mut cursor = match position {
        KeycastPosition::TopLeft | KeycastPosition::BottomLeft => visible_queue_w,
        KeycastPosition::TopCenter | KeycastPosition::BottomCenter => (width + visible_queue_w) / 2,
        KeycastPosition::TopRight | KeycastPosition::BottomRight => width,
    };
    for &pill_w in pill_widths {
        cursor -= pill_w;
        xs.push(cursor);
        cursor -= gap;
    }
    xs
}

fn push_toast(toasts: &mut Vec<KeyToast>, label: String) {
    toasts.push(KeyToast {
        label,
        created_at: Instant::now(),
    });
    let max_kept = MAX_TOASTS * 2;
    if toasts.len() > max_kept {
        toasts.drain(0..toasts.len() - max_kept);
    }
}

fn push_typing_char(typing: &mut Vec<TypingChar>, ch: String) {
    typing.push(TypingChar {
        ch,
        created_at: Instant::now(),
    });
    // Keep a reasonable cap so a long burst doesn't grow unbounded.
    let max_kept = 64;
    if typing.len() > max_kept {
        typing.drain(0..typing.len() - max_kept);
    }
}

fn total_lifetime_ms(hold_ms: u64) -> u64 {
    ENTER_MS + hold_ms + EXIT_MS
}

fn toast_age_ms(toast: &KeyToast, now: Instant) -> u64 {
    now.saturating_duration_since(toast.created_at).as_millis() as u64
}

struct ToastMotion {
    enter_t: f32,
    exit_t: f32,
    alpha: f32,
}

fn toast_motion(age_ms: u64, hold_ms: u64) -> ToastMotion {
    let enter_raw = (age_ms as f32 / ENTER_MS as f32).clamp(0.0, 1.0);
    let exit_start = ENTER_MS + hold_ms;
    let exit_raw = if age_ms <= exit_start {
        0.0
    } else {
        ((age_ms - exit_start) as f32 / EXIT_MS as f32).clamp(0.0, 1.0)
    };
    let enter_t = smootherstep(enter_raw);
    let exit_t = smootherstep(exit_raw);
    let alpha = (enter_t * (1.0 - exit_t)).clamp(0.0, 1.0);
    ToastMotion {
        enter_t,
        exit_t,
        alpha,
    }
}

fn smootherstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn lerp_i32(from: i32, to: i32, t: f32) -> i32 {
    (from as f32 + (to - from) as f32 * t).round() as i32
}

fn draw_pill(
    pixels: &mut [u32],
    width: i32,
    height: i32,
    left: i32,
    top: i32,
    pill_w: i32,
    pill_h: i32,
    radius: i32,
    color: crate::native_theme::Argb,
) {
    if pill_w <= 0 || pill_h <= 0 || width <= 0 || height <= 0 {
        return;
    }

    let color_px = color.to_premultiplied_argb_pixel();
    let r = radius.max(1);
    let right = left + pill_w;
    let bottom = top + pill_h;
    let draw_left = left.max(0);
    let draw_top = top.max(0);
    let draw_right = right.min(width);
    let draw_bottom = bottom.min(height);

    for y in draw_top..draw_bottom {
        for x in draw_left..draw_right {
            let lx = x - left;
            let ly = y - top;
            let cx = if lx < r {
                r
            } else if lx >= pill_w - r {
                pill_w - r - 1
            } else {
                lx
            };
            let cy = if ly < r {
                r
            } else if ly >= pill_h - r {
                pill_h - r - 1
            } else {
                ly
            };
            let dx = (lx - cx) as f32;
            let dy = (ly - cy) as f32;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= r as f32 {
                let falloff = 1.0 - (dist - (r as f32 - 1.0)).clamp(0.0, 1.0);
                let px = if falloff >= 1.0 {
                    color_px
                } else {
                    scale_alpha(color_px, falloff)
                };
                pixels[(y * width + x) as usize] = px;
            }
        }
    }
}

fn scale_alpha(px: u32, scale: f32) -> u32 {
    let scale = scale.clamp(0.0, 1.0);
    let a = (((px >> 24) & 0xff) as f32 * scale) as u32;
    let r = (((px >> 16) & 0xff) as f32 * scale) as u32;
    let g = (((px >> 8) & 0xff) as f32 * scale) as u32;
    let b = ((px & 0xff) as f32 * scale) as u32;
    (a.min(255) << 24) | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

fn overlay_position(
    work: &RECT,
    width: i32,
    height: i32,
    margin: i32,
    position: KeycastPosition,
) -> (i32, i32) {
    let center_x = work.left + (work.right - work.left - width) / 2;
    let left_x = work.left + margin;
    let right_x = work.right - width - margin;
    let top_y = work.top + margin;
    let bottom_y = work.bottom - height - margin;

    match position {
        KeycastPosition::TopLeft => (left_x, top_y),
        KeycastPosition::TopCenter => (center_x, top_y),
        KeycastPosition::TopRight => (right_x, top_y),
        KeycastPosition::BottomLeft => (left_x, bottom_y),
        KeycastPosition::BottomCenter => (center_x, bottom_y),
        KeycastPosition::BottomRight => (right_x, bottom_y),
    }
}

fn trigger_label(trigger: Trigger) -> String {
    modified_label(trigger.modifiers, &key_label(trigger.key))
}

fn modified_label(modifiers: crate::trigger::Modifiers, label: &str) -> String {
    let mut parts = Vec::new();
    if modifiers.ctrl() {
        parts.push("Ctrl".to_string());
    }
    if modifiers.alt() {
        parts.push("Alt".to_string());
    }
    if modifiers.shift() {
        parts.push("Shift".to_string());
    }
    if modifiers.win() {
        parts.push("Win".to_string());
    }
    parts.push(label.to_string());
    parts.join(" + ")
}

fn key_label(key: PhysicalKey) -> String {
    match key {
        PhysicalKey::Keyboard(vk) => pretty_vk(vk),
        PhysicalKey::MouseButton(1) => "Mouse 4".to_string(),
        PhysicalKey::MouseButton(2) => "Mouse 5".to_string(),
        PhysicalKey::MouseButton(3) => "Middle Mouse".to_string(),
        PhysicalKey::MouseButton(_) => "Mouse".to_string(),
        PhysicalKey::WheelUp => "Wheel Up".to_string(),
        PhysicalKey::WheelDown => "Wheel Down".to_string(),
        PhysicalKey::WheelLeft => "Wheel Left".to_string(),
        PhysicalKey::WheelRight => "Wheel Right".to_string(),
    }
}

fn pretty_vk(vk: u8) -> String {
    let name = crate::trigger::vk_to_name(vk);
    match name.as_str() {
        "esc" => "Esc".to_string(),
        "capslock" => "Caps Lock".to_string(),
        "pageup" => "Page Up".to_string(),
        "pagedown" => "Page Down".to_string(),
        "printscreen" => "Print Screen".to_string(),
        "scrolllock" => "Scroll Lock".to_string(),
        "numlock" => "Num Lock".to_string(),
        "contextmenu" => "Menu".to_string(),
        "backquote" => "`".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "slash" => "/".to_string(),
        "semicolon" => ";".to_string(),
        "quote" => "'".to_string(),
        "backslash" => "\\".to_string(),
        "lbracket" => "[".to_string(),
        "rbracket" => "]".to_string(),
        _ if name.len() == 1 => name.to_ascii_uppercase(),
        _ if name.len() > 1
            && name.starts_with('f')
            && name[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            name.to_ascii_uppercase()
        }
        _ if name.starts_with("numpad") => name.replacen("numpad", "Numpad ", 1),
        _ => {
            let mut chars = name.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => name,
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
