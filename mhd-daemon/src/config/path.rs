use std::path::PathBuf;
use std::env;

pub fn resolve_config_path() -> PathBuf {
    if let Ok(custom) = env::var("MHD_CONFIG") {
        return PathBuf::from(custom);
    }
    let mut path = home_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push(".config");
    path.push("mhd");
    path.push("config.toml");
    path
}

pub fn home_dir() -> Option<PathBuf> {
    env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .map(PathBuf::from)
        .ok()
}

pub fn create_example_config(path: &PathBuf) -> Result<(), String> {
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

# Set monitor input to HDMI 1 (0x60 is Input Select, 17 is HDMI 1 on some monitors).
# [[binding]]
# trigger = "ctrl+alt+f1"
# action = "vcp"
# code = "0x60"
# value = "17"

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

# ── Media keys ────────────────────────────────────────────────────

# Volume Up.
# [[binding]]
# trigger = "ctrl+alt+numpad_add"
# action = "media_volume_up"

# Volume Down.
# [[binding]]
# trigger = "ctrl+alt+numpad_subtract"
# action = "media_volume_down"

# Mute toggle.
# [[binding]]
# trigger = "ctrl+alt+m"
# action = "media_mute"

# Play / Pause.
# [[binding]]
# trigger = "ctrl+alt+numpad_multiply"
# action = "media_play_pause"

# Stop.
# [[binding]]
# trigger = "ctrl+alt+numpad_divide"
# action = "media_stop"

# Previous track.
# [[binding]]
# trigger = "ctrl+alt+numpad7"
# action = "media_last_track"

# Next track.
# [[binding]]
# trigger = "ctrl+alt+numpad9"
# action = "media_next_track"
"#;
