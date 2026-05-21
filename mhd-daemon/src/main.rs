//! mhd — hotkey daemon with DDC/CI brightness control.
//!
//! Single binary: tray + daemon core by default, headless via --daemon.

#![windows_subsystem = "windows"]

mod action;
mod app;
mod platform;
mod monitor;
mod monitor_panel;
mod config;
mod hook;
mod tray;
mod trigger;
mod volume_mixer;
mod worker;
mod osd;
mod about;
mod native_theme;
mod config_editor;

use std::env;
use std::process::ExitCode;

use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

use crate::config::path::{resolve_config_path, create_example_config};

fn main() -> ExitCode {
    // Try to attach to parent console so we can print messages if launched from a terminal.
    unsafe { let _ = AttachConsole(ATTACH_PARENT_PROCESS); }

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

    // Start native OSD subsystem
    let osd_handle = osd::start_osd();

    // Create the app core
    let app = match app::App::new(config_path.clone(), quiet, osd_handle.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mhd: {e}");
            return ExitCode::FAILURE;
        }
    };

    let handle = app.handle();

    // Push initial theme to OSD
    osd_handle.set_theme(handle.theme());
    // Push initial theme to volume mixer
    volume_mixer::set_theme(handle.theme());
    // Push initial theme to monitor panel
    monitor_panel::set_theme(handle.theme());

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

    // Shutdown OSD
    osd_handle.shutdown();

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
