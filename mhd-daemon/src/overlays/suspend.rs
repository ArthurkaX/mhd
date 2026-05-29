//! Foreground-aware process suspension.
//!
//! A hotkey marks the current foreground process. When focus moves to another
//! process, the marked process is suspended. When one of its windows becomes
//! foreground again, the process is resumed before the user continues working.

#![allow(unsafe_op_in_unsafe_fn)]

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetAncestor, GetForegroundWindow, GetMessageW, GetWindowTextW,
    GetWindowThreadProcessId, TranslateMessage, WindowFromPoint, EVENT_SYSTEM_FOREGROUND, GA_ROOT,
    MSG, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

use crate::osd::OsdHandle;

const SYSTEM_PROCESS_LIMIT: u32 = 8;
const NOTIFY_MS: u32 = 1200;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process_handle: HANDLE) -> i32;
    fn NtResumeProcess(process_handle: HANDLE) -> i32;
}

static STATE: LazyLock<Arc<Mutex<SuspendState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(SuspendState::default())));
static EVENT_THREAD: OnceLock<()> = OnceLock::new();

#[derive(Debug, Clone)]
struct TargetProcess {
    pid: u32,
    display_name: String,
    suspended: bool,
}

#[derive(Default)]
struct SuspendState {
    target: Option<TargetProcess>,
    foreground_pid: Option<u32>,
    osd: Option<OsdHandle>,
}

/// Toggle "suspend on blur" for the current foreground process.
pub fn toggle_current(osd: &OsdHandle) {
    ensure_event_thread(osd.clone());

    {
        let mut st = STATE.lock().unwrap();
        st.osd = Some(osd.clone());
        if let Some(mut target) = st.target.take() {
            if target.suspended {
                let _ = set_process_suspended(target.pid, false);
                target.suspended = false;
            }
            osd.show_notify(format!("{}: Normal", target.display_name), NOTIFY_MS);
            return;
        }
    }

    let Some(info) = foreground_process_info() else {
        osd.show_notify("No foreground window", NOTIFY_MS);
        return;
    };

    if let Some(reason) = skip_reason(&info) {
        osd.show_notify(reason, NOTIFY_MS);
        return;
    }

    let mut st = STATE.lock().unwrap();
    st.osd = Some(osd.clone());
    st.foreground_pid = Some(info.pid);
    st.target = Some(TargetProcess {
        pid: info.pid,
        display_name: info.display_name.clone(),
        suspended: false,
    });
    osd.show_notify(format!("{}: Suspend on blur", info.display_name), NOTIFY_MS);
}

/// Resume a suspended target under the mouse before Windows finishes activation.
///
/// A suspended GUI process may not process the normal focus activation path, so
/// relying only on EVENT_SYSTEM_FOREGROUND can leave it asleep when clicked.
pub fn resume_if_window_at_point(pt: POINT) {
    let hwnd = unsafe {
        let hwnd = WindowFromPoint(pt);
        if hwnd == HWND::default() {
            return;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        if root == HWND::default() {
            hwnd
        } else {
            root
        }
    };

    let Some(pid) = window_pid(hwnd) else {
        return;
    };

    let mut notify: Option<String> = None;
    let osd = {
        let Ok(mut st) = STATE.try_lock() else {
            return;
        };
        let Some(target) = st.target.as_mut() else {
            return;
        };
        if target.pid != pid {
            return;
        }
        if !target.suspended {
            return;
        }

        if set_process_suspended(target.pid, false).is_ok() {
            target.suspended = false;
            notify = Some(format!("{}: Resumed", target.display_name));
        }

        st.osd.clone()
    };

    if let (Some(osd), Some(text)) = (osd, notify) {
        osd.show_notify(text, NOTIFY_MS);
    }
}

fn ensure_event_thread(osd: OsdHandle) {
    {
        let mut st = STATE.lock().unwrap();
        st.osd = Some(osd);
    }

    let _ = EVENT_THREAD.get_or_init(|| {
        std::thread::spawn(|| unsafe {
            let _hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(foreground_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
    });
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if hwnd == HWND::default() {
        return;
    }

    on_foreground_changed(window_pid(hwnd));
}

fn on_foreground_changed(pid: Option<u32>) {
    let mut notify: Option<String> = None;

    let osd = {
        let mut st = STATE.lock().unwrap();
        if st.foreground_pid == pid {
            return;
        }
        st.foreground_pid = pid;

        if let Some(target) = st.target.as_mut() {
            let should_suspend = Some(target.pid) != pid;
            if target.suspended != should_suspend {
                match set_process_suspended(target.pid, should_suspend) {
                    Ok(()) => {
                        target.suspended = should_suspend;
                        notify = Some(if should_suspend {
                            format!("{}: Suspended", target.display_name)
                        } else {
                            format!("{}: Resumed", target.display_name)
                        });
                    }
                    Err(_) => notify = Some(format!("{}: Access denied", target.display_name)),
                }
            }
        }
        st.osd.clone()
    };

    if let (Some(osd), Some(text)) = (osd, notify) {
        osd.show_notify(text, NOTIFY_MS);
    }
}

fn set_process_suspended(pid: u32, suspended: bool) -> windows::core::Result<()> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )?;
        let status = if suspended {
            NtSuspendProcess(handle)
        } else {
            NtResumeProcess(handle)
        };
        let _ = CloseHandle(handle);
        ntstatus_to_result(status)
    }
}

fn ntstatus_to_result(status: i32) -> windows::core::Result<()> {
    if status >= 0 {
        Ok(())
    } else {
        Err(windows::core::Error::from_win32())
    }
}

#[derive(Debug)]
struct ForegroundProcessInfo {
    pid: u32,
    exe_name: String,
    display_name: String,
}

fn foreground_process_info() -> Option<ForegroundProcessInfo> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() {
            return None;
        }
        let pid = window_pid(hwnd)?;
        let exe_name = process_exe_name(pid).unwrap_or_else(|| format!("PID {pid}"));
        let title = window_title(hwnd);
        let display_name = if title.is_empty() {
            exe_name.clone()
        } else {
            format!("{exe_name} - {title}")
        };
        Some(ForegroundProcessInfo {
            pid,
            exe_name,
            display_name,
        })
    }
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 160];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..len as usize])
        }
    }
}

fn process_exe_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 32768];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .or(Some(path))
    }
}

fn skip_reason(info: &ForegroundProcessInfo) -> Option<&'static str> {
    let exe = info.exe_name.to_ascii_lowercase();
    if info.pid <= SYSTEM_PROCESS_LIMIT || info.pid == unsafe { GetCurrentProcessId() } {
        return Some("System process skipped");
    }

    match exe.as_str() {
        "mhd.exe" | "explorer.exe" | "dwm.exe" | "csrss.exe" | "winlogon.exe" | "services.exe"
        | "lsass.exe" | "smss.exe" | "fontdrvhost.exe" => Some("System process skipped"),
        _ => None,
    }
}
