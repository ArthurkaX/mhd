//! Windows power-plan API and value persistence for the CPU overlay.

use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::System::Power::{
    ACCESS_SCHEME, PowerEnumerate, PowerGetActiveScheme, PowerReadACValueIndex,
    PowerReadDCValueIndex, PowerReadFriendlyName, PowerSetActiveScheme, PowerWriteACValueIndex,
    PowerWriteDCValueIndex,
};
use windows::core::GUID;

use super::PlanValues;

// ── GUIDs for processor power settings ───────────────────────────────
pub(crate) const GUID_PROCESSOR_SUBGROUP: GUID =
    GUID::from_u128(0x54533251_82be_4824_96c1_47b60b740d00);
const GUID_PARKING_MIN: GUID = GUID::from_u128(0x0cc5b647_c1df_4637_891a_dec35c318583);
const GUID_PARKING_MAX: GUID = GUID::from_u128(0xea062031_0e34_4ff1_9b6d_eb1059334028);
// Minimum processor performance state (% of max frequency the CPU can drop to)
const GUID_MIN_PROC_STATE: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964c);
const GUID_MIN_PROC_STATE_CLASS1: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964d);
const GUID_MIN_PROC_STATE_CLASS2: GUID = GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964e);
// Maximum processor performance state (% of max frequency the CPU can go to)
pub(crate) const GUID_MAX_PROC_STATE: GUID =
    GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ec);
pub(crate) const GUID_MAX_PROC_STATE_CLASS1: GUID =
    GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ed);
pub(crate) const GUID_MAX_PROC_STATE_CLASS2: GUID =
    GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ee);
// Processor performance autonomous mode — 0=disabled, 1=enabled.
const GUID_PERF_AUTONOMOUS_MODE: GUID = GUID::from_u128(0x8baa4a8a_14c6_4451_8e8b_14bdbd197537);
// Processor performance boost mode — 0=disabled, 1=enabled, 2=aggressive, etc.
pub(crate) const GUID_PERF_BOOST_MODE: GUID =
    GUID::from_u128(0xbe337238_0d82_4146_a960_4f3749d470c7);
// Processor performance increase policy — 0=Ideal (gradual), 2=Rocket (instant max).
const GUID_INCREASE_POLICY: GUID = GUID::from_u128(0x465e1f50_b610_473a_ab58_00d1077dc418);
// Heterogeneous processor scheduling policy — 0=All, 2=PreferPerf, 4=PreferEff, 5=Auto.
const GUID_HETEROGENEOUS_POLICY: GUID = GUID::from_u128(0x7f2f5cfa_f10c_4823_b5e1_e93ae85f46b5);
// Performance state of parked cores — 0=NoPref, 1=Deepest, 2=Lightest.
const GUID_PARKED_CORE_PERF: GUID = GUID::from_u128(0x447235c7_6a8d_4cc0_8e24_9eaf70b96e2b);
// System cooling policy — 0=Passive (reduce freq), 1=Active (spin fans).
pub(crate) const GUID_COOLING_POLICY: GUID =
    GUID::from_u128(0x94d3a615_a899_4ac5_ae2b_e4d8f634367f);

// ── Win32 error constant ─────────────────────────────────────────────
const ERROR_NO_MORE_ITEMS: u32 = 259;

// ── Power-plan operations ────────────────────────────────────────────

pub(crate) fn enumerate_schemes() -> Vec<(GUID, String)> {
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
        let len = buf
            .chunks(2)
            .take_while(|c| c.len() == 2 && !(c[0] == 0 && c[1] == 0))
            .count();
        let name =
            String::from_utf16_lossy(std::slice::from_raw_parts(buf.as_ptr() as *const u16, len));
        Some(name)
    }
}

pub(crate) fn get_active_scheme_guid() -> GUID {
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

pub(crate) fn set_active_scheme(guid: GUID) {
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

pub(crate) fn write_ac_value(scheme: &GUID, sub: &GUID, setting: &GUID, value: u32) {
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

pub(crate) fn write_dc_value(scheme: &GUID, sub: &GUID, setting: &GUID, value: u32) {
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

pub(super) fn read_current_plan_values() -> (PlanValues, PlanValues) {
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
        min_cores: read_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN),
        max_cores: read_ac_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX),
        min_freq: read_ac_setting_max(&guid, &min_freq_settings),
        max_freq: read_ac_setting_min_nonzero(&guid, &max_freq_settings),
        autonomous_mode: read_power_setting_ac(&guid, &GUID_PERF_AUTONOMOUS_MODE) != 0,
        turbo: read_power_setting_ac(&guid, &GUID_PERF_BOOST_MODE) != 0,
        cooling_policy: read_power_setting_ac(&guid, &GUID_COOLING_POLICY),
        increase_policy: read_power_setting_ac(&guid, &GUID_INCREASE_POLICY),
        hetero_policy: read_power_setting_ac(&guid, &GUID_HETEROGENEOUS_POLICY),
        parked_perf: read_power_setting_ac(&guid, &GUID_PARKED_CORE_PERF),
    };
    let dc = PlanValues {
        min_cores: read_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MIN),
        max_cores: read_dc_value(&guid, &GUID_PROCESSOR_SUBGROUP, &GUID_PARKING_MAX),
        min_freq: read_dc_setting_max(&guid, &min_freq_settings),
        max_freq: read_dc_setting_min_nonzero(&guid, &max_freq_settings),
        autonomous_mode: read_power_setting_dc(&guid, &GUID_PERF_AUTONOMOUS_MODE) != 0,
        turbo: read_power_setting_dc(&guid, &GUID_PERF_BOOST_MODE) != 0,
        cooling_policy: read_power_setting_dc(&guid, &GUID_COOLING_POLICY),
        increase_policy: read_power_setting_dc(&guid, &GUID_INCREASE_POLICY),
        hetero_policy: read_power_setting_dc(&guid, &GUID_HETEROGENEOUS_POLICY),
        parked_perf: read_power_setting_dc(&guid, &GUID_PARKED_CORE_PERF),
    };
    (ac, dc)
}

pub(super) fn write_plan_values(ac: &PlanValues, dc: &PlanValues) {
    let guid = get_active_scheme_guid();
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKING_MIN,
        ac.min_cores,
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKING_MAX,
        ac.max_cores,
    );
    write_ac_values(
        &guid,
        &[
            GUID_MIN_PROC_STATE,
            GUID_MIN_PROC_STATE_CLASS1,
            GUID_MIN_PROC_STATE_CLASS2,
        ],
        ac.min_freq,
    );
    write_ac_values(
        &guid,
        &[
            GUID_MAX_PROC_STATE,
            GUID_MAX_PROC_STATE_CLASS1,
            GUID_MAX_PROC_STATE_CLASS2,
        ],
        ac.max_freq,
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PERF_AUTONOMOUS_MODE,
        if ac.autonomous_mode { 1 } else { 0 },
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PERF_BOOST_MODE,
        if ac.turbo { 1 } else { 0 },
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_COOLING_POLICY,
        ac.cooling_policy,
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_INCREASE_POLICY,
        ac.increase_policy,
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_HETEROGENEOUS_POLICY,
        ac.hetero_policy,
    );
    write_ac_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKED_CORE_PERF,
        ac.parked_perf,
    );

    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKING_MIN,
        dc.min_cores,
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKING_MAX,
        dc.max_cores,
    );
    write_dc_values(
        &guid,
        &[
            GUID_MIN_PROC_STATE,
            GUID_MIN_PROC_STATE_CLASS1,
            GUID_MIN_PROC_STATE_CLASS2,
        ],
        dc.min_freq,
    );
    write_dc_values(
        &guid,
        &[
            GUID_MAX_PROC_STATE,
            GUID_MAX_PROC_STATE_CLASS1,
            GUID_MAX_PROC_STATE_CLASS2,
        ],
        dc.max_freq,
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PERF_AUTONOMOUS_MODE,
        if dc.autonomous_mode { 1 } else { 0 },
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PERF_BOOST_MODE,
        if dc.turbo { 1 } else { 0 },
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_COOLING_POLICY,
        dc.cooling_policy,
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_INCREASE_POLICY,
        dc.increase_policy,
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_HETEROGENEOUS_POLICY,
        dc.hetero_policy,
    );
    write_dc_value(
        &guid,
        &GUID_PROCESSOR_SUBGROUP,
        &GUID_PARKED_CORE_PERF,
        dc.parked_perf,
    );

    unsafe {
        let _ = PowerSetActiveScheme(None, Some(&guid as *const GUID));
    }
}

fn read_ac_setting_max(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings
        .iter()
        .map(|setting| read_ac_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .max()
        .unwrap_or(0)
}

fn read_dc_setting_max(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings
        .iter()
        .map(|setting| read_dc_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .max()
        .unwrap_or(0)
}

fn read_ac_setting_min_nonzero(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings
        .iter()
        .map(|setting| read_ac_value(scheme, &GUID_PROCESSOR_SUBGROUP, setting))
        .filter(|value| *value > 0)
        .min()
        .unwrap_or(0)
}

fn read_dc_setting_min_nonzero(scheme: &GUID, settings: &[GUID]) -> u32 {
    settings
        .iter()
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
