//! Monitor control via DDC/CI (dxva2.dll).
//!
//! Provides brightness, contrast, audio volume and input source control
//! for physical monitors. Uses the DXVA2 High-Level Monitor API loaded
//! dynamically from `dxva2.dll`.

use std::mem::transmute;
use std::sync::LazyLock;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::{BOOL, HANDLE, POINT};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetProcAddress;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

// ── FFI types ───────────────────────────────────────────────────────────

pub type PhysicalMonitorHandle = HANDLE;

type GetNumberOfPhysicalMonitorsFromHMONITORFn =
    unsafe extern "system" fn(HMONITOR, *mut u32) -> BOOL;
type GetPhysicalMonitorsFromHMONITORFn =
    unsafe extern "system" fn(HMONITOR, u32, *mut PhysicalMonitor) -> BOOL;
type GetMonitorBrightnessFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, *mut u32, *mut u32, *mut u32) -> BOOL;
type SetMonitorBrightnessFn = unsafe extern "system" fn(PhysicalMonitorHandle, u32) -> BOOL;
type GetVCPFeatureAndVCPFeatureReplyFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, u8, *mut u32, *mut u32, *mut u32) -> BOOL;
type SetVCPFeatureFn = unsafe extern "system" fn(PhysicalMonitorHandle, u8, u32) -> BOOL;
type GetCapabilitiesStringLengthFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, *mut u32) -> BOOL;
type CapabilitiesRequestAndCapabilitiesReplyFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, *mut u16, u32) -> BOOL;

#[repr(C)]
#[derive(Clone)]
struct PhysicalMonitor {
    handle: PhysicalMonitorHandle,
    description: [u16; 128],
}

// ── Dxva2 singleton ─────────────────────────────────────────────────────

static DXVA2: LazyLock<Result<Dxva2, String>> = LazyLock::new(Dxva2::load);

struct Dxva2 {
    get_number: GetNumberOfPhysicalMonitorsFromHMONITORFn,
    get_physical: GetPhysicalMonitorsFromHMONITORFn,
    get_brightness: GetMonitorBrightnessFn,
    set_brightness: SetMonitorBrightnessFn,
    get_vcp: GetVCPFeatureAndVCPFeatureReplyFn,
    set_vcp: SetVCPFeatureFn,
    caps_length: GetCapabilitiesStringLengthFn,
    caps_reply: CapabilitiesRequestAndCapabilitiesReplyFn,
}

impl Dxva2 {
    fn load() -> Result<Self, String> {
        unsafe {
            let name = windows::core::PCSTR::from_raw(c"dxva2.dll".as_ptr() as *const u8);
            let module = windows::Win32::System::LibraryLoader::LoadLibraryA(name)
                .map_err(|e| format!("cannot load dxva2.dll: {e}"))?;

            macro_rules! load {
                ($fn_name:literal, $type:ty) => {{
                    let addr: unsafe extern "system" fn() -> isize = GetProcAddress(
                        module,
                        windows::core::PCSTR::from_raw(
                            concat!($fn_name, "\0").as_ptr() as *const u8
                        ),
                    )
                    .ok_or_else(|| format!("cannot find {} in dxva2.dll", $fn_name))?;
                    transmute::<unsafe extern "system" fn() -> isize, $type>(addr)
                }};
            }

            Ok(Dxva2 {
                get_number: load!(
                    "GetNumberOfPhysicalMonitorsFromHMONITOR",
                    GetNumberOfPhysicalMonitorsFromHMONITORFn
                ),
                get_physical: load!(
                    "GetPhysicalMonitorsFromHMONITOR",
                    GetPhysicalMonitorsFromHMONITORFn
                ),
                get_brightness: load!("GetMonitorBrightness", GetMonitorBrightnessFn),
                set_brightness: load!("SetMonitorBrightness", SetMonitorBrightnessFn),
                get_vcp: load!(
                    "GetVCPFeatureAndVCPFeatureReply",
                    GetVCPFeatureAndVCPFeatureReplyFn
                ),
                set_vcp: load!("SetVCPFeature", SetVCPFeatureFn),
                caps_length: load!("GetCapabilitiesStringLength", GetCapabilitiesStringLengthFn),
                caps_reply: load!(
                    "CapabilitiesRequestAndCapabilitiesReply",
                    CapabilitiesRequestAndCapabilitiesReplyFn
                ),
            })
        }
    }
}

// ── Retry helper ────────────────────────────────────────────────────────

/// Call a DDC operation with up to `crate::constants::DDC_MAX_RETRIES` retries.
/// Logs failures via `eprintln!` with context.
fn retry_ddc<F, T>(label: &str, mut f: F) -> Result<T, String>
where
    F: FnMut() -> Result<T, String>,
{
    let max = crate::constants::DDC_MAX_RETRIES;
    let base_ms = crate::constants::DDC_RETRY_BASE_MS;
    let mut last_err = String::new();
    for attempt in 1..=max {
        match f() {
            Ok(val) => return Ok(val),
            Err(e) => {
                last_err = e;
                if attempt < max {
                    let delay = base_ms * (1u64 << (attempt - 1)); // 10, 20, 40ms
                    eprintln!(
                        "mhd: DDC/CI {} failed (attempt {}/{}): {}; retrying in {}ms",
                        label, attempt, max, last_err, delay
                    );
                    sleep(Duration::from_millis(delay));
                }
            }
        }
    }
    Err(format!(
        "DDC/CI {} failed after {} attempts: {}",
        label, max, last_err
    ))
}

// ── Monitor info ────────────────────────────────────────────────────────

/// Information about a detected physical monitor.
#[derive(Debug, Clone)]
pub struct PhysicalMonitorInfo {
    pub handle: PhysicalMonitorHandle,
    pub name: String,
}

impl PhysicalMonitorInfo {
    /// Get the monitor's capabilities string (MCCS).
    pub fn capabilities(&self) -> Result<String, String> {
        let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
        get_capabilities_inner(dxva2, self.handle)
    }

    /// Get brightness: (current, min, max).
    /// Retries up to 3 times on transient DDC/CI failures.
    pub fn get_brightness(&self) -> Result<(u32, u32, u32), String> {
        let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
        let handle = self.handle;
        retry_ddc("get_brightness", || unsafe {
            let mut min = 0u32;
            let mut cur = 0u32;
            let mut max = 0u32;
            if !(dxva2.get_brightness)(handle, &mut min, &mut cur, &mut max).as_bool() {
                return Err("cannot get brightness".to_string());
            }
            Ok((cur, min, max))
        })
    }

    /// Set brightness (0-100).
    /// Retries up to 3 times on transient DDC/CI failures.
    #[allow(dead_code)]
    pub fn set_brightness(&self, value: u32) -> Result<(), String> {
        let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
        let handle = self.handle;
        let v = value.min(100);
        retry_ddc("set_brightness", || unsafe {
            if !(dxva2.set_brightness)(handle, v).as_bool() {
                return Err("cannot set brightness".to_string());
            }
            Ok(())
        })
    }

    /// Get a VCP feature value: returns (vcp_type, current, max).
    /// vcp_type: 0=continuous, 1=non-continuous, 2=value-only.
    /// Retries up to 3 times on transient DDC/CI failures.
    pub fn get_vcp(&self, code: u8) -> Result<VcpValue, String> {
        let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
        let handle = self.handle;
        retry_ddc(&format!("get_vcp(0x{code:02X})"), || unsafe {
            let mut vcp_type = 0u32;
            let mut cur = 0u32;
            let mut max = 0u32;
            if !(dxva2.get_vcp)(handle, code, &mut vcp_type, &mut cur, &mut max).as_bool() {
                return Err(format!("VCP feature 0x{code:02X} not supported"));
            }
            Ok(VcpValue {
                vcp_type,
                current: cur,
                max,
            })
        })
    }

    /// Set a VCP feature value.
    /// Retries up to 3 times on transient DDC/CI failures.
    pub fn set_vcp(&self, code: u8, value: u32) -> Result<(), String> {
        let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
        let handle = self.handle;
        retry_ddc(&format!("set_vcp(0x{code:02X})"), || unsafe {
            if !(dxva2.set_vcp)(handle, code, value).as_bool() {
                return Err(format!("cannot set VCP feature 0x{code:02X}"));
            }
            Ok(())
        })
    }
}

/// Result of querying a VCP feature.
#[derive(Debug, Clone, Copy)]
pub struct VcpValue {
    /// 0=continuous (NORMAL), 1=non-continuous (TABLE), 2=value-only.
    #[allow(dead_code)]
    pub vcp_type: u32,
    pub current: u32,
    pub max: u32,
}

/// Parsed supported VCP code.
#[derive(Debug, Clone)]
pub struct SupportedVcp {
    pub code: u8,
    /// Possible values for non-continuous features, if known.
    pub values: Option<Vec<u32>>,
}

// ── Capabilities string parsing ─────────────────────────────────────────

/// Parse the `vcp(...)` section of an MCCS capabilities string.
/// Returns the list of supported VCP codes and, for non-continuous features,
/// their possible values if specified.
///
/// Example input:
///   `vcp(10 12 60(01 03 04) 62 87)`
/// Returns:
///   [{code: 0x10, values: None}, {code: 0x12, values: None},
///    {code: 0x60, values: Some([0x01, 0x03, 0x04])}, ...]
pub fn parse_capabilities_vcp(s: &str) -> Vec<SupportedVcp> {
    // Find the vcp(...) section
    let s_lower = s.to_lowercase();
    let start = match s_lower.find("vcp(") {
        Some(i) => i + 4, // skip "vcp("
        None => return Vec::new(),
    };

    // Find the matching closing paren for vcp(
    let mut depth = 1;
    let mut end = start;
    for (i, ch) in s[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Vec::new(); // malformed
    }

    let content = &s[start..end];

    // Tokenize: split on whitespace while respecting parenthesised groups
    let mut result: Vec<SupportedVcp> = Vec::new();
    let mut i = 0;
    let bytes = content.as_bytes();
    while i < bytes.len() {
        // Skip whitespace
        if bytes[i] == b' ' || bytes[i] == b'\t' {
            i += 1;
            continue;
        }

        if bytes[i] == b'(' {
            // Skip value group for the *next* feature code (we handle it above)
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            continue;
        }

        // Read hex code (two hex chars) or hex code with group
        // Format: `10` or `60(01 03 04)` or `10`
        let hex_start = i;
        while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' && bytes[i] != b'(' {
            i += 1;
        }

        let code_str = &content[hex_start..i];
        if let Ok(code) = u8::from_str_radix(code_str.trim(), 16) {
            // Check if followed by a parenthesised value group
            let mut values: Option<Vec<u32>> = None;
            if i < bytes.len() && bytes[i] == b'(' {
                // Parse value group
                i += 1; // skip '('
                let mut vals: Vec<u32> = Vec::new();
                let val_start = i;
                while i < bytes.len() && bytes[i] != b')' {
                    i += 1;
                }
                let val_str = &content[val_start..i].trim();
                if !val_str.is_empty() {
                    for tok in val_str.split_whitespace() {
                        if let Ok(v) = u32::from_str_radix(tok, 16) {
                            vals.push(v);
                        }
                    }
                }
                if !vals.is_empty() {
                    values = Some(vals);
                }
                if i < bytes.len() {
                    i += 1; // skip ')'
                }
            }
            result.push(SupportedVcp { code, values });
        }
    }

    result
}

// ── Helper: get capabilities string ─────────────────────────────────────

fn get_capabilities_inner(dxva2: &Dxva2, handle: PhysicalMonitorHandle) -> Result<String, String> {
    unsafe {
        let mut len = 0u32;
        if !(dxva2.caps_length)(handle, &mut len).as_bool() || len == 0 {
            return Err("cannot get capabilities string length".to_string());
        }

        let mut buf: Vec<u16> = vec![0u16; len as usize];
        if !(dxva2.caps_reply)(handle, buf.as_mut_ptr(), len).as_bool() {
            return Err("cannot get capabilities string".to_string());
        }

        // The string may be null-terminated within the buffer
        let actual_len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..actual_len]))
    }
}

// ── Cursor-based public API ───────────────────────────────────────────

/// Get the physical monitor(s) under the mouse cursor.
fn cursor_monitor_raw() -> Result<(&'static Dxva2, PhysicalMonitorHandle, String), String> {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let (handle, name) = get_physical_monitors_for_hmon(dxva2, hmon)?;
    Ok((dxva2, handle, name))
}

pub fn adjust_brightness(delta: i32) -> Result<(), String> {
    let (dxva2, handle, _) = cursor_monitor_raw()?;
    // Use retry for the critical get+set sequence
    let new = retry_ddc("adjust_brightness/get", || unsafe {
        let mut min = 0u32;
        let mut cur = 0u32;
        let mut max = 0u32;
        if !(dxva2.get_brightness)(handle, &mut min, &mut cur, &mut max).as_bool() {
            return Err("cannot get brightness".to_string());
        }
        Ok((cur as i32 + delta).clamp(0, 100) as u32)
    })?;
    retry_ddc("adjust_brightness/set", || unsafe {
        if !(dxva2.set_brightness)(handle, new).as_bool() {
            return Err("cannot set brightness".to_string());
        }
        Ok(())
    })
}

pub fn set_brightness_absolute(value: u32) -> Result<(), String> {
    let (dxva2, handle, _) = cursor_monitor_raw()?;
    let v = value.min(100);
    retry_ddc("set_brightness_absolute", || unsafe {
        if !(dxva2.set_brightness)(handle, v).as_bool() {
            return Err("cannot set brightness".to_string());
        }
        Ok(())
    })
}

pub fn get_brightness() -> Result<(u32, String), String> {
    let (dxva2, handle, name) = cursor_monitor_raw()?;
    let value = retry_ddc("get_brightness", || unsafe {
        let mut min = 0u32;
        let mut cur = 0u32;
        let mut max = 0u32;
        if !(dxva2.get_brightness)(handle, &mut min, &mut cur, &mut max).as_bool() {
            return Err("cannot get brightness".to_string());
        }
        Ok(cur)
    })?;
    Ok((value, name))
}

pub fn set_vcp_feature(code: u8, value: u32) -> Result<(), String> {
    let (dxva2, handle, _) = cursor_monitor_raw()?;
    retry_ddc(&format!("set_vcp(0x{code:02X})"), || unsafe {
        if !(dxva2.set_vcp)(handle, code, value).as_bool() {
            return Err(format!("cannot set VCP feature 0x{code:02X}"));
        }
        Ok(())
    })
}

pub fn adjust_vcp_feature(code: u8, delta: i32) -> Result<(), String> {
    let (dxva2, handle, _) = cursor_monitor_raw()?;
    let new = retry_ddc(&format!("adjust_vcp(0x{code:02X})/get"), || unsafe {
        let mut vcp_type = 0u32;
        let mut cur = 0u32;
        let mut max = 0u32;
        if !(dxva2.get_vcp)(handle, code, &mut vcp_type, &mut cur, &mut max).as_bool() {
            return Err(format!("cannot get VCP feature 0x{code:02X}"));
        }
        Ok((cur as i32 + delta).clamp(0, max as i32) as u32)
    })?;
    retry_ddc(&format!("adjust_vcp(0x{code:02X})/set"), || unsafe {
        if !(dxva2.set_vcp)(handle, code, new).as_bool() {
            return Err(format!("cannot set VCP feature 0x{code:02X}"));
        }
        Ok(())
    })
}

// ── Cursor-based enumeration for monitor panel ─────────────────────────

/// Enumerate physical monitors under the cursor display.
pub fn enumerate_cursor_monitor() -> Result<Vec<PhysicalMonitorInfo>, String> {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
    get_physical_monitors_for_hmon(dxva2, hmon)
        .map(|(handle, name)| vec![PhysicalMonitorInfo { handle, name }])
}

// ── New: enumerate ALL physical monitors ────────────────────────────────

#[allow(dead_code)]
/// Enumerate all physical monitors across all display monitors.
/// Returns a list of (handle, name) for each physical monitor.
pub fn enumerate_all_monitors() -> Result<Vec<PhysicalMonitorInfo>, String> {
    let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;

    // Use EnumDisplayMonitors to get all HMONITOR handles
    let hmons = enumerate_display_monitors();

    let mut result: Vec<PhysicalMonitorInfo> = Vec::new();
    for hmon in hmons {
        match get_physical_monitors_for_hmon(dxva2, hmon) {
            Ok((handle, name)) => {
                result.push(PhysicalMonitorInfo { handle, name });
            }
            Err(_) => {
                // Skip monitors that fail — they may not support DXVA2
            }
        }
    }

    if result.is_empty() {
        return Err("no physical monitors found".to_string());
    }

    Ok(result)
}

#[allow(dead_code)]
fn enumerate_display_monitors() -> Vec<HMONITOR> {
    let mut monitors: Vec<HMONITOR> = Vec::new();

    unsafe extern "system" fn monitor_enum_proc(
        hmon: HMONITOR,
        _hdc: windows::Win32::Graphics::Gdi::HDC,
        _rect: *mut windows::Win32::Foundation::RECT,
        data: windows::Win32::Foundation::LPARAM,
    ) -> BOOL {
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(hmon);
        BOOL(1) // continue enumeration
    }

    unsafe {
        let data = &mut monitors as *mut Vec<HMONITOR>;
        let _ = windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(monitor_enum_proc),
            windows::Win32::Foundation::LPARAM(data as isize),
        );
    }

    monitors
}

fn get_physical_monitors_for_hmon(
    dxva2: &Dxva2,
    hmon: HMONITOR,
) -> Result<(PhysicalMonitorHandle, String), String> {
    unsafe {
        let mut count: u32 = 0;
        if !(dxva2.get_number)(hmon, &mut count).as_bool() || count == 0 {
            return Err("no physical monitors found for HMONITOR".to_string());
        }

        let mut monitors: Vec<PhysicalMonitor> = Vec::with_capacity(count as usize);
        if !(dxva2.get_physical)(hmon, count, monitors.as_mut_ptr()).as_bool() {
            return Err("cannot get physical monitors for HMONITOR".to_string());
        }

        monitors.set_len(count as usize);

        let handle = monitors[0].handle;

        let desc_u16 = &monitors[0].description;
        let len = desc_u16
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc_u16.len());
        let name = String::from_utf16_lossy(&desc_u16[..len]);

        std::mem::forget(monitors); // leak — OS owns the structs
        Ok((handle, name))
    }
}

// ── Legacy API wrappers (backwards compatibility) ───────────────────────

// ── Enumerate ALL physical monitors (for compatibility) ─────────────────
