use std::sync::mpsc;

use crate::action::Action;
use crate::app::{AppHandle, DaemonControl};
use crate::monitor;
use crate::platform;

/// Messages sent from hook callbacks to the worker thread.
#[derive(Debug)]
pub enum ActionMessage {
    Execute(Action),
    SwitchScheme(String),
    Quit,
}

/// The action worker runs on a dedicated thread and executes actions.
pub struct ActionWorker {
    handle: AppHandle,
    rx: mpsc::Receiver<ActionMessage>,
}

/// Handle to send actions to the worker.
pub type ActionSender = mpsc::Sender<ActionMessage>;

impl ActionWorker {
    pub fn new(handle: AppHandle) -> (Self, ActionSender) {
        let (tx, rx) = mpsc::channel();
        let worker = ActionWorker { handle, rx };
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
                    if !self.handle.quiet() {
                        println!("mhd: triggered: {}", action.describe());
                    }
                    execute_action(&action, &self.handle);
                }
                ActionMessage::SwitchScheme(scheme) => {
                    if self.handle.switch_scheme(&scheme) && !self.handle.quiet() {
                        println!("mhd: switched to scheme: {}", self.handle.active_scheme());
                    }
                }
                ActionMessage::Quit => {
                    if !self.handle.quiet() {
                        println!("mhd: quit");
                    }
                    self.handle.shutdown();
                    crate::hook::signal_tray_to_quit();
                    break;
                }
            }
        }
    }
}

fn execute_action(action: &Action, handle: &AppHandle) {
    match action {
        Action::ReplaceKey { keys } => platform::send_keys(keys),
        Action::RunPs { command } => run_powershell(command),
        Action::SetBrightness { relative, value } => {
            let res = if *relative {
                monitor::adjust_brightness(*value)
            } else {
                monitor::set_brightness_absolute(*value as u32)
            };

            match res {
                Ok(_) => {
                    if let Ok((new_val, name)) = monitor::get_brightness() {
                        handle.osd.show_brightness(new_val, name);
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
        // SwitchScheme and Quit are dispatched via dedicated ActionMessage
        // variants, never wrapped in ActionMessage::Execute.
        Action::SwitchScheme { .. } | Action::Quit => {}
    }
}

fn run_powershell(command: &str) {
    let result = std::process::Command::new("powershell")
        .args(["-Command", command])
        .spawn();

    if let Err(e) = result {
        eprintln!("mhd: failed to run powershell: {e}");
    }
}
