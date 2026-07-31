//! Foreground-aware process throttling.
//!
//! A hotkey marks the current foreground process. While one of its windows is
//! foreground the process runs normally; when focus moves to another process,
//! Windows power throttling is enabled for the marked process and its CPU
//! affinity is reduced to the last logical processor allowed by its current
//! affinity mask.

#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessAffinityMask, OpenProcess,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, ProcessPowerThrottling,
    QueryFullProcessImageNameW, SetProcessAffinityMask, SetProcessInformation,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetMessageW, GetWindowTextW,
    GetWindowThreadProcessId, MSG, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS,
};
use windows::core::PWSTR;

use crate::osd::OsdHandle;

const SYSTEM_PROCESS_LIMIT: u32 = 8;
const NOTIFY_MS: u32 = 1200;
const AGGRESSIVE_THROTTLE_MASK: u32 =
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;

static STATE: LazyLock<Arc<Mutex<ThrottleState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(ThrottleState::default())));
static EVENT_THREAD: OnceLock<()> = OnceLock::new();

#[derive(Debug, Clone)]
struct TargetProcess {
    pid: u32,
    display_name: String,
    throttled: bool,
    original_affinity: Option<usize>,
}

#[derive(Default)]
struct ThrottleState {
    targets: HashMap<u32, TargetProcess>,
    foreground_pid: Option<u32>,
    osd: Option<OsdHandle>,
}

/// Toggle "throttle on blur" for the current foreground process.
pub fn toggle_current(osd: &OsdHandle) {
    ensure_event_thread(osd.clone());

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

    if let Some(mut target) = st.targets.remove(&info.pid) {
        let _ = set_background_limits(&mut target, false);
        osd.show_notify(format!("{}: Normal", info.display_name), NOTIFY_MS);
        return;
    }

    st.targets.insert(
        info.pid,
        TargetProcess {
            pid: info.pid,
            display_name: info.display_name.clone(),
            throttled: false,
            original_affinity: None,
        },
    );
    osd.show_notify(
        format!("{}: Throttle on blur", info.display_name),
        NOTIFY_MS,
    );
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

    let pid = window_pid(hwnd);
    on_foreground_changed(pid);
}

fn on_foreground_changed(pid: Option<u32>) {
    let mut notify: Option<String> = None;

    let osd = {
        let mut st = STATE.lock().unwrap();
        if st.foreground_pid == pid {
            return;
        }
        st.foreground_pid = pid;

        for target in st.targets.values_mut() {
            let should_throttle = Some(target.pid) != pid;
            if target.throttled == should_throttle {
                continue;
            }

            match set_background_limits(target, should_throttle) {
                Ok(()) => {
                    target.throttled = should_throttle;
                    notify = Some(if should_throttle {
                        format!("{}: Throttled", target.display_name)
                    } else {
                        format!("{}: Normal", target.display_name)
                    });
                }
                Err(_) => notify = Some(format!("{}: Access denied", target.display_name)),
            }
        }

        st.osd.clone()
    };

    if let (Some(osd), Some(text)) = (osd, notify) {
        osd.show_notify(text, NOTIFY_MS);
    }
}

fn set_background_limits(target: &mut TargetProcess, enabled: bool) -> windows::core::Result<()> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            target.pid,
        )?;

        let aggressive_result = apply_throttling_state(handle, AGGRESSIVE_THROTTLE_MASK, enabled);
        let power_result = if aggressive_result.is_err() && enabled {
            apply_throttling_state(handle, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, true)
        } else {
            aggressive_result
        };
        let affinity_result = if enabled {
            apply_single_cpu_affinity(handle, target)
        } else {
            restore_affinity(handle, target)
        };

        let result = if enabled {
            power_result.or(affinity_result)
        } else {
            power_result.and(affinity_result)
        };
        let _ = CloseHandle(handle);
        result
    }
}

pub(crate) unsafe fn apply_throttling_state(
    handle: HANDLE,
    mask: u32,
    enabled: bool,
) -> windows::core::Result<()> {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: mask,
        StateMask: if enabled { mask } else { 0 },
    };
    SetProcessInformation(
        handle,
        ProcessPowerThrottling,
        &state as *const _ as *const _,
        std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
    )
}

unsafe fn apply_single_cpu_affinity(
    handle: HANDLE,
    target: &mut TargetProcess,
) -> windows::core::Result<()> {
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    GetProcessAffinityMask(handle, &mut process_mask, &mut system_mask)?;

    if process_mask == 0 {
        return Ok(());
    }

    if target.original_affinity.is_none() {
        target.original_affinity = Some(process_mask);
    }

    let last_allowed_cpu = highest_bit(process_mask);
    SetProcessAffinityMask(handle, last_allowed_cpu)
}

unsafe fn restore_affinity(
    handle: HANDLE,
    target: &mut TargetProcess,
) -> windows::core::Result<()> {
    if let Some(mask) = target.original_affinity.take() {
        SetProcessAffinityMask(handle, mask)?;
    }
    Ok(())
}

fn highest_bit(mask: usize) -> usize {
    1usize << (usize::BITS - 1 - mask.leading_zeros())
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
