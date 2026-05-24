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

/// Helper: open the Run key with desired access, call `f`, then close the
/// key.  Returns `f`'s result.
unsafe fn with_run_key<R>(
    desired_access: REG_SAM_FLAGS,
    f: impl FnOnce(HKEY) -> Result<R, String>,
) -> Result<R, String> {
    let path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let mut key = HKEY::default();

    let ret = unsafe {
        RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR::from_raw(path.as_ptr()),
        0,
        desired_access,
        &mut key,
    )
    };
    if ret != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW failed: {ret:?}"));
    }

    let result = f(key);

    // Always close the key — even if `f` returned an error the handle
    // is still open.
    unsafe {
        let _ = RegCloseKey(key);
    }

    result
}

/// Install autostart by writing to `HKCU\…\Run`.
pub fn install_autostart() -> Result<(), String> {
    let exe_str = exe_path()?;
    let wide_value = to_wide(&exe_str);
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        with_run_key(KEY_SET_VALUE, |key| {
            // RegSetValueExW expects lpdata as Option<&[u8]>; include the
            // null terminator in the byte count for a proper REG_SZ.
            let slice = std::slice::from_raw_parts(
                wide_value.as_ptr().cast::<u8>(),
                wide_value.len() * 2,
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
        })
    }
}

/// Remove autostart by deleting the value from `HKCU\…\Run`.
pub fn remove_autostart() -> Result<(), String> {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        with_run_key(KEY_SET_VALUE, |key| {
            let ret = RegDeleteValueW(key, PCWSTR::from_raw(wide_name.as_ptr()));
            if ret != ERROR_SUCCESS {
                return Err(format!("RegDeleteValueW failed: {ret:?}"));
            }
            Ok(())
        })
    }
}

/// Check whether autostart is currently enabled (value exists).
pub fn is_autostart_enabled() -> bool {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        let result = with_run_key(KEY_QUERY_VALUE, |key| {
            let ret = RegQueryValueExW(
                key,
                PCWSTR::from_raw(wide_name.as_ptr()),
                None, // lpreserved
                None, // lptype
                None, // lpdata
                None, // lpcbdata
            );
            if ret == ERROR_SUCCESS {
                Ok(true)
            } else {
                Ok(false)
            }
        });
        result.unwrap_or(false)
    }
}
