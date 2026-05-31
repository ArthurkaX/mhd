//! Windows autostart via Task Scheduler with a `HKCU\...\Run` fallback.
//!
//! The preferred path is a per-user logon scheduled task with highest
//! available privileges. If Task Scheduler refuses creation, the code falls
//! back to the standard Run key used by tray-based applications.
//!
//! Fallback registry path:
//!   HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run

use std::process::Command;

use windows::Win32::System::Registry::*;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::core::PCWSTR;

/// Registry value name for the mHD autostart entry.
const VALUE_NAME: &str = "mHD";
const TASK_NAME: &str = "mHD";

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

fn autostart_command() -> Result<String, String> {
    Ok(format!("\"{}\"", exe_path()?))
}

fn decode_process_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let words: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&words);
    }

    String::from_utf8_lossy(bytes).to_string()
}

fn run_schtasks(args: &[&str]) -> Result<String, String> {
    let output = Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run schtasks: {e}"))?;

    if output.status.success() {
        return Ok(decode_process_output(&output.stdout));
    }

    let stderr = decode_process_output(&output.stderr).trim().to_string();
    let stdout = decode_process_output(&output.stdout).trim().to_string();
    let message = if !stderr.is_empty() { stderr } else { stdout };
    Err(format!("schtasks failed: {message}"))
}

fn task_xml() -> Result<Option<String>, String> {
    match run_schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]) {
        Ok(xml) => Ok(Some(xml)),
        Err(e) if e.contains("cannot find") || e.contains("unable to find") => Ok(None),
        Err(e) => Err(e),
    }
}

fn install_scheduled_task() -> Result<(), String> {
    let command = autostart_command()?;
    run_schtasks(&[
        "/Create",
        "/TN",
        TASK_NAME,
        "/SC",
        "ONLOGON",
        "/TR",
        &command,
        "/RL",
        "HIGHEST",
        "/F",
    ])
    .map(|_| ())
}

fn remove_scheduled_task() -> Result<(), String> {
    if task_xml()?.is_none() {
        return Ok(());
    }
    run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]).map(|_| ())
}

fn scheduled_task_status() -> Option<bool> {
    let exe = match exe_path() {
        Ok(v) => v,
        Err(_) => return Some(false),
    };
    let xml = match task_xml() {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(_) => return Some(false),
    };
    Some(xml.contains("<LogonTrigger")
        && xml.contains("<RunLevel>HighestAvailable</RunLevel>")
        && xml.contains(&exe))
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

fn read_autostart_command() -> Result<Option<String>, String> {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        with_run_key(KEY_QUERY_VALUE, |key| {
            let mut value_type = REG_VALUE_TYPE::default();
            let mut byte_len: u32 = 0;
            let ret = RegQueryValueExW(
                key,
                PCWSTR::from_raw(wide_name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut byte_len),
            );
            if ret == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if ret != ERROR_SUCCESS {
                return Err(format!("RegQueryValueExW size query failed: {ret:?}"));
            }
            if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
                return Ok(None);
            }
            if byte_len == 0 {
                return Ok(Some(String::new()));
            }

            let mut bytes = vec![0u8; byte_len as usize];
            let ret = RegQueryValueExW(
                key,
                PCWSTR::from_raw(wide_name.as_ptr()),
                None,
                Some(&mut value_type),
                Some(bytes.as_mut_ptr()),
                Some(&mut byte_len),
            );
            if ret != ERROR_SUCCESS {
                return Err(format!("RegQueryValueExW data query failed: {ret:?}"));
            }

            let words_len = (byte_len as usize) / 2;
            let words = std::slice::from_raw_parts(bytes.as_ptr().cast::<u16>(), words_len);
            let end = words.iter().position(|&c| c == 0).unwrap_or(words.len());
            Ok(Some(String::from_utf16_lossy(&words[..end])))
        })
    }
}

/// Install autostart by writing to `HKCU\…\Run`.
fn install_run_key_autostart() -> Result<(), String> {
    let exe_str = autostart_command()?;
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
fn remove_run_key_autostart() -> Result<(), String> {
    let wide_name = to_wide(VALUE_NAME);

    unsafe {
        with_run_key(KEY_SET_VALUE, |key| {
            let ret = RegDeleteValueW(key, PCWSTR::from_raw(wide_name.as_ptr()));
            if ret == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            if ret != ERROR_SUCCESS {
                return Err(format!("RegDeleteValueW failed: {ret:?}"));
            }
            Ok(())
        })
    }
}

fn is_run_key_autostart_enabled() -> bool {
    let expected = match autostart_command() {
        Ok(v) => v,
        Err(_) => return false,
    };
    matches!(read_autostart_command(), Ok(Some(actual)) if actual == expected)
}

/// Install autostart. Prefer a logon scheduled task with highest available
/// privileges; fall back to `HKCU\...\Run` if Task Scheduler refuses creation.
pub fn install_autostart() -> Result<(), String> {
    match install_scheduled_task() {
        Ok(()) => {
            let _ = remove_run_key_autostart();
            Ok(())
        }
        Err(task_err) => install_run_key_autostart().map_err(|reg_err| {
            format!("scheduled task install failed: {task_err}; registry fallback failed: {reg_err}")
        }),
    }
}

/// Remove all known autostart mechanisms.
pub fn remove_autostart() -> Result<(), String> {
    let task_result = remove_scheduled_task();
    let run_key_result = remove_run_key_autostart();

    match (task_result, run_key_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Err(a), Err(b)) => Err(format!("{a}; {b}")),
    }
}

/// Check whether autostart is currently enabled for the current executable.
pub fn is_autostart_enabled() -> bool {
    match scheduled_task_status() {
        Some(enabled) => enabled,
        None => is_run_key_autostart_enabled(),
    }
}
