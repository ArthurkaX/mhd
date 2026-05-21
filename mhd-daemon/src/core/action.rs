use crate::trigger::{KeyCombo, parse_keys};

/// An action to execute when a trigger fires.
#[derive(Debug, Clone)]
pub enum Action {
    ReplaceKey { keys: KeyCombo },
    RunPs { command: String },
    /// Launch an executable file.
    RunProgram { path: String },
    SwitchScheme { target_scheme: String },
    SetBrightness { relative: bool, value: i32 },
    /// Increase monitor brightness by a configurable step.
    BrightnessUp { value: u32 },
    /// Decrease monitor brightness by a configurable step.
    BrightnessDown { value: u32 },
    Vcp { code: u8, relative: bool, value: i32 },
    ShowVolumeMixer,
    ShowMonitorPanel,
    /// Increase system volume by one step (VK_VOLUME_UP).
    MediaVolumeUp,
    /// Decrease system volume by one step (VK_VOLUME_DOWN).
    MediaVolumeDown,
    /// Toggle system mute (VK_VOLUME_MUTE).
    MediaMute,
    /// Play or pause media (VK_MEDIA_PLAY_PAUSE).
    MediaPlayPause,
    /// Stop media playback (VK_MEDIA_STOP).
    MediaStop,
    /// Go to previous track (VK_MEDIA_PREV_TRACK).
    MediaLastTrack,
    /// Go to next track (VK_MEDIA_NEXT_TRACK).
    MediaNextTrack,
    /// Toggle always‑on‑top for the currently focused window.
    ToggleTopmost,
    /// Show the Power Control overlay (awake/sleep/shutdown).
    PowerActions,
    Quit,
}

/// Raw fields for an action, used during parsing.
pub struct ActionRawFields<'a> {
    pub keys: Option<&'a str>,
    pub command: Option<&'a str>,
    pub path: Option<&'a str>,
    pub target_scheme: Option<&'a str>,
    pub value: Option<&'a str>,
    pub code: Option<&'a str>,
}

impl Action {
    /// Create an action from raw fields.
    pub fn from_raw(action: &str, fields: ActionRawFields) -> Result<Self, String> {
        match action {
            "replace_key" => {
                let keys = fields
                    .keys
                    .ok_or_else(|| "replace_key action requires 'keys' field".to_string())?;
                Self::new_replace_key(keys)
            }
            "run_program" => {
                let path = fields
                    .path
                    .ok_or_else(|| "run_program action requires 'path' field".to_string())?;
                Self::new_run_program(path)
            }
            "run_ps" => {
                let command = fields
                    .command
                    .ok_or_else(|| "run_ps action requires 'command' field".to_string())?;
                Self::new_run_ps(command)
            }
            "switch_scheme" => {
                let target = fields.target_scheme.ok_or_else(|| {
                    "switch_scheme action requires 'target_scheme' field".to_string()
                })?;
                Self::new_switch_scheme(target)
            }
            "set_brightness" => {
                let value = fields
                    .value
                    .ok_or_else(|| "set_brightness action requires 'value' field".to_string())?;
                Self::new_set_brightness(value)
            }
            "vcp" => {
                let code = fields
                    .code
                    .ok_or_else(|| "vcp action requires 'code' field".to_string())?;
                let value = fields
                    .value
                    .ok_or_else(|| "vcp action requires 'value' field".to_string())?;
                Self::new_vcp(code, value)
            }
            "brightness_up" => {
                let value = parse_brightness_step(&fields, "brightness_up")?;
                Ok(Action::BrightnessUp { value })
            }
            "brightness_down" => {
                let value = parse_brightness_step(&fields, "brightness_down")?;
                Ok(Action::BrightnessDown { value })
            }
            "show_monitor_panel" => Ok(Action::ShowMonitorPanel),
            "show_volume_mixer" => Ok(Action::ShowVolumeMixer),
            "media_volume_up" => Ok(Action::MediaVolumeUp),
            "media_volume_down" => Ok(Action::MediaVolumeDown),
            "media_mute" => Ok(Action::MediaMute),
            "media_play_pause" => Ok(Action::MediaPlayPause),
            "media_stop" => Ok(Action::MediaStop),
            "media_last_track" => Ok(Action::MediaLastTrack),
            "toggle_topmost" => Ok(Action::ToggleTopmost),
            "media_next_track" => Ok(Action::MediaNextTrack),
            "power_actions" => Ok(Action::PowerActions),
            "quit" => Ok(Action::Quit),
            other => Err(format!("unknown action: {other}")),
        }
    }

    /// Validate and create a replace_key action.
    pub fn new_replace_key(keys: &str) -> Result<Self, String> {
        let key_combo = parse_keys(keys)?;
        Ok(Action::ReplaceKey { keys: key_combo })
    }

    /// Validate and create a run_ps action.
    pub fn new_run_program(path: &str) -> Result<Self, String> {
        if path.trim().is_empty() {
            return Err("run_program path must not be empty".to_string());
        }
        Ok(Action::RunProgram {
            path: path.to_string(),
        })
    }

    pub fn new_run_ps(command: &str) -> Result<Self, String> {
        if command.trim().is_empty() {
            return Err("run_ps command must not be empty".to_string());
        }
        Ok(Action::RunPs {
            command: command.to_string(),
        })
    }

    /// Validate and create a switch_scheme action.
    pub fn new_switch_scheme(target_scheme: &str) -> Result<Self, String> {
        if target_scheme.trim().is_empty() {
            return Err("switch_scheme target_scheme must not be empty".to_string());
        }
        Ok(Action::SwitchScheme {
            target_scheme: target_scheme.to_string(),
        })
    }

    /// Validate and create a set_brightness action.
    /// Format: "+5", "-10" (relative), or "50" (absolute 0-100).
    pub fn new_set_brightness(raw: &str) -> Result<Self, String> {
        let s = raw.trim();
        if s.is_empty() {
            return Err("set_brightness value must not be empty".to_string());
        }

        if let Some(rest) = s.strip_prefix("+") {
            let v: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("invalid brightness delta: ''{s}''"))?;
            if v == 0 {
                return Err("brightness delta must not be zero".to_string());
            }
            Ok(Action::SetBrightness {
                relative: true,
                value: v,
            })
        } else if let Some(rest) = s.strip_prefix("-") {
            let v: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("invalid brightness delta: ''{s}''"))?;
            if v == 0 {
                return Err("brightness delta must not be zero".to_string());
            }
            Ok(Action::SetBrightness {
                relative: true,
                value: -v,
            })
        } else {
            let v: u32 = s
                .parse()
                .map_err(|_| format!("invalid brightness value: ''{s}''"))?;
            if v > 100 {
                return Err("brightness must be between 0 and 100".to_string());
            }
            Ok(Action::SetBrightness {
                relative: false,
                value: v as i32,
            })
        }
    }

    /// Validate and create a vcp action.
    /// Format: code="0x10", value="+5" or "50".
    pub fn new_vcp(code_raw: &str, value_raw: &str) -> Result<Self, String> {
        let code = if let Some(stripped) = code_raw.strip_prefix("0x") {
            u8::from_str_radix(stripped, 16)
        } else {
            code_raw.parse()
        }.map_err(|_| format!("invalid VCP code: {code_raw}"))?;

        let s = value_raw.trim();
        if s.is_empty() {
            return Err("vcp value must not be empty".to_string());
        }

        if let Some(rest) = s.strip_prefix("+") {
            let v: i32 = rest.trim().parse().map_err(|_| format!("invalid vcp delta: {s}"))?;
            Ok(Action::Vcp { code, relative: true, value: v })
        } else if let Some(rest) = s.strip_prefix("-") {
            let v: i32 = rest.trim().parse().map_err(|_| format!("invalid vcp delta: {s}"))?;
            Ok(Action::Vcp { code, relative: true, value: -v })
        } else {
            let v: i32 = s.parse().map_err(|_| format!("invalid vcp value: {s}"))?;
            Ok(Action::Vcp { code, relative: false, value: v })
        }
    }

    /// Get a human-readable description of the action.
    pub fn describe(&self) -> String {
        match self {
            Action::ReplaceKey { keys } => {
                format!("replace_key: {}", crate::trigger::keys_to_string(keys))
            }
            Action::RunPs { command } => format!("run_ps: {command}"),
            Action::RunProgram { path } => format!("run_program: {path}"),
            Action::SwitchScheme { target_scheme } => format!("switch_scheme: {target_scheme}"),
            Action::SetBrightness { relative, value } => {
                if *relative {
                    format!("set_brightness: {:+}", value)
                } else {
                    format!("set_brightness: {}%", value)
                }
            }
            Action::Vcp { code, relative, value } => {
                if *relative {
                    format!("vcp: 0x{:02X} {:+}", code, value)
                } else {
                    format!("vcp: 0x{:02X} = {}", code, value)
                }
            }
            Action::BrightnessUp { value } => format!("brightness_up: +{}", value),
            Action::BrightnessDown { value } => format!("brightness_down: -{}", value),
            Action::ShowMonitorPanel => "show_monitor_panel".to_string(),
            Action::ShowVolumeMixer => "show_volume_mixer".to_string(),
            Action::MediaVolumeUp => "media_volume_up".to_string(),
            Action::MediaVolumeDown => "media_volume_down".to_string(),
            Action::MediaMute => "media_mute".to_string(),
            Action::MediaPlayPause => "media_play_pause".to_string(),
            Action::MediaStop => "media_stop".to_string(),
            Action::MediaLastTrack => "media_last_track".to_string(),
            Action::ToggleTopmost => "toggle_topmost".to_string(),
            Action::PowerActions => "power_actions".to_string(),
            Action::MediaNextTrack => "media_next_track".to_string(),
            Action::Quit => "quit".to_string(),
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────

/// Parse the `value` field for brightness_up / brightness_down.
fn parse_brightness_step(fields: &ActionRawFields, name: &str) -> Result<u32, String> {
    let value = fields.value
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(5);
    if value == 0 {
        return Err(format!("{name} value must be > 0"));
    }
    Ok(value)
}

// ── Action registry (single source of truth for editor integration) ──

/// Metadata for an action variant.
#[derive(Debug, Clone, Copy)]
pub struct ActionDescriptor {
    /// TOML action name (e.g. "replace_key").
    pub name: &'static str,
    /// Human-readable label (e.g. "Replace Key").
    pub label: &'static str,
    /// Category grouping name (e.g. "Media", "Display", "System").
    pub category: &'static str,
    /// TOML parameter key (e.g. "keys"), or `None` for parameterless actions.
    pub param_key: Option<&'static str>,
}

/// All action variants known to the system.
///
/// This is the **single source of truth** used by the config editor
/// for listing available actions and serialising them to TOML.
/// When adding a new action variant, add its descriptor here.
pub const ALL_ACTIONS: &[ActionDescriptor] = &[
    ActionDescriptor {
        name: "replace_key",
        label: "Replace Key",
        category: "General",
        param_key: Some("keys"),
    },
    ActionDescriptor {
        name: "run_program",
        label: "Run Program",
        category: "General",
        param_key: Some("path"),
    },
    ActionDescriptor {
        name: "run_ps",
        label: "PowerShell",
        category: "General",
        param_key: Some("command"),
    },
    ActionDescriptor {
        name: "brightness_up",
        label: "Brightness Up",
        category: "Display",
        param_key: Some("value"),
    },
    ActionDescriptor {
        name: "brightness_down",
        label: "Brightness Down",
        category: "Display",
        param_key: Some("value"),
    },
    ActionDescriptor {
        name: "show_monitor_panel",
        label: "Monitor Control",
        category: "General",
        param_key: None,
    },
    ActionDescriptor {
        name: "show_volume_mixer",
        label: "Volume Mixer",
        category: "General",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_volume_up",
        label: "Volume Up",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_volume_down",
        label: "Volume Down",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_mute",
        label: "Mute",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_play_pause",
        label: "Play/Pause",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_stop",
        label: "Stop",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_last_track",
        label: "Last Track",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "media_next_track",
        label: "Next Track",
        category: "Media",
        param_key: None,
    },
    ActionDescriptor {
        name: "toggle_topmost",
        label: "Toggle Always On Top",
        category: "General",
        param_key: None,
    },
    ActionDescriptor {
        name: "power_actions",
        label: "Power Control",
        category: "General",
        param_key: None,
    },
    ActionDescriptor {
        name: "quit",
        label: "Quit mhd",
        category: "General",
        param_key: None,
    },
];

/// Find the index of an action descriptor by its TOML name.
/// Returns `None` if not found.
pub fn find_action_index(name: &str) -> Option<usize> {
    ALL_ACTIONS.iter().position(|d| d.name == name)
}