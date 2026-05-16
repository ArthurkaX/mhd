//! Monitor brightness control via DDC/CI (dxva2.dll).

use std::mem::transmute;

use windows::core::PCSTR;
use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::LibraryLoader::GetProcAddress;
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

type PhysicalMonitorHandle = HANDLE;

type GetNumberOfPhysicalMonitorsFromHMONITORFn =
    unsafe extern "system" fn(HMONITOR, *mut u32) -> BOOL;
type GetPhysicalMonitorsFromHMONITORFn =
    unsafe extern "system" fn(HMONITOR, u32, *mut PhysicalMonitor) -> BOOL;
type GetMonitorBrightnessFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, *mut u32, *mut u32, *mut u32) -> BOOL;
type SetMonitorBrightnessFn =
    unsafe extern "system" fn(PhysicalMonitorHandle, u32) -> BOOL;

#[repr(C)]
struct PhysicalMonitor {
    handle: PhysicalMonitorHandle,
    description: [u16; 128],
}

struct Dxva2 {
    get_number: GetNumberOfPhysicalMonitorsFromHMONITORFn,
    get_physical: GetPhysicalMonitorsFromHMONITORFn,
    get_brightness: GetMonitorBrightnessFn,
    set_brightness: SetMonitorBrightnessFn,
}

impl Dxva2 {
    fn load() -> Result<Self, String> {
        // LoadLibraryA + transmute: the dxva2.dll is a system DLL, signatures are well-known.
        unsafe {
            let name = PCSTR::from_raw("dxva2.dll\0".as_ptr());
            let module = windows::Win32::System::LibraryLoader::LoadLibraryA(name)
                .map_err(|e| format!("cannot load dxva2.dll: {e}"))?;

            macro_rules! load {
                ($fn_name:literal, $type:ty) => {{
                    let addr: unsafe extern "system" fn() -> isize =
                        GetProcAddress(module, PCSTR::from_raw(concat!($fn_name, "\0").as_ptr()))
                            .ok_or_else(|| {
                                format!("cannot find {} in dxva2.dll", $fn_name)
                            })?;
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
            })
        }
    }
}

fn primary_monitor() -> HMONITOR {
    unsafe {
        let desktop = GetDesktopWindow();
        MonitorFromWindow(desktop, MONITOR_DEFAULTTONEAREST)
    }
}

fn first_physical_handle(dxva2: &Dxva2, hmon: HMONITOR) -> Result<PhysicalMonitorHandle, String> {
    unsafe {
        let mut count: u32 = 0;
        if !(dxva2.get_number)(hmon, &mut count).as_bool() || count == 0 {
            return Err("no physical monitors found".to_string());
        }

        let mut monitors: Vec<PhysicalMonitor> = Vec::with_capacity(count as usize);
        if !(dxva2.get_physical)(hmon, count, monitors.as_mut_ptr()).as_bool() {
            return Err("cannot get physical monitors".to_string());
        }
        monitors.set_len(count as usize);

        let handle = monitors[0].handle;
        std::mem::forget(monitors); // leak — OS owns the structs
        Ok(handle)
    }
}

pub fn get_brightness() -> Result<u32, String> {
    let dxva2 = Dxva2::load()?;
    let hmon = primary_monitor();
    let handle = first_physical_handle(&dxva2, hmon)?;

    unsafe {
        let mut min = 0u32;
        let mut cur = 0u32;
        let mut max = 0u32;
        if !(dxva2.get_brightness)(handle, &mut min, &mut cur, &mut max).as_bool() {
            return Err("cannot get brightness".to_string());
        }
        Ok(cur)
    }
}

pub fn set_brightness_absolute(value: u32) -> Result<(), String> {
    let v = value.min(100);
    let dxva2 = Dxva2::load()?;
    let hmon = primary_monitor();
    let handle = first_physical_handle(&dxva2, hmon)?;

    unsafe {
        if !(dxva2.set_brightness)(handle, v).as_bool() {
            return Err("cannot set brightness".to_string());
        }
    }
    Ok(())
}

pub fn adjust_brightness(delta: i32) -> Result<(), String> {
    let current = get_brightness()? as i32;
    let new = (current + delta).clamp(0, 100) as u32;
    set_brightness_absolute(new)
}
