//! Quick Note — hotkey-driven text note overlay.
//!
//! Opens a small popup window. Type text, Enter saves to
//! `~/.config/mhd/notes/YYYY-MM-DD.md`, Escape cancels.
//! If blackbox is active, logs a `quicknote` artefact.

#![allow(unsafe_op_in_unsafe_fn)]

/// Quick debug logging to a temp file (survives crashes).
fn qlog(msg: impl std::fmt::Display) {
    use std::io::Write;
    let path = std::env::var("TEMP").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&path).join("mhd_qn.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", msg);
    }
}

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_BACK, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

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
const INPUT_H: i32 = 28;
const HINT_Y: i32 = 56;
const TMR_ID: usize = 1;
const CARET_MS: u32 = 530;
const CLS: &str = "mhd_quicknote_cls";

// ─── Global toggle ─────────────────────────────────────────────────────

static CTRL: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(theme: crate::core::native_theme::NativeTheme, notes_dir: PathBuf, bb: bool) {
    qlog(format!("quicknote: show() called"));
    if let Ok(g) = CTRL.lock() {
        if let Some(ref tx) = *g {
            qlog(format!("quicknote: toggling existing window"));
            let _ = tx.send(());
            return;
        }
    }

    let dying = Arc::new(AtomicBool::new(false));
    let d2 = dying.clone();
    let (tx, rx) = mpsc::channel();
    *CTRL.lock().unwrap() = Some(tx);

    qlog(format!("quicknote: spawning thread"));
    std::thread::Builder::new()
        .name("quicknote".into())
        .spawn(move || {
            qlog(format!("quicknote: thread started"));
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(theme, notes_dir, bb, d2, &rx);
            }));
            qlog(format!("quicknote: thread exiting"));
            *CTRL.lock().unwrap() = None;
        })
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

struct WndState {
    text: String,
    cursor: usize,
    caret_on: bool,
    last_tick: u32,
    _dying: Arc<AtomicBool>,
    notes_dir: PathBuf,
    bb: bool,
    hidden: bool,
    theme: crate::core::native_theme::NativeTheme,
}

fn run(
    theme: crate::core::native_theme::NativeTheme,
    notes_dir: PathBuf,
    bb: bool,
    dying: Arc<AtomicBool>,
    ctrl: &mpsc::Receiver<()>,
) {
    qlog(format!("quicknote: run() entered"));
    let cls = to_utf16_z(CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();
    qlog(format!("quicknote: got module handle"));

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
    qlog(format!("quicknote: class registered"));

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
        Err(e) => { qlog(format!("quicknote: CreateWindowExW failed: {:?}", e)); return; },
    };
    qlog(format!("quicknote: window created hwnd={:?}", hwnd));

    // Mutable state lives on stack for the window's lifetime
    let mut st = WndState {
        text: String::new(),
        cursor: 0,
        caret_on: true,
        last_tick: 0,
        _dying: dying.clone(),
        notes_dir,
        bb,
        hidden: false,
        theme,
    };
    let state_ptr: *mut WndState = &mut st;
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }
    qlog(format!("quicknote: state pointer set"));

    // Centre
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - W) / 2;
    let y = wa.top + (wa.bottom - wa.top - H) / 2;
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), x, y, W, H, SWP_NOZORDER); }
    qlog(format!("quicknote: window positioned"));

    // Title
    let today = date_str();
    let title = format!("Quick Note — {today}\0");
    let tw: Vec<u16> = title.encode_utf16().collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr())); }
    qlog(format!("quicknote: title set"));

    // Show + focus + timer
    qlog(format!("quicknote: about to show window"));
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        qlog(format!("quicknote: window shown"));
        let _ = SetForegroundWindow(hwnd);
        qlog(format!("quicknote: foreground set"));
        let _ = SetFocus(hwnd);
        qlog(format!("quicknote: focus set"));
        let _ = SetTimer(hwnd, TMR_ID, CARET_MS, None);
        qlog(format!("quicknote: timer set"));
    }

    // ── Message loop ──────────────────────────────────────────────────
    qlog(format!("quicknote: entering message loop"));
    loop {
        if dying.load(Ordering::Acquire) { break; }

        let _ = unsafe {
            MsgWaitForMultipleObjects(None, false, INFINITE, QS_ALLINPUT)
        };

        // Toggle hidden/shown
        if ctrl.try_recv().is_ok() {
            st.hidden = !st.hidden;
            st.text.clear();
            st.cursor = 0;
            st.caret_on = true;
            if st.hidden {
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
            unsafe {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
        }
    }

    unsafe {
        let _ = KillTimer(hwnd, TMR_ID);
        let _ = DestroyWindow(hwnd);
    }
}

// ─── Window proc ───────────────────────────────────────────────────────

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    qlog(format!("quicknote: wndproc msg=0x{:04x} wp={}", msg, wp.0));

    let s = || -> Option<&'static mut WndState> {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if ptr == 0 { None } else { Some(&mut *(ptr as *mut WndState)) }
    };

    match msg {
        WM_ACTIVATE => {
            if wp.0 as u32 == WA_INACTIVE {
                let _ = ShowWindow(hwnd, SW_HIDE);
                if let Some(st) = s() { st.hidden = true; }
            }
            LRESULT(0)
        }
        WM_SYSCOMMAND if wp.0 as u32 == SC_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(st) = s() { st.hidden = true; }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            if let Some(st) = s() { st.hidden = true; }
            LRESULT(0)
        }

        WM_KEYDOWN => {
            if let Some(st) = s() {
                let vk = wp.0 as u16;
                if vk == VK_RETURN.0 {
                    save(&st.notes_dir, &st.text, st.bb);
                    st.text.clear(); st.cursor = 0; st.hidden = true;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else if vk == VK_ESCAPE.0 {
                    st.text.clear(); st.cursor = 0; st.hidden = true;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                } else if vk == VK_BACK.0 && st.cursor > 0 {
                    st.cursor -= 1; st.text.remove(st.cursor);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if vk == VK_DELETE.0 && st.cursor < st.text.len() {
                    st.text.remove(st.cursor);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if vk == VK_LEFT.0 && st.cursor > 0 {
                    st.cursor -= 1; let _ = InvalidateRect(hwnd, None, false);
                } else if vk == VK_RIGHT.0 && st.cursor < st.text.len() {
                    st.cursor += 1; let _ = InvalidateRect(hwnd, None, false);
                } else if vk == VK_HOME.0 {
                    st.cursor = 0; let _ = InvalidateRect(hwnd, None, false);
                } else if vk == VK_END.0 {
                    st.cursor = st.text.len(); let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_CHAR => {
            let ch = wp.0 as u32;
            if ch >= 0x20 && ch != 0x7f {
                if let Some(c) = char::from_u32(ch) {
                    if let Some(st) = s() {
                        st.text.insert(st.cursor, c);
                        st.cursor += 1;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }

        WM_TIMER if wp.0 as usize == TMR_ID => {
            if let Some(st) = s() {
                let now = tick_ms();
                if now.saturating_sub(st.last_tick) >= CARET_MS {
                    st.caret_on = !st.caret_on;
                    st.last_tick = now;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() {
                if let Some(st) = s() {
                    paint(hwnd, hdc, &st);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ─── Painting ──────────────────────────────────────────────────────────

fn paint(hwnd: HWND, hdc: HDC, st: &WndState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let cw = rc.right;
        let sc = GetDpiForWindow(hwnd) as f32 / 96.0;
        let ph = |v: i32| (v as f32 * sc) as i32;

        let bg = CreateSolidBrush(st.theme.background.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        let ir = RECT {
            left: ph(PAD), top: ph(16),
            right: cw - ph(PAD), bottom: ph(16 + INPUT_H),
        };
        let sf = CreateSolidBrush(st.theme.surface.to_colorref());
        let _ = FillRect(hdc, &ir, sf);
        let _ = DeleteObject(sf);
        let bd = CreateSolidBrush(st.theme.border.to_colorref());
        let _ = FrameRect(hdc, &ir, bd);
        let _ = DeleteObject(bd);

        let _ = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
        let mut tri = RECT {
            left: ir.left + ph(4), top: ir.top + ph(3),
            right: ir.right - ph(4), bottom: ir.bottom - ph(3),
        };
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, st.theme.text.to_colorref());

        let mut tw: Vec<u16> = st.text.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut tw, &mut tri as *mut RECT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        if st.caret_on && st.cursor <= st.text.len() {
            let before: Vec<u16> = st.text[..st.cursor].encode_utf16().collect();
            let mut sz = SIZE::default();
            if !before.is_empty() {
                let _ = GetTextExtentPoint32W(hdc, &before, &mut sz);
            }
            let cx = tri.left + sz.cx;
            let cw_px = ph(2).max(1);
            let cr = RECT { left: cx, top: ir.top + ph(3), right: cx + cw_px, bottom: ir.bottom - ph(3) };
            let cb = CreateSolidBrush(st.theme.text.to_colorref());
            let _ = FillRect(hdc, &cr, cb);
            let _ = DeleteObject(cb);
        }

        let hint = "Enter to save · Esc to cancel\0";
        let mut hw: Vec<u16> = hint.encode_utf16().collect();
        let mut hr = RECT {
            left: ph(PAD), top: ph(HINT_Y),
            right: cw - ph(PAD), bottom: ph(HINT_Y + 16),
        };
        let _ = SetTextColor(hdc, st.theme.text_muted.to_colorref());
        let _ = DrawTextW(hdc, &mut hw, &mut hr as *mut RECT, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }
}

// ─── Save ──────────────────────────────────────────────────────────────

fn save(notes_dir: &PathBuf, text: &str, bb: bool) {
    let text = text.trim();
    if text.is_empty() { return; }
    if let Err(e) = std::fs::create_dir_all(notes_dir) {
        eprintln!("mhd: quicknote — cannot create notes dir: {e}"); return;
    }
    let today = date_str();
    let path = notes_dir.join(format!("{today}.md"));
    let (h, m, s) = time_hms();
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

// ─── UTC helpers ───────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn date_str() -> String {
    let secs = epoch_secs();
    let days = (secs / 86400) as i64;
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if rem < diy { break; } rem -= diy; y += 1;
    }
    let mdays = if is_leap(y) { [31,29,31,30,31,30,31,31,30,31,30,31] } else { [31,28,31,30,31,30,31,31,30,31,30,31] };
    let mut m = 1u32;
    for &md in &mdays { if rem < md { break; } rem -= md; m += 1; }
    format!("{y:04}-{m:02}-{:02}", (rem + 1) as u32)
}

fn time_hms() -> (u32, u32, u32) {
    let s = epoch_secs() % 86400;
    ((s / 3600) as u32, ((s % 3600) / 60) as u32, (s % 60) as u32)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn tick_ms() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u32
}

// ─── Win32 helpers ─────────────────────────────────────────────────────

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

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_date_str_format() { let s = date_str(); assert_eq!(s.len(), 10); assert_eq!(&s[4..5], "-"); assert_eq!(&s[7..8], "-"); }
    #[test]
    fn test_date_epoch_0() {
        assert_eq!(date_str_from_epoch(0), "1970-01-01");
    }
    fn date_str_from_epoch(secs: u64) -> String {
        let days = (secs / 86400) as i64;
        let mut y = 1970i64;
        let mut rem = days;
        loop { let diy = if is_leap(y) { 366 } else { 365 }; if rem < diy { break; } rem -= diy; y += 1; }
        let mdays = if is_leap(y) { [31,29,31,30,31,30,31,31,30,31,30,31] } else { [31,28,31,30,31,30,31,31,30,31,30,31] };
        let mut m = 1u32;
        for &md in &mdays { if rem < md { break; } rem -= md; m += 1; }
        format!("{y:04}-{m:02}-{:02}", (rem + 1) as u32)
    }
}
