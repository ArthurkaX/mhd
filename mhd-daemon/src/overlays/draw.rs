//! Quick Draw — transparent drawing overlay.
//!
//! Pencil, rectangle, arrow tools with color picker.
//! Toolbar at top center. Escape to hide.
//! Drawings persist while window is hidden.
//! Module unloads when canvas is clean and window closed.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    COLORREF, CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DEFAULT_QUALITY, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    DeleteDC, DeleteObject, DrawTextW, FF_DONTCARE, FW_NORMAL, FillRect, GetDC, GetMonitorInfoW,
    HDC, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS, RGBQUAD,
    ReleaseDC, ScreenToClient, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetCursorPos, GetDesktopWindow, GetWindowLongPtrW, GetWindowRect, IDC_ARROW,
    IDC_CROSS, LoadCursorW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    RegisterClassW, SW_HIDE, SW_SHOW, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WM_ACTIVATE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_SETCURSOR, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
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
const COLORS: [(u8, u8, u8); 5] = [
    (0, 0, 0),
    (220, 50, 50),
    (50, 120, 220),
    (60, 180, 60),
    (240, 200, 40),
];

// ── State types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Pencil,
    Rect,
    Arrow,
    Circle,
}

#[derive(Clone, Copy)]
enum ToolbarAction {
    SetTool(Tool),
    SetColor(usize),
    Clear,
    Close,
    Undo,
}

struct State {
    tool: Tool,
    color_idx: usize,
    thickness: i32,
    width: i32,
    height: i32,
    pixels: Vec<u32>,         // canvas BGRA pixels
    backup: Vec<u32>,         // saved canvas for rubber-band
    history: Vec<Vec<u32>>,   // history for undo
    drag: Option<(i32, i32)>, // mouse-down start for rect/arrow
    dirty: bool,              // true if any drawing exists
    hidden: bool,
    action: Option<ToolbarAction>,
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
        unsafe {
            let _ = SetEvent(self.ev.0);
        }
    }
}

// ── Public API ─────────────────────────────────────────────────────────

pub fn show(theme: NativeTheme) {
    let mut g = CTRL.lock().unwrap();
    if let Some(ref ctrl) = *g {
        unsafe {
            let _ = SetEvent(ctrl.ev.0);
        }
        return;
    }
    let ev = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = Ctrl {
        ev: SafeHandle(ev),
        dying: dying.clone(),
    };
    let cev = c.ev.clone();
    let cdy = c.dying.clone();
    *g = Some(c);
    drop(g);
    std::thread::Builder::new()
        .name("mhd-quickdraw".into())
        .spawn(move || thread_main(cev, cdy, theme))
        .ok();
}

// ── Helpers ────────────────────────────────────────────────────────────

fn sc(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

fn work_rect() -> RECT {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
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
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

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
    unsafe {
        let _ = RegisterClassW(&wc);
    }

    let wr = work_rect();
    let w = wr.right - wr.left;
    let h = wr.bottom - wr.top;

    // Use Layered + Topmost + Toolwindow. DO NOT use WS_EX_TRANSPARENT or WS_EX_NOACTIVATE
    // because we want mouse input and keyboard focus for ESC!
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            wr.left,
            wr.top,
            w,
            h,
            None,
            None,
            hinst,
            None,
        )
    } {
        Ok(hw) => hw,
        Err(_) => return,
    };

    let s = sc(hwnd);
    let pixel_count = (w * h) as usize;
    // Fill with alpha=1 so DWM hit-testing doesn't pass the click through our canvas!
    let mut st = State {
        tool: Tool::Pencil,
        color_idx: 0,
        thickness: 3,
        width: w,
        height: h,
        pixels: vec![0x01000000; pixel_count],
        backup: vec![0x01000000; pixel_count],
        history: Vec::new(),
        drag: None,
        dirty: false,
        hidden: false,
        action: None,
    };

    let state_ptr: *mut State = &mut st;
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    paint(hwnd, &st, s);

    let event = hdl.0;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }

    loop {
        if dying.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        if res == WAIT_OBJECT_0 {
            // Toggle visibility externally
            if st.hidden {
                paint(hwnd, &st, s);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(hwnd);
                }
                st.hidden = false;
            } else if st.dirty {
                st.hidden = true;
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    let _ = ReleaseCapture();
                }
            } else {
                st.hidden = true;
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    let _ = ReleaseCapture();
                }
                break;
            }
        } else if res == windows::Win32::Foundation::WAIT_EVENT(1) {
            let mut msg = MSG::default();
            let mut quit = false;
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        quit = true;
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
            if quit {
                break;
            }

            if let Some(action) = st.action.take() {
                match action {
                    ToolbarAction::Close => {
                        st.hidden = true;
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = ReleaseCapture();
                        }
                    }
                    ToolbarAction::Clear => {
                        if st.history.len() >= 10 {
                            st.history.remove(0);
                        }
                        st.history.push(st.pixels.clone());
                        st.pixels.fill(0x01000000);
                        st.dirty = false;
                        st.drag = None;
                        paint(hwnd, &st, s);
                    }
                    ToolbarAction::Undo => {
                        if let Some(prev) = st.history.pop() {
                            st.pixels = prev;
                            st.drag = None;
                            st.dirty = !st.pixels.iter().all(|&p| p == 0x01000000);
                            paint(hwnd, &st, s);
                        }
                    }
                    ToolbarAction::SetTool(t) => {
                        st.tool = t;
                        paint(hwnd, &st, s);
                    }
                    ToolbarAction::SetColor(c) => {
                        st.color_idx = c;
                        paint(hwnd, &st, s);
                    }
                }
                if st.hidden && !st.dirty {
                    break;
                }
            }
        } else {
            break;
        }
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
        let _ = CloseHandle(event);
    }
    let mut g = CTRL.lock().unwrap();
    *g = None;
}

// ── Window proc ────────────────────────────────────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if !state_ptr.is_null() {
        let st = unsafe { &mut *state_ptr };
        let s = sc(hwnd);

        match msg {
            WM_ACTIVATE => {
                let active = wp.0 & 0xFFFF;
                if active == 0 {
                    // WA_INACTIVE
                    st.action = Some(ToolbarAction::Close);
                }
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u32;
                if vk == 0x1B {
                    // VK_ESCAPE
                    st.action = Some(ToolbarAction::Close);
                } else if vk == 0x5A {
                    // 'Z'
                    let ctrl = unsafe {
                        windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(
                            windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL.0 as i32,
                        )
                    } as u16
                        & 0x8000;
                    if ctrl != 0 {
                        st.action = Some(ToolbarAction::Undo);
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = (wp.0 >> 16) as i16;
                if delta > 0 {
                    st.thickness = (st.thickness + 1).min(20);
                } else if delta < 0 {
                    st.thickness = (st.thickness - 1).max(1);
                }
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let x = (lp.0 as i32) & 0xFFFF;
                let y = ((lp.0 as i32) >> 16) & 0xFFFF;
                let x = if x > 32767 { x - 65536 } else { x };
                let y = if y > 32767 { y - 65536 } else { y };

                let ty = (TOOL_Y as f32 * s) as i32;
                let th = (TOOL_H as f32 * s) as i32;
                if y >= ty && y < ty + th {
                    if let Some(action) = hit_toolbar(x, s) {
                        st.action = Some(action);
                    }
                    return LRESULT(0);
                }

                if x >= 0 && x < st.width && y >= 0 && y < st.height {
                    if st.history.len() >= 10 {
                        st.history.remove(0);
                    }
                    st.history.push(st.pixels.clone());

                    st.dirty = true;
                    match st.tool {
                        Tool::Pencil => {
                            fill_circle(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                x,
                                y,
                                st.thickness,
                                argb_pixel_from_idx(st.color_idx),
                            );
                            st.drag = Some((x, y));
                        }
                        Tool::Rect | Tool::Arrow => {
                            st.backup.copy_from_slice(&st.pixels);
                            let color = argb_pixel_from_idx(st.color_idx);
                            draw_rect_outline(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                x,
                                y,
                                x,
                                y,
                                color,
                                st.thickness,
                            );
                            st.drag = Some((x, y));
                        }
                        Tool::Circle => {
                            st.backup.copy_from_slice(&st.pixels);
                            let color = argb_pixel_from_idx(st.color_idx);
                            draw_circle_outline(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                x,
                                y,
                                x,
                                y,
                                color,
                                st.thickness,
                            );
                            st.drag = Some((x, y));
                        }
                    }
                    unsafe {
                        let _ = SetCapture(hwnd);
                    }
                    paint(hwnd, st, s);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let x = (lp.0 as i32) & 0xFFFF;
                let y = ((lp.0 as i32) >> 16) & 0xFFFF;
                let x = if x > 32767 { x - 65536 } else { x };
                let y = if y > 32767 { y - 65536 } else { y };

                if let Some((sx, sy)) = st.drag {
                    match st.tool {
                        Tool::Pencil => {
                            draw_thick_line(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                sx,
                                sy,
                                x,
                                y,
                                argb_pixel_from_idx(st.color_idx),
                                st.thickness,
                            );
                            st.drag = Some((x, y));
                            paint(hwnd, st, s);
                        }
                        Tool::Rect => {
                            st.pixels.copy_from_slice(&st.backup);
                            draw_rect_outline(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                sx,
                                sy,
                                x,
                                y,
                                argb_pixel_from_idx(st.color_idx),
                                st.thickness,
                            );
                            paint(hwnd, st, s);
                        }
                        Tool::Arrow => {
                            st.pixels.copy_from_slice(&st.backup);
                            draw_arrow(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                sx,
                                sy,
                                x,
                                y,
                                argb_pixel_from_idx(st.color_idx),
                                st.thickness,
                            );
                            paint(hwnd, st, s);
                        }
                        Tool::Circle => {
                            st.pixels.copy_from_slice(&st.backup);
                            draw_circle_outline(
                                &mut st.pixels,
                                st.width,
                                st.height,
                                sx,
                                sy,
                                x,
                                y,
                                argb_pixel_from_idx(st.color_idx),
                                st.thickness,
                            );
                            paint(hwnd, st, s);
                        }
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if st.drag.is_some() {
                    let x = (lp.0 as i32) & 0xFFFF;
                    let y = ((lp.0 as i32) >> 16) & 0xFFFF;
                    let x = if x > 32767 { x - 65536 } else { x };
                    let y = if y > 32767 { y - 65536 } else { y };

                    match st.tool {
                        Tool::Pencil => {}
                        Tool::Rect => {
                            if let Some((sx, sy)) = st.drag {
                                st.pixels.copy_from_slice(&st.backup);
                                draw_rect_outline(
                                    &mut st.pixels,
                                    st.width,
                                    st.height,
                                    sx,
                                    sy,
                                    x,
                                    y,
                                    argb_pixel_from_idx(st.color_idx),
                                    st.thickness,
                                );
                            }
                        }
                        Tool::Arrow => {
                            if let Some((sx, sy)) = st.drag {
                                st.pixels.copy_from_slice(&st.backup);
                                draw_arrow(
                                    &mut st.pixels,
                                    st.width,
                                    st.height,
                                    sx,
                                    sy,
                                    x,
                                    y,
                                    argb_pixel_from_idx(st.color_idx),
                                    st.thickness,
                                );
                            }
                        }
                        Tool::Circle => {
                            if let Some((sx, sy)) = st.drag {
                                st.pixels.copy_from_slice(&st.backup);
                                draw_circle_outline(
                                    &mut st.pixels,
                                    st.width,
                                    st.height,
                                    sx,
                                    sy,
                                    x,
                                    y,
                                    argb_pixel_from_idx(st.color_idx),
                                    st.thickness,
                                );
                            }
                        }
                    }
                    st.drag = None;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    paint(hwnd, st, s);
                }
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                let mut pt = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut pt);
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                let ty = (TOOL_Y as f32 * s) as i32;
                let th = (TOOL_H as f32 * s) as i32;
                if pt.y >= ty && pt.y < ty + th {
                    unsafe {
                        let _ = SetCursor(LoadCursorW(None, IDC_ARROW).unwrap_or_default());
                    }
                } else {
                    unsafe {
                        let _ = SetCursor(LoadCursorW(None, IDC_CROSS).unwrap_or_default());
                    }
                }
                return LRESULT(1);
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ── Toolbar hit-test ───────────────────────────────────────────────────

fn hit_toolbar(x: i32, s: f32) -> Option<ToolbarAction> {
    let sc = |v: i32| (v as f32 * s) as i32;
    let tw = toolbar_width(s);
    let wr = work_rect();
    let avail_w = wr.right - wr.left;
    let tx = (avail_w - tw) / 2;

    let gap = sc(BTN_GAP);
    let tbw = sc(TOOL_BTN_W);
    let ccw = sc(COL_BTN_W);
    let abw = sc(ACT_BTN_W);
    let sep = sc(SEP_W);

    let mut left = tx + gap;

    // Tools
    for ti in 0..4 {
        if x >= left && x < left + tbw {
            return Some(ToolbarAction::SetTool(match ti {
                0 => Tool::Pencil,
                1 => Tool::Rect,
                2 => Tool::Arrow,
                _ => Tool::Circle,
            }));
        }
        left += tbw + gap;
    }

    left += sep;

    // Colors
    for ci in 0..5 {
        if x >= left && x < left + ccw {
            return Some(ToolbarAction::SetColor(ci));
        }
        left += ccw + gap;
    }

    left += sep;

    // Actions
    if x >= left && x < left + abw {
        return Some(ToolbarAction::Undo);
    }
    left += abw + gap;
    if x >= left && x < left + abw {
        return Some(ToolbarAction::Clear);
    }
    left += abw + gap;
    if x >= left && x < left + abw {
        return Some(ToolbarAction::Close);
    }

    None
}

fn argb_pixel_from_idx(idx: usize) -> u32 {
    let (r, g, b) = COLORS[idx.min(COLORS.len() - 1)];
    argb_pixel(r, g, b)
}

// ── Painting ───────────────────────────────────────────────────────────

fn paint(hwnd: HWND, st: &State, s: f32) {
    let dc = unsafe { GetDC(hwnd) };
    if dc.is_invalid() {
        return;
    }
    let mem = unsafe { CreateCompatibleDC(dc) };
    if mem.is_invalid() {
        unsafe {
            let _ = ReleaseDC(hwnd, dc);
        }
        return;
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: st.width,
            biHeight: -st.height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = match unsafe { CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) } {
        Ok(d) => d,
        Err(_) => {
            unsafe {
                let _ = DeleteDC(mem);
                _ = ReleaseDC(hwnd, dc);
            }
            return;
        }
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
        let rc = RECT {
            left: tx,
            top: ty,
            right: tx + tw,
            bottom: ty + th,
        };
        unsafe {
            let _ = FillRect(mem, &rc, br);
            _ = DeleteObject(br);
        }
    }

    let font = make_font(13, s);
    let _of = unsafe { SelectObject(mem, font) };
    unsafe {
        let _ = SetBkMode(mem, TRANSPARENT);
        _ = SetTextColor(mem, COLORREF(0x00EEEEEEu32));
    }

    let gap = (BTN_GAP as f32 * s) as i32;
    let sep = (SEP_W as f32 * s) as i32;
    let mut left = tx + gap;

    // Tool buttons
    let tbw = (TOOL_BTN_W as f32 * s) as i32;
    let tbh = (BTN_H as f32 * s) as i32;
    let tby = (BTN_Y as f32 * s) as i32;
    let tool_names = ["✎", "□", "→", "◯"];

    for ti in 0..4 {
        let sel = match ti {
            0 => st.tool == Tool::Pencil,
            1 => st.tool == Tool::Rect,
            2 => st.tool == Tool::Arrow,
            _ => st.tool == Tool::Circle,
        };
        if sel {
            let br = unsafe { CreateSolidBrush(COLORREF(0x00B49664u32)) };
            let rc = RECT {
                left,
                top: tby,
                right: left + tbw,
                bottom: tby + tbh,
            };
            unsafe {
                let _ = FillRect(mem, &rc, br);
                _ = DeleteObject(br);
            }
            unsafe {
                let _ = SetTextColor(mem, COLORREF(0x00FFFFFFu32));
            }
        } else {
            unsafe {
                let _ = SetTextColor(mem, COLORREF(0x00CCCCCCu32));
            }
        }
        dw(
            mem,
            &mut to_utf16_z(tool_names[ti]),
            &mut rct(left, tby, left + tbw, tby + tbh),
            DT_CENTER | DT_SINGLELINE | DT_VCENTER,
        );
        left += tbw + gap;
    }

    left += sep;

    // Color buttons
    let ccw = (COL_BTN_W as f32 * s) as i32;
    let ccbh = (14.0 * s) as i32;
    let cby = tby + (tbh - ccbh) / 2;
    for ci in 0..5 {
        let (r, g, b) = COLORS[ci];
        let color_val: u32 = (r as u32) | ((g as u32) << 8) | ((b as u32) << 16);
        let br = unsafe { CreateSolidBrush(COLORREF(color_val)) };
        let border = if ci == st.color_idx {
            COLORREF(0x00FFFFFFu32)
        } else {
            COLORREF(0x00888888u32)
        };
        let _ = unsafe { SelectObject(mem, br) };
        let rc = RECT {
            left,
            top: cby,
            right: left + ccw,
            bottom: cby + ccbh,
        };
        unsafe {
            let _ = FillRect(mem, &rc, br);
            _ = DeleteObject(br);
        }
        // border
        let bbr = unsafe { CreateSolidBrush(border) };
        let brc = RECT {
            left,
            top: cby,
            right: left + ccw,
            bottom: cby + 1,
        };
        unsafe {
            let _ = FillRect(mem, &brc, bbr);
        }
        let brc = RECT {
            left,
            top: cby + ccbh - 1,
            right: left + ccw,
            bottom: cby + ccbh,
        };
        unsafe {
            let _ = FillRect(mem, &brc, bbr);
        }
        let brc = RECT {
            left,
            top: cby,
            right: left + 1,
            bottom: cby + ccbh,
        };
        unsafe {
            let _ = FillRect(mem, &brc, bbr);
        }
        let brc = RECT {
            left: left + ccw - 1,
            top: cby,
            right: left + ccw,
            bottom: cby + ccbh,
        };
        unsafe {
            let _ = FillRect(mem, &brc, bbr);
            _ = DeleteObject(bbr);
        }
        left += ccw + gap;
    }

    left += sep;

    // Action buttons
    let abw = (ACT_BTN_W as f32 * s) as i32;
    unsafe {
        let _ = SetTextColor(mem, COLORREF(0x00DDDDDDu32));
    }
    dw(
        mem,
        &mut to_utf16_z("↶"),
        &mut rct(left, tby, left + abw, tby + tbh),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    left += abw + gap;
    dw(
        mem,
        &mut to_utf16_z("C"),
        &mut rct(left, tby, left + abw, tby + tbh),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
    left += abw + gap;
    unsafe {
        let _ = SetTextColor(mem, COLORREF(0x00FF6655u32));
    }
    dw(
        mem,
        &mut to_utf16_z("✕"),
        &mut rct(left, tby, left + abw, tby + tbh),
        DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );

    // Blit
    unsafe {
        // Force alpha=255 for the toolbar rectangle so that GDI drawing doesn't cause alpha=0
        // (which breaks DWM mouse hit-testing for the toolbar buttons)
        let bits_slice = std::slice::from_raw_parts_mut(bits as *mut u32, st.pixels.len());
        for y in ty..(ty + th) {
            if y >= 0 && y < st.height {
                for x in tx..(tx + tw) {
                    if x >= 0 && x < st.width {
                        let idx = y as usize * st.width as usize + x as usize;
                        bits_slice[idx] |= 0xFF000000;
                    }
                }
            }
        }

        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let pt_dst = POINT {
            x: wr.left,
            y: wr.top,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: st.width,
            cy: st.height,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            HDC::default(),
            Some(&pt_dst),
            Some(&sz),
            mem,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe {
        let _ = DeleteObject(font);
        _ = SelectObject(mem, _ob);
        _ = DeleteObject(dib);
        _ = DeleteDC(mem);
        _ = ReleaseDC(hwnd, dc);
    }
}

fn toolbar_width(s: f32) -> i32 {
    let gap = (BTN_GAP as f32 * s) as i32;
    let sep = (SEP_W as f32 * s) as i32;
    let tbw = (TOOL_BTN_W as f32 * s) as i32;
    let ccw = (COL_BTN_W as f32 * s) as i32;
    let abw = (ACT_BTN_W as f32 * s) as i32;
    let margin = gap;
    margin * 2 + tbw * 4 + gap * 3 + sep + ccw * 5 + gap * 4 + sep + abw * 3 + gap * 2
}

// ── Drawing primitives ─────────────────────────────────────────────────

fn fill_circle(pixels: &mut [u32], w: i32, h: i32, cx: i32, cy: i32, radius: i32, color: u32) {
    let r2 = radius * radius;
    for y in (cy - radius)..=(cy + radius) {
        if y < 0 || y >= h {
            continue;
        }
        for x in (cx - radius)..=(cx + radius) {
            if x < 0 || x >= w {
                continue;
            }
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r2 {
                pixels[(y * w + x) as usize] = color;
            }
        }
    }
}

fn draw_thick_line(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
    radius: i32,
) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    loop {
        fill_circle(pixels, w, h, x, y, radius, color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_rect_outline(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
    radius: i32,
) {
    let x_min = x0.min(x1);
    let x_max = x0.max(x1);
    let y_min = y0.min(y1);
    let y_max = y0.max(y1);
    draw_thick_line(pixels, w, h, x_min, y_min, x_max, y_min, color, radius);
    draw_thick_line(pixels, w, h, x_max, y_min, x_max, y_max, color, radius);
    draw_thick_line(pixels, w, h, x_max, y_max, x_min, y_max, color, radius);
    draw_thick_line(pixels, w, h, x_min, y_max, x_min, y_min, color, radius);
}

fn draw_arrow(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
    radius: i32,
) {
    draw_thick_line(pixels, w, h, x0, y0, x1, y1, color, radius);
    // Arrowhead
    let angle = (y1 - y0) as f64;
    let len = ((x1 - x0) as f64).hypot(angle);
    if len < 5.0 {
        return;
    }
    let ux = (x1 - x0) as f64 / len;
    let uy = (y1 - y0) as f64 / len;

    let head_len = 12.0 + (radius as f64) * 3.0;

    let head_angle: f64 = 0.5; // ~28 degrees
    let c = head_angle.cos();
    let s2 = head_angle.sin();
    // Left
    let lx = (x1 as f64 - head_len * (ux * c - uy * s2)) as i32;
    let ly = (y1 as f64 - head_len * (uy * c + ux * s2)) as i32;
    draw_thick_line(pixels, w, h, x1, y1, lx, ly, color, radius);
    // Right
    let rx = (x1 as f64 - head_len * (ux * c + uy * s2)) as i32;
    let ry = (y1 as f64 - head_len * (uy * c - ux * s2)) as i32;
    draw_thick_line(pixels, w, h, x1, y1, rx, ry, color, radius);
}

fn draw_circle_outline(
    pixels: &mut [u32],
    w: i32,
    h: i32,
    cx: i32,
    cy: i32,
    x1: i32,
    y1: i32,
    color: u32,
    radius: i32,
) {
    let dx = x1 - cx;
    let dy = y1 - cy;
    let r = ((dx * dx + dy * dy) as f64).sqrt() as i32;
    if r <= 0 {
        fill_circle(pixels, w, h, cx, cy, radius, color);
        return;
    }

    let r_out = r + radius;
    let r_in = (r - radius).max(0);
    let r_out2 = r_out * r_out;
    let r_in2 = r_in * r_in;

    for y in (cy - r_out)..=(cy + r_out) {
        if y < 0 || y >= h {
            continue;
        }
        for x in (cx - r_out)..=(cx + r_out) {
            if x < 0 || x >= w {
                continue;
            }
            let dx2 = x - cx;
            let dy2 = y - cy;
            let d2 = dx2 * dx2 + dy2 * dy2;
            if d2 >= r_in2 && d2 <= r_out2 {
                pixels[(y * w + x) as usize] = color;
            }
        }
    }
}

// ── Misc helpers ───────────────────────────────────────────────────────

fn make_font(size: i32, s: f32) -> windows::Win32::Graphics::Gdi::HFONT {
    let h = (size as f32 * s) as i32;
    let name = to_utf16_z("Segoe UI");
    unsafe {
        CreateFontW(
            -h,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            PCWSTR::from_raw(name.as_ptr()),
        )
    }
}

fn dw(dc: HDC, text: &mut Vec<u16>, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe {
        let _ = DrawTextW(dc, text, rc as *mut RECT, fmt);
    }
}

fn rct(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}
