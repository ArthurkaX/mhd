//! Quick Note — hotkey-driven text note overlay.
//!
//! Small popup with a standard multiline EDIT control.
//! Enter saves to `~/.config/mhd/notes/YYYY-MM-DD.md`,
//! Shift+Enter inserts a new line, Escape cancels.
//! Second hotkey press closes the window without saving.
//! If blackbox is active, logs the note text.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::SendHwnd;
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

const W: i32 = 520;
const H: i32 = 220;
const PAD: i32 = 12;
const HEADER_H: i32 = 34;
const HINT_H: i32 = 24;
const CLS: &str = "mhd_quicknote_cls";
const EDIT_ID: usize = 100;
const WM_APP_SAVE: u32 = WM_APP;
const WM_APP_CANCEL: u32 = WM_APP + 1;
const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: u32 = 0x0001;
const EC_RIGHTMARGIN: u32 = 0x0002;

// ─── Static window handle ──────────────────────────────────────────────

/// Stores the HWND while Quick Note is open. `None` means no window.
/// The thread sets this after window creation; clears it on exit.
static CTRL: Mutex<Option<SendHwnd>> = Mutex::new(None);

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(theme: crate::core::native_theme::NativeTheme, notes_dir: PathBuf, bb: bool) {
    let mut guard = CTRL.lock().unwrap();
    if let Some(sh) = guard.as_ref() {
        // Second press while window is open → close it (no save)
        unsafe { let _ = PostMessageW(sh.0, WM_CLOSE, WPARAM(0), LPARAM(0)); }
        return;
    }
    // Mark as pending before spawning to prevent double-launch
    *guard = Some(SendHwnd(HWND::default()));
    drop(guard);

    std::thread::Builder::new()
        .name("quicknote".into())
        .spawn(move || {
            run(theme, notes_dir, bb);
        })
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

struct WndState {
    notes_dir: PathBuf,
    bb: bool,
    edit_hwnd: HWND,
    edit_brush: HBRUSH,
    theme: crate::core::native_theme::NativeTheme,
}

fn run(theme: crate::core::native_theme::NativeTheme, notes_dir: PathBuf, bb: bool) {
    let cls = to_utf16_z(CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hi,
            hbrBackground: HBRUSH(2 as _), // COLOR_WINDOW+1
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
            WS_POPUP,
            0, 0, W, H,
            None, None, hi, None,
        )
    } {
        Ok(h) => h,
        Err(_) => { *CTRL.lock().unwrap() = None; return; }
    };

    // ── Create EDIT child ──────────────────────────────────────────
    let edit_hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
            PAD, HEADER_H + PAD,
            W - 2 * PAD,
            H - HEADER_H - HINT_H - 2 * PAD,
            hwnd,
            HMENU(EDIT_ID as _),
            hi,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => { unsafe { let _ = DestroyWindow(hwnd); } *CTRL.lock().unwrap() = None; return; }
    };
    unsafe {
        let _ = SendMessageW(edit_hwnd, WM_SETFONT, WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as _), LPARAM(1));
        // A little breathing room inside the strict borderless EDIT.
        let _ = SendMessageW(edit_hwnd, EM_SETMARGINS, WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize), LPARAM((8 | (8 << 16)) as isize));
    }

    // Subclass EDIT to intercept Enter/Escape
    let old_edit_proc = unsafe {
        SetWindowLongPtrW(edit_hwnd, GWLP_WNDPROC, edit_wndproc as *const () as isize)
    };
    // Store old proc in EDIT's GWLP_USERDATA
    unsafe {
        SetWindowLongPtrW(edit_hwnd, GWLP_USERDATA, old_edit_proc);
    }

    // ── State ──────────────────────────────────────────────────────
    let edit_brush = unsafe { CreateSolidBrush(theme.surface.to_colorref()) };
    let mut st = WndState { notes_dir, bb, edit_hwnd, edit_brush, theme };
    let state_ptr: *mut WndState = &mut st;
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

    // ── Publish HWND so show() can find and close us ───────────────
    *CTRL.lock().unwrap() = Some(SendHwnd(hwnd));

    // ── Centre ─────────────────────────────────────────────────────
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - W) / 2;
    let y = wa.top + (wa.bottom - wa.top - H) / 2;
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), x, y, W, H, SWP_NOZORDER); }

    // ── Title ──────────────────────────────────────────────────────
    let today = date_str();
    let title = format!("Quick Note — {today}\0");
    let tw: Vec<u16> = title.encode_utf16().collect();
    unsafe { let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr())); }

    // ── Show + focus ───────────────────────────────────────────────
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        steal_focus(hwnd, edit_hwnd);
    }

    // ── Message loop (classic GetMessageW) ─────────────────────────
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // ── Thread exit cleanup ────────────────────────────────────────
    *CTRL.lock().unwrap() = None;
}

// ─── Window proc ───────────────────────────────────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let s = || -> Option<&'static mut WndState> {
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr == 0 { None } else { Some(&mut *(ptr as *mut WndState)) }
        }
    };

    match msg {
        WM_NCHITTEST => {
            let mut pt = POINT {
                x: (lp.0 as i16) as i32,
                y: ((lp.0 >> 16) as i16) as i32,
            };
            unsafe { let _ = ScreenToClient(hwnd, &mut pt); }
            if pt.y >= 0 && pt.y < HEADER_H {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }

        // ── Save (Enter pressed in EDIT) ───────────────────────────
        WM_APP_SAVE => {
            if let Some(st) = s() {
                let text = get_edit_text(st.edit_hwnd);
                save(&st.notes_dir, &text, st.bb);
            }
            unsafe { let _ = DestroyWindow(hwnd); }
            LRESULT(0)
        }

        // ── Cancel (Escape pressed in EDIT) ────────────────────────
        WM_APP_CANCEL => {
            unsafe { let _ = DestroyWindow(hwnd); }
            LRESULT(0)
        }

        // ── User clicked X, Alt+F4, or second hotkey press ─────────
        WM_CLOSE => {
            unsafe { let _ = DestroyWindow(hwnd); }
            LRESULT(0)
        }

        // ── Posted by DestroyWindow → posts WM_QUIT to message loop
        WM_DESTROY => {
            if let Some(st) = s() {
                unsafe { let _ = DeleteObject(st.edit_brush); }
            }
            unsafe { PostQuitMessage(0); }
            LRESULT(0)
        }

        // ── Paint background + hint text ───────────────────────────
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_invalid() {
                    if let Some(st) = s() {
                        paint(hwnd, hdc, st);
                    }
                    let _ = EndPaint(hwnd, &ps);
                }
            }
            LRESULT(0)
        }

        WM_CTLCOLOREDIT => {
            if let Some(st) = s() {
                let hdc = HDC(wp.0 as *mut _);
                unsafe {
                    let _ = SetBkMode(hdc, OPAQUE);
                    let _ = SetBkColor(hdc, st.theme.surface.to_colorref());
                    let _ = SetTextColor(hdc, st.theme.text.to_colorref());
                }
                return LRESULT(st.edit_brush.0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ─── EDIT subclass ────────────────────────────────────────────────────

extern "system" fn edit_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let old_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if old_ptr == 0 {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let old_proc: extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            std::mem::transmute(old_ptr);

        match msg {
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                if vk == VK_ESCAPE.0 {
                    if let Ok(parent) = GetParent(hwnd) {
                        let _ = PostMessageW(parent, WM_APP_CANCEL, WPARAM(0), LPARAM(0));
                    }
                    return LRESULT(0);
                } else if vk == VK_RETURN.0 {
                    let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                    let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                    if shift || ctrl {
                        return old_proc(hwnd, msg, wp, lp);
                    }
                    if let Ok(parent) = GetParent(hwnd) {
                        let _ = PostMessageW(parent, WM_APP_SAVE, WPARAM(0), LPARAM(0));
                    }
                    return LRESULT(0);
                }
            }
            _ => {}
        }

        old_proc(hwnd, msg, wp, lp)
    }
}

// ─── Painting (background + hint) ──────────────────────────────────────

fn paint(hwnd: HWND, hdc: HDC, st: &WndState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let bg = CreateSolidBrush(st.theme.background.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        // Thin strict border around the popup.
        let pen = CreatePen(PS_SOLID, 1, st.theme.border.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, rc.left, rc.top, rc.right, rc.bottom);

        // Header separator and edit field border.
        let _ = MoveToEx(hdc, rc.left + 1, HEADER_H, None);
        let _ = LineTo(hdc, rc.right - 1, HEADER_H);
        let edit_rc = RECT {
            left: PAD - 1,
            top: HEADER_H + PAD - 1,
            right: rc.right - PAD + 1,
            bottom: rc.bottom - HINT_H - PAD + 1,
        };
        let _ = Rectangle(hdc, edit_rc.left, edit_rc.top, edit_rc.right, edit_rc.bottom);
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);

        let _ = SetBkMode(hdc, TRANSPARENT);

        let mut title_rc = RECT { left: PAD, top: 0, right: rc.right - PAD, bottom: HEADER_H };
        let _ = SetTextColor(hdc, st.theme.text.to_colorref());
        draw_text(hdc, "Quick Note", &mut title_rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

        let mut hint_rc = RECT {
            left: rc.left + PAD,
            top: rc.bottom - HINT_H,
            right: rc.right - PAD,
            bottom: rc.bottom,
        };
        let _ = SetTextColor(hdc, st.theme.text_muted.to_colorref());
        draw_text(hdc, "Enter saves   ·   Shift+Enter newline   ·   Esc cancels", &mut hint_rc,
                  DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }
}

// ─── Text helpers ─────────────────────────────────────────────────────

fn draw_text(hdc: HDC, text: &str, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    let mut wz: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { let _ = DrawTextW(hdc, &mut wz, rc as *mut RECT, fmt); }
}

fn get_edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 { return String::new(); }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buf);
        buf.truncate(copied.max(0) as usize);
        String::from_utf16_lossy(&buf)
    }
}

// ─── Save ─────────────────────────────────────────────────────────────

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

    #[cfg(feature = "blackbox")]
    if bb {
        crate::blackbox::send_event(crate::blackbox::BlackboxEvent::QuickNote {
            ts: epoch_secs(),
            text: text.to_string(),
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

// ─── Win32 helpers ─────────────────────────────────────────────────────

unsafe fn steal_focus(hwnd: HWND, edit_hwnd: HWND) {
    let our_tid = GetCurrentThreadId();
    let fore_tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
    if fore_tid != our_tid && fore_tid != 0 {
        let _ = AttachThreadInput(fore_tid, our_tid, true);
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(edit_hwnd);
    if fore_tid != our_tid && fore_tid != 0 {
        let _ = AttachThreadInput(fore_tid, our_tid, false);
    }
}

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
    fn test_date_epoch_0() { assert_eq!(date_str_from_epoch(0), "1970-01-01"); }
    #[test]
    fn test_get_edit_text_empty() {
        assert!(get_edit_text(HWND::default()).is_empty());
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
