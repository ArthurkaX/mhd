use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PeekMessageW, TranslateMessage, DispatchMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_REMOVE,
    WM_QUIT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
    HHOOK, WH_KEYBOARD_LL, WH_MOUSE_LL,
    KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, LLKHF_INJECTED,
};

use crate::action::Action;
use crate::config::AppConfig;
use crate::trigger::{get_pressed_modifiers, is_modifier_vk, PhysicalKey, Trigger};
use crate::worker::{ActionMessage, ActionSender};

/// Global hook state, accessible from hook callbacks.
struct HookState {
    config: Arc<Mutex<AppConfig>>,
    tx: ActionSender,
    quiet: bool,
    /// Set of keyboard VKs whose key-down was swallowed; their key-up should also be swallowed.
    swallowed_keys: Mutex<HashSet<u32>>,
    /// Set of mouse XButton numbers whose button-down was swallowed; their button-up should also be swallowed.
    swallowed_mouse: Mutex<HashSet<u8>>,
}

/// Wrapper to make HHOOK Send+Sync safe.
#[derive(Debug)]
struct SendHook(HHOOK);
unsafe impl Send for SendHook {}
unsafe impl Sync for SendHook {}

/// Global hook state (Mutex-protected for thread safety with LazyLock).
type HookStateBox = Option<Box<HookState>>;
static HOOK_STATE: LazyLock<Mutex<HookStateBox>> = LazyLock::new(|| Mutex::new(None));
/// Global keyboard hook handle.
static KB_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));
/// Global mouse hook handle.
static MOUSE_HOOK: LazyLock<Mutex<Option<SendHook>>> = LazyLock::new(|| Mutex::new(None));

pub fn run(
    config: AppConfig,
    tx: ActionSender,
    quiet: bool,
) -> Result<(), String> {
    let config = Arc::new(Mutex::new(config));
    let state = Box::new(HookState {
        config: config.clone(),
        tx,
        quiet,
        swallowed_keys: Mutex::new(HashSet::new()),
        swallowed_mouse: Mutex::new(HashSet::new()),
    });

    // Store state globally
    {
        let mut guard = HOOK_STATE.lock().unwrap();
        *guard = Some(state);
    }

    // Install keyboard hook
    let kb_hook = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
    };
    match kb_hook {
        Ok(h) => {
            let mut guard = KB_HOOK.lock().unwrap();
            *guard = Some(SendHook(h));
        }
        Err(e) => {
            let mut guard = HOOK_STATE.lock().unwrap();
            *guard = None;
            return Err(format!("failed to install keyboard hook: {e}"));
        }
    }

    // Install mouse hook
    let mouse_hook = unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)
    };
    match mouse_hook {
        Ok(h) => {
            let mut guard = MOUSE_HOOK.lock().unwrap();
            *guard = Some(SendHook(h));
        }
        Err(e) => {
            // Unhook keyboard hook
            {
                let mut guard = KB_HOOK.lock().unwrap();
                if let Some(h) = guard.take() {
                    let _ = unsafe { UnhookWindowsHookEx(h.0) };
                }
            }
            let mut guard = HOOK_STATE.lock().unwrap();
            *guard = None;
            return Err(format!("failed to install mouse hook: {e}"));
        }
    }

    if !quiet {
        println!("mhd: listening");
    }

    // Message loop
    let mut msg = MSG::default();
    loop {
        // Process all pending messages
        loop {
            let has_msg = unsafe {
                PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE)
            };
            if !has_msg.as_bool() {
                break;
            }
            if msg.message == WM_QUIT as u32 {
                cleanup();
                return Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Wait for next message (blocking)
        let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if !ret.as_bool() {
            // WM_QUIT
            break;
        }

        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    cleanup();
    Ok(())
}

fn cleanup() {
    // Unhook hooks
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
    // Free the state
    {
        let mut guard = HOOK_STATE.lock().unwrap();
        *guard = None;
    }
}

/// Keyboard low-level hook callback.
#[allow(unused_unsafe)]
unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let guard = HOOK_STATE.lock().unwrap();
        let state = match guard.as_ref() {
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
        let is_key_down = wparam == WM_KEYDOWN as u32 || wparam == WM_SYSKEYDOWN as u32;
        let is_key_up = wparam == WM_KEYUP as u32 || wparam == WM_SYSKEYUP as u32;

        if is_key_down && !is_modifier_vk(vk) {
            let modifiers = get_pressed_modifiers();
            let trigger = Trigger {
                modifiers,
                key: PhysicalKey::Keyboard(vk as u8),
            };

            // Look up the binding and determine action type, then drop the lock before acting
            let match_result = {
                let config = state.config.lock().unwrap();
                config.lookup_trigger(&trigger).map(|b| (b.action.clone(), b.trigger_name.clone()))
            };

            if let Some((action, _trigger_name)) = match_result {
                match &action {
                    Action::SwitchScheme { target_scheme } => {
                        let target = target_scheme.clone();
                        let mut config = state.config.lock().unwrap();
                        if config.switch_scheme(&target) {
                            if !state.quiet {
                                println!("mhd: switched to scheme: {}", config.active_scheme());
                            }
                        }
                    }
                    action => {
                        let _ = state.tx.send(ActionMessage::Execute(action.clone()));
                    }
                }
                state.swallowed_keys.lock().unwrap().insert(vk);
                return LRESULT(1); // Swallow the event
            }
        } else if is_key_up {
            if state.swallowed_keys.lock().unwrap().take(&vk).is_some() {
                return LRESULT(1); // Swallow the key-up too
            }
        }
    }

    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}

/// Mouse low-level hook callback.
#[allow(unused_unsafe)]
unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let guard = HOOK_STATE.lock().unwrap();
        let state = match guard.as_ref() {
            Some(s) => s,
            None => return unsafe { CallNextHookEx(None, n_code, w_param, l_param) },
        };
        let ms_struct = unsafe { &*(l_param.0 as *const MSLLHOOKSTRUCT) };
        let msg_type = w_param.0 as u32;

        if msg_type == WM_XBUTTONDOWN as u32 {
            let xbutton = (ms_struct.mouseData >> 16) as u8;

            if xbutton == 1 || xbutton == 2 {
                let modifiers = get_pressed_modifiers();
                let trigger = Trigger {
                    modifiers,
                    key: PhysicalKey::MouseButton(xbutton),
                };

                let match_result = {
                    let config = state.config.lock().unwrap();
                    config.lookup_trigger(&trigger).map(|b| (b.action.clone(), b.trigger_name.clone()))
                };

                if let Some((action, _trigger_name)) = match_result {
                    match &action {
                        Action::SwitchScheme { target_scheme } => {
                            let target = target_scheme.clone();
                            let mut config = state.config.lock().unwrap();
                            if config.switch_scheme(&target) {
                                if !state.quiet {
                                    println!("mhd: switched to scheme: {}", config.active_scheme());
                                }
                            }
                        }
                        action => {
                            let _ = state.tx.send(ActionMessage::Execute(action.clone()));
                        }
                    }
                    state.swallowed_mouse.lock().unwrap().insert(xbutton);
                    return LRESULT(1); // Swallow the event
                }
            }
        } else if msg_type == WM_XBUTTONUP as u32 {
            let xbutton = (ms_struct.mouseData >> 16) as u8;
            if xbutton == 1 || xbutton == 2 {
                if state.swallowed_mouse.lock().unwrap().take(&xbutton).is_some() {
                    return LRESULT(1); // Swallow the button-up too
                }
            }
        }
    }

    unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
}
