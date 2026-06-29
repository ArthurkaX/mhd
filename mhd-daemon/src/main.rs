//! mhd — hotkey daemon with DDC/CI brightness control.
//!
//! Single binary: tray + daemon core by default, headless via --daemon.

#![windows_subsystem = "windows"]

#[cfg(feature = "blackbox")]
pub(crate) mod blackbox;
mod constants;
mod core;
#[cfg(not(feature = "blackbox"))]
#[allow(dead_code)]
pub(crate) mod blackbox {
    #[derive(Debug, Clone)]
    pub struct BlackboxConfig {
        pub enabled: bool,
        pub idle_seconds: u64,
        pub track_locks: bool,
        pub track_suspend: bool,
        pub window_title_filter: Vec<String>,
    }

    #[derive(Debug, Clone)]
    pub enum BlackboxEvent {
        Input { kind: InputKind, ts: u64 },
        SystemLocked { ts: u64 },
        SystemUnlocked { ts: u64 },
        SystemSuspend { ts: u64 },
        SystemResume { ts: u64 },
        Shutdown,
        ToggleEnabled,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum InputKind {
        Keyboard,
        MouseButton,
        Wheel,
        Move,
    }

    #[derive(Debug)]
    pub struct BlackboxHandle;

    pub fn send_event(_: BlackboxEvent) {}

    pub fn start(_: BlackboxConfig) -> Result<BlackboxHandle, String> {
        Ok(BlackboxHandle)
    }

    impl BlackboxHandle {
        pub fn shutdown(&mut self) {}

        pub fn toggle(&self) {}
    }
}
mod app;
mod config;
#[cfg(feature = "debug-dump")]
mod crash_dump;
mod ddc;
mod osd;
mod overlays;
pub mod renderer;
pub mod win32;
// Re-export core modules so existing `crate::hook::*` etc. still resolve.
pub use core::{action, hook, llm_proxy, native_theme, platform, trigger, worker};
// Re-export overlay modules so existing `crate::tray::*` etc. still resolve.
pub use overlays::{
    about, autostart, cpu_plan, draw, keycast, monitor, note, power, suspend, throttle, topmost,
    tray, vision_prompt, volume,
};
use std::env;
use std::process::ExitCode;
#[cfg(not(feature = "debug-dump"))]
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::PCWSTR;

use crate::app::DaemonControl;
use crate::config::path::{create_bundled_themes, create_example_config, resolve_config_path};

/// Show a blocking error dialog. Used for fatal startup failures (e.g. an
/// unparseable config) that the user must see even when mhd is launched with
/// `windows_subsystem = "windows"` and no console is attached.
fn show_error_dialog(title: &str, message: &str) {
    let msg = crate::renderer::to_utf16_z(message);
    let title = crate::renderer::to_utf16_z(title);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR::from_raw(msg.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Install a panic hook that writes the panic message and backtrace to
/// `<config_dir>/crash.log` for post‑mortem analysis.
///
/// The file is overwritten on each panic so you always have the *last*
/// crash log.
#[cfg(not(feature = "debug-dump"))]
fn setup_panic_hook() {
    // Resolve config directory once (before any potential panic)
    let config_path = resolve_config_path();
    let log_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    std::panic::set_hook(Box::new(move |info| {
        // Timestamp
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let panic_msg = if let Some(msg) = info.payload().downcast_ref::<&str>() {
            msg.to_string()
        } else if let Some(msg) = info.payload().downcast_ref::<String>() {
            msg.clone()
        } else {
            "(non‑string panic payload)".to_string()
        };

        let location = info
            .location()
            .map(|loc| format!("{}:{}", loc.file(), loc.line()))
            .unwrap_or_else(|| "(unknown location)".to_string());

        // Capture backtrace if available (Rust 1.71+ captures by default)
        let backtrace = std::backtrace::Backtrace::capture();
        let bt = format!("{backtrace}");

        let log_content = format!(
            "mhd crash log (timestamp: {ts})\n\
             ─────────────────────────────────────\n\
             Location: {location}\n\
             Message:  {panic_msg}\n\
             \n\
             Backtrace:\n\
             {bt}\n\
             ─────────────────────────────────────\n\
             END\n"
        );

        // Write to crash.log in the config directory.
        // Silently ignore write errors – we're already panicking.
        let log_path = log_dir.join("crash.log");
        let _ = std::fs::write(&log_path, &log_content);

        // Also try to write a more unique filename with timestamp
        let dated_path = log_dir.join(format!("crash_{ts}.log"));
        let _ = std::fs::write(&dated_path, &log_content);

        // Print to stderr in case there's a console attached
        eprintln!(
            "mhd PANIC — crash details written to {}",
            log_path.display()
        );
    }));
}

fn main() -> ExitCode {
    #[cfg(feature = "debug-dump")]
    {
        crash_dump::clear_old_logs();
        crash_dump::install();
    }

    #[cfg(not(feature = "debug-dump"))]
    {
        // Install panic hook — saves panic details to the config directory
        // so crashes can be diagnosed without a terminal.
        setup_panic_hook();
    }

    // Try to attach to parent console so we can print messages if launched from a terminal.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    let args: Vec<String> = env::args().collect();
    let mut quiet = false;
    let mut no_tray = false;
    let mut debug_quicknote = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--quiet" => quiet = true,
            "--daemon" | "--no-tray" => no_tray = true,
            "--debug-quicknote" => debug_quicknote = true,
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

    if debug_quicknote {
        note::set_debug_logging(true);
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
                let _ = AllocConsole();
            }
        }
        println!("mhd: quicknote debug logging enabled");
    }

    let config_path = resolve_config_path();

    // Check if config exists
    if !config_path.exists() {
        match create_example_config(&config_path) {
            Ok(()) => {
                println!("mhd: created example config: {}", config_path.display());
                // Create bundled themes on first launch
                if let Err(e) = create_bundled_themes() {
                    eprintln!("mhd: warning — could not create bundled themes: {e}");
                }
                println!("mhd: uncomment bindings to enable them");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("mhd: {e}");
                show_error_dialog(
                    "mhd — Config Error",
                    &format!(
                        "Failed to load configuration:\n\n{e}\n\nFile: {}",
                        config_path.display()
                    ),
                );
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
            show_error_dialog(
                "mhd — Init Error",
                &format!("Failed to initialize application:\n\n{e}"),
            );
            return ExitCode::FAILURE;
        }
    };

    let handle = app.handle();

    // Push initial theme to OSD
    osd_handle.set_theme(handle.theme());

    // Sync autostart state with config on startup. This also repairs stale
    // entries that point at an old executable path or use an obsolete trigger.
    let config_autostart = handle.config.lock().unwrap().autostart();
    let reg_autostart = crate::autostart::is_autostart_enabled();
    if config_autostart && !reg_autostart {
        if let Err(e) = crate::autostart::install_autostart() {
            eprintln!("mhd: failed to sync autostart (install): {e}");
        }
    } else if !config_autostart
        && reg_autostart
        && let Err(e) = crate::autostart::remove_autostart()
    {
        eprintln!("mhd: failed to sync autostart (remove): {e}");
    }

    // Start the llm-proxy child if enabled in config.
    {
        let llm_cfg = handle.llm_proxy_config();
        if llm_cfg.enabled {
            if crate::llm_proxy::start(&llm_cfg) {
                if !quiet {
                    println!("mhd: llm-proxy started on port {}", llm_cfg.port);
                }
            } else if !quiet {
                eprintln!("mhd: llm-proxy failed to start");
            }
        }
    }

    // Watch settings.json for edits and reload live knobs on change.
    // Uses a background thread so the watcher never blocks the hook loop.
    {
        use std::sync::atomic::{AtomicI64, Ordering as AO};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Use leading `::` to reach the external crate; `llm_proxy` in this
        // file is shadowed by the re-export of crate::core::llm_proxy.
        let settings_path = ::llm_proxy::config::config_dir().join("settings.json");
        let watch_handle = handle.clone();
        let watch_quiet = quiet;

        std::thread::spawn(move || {
            use notify::{Config as WatchConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

            let last_handled = AtomicI64::new(0);
            let (tx, rx) = std::sync::mpsc::channel();
            let tx2 = tx.clone();

            let mut watcher = match RecommendedWatcher::new(
                move |res| { let _ = tx2.send(res); },
                WatchConfig::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("mhd: settings watcher failed to create: {e}");
                    return;
                }
            };

            // Watch the directory so atomic-rename saves (editor, PowerShell
            // Set-Content) still generate an event for settings.json.
            let watch_dir = settings_path
                .parent()
                .map(|p: &std::path::Path| p.to_path_buf())
                .unwrap_or_else(|| settings_path.clone());

            if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
                eprintln!("mhd: settings watcher failed to start: {e}");
                return;
            }

            for event in rx {
                let ev = match event {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("mhd: settings watcher error: {e}");
                        continue;
                    }
                };

                // Only react to modify/create events whose path is settings.json.
                let is_settings = ev.paths.iter().any(|p| {
                    p.file_name().and_then(|n| n.to_str()) == Some("settings.json")
                });
                let is_write = matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_));
                if !is_settings || !is_write {
                    continue;
                }

                // Debounce: skip events within 500 ms of the last handled one.
                // Editors (VS Code, Zed, etc.) emit multiple events per save.
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let prev = last_handled.load(AO::Relaxed);
                if now - prev < 500 {
                    continue;
                }
                last_handled.store(now, AO::Relaxed);

                match watch_handle.reload_config() {
                    Ok(()) => {
                        if !watch_quiet {
                            println!("mhd: settings.json changed \u{2014} reloaded");
                        }
                    }
                    Err(e) => {
                        eprintln!("mhd: settings.json reload error: {e}");
                    }
                }
            }
        });
    }

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

    // Stop the llm-proxy child if we started it.
    crate::llm_proxy::stop();

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
    eprintln!("  mhd.exe --debug-quicknote  Print QuickNote lifecycle/debug events to console");
    eprintln!();
    eprintln!("Debug crash dumps:");
    eprintln!("  cargo build --release --features debug-dump");
    eprintln!("  writes %TEMP%\\mhd_debug_dump.log and %TEMP%\\mhd_*.dmp");
    eprintln!("  mhd.exe --help        Show this help");
}
