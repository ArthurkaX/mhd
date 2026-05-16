use std::sync::mpsc;

use crate::action::Action;
use crate::monitor;
use crate::trigger::{KeyCombo, PhysicalKey};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};

/// Messages sent from hook callbacks to the worker thread.
#[derive(Debug)]
pub enum ActionMessage {
    Execute(Action),
    #[allow(dead_code)]
    SwitchScheme(String),
    #[allow(dead_code)]
    Quit,
}

/// The action worker runs on a dedicated thread and executes actions.
pub struct ActionWorker {
    quiet: bool,
    rx: mpsc::Receiver<ActionMessage>,
}

/// Handle to send actions to the worker.
pub type ActionSender = mpsc::Sender<ActionMessage>;

impl ActionWorker {
    pub fn new(quiet: bool) -> (Self, ActionSender) {
        let (tx, rx) = mpsc::channel();
        let worker = ActionWorker { quiet, rx };
        (worker, tx)
    }

    /// Spawn the worker thread. Returns a JoinHandle.
    pub fn spawn(self) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_loop();
        })
    }

    fn run_loop(self) {
        let mut pending = None;

        loop {
            let msg = if let Some(m) = pending.take() {
                m
            } else {
                match self.rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                }
            };

            let action_to_execute = msg;

            // Prevent queue buildup: drop identical pending execution messages
            if let ActionMessage::Execute(ref act1) = action_to_execute {
                let desc1 = act1.describe();
                loop {
                    match self.rx.try_recv() {
                        Ok(ActionMessage::Execute(act2)) if act2.describe() == desc1 => {
                            // Drop identical action that accumulated while we were busy
                        }
                        Ok(other_msg) => {
                            // Different message, save it for the next iteration
                            pending = Some(other_msg);
                            break;
                        }
                        Err(_) => break, // Channel empty
                    }
                }
            }

            match action_to_execute {
                ActionMessage::Execute(action) => {
                    if !self.quiet {
                        println!("mhd: triggered: {}", action.describe());
                    }
                    execute_action(&action);
                }
                ActionMessage::SwitchScheme(_scheme) => {
                    // Handled in hook callback directly
                }
                ActionMessage::Quit => {
                    // Quit is handled via PostQuitMessage in the hook callback
                    break;
                }
            }
        }
    }
}

fn execute_action(action: &Action) {
    match action {
        Action::ReplaceKey { keys } => send_replace_key(keys),
        Action::RunPs { command } => run_powershell(command),
        Action::SwitchScheme { .. } => {
            // ?????????????? ? ?????? ?????
        }
        Action::SetBrightness { relative, value } => {
            let res = if *relative {
                monitor::adjust_brightness(*value)
            } else {
                monitor::set_brightness_absolute(*value as u32)
            };

            match res {
                Ok(_) => {
                    if let Ok(new_val) = monitor::get_brightness() {
                        crate::ui::show_brightness(new_val);
                    }
                }
                Err(e) => eprintln!("mhd: brightness error: {e}"),
            }
        }
        Action::Vcp { code, relative, value } => {
            if *relative {
                if let Err(e) = monitor::adjust_vcp_feature(*code, *value) {
                    eprintln!("mhd: VCP error: {e}");
                }
            } else {
                if let Err(e) = monitor::set_vcp_feature(*code, *value as u32) {
                    eprintln!("mhd: VCP error: {e}");
                }
            }
        }
        Action::Quit => {
            // ?????????????? ????? PostQuitMessage ?? ????????? ?????? ????
        }
    }
}

fn send_replace_key(keys: &KeyCombo) {
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
            // The modifiers are already being pressed above, we just need to release them below.
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

fn run_powershell(command: &str) {
    let result = std::process::Command::new("powershell")
        .args(["-Command", command])
        .spawn();

    if let Err(e) = result {
        eprintln!("mhd: failed to run powershell: {e}");
    }
}
