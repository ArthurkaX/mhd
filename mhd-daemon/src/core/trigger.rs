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

/// A non-modifier key, mouse button, or wheel direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalKey {
    Keyboard(u8),    // Windows virtual key code
    MouseButton(u8), // 1 = XBUTTON1, 2 = XBUTTON2, 3 = MBUTTON (middle)
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
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

/// Internal: parse a modifier+key string into (modifiers, optional key).
/// Used by both `parse_keys` and `parse_trigger`.
fn parse_combo_inner(s: &str, label: &str) -> Result<(Modifiers, Option<PhysicalKey>), String> {
    let original = s.trim().to_lowercase();
    let parts: Vec<&str> = original.split('+').map(|p| p.trim()).collect();

    if parts.is_empty() {
        return Err(format!("empty {}", label));
    }

    let mut modifiers = Modifiers(0);
    let mut key: Option<PhysicalKey> = None;
    let mut seen_modifiers = HashSet::new();

    for part in &parts {
        if let Some(mod_flag) = parse_modifier(part) {
            if !seen_modifiers.insert(*part) {
                return Err(format!("duplicate modifier in {}: '{}'", label, part));
            }
            modifiers = Modifiers(modifiers.0 | mod_flag);
        } else if let Some(parsed_key) = parse_key(part)? {
            if key.is_some() {
                return Err(format!("multiple non-modifier keys in {}: '{}'", label, s));
            }
            key = Some(parsed_key);
        } else {
            return Err(format!("unknown key in {}: '{}'", label, part));
        }
    }

    Ok((modifiers, key))
}

/// Parse a keys value (same syntax as trigger) for replace_key action.
/// Unlike triggers, this allows modifier-only combinations.
pub fn parse_keys(s: &str) -> Result<KeyCombo, String> {
    let (modifiers, key) = parse_combo_inner(s, "keys")?;
    Ok(KeyCombo { modifiers, key })
}

/// Parse a trigger string like "alt+shift+1" or "mouseButton1".
/// A trigger MUST contain exactly one non-modifier key/button.
pub fn parse_trigger(s: &str) -> Result<ParsedTrigger, String> {
    let (modifiers, key) = parse_combo_inner(s, "trigger")?;
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
    // mousebutton3 = middle, mousebutton4 = XBUTTON1, mousebutton5 = XBUTTON2
    if s == "mousebutton3" {
        return Ok(Some(PhysicalKey::MouseButton(3)));
    }
    if s == "mousebutton4" {
        return Ok(Some(PhysicalKey::MouseButton(1)));
    }
    if s == "mousebutton5" {
        return Ok(Some(PhysicalKey::MouseButton(2)));
    }

    // Wheel / tilt events
    if s == "wheel_up" {
        return Ok(Some(PhysicalKey::WheelUp));
    }
    if s == "wheel_down" {
        return Ok(Some(PhysicalKey::WheelDown));
    }
    if s == "wheel_left" {
        return Ok(Some(PhysicalKey::WheelLeft));
    }
    if s == "wheel_right" {
        return Ok(Some(PhysicalKey::WheelRight));
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

    // Numpad digits 0-9 (algorithmic)
    if let Some(rest) = s.strip_prefix("numpad")
        && rest.len() == 1
        && let Ok(n) = rest.parse::<u8>()
    {
        return Ok(Some(PhysicalKey::Keyboard(0x60 + n)));
    }

    // Named keys (irregular) — single source of truth shared with `vk_to_name`.
    if let Some(&(vk, _)) = NAMED_VKS.iter().find(|(_, names)| names.contains(&s)) {
        return Ok(Some(PhysicalKey::Keyboard(vk)));
    }

    // Raw hex virtual-key code, e.g. "0x7b"
    if let Some(hex) = s.strip_prefix("0x")
        && let Ok(vk) = u8::from_str_radix(hex, 16)
    {
        return Ok(Some(PhysicalKey::Keyboard(vk)));
    }

    Err(format!("unknown key: '{}'", s))
}

/// Irregular named virtual keys, shared by [`parse_key`] and [`vk_to_name`].
///
/// Each entry is `(vk, &[canonical, aliases...])`: the **first** name is the
/// canonical one emitted by `vk_to_name`; every name in the slice is accepted
/// by `parse_key`. Algorithmic ranges (a–z, 0–9, F1–F24, numpad0–9) are handled
/// directly in both functions and are intentionally absent here.
///
/// This is the single source of truth — adding a key here teaches both
/// directions at once (the `test_named_vks_round_trip` test enforces it).
#[rustfmt::skip]
const NAMED_VKS: &[(u8, &[&str])] = &[
    (0x14, &["capslock", "capital"]),
    (0x20, &["space"]),
    (0x09, &["tab"]),
    (0x0D, &["enter", "return"]),
    (0x1B, &["esc", "escape"]),
    (0x08, &["backspace"]),
    (0x2E, &["delete", "del"]),
    (0x2D, &["insert", "ins"]),
    (0x24, &["home"]),
    (0x23, &["end"]),
    (0x21, &["pageup", "prior"]),
    (0x22, &["pagedown", "next"]),
    (0x25, &["left"]),
    (0x27, &["right"]),
    (0x26, &["up"]),
    (0x28, &["down"]),
    (0x5D, &["contextmenu", "apps"]),
    (0x91, &["scrolllock"]),
    (0x90, &["numlock"]),
    (0x2C, &["printscreen"]),
    (0x13, &["pause", "break"]),

    (0xA0, &["lshift"]),
    (0xA1, &["rshift"]),
    (0xA2, &["lctrl", "lcontrol"]),
    (0xA3, &["rctrl", "rcontrol"]),
    (0xA4, &["lalt", "lmenu"]),
    (0xA5, &["ralt", "rmenu"]),
    (0x5B, &["lwin"]),
    (0x5C, &["rwin"]),

    // OEM keys
    (0xBD, &["minus", "oem_minus"]),
    (0xBB, &["equal", "oem_equal", "equals"]),
    (0xBC, &["comma", "oem_comma"]),
    (0xBE, &["period", "oem_period"]),
    (0xBF, &["slash", "oem_slash"]),
    (0xBA, &["semicolon", "oem_semicolon"]),
    (0xDE, &["quote", "oem_quote"]),
    (0xDC, &["backslash", "oem_backslash"]),
    (0xDB, &["lbracket", "oem_lbracket", "oem_4"]),
    (0xDD, &["rbracket", "oem_rbracket", "oem_6"]),
    (0xC0, &["backquote", "oem_3", "grave"]),

    // Numpad operators
    (0x6A, &["numpad_star", "numpadmultiply"]),
    (0x6B, &["numpad_plus", "numpadadd", "numpad_add"]),
    (0x6D, &["numpad_minus", "numpadsubtract", "numpad_subtract"]),
    (0x6F, &["numpad_slash", "numpaddivide"]),
    (0x6C, &["numpadenter"]),
    (0x6E, &["numpad_dot", "numpaddecimal"]),

    // Media keys
    (0xAD, &["volume_mute"]),
    (0xAE, &["volume_down"]),
    (0xAF, &["volume_up"]),
    (0xB0, &["media_next"]),
    (0xB1, &["media_prev"]),
    (0xB2, &["media_stop"]),
    (0xB3, &["media_play_pause"]),
];

pub fn known_key_names() -> Vec<String> {
    let mut names = Vec::new();
    for ch in b'a'..=b'z' {
        names.push((ch as char).to_string());
    }
    for ch in b'0'..=b'9' {
        names.push((ch as char).to_string());
    }
    for n in 1..=24 {
        names.push(format!("f{n}"));
    }
    for n in 0..=9 {
        names.push(format!("numpad{n}"));
    }
    for &(_, aliases) in NAMED_VKS {
        names.push(aliases[0].to_string());
    }
    names.extend([
        "mousebutton3".to_string(),
        "mousebutton4".to_string(),
        "mousebutton5".to_string(),
        "wheel_up".to_string(),
        "wheel_down".to_string(),
        "wheel_left".to_string(),
        "wheel_right".to_string(),
    ]);
    names
}

/// Check if a virtual key code is a modifier key.
pub fn is_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    )
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
                parts.push(vk_to_name(vk));
            }
            PhysicalKey::MouseButton(n) => {
                let name = match n {
                    1 => "mousebutton4",
                    2 => "mousebutton5",
                    3 => "mousebutton3",
                    _ => "mousebutton?",
                };
                parts.push(name.to_string());
            }
            PhysicalKey::WheelUp => parts.push("wheel_up".to_string()),
            PhysicalKey::WheelDown => parts.push("wheel_down".to_string()),
            PhysicalKey::WheelLeft => parts.push("wheel_left".to_string()),
            PhysicalKey::WheelRight => parts.push("wheel_right".to_string()),
        }
    }
    parts.join("+")
}

pub fn vk_to_name(vk: u8) -> String {
    // Algorithmic ranges first.
    match vk {
        0x30..=0x39 | 0x41..=0x5A => return (vk as char).to_ascii_lowercase().to_string(),
        0x70..=0x87 => return format!("f{}", vk - 0x70 + 1),
        0x60..=0x69 => return format!("numpad{}", vk - 0x60),
        _ => {}
    }

    // Irregular named keys — same table `parse_key` reads, so the canonical
    // name round-trips back to this vk.
    NAMED_VKS
        .iter()
        .find(|(code, _)| *code == vk)
        .map(|(_, names)| names[0].to_string())
        .unwrap_or_else(|| format!("0x{:02x}", vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Modifier key detection ────────────────────────────────────

    #[test]
    fn test_is_modifier_vk() {
        assert!(is_modifier_vk(0x10)); // VK_SHIFT
        assert!(is_modifier_vk(0xA0)); // VK_LSHIFT
        assert!(is_modifier_vk(0xA1)); // VK_RSHIFT
        assert!(is_modifier_vk(0x5B)); // VK_LWIN
        assert!(!is_modifier_vk(b'S' as u32));
    }

    // ── Standard trigger parsing ──────────────────────────────────

    #[test]
    fn test_parse_trigger_win_shift_s() {
        let pt = parse_trigger("win+shift+s").unwrap();
        assert!(pt.trigger.modifiers.win());
        assert!(pt.trigger.modifiers.shift());
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(b'S'));
    }

    #[test]
    fn test_parse_trigger_ctrl_alt_1() {
        let pt = parse_trigger("ctrl+alt+1").unwrap();
        assert!(pt.trigger.modifiers.ctrl());
        assert!(pt.trigger.modifiers.alt());
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(b'1'));
    }

    #[test]
    fn test_parse_trigger_just_key() {
        let pt = parse_trigger("f5").unwrap();
        assert!(!pt.trigger.modifiers.ctrl());
        assert!(!pt.trigger.modifiers.alt());
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(0x74)); // VK_F5
    }

    #[test]
    fn test_parse_trigger_mouse_button() {
        let pt = parse_trigger("mousebutton4").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::MouseButton(1));
    }

    #[test]
    fn test_parse_trigger_wheel() {
        let pt = parse_trigger("wheel_up").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::WheelUp);

        let pt = parse_trigger("wheel_down").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::WheelDown);
    }

    #[test]
    fn test_parse_trigger_special_keys() {
        let cases = [
            ("space", 0x20),
            ("tab", 0x09),
            ("enter", 0x0D),
            ("esc", 0x1B),
            ("backspace", 0x08),
            ("delete", 0x2E),
            ("insert", 0x2D),
            ("home", 0x24),
            ("end", 0x23),
            ("pageup", 0x21),
            ("pagedown", 0x22),
            ("left", 0x25),
            ("right", 0x27),
            ("up", 0x26),
            ("down", 0x28),
        ];
        for (name, vk) in &cases {
            let pt = parse_trigger(name).unwrap();
            assert_eq!(
                pt.trigger.key,
                PhysicalKey::Keyboard(*vk),
                "failed for '{name}'"
            );
        }
    }

    #[test]
    fn test_parse_trigger_numpad() {
        let pt = parse_trigger("numpad0").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(0x60));

        let pt = parse_trigger("numpad_plus").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(0x6B));
    }

    #[test]
    fn test_parse_trigger_hex_key() {
        let pt = parse_trigger("0x7B").unwrap();
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(0x7B));
    }

    // ── Edge cases / error handling ───────────────────────────────

    #[test]
    fn test_parse_trigger_empty_fails() {
        assert!(parse_trigger("").is_err());
        assert!(parse_trigger("   ").is_err());
    }

    #[test]
    fn test_parse_trigger_modifier_only_fails() {
        assert!(parse_trigger("ctrl+alt").is_err());
        assert!(parse_trigger("win+shift").is_err());
    }

    #[test]
    fn test_parse_trigger_duplicate_modifier_fails() {
        assert!(parse_trigger("ctrl+ctrl+a").is_err());
        assert!(parse_trigger("shift+alt+shift+x").is_err());
    }

    #[test]
    fn test_parse_trigger_multiple_keys_fails() {
        assert!(parse_trigger("a+b").is_err());
        assert!(parse_trigger("ctrl+a+b").is_err());
    }

    #[test]
    fn test_parse_trigger_unknown_key_fails() {
        assert!(parse_trigger("ctrl+alt+foobar").is_err());
    }

    #[test]
    fn test_parse_trigger_case_insensitive() {
        let pt = parse_trigger("CTRL+ALT+S").unwrap();
        assert!(pt.trigger.modifiers.ctrl());
        assert!(pt.trigger.modifiers.alt());
        assert_eq!(pt.trigger.key, PhysicalKey::Keyboard(b'S'));

        let pt = parse_trigger("Ctrl+Shift+F1").unwrap();
        assert!(pt.trigger.modifiers.ctrl());
        assert!(pt.trigger.modifiers.shift());
    }

    // ── Keys (modifier-only combos for replace_key) ─────────────────

    #[test]
    fn test_parse_keys_modifier_only() {
        // Keys allows modifier-only combos
        let kc = parse_keys("ctrl+alt+shift").unwrap();
        assert!(kc.modifiers.ctrl());
        assert!(kc.modifiers.alt());
        assert!(kc.modifiers.shift());
        assert!(kc.key.is_none());
    }

    #[test]
    fn test_parse_keys_with_key() {
        let kc = parse_keys("ctrl+win+z").unwrap();
        assert!(kc.modifiers.ctrl());
        assert!(kc.modifiers.win());
        assert_eq!(kc.key, Some(PhysicalKey::Keyboard(b'Z')));
    }

    // ── keys_to_string round-trip ──────────────────────────────────

    #[test]
    fn test_keys_to_string_win_shift_s() {
        let keys = KeyCombo {
            modifiers: Modifiers(MOD_WIN | MOD_SHIFT),
            key: Some(PhysicalKey::Keyboard(b'S')),
        };
        assert_eq!(keys_to_string(&keys), "shift+win+s");
    }

    #[test]
    fn test_keys_to_string_no_modifiers() {
        let keys = KeyCombo {
            modifiers: Modifiers(0),
            key: Some(PhysicalKey::Keyboard(b'A')),
        };
        assert_eq!(keys_to_string(&keys), "a");
    }

    #[test]
    fn test_keys_to_string_mouse() {
        let keys = KeyCombo {
            modifiers: Modifiers(0),
            key: Some(PhysicalKey::MouseButton(1)),
        };
        assert_eq!(keys_to_string(&keys), "mousebutton4");
    }

    #[test]
    fn test_keys_to_string_wheel() {
        let keys = KeyCombo {
            modifiers: Modifiers(MOD_CTRL),
            key: Some(PhysicalKey::WheelUp),
        };
        assert_eq!(keys_to_string(&keys), "ctrl+wheel_up");
    }

    #[test]
    fn test_keys_to_string_modifier_only() {
        let keys = KeyCombo {
            modifiers: Modifiers(MOD_ALT | MOD_SHIFT),
            key: None,
        };
        assert_eq!(keys_to_string(&keys), "alt+shift");
    }

    // ── vk_to_name round-trip for common cases ─────────────────────

    #[test]
    fn test_vk_to_name_modifiers() {
        assert_eq!(vk_to_name(0xA0), "lshift");
        assert_eq!(vk_to_name(0x5B), "lwin");
    }

    #[test]
    fn test_vk_to_name_function_keys() {
        for i in 0..=12 {
            let vk = 0x70 + i;
            assert_eq!(vk_to_name(vk), format!("f{}", i + 1));
        }
    }

    #[test]
    fn test_vk_to_name_unknown_returns_hex() {
        assert_eq!(vk_to_name(0xFF), "0xff");
    }

    // ── Single-source-of-truth invariant for NAMED_VKS ─────────────

    #[test]
    fn test_named_vks_round_trip() {
        for &(vk, names) in NAMED_VKS {
            // Canonical name is what vk_to_name emits.
            assert_eq!(vk_to_name(vk), names[0], "vk_to_name(0x{vk:02x})");
            // Every alias parses back to this vk.
            for name in names {
                let parsed = parse_key(name).unwrap();
                assert_eq!(
                    parsed,
                    Some(PhysicalKey::Keyboard(vk)),
                    "parse_key({name:?})"
                );
            }
        }
    }

    #[test]
    fn test_named_vks_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for &(_, names) in NAMED_VKS {
            for name in names {
                assert!(seen.insert(*name), "duplicate key name in table: {name:?}");
            }
        }
    }

    #[test]
    fn test_parse_trigger_pause() {
        assert_eq!(
            parse_trigger("pause").unwrap().trigger.key,
            PhysicalKey::Keyboard(0x13)
        );
        assert_eq!(
            parse_trigger("break").unwrap().trigger.key,
            PhysicalKey::Keyboard(0x13)
        );
    }
}
