pub mod painter;

use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_EVENT, WAIT_OBJECT_0, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITORINFO, GetMonitorInfoW};
use windows::core::PCWSTR;

use crate::native_theme::NativeTheme;
use self::painter::paint_osd;
/// Re-export shared renderer primitives for backward compatibility.
/// All overlay windows access these via `crate::osd::*`.
pub use crate::renderer::{draw_rounded_rect, to_utf16_z, create_font,
    ShellRenderer, centered_position};

#[derive(Copy, Clone)]
struct ThreadHandle(HANDLE);
unsafe impl Send for ThreadHandle {}
unsafe impl Sync for ThreadHandle {}

impl ThreadHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

#[derive(Clone)]
pub struct OsdHandle {
    inner: Arc<Mutex<OsdInner>>,
}

struct OsdInner {
    queue: Vec<OsdCommand>,
    event: ThreadHandle,
}

enum OsdCommand {
    Show { value: u32, monitor_name: String },
    SetTheme(NativeTheme),
    Shutdown,
}

impl OsdHandle {
    pub fn show_brightness(&self, value: u32, monitor_name: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::Show { value, monitor_name });
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }

    pub fn set_theme(&self, theme: NativeTheme) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::SetTheme(theme));
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push(OsdCommand::Shutdown);
        unsafe {
            let _ = SetEvent(inner.event.raw());
        }
    }
}

pub fn start_osd() -> OsdHandle {
    let event = unsafe { CreateEventW(None, false, false, None).expect("CreateEventW for OSD") };

    let inner = Arc::new(Mutex::new(OsdInner {
        queue: Vec::new(),
        event: ThreadHandle(event),
    }));

    let inner2 = Arc::clone(&inner);
    std::thread::Builder::new()
        .name("mhd-osd".into())
        .spawn(move || {
            osd_thread(inner2);
        })
        .expect("spawn OSD thread");

    OsdHandle { inner }
}

const OSD_WIDTH_BASE: i32 = 380;
const OSD_HEIGHT_BASE: i32 = 112;
const HIDE_TIMEOUT_MS: u32 = 2000;
const HIDE_TIMER_ID: usize = 1;
const MSG_ARRIVED: WAIT_EVENT = WAIT_EVENT(1);

fn osd_thread(inner: Arc<Mutex<OsdInner>>) {
    let event = { inner.lock().unwrap().event.raw() };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cls_name = to_utf16_z("mhd_osd_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinstance: HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(osd_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(cls_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR::from_raw(cls_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            OSD_WIDTH_BASE,
            OSD_HEIGHT_BASE,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return,
    };

    let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
    let scale = dpi / 96.0;
    let osd_w = (OSD_WIDTH_BASE as f32 * scale) as i32;
    let osd_h = (OSD_HEIGHT_BASE as f32 * scale) as i32;

    unsafe {
        let _ = SetWindowPos(hwnd, None, 0, 0, osd_w, osd_h, SWP_NOMOVE | SWP_NOZORDER);
    }

    let mut _value: u32 = 50;
    let mut _monitor_name: String = String::new();
    let mut theme: NativeTheme = NativeTheme::default();

    let work = monitor_work_rect();

    loop {
        let res = unsafe {
            MsgWaitForMultipleObjects(Some(&[event]), false, INFINITE, QS_ALLINPUT)
        };

        match res {
            WAIT_OBJECT_0 => {
                let mut guard = inner.lock().unwrap();
                while let Some(cmd) = guard.queue.pop() {
                    match cmd {
                        OsdCommand::Show {
                            value: v,
                            monitor_name: n,
                        } => {
                            _value = v;
                            _monitor_name = n;
                            unsafe {
                                let _ = SetTimer(hwnd, HIDE_TIMER_ID, HIDE_TIMEOUT_MS, None);
                            }
                            paint_osd(
                                hwnd,
                                _value,
                                &_monitor_name,
                                &work,
                                osd_w,
                                osd_h,
                                scale,
                                &theme,
                            );
                            unsafe {
                                let _ = ShowWindow(hwnd, SW_SHOWNA);
                            }
                        }
                        OsdCommand::SetTheme(t) => {
                            theme = t;
                        }
                        OsdCommand::Shutdown => {
                            drop(guard);
                            unsafe {
                                DestroyWindow(hwnd).ok();
                            }
                            return;
                        }
                    }
                }
            }
            MSG_ARRIVED => {
                let mut msg = MSG::default();
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT {
                            return;
                        }
                        if msg.message == WM_TIMER && msg.wParam.0 == HIDE_TIMER_ID {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                            let _ = KillTimer(hwnd, HIDE_TIMER_ID);
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            _ => break,
        }
    }
}

extern "system" fn osd_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn monitor_work_rect() -> windows::Win32::Foundation::RECT {
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
