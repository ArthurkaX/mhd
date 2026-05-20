//! Win32 platform glue — input simulation, modifier detection, and other
//! platform-specific helpers extracted from hook.rs and worker.rs for
//! better modularity.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

use crate::trigger::{KeyCombo, Modifiers, PhysicalKey, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};

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
pub fn send_keys(keys: &KeyCombo) {
    let mut inputs: Vec<INPUT> = Vec::new();

    // Press modifiers
    if keys.modifiers.alt() {
        push_key_event(&mut inputs, 0x12, false);
    }
    if keys.modifiers.ctrl() {
        push_key_event(&mut inputs, 0x11, false);
    }
    if keys.modifiers.shift() {
        push_key_event(&mut inputs, 0x10, false);
    }
    if keys.modifiers.win() {
        push_key_event(&mut inputs, 0x5B, false);
    }

    // Press and release the main key (if present)
    match keys.key {
        Some(PhysicalKey::Keyboard(vk)) => {
            push_key_event(&mut inputs, vk as u16, false);
            push_key_event(&mut inputs, vk as u16, true);
        }
        Some(PhysicalKey::MouseButton(_)) => {
            eprintln!("mhd: warning: replace_key with mouse button not supported");
        }
        None => {
            // Modifier-only combo (e.g., alt+shift): press and release modifiers
        }
    }

    // Release modifiers in reverse order
    if keys.modifiers.win() {
        push_key_event(&mut inputs, 0x5B, true);
    }
    if keys.modifiers.shift() {
        push_key_event(&mut inputs, 0x10, true);
    }
    if keys.modifiers.ctrl() {
        push_key_event(&mut inputs, 0x11, true);
    }
    if keys.modifiers.alt() {
        push_key_event(&mut inputs, 0x12, true);
    }

    if !inputs.is_empty() {
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

/// Send a single media key via `SendInput` (e.g. VK_VOLUME_UP, VK_MEDIA_PLAY_PAUSE).
pub fn send_media_key(vk: u16) {
    let mut inputs = Vec::new();
    push_key_event(&mut inputs, vk, false);
    push_key_event(&mut inputs, vk, true);
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
