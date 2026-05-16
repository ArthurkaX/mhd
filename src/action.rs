use crate::trigger::{parse_keys, PhysicalKey, KeyCombo};

/// An action to execute when a trigger fires.
#[derive(Debug, Clone)]
pub enum Action {
    ReplaceKey { keys: KeyCombo },
    RunPs { command: String },
    SwitchScheme { target_scheme: String },
}

impl Action {
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

    /// Get a human-readable description of the action.
    pub fn describe(&self) -> String {
        match self {
            Action::ReplaceKey { keys } => {
                let mut parts = Vec::new();
                if keys.modifiers.alt() { parts.push("Alt".to_string()); }
                if keys.modifiers.ctrl() { parts.push("Ctrl".to_string()); }
                if keys.modifiers.shift() { parts.push("Shift".to_string()); }
                if keys.modifiers.win() { parts.push("Win".to_string()); }
                match keys.key {
                    Some(PhysicalKey::Keyboard(vk)) => parts.push(vk_to_name(vk)),
                    Some(PhysicalKey::MouseButton(n)) => parts.push(format!("MouseButton{n}")),
                    None => {} // modifier-only combo
                }
                format!("replace_key: {}", parts.join("+"))
            }
            Action::RunPs { command } => format!("run_ps: {command}"),
            Action::SwitchScheme { target_scheme } => format!("switch_scheme: {target_scheme}"),
        }
    }
}

fn vk_to_name(vk: u8) -> String {
    match vk {
        0x08 => "Backspace".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x14 => "CapsLock".to_string(),
        0x20 => "Space".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2C => "PrintScreen".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x30..=0x39 => (vk as char).to_string(),
        0x41..=0x5A => (vk as char).to_string(),
        0x5B => "LWin".to_string(),
        0x5C => "RWin".to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x90 => "NumLock".to_string(),
        0x91 => "ScrollLock".to_string(),
        _ => format!("0x{:02X}", vk),
    }
}
