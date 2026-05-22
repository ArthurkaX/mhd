use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::action::Action;
use crate::app::{AppHandle, DaemonControl};
use crate::platform::get_pressed_modifiers;
use crate::trigger::{PhysicalKey, Trigger, is_modifier_vk};
use crate::worker::{ActionMessage, ActionSender};

pub const WM_BINDING_CAPTURED: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 100;

/// Global hook state, accessible from hook callbacks.
struct HookState {
    handle: AppHandle,
    tx: ActionSender,
    /// Lock‑free set of VK codes (0‑255) whose key‑up should be swallowed.
    /// Index = VK; `true` = swallowed.
    swallowed_keys: Box<[AtomicBool; 256]>,
    /// Lock‑free flags for mouse buttons whose up should be swallowed.
    /// Bit 0 = XBUTTON1, bit 1 = XBUTTON2, bit 2 = MBUTTON.
    swallowed_mouse: AtomicU8,
}

/// Wrapper to make HHOOK Send+Sync safe.
#[derive(Debug)]
struct SendHook(HHOOK);
unsafe impl Send for SendHook {}
unsafe impl Sync for SendHook {}

/// Global hook state — set once before the message loop, never changed.
/// Lock-free read in the hot path avoids the low-level hook timeout issue.
static HOOK_STATE: OnceLock<&'static HookState> = OnceLock::new();

/// Global keyboard hook handle.
static KB_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));
/// Global mouse hook handle.
static MOUSE_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));

/// Window currently recording bindings.
static RECORDING_WINDOW: LazyLock<Mutex<Option<crate::app::SendHwnd>>> =
    LazyLock::new(|| Mutex::new(None));

/// Set the window that should receive captured bindings.
pub fn set_recording_window(hwnd: Option<HWND>) {
    let mut guard = RECORDING_WINDOW.lock().unwrap();
    *guard = hwnd.map(crate::app::SendHwnd);
}

/// Entry point used by `app.rs`. Accepts a shared config so reloads are visible.
pub fn run_with_config(handle: AppHandle, tx: ActionSender) -> Result<(), String> {
    run_impl(handle, tx)
}

fn run_impl(handle: AppHandle, tx: ActionSender) -> Result<(), String> {
    let quiet = handle.quiet;

    // Install keyboard hook first (no state needed yet — callbacks will
    // skip via OnceLock until state is set below).
    let kb_hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) };
    let kb_hook = match kb_hook {
        Ok(h) => h,
        Err(e) => return Err(format!("failed to install keyboard hook: {e}")),
    };

    // Install mouse hook
    let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) };
    let mouse_hook = match mouse_hook {
        Ok(h) => h,
        Err(e) => {
            let _ = unsafe { UnhookWindowsHookEx(kb_hook) };
            return Err(format!("failed to install mouse hook: {e}"));
        }
    };

    // Store hook handles
    {
        let mut guard = KB_HOOK.lock().unwrap();
        *guard = Some(SendHook(kb_hook));
    }
    {
        let mut guard = MOUSE_HOOK.lock().unwrap();
        *guard = Some(SendHook(mouse_hook));
    }

    // Now that hooks are installed, set the global state.
    // Leak the Box so we have a &'static reference — this is safe because
    // the state lives until cleanup (process exit or WM_QUIT).
    let state = Box::new(HookState {
        handle,
        tx,
        swallowed_keys: Box::new([const { AtomicBool::new(false) }; 256]),
        swallowed_mouse: AtomicU8::new(0),
    });
    let state_ref: &'static HookState = Box::leak(state);
    let _ = HOOK_STATE.set(state_ref);

    if !quiet {
        println!("mhd: listening");
    }

    // Blocking message loop. Low-level hooks are delivered through this thread's
    // message queue; IPC shutdown wakes it with PostThreadMessageW(WM_QUIT).
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if ret.0 <= 0 || msg.message == WM_QUIT {
            cleanup();
            return Ok(());
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn cleanup() {
    // Unhook hooks — after this, no more callbacks can fire.
    {
        let mut guard = KB_HOOK.lock().unwrap();
        if let Some(h) = guard.take() {
            let _ = unsafe { UnhookWindowsHookEx(h.0) };
        }
    }
    {
        let mut guard = MOUSE_HOOK.lock().unwrap();
        if let Some(h) = guard.take() {
            let _ = unsafe { UnhookWindowsHookEx(h.0) };
        }
    }
    // OnceLock state is intentionally leaked — the process is exiting or
    // about to exit, so cleanup is not necessary. No more callbacks can
    // fire because the hooks are uninstalled.
}

pub(crate) fn signal_tray_to_quit() {
    let class: Vec<u16> = format!("{}\0", crate::tray::TRAY_CLASS).encode_utf16().collect();
    let title: Vec<u16> = format!("{}\0", crate::tray::TRAY_TITLE).encode_utf16().collect();
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
        if let Ok(hwnd) = FindWindowW(
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
        ) {
            if hwnd != HWND::default() {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn dispatch_trigger(state: &HookState, trigger: Trigger) -> bool {
    // Check if recording
    if let Some(target_hwnd) = *RECORDING_WINDOW.lock().unwrap() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        let mut data = trigger.modifiers.0 as usize;
        match trigger.key {
            PhysicalKey::Keyboard(vk) => {
                data |= 0 << 8; // type 0 = keyboard
                data |= (vk as usize) << 16;
                state.swallowed_keys[vk as usize].store(true, Ordering::Release);
            }
            PhysicalKey::MouseButton(btn) => {
                data |= 1 << 8; // type 1 = mouse
                data |= (btn as usize) << 16;
                let bit = mouse_btn_bit(btn);
                state.swallowed_mouse.fetch_or(bit, Ordering::Release);
            }
            PhysicalKey::WheelUp
            | PhysicalKey::WheelDown
            | PhysicalKey::WheelLeft
            | PhysicalKey::WheelRight => {
                data |= 2 << 8; // type 2 = wheel
                let dir: u8 = match trigger.key {
                    PhysicalKey::WheelUp => 0,
                    PhysicalKey::WheelDown => 1,
                    PhysicalKey::WheelLeft => 2,
                    PhysicalKey::WheelRight => 3,
                    _ => unreachable!(),
                };
                data |= (dir as usize) << 16;
            }
        }
        unsafe {
            let _ = PostMessageW(
                target_hwnd.0,
                WM_BINDING_CAPTURED,
                WPARAM(0),
                LPARAM(data as isize),
            );
        }
        return true;
    }

    // Look up the binding
    let match_result = state.handle.lookup_trigger(&trigger);

    if let Some(action) = match_result {
        match action {
            Action::SwitchScheme { target_scheme } => {
                let _ = state.tx.send(ActionMessage::SwitchScheme(target_scheme));
            }
            Action::Quit => {
                let _ = state.tx.send(ActionMessage::Quit);
            }
            action => {
                let _ = state.tx.send(ActionMessage::Execute(action));
            }
        }

        // Mark as swallowed (wheel events are one-shot, no separate up)
        match trigger.key {
            PhysicalKey::Keyboard(vk) => {
                state.swallowed_keys[vk as usize].store(true, Ordering::Release);
            }
            PhysicalKey::MouseButton(btn) => {
                let bit = mouse_btn_bit(btn);
                state.swallowed_mouse.fetch_or(bit, Ordering::Release);
            }
            PhysicalKey::WheelUp | PhysicalKey::WheelDown | PhysicalKey::WheelLeft | PhysicalKey::WheelRight => {
                // No separate up — already swallowed by returning LRESULT(1)
            }
        }
        return true;
    }

    false
}

/// Keyboard low-level hook callback.
#[allow(unused_unsafe)]
unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        // Lock-free access — OnceLock::get() is a simple pointer read.
        let state = match HOOK_STATE.get() {
            Some(s) => s,
            None => return unsafe { CallNextHookEx(None, n_code, w_param, l_param) },
        };
        let kb_struct = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
        let vk = kb_struct.vkCode;
        let flags = kb_struct.flags;

        // Skip injected events to avoid infinite loops with our own SendInput
        if unsafe { (flags & LLKHF_INJECTED).0 != 0 } {
            return unsafe { CallNextHookEx(None, n_code, w_param, l_param) };
        }

        let wparam = w_param.0 as u32;
        let is_key_down = wparam == WM_KEYDOWN || wparam == WM_SYSKEYDOWN;
        let is_key_up = wparam == WM_KEYUP || wparam == WM_SYSKEYUP;

        // Report all non‑modifier key‑downs to blackbox (if active)
        #[cfg(feature = "blackbox")]
        if is_key_down && !is_modifier_vk(vk) {
            use crate::blackbox::{self, BlackboxEvent, InputKind};

            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            blackbox::send_event(BlackboxEvent::Input {
                kind: InputKind::Keyboard,
                ts,
            });
        }

        if is_key_down && !is_modifier_vk(vk) {
            let modifiers = get_pressed_modifiers();
            let trigger = Trigger {
                modifiers,
                key: PhysicalKey::Keyboard(vk as u8),
            };

            if dispatch_trigger(state, trigger) {
                return LRESULT(1);
            }
        } else if is_key_up && state.swallowed_keys[vk as usize].swap(false, Ordering::Acquire) {
            return LRESULT(1); // Swallow the key-up too
        }
    }

    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

/// Return the wheel delta from MSLLHOOKSTRUCT.mouseData.
/// For WM_MOUSEWHEEL / WM_MOUSEHWHEEL the high-order word contains the delta.
fn wheel_delta(mouse_data: u32) -> i32 {
    ((mouse_data >> 16) & 0xFFFF) as i16 as i32
}

/// Mouse low-level hook callback.
#[allow(unused_unsafe)]
unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        // Lock-free access — OnceLock::get() is a simple pointer read.
        let state = match HOOK_STATE.get() {
            Some(s) => s,
            None => return unsafe { CallNextHookEx(None, n_code, w_param, l_param) },
        };
        let ms_struct = unsafe { &*(l_param.0 as *const MSLLHOOKSTRUCT) };
        let msg_type = w_param.0 as u32;

        // Helper to send counted action to blackbox.
        #[cfg(feature = "blackbox")]
        let bb_input = |kind: crate::blackbox::InputKind| {
            use crate::blackbox::{self, BlackboxEvent};

            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            blackbox::send_event(BlackboxEvent::Input { kind, ts });
        };

        match msg_type {
            WM_XBUTTONDOWN => {
                let xbutton = (ms_struct.mouseData >> 16) as u8;
                if xbutton == 1 || xbutton == 2 {
                    #[cfg(feature = "blackbox")]
                    bb_input(crate::blackbox::InputKind::MouseButton);
                    let modifiers = get_pressed_modifiers();
                    let trigger = Trigger {
                        modifiers,
                        key: PhysicalKey::MouseButton(xbutton),
                    };
                    if dispatch_trigger(state, trigger) {
                        return LRESULT(1);
                    }
                }
            }
            WM_XBUTTONUP => {
                let xbutton = (ms_struct.mouseData >> 16) as u8;
                if xbutton == 1 || xbutton == 2 {
                    let bit = mouse_btn_bit(xbutton);
                    if state.swallowed_mouse.fetch_and(!bit, Ordering::Acquire) & bit != 0 {
                        return LRESULT(1);
                    }
                }
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                #[cfg(feature = "blackbox")]
                bb_input(crate::blackbox::InputKind::MouseButton);
            }
            WM_MBUTTONDOWN => {
                #[cfg(feature = "blackbox")]
                bb_input(crate::blackbox::InputKind::MouseButton);
                // Middle button
                let modifiers = get_pressed_modifiers();
                let trigger = Trigger {
                    modifiers,
                    key: PhysicalKey::MouseButton(3),
                };
                if dispatch_trigger(state, trigger) {
                    return LRESULT(1);
                }
            }
            WM_MBUTTONUP => {
                let bit = mouse_btn_bit(3);
                if state.swallowed_mouse.fetch_and(!bit, Ordering::Acquire) & bit != 0 {
                    return LRESULT(1);
                }
            }
            WM_MOUSEWHEEL => {
                #[cfg(feature = "blackbox")]
                bb_input(crate::blackbox::InputKind::Wheel);
                // Delta is in MSLLHOOKSTRUCT.mouseData, NOT in wParam (for LL hooks)
                let delta = wheel_delta(ms_struct.mouseData);
                let key = if delta > 0 { PhysicalKey::WheelUp } else { PhysicalKey::WheelDown };
                let modifiers = get_pressed_modifiers();
                let trigger = Trigger { modifiers, key };
                if dispatch_trigger(state, trigger) {
                    return LRESULT(1);
                }
            }
            WM_MOUSEHWHEEL => {
                #[cfg(feature = "blackbox")]
                bb_input(crate::blackbox::InputKind::Wheel);
                let delta = wheel_delta(ms_struct.mouseData);
                let key = if delta > 0 { PhysicalKey::WheelRight } else { PhysicalKey::WheelLeft };
                let modifiers = get_pressed_modifiers();
                let trigger = Trigger { modifiers, key };
                if dispatch_trigger(state, trigger) {
                    return LRESULT(1);
                }
            }
            _ => {}
        }
    }

    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

/// Convert mouse button number to bit flag for `swallowed_mouse` AtomicU8.
/// 1 = XBUTTON1 → bit 0, 2 = XBUTTON2 → bit 1, 3 = MBUTTON → bit 2.
fn mouse_btn_bit(btn: u8) -> u8 {
    1u8.checked_shl(btn.saturating_sub(1) as u32).unwrap_or(0)
}


