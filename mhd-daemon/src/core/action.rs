use crate::trigger::{KeyCombo, parse_keys};

/// An action to execute when a trigger fires.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    ReplaceKey {
        keys: KeyCombo,
    },
    RunPs {
        command: String,
    },
    /// Launch an executable file.
    RunProgram {
        path: String,
    },
    SwitchScheme {
        target_scheme: String,
    },
    SetBrightness {
        relative: bool,
        value: i32,
    },
    /// Increase monitor brightness by a configurable step.
    BrightnessUp {
        value: u32,
    },
    /// Decrease monitor brightness by a configurable step.
    BrightnessDown {
        value: u32,
    },
    Vcp {
        code: u8,
        relative: bool,
        value: i32,
    },
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
    /// Toggle full process suspension for the current process when it loses focus.
    ToggleSuspendOnBlur,
    /// Toggle aggressive power throttling for the current process when it loses focus.
    ToggleThrottleOnBlur,
    /// Show the Power Control overlay (awake/sleep/shutdown).
    PowerActions,
    /// Toggle quiet mode: display off, CPU capped, machine keeps running.
    ToggleQuietMode,
    /// Put the computer to sleep immediately.
    Sleep,
    /// Hibernate the computer immediately.
    Hibernate,
    /// Show the Quick Draw overlay (drawing tools).
    QuickDraw,
    /// Show the Quick Note overlay (text note).
    QuickNote,
    /// Show the Pomodoro timer overlay.
    Pomodoro,
    /// Toggle KeyCast keystroke overlay for recording/streaming.
    ToggleKeycast,
    /// Show the Breathe pacer overlay. Optional preset: balanced, calm, extended, auto.
    Breathe {
        preset: Option<String>,
    },
    /// Switch active Windows power plan. target = plan name or "next".
    SwitchPowerPlan {
        target: String,
    },
    /// Show the CPU power plan settings panel.
    ShowCpuPanel,
    /// Show the LLM model selector overlay (per-tier routing for Claude Code).
    ShowLlmModels,
    /// Show the real-time proxy trace window.
    ShowProxyTrace,
    /// Show the LLM Monitor window (overview quota, activity, context trim).
    ShowLlmActivity,
    /// Toggle the llm-proxy process on/off.
    ToggleLlmProxy,
    /// Toggle the subscription-quota watcher on/off.
    ToggleQuotaWatcher,
    /// Capture the foreground monitor, send to a vision LLM, copy result to clipboard.
    VisionScreenshot,
    /// Capture, crop, annotate, and analyze a screenshot interactively.
    VisionSnip,
    Quit,
}

/// Raw fields for an action, used during parsing.
#[derive(Debug, Clone)]
pub struct ActionRawFields<'a> {
    pub keys: Option<&'a str>,
    pub command: Option<&'a str>,
    pub path: Option<&'a str>,
    pub target_scheme: Option<&'a str>,
    pub value: Option<&'a str>,
    pub code: Option<&'a str>,
    pub target: Option<&'a str>,
    pub preset: Option<&'a str>,
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
            "toggle_suspend_on_blur" => Ok(Action::ToggleSuspendOnBlur),
            "toggle_throttle_on_blur" => Ok(Action::ToggleThrottleOnBlur),
            "media_next_track" => Ok(Action::MediaNextTrack),
            "quick_draw" => Ok(Action::QuickDraw),
            "quick_note" => Ok(Action::QuickNote),
            "pomodoro" => Ok(Action::Pomodoro),
            "toggle_keycast" => Ok(Action::ToggleKeycast),
            "breathe" => {
                // `preset` field is optional. If absent or empty => auto-select by time of day.
                let preset = fields
                    .preset
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());
                Ok(Action::Breathe { preset })
            }
            "switch_power_plan" => {
                let target = fields
                    .target
                    .ok_or_else(|| "switch_power_plan requires 'target' field".to_string())?;
                Ok(Action::SwitchPowerPlan {
                    target: target.to_string(),
                })
            }
            "show_cpu_panel" => Ok(Action::ShowCpuPanel),
            "show_llm_models" => Ok(Action::ShowLlmModels),
            "show_proxy_trace" => Ok(Action::ShowProxyTrace),
            "show_llm_activity" => Ok(Action::ShowLlmActivity),
            "toggle_llm_proxy" => Ok(Action::ToggleLlmProxy),
            "toggle_quota_watcher" => Ok(Action::ToggleQuotaWatcher),
            // Why: accept legacy key binding name for backward compatibility.
            "toggle_codex_watcher" => Ok(Action::ToggleQuotaWatcher),
            "power_actions" => Ok(Action::PowerActions),
            "toggle_quiet_mode" => Ok(Action::ToggleQuietMode),
            "sleep" => Ok(Action::Sleep),
            "hibernate" => Ok(Action::Hibernate),
            "vision_screenshot" => Ok(Action::VisionScreenshot),
            "vision_snip" => Ok(Action::VisionSnip),
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
        }
        .map_err(|_| format!("invalid VCP code: {code_raw}"))?;

        let s = value_raw.trim();
        if s.is_empty() {
            return Err("vcp value must not be empty".to_string());
        }

        if let Some(rest) = s.strip_prefix("+") {
            let v: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("invalid vcp delta: {s}"))?;
            Ok(Action::Vcp {
                code,
                relative: true,
                value: v,
            })
        } else if let Some(rest) = s.strip_prefix("-") {
            let v: i32 = rest
                .trim()
                .parse()
                .map_err(|_| format!("invalid vcp delta: {s}"))?;
            Ok(Action::Vcp {
                code,
                relative: true,
                value: -v,
            })
        } else {
            let v: i32 = s.parse().map_err(|_| format!("invalid vcp value: {s}"))?;
            Ok(Action::Vcp {
                code,
                relative: false,
                value: v,
            })
        }
    }

    /// Stable TOML action name used for serialisation and editor matching.
    pub fn name(&self) -> &'static str {
        match self {
            Action::ReplaceKey { .. } => "replace_key",
            Action::RunPs { .. } => "run_ps",
            Action::RunProgram { .. } => "run_program",
            Action::SwitchScheme { .. } => "switch_scheme",
            Action::SetBrightness {
                relative: true,
                value,
            } if *value > 0 => "brightness_up",
            Action::SetBrightness { .. } => "brightness_down",
            Action::BrightnessUp { .. } => "brightness_up",
            Action::BrightnessDown { .. } => "brightness_down",
            Action::Vcp { .. } => "vcp",
            Action::ShowMonitorPanel => "show_monitor_panel",
            Action::ShowVolumeMixer => "show_volume_mixer",
            Action::MediaVolumeUp => "media_volume_up",
            Action::MediaVolumeDown => "media_volume_down",
            Action::MediaMute => "media_mute",
            Action::MediaPlayPause => "media_play_pause",
            Action::MediaStop => "media_stop",
            Action::MediaLastTrack => "media_last_track",
            Action::MediaNextTrack => "media_next_track",
            Action::ToggleTopmost => "toggle_topmost",
            Action::ToggleSuspendOnBlur => "toggle_suspend_on_blur",
            Action::ToggleThrottleOnBlur => "toggle_throttle_on_blur",
            Action::PowerActions => "power_actions",
            Action::ToggleQuietMode => "toggle_quiet_mode",
            Action::Sleep => "sleep",
            Action::Hibernate => "hibernate",
            Action::QuickDraw => "quick_draw",
            Action::QuickNote => "quick_note",
            Action::SwitchPowerPlan { .. } => "switch_power_plan",
            Action::ShowCpuPanel => "show_cpu_panel",
            Action::ShowLlmModels => "show_llm_models",
            Action::ShowProxyTrace => "show_proxy_trace",
            Action::ShowLlmActivity => "show_llm_activity",
            Action::ToggleLlmProxy => "toggle_llm_proxy",
            Action::ToggleQuotaWatcher => "toggle_quota_watcher",
            Action::VisionScreenshot => "vision_screenshot",
            Action::VisionSnip => "vision_snip",
            Action::Pomodoro => "pomodoro",
            Action::ToggleKeycast => "toggle_keycast",
            Action::Breathe { .. } => "breathe",
            Action::Quit => "quit",
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
            Action::Vcp {
                code,
                relative,
                value,
            } => {
                if *relative {
                    format!("vcp: 0x{:02X} {:+}", code, value)
                } else {
                    format!("vcp: 0x{:02X} = {}", code, value)
                }
            }
            Action::BrightnessUp { value } => format!("brightness_up: +{}", value),
            Action::BrightnessDown { value } => format!("brightness_down: -{}", value),
            Action::ShowMonitorPanel
            | Action::ShowVolumeMixer
            | Action::MediaVolumeUp
            | Action::MediaVolumeDown
            | Action::MediaMute
            | Action::MediaPlayPause
            | Action::MediaStop
            | Action::MediaLastTrack
            | Action::MediaNextTrack
            | Action::ToggleTopmost
            | Action::ToggleSuspendOnBlur
            | Action::ToggleThrottleOnBlur
            | Action::PowerActions
            | Action::ToggleQuietMode
            | Action::Sleep
            | Action::Hibernate
            | Action::QuickDraw
            | Action::QuickNote
            | Action::SwitchPowerPlan { .. }
            | Action::ShowCpuPanel
            | Action::ShowLlmModels
            | Action::ShowProxyTrace
            | Action::ShowLlmActivity
            | Action::ToggleLlmProxy
            | Action::ToggleQuotaWatcher
            | Action::Pomodoro
            | Action::ToggleKeycast
            | Action::Breathe { .. }
            | Action::VisionScreenshot
            | Action::VisionSnip
            | Action::Quit => self.name().to_string(),
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────

/// Parse the `value` field for brightness_up / brightness_down.
fn parse_brightness_step(fields: &ActionRawFields, name: &str) -> Result<u32, String> {
    let value = fields
        .value
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(5);
    if value == 0 {
        return Err(format!("{name} value must be > 0"));
    }
    Ok(value)
}

/// Category group for an action variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCategory {
    Input,
    Display,
    AudioMedia,
    Tools,
    LlmProxy,
    Automation,
    System,
}

impl ActionCategory {
    pub fn label(&self) -> &'static str {
        match self {
            ActionCategory::Input => "Input",
            ActionCategory::Display => "Display",
            ActionCategory::AudioMedia => "Audio / Media",
            ActionCategory::Tools => "Tools",
            ActionCategory::LlmProxy => "LLM Proxy",
            ActionCategory::Automation => "Automation",
            ActionCategory::System => "System",
        }
    }
}

/// Schema describing what kind of parameter an action expects.
/// Used by the settings editor to render the correct input UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionParamSchema {
    /// No parameter needed.
    None,
    /// Free‑form text input.
    Text,
    /// File path picker (exe, lnk, bat).
    FilePath,
    /// Key combination recording (for replace_key).
    KeyMapping,
    /// Numeric value with min/max/unit.
    Number {
        unit: &'static str,
        min: i32,
        max: i32,
    },
    /// Power action selector (shutdown, sleep, etc.).
    PowerAction,
}

// ── Action registry (single source of truth for editor integration) ──

/// Metadata for an action variant.
#[derive(Debug, Clone, Copy)]
pub struct ActionDescriptor {
    /// TOML action name (e.g. "replace_key").
    pub name: &'static str,
    /// Human-readable label (e.g. "Replace Key").
    pub label: &'static str,
    /// Short user-facing description for searchable pickers.
    pub description: &'static str,
    /// Category grouping.
    pub category: ActionCategory,
    /// TOML parameter key (e.g. "keys"), or `None` for parameterless actions.
    pub param_key: Option<&'static str>,
    /// Parameter schema for editor UI.
    pub param_schema: ActionParamSchema,
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
        description: "Send another key or shortcut instead.",
        category: ActionCategory::Input,
        param_key: Some("keys"),
        param_schema: ActionParamSchema::KeyMapping,
    },
    ActionDescriptor {
        name: "run_program",
        label: "Run Program",
        description: "Launch an executable, script, or shortcut.",
        category: ActionCategory::Automation,
        param_key: Some("path"),
        param_schema: ActionParamSchema::FilePath,
    },
    ActionDescriptor {
        name: "run_ps",
        label: "PowerShell",
        description: "Run a PowerShell command.",
        category: ActionCategory::Automation,
        param_key: Some("command"),
        param_schema: ActionParamSchema::Text,
    },
    ActionDescriptor {
        name: "brightness_up",
        label: "Brightness Up",
        description: "Increase monitor brightness by a percentage.",
        category: ActionCategory::Display,
        param_key: Some("value"),
        param_schema: ActionParamSchema::Number {
            unit: "%",
            min: 1,
            max: 100,
        },
    },
    ActionDescriptor {
        name: "brightness_down",
        label: "Brightness Down",
        description: "Decrease monitor brightness by a percentage.",
        category: ActionCategory::Display,
        param_key: Some("value"),
        param_schema: ActionParamSchema::Number {
            unit: "%",
            min: 1,
            max: 100,
        },
    },
    ActionDescriptor {
        name: "show_monitor_panel",
        label: "Monitor Control",
        description: "Open the monitor brightness/control panel.",
        category: ActionCategory::Display,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "show_volume_mixer",
        label: "Volume Mixer",
        description: "Open the app volume mixer overlay.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_volume_up",
        label: "Volume Up",
        description: "Send a media volume-up key press.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_volume_down",
        label: "Volume Down",
        description: "Send a media volume-down key press.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_mute",
        label: "Mute",
        description: "Toggle system audio mute.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_play_pause",
        label: "Play/Pause",
        description: "Send a media play/pause key press.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_stop",
        label: "Stop",
        description: "Send a media stop key press.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_last_track",
        label: "Last Track",
        description: "Skip to the previous media track.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "media_next_track",
        label: "Next Track",
        description: "Skip to the next media track.",
        category: ActionCategory::AudioMedia,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_topmost",
        label: "Toggle Always On Top",
        description: "Toggle topmost state for the active window.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_suspend_on_blur",
        label: "Suspend On Blur",
        description: "Suspend the active app when it loses focus.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_throttle_on_blur",
        label: "Throttle On Blur",
        description: "Throttle the active app after focus changes.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "power_actions",
        label: "Power Control",
        description: "Run a selected system power action.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::PowerAction,
    },
    ActionDescriptor {
        name: "sleep",
        label: "Sleep",
        description: "Put the computer to sleep immediately.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "hibernate",
        label: "Hibernate",
        description: "Hibernate the computer immediately.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_quiet_mode",
        label: "Quiet Mode",
        description: "Display off, CPU capped, machine keeps running.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "quick_draw",
        label: "Quick Draw",
        description: "Open the quick drawing/screenshot overlay.",
        category: ActionCategory::Tools,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "quick_note",
        label: "Quick Note",
        description: "Open the quick note overlay.",
        category: ActionCategory::Tools,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "pomodoro",
        label: "Pomodoro Timer",
        description: "Open or control the Pomodoro timer.",
        category: ActionCategory::Tools,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_keycast",
        label: "Toggle KeyCast",
        description: "Show or hide pressed keys for recordings and streams.",
        category: ActionCategory::Tools,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "breathe",
        label: "Breathe",
        description: "Open the paced breathing overlay. Optional preset: balanced, calm, extended, or auto.",
        category: ActionCategory::Tools,
        param_key: Some("preset"),
        param_schema: ActionParamSchema::Text,
    },
    ActionDescriptor {
        name: "switch_power_plan",
        label: "Switch Power Plan",
        description: "Switch to a configured Windows power plan.",
        category: ActionCategory::System,
        param_key: Some("target"),
        param_schema: ActionParamSchema::Text,
    },
    ActionDescriptor {
        name: "show_cpu_panel",
        label: "CPU Power Panel",
        description: "Open the CPU and power tuning overlay.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "show_llm_models",
        label: "LLM Models",
        description: "Open the LLM proxy model selector.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_llm_proxy",
        label: "Toggle LLM Proxy",
        description: "Enable or disable the local LLM proxy.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "toggle_quota_watcher",
        label: "Toggle Quota Watcher",
        description: "Enable or disable the background subscription-quota import.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "show_proxy_trace",
        label: "Proxy Trace",
        description: "Open the proxy routing trace window.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "show_llm_activity",
        label: "LLM Activity",
        description: "Open the LLM usage monitor (quota, activity, context trim).",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "quit",
        label: "Quit mhd",
        description: "Exit mhd.",
        category: ActionCategory::System,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "vision_screenshot",
        label: "Vision Screenshot",
        description: "Capture screen, analyze with vision LLM, copy result.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
    ActionDescriptor {
        name: "vision_snip",
        label: "Vision Snip",
        description: "Capture, crop, annotate, and analyze a screenshot.",
        category: ActionCategory::LlmProxy,
        param_key: None,
        param_schema: ActionParamSchema::None,
    },
];

/// Find the index of an action descriptor by its TOML name.
/// Returns `None` if not found.
pub fn find_action_index(name: &str) -> Option<usize> {
    ALL_ACTIONS.iter().position(|d| d.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::Modifiers;

    // ── Action name() stability ────────────────────────────────────

    #[test]
    fn test_action_name_stability() {
        // Every action variant has a stable name that round-trips
        let actions = [
            Action::ReplaceKey {
                keys: KeyCombo {
                    modifiers: Modifiers(0),
                    key: None,
                },
            },
            Action::RunPs {
                command: "echo".into(),
            },
            Action::RunProgram {
                path: "notepad.exe".into(),
            },
            Action::SwitchScheme {
                target_scheme: "default".into(),
            },
            Action::SetBrightness {
                relative: true,
                value: 5,
            },
            Action::BrightnessUp { value: 10 },
            Action::BrightnessDown { value: 10 },
            Action::Vcp {
                code: 0x10,
                relative: true,
                value: 5,
            },
            Action::ShowMonitorPanel,
            Action::ShowVolumeMixer,
            Action::MediaVolumeUp,
            Action::MediaVolumeDown,
            Action::MediaMute,
            Action::MediaPlayPause,
            Action::MediaStop,
            Action::MediaLastTrack,
            Action::MediaNextTrack,
            Action::ToggleTopmost,
            Action::ToggleSuspendOnBlur,
            Action::ToggleThrottleOnBlur,
            Action::PowerActions,
            Action::ToggleQuietMode,
            Action::Sleep,
            Action::Hibernate,
            Action::QuickDraw,
            Action::QuickNote,
            Action::SwitchPowerPlan {
                target: "next".into(),
            },
            Action::ShowCpuPanel,
            Action::VisionScreenshot,
            Action::Quit,
        ];

        for (i, action) in actions.iter().enumerate() {
            let name = action.name();
            // Every name should be a non-empty ASCII string
            assert!(!name.is_empty(), "action[{}] has empty name", i);
            assert!(name.is_ascii(), "action[{}] name '{}' not ASCII", i, name);
            // Round-trip: from_raw(name) with appropriate fields should succeed
            let fields = match action {
                Action::ReplaceKey { .. } => ActionRawFields {
                    keys: Some("ctrl+alt+x"),
                    command: None,
                    path: None,
                    target_scheme: None,
                    value: None,
                    code: None,
                    target: None,
                    preset: None,
                },
                Action::RunPs { .. } => ActionRawFields {
                    command: Some("echo test"),
                    keys: None,
                    path: None,
                    target_scheme: None,
                    value: None,
                    code: None,
                    target: None,
                    preset: None,
                },
                Action::RunProgram { .. } => ActionRawFields {
                    path: Some("notepad.exe"),
                    keys: None,
                    command: None,
                    target_scheme: None,
                    value: None,
                    code: None,
                    target: None,
                    preset: None,
                },
                Action::SwitchScheme { .. } => ActionRawFields {
                    target_scheme: Some("default"),
                    keys: None,
                    command: None,
                    path: None,
                    value: None,
                    code: None,
                    target: None,
                    preset: None,
                },
                Action::BrightnessUp { .. }
                | Action::BrightnessDown { .. }
                | Action::SetBrightness { .. } => ActionRawFields {
                    value: Some("10"),
                    keys: None,
                    command: None,
                    path: None,
                    target_scheme: None,
                    code: None,
                    target: None,
                    preset: None,
                },
                Action::Vcp { .. } => ActionRawFields {
                    code: Some("0x10"),
                    value: Some("+5"),
                    keys: None,
                    command: None,
                    path: None,
                    target_scheme: None,
                    target: None,
                    preset: None,
                },
                Action::SwitchPowerPlan { .. } => ActionRawFields {
                    target: Some("next"),
                    keys: None,
                    command: None,
                    path: None,
                    target_scheme: None,
                    value: None,
                    code: None,
                    preset: None,
                },
                _ => ActionRawFields {
                    keys: None,
                    command: None,
                    path: None,
                    target_scheme: None,
                    value: None,
                    code: None,
                    target: None,
                    preset: None,
                },
            };
            let roundtrip = Action::from_raw(name, fields);
            assert!(
                roundtrip.is_ok(),
                "action '{}' failed round-trip: {:?}",
                name,
                roundtrip
            );
        }
    }

    // ── Action creation validation ─────────────────────────────────

    #[test]
    fn test_replace_key_requires_valid_keys() {
        assert!(Action::new_replace_key("ctrl+alt+a").is_ok());
        // Empty triggers are invalid
        assert!(Action::new_replace_key("").is_err());
        // Unknown key
        assert!(Action::new_replace_key("ctrl+alt+foobar").is_err());
    }

    #[test]
    fn test_run_ps_requires_command() {
        assert!(Action::new_run_ps("echo hello").is_ok());
        assert!(Action::new_run_ps("").is_err());
        assert!(Action::new_run_ps("   ").is_err());
    }

    #[test]
    fn test_run_program_requires_path() {
        assert!(Action::new_run_program("notepad.exe").is_ok());
        assert!(Action::new_run_program("").is_err());
    }

    #[test]
    fn test_set_brightness_validation() {
        // Relative positive
        let a = Action::new_set_brightness("+5").unwrap();
        assert_eq!(
            a,
            Action::SetBrightness {
                relative: true,
                value: 5
            }
        );

        // Relative negative
        let a = Action::new_set_brightness("-3").unwrap();
        assert_eq!(
            a,
            Action::SetBrightness {
                relative: true,
                value: -3
            }
        );

        // Absolute
        let a = Action::new_set_brightness("75").unwrap();
        assert_eq!(
            a,
            Action::SetBrightness {
                relative: false,
                value: 75
            }
        );

        // Invalid
        assert!(Action::new_set_brightness("").is_err());
        assert!(Action::new_set_brightness("+0").is_err());
        assert!(Action::new_set_brightness("101").is_err());
        assert!(Action::new_set_brightness("abc").is_err());
    }

    #[test]
    fn test_vcp_validation() {
        // Relative positive
        let a = Action::new_vcp("0x10", "+5").unwrap();
        assert_eq!(
            a,
            Action::Vcp {
                code: 0x10,
                relative: true,
                value: 5
            }
        );

        // Absolute
        let a = Action::new_vcp("0x60", "3").unwrap();
        assert_eq!(
            a,
            Action::Vcp {
                code: 0x60,
                relative: false,
                value: 3
            }
        );

        // Invalid code format
        assert!(Action::new_vcp("xyz", "5").is_err());
        assert!(Action::new_vcp("", "5").is_err());

        // Invalid value
        assert!(Action::new_vcp("0x10", "").is_err());
        assert!(Action::new_vcp("0x10", "abc").is_err());
    }

    #[test]
    fn test_switch_scheme_validation() {
        assert!(Action::new_switch_scheme("gaming").is_ok());
        assert!(Action::new_switch_scheme("").is_err());
    }

    // ── Unknown action names ───────────────────────────────────────

    #[test]
    fn test_unknown_action_name() {
        let fields = ActionRawFields {
            keys: None,
            command: None,
            path: None,
            target_scheme: None,
            value: None,
            code: None,
            target: None,
            preset: None,
        };
        let result = Action::from_raw("nonexistent_action", fields);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown action"));
    }

    #[test]
    fn test_legacy_toggle_codex_watcher_still_parses() {
        // Why: existing key bindings use the old name; must never break.
        let fields = ActionRawFields {
            keys: None,
            command: None,
            path: None,
            target_scheme: None,
            value: None,
            code: None,
            target: None,
            preset: None,
        };
        let result = Action::from_raw("toggle_codex_watcher", fields);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Action::ToggleQuotaWatcher);
    }

    #[test]
    fn test_missing_required_field() {
        // replace_key without 'keys' field
        let fields = ActionRawFields {
            keys: None,
            command: None,
            path: None,
            target_scheme: None,
            value: None,
            code: None,
            target: None,
            preset: None,
        };
        let result = Action::from_raw("replace_key", fields);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("replace_key action requires 'keys' field")
        );

        // run_program without 'path' field
        let fields = ActionRawFields {
            keys: None,
            command: None,
            path: None,
            target_scheme: None,
            value: None,
            code: None,
            target: None,
            preset: None,
        };
        let result = Action::from_raw("run_program", fields);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("run_program action requires 'path' field")
        );
    }

    // ── Action descriptor registry ─────────────────────────────────

    #[test]
    fn test_find_action_index_found() {
        assert_eq!(find_action_index("quit"), Some(33)); // quit index in ALL_ACTIONS
        assert_eq!(find_action_index("replace_key"), Some(0));
        assert_eq!(find_action_index("brightness_up"), Some(3));
    }

    #[test]
    fn test_find_action_index_not_found() {
        assert_eq!(find_action_index("does_not_exist"), None);
        assert_eq!(find_action_index(""), None);
    }

    #[test]
    fn test_all_action_descriptors_unique_names() {
        let mut names: Vec<&str> = ALL_ACTIONS.iter().map(|d| d.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            ALL_ACTIONS.len(),
            "duplicate names in ALL_ACTIONS"
        );
    }

    #[test]
    fn test_all_action_descriptors_match_from_raw() {
        // Every descriptor name should be parseable by from_raw if given appropriate fields
        for desc in ALL_ACTIONS {
            let fields = ActionRawFields {
                keys: Some("ctrl+alt+x"),
                command: Some("echo"),
                path: Some("notepad.exe"),
                target_scheme: Some("default"),
                value: Some("10"),
                code: Some("0x10"),
                target: Some("next"),
                preset: Some("calm"),
            };
            let result = Action::from_raw(desc.name, fields);
            assert!(
                result.is_ok(),
                "descriptor '{}' cannot be parsed: {:?}",
                desc.name,
                result
            );
        }
    }
}
