//! Quick Note — hotkey-driven text note overlay.
//!
//! Small popup with a standard multiline EDIT control.
//! Enter saves to `~/.config/mhd/notes/YYYY-MM-DD.md`,
//! Shift+Enter inserts a new line, Escape cancels.
//! Second hotkey press closes the window without saving.
//! If blackbox is active, logs the note text.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::SendHwnd;
use crate::config::path::home_dir;
use crate::core::native_theme::Argb;
use crate::win32::text_host::{TextHost, TextHostKind};

// ── Sink ───────────────────────────────────────────────────────────────

/// Where a Quick Note is persisted on save.
#[derive(Debug, Clone)]
pub enum NoteSink {
    /// Append to a daily Markdown file under the given directory.
    File(PathBuf),
    /// Write to the `notes` table in the embedded proxy's SQLite database.
    ProxyDb,
}

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
const WM_APP_SAVE: u32 = WM_APP;
const WM_APP_CANCEL: u32 = WM_APP + 1;

// ─── Static window handle ──────────────────────────────────────────────

/// Stores the HWND while Quick Note is open. `None` means no window.
/// The thread sets this after window creation; clears it on exit.
static CTRL: Mutex<Option<SendHwnd>> = Mutex::new(None);
static DEBUG_LOG: AtomicBool = AtomicBool::new(false);

pub fn set_debug_logging(enabled: bool) {
    DEBUG_LOG.store(enabled, Ordering::Release);
}

fn qn_log(msg: impl AsRef<str>) {
    if DEBUG_LOG.load(Ordering::Acquire) {
        println!("[quicknote] {}", msg.as_ref());
    }
}

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(theme: crate::core::native_theme::NativeTheme, sink: NoteSink, bb: bool) {
    qn_log(format!("show() theme={}", theme.name));
    let Ok(mut guard) = CTRL.lock() else {
        qn_log("show(): CTRL lock poisoned");
        return;
    };
    if let Some(sh) = guard.as_ref() {
        qn_log(format!(
            "show(): already open, posting WM_CLOSE to {:?}",
            sh.0
        ));
        // Second press while window is open → close it (no save)
        unsafe {
            let _ = PostMessageW(sh.0, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        return;
    }
    // Mark as pending before spawning to prevent double-launch
    *guard = Some(SendHwnd(HWND::default()));
    drop(guard);

    std::thread::Builder::new()
        .name("quicknote".into())
        .spawn(move || {
            qn_log("thread start");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(theme, sink, bb)
            }));
            if r.is_err() {
                qn_log("thread panic caught");
            }
            qn_log("thread end");
        })
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

struct WndState {
    sink: NoteSink,
    bb: bool,
    text_host: TextHost,
    edit_font: HFONT,
    theme: crate::core::native_theme::NativeTheme,
}

fn run(theme: crate::core::native_theme::NativeTheme, sink: NoteSink, bb: bool) {
    qn_log("run(): registering class");
    let cls = crate::renderer::to_utf16_z(CLS);
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
            W,
            H,
            None,
            None,
            hi,
            None,
        )
    } {
        Ok(h) => {
            qn_log(format!("run(): parent hwnd={h:?}"));
            h
        }
        Err(e) => {
            qn_log(format!("run(): CreateWindowEx parent failed: {e}"));
            if let Ok(mut g) = CTRL.lock() {
                *g = None;
            }
            return;
        }
    };

    // Apply background alpha as uniform window opacity. Standard child EDIT
    // controls cannot do true per-pixel alpha, but this makes glass themes
    // consistently translucent instead of silently ignoring alpha.
    if theme.background.a < 255 {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA);
        }
    }

    // ── Create EDIT child via TextHost ─────────────────────────────
    let brush_color = if theme.surface.a == 255 {
        theme.surface
    } else {
        theme.surface.blend_over(theme.background)
    };
    let text_host = TextHost::create(
        TextHostKind::RichEdit,
        hwnd,
        PAD,
        HEADER_H + PAD,
        W - 2 * PAD,
        H - HEADER_H - HINT_H - 2 * PAD,
        (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
        edit_wndproc,
        brush_color,
    )
    .expect("TextHost::create failed");
    text_host.set_margins(8, 8);
    // Larger font for comfortable editing
    let edit_font = crate::osd::create_font(-16, false, "Segoe UI");
    text_host.set_font(edit_font);
    // Set text colour directly (RichEdit ignores SetTextColor from WM_CTLCOLOREDIT)
    let surface_for_color = if theme.surface.a == 255 {
        theme.surface
    } else {
        theme.surface.blend_over(theme.background)
    };
    text_host.set_default_text_color(surface_for_color.contrasting_text_color());
    qn_log(format!("run(): edit hwnd={:?}", text_host.hwnd()));

    // ── State ──────────────────────────────────────────────────────
    let mut st = WndState {
        sink,
        bb,
        text_host,
        edit_font,
        theme,
    };
    let state_ptr: *mut WndState = &mut st;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    // ── Publish HWND so show() can find and close us ───────────────
    qn_log("run(): publishing hwnd");
    if let Ok(mut g) = CTRL.lock() {
        *g = Some(SendHwnd(hwnd));
    }

    // ── Centre ─────────────────────────────────────────────────────
    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - W) / 2;
    let y = wa.top + (wa.bottom - wa.top - H) / 2;
    unsafe {
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, W, H, SWP_NOZORDER);
    }

    // ── Title ──────────────────────────────────────────────────────
    let today = date_str();
    let title = format!("Quick Note — {today}\0");
    let tw: Vec<u16> = title.encode_utf16().collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr()));
    }

    // ── Show + focus ───────────────────────────────────────────────
    unsafe {
        qn_log("run(): show + focus");
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        st.text_host.focus(hwnd);
    }

    // ── Message loop (classic GetMessageW) ─────────────────────────
    qn_log("run(): entering message loop");
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    // ── Thread exit cleanup ────────────────────────────────────────
    qn_log("run(): message loop exited, clearing CTRL");
    if let Ok(mut g) = CTRL.lock() {
        *g = None;
    }
}

// ─── Window proc ───────────────────────────────────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(|| wndproc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let s = || -> Option<&'static mut WndState> {
        unsafe {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr == 0 {
                None
            } else {
                Some(&mut *(ptr as *mut WndState))
            }
        }
    };

    match msg {
        WM_NCHITTEST => {
            let mut pt = POINT {
                x: (lp.0 as i16) as i32,
                y: ((lp.0 >> 16) as i16) as i32,
            };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            if pt.y >= 0 && pt.y < HEADER_H {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }

        // ── Save (Enter pressed in EDIT) ───────────────────────────
        WM_APP_SAVE => {
            qn_log("wndproc: WM_APP_SAVE");
            if let Some(st) = s() {
                let text = st.text_host.get_text();
                save(&st.sink, &text, st.bb);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        // ── Cancel (Escape pressed in EDIT) ────────────────────────
        WM_APP_CANCEL => {
            qn_log("wndproc: WM_APP_CANCEL");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        // ── User clicked X, Alt+F4, or second hotkey press ─────────
        WM_CLOSE => {
            qn_log("wndproc: WM_CLOSE");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        // ── Posted by DestroyWindow → posts WM_QUIT to message loop
        WM_DESTROY => {
            qn_log("wndproc: WM_DESTROY");
            // Mark inactive immediately. This avoids a stale HWND if the hotkey
            // is pressed again while the window thread is still unwinding.
            if let Ok(mut g) = CTRL.lock() {
                *g = None;
            }
            // TextHost::drop will clean up the EDIT background brush.
            // The EDIT child is destroyed automatically by DestroyWindow.
            // Clean up the GDI font we created.
            if let Some(st) = s()
                && !st.edit_font.is_invalid()
            {
                unsafe {
                    let _ = DeleteObject(st.edit_font);
                }
            }
            unsafe {
                PostQuitMessage(0);
            }
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
            qn_log("wndproc: WM_CTLCOLOREDIT");
            if let Some(st) = s() {
                let hdc = HDC(wp.0 as *mut _);
                unsafe {
                    let _ = SetBkMode(hdc, OPAQUE);
                    let surface = st.theme.surface.blend_over(st.theme.background);
                    let _ = SetBkColor(hdc, surface.to_colorref());
                    // Use contrasting text colour (white on dark bg, black on light bg)
                    // to ensure readability regardless of theme's `text` value.
                    let text_color = surface.contrasting_text_color();
                    let _ = SetTextColor(hdc, text_color.to_colorref());
                }
                return LRESULT(st.text_host.brush().0 as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ─── EDIT subclass ────────────────────────────────────────────────────

extern "system" fn edit_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(|| edit_wndproc_inner(hwnd, msg, wp, lp)) {
        Ok(r) => r,
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn edit_wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let old_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if old_ptr == 0 {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let old_proc: WNDPROC = std::mem::transmute(old_ptr);

        if msg == WM_KEYDOWN {
            let vk = wp.0 as u16;
            if vk == VK_ESCAPE.0 {
                qn_log("edit: Escape");
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_CANCEL, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            } else if vk == VK_RETURN.0 {
                qn_log("edit: Return");
                let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                if shift || ctrl {
                    return CallWindowProcW(old_proc, hwnd, msg, wp, lp);
                }
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_SAVE, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            }
        }

        CallWindowProcW(old_proc, hwnd, msg, wp, lp)
    }
}

// ─── Painting (background + hint) ──────────────────────────────────────

fn paint(hwnd: HWND, hdc: HDC, st: &WndState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let bg_b = st.theme.background.blend_over(Argb::new(255, 0, 0, 0));
        let bg = CreateSolidBrush(bg_b.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        // Thin strict border around the popup.
        let pen_color = st.theme.border.blend_over(st.theme.background);
        let pen = CreatePen(PS_SOLID, 1, pen_color.to_colorref());
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
        let _ = Rectangle(
            hdc,
            edit_rc.left,
            edit_rc.top,
            edit_rc.right,
            edit_rc.bottom,
        );
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);

        let _ = SetBkMode(hdc, TRANSPARENT);

        let mut title_rc = RECT {
            left: PAD,
            top: 0,
            right: rc.right - PAD,
            bottom: HEADER_H,
        };
        let title_color = st.theme.background.contrasting_text_color();
        let _ = SetTextColor(hdc, title_color.to_colorref());
        draw_text(
            hdc,
            "Quick Note",
            &mut title_rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );

        let mut hint_rc = RECT {
            left: rc.left + PAD,
            top: rc.bottom - HINT_H,
            right: rc.right - PAD,
            bottom: rc.bottom,
        };
        let hint_color = st.theme.background.contrasting_text_color().with_alpha(160);
        let _ = SetTextColor(hdc, hint_color.to_colorref());
        draw_text(
            hdc,
            "Enter saves   ·   Shift+Enter newline   ·   Esc cancels",
            &mut hint_rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

// ─── Text helpers ─────────────────────────────────────────────────────

fn draw_text(hdc: HDC, text: &str, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    let mut wz: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = DrawTextW(hdc, &mut wz, rc as *mut RECT, fmt);
    }
}

// get_edit_text moved to crate::win32::text_host::get_edit_text

// ─── Save ─────────────────────────────────────────────────────────────

fn save(sink: &NoteSink, text: &str, bb: bool) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    #[cfg(not(feature = "blackbox"))]
    let _ = bb;
    match sink {
        NoteSink::File(notes_dir) => {
            if let Err(e) = std::fs::create_dir_all(notes_dir) {
                eprintln!("mhd: quicknote — cannot create notes dir: {e}");
                return;
            }
            let today = date_str();
            let path = notes_dir.join(format!("{today}.md"));
            let (h, m, s) = time_hms();
            let entry = format!("## {today} {h:02}:{m:02}:{s:02}\n{text}\n\n");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
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
        NoteSink::ProxyDb => {
            crate::core::llm_proxy::log_note(text);
        }
    }
}

// ─── UTC helpers ───────────────────────────────────────────────────────

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn date_str() -> String {
    let secs = epoch_secs();
    let days = (secs / 86400) as i64;
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if rem < diy {
            break;
        }
        rem -= diy;
        y += 1;
    }
    let mdays = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u32;
    for &md in &mdays {
        if rem < md {
            break;
        }
        rem -= md;
        m += 1;
    }
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

fn work_area() -> RECT {
    unsafe {
        let mut r = std::mem::zeroed();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        r
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_date_str_format() {
        let s = date_str();
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }
    #[test]
    fn test_date_epoch_0() {
        assert_eq!(date_str_from_epoch(0), "1970-01-01");
    }
    #[test]
    fn test_get_edit_text_empty() {
        assert!(crate::win32::text_host::get_edit_text(HWND::default()).is_empty());
    }

    fn date_str_from_epoch(secs: u64) -> String {
        let days = (secs / 86400) as i64;
        let mut y = 1970i64;
        let mut rem = days;
        loop {
            let diy = if is_leap(y) { 366 } else { 365 };
            if rem < diy {
                break;
            }
            rem -= diy;
            y += 1;
        }
        let mdays = if is_leap(y) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut m = 1u32;
        for &md in &mdays {
            if rem < md {
                break;
            }
            rem -= md;
            m += 1;
        }
        format!("{y:04}-{m:02}-{:02}", (rem + 1) as u32)
    }
}
