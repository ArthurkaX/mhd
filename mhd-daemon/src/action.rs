use crate::trigger::{KeyCombo, parse_keys};

/// An action to execute when a trigger fires.
#[derive(Debug, Clone)]
pub enum Action {
    ReplaceKey { keys: KeyCombo },
    RunPs { command: String },
    SwitchScheme { target_scheme: String },
    SetBrightness { relative: bool, value: i32 },
    Vcp { code: u8, relative: bool, value: i32 },
    ShowVolumeMixer,
    Quit,
}

/// Raw fields for an action, used during parsing.
pub struct ActionRawFields<'a> {
    pub keys: Option<&'a str>,
    pub command: Option<&'a str>,
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
            "show_volume_mixer" => Ok(Action::ShowVolumeMixer),
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
            Action::ShowVolumeMixer => "show_volume_mixer".to_string(),
            Action::Quit => "quit".to_string(),
        }
    }
}