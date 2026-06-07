use std::sync::mpsc;
use std::thread;

use super::{BlackboxEvent, epoch_secs};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetMessageW, GetWindowLongPtrW, HMENU, MSG, PostThreadMessageW, RegisterClassW,
    SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WM_CREATE, WM_DESTROY, WM_POWERBROADCAST,
    WM_QUIT, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
};

const WTS_SESSION_LOCK_EVENT: u32 = 0x7;
const WTS_SESSION_UNLOCK_EVENT: u32 = 0x8;
const PBT_APMSUSPEND: usize = 0x0004;
const PBT_APMRESUMESUSPEND: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

struct ListenerState {
    tx: mpsc::Sender<BlackboxEvent>,
    track_locks: bool,
    track_suspend: bool,
}

pub struct SysEventsHandle {
    thread_id: u32,
    join: Option<thread::JoinHandle<()>>,
}

impl SysEventsHandle {
    pub fn shutdown(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(j) = self.join.take() {
            let _: () = j.join().unwrap_or(());
        }
    }
}

pub fn start(
    tx: mpsc::Sender<BlackboxEvent>,
    track_locks: bool,
    track_suspend: bool,
) -> SysEventsHandle {
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = thread::Builder::new()
        .name("blackbox-sys-events".into())
        .spawn(move || {
            run_message_loop(tx, track_locks, track_suspend, ready_tx);
        });

    match join {
        Ok(join) => {
            let thread_id = ready_rx.recv().unwrap_or(0);
            SysEventsHandle {
                thread_id,
                join: Some(join),
            }
        }
        Err(e) => {
            eprintln!("mhd: blackbox: cannot spawn sys-events thread: {e}");
            SysEventsHandle {
                thread_id: 0,
                join: None,
            }
        }
    }
}

fn run_message_loop(
    tx: mpsc::Sender<BlackboxEvent>,
    track_locks: bool,
    track_suspend: bool,
    ready_tx: mpsc::Sender<u32>,
) {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = windows::core::w!("mhd_blackbox_sys_events");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let mut state = Box::new(ListenerState {
            tx,
            track_locks,
            track_suspend,
        });
        let state_ptr = state.as_mut() as *mut ListenerState as *mut core::ffi::c_void;

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            windows::core::w!("mhd blackbox sys events"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinstance,
            Some(state_ptr),
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                eprintln!("mhd: blackbox: cannot create sys-events window: {e}");
                let _ = ready_tx.send(0);
                return;
            }
        };

        let _ = ready_tx.send(GetCurrentThreadId());

        if track_locks {
            if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
                eprintln!("mhd: blackbox: WTSRegisterSessionNotification failed: {e}");
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if track_locks {
            let _ = WTSUnRegisterSessionNotification(hwnd);
        }
        let _ = DestroyWindow(hwnd);
        drop(state);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let createstruct = lparam.0 as *const CREATESTRUCTW;
            if !createstruct.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*createstruct).lpCreateParams as isize);
                }
            }
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE => {
            if let Some(state) = listener_state(hwnd) {
                if state.track_locks {
                    let event = match wparam.0 as u32 {
                        WTS_SESSION_LOCK_EVENT => {
                            Some(BlackboxEvent::SystemLocked { ts: epoch_secs() })
                        }
                        WTS_SESSION_UNLOCK_EVENT => {
                            Some(BlackboxEvent::SystemUnlocked { ts: epoch_secs() })
                        }
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = state.tx.send(event);
                    }
                }
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            if let Some(state) = listener_state(hwnd) {
                if state.track_suspend {
                    let event = match wparam.0 {
                        PBT_APMSUSPEND => Some(BlackboxEvent::SystemSuspend { ts: epoch_secs() }),
                        PBT_APMRESUMESUSPEND | PBT_APMRESUMEAUTOMATIC => {
                            Some(BlackboxEvent::SystemResume { ts: epoch_secs() })
                        }
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = state.tx.send(event);
                    }
                }
            }
            LRESULT(1)
        }
        WM_DESTROY => LRESULT(0),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn listener_state(hwnd: HWND) -> Option<&'static ListenerState> {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ListenerState;
        ptr.as_ref()
    }
}
