use std::env;
use std::path::PathBuf;

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

/// Bundled themes shipped with the binary.
struct BundledTheme {
    pub filename: &'static str,
    pub content: &'static str,
}

const BUNDLED_THEMES: &[BundledTheme] = &[
    BundledTheme {
        filename: "carbon.json",
        content: include_str!("../../../themes/carbon.json"),
    },
    BundledTheme {
        filename: "paper.json",
        content: include_str!("../../../themes/paper.json"),
    },
    BundledTheme {
        filename: "night_glass.json",
        content: include_str!("../../../themes/night_glass.json"),
    },
    BundledTheme {
        filename: "day_glass.json",
        content: include_str!("../../../themes/day_glass.json"),
    },
    BundledTheme {
        filename: "ember.json",
        content: include_str!("../../../themes/ember.json"),
    },
];

/// Create the themes directory and write bundled theme files.
/// Silently skips files that already exist.
pub fn create_bundled_themes() -> Result<(), String> {
    let dir = crate::native_theme::themes_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create themes directory: {e}"))?;

    for theme in BUNDLED_THEMES {
        let path = dir.join(theme.filename);
        if path.exists() {
            continue;
        }
        std::fs::write(&path, theme.content.trim_start())
            .map_err(|e| format!("cannot write theme '{}': {e}", theme.filename))?;
    }
    Ok(())
}

#[cfg(not(feature = "blackbox"))]
const EXAMPLE_CONFIG: &str = r#"# mhd config
# Path: %USERPROFILE%\.config\mhd\config.toml
#
# Uncomment bindings to enable them.
# The main idea: if Windows or another app uses an uncomfortable shortcut
# and does not make it easy to change, map a convenient key/button to it here.
#
# Optional startup scheme. If omitted, "default" is used.
# active_scheme = "default"
#
# Step size for media_volume_up / media_volume_down (default: 1).
# Each step sends one VK_VOLUME_UP/DOWN key press.
# volume_step = 1
#
# Autostart mhd at user logon (via scheduled task with highest privileges).
# autostart = true

# Quit mhd (Ctrl+Alt+F12).
[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"

# CapsLock -> Alt+Shift.
# Useful when Alt+Shift is your Windows keyboard layout switch:
# one key changes language/layout instead of a two-key chord.
# [[binding]]
# trigger = "capslock"
# action = "replace_key"
# keys = "alt+shift"

# Mouse side buttons -> virtual desktop navigation.
# Windows uses Ctrl+Win+Left/Right for this, which is not comfortable
# during normal mouse-driven work. mhd can put that action under your thumb.
# [[binding]]
# trigger = "mouseButton4"
# action = "replace_key"
# keys = "ctrl+win+left"

# Mouse button 5 does the symmetric action: switch to the right desktop.
# [[binding]]
# trigger = "mouseButton5"
# action = "replace_key"
# keys = "ctrl+win+right"

# Increase monitor brightness via DDC/CI.
# Useful for external monitors where Windows brightness controls do not work.
# [[binding]]
# trigger = "ctrl+alt+numpad_add"
# action = "brightness_up"
# value = "5"

# Decrease monitor brightness via DDC/CI.
# [[binding]]
# trigger = "ctrl+alt+numpad_subtract"
# action = "brightness_down"
# value = "5"

# Open Windows Terminal from a shortcut.
# [[binding]]
# trigger = "ctrl+alt+t"
# action = "run_ps"
# command = "Start-Process wt"

# Toggle aggressive Windows power throttling + one-CPU affinity for the current app when it loses focus.
# Useful for heavy games/tools that should keep running with reduced CPU pressure in background.
# [[binding]]
# trigger = "ctrl+alt+f10"
# action = "toggle_throttle_on_blur"

# Toggle full process suspension for the current app when it loses focus.
# Useful for heavy games/tools that should stop consuming resources in background.
# [[binding]]
# trigger = "ctrl+alt+f9"
# action = "toggle_suspend_on_blur"

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

#[cfg(feature = "blackbox")]
const EXAMPLE_CONFIG: &str = r#"# mhd config
# Path: %USERPROFILE%\.config\mhd\config.toml
#
# Uncomment bindings to enable them.
# The main idea: if Windows or another app uses an uncomfortable shortcut
# and does not make it easy to change, map a convenient key/button to it here.
#
# Optional startup scheme. If omitted, "default" is used.
# active_scheme = "default"
#
# Step size for media_volume_up / media_volume_down (default: 1).
# Each step sends one VK_VOLUME_UP/DOWN key press.
# volume_step = 1
#
# Autostart mhd at user logon (via scheduled task with highest privileges).
# autostart = true

# Behavioural logger (disabled by default).
# [blackbox]
# enabled = true
# idle_seconds = 300

# Quit mhd (Ctrl+Alt+F12).
[[binding]]
trigger = "ctrl+alt+f12"
action = "quit"

# CapsLock -> Alt+Shift.
# Useful when Alt+Shift is your Windows keyboard layout switch:
# one key changes language/layout instead of a two-key chord.
# [[binding]]
# trigger = "capslock"
# action = "replace_key"
# keys = "alt+shift"

# Mouse side buttons -> virtual desktop navigation.
# Windows uses Ctrl+Win+Left/Right for this, which is not comfortable
# during normal mouse-driven work. mhd can put that action under your thumb.
# [[binding]]
# trigger = "mouseButton4"
# action = "replace_key"
# keys = "ctrl+win+left"

# Mouse button 5 does the symmetric action: switch to the right desktop.
# [[binding]]
# trigger = "mouseButton5"
# action = "replace_key"
# keys = "ctrl+win+right"

# Increase monitor brightness via DDC/CI.
# Useful for external monitors where Windows brightness controls do not work.
# [[binding]]
# trigger = "ctrl+alt+numpad_add"
# action = "brightness_up"
# value = "5"

# Decrease monitor brightness via DDC/CI.
# [[binding]]
# trigger = "ctrl+alt+numpad_subtract"
# action = "brightness_down"
# value = "5"

# Open Windows Terminal from a shortcut.
# [[binding]]
# trigger = "ctrl+alt+t"
# action = "run_ps"
# command = "Start-Process wt"

# Toggle aggressive Windows power throttling + one-CPU affinity for the current app when it loses focus.
# Useful for heavy games/tools that should keep running with reduced CPU pressure in background.
# [[binding]]
# trigger = "ctrl+alt+f10"
# action = "toggle_throttle_on_blur"

# Toggle full process suspension for the current app when it loses focus.
# Useful for heavy games/tools that should stop consuming resources in background.
# [[binding]]
# trigger = "ctrl+alt+f9"
# action = "toggle_suspend_on_blur"

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
