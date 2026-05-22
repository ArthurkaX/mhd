//! Win32 clipboard helper for transcribe output.

use windows::Win32::Foundation::{HWND, HANDLE};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Write text to the system clipboard (CF_UNICODETEXT = 13).
///
/// Returns `Ok(())` on success.
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    // Convert to UTF-16 null-terminated
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_size = wide.len() * 2;

    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return Err("OpenClipboard failed".into());
        }
        let _ = EmptyClipboard();

        let h = GlobalAlloc(GMEM_MOVEABLE, byte_size)
            .map_err(|_| "GlobalAlloc failed".to_string())?;

        let ptr = GlobalLock(h) as *mut u16;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalLock returned null".into());
        }

        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(h);

        // SetClipboardData takes HANDLE(HGLOBAL.0)
        let handle = HANDLE(h.0);
        if SetClipboardData(13u32, handle).is_err() {
            let _ = CloseClipboard();
            return Err("SetClipboardData failed".into());
        }

        // After SetClipboardData succeeds, we must NOT free h — the system owns it.
        let _ = CloseClipboard();
    }

    Ok(())
}

/// Send Ctrl+V paste via `SendInput`.
pub fn send_paste() -> Result<(), String> {
    unsafe {
        let vk_v = VK_V;
        let sc_v = MapVirtualKeyW(vk_v.0 as u32, MAPVK_VK_TO_VSC) as u16;

        // Ctrl down
        let ctrl_down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYEVENTF_EXTENDEDKEY,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // V down
        let v_down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk_v,
                    wScan: sc_v,
                    dwFlags: KEYEVENTF_SCANCODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // V up
        let v_up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk_v,
                    wScan: sc_v,
                    dwFlags: KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // Ctrl up
        let ctrl_up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let all = [ctrl_down, v_down, v_up, ctrl_up];
        let sent = SendInput(&all, std::mem::size_of::<INPUT>() as i32);
        if sent == 0 {
            return Err("SendInput for paste returned 0".into());
        }
    }
    Ok(())
}
