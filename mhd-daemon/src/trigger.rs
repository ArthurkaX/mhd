use std::collections::HashSet;

/// Bit flags for modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modifiers(pub u8);

pub const MOD_ALT: u8 = 0x01;
pub const MOD_CTRL: u8 = 0x02;
pub const MOD_SHIFT: u8 = 0x04;
pub const MOD_WIN: u8 = 0x08;

impl Modifiers {
    pub fn alt(&self) -> bool {
        self.0 & MOD_ALT != 0
    }
    pub fn ctrl(&self) -> bool {
        self.0 & MOD_CTRL != 0
    }
    pub fn shift(&self) -> bool {
        self.0 & MOD_SHIFT != 0
    }
    pub fn win(&self) -> bool {
        self.0 & MOD_WIN != 0
    }
}

/// A non-modifier key or mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalKey {
    Keyboard(u8),    // Windows virtual key code
    MouseButton(u8), // 1 = XBUTTON1, 2 = XBUTTON2
}

/// A trigger: modifiers + one non-modifier key/button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trigger {
    pub modifiers: Modifiers,
    pub key: PhysicalKey,
}

/// Parsed key combination for replace_key (allows modifier-only combos).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub modifiers: Modifiers,
    pub key: Option<PhysicalKey>,
}

/// Parsed trigger string (includes original text for logging).
#[derive(Debug, Clone)]
pub struct ParsedTrigger {
    pub trigger: Trigger,
    pub original: String,
}

/// Parse a keys value (same syntax as trigger) for replace_key action.
/// Unlike triggers, this allows modifier-only combinations.
pub fn parse_keys(s: &str) -> Result<KeyCombo, String> {
    let original = s.trim().to_lowercase();
    let parts: Vec<&str> = original.split('+').map(|p| p.trim()).collect();

    if parts.is_empty() {
        return Err("empty keys".to_string());
    }

    let mut modifiers = Modifiers(0);
    let mut key: Option<PhysicalKey> = None;
    let mut seen_modifiers = HashSet::new();

    for part in &parts {
        if let Some(mod_flag) = parse_modifier(part) {
            if !seen_modifiers.insert(*part) {
                return Err(format!("duplicate modifier in keys: '{}'", part));
            }
            modifiers = Modifiers(modifiers.0 | mod_flag);
        } else if let Some(parsed_key) = parse_key(part)? {
            if key.is_some() {
                return Err(format!("multiple non-modifier keys in keys: '{}'", s));
            }
            key = Some(parsed_key);
        } else {
            return Err(format!("unknown key: '{}'", part));
        }
    }

    Ok(KeyCombo { modifiers, key })
}

/// Parse a trigger string like "alt+shift+1" or "mouseButton1".
/// A trigger MUST contain exactly one non-modifier key/button.
pub fn parse_trigger(s: &str) -> Result<ParsedTrigger, String> {
    let original = s.trim().to_lowercase();
    let parts: Vec<&str> = original.split('+').map(|p| p.trim()).collect();

    if parts.is_empty() {
        return Err("empty trigger".to_string());
    }

    let mut modifiers = Modifiers(0);
    let mut key: Option<PhysicalKey> = None;
    let mut seen_modifiers = HashSet::new();

    for part in &parts {
        if let Some(mod_flag) = parse_modifier(part) {
            if !seen_modifiers.insert(*part) {
                return Err(format!("duplicate modifier in trigger: '{}'", part));
            }
            modifiers = Modifiers(modifiers.0 | mod_flag);
        } else if let Some(parsed_key) = parse_key(part)? {
            if key.is_some() {
                return Err(format!("multiple non-modifier keys in trigger: '{}'", s));
            }
            key = Some(parsed_key);
        } else {
            return Err(format!("unknown key: '{}'", part));
        }
    }

    let key = key.ok_or_else(|| format!("no non-modifier key in trigger: '{}'", s))?;

    Ok(ParsedTrigger {
        trigger: Trigger { modifiers, key },
        original: s.trim().to_string(),
    })
}

fn parse_modifier(s: &str) -> Option<u8> {
    match s {
        "alt" => Some(MOD_ALT),
        "ctrl" | "control" => Some(MOD_CTRL),
        "shift" => Some(MOD_SHIFT),
        "win" | "super" => Some(MOD_WIN),
        _ => None,
    }
}

fn parse_key(s: &str) -> Result<Option<PhysicalKey>, String> {
    // Mouse buttons — conventional names
    // MouseButton4 = XBUTTON1, MouseButton5 = XBUTTON2
    if s == "mousebutton4" {
        return Ok(Some(PhysicalKey::MouseButton(1)));
    }
    if s == "mousebutton5" {
        return Ok(Some(PhysicalKey::MouseButton(2)));
    }

    // Letters a-z
    if s.len() == 1 {
        let ch = s.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            let vk = b'A' + (ch.to_ascii_uppercase() as u8 - b'A');
            return Ok(Some(PhysicalKey::Keyboard(vk)));
        }
        if ch.is_ascii_digit() {
            let vk = ch as u8;
            return Ok(Some(PhysicalKey::Keyboard(vk)));
        }
    }

    // Function keys f1-f24
    if s.starts_with('f')
        && s.len() <= 3
        && let Ok(n) = s[1..].parse::<u8>()
        && (1..=24).contains(&n)
    {
        let vk = 0x70u8 + (n - 1); // VK_F1 = 0x70
        return Ok(Some(PhysicalKey::Keyboard(vk)));
    }

    // Named keys
    let vk = match s {
        "capslock" | "capital" => 0x14,
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "prior" => 0x21,
        "pagedown" | "next" => 0x22,
        "left" => 0x25,
        "right" => 0x27,
        "up" => 0x26,
        "down" => 0x28,
        "contextmenu" | "apps" => 0x5D,
        "scrolllock" => 0x91,
        "numlock" => 0x90,
        "printscreen" => 0x2C,

        "lshift" => 0xA0,
        "rshift" => 0xA1,
        "lctrl" | "lcontrol" => 0xA2,
        "rctrl" | "rcontrol" => 0xA3,
        "lalt" | "lmenu" => 0xA4,
        "ralt" | "rmenu" => 0xA5,
        "lwin" => 0x5B,
        "rwin" => 0x5C,

        // OEM keys
        "minus" | "oem_minus" => 0xBD,
        "equal" | "oem_equal" | "equals" => 0xBB,
        "comma" | "oem_comma" => 0xBC,
        "period" | "oem_period" => 0xBE,
        "slash" | "oem_slash" => 0xBF,
        "semicolon" | "oem_semicolon" => 0xBA,
        "quote" | "oem_quote" => 0xDE,
        "backslash" | "oem_backslash" => 0xDC,
        "lbracket" | "oem_lbracket" | "oem_4" => 0xDB,
        "rbracket" | "oem_rbracket" | "oem_6" => 0xDD,
        "backquote" | "oem_3" | "grave" => 0xC0,

        // Numpad keys
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        "numpadmultiply" | "numpad_star" => 0x6A,
        "numpadadd" | "numpad_plus" | "numpad_add" => 0x6B,
        "numpadsubtract" | "numpad_minus" | "numpad_subtract" => 0x6D,
        "numpaddivide" | "numpad_slash" => 0x6F,
        "numpadenter" => 0x6C,
        "numpaddecimal" | "numpad_dot" => 0x6E,

        // Media keys
        "volume_mute" => 0xAD,
        "volume_down" => 0xAE,
        "volume_up" => 0xAF,
        "media_next" => 0xB0,
        "media_prev" => 0xB1,
        "media_stop" => 0xB2,
        "media_play_pause" => 0xB3,

        _ => {
            if s.starts_with("0x") && let Ok(vk) = u8::from_str_radix(&s[2..], 16) {
                vk
            } else {
                return Err(format!("unknown key: '{}'", s));
            }
        }
    };

    Ok(Some(PhysicalKey::Keyboard(vk)))
}

/// Check if a virtual key code is a modifier key.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    )
}

/// Get currently pressed modifier keys.
pub fn get_pressed_modifiers() -> Modifiers {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };

    let mut mods = 0u8;
    unsafe {
        if (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0 {
            mods |= MOD_ALT;
        }
        if (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0 {
            mods |= MOD_CTRL;
        }
        if (GetAsyncKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0 {
            mods |= MOD_SHIFT;
        }
        if (GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0
            || (GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0
        {
            mods |= MOD_WIN;
        }
    }
    Modifiers(mods)
}

/// Convert a KeyCombo back to a string like "alt+shift+a".
pub fn keys_to_string(keys: &KeyCombo) -> String {
    let mut parts = Vec::new();
    if keys.modifiers.ctrl() {
        parts.push("ctrl".to_string());
    }
    if keys.modifiers.alt() {
        parts.push("alt".to_string());
    }
    if keys.modifiers.shift() {
        parts.push("shift".to_string());
    }
    if keys.modifiers.win() {
        parts.push("win".to_string());
    }
    if let Some(key) = keys.key {
        match key {
            PhysicalKey::Keyboard(vk) => {
                parts.push(vk_to_string(vk));
            }
            PhysicalKey::MouseButton(n) => {
                parts.push(format!("mousebutton{}", n + 3));
            }
        }
    }
    parts.join("+")
}

fn vk_to_string(vk: u8) -> String {
    match vk {
        0x30..=0x39 => (vk as char).to_string().to_lowercase(),
        0x41..=0x5A => (vk as char).to_string().to_lowercase(),
        0x70..=0x87 => format!("f{}", vk - 0x70 + 1),
        0x60..=0x69 => format!("numpad{}", vk - 0x60),
        0x14 => "capslock".into(),
        0x20 => "space".into(),
        0x09 => "tab".into(),
        0x0D => "enter".into(),
        0x1B => "esc".into(),
        0x08 => "backspace".into(),
        0x2E => "delete".into(),
        0x2D => "insert".into(),
        0x24 => "home".into(),
        0x23 => "end".into(),
        0x21 => "pageup".into(),
        0x22 => "pagedown".into(),
        0x25 => "left".into(),
        0x27 => "right".into(),
        0x26 => "up".into(),
        0x28 => "down".into(),
        0x5D => "contextmenu".into(),
        0x91 => "scrolllock".into(),
        0x90 => "numlock".into(),
        0x2C => "printscreen".into(),
        0xA0 => "lshift".into(),
        0xA1 => "rshift".into(),
        0xA2 => "lctrl".into(),
        0xA3 => "rctrl".into(),
        0xA4 => "lalt".into(),
        0xA5 => "ralt".into(),
        0x5B => "lwin".into(),
        0x5C => "rwin".into(),
        0xBD => "minus".into(),
        0xBB => "equal".into(),
        0xBC => "comma".into(),
        0xBE => "period".into(),
        0xBF => "slash".into(),
        0xBA => "semicolon".into(),
        0xDE => "quote".into(),
        0xDC => "backslash".into(),
        0xDB => "lbracket".into(),
        0xDD => "rbracket".into(),
        0xC0 => "backquote".into(),
        0x6A => "numpad_star".into(),
        0x6B => "numpad_plus".into(),
        0x6D => "numpad_minus".into(),
        0x6F => "numpad_slash".into(),
        0x6C => "numpadenter".into(),
        0x6E => "numpad_dot".into(),
        0xAD => "volume_mute".into(),
        0xAE => "volume_down".into(),
        0xAF => "volume_up".into(),
        0xB0 => "media_next".into(),
        0xB1 => "media_prev".into(),
        0xB2 => "media_stop".into(),
        0xB3 => "media_play_pause".into(),
        _ => format!("0x{:02x}", vk),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_modifier_vk() {
        assert!(is_modifier_vk(0x10)); // VK_SHIFT
        assert!(is_modifier_vk(0xA0)); // VK_LSHIFT
        assert!(is_modifier_vk(0xA1)); // VK_RSHIFT
        assert!(is_modifier_vk(0x5B)); // VK_LWIN
        assert!(!is_modifier_vk(b'S' as u32));
    }

    #[test]
    fn test_parse_trigger_win_shift_s() {
        let pt = parse_trigger("win+shift+s").unwrap();
        assert!(pt.trigger.modifiers.win());
        assert!(pt.trigger.modifiers.shift());
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(b'S'));
    }

    #[test]
    fn test_keys_to_string_win_shift_s() {
        let keys = KeyCombo {
            modifiers: Modifiers(MOD_WIN | MOD_SHIFT),
            key: Some(PhysicalKey::Keyboard(b'S')),
        };
        assert_eq!(keys_to_string(&keys), "shift+win+s");
    }

    #[test]
    fn test_vk_to_string_modifiers() {
        assert_eq!(vk_to_string(0xA0), "lshift");
        assert_eq!(vk_to_string(0x5B), "lwin");
    }
}
