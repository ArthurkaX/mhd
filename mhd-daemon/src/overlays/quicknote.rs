//! Quick Note — hotkey-driven text note overlay.
//!
//! Opens a small popup window. Type text, Enter saves to
//! `~/.config/mhd/notes/YYYY-MM-DD.md`, Escape cancels.
//! If blackbox is active, logs a `quicknote` artefact.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_BACK, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::*;

// ── Manual kernel32 FFI (not in windows-0.58 feature set) ────────
#[repr(C)]
#[allow(non_camel_case_types, non_snake_case)]
#[derive(Default)]
struct SYSTEMTIME {
    wYear: u16,
    wMonth: u16,
    wDayOfWeek: u16,
    wDay: u16,
    wHour: u16,
    wMinute: u16,
    wSecond: u16,
    wMilliseconds: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetTickCount() -> u32;
    fn GetLocalTime(lpSystemTime: *mut SYSTEMTIME);
}

use crate::config::path::home_dir;

// ── Config ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct QuickNoteConfig {
    pub enabled: bool,
    pub notes_dir: PathBuf,
}

impl Default for QuickNoteConfig {
    fn default() -> Self {
        QuickNoteConfig {
            enabled: true,
            notes_dir: default_notes_dir(),
        }
    }
}

fn default_notes_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("notes")
}

// ─── Constants ────────────────────────────────────────────────────────

const W: i32 = 480;
const H: i32 = 110;
const PAD: i32 = 12;
const INPUT_Y: i32 = 16;
const INPUT_H: i32 = 28;
const HINT_Y: i32 = 56;
const CARET_MS: u32 = 530;
const CLS: &str = "mhd_quicknote_cls";

// ─── Global toggle channel ─────────────────────────────────────────────

static CTRL: std::sync::Mutex<Option<mpsc::Sender<()>>> = std::sync::Mutex::new(None);

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Show or toggle the quick note window.
pub fn show(theme: crate::core::native_theme::NativeTheme, notes_dir: PathBuf, bb_enabled: bool) {
    if let Ok(g) = CTRL.lock() {
        if let Some(ref tx) = *g {
            let _ = tx.send(());
            return;
        }
    }

    let dying = Arc::new(AtomicBool::new(false));
    let d2 = dying.clone();
    let (tx, rx) = mpsc::channel();
    *CTRL.lock().unwrap() = Some(tx);

    std::thread::Builder::new()
        .name("quicknote".into())
        .spawn(move || run(theme, notes_dir, bb_enabled, d2, &rx))
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

fn run(
    theme: crate::core::native_theme::NativeTheme,
    notes_dir: PathBuf,
    bb: bool,
    dying: Arc<AtomicBool>,
    ctrl: &mpsc::Receiver<()>,
) {
    // Register class
    let cls = to_utf16_z(CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hi,
            hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
            lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            0, 0, W, H,
            None, None, hi, None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    // Centre on primary monitor
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - W) / 2;
    let y = wa.top + (wa.bottom - wa.top - H) / 2;
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), x, y, W, H, SWP_NOZORDER); }

    // Title bar
    let today = today_str();
    let title = format!("Quick Note — {today}\0");
    let tw: Vec<u16> = title.encode_utf16().collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr())); }

    // Store theme pointer in user data
    let theme_box = Box::into_raw(Box::new(theme));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, theme_box as isize); }

    // Show + focus
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = SetTimer(hwnd, 1, CARET_MS, None);
    }

    // ── Message loop ──────────────────────────────────────────────────
    let mut hidden = false;
    let mut text = String::new();
    let mut cursor = 0usize;
    let mut caret_on = true;
    let mut last_tick = 0u32;

    loop {
        if dying.load(Ordering::Acquire) { break; }

        let _ = unsafe {
            MsgWaitForMultipleObjects(
                None,
                false,
                if hidden { 200 } else { INFINITE },
                QS_ALLINPUT,
            )
        };

        // Toggle signal
        if ctrl.try_recv().is_ok() {
            hidden = !hidden;
            text.clear();
            cursor = 0;
            caret_on = true;
            if hidden {
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            } else {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(hwnd);
                    let _ = InvalidateRect(hwnd, None, true);
                }
            }
        }

        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_QUIT {
                dying.store(true, Ordering::Release);
                break;
            }
            match msg.message {
                WM_KEYDOWN => {
                    let vk = msg.wParam.0 as u16;
                    if vk == VK_RETURN.0 {
                        save(&notes_dir, &text, bb);
                        text.clear();
                        cursor = 0;
                        hidden = true;
                        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                    } else if vk == VK_ESCAPE.0 {
                        text.clear();
                        cursor = 0;
                        hidden = true;
                        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                    } else if vk == VK_BACK.0 {
                        if cursor > 0 {
                            cursor -= 1;
                            text.remove(cursor);
                            unsafe { let _ = InvalidateRect(hwnd, None, false); }
                        }
                    } else if vk == VK_DELETE.0 {
                        if cursor < text.len() {
                            text.remove(cursor);
                            unsafe { let _ = InvalidateRect(hwnd, None, false); }
                        }
                    } else if vk == VK_LEFT.0 && cursor > 0 {
                        cursor -= 1;
                        unsafe { let _ = InvalidateRect(hwnd, None, false); }
                    } else if vk == VK_RIGHT.0 && cursor < text.len() {
                        cursor += 1;
                        unsafe { let _ = InvalidateRect(hwnd, None, false); }
                    } else if vk == VK_HOME.0 {
                        cursor = 0;
                        unsafe { let _ = InvalidateRect(hwnd, None, false); }
                    } else if vk == VK_END.0 {
                        cursor = text.len();
                        unsafe { let _ = InvalidateRect(hwnd, None, false); }
                    }
                }
                WM_CHAR => {
                    let ch = msg.wParam.0 as u32;
                    if ch >= 0x20 && ch != 0x7f {
                        if let Some(c) = char::from_u32(ch) {
                            text.insert(cursor, c);
                            cursor += 1;
                            unsafe { let _ = InvalidateRect(hwnd, None, false); }
                        }
                    }
                }
                WM_TIMER if msg.wParam.0 as u32 == 1 => {
                    let now = unsafe { GetTickCount() };
                    if now.saturating_sub(last_tick) >= CARET_MS {
                        caret_on = !caret_on;
                        last_tick = now;
                        unsafe { let _ = InvalidateRect(hwnd, None, false); }
                    }
                }
                WM_PAINT => {
                    handle_paint(hwnd, &text, cursor, caret_on);
                }
                WM_ACTIVATE => {
                    if msg.wParam.0 as u32 == WA_INACTIVE {
                        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                        hidden = true;
                    }
                }
                WM_CLOSE | WM_SYSCOMMAND if msg.wParam.0 as u32 == SC_CLOSE => {
                    unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
                    hidden = true;
                }
                _ => {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        let _ = DispatchMessageW(&msg);
                    }
                }
            }
        }
    }

    // Free theme box
    unsafe {
        let _ = KillTimer(hwnd, 1);
        let _ = DestroyWindow(hwnd);
        let _ = Box::from_raw(theme_box);
    }
}

// ─── Window proc (minimal — handles DefWindowProc for caption buttons) ──

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ─── Painting ──────────────────────────────────────────────────────────

fn handle_paint(hwnd: HWND, text: &str, cursor: usize, caret_on: bool) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() { return; }

        // Get theme from user data
        let theme = {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr == 0 { let _ = EndPaint(hwnd, &ps); return; }
            &*(ptr as *const crate::core::native_theme::NativeTheme)
        };

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let cw = rc.right;
        let sc = sc_from_hwnd(hwnd);
        let ph = |v: i32| (v as f32 * sc) as i32;

        // Background
        let bg_brush = CreateSolidBrush(theme.background.to_colorref());
        let _ = FillRect(hdc, &rc, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Input field background
        let ir = RECT {
            left: ph(PAD),
            top: ph(INPUT_Y),
            right: cw - ph(PAD),
            bottom: ph(INPUT_Y + INPUT_H),
        };
        let surf_brush = CreateSolidBrush(theme.surface.to_colorref());
        let _ = FillRect(hdc, &ir, surf_brush);
        let _ = DeleteObject(surf_brush);

        // Input field border
        let border_brush = CreateSolidBrush(theme.border.to_colorref());
        let _ = FrameRect(hdc, &ir, border_brush);
        let _ = DeleteObject(border_brush);

        // Text
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let _ = SelectObject(hdc, font);
        let mut tri = RECT {
            left: ir.left + ph(4),
            top: ir.top + ph(3),
            right: ir.right - ph(4),
            bottom: ir.bottom - ph(3),
        };
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, theme.text.to_colorref());
        let mut tw: Vec<u16> = text.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut tw, &mut tri as *mut RECT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        // Caret
        if caret_on {
            let before: Vec<u16> = text[..cursor].encode_utf16().collect();
            let mut sz = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &before, &mut sz);
            let cx = tri.left + sz.cx;
            let cy = ir.top + ph(3);
            let cw_px = ph(2).max(1);
            let caret_rc = RECT { left: cx, top: cy, right: cx + cw_px, bottom: ir.bottom - ph(3) };
            let caret_brush = CreateSolidBrush(theme.text.to_colorref());
            let _ = FillRect(hdc, &caret_rc, caret_brush);
            let _ = DeleteObject(caret_brush);
        }

        // Hint
        let hint = "Enter to save · Esc to cancel\0";
        let mut hw: Vec<u16> = hint.encode_utf16().collect();
        let mut hr = RECT {
            left: ph(PAD),
            top: ph(HINT_Y),
            right: cw - ph(PAD),
            bottom: ph(HINT_Y + 16),
        };
        let _ = SetTextColor(hdc, theme.text_muted.to_colorref());
        let _ = DrawTextW(hdc, &mut hw, &mut hr as *mut RECT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        let _ = EndPaint(hwnd, &ps);
    }
}

// ─── Save & Blackbox ───────────────────────────────────────────────────

fn save(notes_dir: &PathBuf, text: &str, bb: bool) {
    let text = text.trim();
    if text.is_empty() { return; }

    if let Err(e) = std::fs::create_dir_all(notes_dir) {
        eprintln!("mhd: quicknote — cannot create notes dir: {e}");
        return;
    }

    let today = today_str();
    let path = notes_dir.join(format!("{today}.md"));
    let (h, m, s) = now_hms();
    let entry = format!("## {today} {h:02}:{m:02}:{s:02}\n{text}\n\n");

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = f.write_all(entry.as_bytes());
    }

    if bb {
        #[cfg(feature = "blackbox")]
        crate::blackbox::send_event(crate::blackbox::BlackboxEvent::Input {
            kind: crate::blackbox::InputKind::Keyboard,
            ts: epoch_secs(),
        });
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn to_utf16_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn work_area() -> RECT {
    unsafe {
        let mut r = std::mem::zeroed();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        r
    }
}

fn today_str() -> String {
    unsafe {
        let mut st = std::mem::zeroed::<SYSTEMTIME>();
        GetLocalTime(&mut st);
        format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
    }
}

fn now_hms() -> (u32, u32, u32) {
    unsafe {
        let mut st = std::mem::zeroed::<SYSTEMTIME>();
        GetLocalTime(&mut st);
        (st.wHour.into(), st.wMinute.into(), st.wSecond.into())
    }
}

fn sc_from_hwnd(hwnd: HWND) -> f32 {
    unsafe {
        let dc = GetDC(hwnd);
        if dc.is_invalid() { return 1.0; }
        let dpi = GetDeviceCaps(dc, LOGPIXELSY);
        let _ = ReleaseDC(hwnd, dc);
        dpi as f32 / 96.0
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_today_str_format() {
        let s = today_str();
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }
}
