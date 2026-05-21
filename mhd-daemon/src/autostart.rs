//! Windows autostart via the `HKCU\…\Run` registry key.
//!
//! This is the standard mechanism for tray‑based applications — the app
//! runs at user logon **with normal user privileges**, so the tray icon
//! is visible (elevated processes cannot show tray icons in Windows).
//!
//! Registry path:
//!   HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run
//!
//! No admin rights are required to read/write this key.

use windows::Win32::System::Registry::*;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::core::PCWSTR;

/// Registry value name for the mHD autostart entry.
const VALUE_NAME: &str = "mHD";

/// Convert a Rust string to a null‑terminated UTF‑16 vector.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Get the full path to the current executable.
fn exe_path() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("cannot determine exe path: {e}"))
}

/// Install autostart by writing to `HKCU\…\Run`.
pub fn install_autostart() -> Result<(), String> {
    let exe_str = exe_path()?;
    let wide_value = to_wide(&exe_str);
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        let path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let mut key = HKEY::default();

        let ret = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut key,
        );
        if ret != ERROR_SUCCESS {
            return Err(format!("RegOpenKeyExW failed: {ret:?}"));
        }

        // RegSetValueExW expects lpdata as Option<&[u8]>, the slice length is byte count
        let slice = std::slice::from_raw_parts(
            wide_value.as_ptr().cast::<u8>(),
            (wide_value.len() - 1) * 2,
        );

        let ret = RegSetValueExW(
            key,
            PCWSTR::from_raw(wide_name.as_ptr()),
            0,
            REG_SZ,
            Some(slice),
        );
        if ret != ERROR_SUCCESS {
            return Err(format!("RegSetValueExW failed: {ret:?}"));
        }

        Ok(())
    }
}

/// Remove autostart by deleting the value from `HKCU\…\Run`.
pub fn remove_autostart() -> Result<(), String> {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        let path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let mut key = HKEY::default();

        let ret = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut key,
        );
        if ret != ERROR_SUCCESS {
            return Err(format!("RegOpenKeyExW failed: {ret:?}"));
        }

        let ret = RegDeleteValueW(key, PCWSTR::from_raw(wide_name.as_ptr()));
        if ret != ERROR_SUCCESS {
            return Err(format!("RegDeleteValueW failed: {ret:?}"));
        }

        Ok(())
    }
}

/// Check whether autostart is currently enabled (value exists).
#[allow(dead_code)]
pub fn is_autostart_enabled() -> bool {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        let path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let mut key = HKEY::default();

        let ret = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        );
        if ret != ERROR_SUCCESS {
            return false;
        }

        let ret = RegQueryValueExW(
            key,
            PCWSTR::from_raw(wide_name.as_ptr()),
            None,  // lpreserved
            None,  // lptype
            None,  // lpdata
            None,  // lpcbdata
        );
        ret == ERROR_SUCCESS
    }
}
