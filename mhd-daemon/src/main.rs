//! mhd — hotkey daemon with DDC/CI brightness control.
//!
//! Single binary: tray + daemon core by default, headless via --daemon.

#![windows_subsystem = "windows"]

mod action;
mod app;
mod brightness;
mod config;
mod hook;
mod ipc;
mod tray;
mod trigger;
mod worker;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn resolve_config_path() -> PathBuf {
    if let Ok(custom) = env::var("MHD_CONFIG") {
        return PathBuf::from(custom);
    }
    let mut path = home_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push(".config");
    path.push("mhd");
    path.push("config.toml");
    path
}

fn home_dir() -> Option<PathBuf> {
    env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .map(PathBuf::from)
        .ok()
}

fn create_example_config(path: &PathBuf) -> Result<(), String> {
    let parent = path.parent().ok_or("cannot determine config directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create config directory: {e}"))?;

    let example = EXAMPLE_CONFIG.trim_start();
    std::fs::write(path, example).map_err(|e| format!("cannot write example config: {e}"))?;
    Ok(())
}

const EXAMPLE_CONFIG: &str = r#"# mhd config
# Path: %USERPROFILE%\.config\mhd\config.toml
#
# Uncomment bindings to enable them.
#
# Optional startup scheme. If omitted, "default" is used.
# active_scheme = "default"

# Quit mhd (Ctrl+Alt+F12).
[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"

# Replace CapsLock with Alt+Shift for keyboard layout switching.
# [[binding]]
# trigger = "capslock"
# action = "replace_key"
# keys = "alt+shift"

# Switch to the left virtual desktop using mouse button 4 (side button).
# [[binding]]
# trigger = "mouseButton4"
# action = "replace_key"
# keys = "ctrl+win+left"

# Switch to the right virtual desktop using mouse button 5 (side button).
# [[binding]]
# trigger = "mouseButton5"
# action = "replace_key"
# keys = "ctrl+win+right"

# Increase monitor brightness via DDC/CI.
# [[binding]]
# trigger = "ctrl+alt+numpad_add"
# action = "set_brightness"
# value = "+5"

# Decrease monitor brightness via DDC/CI.
# [[binding]]
# trigger = "ctrl+alt+numpad_subtract"
# action = "set_brightness"
# value = "-5"

# Open Windows Terminal.
# [[binding]]
# trigger = "ctrl+alt+t"
# action = "run_ps"
# command = "Start-Process wt"
"#;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut quiet = false;
    let mut no_tray = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--quiet" => quiet = true,
            "--daemon" | "--no-tray" => no_tray = true,
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("mhd: unknown argument: {other}");
                print_help();
                return ExitCode::FAILURE;
            }
        }
    }

    let config_path = resolve_config_path();

    // Check if config exists
    if !config_path.exists() {
        match create_example_config(&config_path) {
            Ok(()) => {
                println!("mhd: created example config: {}", config_path.display());
                println!("mhd: uncomment bindings to enable them");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("mhd: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Create the app core
    let app = match app::App::new(config_path.clone(), quiet) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mhd: {e}");
            return ExitCode::FAILURE;
        }
    };

    let handle = app.handle();

    // Start IPC server on background thread (always — needed for
    // external control in headless mode, and useful for debugging
    // even with tray).
    let _ipc_handle = ipc::run_ipc_server(handle.clone());

    if no_tray {
        // Headless / daemon mode: block on the hook message loop.
        if !quiet {
            println!("mhd: daemon mode (no tray)");
        }
        if let Err(e) = app.run() {
            eprintln!("mhd: {e}");
            return ExitCode::FAILURE;
        }
        if !quiet {
            println!("mhd: stopped");
        }
    } else {
        // Full mode: spawn hook loop on background thread, run tray on
        // the main thread. The main thread owns the UI message pump.
        let app_for_hooks = app; // consume App

        // Spawn the hook thread
        let hook_handle = std::thread::spawn(move || {
            if let Err(e) = app_for_hooks.run() {
                eprintln!("mhd: hook error: {e}");
            }
        });

        // Run the tray on the main thread (blocks until quit)
        tray::run(handle);

        // Tray exited – make sure hooks stop too
        let _ = hook_handle.join();
        if !quiet {
            println!("mhd: stopped");
        }
    }

    ExitCode::SUCCESS
}

fn print_help() {
    eprintln!("mhd — hotkey daemon with DDC/CI brightness control");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  mhd.exe               Run with system tray (default)");
    eprintln!("  mhd.exe --daemon      Run headless (no tray)");
    eprintln!("  mhd.exe --quiet       Suppress startup messages");
    eprintln!("  mhd.exe --help        Show this help");
}
