//! Vision Prompt — small multiline editor for the screenshot prompt.
//!
//! Similar to Quick Note, but edits a single string stored in
//! `~/.config/mhd/llm-proxy/settings.json` under `vision_prompt`.
//! Enter saves, Shift+Enter inserts a newline, Escape cancels.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_ESCAPE, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use crate::app::SendHwnd;
use crate::core::native_theme::Argb;
use crate::win32::text_host::{TextHost, TextHostKind};

// ─── Constants ────────────────────────────────────────────────────────

const W: i32 = 520;
const H: i32 = 220;
const PAD: i32 = 12;
const HEADER_H: i32 = 34;
const HINT_H: i32 = 24;
const CLS: &str = "mhd_vision_prompt_cls";
const WM_APP_SAVE: u32 = WM_APP;
const WM_APP_CANCEL: u32 = WM_APP + 1;

// ─── Static window handle ──────────────────────────────────────────────

static CTRL: Mutex<Option<SendHwnd>> = Mutex::new(None);
static DEBUG_LOG: AtomicBool = AtomicBool::new(false);

pub fn set_debug_logging(enabled: bool) {
    DEBUG_LOG.store(enabled, Ordering::Release);
}

fn vp_log(msg: impl AsRef<str>) {
    if DEBUG_LOG.load(Ordering::Acquire) {
        println!("[vision_prompt] {}", msg.as_ref());
    }
}

pub fn is_active() -> bool {
    CTRL.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn show(
    theme: crate::core::native_theme::NativeTheme,
    initial_prompt: String,
    notify: Option<(SendHwnd, u32)>,
) {
    vp_log(format!(
        "show() theme={} prompt_len={}",
        theme.name,
        initial_prompt.len()
    ));
    let Ok(mut guard) = CTRL.lock() else {
        vp_log("show(): CTRL lock poisoned");
        return;
    };
    if let Some(sh) = guard.as_ref() {
        vp_log(format!(
            "show(): already open, posting WM_CLOSE to {:?}",
            sh.0
        ));
        unsafe {
            let _ = PostMessageW(sh.0, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        return;
    }
    *guard = Some(SendHwnd(HWND::default()));
    drop(guard);

    std::thread::Builder::new()
        .name("vision_prompt".into())
        .spawn(move || {
            vp_log("thread start");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(theme, initial_prompt, notify)
            }));
            if r.is_err() {
                vp_log("thread panic caught");
            }
            vp_log("thread end");
        })
        .ok();
}

// ─── Window thread ─────────────────────────────────────────────────────

struct WndState {
    text_host: TextHost,
    edit_font: HFONT,
    theme: crate::core::native_theme::NativeTheme,
    notify: Option<(SendHwnd, u32)>,
}

fn run(
    theme: crate::core::native_theme::NativeTheme,
    initial_prompt: String,
    notify: Option<(SendHwnd, u32)>,
) {
    vp_log("run(): registering class");
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
            vp_log(format!("run(): parent hwnd={h:?}"));
            h
        }
        Err(e) => {
            vp_log(format!("run(): CreateWindowEx parent failed: {e}"));
            if let Ok(mut g) = CTRL.lock() {
                *g = None;
            }
            return;
        }
    };

    if theme.background.a < 255 {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), theme.background.a, LWA_ALPHA);
        }
    }

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
    let edit_font = crate::osd::create_font(-16, false, "Segoe UI");
    text_host.set_font(edit_font);
    text_host.set_text(&initial_prompt);
    let surface_for_color = if theme.surface.a == 255 {
        theme.surface
    } else {
        theme.surface.blend_over(theme.background)
    };
    text_host.set_default_text_color(surface_for_color.contrasting_text_color());
    vp_log(format!("run(): edit hwnd={:?}", text_host.hwnd()));

    let mut st = WndState {
        text_host,
        edit_font,
        theme,
        notify,
    };
    let state_ptr: *mut WndState = &mut st;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    if let Ok(mut g) = CTRL.lock() {
        *g = Some(SendHwnd(hwnd));
    }

    let wa = work_area();
    let x = wa.left + (wa.right - wa.left - W) / 2;
    let y = wa.top + (wa.bottom - wa.top - H) / 2;
    unsafe {
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, W, H, SWP_NOZORDER);
    }

    let title = "Vision Prompt\0";
    let tw: Vec<u16> = title.encode_utf16().collect();
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(tw.as_ptr()));
    }

    unsafe {
        vp_log("run(): show + focus");
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        st.text_host.focus(hwnd);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

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

        WM_APP_SAVE => {
            vp_log("wndproc: WM_APP_SAVE");
            if let Some(st) = s() {
                let text = st.text_host.get_text();
                let notify = st.notify;
                save(&text, notify);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_APP_CANCEL => {
            vp_log("wndproc: WM_APP_CANCEL");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_CLOSE => {
            vp_log("wndproc: WM_CLOSE");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            vp_log("wndproc: WM_DESTROY");
            if let Ok(mut g) = CTRL.lock() {
                *g = None;
            }
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
            vp_log("wndproc: WM_CTLCOLOREDIT");
            if let Some(st) = s() {
                let hdc = HDC(wp.0 as *mut _);
                unsafe {
                    let _ = SetBkMode(hdc, OPAQUE);
                    let surface = st.theme.surface.blend_over(st.theme.background);
                    let _ = SetBkColor(hdc, surface.to_colorref());
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
                vp_log("edit: Escape");
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(parent, WM_APP_CANCEL, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            } else if vk == VK_RETURN.0 {
                vp_log("edit: Return");
                let shift = (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
                if shift {
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

// ─── Painting ───────────────────────────────────────────────────────────

fn paint(hwnd: HWND, hdc: HDC, st: &WndState) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let bg_b = st.theme.background.blend_over(Argb::new(255, 0, 0, 0));
        let bg = CreateSolidBrush(bg_b.to_colorref());
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg);

        let pen_color = st.theme.border.blend_over(st.theme.background);
        let pen = CreatePen(PS_SOLID, 1, pen_color.to_colorref());
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, rc.left, rc.top, rc.right, rc.bottom);

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
            "Vision Prompt",
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

fn draw_text(hdc: HDC, text: &str, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    let mut wz: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = DrawTextW(hdc, &mut wz, rc as *mut RECT, fmt);
    }
}

// ─── Save ───────────────────────────────────────────────────────────────

fn save(text: &str, notify: Option<(SendHwnd, u32)>) {
    let text = text.trim();
    match llm_proxy::config::load_settings() {
        Ok(mut settings) => {
            settings.vision_prompt = text.to_string();
            if let Err(e) = llm_proxy::config::save_settings(&settings) {
                eprintln!("mhd: vision prompt — cannot save settings: {e}");
            } else {
                eprintln!("mhd: vision prompt updated");
                if let Some((hwnd, msg)) = notify {
                    unsafe {
                        let _ = PostMessageW(hwnd.0, msg, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("mhd: vision prompt — cannot load settings: {e}");
        }
    }
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
