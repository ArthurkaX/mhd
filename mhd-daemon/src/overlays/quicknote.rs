//! Quick Note — hotkey-driven text note overlay.
//!
//! Small popup with a standard multiline EDIT control.
//! Enter saves to `~/.config/mhd/notes/YYYY-MM-DD.md`,
//! Shift+Enter inserts a new line, Escape cancels.
//! If blackbox is active, logs a `quicknote` artefact.

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
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT};
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
const H: i32 = 200;
const PAD: i32 = 8;
const CLS: &str = "mhd_quicknote_cls";
const EDIT_ID: usize = 100;
const WM_APP_SAVE: u32 = WM_APP;
const WM_APP_CANCEL: u32 = WM_APP + 1;

// ─── Global toggle ─────────────────────────────────────────────────────

static CTRL: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(theme: crate::core::native_theme::NativeTheme, notes_dir: PathBuf, bb: bool) {
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
        .spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(theme, notes_dir, bb, d2, &rx);
            }));
            *CTRL.lock().unwrap() = None;
        })
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

struct WndState {
    _dying: Arc<AtomicBool>,
    notes_dir: PathBuf,
    bb: bool,
    hidden: bool,
    edit_hwnd: HWND,
    theme: crate::core::native_theme::NativeTheme,
}

fn run(
    theme: crate::core::native_theme::NativeTheme,
    notes_dir: PathBuf,
    bb: bool,
    dying: Arc<AtomicBool>,
    ctrl: &mpsc::Receiver<()>,
) {
    let cls = to_utf16_z(CLS);
    let hi: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default() }.into();

    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hi,
            hbrBackground: HBRUSH(2 as _), // COLOR_WINDOW+1 = white brush
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

    // ── Create EDIT child ──────────────────────────────────────────
    let edit_hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            windows::core::w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
            PAD,
            PAD,
            W - 2 * PAD,
            H - 2 * PAD - 20,
            hwnd,
            HMENU(EDIT_ID as _),
            hi,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => { unsafe { let _ = DestroyWindow(hwnd); } return; }
    };
    // Set default GUI font for the EDIT control
    unsafe {
        let _ = SendMessageW(edit_hwnd, WM_SETFONT, WPARAM(GetStockObject(DEFAULT_GUI_FONT).0 as _), LPARAM(1));
    }

    // Subclass the EDIT to intercept Enter/Escape
    let old_edit_proc = unsafe {
        Some(std::mem::transmute::<_, extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(
            SetWindowLongPtrW(edit_hwnd, GWLP_WNDPROC, edit_wndproc as *const () as isize)
        ))
    };
    // Store old proc in EDIT's GWLP_USERDATA
    unsafe {
        SetWindowLongPtrW(edit_hwnd, GWLP_USERDATA, old_edit_proc.unwrap() as isize);
    }

    // ── State ──────────────────────────────────────────────────────
    let mut st = WndState {
        _dying: dying.clone(),
        notes_dir,
        bb,
        hidden: false,
        edit_hwnd,
        theme,
    };
    let state_ptr: *mut WndState = &mut st;
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

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
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit_hwnd);
    }

    // ── Message loop ───────────────────────────────────────────────
    loop {
        if dying.load(Ordering::Acquire) { break; }

        let _ = unsafe {
            MsgWaitForMultipleObjects(None, false, INFINITE, QS_ALLINPUT)
        };

        // Toggle hidden/shown
        if ctrl.try_recv().is_ok() {
            st.hidden = !st.hidden;
            if st.hidden {
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            } else {
                unsafe {
                    // Clear EDIT content
                    let _ = SetWindowTextW(edit_hwnd, PCWSTR::null());
                    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(edit_hwnd);
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
        // Restore original EDIT wndproc before destroying
        if let Some(old_proc) = old_edit_proc {
            SetWindowLongPtrW(edit_hwnd, GWLP_WNDPROC, old_proc as isize);
        }
        let _ = DestroyWindow(hwnd);
    }
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
        WM_ACTIVATE => {
            if wp.0 as u32 == WA_INACTIVE {
                if let Some(st) = s() { st.hidden = true; }
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            }
            LRESULT(0)
        }
        WM_SYSCOMMAND if wp.0 as u32 == SC_CLOSE => {
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            if let Some(st) = s() { st.hidden = true; }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            if let Some(st) = s() { st.hidden = true; }
            LRESULT(0)
        }

        // ── Custom messages from EDIT subclass ─────────────────────
        WM_APP_SAVE => {
            if let Some(st) = s() {
                let text = get_edit_text(st.edit_hwnd);
                save(&st.notes_dir, &text, st.bb);
                st.hidden = true;
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); }
            }
            LRESULT(0)
        }
        WM_APP_CANCEL => {
            if let Some(st) = s() {
                st.hidden = true;
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
            LRESULT(0)
        }

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
            // Return 0 to indicate paint was handled
            LRESULT(0)
        }

        WM_CTLCOLOREDIT => {
            // Let EDIT use its default colors (no custom theming for now)
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ─── EDIT subclass ────────────────────────────────────────────────────

extern "system" fn edit_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        // Read old proc from this window's GWLP_USERDATA
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
                        // Let EDIT handle it (inserts \r\n)
                        return old_proc(hwnd, msg, wp, lp);
                    }
                    // Save and close
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

        // Fill background
        let bg = CreateSolidBrush(st.theme.background.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        // Draw hint text at the bottom
        let mut hint_rc = RECT {
            left: rc.left + PAD,
            top: rc.bottom - 18,
            right: rc.right - PAD,
            bottom: rc.bottom - 2,
        };
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, st.theme.text_muted.to_colorref());
        draw_text(hdc, "Enter · save    Shift+Enter · new line    Esc · cancel", &mut hint_rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }
}

// ─── Text helpers ──────────────────────────────────────────────────────

fn draw_text(hdc: HDC, text: &str, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    // Always pass a buffer with at least a NUL so DrawTextW never sees a
    // dangling pointer from an empty Rust slice.
    let mut wz: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { let _ = DrawTextW(hdc, &mut wz, rc as *mut RECT, fmt); }
}

fn get_edit_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buf);
        buf.truncate(copied.max(0) as usize);
        String::from_utf16_lossy(&buf)
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
