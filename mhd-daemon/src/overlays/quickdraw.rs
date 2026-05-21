//! Quick Draw — transparent drawing overlay.
//!
//! Pencil, rectangle, arrow tools with color picker.
//! Toolbar at top center. Escape to hide.
//! Drawings persist while window is hidden.
//! Module unloads when canvas is clean and window closed.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE,
    WAIT_EVENT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DIB_RGB_COLORS,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_NORMAL, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    OUT_DEFAULT_PRECIS, RGBQUAD, TRANSPARENT, AC_SRC_ALPHA, AC_SRC_OVER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDesktopWindow,
    GetWindowRect, LoadCursorW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassW, SetCursor, ShowWindow, UpdateLayeredWindow,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, IDC_CROSS, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_SHOWNA,
    ULW_ALPHA, WM_ACTIVATE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_SETCURSOR,
    HTCLIENT,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WNDCLASSW, MSG,
};
use windows::core::PCWSTR;

use crate::native_theme::NativeTheme;
use crate::osd::to_utf16_z;

// ── Constants ──────────────────────────────────────────────────────────

const TOOL_H: i32 = 36;
const TOOL_Y: i32 = 8;
const BTN_H: i32 = 26;
const BTN_Y: i32 = TOOL_Y + (TOOL_H - BTN_H) / 2;
const BTN_GAP: i32 = 4;
const TOOL_BTN_W: i32 = 28;
const COL_BTN_W: i32 = 18;
const ACT_BTN_W: i32 = 28;
const SEP_W: i32 = 8;
const COLORS: [(u8, u8, u8); 5] = [(0,0,0), (220,50,50), (50,120,220), (60,180,60), (240,200,40)];

// ── State types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Pencil,
    Rect,
    Arrow,
}

struct State {
    tool: Tool,
    color_idx: usize,
    width: i32,
    height: i32,
    pixels: Vec<u32>,         // canvas BGRA pixels
    backup: Vec<u32>,         // saved canvas for rubber-band
    drag: Option<(i32, i32)>, // mouse-down start for rect/arrow
    dirty: bool,              // true if any drawing exists
    hidden: bool,
}

// ── Thread control ─────────────────────────────────────────────────────

static CTRL: Mutex<Option<Ctrl>> = Mutex::new(None);

#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct Ctrl {
    ev: SafeHandle,
    dying: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        self.dying.store(true, std::sync::atomic::Ordering::Release);
        unsafe { let _ = SetEvent(self.ev.0); }
    }
}

// ── Public API ─────────────────────────────────────────────────────────

pub fn show(theme: NativeTheme) {
    let mut g = CTRL.lock().unwrap();
    if let Some(ref ctrl) = *g {
        unsafe { let _ = SetEvent(ctrl.ev.0); }
        return;
    }
    let ev = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = Ctrl { ev: SafeHandle(ev), dying: dying.clone() };
    let cev = c.ev.clone();
    let cdy = c.dying.clone();
    *g = Some(c);
    drop(g);
    std::thread::Builder::new().name("mhd-quickdraw".into())
        .spawn(move || thread_main(cev, cdy, theme)).ok();
}

// ── Helpers ────────────────────────────────────────────────────────────

fn sc(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

fn work_rect() -> RECT {
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let hm = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(hm, &mut mi);
        mi.rcWork
    }
}

fn argb_pixel(r: u8, g: u8, b: u8) -> u32 {
    0xFF000000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}



// ── Thread main ────────────────────────────────────────────────────────

fn thread_main(hdl: SafeHandle, dying: Arc<std::sync::atomic::AtomicBool>, _theme: NativeTheme) {
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }

    let cls = to_utf16_z("mhd_qd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
        ..Default::default()
    };
    unsafe { let _ = RegisterClassW(&wc); }

    let wr = work_rect();
    let w = wr.right - wr.left;
    let h = wr.bottom - wr.top;

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(cls.as_ptr()), PCWSTR::null(),
            WS_POPUP, wr.left, wr.top, w, h,
            None, None, hinst, None,
        )
    } { Ok(hw) => hw, Err(_) => return, };

    let s = sc(hwnd);
    let pixel_count = (w * h) as usize;
    let mut st = State {
        tool: Tool::Pencil,
        color_idx: 0, // black
        width: w,
        height: h,
        pixels: vec![0u32; pixel_count],
        backup: vec![0u32; pixel_count],
        drag: None,
        dirty: false,
        hidden: false,
    };

    // Store state pointer for wndproc
    let state_ptr: *mut State = &mut st;
    unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
        hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA, state_ptr as isize); }

    paint(hwnd, &st, s);

    let event = hdl.0;
    unsafe { let _ = ShowWindow(hwnd, SW_SHOWNA); }

    loop {
        if dying.load(std::sync::atomic::Ordering::Acquire) { break; }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        match res {
            WAIT_OBJECT_0 => { // toggle
                if st.hidden {
                    // Re-show
                    paint(hwnd, &st, s);
                    unsafe { let _ = ShowWindow(hwnd, SW_SHOWNA); }
                    st.hidden = false;
                } else if st.dirty {
                    // Hide but keep state
                    st.hidden = true;
                    unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); }
                } else {
                    // Clean canvas, unload entirely
                    st.hidden = true;
                    unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); }
                    break; // exit thread, module unloads
                }
            }
            WAIT_EVENT(1) => {
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT { break; }
                        if msg.hwnd == hwnd {
                            if !msg_handler(hwnd, &msg, &mut st, s) {
                                st.hidden = true;
                                let _ = ShowWindow(hwnd, SW_HIDE); let _ = ReleaseCapture();
                                if !st.dirty { break; }
                            }
                        }
                    }
                }
                // Repaint if visible and any repaint-relevant messages were processed
                // Actually, paint-on-demand in msg_handler for drawing, and here only for ongoing preview
                if !st.hidden {
                    paint(hwnd, &st, s);
                }
            }
            _ => break,
        }
    }

    // Unload
    unsafe { let _ = DestroyWindow(hwnd); }
}

// ── Message handler ────────────────────────────────────────────────────

fn msg_handler(hwnd: HWND, msg: &MSG, st: &mut State, s: f32) -> bool {
    match msg.message {
        WM_KEYDOWN if msg.wParam.0 as u32 == 0x1B => { // Escape
            return false;
        }
        WM_ACTIVATE if msg.wParam.0 as u32 == 0 => { // focus loss
            return false;
        }
        WM_LBUTTONDOWN => {
            let x = (msg.lParam.0 as i32) & 0xFFFF;
            let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
            let scx = |v: i32| (v as f32 * s) as i32;

            // Check toolbar hit
            let ty = scx(BTN_Y);
            let th = scx(BTN_H);
            if y >= ty && y < ty + th {
                if let Some(action) = hit_toolbar(x, y, s) {
                    handle_toolbar_action(hwnd, st, action);
                    return true;
                }
                return true; // click on toolbar but no button — consume it
            }

            // Canvas hit — start drawing
            let cx = x;
            let cy = y;
            if cx >= 0 && cx < st.width && cy >= 0 && cy < st.height {
                st.dirty = true;
                match st.tool {
                    Tool::Pencil => {
                        set_pixel(&mut st.pixels, st.width, st.height, cx, cy, argb_pixel_from_idx(st.color_idx));
                        st.drag = Some((cx, cy));
                        paint(hwnd, st, s);
                    }
                    Tool::Rect | Tool::Arrow => {
                        // Save canvas for rubber-band
                        st.backup.copy_from_slice(&st.pixels);
                        let color = argb_pixel_from_idx(st.color_idx);
                        draw_rect_outline(&mut st.pixels, st.width, st.height, cx, cy, cx, cy, color);
                        st.drag = Some((cx, cy));
                        paint(hwnd, st, s);
                    }
                }
                unsafe { let _ = SetCapture(hwnd); }
            }
            return true;
        }
        WM_MOUSEMOVE => {
            let x = (msg.lParam.0 as i32) & 0xFFFF;
            let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
            if let Some((sx, sy)) = st.drag {
                match st.tool {
                    Tool::Pencil => {
                        draw_line(&mut st.pixels, st.width, st.height, sx, sy, x, y, argb_pixel_from_idx(st.color_idx));
                        st.drag = Some((x, y));
                        paint(hwnd, st, s);
                    }
                    Tool::Rect => {
                        st.pixels.copy_from_slice(&st.backup);
                        draw_rect_outline(&mut st.pixels, st.width, st.height, sx, sy, x, y, argb_pixel_from_idx(st.color_idx));
                        paint(hwnd, st, s);
                    }
                    Tool::Arrow => {
                        st.pixels.copy_from_slice(&st.backup);
                        draw_arrow(&mut st.pixels, st.width, st.height, sx, sy, x, y, argb_pixel_from_idx(st.color_idx));
                        paint(hwnd, st, s);
                    }
                }
            }
            return true;
        }
        WM_LBUTTONUP => {
            if st.drag.is_some() {
                let x = (msg.lParam.0 as i32) & 0xFFFF;
                let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
                match st.tool {
                    Tool::Pencil => {}
                    Tool::Rect => {
                        if let Some((sx, sy)) = st.drag {
                            st.pixels.copy_from_slice(&st.backup);
                            draw_rect_outline(&mut st.pixels, st.width, st.height, sx, sy, x, y, argb_pixel_from_idx(st.color_idx));
                        }
                    }
                    Tool::Arrow => {
                        if let Some((sx, sy)) = st.drag {
                            st.pixels.copy_from_slice(&st.backup);
                            draw_arrow(&mut st.pixels, st.width, st.height, sx, sy, x, y, argb_pixel_from_idx(st.color_idx));
                        }
                    }
                }
                st.drag = None;
                unsafe { let _ = ReleaseCapture(); }
                paint(hwnd, st, s);
            }
            return true;
        }
        WM_SETCURSOR => {
            let ty = (BTN_Y as f32 * s) as i32;
            let th = (BTN_H as f32 * s) as i32;
            let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;
            if msg.hwnd == hwnd && (msg.wParam.0 as u32 >> 16) as u32 == HTCLIENT {
                if y >= ty && y < ty + th {
                    unsafe { let _ = SetCursor(LoadCursorW(None, IDC_ARROW).unwrap_or_default()); }
                } else {
                    unsafe { let _ = SetCursor(LoadCursorW(None, IDC_CROSS).unwrap_or_default()); }
                }
                return true;
            }
            return true;
        }
        _ => {}
    }
    true
}

// ── Toolbar hit-test ───────────────────────────────────────────────────

enum ToolbarAction {
    SetTool(Tool),
    SetColor(usize),
    Clear,
    Close,
}

fn hit_toolbar(x: i32, _y: i32, s: f32) -> Option<ToolbarAction> {
    let sc = |v: i32| (v as f32 * s) as i32;
    let bw = sc(BTN_GAP); // left margin
    let tbw = sc(TOOL_BTN_W);
    let ccw = sc(COL_BTN_W);
    let abw = sc(ACT_BTN_W);
    let gap = sc(BTN_GAP);
    let sep = sc(SEP_W);

    let mut left = bw;

    // Tools
    for ti in 0..3 {
        if x >= left && x < left + tbw { return Some(ToolbarAction::SetTool(match ti { 0 => Tool::Pencil, 1 => Tool::Rect, _ => Tool::Arrow })); }
        left += tbw + gap;
    }

    left += sep;

    // Colors
    for ci in 0..5 {
        if x >= left && x < left + ccw { return Some(ToolbarAction::SetColor(ci)); }
        left += ccw + gap;
    }

    left += sep;

    // Actions
    if x >= left && x < left + abw { return Some(ToolbarAction::Clear); }
    left += abw + gap;
    if x >= left && x < left + abw { return Some(ToolbarAction::Close); }

    None
}

fn handle_toolbar_action(hwnd: HWND, st: &mut State, action: ToolbarAction) {
    match action {
        ToolbarAction::SetTool(t) => st.tool = t,
        ToolbarAction::SetColor(i) => st.color_idx = i,
        ToolbarAction::Clear => {
            st.pixels.fill(0);
            st.dirty = false;
            st.drag = None;
            paint(hwnd, st, sc(hwnd));
        }
        ToolbarAction::Close => {
            st.hidden = true;
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); }
        }
    }
}

fn argb_pixel_from_idx(idx: usize) -> u32 {
    let (r, g, b) = COLORS[idx.min(COLORS.len() - 1)];
    argb_pixel(r, g, b)
}

// ── Window proc ────────────────────────────────────────────────────────

extern "system" fn wndproc(_h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(_h, msg, wp, lp) }
}

// ── Painting ───────────────────────────────────────────────────────────

fn paint(hwnd: HWND, st: &State, s: f32) {
    let dc = unsafe { GetDC(hwnd) };
    if dc.is_invalid() { return; }
    let mem = unsafe { CreateCompatibleDC(dc) };
    if mem.is_invalid() { unsafe { let _ = ReleaseDC(hwnd, dc); } return; }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: st.width, biHeight: -st.height, biPlanes: 1, biBitCount: 32,
            biCompression: 0, ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = match unsafe { CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) } {
        Ok(d) => d, Err(_) => { unsafe { let _ = DeleteDC(mem); _ = ReleaseDC(hwnd, dc); } return; }
    };
    let _ob = unsafe { SelectObject(mem, dib) };

    // Copy canvas pixels
    unsafe {
        std::ptr::copy_nonoverlapping(st.pixels.as_ptr(), bits as *mut u32, st.pixels.len());
    }

    // Draw toolbar background
    let tw = toolbar_width(s);
    let wr = work_rect();
    let avail_w = wr.right - wr.left;
    let tx = (avail_w - tw) / 2;
    let ty = (TOOL_Y as f32 * s) as i32;
    let th = (TOOL_H as f32 * s) as i32;
    {
        let br = unsafe { CreateSolidBrush(COLORREF(0xBB111111u32)) };
        let rc = RECT { left: tx, top: ty, right: tx + tw, bottom: ty + th };
        unsafe { let _ = FillRect(mem, &rc, br); _ = DeleteObject(br); }
    }

    let font = make_font(13, s);
    let _of = unsafe { SelectObject(mem, font) };
    unsafe { let _ = SetBkMode(mem, TRANSPARENT); _ = SetTextColor(mem, COLORREF(0x00EEEEEEu32)); }

    let gap = (BTN_GAP as f32 * s) as i32;
    let sep = (SEP_W as f32 * s) as i32;
    let mut left = tx + gap;

    // Tool buttons
    let tbw = (TOOL_BTN_W as f32 * s) as i32;
    let tbh = (BTN_H as f32 * s) as i32;
    let tby = (BTN_Y as f32 * s) as i32;
    let tool_names = ["✎", "□", "→"];

    for ti in 0..3 {
        let sel = match ti { 0 => st.tool == Tool::Pencil, 1 => st.tool == Tool::Rect, _ => st.tool == Tool::Arrow };
        if sel {
            let br = unsafe { CreateSolidBrush(COLORREF(0x44559999u32)) };
            let rc = RECT { left, top: tby, right: left + tbw, bottom: tby + tbh };
            unsafe { let _ = FillRect(mem, &rc, br); _ = DeleteObject(br); }
            unsafe { let _ = SetTextColor(mem, COLORREF(0x00FFFFFFu32)); }
        } else {
            unsafe { let _ = SetTextColor(mem, COLORREF(0x00CCCCCCu32)); }
        }
        dw(mem, &mut to_utf16_z(tool_names[ti]), &mut rct(left, tby, left + tbw, tby + tbh), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
        left += tbw + gap;
    }

    left += sep;

    // Color buttons
    let ccw = (COL_BTN_W as f32 * s) as i32;
    let ccbh = (14.0 * s) as i32;
    let cby = tby + (tbh - ccbh) / 2;
    for ci in 0..5 {
        let (r, g, b) = COLORS[ci];
        let color_val: u32 = (b as u32) | ((g as u32) << 8) | ((r as u32) << 16);
        let br = unsafe { CreateSolidBrush(COLORREF(color_val)) };
        let border = if ci == st.color_idx { COLORREF(0x00FFFFFFu32) } else { COLORREF(0x00888888u32) };
        let _ = unsafe { SelectObject(mem, br) };
        let rc = RECT { left, top: cby, right: left + ccw, bottom: cby + ccbh };
        unsafe { let _ = FillRect(mem, &rc, br); _ = DeleteObject(br); }
        // border
        let bbr = unsafe { CreateSolidBrush(border) };
        let brc = RECT { left, top: cby, right: left + ccw, bottom: cby + 1 };
        unsafe { let _ = FillRect(mem, &brc, bbr); }
        let brc = RECT { left, top: cby + ccbh - 1, right: left + ccw, bottom: cby + ccbh };
        unsafe { let _ = FillRect(mem, &brc, bbr); }
        let brc = RECT { left, top: cby, right: left + 1, bottom: cby + ccbh };
        unsafe { let _ = FillRect(mem, &brc, bbr); }
        let brc = RECT { left: left + ccw - 1, top: cby, right: left + ccw, bottom: cby + ccbh };
        unsafe { let _ = FillRect(mem, &brc, bbr); _ = DeleteObject(bbr); }
        left += ccw + gap;
    }

    left += sep;

    // Action buttons
    let abw = (ACT_BTN_W as f32 * s) as i32;
    unsafe { let _ = SetTextColor(mem, COLORREF(0x00DDDDDDu32)); }
    dw(mem, &mut to_utf16_z("C"), &mut rct(left, tby, left + abw, tby + tbh), DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    left += abw + gap;
    unsafe { let _ = SetTextColor(mem, COLORREF(0x00FF6655u32)); }
    dw(mem, &mut to_utf16_z("✕"), &mut rct(left, tby, left + abw, tby + tbh), DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // Blit
    unsafe {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8, BlendFlags: 0,
            SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let pt_dst = POINT { x: wr.left, y: wr.top };
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE { cx: st.width, cy: st.height };
        let _ = UpdateLayeredWindow(hwnd, HDC::default(), Some(&pt_dst), Some(&sz),
                                    mem, Some(&pt_src), COLORREF(0), Some(&blend), ULW_ALPHA);
    }

    unsafe { let _ = DeleteObject(font); _ = SelectObject(mem, _ob); _ = DeleteObject(dib); _ = DeleteDC(mem); _ = ReleaseDC(hwnd, dc); }
}



fn toolbar_width(s: f32) -> i32 {
    let gap = (BTN_GAP as f32 * s) as i32;
    let sep = (SEP_W as f32 * s) as i32;
    let tbw = (TOOL_BTN_W as f32 * s) as i32;
    let ccw = (COL_BTN_W as f32 * s) as i32;
    let abw = (ACT_BTN_W as f32 * s) as i32;
    let margin = gap;
    margin * 2
        + tbw * 3 + gap * 2
        + sep
        + ccw * 5 + gap * 4
        + sep
        + abw * 2 + gap * 1
}

// ── Drawing primitives ─────────────────────────────────────────────────

fn set_pixel(pixels: &mut [u32], w: i32, _h: i32, x: i32, y: i32, color: u32) {
    if x >= 0 && x < w && y >= 0 {
        let idx = y as usize * w as usize + x as usize;
        if idx < pixels.len() {
            pixels[idx] = color;
        }
    }
}

fn draw_line(pixels: &mut [u32], w: i32, h: i32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    loop {
        if x >= 0 && x < w && y >= 0 && y < h {
            let idx = y as usize * w as usize + x as usize;
            if idx < pixels.len() { pixels[idx] = color; }
        }
        if x == x1 && y == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x += sx; }
        if e2 <= dx { err += dx; y += sy; }
    }
}

fn draw_rect_outline(pixels: &mut [u32], w: i32, h: i32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let x_min = x0.min(x1);
    let x_max = x0.max(x1);
    let y_min = y0.min(y1);
    let y_max = y0.max(y1);
    draw_line(pixels, w, h, x_min, y_min, x_max, y_min, color);
    draw_line(pixels, w, h, x_max, y_min, x_max, y_max, color);
    draw_line(pixels, w, h, x_max, y_max, x_min, y_max, color);
    draw_line(pixels, w, h, x_min, y_max, x_min, y_min, color);
}

fn draw_arrow(pixels: &mut [u32], w: i32, h: i32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    draw_line(pixels, w, h, x0, y0, x1, y1, color);
    // Arrowhead
    let angle = (y1 - y0) as f64;
    let len = ((x1 - x0) as f64).hypot(angle);
    if len < 5.0 { return; }
    let ux = (x1 - x0) as f64 / len;
    let uy = (y1 - y0) as f64 / len;
    let head_len = 12.0;
    let head_angle: f64 = 0.5; // ~28 degrees
    let c = head_angle.cos();
    let s2 = head_angle.sin();
    // Left
    let lx = (x1 as f64 - head_len * (ux * c - uy * s2)) as i32;
    let ly = (y1 as f64 - head_len * (uy * c + ux * s2)) as i32;
    draw_line(pixels, w, h, x1, y1, lx, ly, color);
    // Right
    let rx = (x1 as f64 - head_len * (ux * c + uy * s2)) as i32;
    let ry = (y1 as f64 - head_len * (uy * c - ux * s2)) as i32;
    draw_line(pixels, w, h, x1, y1, rx, ry, color);
}

// ── Misc helpers ───────────────────────────────────────────────────────

fn make_font(size: i32, s: f32) -> windows::Win32::Graphics::Gdi::HFONT {
    let h = (size as f32 * s) as i32;
    let name = to_utf16_z("Segoe UI");
    unsafe {
        CreateFontW(-h, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
            DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32, DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32, PCWSTR::from_raw(name.as_ptr()))
    }
}

fn dw(dc: HDC, text: &mut Vec<u16>, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe { let _ = DrawTextW(dc, text, rc as *mut RECT, fmt); }
}

fn rct(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}
