mod action;
mod config;
mod hook;
mod trigger;
mod worker;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use config::AppConfig;
use worker::ActionWorker;

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
# Requires the VirtualDesktop PowerShell module:
#   Install-Module VirtualDesktop -Scope CurrentUser
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

# Switch to desktop 1.
# [[binding]]
# trigger = "alt+1"
# action = "run_ps"
# command = "Switch-Desktop -Desktop 0"

# Move active window to desktop 1.
# [[binding]]
# trigger = "alt+shift+1"
# action = "run_ps"
# command = "Move-Window -Desktop 0"
"#;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mut quiet = false;
    let mut edit = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--quiet" => quiet = true,
            "--edit" => edit = true,
            other => {
                eprintln!("mhd: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let config_path = resolve_config_path();

    if edit {
        return run_edit(&config_path);
    }

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

    // Read and parse config
    let config_content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mhd: cannot read config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let app_config = match AppConfig::parse(&config_content, &config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mhd: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Check for empty active bindings
    if app_config.active_bindings().is_empty() {
        eprintln!("mhd: config empty: {}", config_path.display());
        return ExitCode::FAILURE;
    }

    if !quiet {
        println!("mhd: loaded config: {}", config_path.display());
        println!(
            "mhd: loaded bindings: {}",
            app_config.active_bindings().len()
        );
    }

    // Create action worker and get sender
    let (worker, tx) = ActionWorker::new(quiet);

    // Spawn worker thread for action execution
    let _worker_handle = worker.spawn();

    // Install hooks and run message loop
    if let Err(e) = hook::run(app_config, tx, quiet) {
        eprintln!("mhd: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run_edit(config_path: &PathBuf) -> ExitCode {
    if !config_path.exists() {
        if let Err(e) = create_example_config(config_path) {
            eprintln!("mhd: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Open with default editor via ShellExecuteW
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;
    use windows::core::PCWSTR;

    let wide_path = config_path.to_str().unwrap_or("");
    let mut wide: Vec<u16> = wide_path.encode_utf16().collect();
    wide.push(0);

    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR::null(),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWDEFAULT,
        );
    }

    ExitCode::SUCCESS
}
