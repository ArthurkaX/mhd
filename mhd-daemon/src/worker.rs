use std::sync::mpsc;

use crate::action::Action;
use crate::app::{AppHandle, DaemonControl};
use crate::monitor;
use crate::platform;
use crate::monitor_panel;
use crate::volume_mixer;

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
            monitor_panel::show(handle.theme());
        }
        Action::ShowVolumeMixer => {
            volume_mixer::show(handle.theme());
        }
        Action::BrightnessUp { value } => {
            if monitor::adjust_brightness(*value as i32).is_ok() {
                if let Ok((new_val, name)) = monitor::get_brightness() {
                    handle.osd.show_brightness(new_val, name);
                }
            }
        }
        Action::BrightnessDown { value } => {
            if monitor::adjust_brightness(-(*value as i32)).is_ok() {
                if let Ok((new_val, name)) = monitor::get_brightness() {
                    handle.osd.show_brightness(new_val, name);
                }
            }
        }
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
        Action::MediaVolumeUp => platform::send_media_key_n(0xAF, vstep),
        Action::MediaVolumeDown => platform::send_media_key_n(0xAE, vstep),
        Action::MediaMute => platform::send_media_key(0xAD),
        Action::MediaPlayPause => platform::send_media_key(0xB3),
        Action::MediaStop => platform::send_media_key(0xB2),
        Action::MediaLastTrack => platform::send_media_key(0xB1),
        Action::MediaNextTrack => platform::send_media_key(0xB0),
        Action::ToggleTopmost => toggle_topmost_pin(),
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

fn toggle_topmost_pin() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongPtrW, GetWindowTextW, SetWindowPos, SetWindowTextW,
        GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, WS_EX_TOPMOST,
    };

    // Use ASCII marker that renders in ANY font on any Windows version
    const PIN_MARKER: &str = " [Pin]";
    // DWM colour for the border — accent blue/green
    const PIN_BORDER_COLOR: u32 = 0x00FFAA44; // ABGR: orange-ish
    const RESET_COLOR: u32 = 0xFFFFFFFE;      // DWMWA_COLOR_DEFAULT / NONE

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() {
            return;
        }

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let is_topmost = (ex_style as u32 & WS_EX_TOPMOST.0) != 0;

        if is_topmost {
            // ── Unpin ────────────────────────────────────────────
            let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

            // Restore original title (strip marker)
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, &mut buf);
            if len > 0 {
                let title = String::from_utf16_lossy(&buf[..len as usize]);
                if let Some(pos) = title.rfind(PIN_MARKER) {
                    let new_title = title[..pos].to_string();
                    let wide: Vec<u16> = new_title.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(wide.as_ptr()));
                }
            }

            // Reset DWM border / caption colour to default
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &RESET_COLOR as *const _ as *const _, 4);
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &RESET_COLOR as *const _ as *const _, 4);
        } else {
            // ── Pin ──────────────────────────────────────────────
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

            // Append marker to title
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, &mut buf);
            if len > 0 {
                let title = String::from_utf16_lossy(&buf[..len as usize]);
                if !title.contains(PIN_MARKER) {
                    let new_title = format!("{}{}", title, PIN_MARKER);
                    let wide: Vec<u16> = new_title.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(wide.as_ptr()));
                }
            }

            // Set DWM border colour (accent tint) — works on Win10 20H1+
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &PIN_BORDER_COLOR as *const _ as *const _, 4);
            // Also tint the caption area slightly
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &PIN_BORDER_COLOR as *const _ as *const _, 4);
        }
    }
}
