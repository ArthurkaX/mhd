//! Win32 platform glue — input simulation, modifier detection, and other
//! platform-specific helpers extracted from hook.rs and worker.rs for
//! better modularity.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

use crate::trigger::{KeyCombo, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN, Modifiers, PhysicalKey};

/// Get currently pressed modifier keys.
///
/// Uses `GetAsyncKeyState` to sample the physical state of modifier keys
/// at the time the hook callback fires.
pub fn get_pressed_modifiers() -> Modifiers {
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

/// Send a sequence of key events via `SendInput`.
///
/// Presses modifiers in order (alt, ctrl, shift, win), then presses and
/// releases the main key (if any), then releases modifiers in reverse
/// order (win, shift, ctrl, alt).
///
/// For **modifier‑only** combos (no main key) the function avoids sending
/// key‑up events for modifiers that are already physically held —
/// otherwise the synthetic release would fight with the user's still‑held
/// physical keys (the typical case when the trigger combo included those
/// same modifiers).
pub fn send_keys(keys: &KeyCombo) {
    let mut inputs: Vec<INPUT> = Vec::new();

    // Helper: check if a VK is currently physically pressed.
    let is_pressed = |vk: u16| -> bool { unsafe { GetAsyncKeyState(vk as i32) < 0 } };

    let is_modifier_only = keys.key.is_none();

    // Press modifiers (skip if already physically held for modifier-only combos)
    if keys.modifiers.alt() && !(is_modifier_only && is_pressed(0x12)) {
        push_key_event(&mut inputs, 0x12, false);
    }
    if keys.modifiers.ctrl() && !(is_modifier_only && is_pressed(0x11)) {
        push_key_event(&mut inputs, 0x11, false);
    }
    if keys.modifiers.shift() && !(is_modifier_only && is_pressed(0x10)) {
        push_key_event(&mut inputs, 0x10, false);
    }
    if keys.modifiers.win() && !(is_modifier_only && is_pressed(0x5B)) {
        push_key_event(&mut inputs, 0x5B, false);
    }

    // Press and release the main key (if present)
    match keys.key {
        Some(PhysicalKey::Keyboard(vk)) => {
            push_key_event(&mut inputs, vk as u16, false);
            push_key_event(&mut inputs, vk as u16, true);
        }
        Some(PhysicalKey::MouseButton(_))
        | Some(PhysicalKey::WheelUp)
        | Some(PhysicalKey::WheelDown)
        | Some(PhysicalKey::WheelLeft)
        | Some(PhysicalKey::WheelRight) => {
            eprintln!("mhd: warning: replace_key with mouse/wheel not supported");
        }
        None => {
            // Modifier-only combo (e.g., alt+shift):
            // Do NOT release modifiers — the user's physical key‑up will
            // take care of that.  Releasing them here would fight with
            // physically‑held keys from the trigger combo.
        }
    }

    // Release modifiers in reverse order (only for combos that have a main key).
    // Skip any modifier the user is still physically holding — the trigger combo
    // usually includes those same modifiers, so a synthetic key-up here would
    // desync modifier state until the user physically re-presses. The physical
    // key-up will release them naturally.
    if !is_modifier_only {
        if keys.modifiers.win() && !is_pressed(0x5B) {
            push_key_event(&mut inputs, 0x5B, true);
        }
        if keys.modifiers.shift() && !is_pressed(0x10) {
            push_key_event(&mut inputs, 0x10, true);
        }
        if keys.modifiers.ctrl() && !is_pressed(0x11) {
            push_key_event(&mut inputs, 0x11, true);
        }
        if keys.modifiers.alt() && !is_pressed(0x12) {
            push_key_event(&mut inputs, 0x12, true);
        }
    }

    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

/// Send a single media key via `SendInput` (e.g. VK_VOLUME_UP, VK_MEDIA_PLAY_PAUSE).
pub fn send_media_key(vk: u16) {
    send_media_key_n(vk, 1);
}

/// Send a media key `count` times (for multi‑step volume/brightness).
pub fn send_media_key_n(vk: u16, count: u32) {
    let count = count.max(1).min(100);
    let mut inputs = Vec::with_capacity((count * 2) as usize);
    for _ in 0..count {
        push_key_event(&mut inputs, vk, false);
        push_key_event(&mut inputs, vk, true);
    }
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn push_key_event(inputs: &mut Vec<INPUT>, vk: u16, up: bool) {
    let flags = if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };

    let ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        wScan: 0,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    inputs.push(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 { ki },
    });
}
