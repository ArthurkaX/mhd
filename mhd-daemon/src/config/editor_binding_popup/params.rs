//! Parameter-file selection and captured-key serialization for the binding
//! editor popup.
//!
//! Owns the OS file-picker dialog for `FilePath` parameters, the default
//! parameter value for each `ActionParamSchema`, and the decoding of a
//! captured key (`WM_BINDING_CAPTURED` data) into its display string.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

use crate::core::action::ActionParamSchema;
use crate::core::trigger::{KeyCombo, Modifiers, PhysicalKey, keys_to_string};

/// Return the default param value for a given schema.
///
/// Called when the user switches the action kind, so stale data from the
/// previous action is replaced with something sensible for the new one.
pub(crate) fn default_param_for_schema(schema: ActionParamSchema) -> String {
    match schema {
        ActionParamSchema::Number { .. } => "5".to_string(),
        ActionParamSchema::None
        | ActionParamSchema::PowerAction
        | ActionParamSchema::Text
        | ActionParamSchema::FilePath
        | ActionParamSchema::KeyMapping => String::new(),
    }
}

/// Open a file picker dialog for FilePath action parameters.
pub(crate) fn pick_param_file(parent: HWND) -> Option<String> {
    use std::mem;
    unsafe {
        let mut ofn: OPENFILENAMEW = mem::zeroed();
        let mut buf = [0u16; 1024];
        let filter: Vec<u16> = "All Files\0*.*\0".encode_utf16().collect();

        ofn.lStructSize = mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = parent;
        ofn.lpstrFilter = windows::core::PCWSTR::from_raw(filter.as_ptr());
        ofn.lpstrFile = windows::core::PWSTR(buf.as_mut_ptr());
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrTitle = windows::core::w!("Select File");
        ofn.Flags = OFN_FILEMUSTEXIST | OFN_HIDEREADONLY | OFN_PATHMUSTEXIST;

        if GetOpenFileNameW(&mut ofn).as_bool() {
            let len = (0..buf.len()).find(|&i| buf[i] == 0).unwrap_or(0);
            if len > 0 {
                return Some(String::from_utf16_lossy(&buf[..len]));
            }
        }
    }
    None
}

/// Resolve captured key data to a string.
///
/// `WM_BINDING_CAPTURED` packs `LPARAM` as: low byte = modifiers, next byte =
/// key type (0 = keyboard, 1 = mouse button, 2 = wheel), next byte = key value.
pub(crate) fn key_to_string(data: usize) -> String {
    let mods = Modifiers((data & 0xFF) as u8);
    let key_type = (data >> 8) & 0xFF;
    let key_val = (data >> 16) & 0xFF;

    let physical_key = if key_type == 0 {
        Some(PhysicalKey::Keyboard(key_val as u8))
    } else if key_type == 1 {
        Some(PhysicalKey::MouseButton(key_val as u8))
    } else {
        match key_val {
            0 => Some(PhysicalKey::WheelUp),
            1 => Some(PhysicalKey::WheelDown),
            2 => Some(PhysicalKey::WheelLeft),
            3 => Some(PhysicalKey::WheelRight),
            _ => None,
        }
    };

    let kc = KeyCombo {
        modifiers: mods,
        key: physical_key,
    };

    keys_to_string(&kc)
}
