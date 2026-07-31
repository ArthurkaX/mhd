//! Quiet mode — one hotkey makes the machine quiet.
//!
//! Display off, CPU capped, fans calmed — but the machine keeps running its
//! workload. Any user input (mouse or keyboard) exits the mode and restores
//! the previous power scheme.
//!
//! The power-scheme mutation happens synchronously on the caller's thread
//! (kept fast); the EcoQoS sweep, the input poll loop and the cleanup all run
//! on the `mhd-quiet` thread, which is the sole owner of the cleanup path.

#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use windows::Win32::Foundation::{
    CloseHandle, HLOCAL, HWND, LPARAM, LocalFree, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Power::{
    ES_CONTINUOUS, ES_SYSTEM_REQUIRED, PowerDuplicateScheme, PowerWriteFriendlyName,
    SetThreadExecutionState,
};
use windows::Win32::System::ProcessStatus::EnumProcesses;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_SYSCOMMAND};
use windows::core::{GUID, PWSTR};

use crate::osd::OsdHandle;
use crate::overlays::cpu_plan::{
    GUID_COOLING_POLICY, GUID_MAX_PROC_STATE, GUID_MAX_PROC_STATE_CLASS1,
    GUID_MAX_PROC_STATE_CLASS2, GUID_PERF_BOOST_MODE, GUID_PROCESSOR_SUBGROUP, enumerate_schemes,
    get_active_scheme_guid, set_active_scheme, write_ac_value, write_dc_value,
};
use crate::overlays::throttle::apply_throttling_state;

// GUIDs for the sleep subgroup, declared locally (not present in cpu_plan.rs).
const GUID_SLEEP_SUBGROUP: GUID = GUID::from_u128(0x238c9fa8_0aad_41ed_83f4_97be242c8f20);
const GUID_STANDBY_TIMEOUT: GUID = GUID::from_u128(0x29f6c1db_86da_48c5_9fdb_f2b67b1f44da);

const QUIET_SCHEME_NAME: &str = "mhd Quiet";
const GRACE_MS: u64 = 1500; // first input window is ignored (see thread_main)
const TICK_MS: u64 = 250; // input poll interval
const NOTIFY_MS: u32 = 1500;
const SYSTEM_PROCESS_LIMIT: u32 = 8;

// System-critical processes that must never be EcoQoS-throttled. Same list as
// suspend.rs's skip_reason.
const SYSTEM_EXE_SKIP: &[&str] = &[
    "mhd.exe",
    "explorer.exe",
    "dwm.exe",
    "csrss.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "smss.exe",
    "fontdrvhost.exe",
];

/// Configuration snapshot for a quiet-mode session.
pub struct QuietConfig {
    pub cpu_max: u32,
    pub eco_qos: bool,
    pub exclude: Vec<String>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: LazyLock<Arc<Mutex<QuietState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(QuietState::default())));
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

#[derive(Default)]
struct QuietState {
    prev_scheme: Option<GUID>,
    eco_pids: Vec<u32>,
    osd: Option<OsdHandle>,
}

// ── Public API ───────────────────────────────────────────────────────

/// Whether a quiet session is currently active.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Toggle quiet mode: deactivate if active, otherwise activate.
pub fn toggle(cfg: &QuietConfig, osd: &OsdHandle) {
    if ACTIVE.load(Ordering::Acquire) {
        deactivate();
    } else {
        activate(cfg, osd);
    }
}

/// Deactivate quiet mode and restore everything. Idempotent; safe to call when
/// inactive (used on daemon shutdown).
pub fn deactivate() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }

    // Take the JoinHandle out of the mutex and drop the guard BEFORE joining:
    // the thread's cleanup path locks STATE, so holding either guard across the
    // join would deadlock.
    let handle = {
        let mut guard = THREAD.lock().unwrap();
        guard.take()
    };
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

// ── Enter (synchronous part, kept fast) ──────────────────────────────

fn activate(cfg: &QuietConfig, osd: &OsdHandle) {
    let Some(quiet_guid) = ensure_quiet_scheme() else {
        osd.show_notify("Quiet mode: scheme failed", NOTIFY_MS);
        return;
    };

    // Where to return to on exit. If the quiet scheme is already active — a
    // leftover from a daemon that died mid-session — restoring it would leave
    // the CPU capped for good, so fall back to any other scheme instead.
    let active = get_active_scheme_guid();
    let prev_scheme = if active != GUID::default() && active != quiet_guid {
        Some(active)
    } else {
        enumerate_schemes()
            .into_iter()
            .find(|(guid, _)| *guid != quiet_guid)
            .map(|(guid, _)| guid)
    };

    apply_quiet_values(&quiet_guid, cfg.cpu_max);
    set_active_scheme(quiet_guid);

    {
        let mut st = STATE.lock().unwrap();
        st.prev_scheme = prev_scheme;
        st.eco_pids.clear();
        st.osd = Some(osd.clone());
    }

    // Set the flag BEFORE spawning so the thread never sees it false after it
    // starts (its first action after setup would otherwise be an immediate
    // exit).
    ACTIVE.store(true, Ordering::Release);

    let eco_qos = cfg.eco_qos;
    let exclude = cfg.exclude.clone();
    let handle = std::thread::Builder::new()
        .name("mhd-quiet".into())
        .spawn(move || thread_main(eco_qos, exclude))
        .ok();

    match handle {
        Some(handle) => {
            let mut guard = THREAD.lock().unwrap();
            *guard = Some(handle);
            drop(guard);
            osd.show_notify("Quiet mode on", NOTIFY_MS);
        }
        None => {
            // Thread spawn failed — roll back synchronously.
            ACTIVE.store(false, Ordering::Release);
            if let Some(guid) = prev_scheme {
                set_active_scheme(guid);
            }
            let mut st = STATE.lock().unwrap();
            st.prev_scheme = None;
            st.eco_pids.clear();
            st.osd = None;
            osd.show_notify("Quiet mode: failed", NOTIFY_MS);
        }
    }
}

/// Find the `mhd Quiet` scheme, creating it (a clone of the active scheme,
/// renamed) if it does not exist yet.
fn ensure_quiet_scheme() -> Option<GUID> {
    if let Some((guid, _)) = enumerate_schemes()
        .into_iter()
        .find(|(_, name)| name == QUIET_SCHEME_NAME)
    {
        return Some(guid);
    }

    let source = get_active_scheme_guid();
    if source == GUID::default() {
        return None;
    }

    let mut ptr: *mut GUID = std::ptr::null_mut();
    let result: WIN32_ERROR =
        unsafe { PowerDuplicateScheme(None, &source as *const GUID, &mut ptr) };
    if result.0 != 0 || ptr.is_null() {
        return None;
    }
    let guid = unsafe { *ptr };
    unsafe {
        let _ = LocalFree(HLOCAL(ptr as *mut _));
    }

    // Rename the clone so it is recognizable (and reusable) next time.
    let name: Vec<u16> = QUIET_SCHEME_NAME.encode_utf16().collect();
    let bytes = unsafe { std::slice::from_raw_parts(name.as_ptr() as *const u8, name.len() * 2) };
    let result: WIN32_ERROR =
        unsafe { PowerWriteFriendlyName(None, &guid as *const GUID, None, None, bytes) };
    if result.0 != 0 {
        return None;
    }

    Some(guid)
}

/// Write the quiet values into the scheme, both AC and DC for every setting.
fn apply_quiet_values(guid: &GUID, cpu_max: u32) {
    for setting in [
        GUID_MAX_PROC_STATE,
        GUID_MAX_PROC_STATE_CLASS1,
        GUID_MAX_PROC_STATE_CLASS2,
    ] {
        write_ac_value(guid, &GUID_PROCESSOR_SUBGROUP, &setting, cpu_max);
        write_dc_value(guid, &GUID_PROCESSOR_SUBGROUP, &setting, cpu_max);
    }

    // Turbo off.
    write_ac_value(guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_BOOST_MODE, 0);
    write_dc_value(guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_BOOST_MODE, 0);

    // Passive cooling — prefer slowing the CPU over spinning the fans.
    write_ac_value(guid, &GUID_PROCESSOR_SUBGROUP, &GUID_COOLING_POLICY, 0);
    write_dc_value(guid, &GUID_PROCESSOR_SUBGROUP, &GUID_COOLING_POLICY, 0);

    // Never sleep while quiet.
    write_ac_value(guid, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_TIMEOUT, 0);
    write_dc_value(guid, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_TIMEOUT, 0);
}

// ── The `mhd-quiet` thread ───────────────────────────────────────────

fn thread_main(eco_qos: bool, exclude: Vec<String>) {
    // ES_SYSTEM_REQUIRED is a per-thread flag; this thread owns it for the
    // whole session and clears it during cleanup.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
    }

    if eco_qos {
        eco_qos_sweep(&exclude);
    }

    // Turn the display off LAST so the user is not left in the dark while the
    // slower setup above runs.
    turn_off_display();

    // Grace window: the keypress that enabled the mode also refreshed the
    // last-input timestamp, so re-baseline for the first GRACE_MS and only
    // start comparing after it. Otherwise the mode exits on key release.
    let started = std::time::Instant::now();
    let mut baseline: Option<u32> = None;
    loop {
        if !ACTIVE.load(Ordering::Acquire) {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(TICK_MS));

        let Some(input) = last_input_time() else {
            continue;
        };

        if (started.elapsed().as_millis() as u64) < GRACE_MS {
            // Reset the baseline every tick during the grace window and never
            // compare — the enabling keypress must not count as new input.
            baseline = Some(input);
            continue;
        }

        // Grace window over — freeze the baseline and break on any new input.
        match baseline {
            None => baseline = Some(input),
            Some(b) if input != b => break,
            Some(_) => {}
        }
    }

    cleanup();
}

/// Restore everything. The thread is the sole owner of this path.
fn cleanup() {
    // Revert EcoQoS. Take the pids out under the lock so the guard is never
    // held across the (slow) handle operations.
    let eco_pids = {
        let mut st = STATE.lock().unwrap();
        std::mem::take(&mut st.eco_pids)
    };
    for pid in eco_pids {
        revert_eco_qos(pid);
    }

    let (prev_scheme, osd) = {
        let mut st = STATE.lock().unwrap();
        (st.prev_scheme.take(), st.osd.clone())
    };

    if let Some(guid) = prev_scheme {
        set_active_scheme(guid);
    }

    // This thread owns the system-required flag — release it here.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS);
    }

    ACTIVE.store(false, Ordering::Release);

    if let Some(osd) = osd {
        osd.show_notify("Quiet mode off", NOTIFY_MS);
    }
}

// ── EcoQoS sweep ─────────────────────────────────────────────────────

/// Throttle every eligible process to EcoQoS and record the touched pids so
/// cleanup can revert them.
fn eco_qos_sweep(exclude: &[String]) {
    let current = unsafe { GetCurrentProcessId() };
    for pid in process_ids() {
        if pid <= SYSTEM_PROCESS_LIMIT || pid == current {
            continue;
        }

        let Some(name) = process_exe_name(pid) else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        if SYSTEM_EXE_SKIP.contains(&name.as_str())
            || exclude
                .iter()
                .any(|e| e.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }

        // Access denied is normal and expected for many processes — just skip.
        let Ok(handle) = (unsafe {
            OpenProcess(
                PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }) else {
            continue;
        };

        let applied = unsafe {
            apply_throttling_state(handle, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, true)
        }
        .is_ok();
        if applied {
            let mut st = STATE.lock().unwrap();
            st.eco_pids.push(pid);
        }
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

fn revert_eco_qos(pid: u32) {
    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    }) else {
        return;
    };
    unsafe {
        let _ = apply_throttling_state(handle, PROCESS_POWER_THROTTLING_EXECUTION_SPEED, false);
        let _ = CloseHandle(handle);
    }
}

fn process_ids() -> Vec<u32> {
    let mut buf: Vec<u32> = vec![0u32; 1024];
    loop {
        let cb = (buf.len() * std::mem::size_of::<u32>()) as u32;
        let mut needed: u32 = 0;
        let _ = unsafe { EnumProcesses(buf.as_mut_ptr(), cb, &mut needed) };
        let count = (needed as usize) / std::mem::size_of::<u32>();
        if count == 0 {
            return Vec::new();
        }
        if count < buf.len() {
            buf.truncate(count);
            return buf;
        }
        // Buffer too small — grow to the required size and retry.
        buf.resize(count.max(buf.len() * 2), 0);
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

// ── Input detection ──────────────────────────────────────────────────

fn last_input_time() -> Option<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            Some(info.dwTime)
        } else {
            None
        }
    }
}

// ── Display off ──────────────────────────────────────────────────────

fn turn_off_display() {
    // SendMessage(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2)
    unsafe {
        let _ = SendMessageW(
            HWND(0xFFFF as *mut c_void),
            WM_SYSCOMMAND,
            WPARAM(0xF170),
            LPARAM(2),
        );
    }
}
