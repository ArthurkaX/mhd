//! Tray module — system tray icon + context menu.
//!
//! Lives in the same process as the daemon core. Communicates with the
//! daemon core via [`AppHandle`] — no named pipe needed.

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;



use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetCursorPos,
    GetMessageW, InsertMenuW, LoadImageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HICON, IMAGE_ICON, LR_LOADFROMFILE, MF_BYPOSITION, MF_GRAYED, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_RBUTTONUP, WM_USER,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
#[cfg(feature = "blackbox")]
use windows::Win32::UI::WindowsAndMessaging::MF_CHECKED;

use crate::app::{AppHandle, DaemonControl};
use crate::monitor_panel;
use crate::volume_mixer;
use crate::power;
use crate::quickdraw;
use crate::quicknote;

const WM_TRAYICON: u32 = WM_USER + 1;

const CMD_STATUS: usize = 1;
const CMD_EDIT_CONFIG: usize = 2;
const CMD_RELOAD: usize = 3;
const CMD_VOLUME_MIXER: usize = 6;
const CMD_MONITOR_PANEL: usize = 7;
const CMD_POWER: usize = 8;
const CMD_QUICK_DRAW: usize = 9;
const CMD_QUICK_NOTE: usize = 11;
#[cfg(feature = "blackbox")]
const CMD_BLACKBOX_TOGGLE: usize = 10;
const CMD_ABOUT: usize = 4;
const CMD_QUIT: usize = 5;

// ── State ──────────────────────────────────────────────────────────────

struct TrayState {
    app: AppHandle,
}

unsafe impl Send for TrayState {}
unsafe impl Sync for TrayState {}

static STATE: OnceLock<Box<TrayState>> = OnceLock::new();

fn state_ref() -> Option<&'static TrayState> {
    STATE.get().map(|b| b.as_ref())
}

// ── Icon loading ───────────────────────────────────────────────────────

fn load_tray_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();

        // Try to load embedded icon (IDI_MHD = 1)
        if let Ok(h) = LoadImageW(
            hinst,
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::IMAGE_FLAGS(0),
        ) {
            return HICON(h.0);
        }

        // Fallback to mHD_32.png next to the exe
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let icon_path = exe_dir.join("mHD_32.png");
        let wide_icon: Vec<u16> = icon_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        match LoadImageW(
            None,
            PCWSTR::from_raw(wide_icon.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE,
        ) {
            Ok(h) => HICON(h.0),
            Err(_) => HICON::default(),
        }
    }
}

// ── Context menu ───────────────────────────────────────────────────────

fn show_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };

        let state = match state_ref() {
            Some(s) => s,
            None => return,
        };

        let running = state.app.status();
        let status_text = if running {
            "Status: running\0"
        } else {
            "Status: stopped\0"
        };
        let ws: Vec<u16> = status_text.encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            0,
            MF_BYPOSITION | MF_STRING | MF_GRAYED,
            CMD_STATUS,
            PCWSTR::from_raw(ws.as_ptr()),
        );

        let edit: Vec<u16> = "Edit Config\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            1,
            MF_BYPOSITION | MF_STRING,
            CMD_EDIT_CONFIG,
            PCWSTR::from_raw(edit.as_ptr()),
        );

        let reload: Vec<u16> = "Reload Config\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            2,
            MF_BYPOSITION | MF_STRING,
            CMD_RELOAD,
            PCWSTR::from_raw(reload.as_ptr()),
        );

        let monitor_panel: Vec<u16> = "Monitor Control\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            3,
            MF_BYPOSITION | MF_STRING,
            CMD_MONITOR_PANEL,
            PCWSTR::from_raw(monitor_panel.as_ptr()),
        );

        let volume_mixer: Vec<u16> = "Volume Mixer\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            4,
            MF_BYPOSITION | MF_STRING,
            CMD_VOLUME_MIXER,
            PCWSTR::from_raw(volume_mixer.as_ptr()),
        );

        let power_ctrl: Vec<u16> = "Power Control\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            5,
            MF_BYPOSITION | MF_STRING,
            CMD_POWER,
            PCWSTR::from_raw(power_ctrl.as_ptr()),
        );

        let quick_draw: Vec<u16> = "Quick Draw\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            6,
            MF_BYPOSITION | MF_STRING,
            CMD_QUICK_DRAW,
            PCWSTR::from_raw(quick_draw.as_ptr()),
        );

        let quick_note: Vec<u16> = "Quick Note\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            7,
            MF_BYPOSITION | MF_STRING,
            CMD_QUICK_NOTE,
            PCWSTR::from_raw(quick_note.as_ptr()),
        );

        #[cfg(feature = "blackbox")]
        {
            let bb_text = if state.app.blackbox_enabled() { "Blackbox: on\0" } else { "Blackbox: off\0" };
            let bb_label: Vec<u16> = bb_text.encode_utf16().collect();
            let bb_flags = if state.app.blackbox_enabled() {
                MF_BYPOSITION | MF_STRING | MF_CHECKED
            } else {
                MF_BYPOSITION | MF_STRING
            };
            let _ = InsertMenuW(
                menu,
                8,
                bb_flags,
                CMD_BLACKBOX_TOGGLE,
                PCWSTR::from_raw(bb_label.as_ptr()),
            );
        }

        let about: Vec<u16> = "About\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            if cfg!(feature = "blackbox") { 9 } else { 8 },
            MF_BYPOSITION | MF_STRING,
            CMD_ABOUT,
            PCWSTR::from_raw(about.as_ptr()),
        );

        let quit: Vec<u16> = "Quit mhd\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            if cfg!(feature = "blackbox") { 10 } else { 9 },
            MF_BYPOSITION | MF_STRING,
            CMD_QUIT,
            PCWSTR::from_raw(quit.as_ptr()),
        );

        let _ = SetForegroundWindow(hwnd);

        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);

        let _ = TrackPopupMenu(
            menu,
            TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
    }
}

// ── Window procedure ───────────────────────────────────────────────────

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            if let Some(state) = STATE.get() {
                let mut nid = NOTIFYICONDATAW::default();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd;
                nid.uID = 1;
                nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                nid.uCallbackMessage = WM_TRAYICON;
                nid.hIcon = load_tray_icon();

                let tip = if state.app.status() {
                    "mhd — running\0"
                } else {
                    "mhd — stopped\0"
                };
                let wt: Vec<u16> = tip.encode_utf16().collect();
                let mut ta = [0u16; 128];
                let len = wt.len().min(127);
                ta[..len].copy_from_slice(&wt[..len]);
                nid.szTip = ta;

                unsafe {
                    let _ = Shell_NotifyIconW(NIM_ADD, &nid as *const _ as *mut _);
                }
            }

            LRESULT(0)
        }

        WM_TRAYICON => {
            if lparam.0 == WM_RBUTTONUP as isize {
                show_menu(hwnd);
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let cmd = wparam.0;
            if let Some(state) = state_ref() {
                match cmd {
                    CMD_EDIT_CONFIG => {
                        crate::config_editor::show_config_editor(state.app.clone());
                    }
                    CMD_RELOAD => {
                        if let Err(e) = state.app.reload_config() {
                            eprintln!("mhd: reload error: {e}");
                        }
                    }
                    CMD_MONITOR_PANEL => {
                        monitor_panel::show(state.app.theme());
                    }
                    CMD_VOLUME_MIXER => {
                        volume_mixer::show(state.app.theme());
                    }
                    CMD_POWER => {
                        power::show(state.app.theme());
                    }
                    CMD_QUICK_DRAW => {
                        quickdraw::show(state.app.theme());
                    }
                    CMD_QUICK_NOTE => {
                        let cfg = state.app.quicknote_config();
                        #[cfg(feature = "blackbox")]
                        let bb = state.app.blackbox_enabled();
                        #[cfg(not(feature = "blackbox"))]
                        let bb = false;
                        quicknote::show(state.app.theme(), cfg.notes_dir.clone(), bb);
                    }
                    #[cfg(feature = "blackbox")]
                    CMD_BLACKBOX_TOGGLE => {
                        state.app.toggle_blackbox();
                    }
                    CMD_ABOUT => {
                        crate::about::show_about(state.app.theme());
                    }
                    CMD_QUIT => {
                        state.app.shutdown();
                        // Sleep a tiny bit to let the hook thread process WM_QUIT,
                        // then post our own quit to exit the tray message loop.
                        std::thread::sleep(Duration::from_millis(200));
                        unsafe {
                            PostQuitMessage(0)
                        };
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            unsafe {
                let mut nid = NOTIFYICONDATAW::default();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd;
                nid.uID = 1;
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid as *const _ as *mut _);
            }
            unsafe {
                PostQuitMessage(0)
            };
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── Window class / title constants — shared with hook.rs ───────────────

/// Window class name for the tray icon window.
pub const TRAY_CLASS: &str = "mhdTrayClass";
/// Window title for the tray icon window.
pub const TRAY_TITLE: &str = "mhd-tray";

// ── Entry point ────────────────────────────────────────────────────────

pub fn run(app: AppHandle) {
    let class: Vec<u16> = format!("{}\0", TRAY_CLASS).encode_utf16().collect();
    let title: Vec<u16> = format!("{}\0", TRAY_TITLE).encode_utf16().collect();

    // Check if another tray window already exists
    unsafe {
        if let Ok(h) = FindWindowW(
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
        ) {
            if h != HWND::default() {
                return;
            }
        }
    }

    let hinst = unsafe { GetModuleHandleW(PCWSTR::null()).unwrap_or_default() };

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst.into(),
        lpszClassName: PCWSTR::from_raw(class.as_ptr()),
        ..Default::default()
    };

    if unsafe { RegisterClassW(&wc) } == 0 {
        return;
    }

    let _ = STATE.set(Box::new(TrayState {
        app,
    }));

    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            HWND::default(),
            None,
            hinst,
            None,
        )
    };

    let Ok(hwnd) = hwnd else {
        return;
    };
    if hwnd == HWND::default() {
        return;
    }

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
        if !ret.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
