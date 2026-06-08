use std::sync::mpsc;

use crate::action::Action;
use crate::app::{AppHandle, DaemonControl};
use crate::cpu_plan;
use crate::ddc;
use crate::monitor;
use crate::platform;
use crate::power;
use crate::volume;

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

            // Prevent queue buildup: drop identical pending execution messages
            if let ActionMessage::Execute(ref act1) = msg {
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

            match msg {
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
    // Read volume step from config (brightness uses per-action value)
    let vstep = {
        let cfg = handle.config.lock().unwrap();
        cfg.volume_step()
    };
    match action {
        Action::ReplaceKey { keys } => platform::send_keys(keys),
        Action::RunProgram { path } => {
            if let Err(e) = std::process::Command::new(path).spawn() {
                eprintln!("mhd: failed to launch '{}': {e}", path);
            }
        }
        Action::RunPs { command } => run_powershell(command),
        Action::ShowMonitorPanel => {
            monitor::show(handle.theme());
        }
        Action::ShowVolumeMixer => {
            volume::show(handle.theme());
        }
        Action::PowerActions => {
            power::show(handle.theme());
        }
        Action::QuickDraw => {
            crate::overlays::draw::show(handle.theme());
        }
        Action::QuickNote => {
            let cfg = handle.quicknote_config();
            let bb = {
                #[cfg(feature = "blackbox")]
                {
                    handle.blackbox_enabled()
                }
                #[cfg(not(feature = "blackbox"))]
                {
                    false
                }
            };
            crate::overlays::note::show(handle.theme(), cfg.notes_dir.clone(), bb);
        }
        Action::Pomodoro => {
            let bb = {
                #[cfg(feature = "blackbox")]
                {
                    handle.blackbox_enabled()
                }
                #[cfg(not(feature = "blackbox"))]
                {
                    false
                }
            };
            crate::overlays::pomodoro::show(handle.theme(), bb);
        }
        Action::BrightnessUp { value } => {
            if ddc::adjust_brightness(*value as i32).is_ok() {
                if let Ok((new_val, name)) = ddc::get_brightness() {
                    handle.osd.show_brightness(new_val, name);
                }
            }
        }
        Action::BrightnessDown { value } => {
            if ddc::adjust_brightness(-(*value as i32)).is_ok() {
                if let Ok((new_val, name)) = ddc::get_brightness() {
                    handle.osd.show_brightness(new_val, name);
                }
            }
        }
        Action::SetBrightness { relative, value } => {
            let res = if *relative {
                ddc::adjust_brightness(*value)
            } else {
                ddc::set_brightness_absolute(*value as u32)
            };

            match res {
                Ok(_) => {
                    if let Ok((new_val, name)) = ddc::get_brightness() {
                        handle.osd.show_brightness(new_val, name);
                    }
                }
                Err(e) => eprintln!("mhd: brightness error: {e}"),
            }
        }
        Action::Vcp {
            code,
            relative,
            value,
        } => {
            if *relative {
                if let Err(e) = ddc::adjust_vcp_feature(*code, *value) {
                    eprintln!("mhd: VCP error: {e}");
                }
            } else {
                if let Err(e) = ddc::set_vcp_feature(*code, *value as u32) {
                    eprintln!("mhd: VCP error: {e}");
                }
            }
        }
        Action::MediaVolumeUp => platform::send_media_key_n(0xAF, vstep),
        Action::MediaVolumeDown => platform::send_media_key_n(0xAE, vstep),
        Action::MediaMute => platform::send_media_key(0xAD),
        Action::MediaPlayPause => platform::send_media_key(0xB3),
        Action::MediaStop => platform::send_media_key(0xB2),
        Action::MediaLastTrack => platform::send_media_key(0xB1),
        Action::MediaNextTrack => platform::send_media_key(0xB0),
        Action::ToggleTopmost => crate::topmost::toggle(),
        Action::ToggleSuspendOnBlur => crate::suspend::toggle_current(&handle.osd),
        Action::ToggleThrottleOnBlur => crate::throttle::toggle_current(&handle.osd),
        Action::SwitchPowerPlan { target } => {
            let plans = { handle.config.lock().unwrap().power_plans.clone() };
            cpu_plan::switch_plan(target, &plans, &handle.osd);
        }
        Action::ShowCpuPanel => {
            cpu_plan::show_panel(handle.theme());
        }
        Action::ShowLlmModels => {
            crate::overlays::llm_models::show(handle.theme(), handle.llm_proxy_config());
        }
        Action::ToggleLlmProxy => {
            let cfg = handle.llm_proxy_config();
            let on = crate::llm_proxy::toggle(&cfg);
            if !handle.quiet() {
                println!("mhd: llm-proxy {}", if on { "started" } else { "stopped" });
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
