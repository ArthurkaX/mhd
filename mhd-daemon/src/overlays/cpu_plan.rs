//! CPU Power Plan overlay — switch power plans and edit parking/freq settings.
//!
//! Ephemeral thread pattern (like volume_mixer / power).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::GUID;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, RECT,
    WAIT_EVENT, WAIT_OBJECT_0, WPARAM, LocalFree, WIN32_ERROR, HLOCAL};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromWindow, SelectObject, SetBkMode, SetTextColor,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    PowerEnumerate, PowerGetActiveScheme, PowerSetActiveScheme,
    PowerReadFriendlyName, PowerReadACValueIndex, PowerWriteACValueIndex,
    PowerReadDCValueIndex, PowerWriteDCValueIndex,
    CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION,
    ACCESS_SCHEME,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_CSTATUS_NEW_DATA,
    PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemCpuSetInformation,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    SYSTEM_CPU_SET_INFORMATION,
    RelationProcessorCore, CpuSetInformation,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, INFINITE};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture,
    TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetDesktopWindow,
    GetWindowRect, KillTimer, LoadCursorW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassW, SetForegroundWindow, SetTimer, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_SHOW,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    WM_ACTIVATE, WM_CHAR, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_TIMER,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WNDCLASSW, MSG,
};
use windows::core::PCWSTR;

use crate::native_theme::{Argb, NativeTheme};
use crate::osd::{to_utf16_z, OsdHandle};
use crate::constants::WM_MOUSELEAVE;

// ── Layout (unscaled, 96 DPI) ────────────────────────────────────────
const W: i32 = 380;
const PAD: i32 = 14;
const HDR_H: i32 = 32;
const PLAN_H: i32 = 28;
const SEP_H: i32 = 1;
const SEC_H: i32 = 22;        // section header height
const ROW_H: i32 = 26;        // settings row height
const MON_HDR_H: i32 = 26;    // monitor section header
const LOAD_ROW_H: i32 = 24;   // stress load controls row
const BAR_H: i32 = 18;        // per-core monitor row height
const CORE_BAR_H: i32 = 8;    // compact load bar inside each monitor row
const BAR_GAP: i32 = 3;       // gap between bars
const SUMMARY_H: i32 = 18;    // group summary line
const BTN_W: i32 = 80;        // apply button width
const BTN_H: i32 = 28;        // apply button height
const BTN_ROW_H: i32 = 36;    // apply button row height
const PAD_INNER: i32 = 8;     // inner padding between columns

const RADIUS: i32 = 8;

// ── Timer IDs ────────────────────────────────────────────────────────
const TIMER_MONITOR: usize = 1;

// ── GUIDs for processor power settings ───────────────────────────────
const GUID_PROCESSOR_SUBGROUP: GUID = GUID::from_u128(0x54533251_82be_4824_96c1_47b60b740d00);
const GUID_PARKING_MIN: GUID = GUID::from_u128(0x0cc5b647_c1df_4637_891a_dec35c318583);
const GUID_PARKING_MAX: GUID = GUID::from_u128(0xea062031_0e34_4ff1_9b6d_eb1059334028);
// Minimum processor performance state (% of max frequency the CPU can drop to)
const GUID_MIN_PROC_STATE: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964c);
const GUID_MIN_PROC_STATE_CLASS1: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964d);
const GUID_MIN_PROC_STATE_CLASS2: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964e);
// Maximum processor performance state (% of max frequency the CPU can go to)
const GUID_MAX_PROC_STATE: GUID = GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ec);
const GUID_MAX_PROC_STATE_CLASS1: GUID = GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ed);
const GUID_MAX_PROC_STATE_CLASS2: GUID = GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ee);
// Processor performance autonomous mode — 0=disabled, 1=enabled.
const GUID_PERF_AUTONOMOUS_MODE: GUID = GUID::from_u128(0x8baa4a8a_14c6_4451_8e8b_14bdbd197537);
// Processor performance boost mode — 0=disabled, 1=enabled, 2=aggressive, etc.
const GUID_PERF_BOOST_MODE: GUID = GUID::from_u128(0xbe337238_0d82_4146_a960_4f3749d470c7);

// ── Win32 error constant ─────────────────────────────────────────────
const ERROR_NO_MORE_ITEMS: u32 = 259;

// ── NtQuerySystemInformation imports (ntdll.dll) ────────────────────
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        system_information_class: u32,
        system_information: *mut std::ffi::c_void,
        system_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PerfInfo {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
}

// ── State types ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Field {
    AcMinCores,  DcMinCores,
    AcMaxCores,  DcMaxCores,
    AcMinFreq,   DcMinFreq,
    AcMaxFreq,   DcMaxFreq,
}

#[derive(Clone, Copy)]
struct PlanValues {
    /// 0–100: core parking minimum unparked cores (%)
    min_cores: u32,
    /// 0–100: core parking maximum unparked cores (%)
    max_cores: u32,
    /// 0–100: minimum processor performance state %
    min_freq: u32,
    /// 0–100: maximum processor performance state %
    max_freq: u32,
    /// Adaptive frequency scaling on/off
    freq_scale: bool,
    /// Turbo boost on/off
    turbo: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum StressLevel {
    Off,
    Threads(usize),
}

struct MonitorState {
    core_load: Vec<f32>,            // 0.0–1.0 per logical core
    core_parked: Vec<bool>,         // parked status per logical core
    core_freq_mhz: Vec<u32>,        // current MHz per logical core when available
    freq_sampler: Option<PdhFreqSampler>,
    p_cores: Vec<usize>,            // logical indices of P-cores
    e_cores: Vec<usize>,            // logical indices of E-cores
    stress_level: StressLevel,
    stress_stop: Arc<AtomicBool>,
    /// Snapshot of previous perf info for delta computation (None = not yet sampled)
    prev_perf: Option<Vec<PerfInfo>>,
}

struct PanelState {
    theme: NativeTheme,
    schemes: Vec<(GUID, String)>,
    active_plan_name: String,
    ac: PlanValues,
    dc: PlanValues,
    dirty: bool,
    focused: Option<Field>,
    edit_text: String,
    edit_select_all: bool,
    pos: POINT,
    monitor: MonitorState,
    stress_handles: Vec<std::thread::JoinHandle<()>>,
}

// ── Thread control ───────────────────────────────────────────────────

static CTRL: Mutex<Option<Ctrl>> = Mutex::new(None);

#[derive(Clone)]
struct SafeHandle(HANDLE);
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

struct Ctrl {
    ev: SafeHandle,
    dying: Arc<AtomicBool>,
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        self.dying.store(true, Ordering::Release);
        unsafe { let _ = SetEvent(self.ev.0); }
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Show (or re‑show) the CPU Power Plan overlay.
pub fn show_panel(theme: NativeTheme) {
    let mut g = CTRL.lock().unwrap();
    if let Some(ref ctrl) = *g {
        unsafe { let _ = SetEvent(ctrl.ev.0); }
        return;
    }
    let ev = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let dying = Arc::new(AtomicBool::new(false));
    let c = Ctrl { ev: SafeHandle(ev), dying: dying.clone() };
    let cev = c.ev.clone();
    let cdy = c.dying.clone();
    *g = Some(c);
    drop(g);
    std::thread::Builder::new().name("mhd-cpuplan".into())
        .spawn(move || thread_main(cev, cdy, theme)).ok();
}

/// Switch the active power plan and show OSD notification.
pub fn switch_plan(target: &str, plans: &[String], osd: &OsdHandle) {
    let schemes = enumerate_schemes();
    if schemes.is_empty() {
        return;
    }

    let new_guid = if target == "next" {
        let active_guid = get_active_scheme_guid();
        let order: Vec<GUID> = plans.iter()
            .filter_map(|n| schemes.iter().find(|(_, name)| name == n).map(|(g, _)| *g))
            .collect();
        if order.is_empty() {
            let active_pos = schemes.iter().position(|(g, _)| *g == active_guid).unwrap_or(0);
            let next = (active_pos + 1) % schemes.len();
            schemes[next].0
        } else {
            let cur_pos = order.iter().position(|g| *g == active_guid)
                .unwrap_or(order.len().wrapping_sub(1));
            order[(cur_pos + 1) % order.len()]
        }
    } else {
        match schemes.iter().find(|(_, n)| n == target) {
            Some((g, _)) => *g,
            None => return,
        }
    };

    set_active_scheme(new_guid);
    let name = schemes.iter().find(|(g, _)| *g == new_guid)
        .map(|(_, n)| n.as_str())
        .unwrap_or("Unknown");
    osd.show_notify(name, 2000);
}

/// Switch to the n-th enumerated scheme (for tray use).
pub fn switch_plan_by_index(index: usize, osd: &OsdHandle) {
    let schemes = enumerate_schemes();
    if let Some((guid, name)) = schemes.get(index) {
        set_active_scheme(*guid);
        osd.show_notify(name.as_str(), 2000);
    }
}

// ── CPU topology detection ──────────────────────────────────────────

/// Detect P-core and E-core logical indices using `GetLogicalProcessorInformationEx`.
/// If no hybrid architecture (no P/E cores), all cores go into `p_cores`.
fn detect_core_topology() -> (Vec<usize>, Vec<usize>) {
    // First call: get required buffer size
    let mut buf_size: u32 = 0;
    let result = unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut buf_size)
    };
    if result.is_err() && buf_size == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
    let result = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut buf_size,
        )
    };
    if result.is_err() {
        return (Vec::new(), Vec::new());
    }

    let mut p_cores: Vec<usize> = Vec::new();
    let mut e_cores: Vec<usize> = Vec::new();
    let mut offset: usize = 0;

    while offset < buf.len() {
        // SAFETY: We trust Windows filled in a valid SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX
        let info = unsafe { &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        let size = info.Size as usize;
        if size == 0 {
            break;
        }

        // Only core relationships
        if info.Relationship == RelationProcessorCore {
            // SAFETY: union access for Processor variant
            let proc = unsafe { &info.Anonymous.Processor };
            let efficiency = proc.EfficiencyClass;
            let group_count = proc.GroupCount as usize;

            // Extract logical processor indices from group masks
            for g in 0..group_count {
                let mask = if g < proc.GroupMask.len() {
                    proc.GroupMask[g].Mask
                } else {
                    0
                };
                let logical_indices = mask_to_indices(mask);
                let dest = if efficiency == 0 { &mut e_cores } else { &mut p_cores };
                dest.extend(logical_indices);
            }
        }

        offset += size;
    }

    // If no E-cores detected, put everything in P-cores (non-hybrid)
    if e_cores.is_empty() && !p_cores.is_empty() {
        let mut all = Vec::new();
        std::mem::swap(&mut all, &mut p_cores);
        e_cores = all;
        p_cores.clear();
    }

    (p_cores, e_cores)
}

fn mask_to_indices(mask: usize) -> Vec<usize> {
    let mut indices = Vec::new();
    for i in 0..(std::mem::size_of::<usize>() * 8) {
        if (mask >> i) & 1 != 0 {
            indices.push(i);
        }
    }
    indices
}

// ── Per-core load sampling via NtQuerySystemInformation ────────────

/// Read per-core idle/kernel/user performance counters.
fn read_perf_info() -> Option<Vec<PerfInfo>> {
    // First call to get required buffer size.
    // STATUS_INFO_LENGTH_MISMATCH (0xC0000004) is expected here — it's negative but still
    // populates buf_size with the required length. Only bail if buf_size is still 0.
    let mut buf_size: u32 = 0;
    unsafe {
        NtQuerySystemInformation(
            SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut buf_size,
        );
    };
    if buf_size == 0 {
        return None;
    }

    let count = (buf_size as usize) / std::mem::size_of::<PerfInfo>();
    let mut buf: Vec<PerfInfo> = vec![PerfInfo::default(); count];

    let result = unsafe {
        NtQuerySystemInformation(
            SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            buf_size,
            &mut buf_size,
        )
    };
    if result < 0 {
        return None;
    }

    Some(buf)
}

/// Compute per-core load from two snapshots (current - previous).
/// Returns load values in 0.0–1.0 range.
fn compute_loads(prev: &[PerfInfo], curr: &[PerfInfo]) -> Vec<f32> {
    let n = prev.len().min(curr.len());
    let mut loads = Vec::with_capacity(n);
    for i in 0..n {
        let prev_idle = prev[i].idle_time;
        let prev_busy = prev[i].kernel_time + prev[i].user_time + prev[i].dpc_time + prev[i].interrupt_time;
        let prev_total = prev_idle + prev_busy;

        let curr_idle = curr[i].idle_time;
        let curr_busy = curr[i].kernel_time + curr[i].user_time + curr[i].dpc_time + curr[i].interrupt_time;
        let curr_total = curr_idle + curr_busy;

        let total_delta = curr_total - prev_total;
        let busy_delta = curr_busy - prev_busy;

        let load = if total_delta > 0 {
            (busy_delta as f64 / total_delta as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        loads.push(load as f32);
    }
    loads
}

// ── Parked state detection ──────────────────────────────────────────

/// Read parked status for all logical processors using `GetSystemCpuSetInformation`.
fn read_parked_state() -> Vec<bool> {
    let mut buf_size: u32 = 0;
    // First call to get size
    unsafe {
        let _ = GetSystemCpuSetInformation(
            None,
            0,
            &mut buf_size,
            HANDLE::default(),
            0,
        );
    }

    if buf_size == 0 {
        return Vec::new();
    }

    let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
    let mut ret_len: u32 = 0;
    unsafe {
        let _ = GetSystemCpuSetInformation(
            Some(buf.as_mut_ptr() as *mut _),
            buf_size,
            &mut ret_len,
            HANDLE::default(),
            0,
        );
    }

    if ret_len == 0 {
        return Vec::new();
    }

    // We need to map by logical processor index, so build a map
    // First find max LP index to size the result
    let mut max_lp: usize = 0;
    let mut offset: usize = 0;
    while offset < ret_len as usize {
        // SAFETY: Windows filled in SYSTEM_CPU_SET_INFORMATION
        let info = unsafe { &*(buf.as_ptr().add(offset) as *const SYSTEM_CPU_SET_INFORMATION) };
        let size = info.Size as usize;
        if size == 0 { break; }

        if info.Type == CpuSetInformation {
            let cpu_set = unsafe { &info.Anonymous.CpuSet };
            let lp_idx = cpu_set.LogicalProcessorIndex as usize;
            if lp_idx > max_lp { max_lp = lp_idx; }
        }
        offset += size;
    }

    let mut parked = vec![false; max_lp + 1];

    let mut offset = 0;
    while offset < ret_len as usize {
        let info = unsafe { &*(buf.as_ptr().add(offset) as *const SYSTEM_CPU_SET_INFORMATION) };
        let size = info.Size as usize;
        if size == 0 { break; }

        if info.Type == CpuSetInformation {
            let cpu_set = unsafe { &info.Anonymous.CpuSet };
            let lp_idx = cpu_set.LogicalProcessorIndex as usize;
            // AllFlags bit 0 = PARKED
            let all_flags = unsafe { cpu_set.Anonymous1.AllFlags };
            let is_parked = (all_flags & 0x01) != 0;
            if lp_idx < parked.len() {
                parked[lp_idx] = is_parked;
            }
        }
        offset += size;
    }

    parked
}

// ── Per-core frequency sampling via powrprof ────────────────────────

/// Read current per-logical-processor MHz using standard Windows power APIs.
fn read_processor_frequencies(count_hint: usize) -> Option<Vec<u32>> {
    let count = count_hint
        .max(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let mut info = vec![PROCESSOR_POWER_INFORMATION::default(); count];
    let byte_len = (info.len() * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(info.as_mut_ptr() as *mut std::ffi::c_void),
            byte_len,
        )
    };
    if status.0 < 0 {
        return None;
    }

    Some(info.into_iter().map(|p| p.CurrentMhz).collect())
}

fn read_processor_base_mhz(count_hint: usize) -> Option<u32> {
    let count = count_hint
        .max(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let mut info = vec![PROCESSOR_POWER_INFORMATION::default(); count];
    let byte_len = (info.len() * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(info.as_mut_ptr() as *mut std::ffi::c_void),
            byte_len,
        )
    };
    if status.0 < 0 {
        return None;
    }

    info.iter()
        .find_map(|p| (p.MaxMhz > 0).then_some(p.MaxMhz))
        .or_else(|| info.iter().find_map(|p| (p.CurrentMhz > 0).then_some(p.CurrentMhz)))
}

struct PdhFreqSampler {
    query: isize,
    counters: Vec<isize>,
    base_mhz: u32,
}

impl PdhFreqSampler {
    fn new(count: usize, base_mhz: u32) -> Option<Self> {
        if count == 0 || base_mhz == 0 {
            return None;
        }

        let mut query = 0isize;
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }

        let mut counters = Vec::with_capacity(count);
        for i in 0..count {
            let path = to_utf16_z(&format!("\\Processor Information(0,{i})\\% Processor Performance"));
            let mut counter = 0isize;
            let status = unsafe {
                PdhAddEnglishCounterW(query, PCWSTR::from_raw(path.as_ptr()), 0, &mut counter)
            };
            if status == 0 {
                counters.push(counter);
            }
        }

        if counters.is_empty() {
            unsafe { let _ = PdhCloseQuery(query); }
            return None;
        }

        let sampler = Self { query, counters, base_mhz };
        let _ = sampler.collect();
        Some(sampler)
    }

    fn collect(&self) -> Option<Vec<u32>> {
        let status = unsafe { PdhCollectQueryData(self.query) };
        if status != 0 {
            return None;
        }

        let mut freqs = Vec::with_capacity(self.counters.len());
        for counter in &self.counters {
            let mut value = PDH_FMT_COUNTERVALUE::default();
            let status = unsafe {
                PdhGetFormattedCounterValue(*counter, PDH_FMT_DOUBLE, None, &mut value)
            };
            if status != 0
                || !matches!(value.CStatus, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA)
            {
                freqs.push(0);
                continue;
            }

            let perf_percent = unsafe { value.Anonymous.doubleValue }.max(0.0);
            let mhz = (self.base_mhz as f64 * perf_percent / 100.0).round() as u32;
            freqs.push(mhz);
        }

        Some(freqs)
    }
}

impl Drop for PdhFreqSampler {
    fn drop(&mut self) {
        unsafe { let _ = PdhCloseQuery(self.query); }
    }
}

fn read_effective_processor_frequencies(mon: &MonitorState) -> Option<Vec<u32>> {
    mon.freq_sampler
        .as_ref()
        .and_then(PdhFreqSampler::collect)
        .or_else(|| read_processor_frequencies(mon.core_freq_mhz.len()))
}

// ── Stress threads ─────────────────────────────────────────────────

fn spawn_stress_threads(count: usize, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let stp = stop.clone();
        let h = std::thread::Builder::new()
            .name("mhd-stress".into())
            .spawn(move || {
                while !stp.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
            .ok();
        if let Some(h) = h {
            handles.push(h);
        }
    }
    handles
}

fn stop_stress_threads(stop: &Arc<AtomicBool>, handles: &mut Vec<std::thread::JoinHandle<()>>) {
    stop.store(true, Ordering::Release);
    for h in handles.drain(..) {
        let _ = h.join();
    }
}

// ── Win32 Power helpers ──────────────────────────────────────────────

pub fn enumerate_schemes() -> Vec<(GUID, String)> {
    let mut schemes: Vec<(GUID, String)> = Vec::new();
    let mut index: u32 = 0;
    loop {
        let mut guid = GUID::default();
        let mut size = std::mem::size_of::<GUID>() as u32;
        let result: WIN32_ERROR = unsafe {
            PowerEnumerate(
                None,
                None,
                None,
                ACCESS_SCHEME,
                index,
                Some(&mut guid as *mut _ as *mut u8),
                &mut size,
            )
        };
        if result.0 != 0 {
            if result.0 == ERROR_NO_MORE_ITEMS {
                break;
            }
            index += 1;
            continue;
        }
        let name = read_scheme_name(&guid).unwrap_or_else(|| format!("Scheme {}", index));
        schemes.push((guid, name));
        index += 1;
    }
    schemes
}

fn read_scheme_name(guid: &GUID) -> Option<String> {
    unsafe {
        let mut buf_size: u32 = 0;
        let result: WIN32_ERROR = PowerReadFriendlyName(
            None,
            Some(guid as *const GUID),
            None,
            None,
            None,
            &mut buf_size,
        );
        if result.0 != 0 || buf_size == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
        let mut actual_size = buf_size;
        let result: WIN32_ERROR = PowerReadFriendlyName(
            None,
            Some(guid as *const GUID),
            None,
            None,
            Some(buf.as_mut_ptr()),
            &mut actual_size,
        );
        if result.0 != 0 {
            return None;
        }
        let len = buf.chunks(2)
            .take_while(|c| c.len() == 2 && !(c[0] == 0 && c[1] == 0))
            .count();
        let name = String::from_utf16_lossy(
            std::slice::from_raw_parts(
                buf.as_ptr() as *const u16,
                len,
            )
        );
        Some(name)
    }
}

pub fn get_active_scheme_guid() -> GUID {
    unsafe {
        let mut ptr: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut ptr).0 == 0 && !ptr.is_null() {
            let guid = *ptr;
            let _ = LocalFree(HLOCAL(ptr as *mut _));
            return guid;
        }
    }
    GUID::default()
}

fn set_active_scheme(guid: GUID) {
    unsafe {
        let _ = PowerSetActiveScheme(None, Some(&guid as *const GUID));
    }
}

fn read_ac_value(scheme: &GUID, sub: &GUID, setting: &GUID) -> u32 {
    unsafe {
        let mut val: u32 = 0;
        let _ = PowerReadACValueIndex(
            None,
            Some(scheme as *const GUID),
            Some(sub as *const GUID),
            Some(setting as *const GUID),
            &mut val,
        );
        val
    }
}

fn read_dc_value(scheme: &GUID, sub: &GUID, setting: &GUID) -> u32 {
    unsafe {
        let mut val: u32 = 0;
        let _ = PowerReadDCValueIndex(
            None,
            Some(scheme as *const GUID),
            Some(sub as *const GUID),
            Some(setting as *const GUID),
            &mut val,
        );
        val
    }
}

fn write_ac_value(scheme: &GUID, sub: &GUID, setting: &GUID, value: u32) {
    unsafe {
        let _ = PowerWriteACValueIndex(
            None,
            scheme as *const GUID,
            Some(sub as *const GUID),
            Some(setting as *const GUID),
            value,
        );
    }
}

fn write_dc_value(scheme: &GUID, sub: &GUID, setting: &GUID, value: u32) {
    unsafe {
        let _ = PowerWriteDCValueIndex(
            None,
            scheme as *const GUID,
            Some(sub as *const GUID),
            Some(setting as *const GUID),
            value,
        );
    }
}

// ── Read/write plan values (expanded) ────────────────────────────────

fn read_current_plan_values() -> (PlanValues, PlanValues) {
    let guid = get_active_scheme_guid();
    let min_freq_settings = [
        GUID_MIN_PROC_STATE,
        GUID_MIN_PROC_STATE_CLASS1,
        GUID_MIN_PROC_STATE_CLASS2,
    ];
    let max_freq_settings = [
        GUID_MAX_PROC_STATE,
        GUID_MAX_PROC_STATE_CLASS1,
        GUID_MAX_PROC_STATE_CLASS2,
    ];

    let ac = PlanValues {
        min_cores:  read_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN),
        max_cores:  read_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX),
        min_freq:   read_ac_setting_max(&guid, &min_freq_settings),
        max_freq:   read_ac_setting_min_nonzero(&guid, &max_freq_settings),
        freq_scale: read_power_setting_ac(&guid, &GUID_PERF_AUTONOMOUS_MODE) != 0,
        turbo:      read_power_setting_ac(&guid, &GUID_PERF_BOOST_MODE) != 0,
    };
    let dc = PlanValues {
        min_cores:  read_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN),
        max_cores:  read_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX),
        min_freq:   read_dc_setting_max(&guid, &min_freq_settings),
        max_freq:   read_dc_setting_min_nonzero(&guid, &max_freq_settings),
        freq_scale: read_power_setting_dc(&guid, &GUID_PERF_AUTONOMOUS_MODE) != 0,
        turbo:      read_power_setting_dc(&guid, &GUID_PERF_BOOST_MODE) != 0,
    };
    (ac, dc)
}

fn write_plan_values(ac: &PlanValues, dc: &PlanValues) {
    let guid = get_active_scheme_guid();
    write_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN, ac.min_cores);
    write_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX, ac.max_cores);
    write_ac_values(&guid, &[
        GUID_MIN_PROC_STATE,
        GUID_MIN_PROC_STATE_CLASS1,
        GUID_MIN_PROC_STATE_CLASS2,
    ], ac.min_freq);
    write_ac_values(&guid, &[
        GUID_MAX_PROC_STATE,
        GUID_MAX_PROC_STATE_CLASS1,
        GUID_MAX_PROC_STATE_CLASS2,
    ], ac.max_freq);
    write_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_AUTONOMOUS_MODE, if ac.freq_scale { 1 } else { 0 });
    write_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_BOOST_MODE, if ac.turbo { 1 } else { 0 });

    write_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN, dc.min_cores);
    write_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX, dc.max_cores);
    write_dc_values(&guid, &[
        GUID_MIN_PROC_STATE,
        GUID_MIN_PROC_STATE_CLASS1,
        GUID_MIN_PROC_STATE_CLASS2,
    ], dc.min_freq);
    write_dc_values(&guid, &[
        GUID_MAX_PROC_STATE,
        GUID_MAX_PROC_STATE_CLASS1,
        GUID_MAX_PROC_STATE_CLASS2,
    ], dc.max_freq);
    write_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_AUTONOMOUS_MODE, if dc.freq_scale { 1 } else { 0 });
    write_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PERF_BOOST_MODE, if dc.turbo { 1 } else { 0 });

    unsafe { let _ = PowerSetActiveScheme(None, Some(&guid as *const GUID)); }
}

fn read_ac_setting_max(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings.iter()
        .map(|setting| read_ac_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .max()
        .unwrap_or(0)
}

fn read_dc_setting_max(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings.iter()
        .map(|setting| read_dc_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .max()
        .unwrap_or(0)
}

fn read_ac_setting_min_nonzero(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings.iter()
        .map(|setting| read_ac_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .filter(|value| *value > 0)
        .min()
        .unwrap_or(0)
}

fn read_dc_setting_min_nonzero(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings.iter()
        .map(|setting| read_dc_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .filter(|value| *value > 0)
        .min()
        .unwrap_or(0)
}

fn write_ac_values(scheme: &GUID, settings: &[GUID], value: u32) {
    for setting in settings {
        write_ac_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting, value);
    }
}

fn write_dc_values(scheme: &GUID, settings: &[GUID], value: u32) {
    for setting in settings {
        write_dc_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting, value);
    }
}

fn read_power_setting_ac(scheme: &GUID, setting: &GUID) -> u32 {
    unsafe {
        let mut val: u32 = 0;
        let _ = PowerReadACValueIndex(
            None,
            Some(scheme as *const GUID),
            Some(&GUID_PROCESSOR_SUBGROUP as *const GUID),
            Some(setting as *const GUID),
            &mut val,
        );
        val
    }
}

fn read_power_setting_dc(scheme: &GUID, setting: &GUID) -> u32 {
    unsafe {
        let mut val: u32 = 0;
        let _ = PowerReadDCValueIndex(
            None,
            Some(scheme as *const GUID),
            Some(&GUID_PROCESSOR_SUBGROUP as *const GUID),
            Some(setting as *const GUID),
            &mut val,
        );
        val
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn work_area() -> RECT {
    unsafe {
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let hm = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(hm, &mut mi);
        mi.rcWork
    }
}

fn sc_from_hwnd(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

fn compute_total_h(sc: f32, p_count: usize, e_count: usize) -> i32 {
    let mut h = HDR_H + PLAN_H; // header + plan row
    // Settings section
    h += SEC_H + ROW_H + ROW_H; // CORES header + Min/Max Cores
    h += SEC_H + ROW_H + ROW_H; // FREQUENCY header + Min Speed + Max Speed
    h += SEC_H + ROW_H + ROW_H; // POWER FEATURES header + Freq Scaling + Turbo
    h += SEP_H; // separator

    // Monitor section
    h += MON_HDR_H; // monitor header
    h += LOAD_ROW_H; // load controls

    // P-core group
    if p_count > 0 {
        h += SEC_H; // group header
        h += (BAR_H + BAR_GAP) * p_count as i32 - BAR_GAP; // bars
        h += SUMMARY_H; // summary
    }

    // E-core group
    if e_count > 0 {
        h += SEC_H; // group header
        h += (BAR_H + BAR_GAP) * e_count as i32 - BAR_GAP; // bars
        h += SUMMARY_H; // summary
    }

    // If no cores at all
    if p_count == 0 && e_count == 0 {
        h += ROW_H; // "No cores detected" message
    }

    h += BTN_ROW_H; // Apply row

    (h as f32 * sc) as i32
}

// ── Thread entry point ───────────────────────────────────────────────

fn thread_main(hdl: SafeHandle, dying: Arc<AtomicBool>, theme: NativeTheme) {
    unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }

    let cls = to_utf16_z("mhd_cpuplan_cls");
    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let hinst: windows::Win32::Foundation::HINSTANCE = hinst.into();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(cpuplan_wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: PCWSTR::from_raw(cls.as_ptr()),
        ..Default::default()
    };
    unsafe { let _ = RegisterClassW(&wc); }

    // Detect core topology
    let (p_cores, e_cores) = detect_core_topology();
    let total_cores = p_cores.len() + e_cores.len();

    // Initial compute TOTAL_H using unscaled values (we'll use final win_h)
    let init_total = compute_total_h(1.0, p_cores.len(), e_cores.len());

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(cls.as_ptr()), PCWSTR::null(),
            WS_POPUP, 0, 0, W, init_total,
            None, None, hinst, None,
        )
    } { Ok(h) => h, Err(_) => return, };

    let sc = sc_from_hwnd(hwnd);
    let win_w = (W as f32 * sc) as i32;
    let win_h = compute_total_h(sc, p_cores.len(), e_cores.len());

    let wa = work_area();
    let pos = POINT {
        x: wa.left + (wa.right - wa.left - win_w) / 2,
        y: wa.top + (wa.bottom - wa.top - win_h) / 2,
    };
    unsafe { let _ = SetWindowPos(hwnd, HWND::default(), pos.x, pos.y, win_w, win_h, SWP_NOZORDER); }

    let active_schemes = enumerate_schemes();
    let active_guid = get_active_scheme_guid();
    let active_plan_name = active_schemes.iter()
        .find(|(g, _)| *g == active_guid)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let (ac, dc) = read_current_plan_values();

    let stress_stop = Arc::new(AtomicBool::new(false));
    let freq_sampler = read_processor_base_mhz(total_cores.max(1))
        .and_then(|base_mhz| PdhFreqSampler::new(total_cores.max(1), base_mhz));

    let mut st = PanelState {
        theme,
        schemes: active_schemes,
        active_plan_name,
        ac,
        dc,
        dirty: false,
        focused: None,
        edit_text: String::new(),
        edit_select_all: false,
        pos,
        monitor: MonitorState {
            core_load: vec![0.0; total_cores.max(1)],
            core_parked: vec![false; total_cores.max(1)],
            core_freq_mhz: vec![0; total_cores.max(1)],
            freq_sampler,
            p_cores,
            e_cores,
            stress_level: StressLevel::Off,
            stress_stop: stress_stop.clone(),
            prev_perf: None,
        },
        stress_handles: Vec::new(),
    };

    if dying.load(Ordering::Acquire) {
        unsafe { let _ = DestroyWindow(hwnd); } return;
    }

    // Sample initial perf info
    if let Some(perf) = read_perf_info() {
        st.monitor.prev_perf = Some(perf);
    }
    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
    }

    paint_panel(hwnd, &st, win_w, win_h, sc);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // Start monitor timer
        let _ = SetTimer(hwnd, TIMER_MONITOR, 500, None);
    }

    let event = hdl.0;
    let mut hidden = false;
    let mut drag: Option<(i32, i32)> = None;
    let mut mouse_tracked = false;

    loop {
        if dying.load(Ordering::Acquire) { break; }

        let wait = [event];
        let res = unsafe { MsgWaitForMultipleObjects(Some(&wait), false, INFINITE, QS_ALLINPUT) };

        match res {
            WAIT_OBJECT_0 => {
                hidden = !hidden;
                if hidden {
                    unsafe { let _ = ReleaseCapture(); _ = ShowWindow(hwnd, SW_HIDE); _ = InvalidateRect(hwnd, None, false); }
                    // Kill timer when hidden
                    unsafe { let _ = KillTimer(hwnd, TIMER_MONITOR); }
                } else {
                    // Re-read current values when showing
                    let (ac_new, dc_new) = read_current_plan_values();
                    st.ac = ac_new;
                    st.dc = dc_new;
                    st.dirty = false;
                    st.focused = None;
                    st.edit_select_all = false;
                    st.schemes = enumerate_schemes();
                    let ag = get_active_scheme_guid();
                    st.active_plan_name = st.schemes.iter()
                        .find(|(g, _)| *g == ag)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "Unknown".to_string());

                    // Reset monitor state on show
                    if let Some(perf) = read_perf_info() {
                        st.monitor.prev_perf = Some(perf);
                    }
                    st.monitor.core_load.fill(0.0);
                    st.monitor.core_parked = read_parked_state();
                    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
                        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
                    }

                    paint_panel(hwnd, &st, win_w, win_h, sc);
                    unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
                    // Restart timer
                    unsafe { let _ = SetTimer(hwnd, TIMER_MONITOR, 500, None); }
                }
            }
            WAIT_EVENT(1) => {
                let mut msg = MSG::default();
                let mut repaint = false;
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        if msg.message == WM_QUIT { break; }
                        // Translate keyboard messages so WM_KEYDOWN → WM_CHAR for digit input
                        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                        if msg.hwnd == hwnd {
                            if !matches!(msg.message, WM_MOUSEMOVE | WM_MOUSELEAVE) {
                                repaint = true;
                            }
                            if msg.message == WM_KEYDOWN || msg.message == WM_CHAR {
                                if handle_key(&msg, &mut st, &mut hidden) {
                                    repaint = true;
                                }
                            }
                            if msg.message == WM_MOUSEWHEEL {
                                if handle_wheel(&msg, &mut st, sc) {
                                    repaint = true;
                                }
                            }
                            if msg.message == WM_TIMER && msg.wParam.0 == TIMER_MONITOR {
                                handle_monitor_timer(&mut st);
                                repaint = true;
                            }
                            if !msg_handler(hwnd, &msg, &mut st, &mut drag, &mut mouse_tracked, &mut hidden, sc) {
                                hidden = true;
                                repaint = false;
                            }
                        }
                    }
                }
                if repaint && !hidden {
                    paint_panel(hwnd, &st, win_w, win_h, sc);
                }
            }
            _ => break,
        }
    }

    // Stop stress threads on exit
    stop_stress_threads(&st.monitor.stress_stop, &mut st.stress_handles);
    unsafe { let _ = KillTimer(hwnd, TIMER_MONITOR); _ = DestroyWindow(hwnd); }
}

// ── Monitor timer handler ───────────────────────────────────────────

fn handle_monitor_timer(st: &mut PanelState) {
    // Read new perf info
    let curr_perf = match read_perf_info() {
        Some(p) => p,
        None => return,
    };

    // If we have a previous snapshot, compute loads
    if let Some(ref prev) = st.monitor.prev_perf {
        let loads = compute_loads(prev, &curr_perf);
        // Match lengths to our core vectors
        let n = st.monitor.core_load.len().min(loads.len());
        for i in 0..n {
            st.monitor.core_load[i] = loads[i];
        }
    }

    // Store current as previous for next tick
    st.monitor.prev_perf = Some(curr_perf);

    // Read parked state
    let parked = read_parked_state();
    update_vec_prefix(&mut st.monitor.core_parked, &parked);

    if let Some(freqs) = read_effective_processor_frequencies(&st.monitor) {
        update_vec_prefix(&mut st.monitor.core_freq_mhz, &freqs);
    }
}

fn update_vec_prefix<T: Copy>(dst: &mut [T], src: &[T]) {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
}

// ── Key handling ─────────────────────────────────────────────────────

fn handle_key(msg: &MSG, st: &mut PanelState, hidden: &mut bool) -> bool {
    let vk = msg.wParam.0 as u32;
    match vk {
        0x1B => { // Escape
            *hidden = true;
            st.focused = None;
            st.edit_select_all = false;
            unsafe { let _ = ReleaseCapture(); }
            return true;
        }
        0x0D => { // Enter — apply
            if st.dirty {
                flush_edit(st);
                write_plan_values(&st.ac, &st.dc);
                st.dirty = false;
                st.focused = None;
                st.edit_select_all = false;
                unsafe { let _ = ReleaseCapture(); }
                return true;
            }
        }
        0x09 => { // Tab — cycle focus through editable fields
            flush_edit(st);
            let field = match st.focused {
                None | Some(Field::DcMaxFreq) => Field::AcMinCores,
                Some(Field::AcMinCores) => Field::DcMinCores,
                Some(Field::DcMinCores) => Field::AcMaxCores,
                Some(Field::AcMaxCores) => Field::DcMaxCores,
                Some(Field::DcMaxCores) => Field::AcMinFreq,
                Some(Field::AcMinFreq) => Field::DcMinFreq,
                Some(Field::DcMinFreq) => Field::AcMaxFreq,
                Some(Field::AcMaxFreq) => Field::DcMaxFreq,
            };
            focus_field(st, field);
            return true;
        }
        _ => {
            if st.focused.is_some() {
                // WM_CHAR: wParam is the character code
                if msg.message == WM_CHAR {
                    let ch = vk;
                    if ch >= '0' as u32 && ch <= '9' as u32 {
                        if st.edit_select_all {
                            st.edit_text.clear();
                            st.edit_select_all = false;
                        }
                        if st.edit_text.len() < 3 {
                            st.edit_text.push(char::from_u32(ch).unwrap_or('0'));
                            st.dirty = true;
                        }
                        return true;
                    }
                    if ch == 0x08 { // Backspace as char
                        if st.edit_select_all {
                            st.edit_text.clear();
                            st.edit_select_all = false;
                            st.dirty = true;
                            return true;
                        }
                        if !st.edit_text.is_empty() {
                            st.edit_text.pop();
                            st.dirty = true;
                        }
                        return true;
                    }
                }
                // WM_KEYDOWN fallback for backspace
                if msg.message == WM_KEYDOWN && vk == 0x08 {
                    if st.edit_select_all {
                        st.edit_text.clear();
                        st.edit_select_all = false;
                    } else if !st.edit_text.is_empty() {
                        st.edit_text.pop();
                    }
                    st.dirty = true;
                    return true;
                }
            }
        }
    }
    false
}

// ── Wheel handling ──────────────────────────────────────────────────

fn handle_wheel(msg: &MSG, st: &mut PanelState, _sc: f32) -> bool {
    let delta = ((msg.wParam.0 >> 16) & 0xffff) as i16;
    let steps = delta / 120;
    if let Some(field) = st.focused {
        flush_edit(st);
        let val = match field {
            Field::AcMinCores => &mut st.ac.min_cores,
            Field::DcMinCores => &mut st.dc.min_cores,
            Field::AcMaxCores => &mut st.ac.max_cores,
            Field::DcMaxCores => &mut st.dc.max_cores,
            Field::AcMinFreq => &mut st.ac.min_freq,
            Field::DcMinFreq => &mut st.dc.min_freq,
            Field::AcMaxFreq => &mut st.ac.max_freq,
            Field::DcMaxFreq => &mut st.dc.max_freq,
        };
        if steps > 0 {
            *val = (*val + steps as u32).min(100);
        } else {
            *val = val.saturating_sub((-steps) as u32);
        }
        st.dirty = true;
        return true;
    }
    false
}

/// Get current value for a field (as u32)
fn get_field_value(st: &PanelState, field: Field) -> u32 {
    match field {
        Field::AcMinCores => st.ac.min_cores,
        Field::DcMinCores => st.dc.min_cores,
        Field::AcMaxCores => st.ac.max_cores,
        Field::DcMaxCores => st.dc.max_cores,
        Field::AcMinFreq => st.ac.min_freq,
        Field::DcMinFreq => st.dc.min_freq,
        Field::AcMaxFreq => st.ac.max_freq,
        Field::DcMaxFreq => st.dc.max_freq,
    }
}

fn focus_field(st: &mut PanelState, field: Field) {
    st.focused = Some(field);
    st.edit_text = get_field_value(st, field).to_string();
    st.edit_select_all = true;
}

fn clear_focus(st: &mut PanelState) {
    st.focused = None;
    st.edit_select_all = false;
}

fn flush_edit(st: &mut PanelState) {
    if st.edit_text.is_empty() || st.focused.is_none() { return; }
    if let Ok(v) = st.edit_text.parse::<u32>() {
        let v = v.min(100);
        match st.focused.unwrap() {
            Field::AcMinCores => st.ac.min_cores = v,
            Field::DcMinCores => st.dc.min_cores = v,
            Field::AcMaxCores => st.ac.max_cores = v,
            Field::DcMaxCores => st.dc.max_cores = v,
            Field::AcMinFreq => st.ac.min_freq = v,
            Field::DcMinFreq => st.dc.min_freq = v,
            Field::AcMaxFreq => st.ac.max_freq = v,
            Field::DcMaxFreq => st.dc.max_freq = v,
        }
    }
    st.edit_text.clear();
    st.edit_select_all = false;
}

// ── Message handler ─────────────────────────────────────────────────

fn msg_handler(
    hwnd: HWND, msg: &MSG, st: &mut PanelState,
    drag: &mut Option<(i32, i32)>, mouse_tracked: &mut bool,
    hidden: &mut bool, sc: f32,
) -> bool {
    if msg.message == WM_ACTIVATE && msg.wParam.0 as u32 == 0 {
        *hidden = true;
        clear_focus(st);
        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); _ = KillTimer(hwnd, TIMER_MONITOR); }
        return false;
    }

    if msg.message == WM_LBUTTONDOWN {
        let x = (msg.lParam.0 as i32) & 0xFFFF;
        let y = ((msg.lParam.0 as i32) >> 16) & 0xFFFF;

        // Header: drag or close
        if y < (HDR_H as f32 * sc) as i32 {
            let cx = (W as f32 * sc) as i32 - (PAD as f32 * sc) as i32 - (20.0 * sc) as i32;
            if x >= cx && x <= cx + (20.0 * sc) as i32 {
                *hidden = true;
                clear_focus(st);
                unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); _ = KillTimer(hwnd, TIMER_MONITOR); }
                return false;
            }
            *drag = Some((x, y));
            unsafe { let _ = SetCapture(hwnd); }
            return true;
        }

        // Plan row: click cycles to next power plan
        if hit_plan_row(y, sc) {
            let cur = st.schemes.iter().position(|(_, n)| n == &st.active_plan_name).unwrap_or(0);
            let next = (cur + 1) % st.schemes.len().max(1);
            if let Some((guid, name)) = st.schemes.get(next) {
                set_active_scheme(*guid);
                st.active_plan_name = name.clone();
                let (ac_new, dc_new) = read_current_plan_values();
                st.ac = ac_new;
                st.dc = dc_new;
                st.dirty = false;
                clear_focus(st);
            }
            return true;
        }

        // Settings rows: value cells and toggles
        let _y_u = (y as f32 / sc) as i32; // unscaled Y

        // Try to hit a value cell
        if let Some(field) = hit_value_cell(x, y, sc) {
            flush_edit(st);
            focus_field(st, field);
            // Give the popup keyboard focus so WM_KEYDOWN/WM_CHAR arrive.
            unsafe { let _ = SetForegroundWindow(hwnd); }
            return true;
        }

        // Try to hit a toggle
        if hit_toggle(x, y, sc, &mut st.ac, &mut st.dc, &mut st.dirty) {
            flush_edit(st);
            return true;
        }

        // Stress buttons
        if hit_stress_button(x, y, sc, &mut st.monitor, &mut st.stress_handles) {
            flush_edit(st);
            clear_focus(st);
            return true;
        }

        // Apply button
        if hit_apply(x, y, sc, st) {
            flush_edit(st);
            if st.dirty {
                write_plan_values(&st.ac, &st.dc);
                st.dirty = false;
                clear_focus(st);
            }
            *hidden = true;
            clear_focus(st);
            unsafe { let _ = ShowWindow(hwnd, SW_HIDE); _ = ReleaseCapture(); _ = KillTimer(hwnd, TIMER_MONITOR); }
            return false;
        }

        // Click outside editable fields → apply current edit and clear focus
        if st.focused.is_some() {
            flush_edit(st);
            clear_focus(st);
            st.dirty = true;
            unsafe { let _ = ReleaseCapture(); }
        }

        return true;
    }

    if msg.message == WM_LBUTTONUP {
        if drag.is_some() {
            *drag = None;
            unsafe { let _ = ReleaseCapture(); }
        }
        return true;
    }

    if msg.message == WM_MOUSEMOVE {
        if let Some((grab_x, grab_y)) = *drag {
            let mut cursor = POINT::default();
            unsafe { let _ = GetCursorPos(&mut cursor); }
            let nx = cursor.x - grab_x;
            let ny = cursor.y - grab_y;
            unsafe {
                let _ = SetWindowPos(hwnd, HWND::default(), nx, ny, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            }
            st.pos = POINT { x: nx, y: ny };
        }
        if !*hidden && !*mouse_tracked {
            let mut tm = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE, hwndTrack: hwnd, ..Default::default()
            };
            unsafe { let _ = TrackMouseEvent(&mut tm); }
            *mouse_tracked = true;
        }
        return true;
    }

    if msg.message == WM_MOUSELEAVE {
        *mouse_tracked = false;
        return true;
    }

    true
}

// ── Hit tests ────────────────────────────────────────────────────────

fn hit_plan_row(y: i32, sc: f32) -> bool {
    let plan_y = (HDR_H as f32 * sc) as i32;
    let plan_bottom = plan_y + (PLAN_H as f32 * sc) as i32;
    y >= plan_y && y < plan_bottom
}

/// Settings Y ranges (unscaled) — compute them from the layout.
fn settings_y_ranges(sc: f32) -> SettingsYRanges {
    let s = sc;
    let mut y = HDR_H + PLAN_H + SEP_H;

    // CORES section
    let cores_sec_y = y;
    y += SEC_H;
    let min_cores_y = y;
    y += ROW_H;
    let max_cores_y = y;
    y += ROW_H;
    let _cores_sec_bot = y;

    // FREQUENCY section
    let freq_sec_y = y;
    y += SEC_H;
    let min_freq_y = y;
    y += ROW_H;
    let max_freq_y = y;
    y += ROW_H;
    let _freq_sec_bot = y;

    // POWER FEATURES section
    let pf_sec_y = y;
    y += SEC_H;
    let freq_scale_y = y;
    y += ROW_H;
    let turbo_y = y;
    y += ROW_H;
    let pf_sec_bot = y;

    // Separator after settings
    y += SEP_H;

    // Monitor header
    let mon_hdr_y = y;
    let _ = y + MON_HDR_H;

    SettingsYRanges {
        cores_sec_y: (cores_sec_y as f32 * s) as i32,
        min_cores_y: (min_cores_y as f32 * s) as i32,
        max_cores_y: (max_cores_y as f32 * s) as i32,
        freq_sec_y: (freq_sec_y as f32 * s) as i32,
        min_freq_y: (min_freq_y as f32 * s) as i32,
        max_freq_y: (max_freq_y as f32 * s) as i32,
        pf_sec_y: (pf_sec_y as f32 * s) as i32,
        freq_scale_y: (freq_scale_y as f32 * s) as i32,
        turbo_y: (turbo_y as f32 * s) as i32,
        pf_sec_bot: (pf_sec_bot as f32 * s) as i32,
        mon_hdr_y: (mon_hdr_y as f32 * s) as i32,
    }
}

struct SettingsYRanges {
    cores_sec_y: i32,
    min_cores_y: i32,
    max_cores_y: i32,
    freq_sec_y: i32,
    min_freq_y: i32,
    max_freq_y: i32,
    pf_sec_y: i32,
    freq_scale_y: i32,
    turbo_y: i32,
    pf_sec_bot: i32,
    mon_hdr_y: i32,
}

fn hit_value_cell(x: i32, y: i32, sc: f32) -> Option<Field> {
    let ranges = settings_y_ranges(sc);
    let sx = (PAD as f32 * sc + (LABEL_W as f32 * sc)) as i32;
    let vw = (VAL_W as f32 * sc) as i32;
    let gap = (PAD_INNER as f32 * sc) as i32;
    let row_h = (ROW_H as f32 * sc) as i32;

    // Core parking min cores row
    if y >= ranges.min_cores_y && y < ranges.min_cores_y + row_h {
        if x >= sx && x < sx + vw { return Some(Field::AcMinCores); }
        if x >= sx + vw + gap && x < sx + vw + gap + vw { return Some(Field::DcMinCores); }
    }

    // Core parking max cores row
    if y >= ranges.max_cores_y && y < ranges.max_cores_y + row_h {
        if x >= sx && x < sx + vw { return Some(Field::AcMaxCores); }
        if x >= sx + vw + gap && x < sx + vw + gap + vw { return Some(Field::DcMaxCores); }
    }

    // Min Freq row
    if y >= ranges.min_freq_y && y < ranges.min_freq_y + row_h {
        if x >= sx && x < sx + vw { return Some(Field::AcMinFreq); }
        if x >= sx + vw + gap && x < sx + vw + gap + vw { return Some(Field::DcMinFreq); }
    }

    // Max Freq row
    if y >= ranges.max_freq_y && y < ranges.max_freq_y + row_h {
        if x >= sx && x < sx + vw { return Some(Field::AcMaxFreq); }
        if x >= sx + vw + gap && x < sx + vw + gap + vw { return Some(Field::DcMaxFreq); }
    }

    None
}

fn hit_toggle(x: i32, y: i32, sc: f32, ac: &mut PlanValues, dc: &mut PlanValues, dirty: &mut bool) -> bool {
    let ranges = settings_y_ranges(sc);
    let row_h = (ROW_H as f32 * sc) as i32;
    let sx = (PAD as f32 * sc + (LABEL_W as f32 * sc)) as i32;
    let vw = (VAL_W as f32 * sc) as i32;
    let _gap = (PAD_INNER as f32 * sc) as i32;

    // Freq Scaling row
    if y >= ranges.freq_scale_y && y < ranges.freq_scale_y + row_h {
        let toggle_rects = toggle_rects(sx, ranges.freq_scale_y, vw, row_h, sc);
        if x >= toggle_rects.0 .0 && x < toggle_rects.0 .1 {
            ac.freq_scale = !ac.freq_scale;
            *dirty = true;
            return true;
        }
        if x >= toggle_rects.1 .0 && x < toggle_rects.1 .1 {
            dc.freq_scale = !dc.freq_scale;
            *dirty = true;
            return true;
        }
    }

    // Turbo Boost row
    if y >= ranges.turbo_y && y < ranges.turbo_y + row_h {
        let toggle_rects = toggle_rects(sx, ranges.turbo_y, vw, row_h, sc);
        if x >= toggle_rects.0 .0 && x < toggle_rects.0 .1 {
            ac.turbo = !ac.turbo;
            *dirty = true;
            return true;
        }
        if x >= toggle_rects.1 .0 && x < toggle_rects.1 .1 {
            dc.turbo = !dc.turbo;
            *dirty = true;
            return true;
        }
    }

    false
}

fn toggle_rects(sx: i32, _ry: i32, vw: i32, _row_h: i32, sc: f32) -> ((i32, i32), (i32, i32)) {
    let tog_w = (36.0 * sc) as i32;
    let gap = (PAD_INNER as f32 * sc) as i32;
    // AC toggle centered in AC column
    let ac_x = sx + (vw - tog_w) / 2;
    let ac_x2 = ac_x + tog_w;
    // DC toggle centered in DC column
    let dc_x = sx + vw + gap + (vw - tog_w) / 2;
    let dc_x2 = dc_x + tog_w;
    ((ac_x, ac_x2), (dc_x, dc_x2))
}

fn hit_stress_button(x: i32, y: i32, sc: f32, mon: &mut MonitorState, handles: &mut Vec<std::thread::JoinHandle<()>>) -> bool {
    let ranges = settings_y_ranges(sc);
    let load_y = ranges.mon_hdr_y + (MON_HDR_H as f32 * sc) as i32;
    let load_h = (LOAD_ROW_H as f32 * sc) as i32;
    let win_w = (W as f32 * sc) as i32;

    for (level, x1, x2, y1, y2) in stress_button_rects(win_w, load_y, load_h, sc, mon.core_load.len()) {
        if x >= x1 && x < x2 && y >= y1 && y < y2 {
            stop_stress_threads(&mon.stress_stop, handles);
            mon.stress_stop = Arc::new(AtomicBool::new(false));
            mon.stress_level = level;
            if let StressLevel::Threads(count) = level {
                let stop = Arc::new(AtomicBool::new(false));
                mon.stress_stop = stop.clone();
                *handles = spawn_stress_threads(count, stop);
            }
            return true;
        }
    }

    false
}

fn hit_apply(x: i32, y: i32, sc: f32, st: &PanelState) -> bool {
    let ranges = settings_y_ranges(sc);
    let mut content_y = ranges.mon_hdr_y + ((MON_HDR_H + LOAD_ROW_H) as f32 * sc) as i32;

    // Add core group bars and summary heights
    if !st.monitor.p_cores.is_empty() {
        content_y += (SEC_H as f32 * sc) as i32;
        content_y += ((BAR_H + BAR_GAP) * st.monitor.p_cores.len() as i32 - BAR_GAP) as f32 as i32;
        content_y += (SUMMARY_H as f32 * sc) as i32;
    }
    if !st.monitor.e_cores.is_empty() {
        content_y += (SEC_H as f32 * sc) as i32;
        content_y += ((BAR_H + BAR_GAP) * st.monitor.e_cores.len() as i32 - BAR_GAP) as f32 as i32;
        content_y += (SUMMARY_H as f32 * sc) as i32;
    }
    if st.monitor.p_cores.is_empty() && st.monitor.e_cores.is_empty() {
        content_y += (ROW_H as f32 * sc) as i32;
    }

    let win_w = (W as f32 * sc) as i32;
    let btn_w = (BTN_W as f32 * sc) as i32;
    let btn_h = (BTN_H as f32 * sc) as i32;
    let btn_y = content_y + ((BTN_ROW_H - BTN_H) as f32 * sc / 2.0) as i32;
    let bx = (win_w - btn_w) / 2;

    y >= btn_y && y < btn_y + btn_h && x >= bx && x < bx + btn_w
}

// ── Layout constant re-exports for paint ─────────────────────────────

const LABEL_W: i32 = 140;
const VAL_W: i32 = 56;

// ── Painting ─────────────────────────────────────────────────────────

fn paint_panel(hwnd: HWND, st: &PanelState, w: i32, h: i32, sc: f32) {
    let mut frame = match crate::renderer::DibFrame::new(w, h) {
        Some(f) => f,
        None => return,
    };
    let pixels = frame.pixels_mut();
    crate::osd::draw_rounded_rect(pixels, w, h, (RADIUS as f32 * sc) as i32, st.theme.background);
    let mem = frame.dc();
    let font = crate::osd::create_font(-(14.0 * sc) as i32, false, "Segoe UI");
    let _of = unsafe { SelectObject(mem, font) };

    let fg = st.theme.text;
    let accent = st.theme.accent;
    let dim = st.theme.text_muted;
    // let bg = st.theme.background;
    let border = st.theme.border;
    let pad_sc = (PAD as f32 * sc) as i32;
    let s = sc;

    let ranges = settings_y_ranges(sc);

    // ── Header ──
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("CPU Power Plan"),
       &mut rct(0, (8.0 * s) as i32, w, (HDR_H as f32 * s) as i32),
       DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // Close ✕
    let cx = w - pad_sc - (20.0 * s) as i32;
    let cx2 = cx + (20.0 * s) as i32;
    tcol(mem, dim);
    dw(mem, &mut to_utf16_z("✕"),
       &mut rct(cx, (8.0 * s) as i32, cx2, (HDR_H as f32 * s) as i32 - (8.0 * s) as i32),
       DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // ── Active plan name (clickable) ──
    let plan_y = (HDR_H as f32 * s) as i32;
    tcol(mem, accent);
    let mut plan_text = to_utf16_z(&format!("▸ {}", st.active_plan_name));
    dw(mem, &mut plan_text,
       &mut rct(pad_sc, plan_y, w - pad_sc, plan_y + (PLAN_H as f32 * s) as i32),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // ── Separator after plan row ──
    let sep1_y = plan_y + (PLAN_H as f32 * s) as i32;
    fill_rect(mem, pad_sc, sep1_y, w - pad_sc, sep1_y + 1, border);

    // ── Settings section ──────────────────────────────────────────────

    let sx = pad_sc + (LABEL_W as f32 * s) as i32;
    let vw = (VAL_W as f32 * s) as i32;
    let gap = (PAD_INNER as f32 * s) as i32;

    // Column headers for AC/DC
    tcol(mem, dim);
    dw(mem, &mut to_utf16_z("AC"),
       &mut rct(sx, ranges.cores_sec_y, sx + vw, ranges.cores_sec_y + (SEC_H as f32 * s) as i32),
       DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    dw(mem, &mut to_utf16_z("DC"),
       &mut rct(sx + vw + gap, ranges.cores_sec_y, sx + vw + gap + vw, ranges.cores_sec_y + (SEC_H as f32 * s) as i32),
       DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // ── CORES section ──
    draw_section_header(mem, pad_sc, ranges.cores_sec_y, w, s, "CORES", fg, dim);
    let row_h = (ROW_H as f32 * s) as i32;

    // Min Cores %
    let ry = ranges.min_cores_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Min Cores %"),
       &mut rct(pad_sc, ry, pad_sc + (LABEL_W as f32 * s) as i32, ry + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    let ac_str = if st.focused == Some(Field::AcMinCores) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.min_cores)
    };
    draw_value_cell(mem, sx, ry, vw, row_h, s, &ac_str, Field::AcMinCores, st.focused, st.dirty, fg, dim, accent);
    let dc_str = if st.focused == Some(Field::DcMinCores) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.min_cores)
    };
    draw_value_cell(mem, sx + vw + gap, ry, vw, row_h, s, &dc_str, Field::DcMinCores, st.focused, st.dirty, fg, dim, accent);

    // Max Cores %
    let ry = ranges.max_cores_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Max Cores %"),
       &mut rct(pad_sc, ry, pad_sc + (LABEL_W as f32 * s) as i32, ry + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    let ac_str = if st.focused == Some(Field::AcMaxCores) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.max_cores)
    };
    draw_value_cell(mem, sx, ry, vw, row_h, s, &ac_str, Field::AcMaxCores, st.focused, st.dirty, fg, dim, accent);
    let dc_str = if st.focused == Some(Field::DcMaxCores) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.max_cores)
    };
    draw_value_cell(mem, sx + vw + gap, ry, vw, row_h, s, &dc_str, Field::DcMaxCores, st.focused, st.dirty, fg, dim, accent);

    // ── FREQUENCY section ──
    draw_section_header(mem, pad_sc, ranges.freq_sec_y, w, s, "FREQUENCY", fg, dim);

    // Min Speed %
    let ry_min = ranges.min_freq_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Min Speed %"),
       &mut rct(pad_sc, ry_min, pad_sc + (LABEL_W as f32 * s) as i32, ry_min + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    let ac_str = if st.focused == Some(Field::AcMinFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.min_freq)
    };
    draw_value_cell(mem, sx, ry_min, vw, row_h, s, &ac_str, Field::AcMinFreq, st.focused, st.dirty, fg, dim, accent);
    let dc_str = if st.focused == Some(Field::DcMinFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.min_freq)
    };
    draw_value_cell(mem, sx + vw + gap, ry_min, vw, row_h, s, &dc_str, Field::DcMinFreq, st.focused, st.dirty, fg, dim, accent);

    // Max Speed %
    let ry_max = ranges.max_freq_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Max Speed %"),
       &mut rct(pad_sc, ry_max, pad_sc + (LABEL_W as f32 * s) as i32, ry_max + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    let ac_str = if st.focused == Some(Field::AcMaxFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.ac.max_freq)
    };
    draw_value_cell(mem, sx, ry_max, vw, row_h, s, &ac_str, Field::AcMaxFreq, st.focused, st.dirty, fg, dim, accent);
    let dc_str = if st.focused == Some(Field::DcMaxFreq) {
        edit_display(st)
    } else {
        format!("{}%", st.dc.max_freq)
    };
    draw_value_cell(mem, sx + vw + gap, ry_max, vw, row_h, s, &dc_str, Field::DcMaxFreq, st.focused, st.dirty, fg, dim, accent);

    // ── POWER FEATURES section ──
    draw_section_header(mem, pad_sc, ranges.pf_sec_y, w, s, "POWER FEATURES", fg, dim);

    // Freq Scaling toggle
    let ry_fs = ranges.freq_scale_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Freq Scaling"),
       &mut rct(pad_sc, ry_fs, pad_sc + (LABEL_W as f32 * s) as i32, ry_fs + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    draw_toggle(mem, sx, ry_fs, vw, row_h, s, st.ac.freq_scale, &st.theme);
    draw_toggle(mem, sx + vw + gap, ry_fs, vw, row_h, s, st.dc.freq_scale, &st.theme);

    // Turbo Boost toggle
    let ry_tb = ranges.turbo_y;
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Turbo Boost"),
       &mut rct(pad_sc, ry_tb, pad_sc + (LABEL_W as f32 * s) as i32, ry_tb + row_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    draw_toggle(mem, sx, ry_tb, vw, row_h, s, st.ac.turbo, &st.theme);
    draw_toggle(mem, sx + vw + gap, ry_tb, vw, row_h, s, st.dc.turbo, &st.theme);

    // ── Separator before monitor section ──
    let sep2_y = ranges.pf_sec_bot;
    fill_rect(mem, pad_sc, sep2_y, w - pad_sc, sep2_y + 1, border);

    // ── LIVE MONITOR section ──
    let mon_y = ranges.mon_hdr_y;
    let mon_h = (MON_HDR_H as f32 * s) as i32;

    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("LIVE MONITOR"),
       &mut rct(pad_sc, mon_y, (W as f32 * s / 2.0) as i32, mon_y + mon_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    let load_y = mon_y + mon_h;
    let load_h = (LOAD_ROW_H as f32 * s) as i32;
    draw_stress_buttons(mem, w, load_y, load_h, s, &st.monitor, &st.theme);

    // ── Core bars ──
    let mut content_y = load_y + load_h;

    // Bar area width
    let bar_area_w = w - pad_sc * 2;

    // P-CORES group
    if !st.monitor.p_cores.is_empty() {
        tcol(mem, dim);
        let pc_hdr = content_y;
        let pc_hdr_h = (SEC_H as f32 * s) as i32;
        dw(mem, &mut to_utf16_z(&format!("P-CORES ({})", st.monitor.p_cores.len())),
           &mut rct(pad_sc, pc_hdr, w - pad_sc, pc_hdr + pc_hdr_h),
           DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        content_y = pc_hdr + pc_hdr_h;

        let bar_h = (BAR_H as f32 * s) as i32;
        let bar_gap = (BAR_GAP as f32 * s) as i32;

        for (i, &core_idx) in st.monitor.p_cores.iter().enumerate() {
            let load = if core_idx < st.monitor.core_load.len() {
                st.monitor.core_load[core_idx]
            } else {
                0.0
            };
            let parked = if core_idx < st.monitor.core_parked.len() {
                st.monitor.core_parked[core_idx]
            } else {
                false
            };
            let freq_mhz = st.monitor.core_freq_mhz.get(core_idx).copied().unwrap_or(0);
            draw_core_row(mem, pad_sc, content_y, bar_area_w, bar_h, s, &format!("P{}", i), load, freq_mhz, parked, &st.theme);
            content_y += bar_h + bar_gap;
        }
        content_y -= bar_gap; // remove last gap

        // Summary line
        let avg_load = avg_load_for_cores(&st.monitor, &st.monitor.p_cores);
        let parked_count = st.monitor.p_cores.iter()
            .filter(|&&i| i < st.monitor.core_parked.len() && st.monitor.core_parked[i])
            .count();
        let summary = format!("Load: {:.0}%   Parked: {}/{}", avg_load * 100.0, parked_count, st.monitor.p_cores.len());
        tcol(mem, dim);
        let sum_h = (SUMMARY_H as f32 * s) as i32;
        dw(mem, &mut to_utf16_z(&summary),
           &mut rct(pad_sc, content_y, w - pad_sc, content_y + sum_h),
           DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        content_y += sum_h;
    }

    // E-CORES group
    if !st.monitor.e_cores.is_empty() {
        tcol(mem, dim);
        let ec_hdr = content_y;
        let ec_hdr_h = (SEC_H as f32 * s) as i32;
        dw(mem, &mut to_utf16_z(&format!("E-CORES ({})", st.monitor.e_cores.len())),
           &mut rct(pad_sc, ec_hdr, w - pad_sc, ec_hdr + ec_hdr_h),
           DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        content_y = ec_hdr + ec_hdr_h;

        let bar_h = (BAR_H as f32 * s) as i32;
        let bar_gap = (BAR_GAP as f32 * s) as i32;

        for (i, &core_idx) in st.monitor.e_cores.iter().enumerate() {
            let load = if core_idx < st.monitor.core_load.len() {
                st.monitor.core_load[core_idx]
            } else {
                0.0
            };
            let parked = if core_idx < st.monitor.core_parked.len() {
                st.monitor.core_parked[core_idx]
            } else {
                false
            };
            let freq_mhz = st.monitor.core_freq_mhz.get(core_idx).copied().unwrap_or(0);
            draw_core_row(mem, pad_sc, content_y, bar_area_w, bar_h, s, &format!("E{}", i), load, freq_mhz, parked, &st.theme);
            content_y += bar_h + bar_gap;
        }
        content_y -= bar_gap;

        // Summary line
        let avg_load = avg_load_for_cores(&st.monitor, &st.monitor.e_cores);
        let parked_count = st.monitor.e_cores.iter()
            .filter(|&&i| i < st.monitor.core_parked.len() && st.monitor.core_parked[i])
            .count();
        let summary = format!("Load: {:.0}%   Parked: {}/{}", avg_load * 100.0, parked_count, st.monitor.e_cores.len());
        tcol(mem, dim);
        let sum_h = (SUMMARY_H as f32 * s) as i32;
        dw(mem, &mut to_utf16_z(&summary),
           &mut rct(pad_sc, content_y, w - pad_sc, content_y + sum_h),
           DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        content_y += sum_h;
    }

    if st.monitor.p_cores.is_empty() && st.monitor.e_cores.is_empty() {
        tcol(mem, dim);
        dw(mem, &mut to_utf16_z("No cores detected"),
           &mut rct(pad_sc, content_y, w - pad_sc, content_y + (ROW_H as f32 * s) as i32),
           DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        content_y += (ROW_H as f32 * s) as i32;
    }

    // ── Apply button ──
    let btn_w = (BTN_W as f32 * s) as i32;
    let btn_h_actual = (BTN_H as f32 * s) as i32;
    let btn_row_h = (BTN_ROW_H as f32 * s) as i32;
    let btn_y = content_y + (btn_row_h - btn_h_actual) / 2;
    let bx = (w - btn_w) / 2;
    let btn_color = if st.dirty { accent } else { dim };
    fill_rect(mem, bx, btn_y, bx + btn_w, btn_y + btn_h_actual, btn_color);
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z("Apply"),
       &mut rct(bx, btn_y, bx + btn_w, btn_y + btn_h_actual),
       DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    frame.fix_gdi_alpha(st.theme.background);

    unsafe {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        frame.present_layered(hwnd, wr.left, wr.top, 255);
    }

    unsafe { let _ = DeleteObject(font); }
}

fn avg_load_for_cores(mon: &MonitorState, cores: &[usize]) -> f32 {
    let mut sum = 0.0;
    let mut count = 0;
    for &ci in cores {
        if ci < mon.core_load.len() {
            sum += mon.core_load[ci];
            count += 1;
        }
    }
    if count > 0 { sum / count as f32 } else { 0.0 }
}

fn draw_section_header(mem: HDC, x: i32, y: i32, w: i32, _sc: f32, label: &str, fg: Argb, _dim: Argb) {
    let sec_h = (SEC_H as f32 * _sc) as i32;
    // Dim label for section header
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z(label),
       &mut rct(x, y, w - x, y + sec_h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
}

fn edit_display(st: &PanelState) -> String {
    if st.edit_text.is_empty() {
        "|".to_string()
    } else {
        format!("{}|", st.edit_text)
    }
}

fn draw_value_cell(
    mem: HDC, x: i32, y: i32, w: i32, h: i32, _sc: f32,
    text: &str, field: Field, focused: Option<Field>, _dirty: bool,
    fg: Argb, dim: Argb, accent: Argb,
) {
    let is_focused = focused == Some(field);
    let cell_border = if is_focused { accent } else { dim };

    // Background for focused cell
    if is_focused {
        fill_rect(mem, x, y, x + w, y + h, accent.with_alpha(0x30));
    }
    // Border
    let bw = (1.0 * _sc) as i32;
    fill_rect(mem, x, y, x + w, y + bw, cell_border);
    fill_rect(mem, x, y + h - bw, x + w, y + h, cell_border);
    fill_rect(mem, x, y, x + bw, y + h, cell_border);
    fill_rect(mem, x + w - bw, y, x + w, y + h, cell_border);

    // Text
    tcol(mem, fg);
    dw(mem, &mut to_utf16_z(text),
       &mut rct(x + 4, y, x + w - 4, y + h),
       DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
}

fn draw_toggle(mem: HDC, col_x: i32, row_y: i32, vw: i32, row_h: i32, sc: f32, enabled: bool, theme: &NativeTheme) {
    let tog_w = (36.0 * sc) as i32;
    let tog_h = (16.0 * sc) as i32;
    let tx = col_x + (vw - tog_w) / 2;
    let ty = row_y + (row_h - tog_h) / 2;

    let pill = if enabled { theme.accent } else { theme.text_muted };
    fill_rect(mem, tx, ty, tx + tog_w, ty + tog_h, pill);

    // Knob
    let knob_m = (3.0 * sc) as i32;
    let knob_d = tog_h - knob_m * 2;
    let knob_x = if enabled { tx + tog_w - knob_d - knob_m } else { tx + knob_m };
    fill_rect(mem, knob_x, ty + knob_m, knob_x + knob_d, ty + knob_m + knob_d, theme.text);
}

fn draw_core_row(
    mem: HDC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    _sc: f32,
    label: &str,
    load: f32,
    freq_mhz: u32,
    parked: bool,
    theme: &NativeTheme,
) {
    let label_w = (34.0 * _sc) as i32;
    let load_w = (44.0 * _sc) as i32;
    let freq_w = (48.0 * _sc) as i32;
    let gap = (8.0 * _sc) as i32;
    let bar_x = x + label_w + load_w + freq_w + gap * 3;
    let bar_w = (w - (bar_x - x)).max(20);
    let text_col = if parked { theme.text_muted } else { theme.text };
    let load_text = if parked {
        "PARK".to_string()
    } else {
        format!("{:>3}%", (load * 100.0).round() as u32)
    };
    let freq_text = if freq_mhz > 0 {
        format!("{:.1}G", freq_mhz as f32 / 1000.0)
    } else {
        "-.-G".to_string()
    };

    tcol(mem, text_col);
    dw(mem, &mut to_utf16_z(label),
       &mut rct(x, y, x + label_w, y + h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    dw(mem, &mut to_utf16_z(&load_text),
       &mut rct(x + label_w + gap, y, x + label_w + gap + load_w, y + h),
       DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
    dw(mem, &mut to_utf16_z(&freq_text),
       &mut rct(x + label_w + load_w + gap * 2, y, bar_x - gap, y + h),
       DT_RIGHT | DT_SINGLELINE | DT_VCENTER);

    let bar_h = (CORE_BAR_H as f32 * _sc) as i32;
    let bar_y = y + (h - bar_h) / 2;
    draw_core_bar(mem, bar_x, bar_y, bar_w, bar_h, load, parked, theme);
}

fn draw_core_bar(mem: HDC, x: i32, y: i32, w: i32, h: i32, load: f32, parked: bool, theme: &NativeTheme) {
    if parked {
        // Entire bar dim/charcoal
        fill_rect(mem, x, y, x + w, y + h, theme.bar_background);
        return;
    }

    let fill_w = (w as f32 * load) as i32;
    // Fill portion: interpolate between dim and accent based on load
    let fill_color = if load > 0.7 {
        theme.accent
    } else if load > 0.4 {
        // Blend accent and text_muted
        Argb::new(255,
            ((theme.accent.r as u32 * 2 + theme.text_muted.r as u32) / 3) as u8,
            ((theme.accent.g as u32 * 2 + theme.text_muted.g as u32) / 3) as u8,
            ((theme.accent.b as u32 * 2 + theme.text_muted.b as u32) / 3) as u8,
        )
    } else {
        // Green-ish tint derived from accent
        Argb::new(255,
            (theme.accent.r as u32 * 3 / 4) as u8,
            theme.accent.g,
            (theme.accent.b as u32 * 3 / 4) as u8,
        )
    };

    if fill_w > 0 {
        fill_rect(mem, x, y, x + fill_w, y + h, fill_color);
    }
    // Remainder (idle portion)
    if fill_w < w {
        fill_rect(mem, x + fill_w, y, x + w, y + h, theme.bar_background);
    }
}

fn draw_stress_buttons(mem: HDC, win_w: i32, y: i32, h: i32, sc: f32, mon: &MonitorState, theme: &NativeTheme) {
    let rects = stress_button_rects(win_w, y, h, sc, mon.core_load.len());
    let pad_sc = (PAD as f32 * sc) as i32;
    tcol(mem, theme.text_muted);
    dw(mem, &mut to_utf16_z("LOAD"),
       &mut rct(pad_sc, y, pad_sc + (54.0 * sc) as i32, y + h),
       DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    for (level, x1, x2, y1, y2) in rects {
        let label = stress_label(level);
        let is_active = mon.stress_level == level;
        let btn_color = if is_active { theme.accent } else { theme.text_muted };
        fill_rect(mem, x1, y1, x2, y2, btn_color);
        tcol(mem, btn_color.contrasting_text_color());
        dw(mem, &mut to_utf16_z(&label),
           &mut rct(x1, y1, x2, y2),
           DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }
}

fn stress_button_rects(win_w: i32, y: i32, h: i32, sc: f32, core_count: usize) -> Vec<(StressLevel, i32, i32, i32, i32)> {
    let levels = stress_levels(core_count);
    let gap = (4.0 * sc) as i32;
    let btn_h = (18.0 * sc) as i32;
    let by = y + (h - btn_h) / 2;
    let widths: Vec<i32> = levels.iter()
        .map(|level| match level {
            StressLevel::Off => (36.0 * sc) as i32,
            StressLevel::Threads(n) if *n >= 100 => (38.0 * sc) as i32,
            StressLevel::Threads(n) if *n >= 10 => (30.0 * sc) as i32,
            StressLevel::Threads(_) => (24.0 * sc) as i32,
        })
        .collect();
    let total_w: i32 = widths.iter().sum::<i32>() + gap * (levels.len() as i32 - 1);
    let mut x = win_w - (PAD as f32 * sc) as i32 - total_w;
    let mut rects = Vec::with_capacity(levels.len());
    for (level, width) in levels.into_iter().zip(widths) {
        rects.push((level, x, x + width, by, by + btn_h));
        x += width + gap;
    }
    rects
}

fn stress_levels(core_count: usize) -> Vec<StressLevel> {
    let core_count = core_count.max(1);
    const MAX_NUMERIC_BUTTONS: usize = 6;
    let mut levels = Vec::with_capacity(MAX_NUMERIC_BUTTONS + 1);
    levels.push(StressLevel::Off);

    if core_count == 1 {
        levels.push(StressLevel::Threads(1));
        return levels;
    }

    let step = ((core_count - 1) + (MAX_NUMERIC_BUTTONS - 2)) / (MAX_NUMERIC_BUTTONS - 1);
    let mut value = 1usize;
    while value < core_count && levels.len() < MAX_NUMERIC_BUTTONS {
        levels.push(StressLevel::Threads(value));
        value = (value + step).min(core_count);
    }
    if levels.last().copied() != Some(StressLevel::Threads(core_count)) {
        levels.push(StressLevel::Threads(core_count));
    }
    levels
}

fn stress_label(level: StressLevel) -> String {
    match level {
        StressLevel::Off => "OFF".to_string(),
        StressLevel::Threads(count) => count.to_string(),
    }
}

// ── Drawing helpers ──────────────────────────────────────────────────

fn fill_rect(dc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: Argb) {
    if x2 <= x1 || y2 <= y1 { return; }
    let br = unsafe { CreateSolidBrush(color.to_colorref()) };
    let r = RECT { left: x1, top: y1, right: x2, bottom: y2 };
    unsafe { let _ = FillRect(dc, &r, br); _ = DeleteObject(br); }
}

fn tcol(dc: HDC, color: Argb) {
    unsafe { let _ = SetTextColor(dc, color.to_colorref()); _ = SetBkMode(dc, TRANSPARENT); }
}

fn dw(dc: HDC, text: &mut Vec<u16>, rc: &mut RECT, fmt: DRAW_TEXT_FORMAT) {
    unsafe { let _ = DrawTextW(dc, text, rc as *mut RECT, fmt); }
}

fn rct(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

extern "system" fn cpuplan_wndproc(_h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(_h, msg, wp, lp) }
}
