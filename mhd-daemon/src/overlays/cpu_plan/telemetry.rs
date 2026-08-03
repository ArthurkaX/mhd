//! Windows CPU topology, usage, parking, and frequency telemetry.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW,
};
use windows::Win32::System::Power::{
    CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation,
};
use windows::Win32::System::SystemInformation::{
    CpuSetInformation, GetLogicalProcessorInformationEx, GetSystemCpuSetInformation,
    RelationProcessorCore, SYSTEM_CPU_SET_INFORMATION, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::core::PCWSTR;

use crate::osd::to_utf16_z;

use super::MonitorState;

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
pub(super) struct PerfInfo {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
}

// ── CPU topology detection ──────────────────────────────────────────

/// Detect P-core and E-core logical indices using `GetLogicalProcessorInformationEx`.
/// If no hybrid architecture (no P/E cores), all cores go into `p_cores`.
pub(super) fn detect_core_topology() -> (Vec<usize>, Vec<usize>) {
    // First call: get required buffer size
    let mut buf_size: u32 = 0;
    let result =
        unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut buf_size) };
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
        let info = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
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
                let dest = if efficiency == 0 {
                    &mut e_cores
                } else {
                    &mut p_cores
                };
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
pub(super) fn read_perf_info() -> Option<Vec<PerfInfo>> {
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
pub(super) fn compute_loads(prev: &[PerfInfo], curr: &[PerfInfo]) -> Vec<f32> {
    let n = prev.len().min(curr.len());
    let mut loads = Vec::with_capacity(n);
    for i in 0..n {
        let prev_idle = prev[i].idle_time;
        let prev_busy =
            prev[i].kernel_time + prev[i].user_time + prev[i].dpc_time + prev[i].interrupt_time;
        let prev_total = prev_idle + prev_busy;

        let curr_idle = curr[i].idle_time;
        let curr_busy =
            curr[i].kernel_time + curr[i].user_time + curr[i].dpc_time + curr[i].interrupt_time;
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
pub(super) fn read_parked_state() -> Vec<bool> {
    let mut buf_size: u32 = 0;
    // First call to get size
    unsafe {
        let _ = GetSystemCpuSetInformation(None, 0, &mut buf_size, HANDLE::default(), 0);
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
        if size == 0 {
            break;
        }

        if info.Type == CpuSetInformation {
            let cpu_set = unsafe { &info.Anonymous.CpuSet };
            let lp_idx = cpu_set.LogicalProcessorIndex as usize;
            if lp_idx > max_lp {
                max_lp = lp_idx;
            }
        }
        offset += size;
    }

    let mut parked = vec![false; max_lp + 1];

    let mut offset = 0;
    while offset < ret_len as usize {
        let info = unsafe { &*(buf.as_ptr().add(offset) as *const SYSTEM_CPU_SET_INFORMATION) };
        let size = info.Size as usize;
        if size == 0 {
            break;
        }

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
pub(super) fn read_processor_frequencies(count_hint: usize) -> Option<Vec<u32>> {
    let count = count_hint.max(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    );
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

pub(super) fn read_processor_base_mhz(count_hint: usize) -> Option<u32> {
    let count = count_hint.max(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    );
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
        .or_else(|| {
            info.iter()
                .find_map(|p| (p.CurrentMhz > 0).then_some(p.CurrentMhz))
        })
}

pub(super) struct PdhFreqSampler {
    query: isize,
    counters: Vec<isize>,
    base_mhz: u32,
}

impl PdhFreqSampler {
    pub(super) fn new(count: usize, base_mhz: u32) -> Option<Self> {
        if count == 0 || base_mhz == 0 {
            return None;
        }

        let mut query = 0isize;
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }

        let mut counters = Vec::with_capacity(count);
        for i in 0..count {
            let path = to_utf16_z(&format!(
                "\\Processor Information(0,{i})\\% Processor Performance"
            ));
            let mut counter = 0isize;
            let status = unsafe {
                PdhAddEnglishCounterW(query, PCWSTR::from_raw(path.as_ptr()), 0, &mut counter)
            };
            if status == 0 {
                counters.push(counter);
            }
        }

        if counters.is_empty() {
            unsafe {
                let _ = PdhCloseQuery(query);
            }
            return None;
        }

        let sampler = Self {
            query,
            counters,
            base_mhz,
        };
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
            let status =
                unsafe { PdhGetFormattedCounterValue(*counter, PDH_FMT_DOUBLE, None, &mut value) };
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
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

pub(super) fn read_effective_processor_frequencies(mon: &MonitorState) -> Option<Vec<u32>> {
    mon.freq_sampler
        .as_ref()
        .and_then(PdhFreqSampler::collect)
        .or_else(|| read_processor_frequencies(mon.core_freq_mhz.len()))
}
