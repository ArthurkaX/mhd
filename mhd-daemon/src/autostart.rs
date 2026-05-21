//! Windows autostart via Scheduled Task (run at logon with highest privileges).
//!
//! Uses `schtasks.exe` to create/delete a task named `mHD`. The task runs
//! at user logon with the highest available privileges (elevated), so mhd
//! can control DDC/CI brightness without needing to be launched as admin
//! manually.
//!
//! If creating the task with highest privileges fails (e.g. the user is
//! not an administrator), it falls back to limited privileges so autostart
//! still works — just without elevation.

use std::process::Command;

/// Task name used in the Windows Task Scheduler.
const TASK_NAME: &str = "mHD";

/// Install the autostart scheduled task.
///
/// Returns `Ok(true)` if created with highest privileges,
/// `Ok(false)` if created with limited privileges,
/// `Err(msg)` on failure.
pub fn install_autostart() -> Result<bool, String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("cannot determine exe path: {e}"))?;
    let exe_str = exe_path.to_string_lossy();

    // Step 1: try with highest privileges (admin)
    let highest_result = run_schtasks(&[
        "/create",
        "/tn", TASK_NAME,
        "/tr", &format!("\"{exe_str}\""),
        "/sc", "onlogon",
        "/rl", "highest",
        "/f",
    ]);

    match highest_result {
        Ok(_) => return Ok(true),
        Err(e) => {
            // If access denied or elevation failed, try limited
            let limited_result = run_schtasks(&[
                "/create",
                "/tn", TASK_NAME,
                "/tr", &format!("\"{exe_str}\""),
                "/sc", "onlogon",
                "/rl", "limited",
                "/f",
            ]);
            match limited_result {
                Ok(_) => {
                    eprintln!("mhd: autostart installed (limited privileges — {e})");
                    Ok(false)
                }
                Err(e2) => Err(format!(
                    "failed to create scheduled task (highest: {e}, limited: {e2})"
                )),
            }
        }
    }
}

/// Remove the autostart scheduled task.
pub fn remove_autostart() -> Result<(), String> {
    run_schtasks(&["/delete", "/tn", TASK_NAME, "/f"])
        .map(|_| ())
        .map_err(|e| format!("failed to remove autostart: {e}"))
}

/// Check whether the autostart task exists.
#[allow(dead_code)]
pub fn is_autostart_enabled() -> bool {
    // Query with a simple check — exit code 0 means the task exists.
    let output = Command::new("schtasks")
        .args(["/query", "/tn", TASK_NAME])
        .output();

    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// Run `schtasks.exe` with the given arguments.
fn run_schtasks(args: &[&str]) -> Result<String, String> {
    let output = Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|e| format!("failed to launch schtasks.exe: {e}"))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let msg = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).to_string()
        } else {
            stderr
        };
        Err(msg.trim().to_string())
    }
}
