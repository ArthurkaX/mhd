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
    HICON, IMAGE_ICON, LR_LOADFROMFILE, MF_BYPOSITION, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_RBUTTONUP, WM_USER,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::app::{AppHandle, DaemonControl};
use crate::monitor;
use crate::volume;
use crate::draw;
use crate::note;

const WM_TRAYICON: u32 = WM_USER + 1;

const CMD_TOGGLE_SUSPEND: usize = 1;
#[cfg(feature = "blackbox")]
const CMD_BLACKBOX_TOGGLE: usize = 2;
const CMD_EDIT_CONFIG: usize = 9;
const CMD_VOLUME: usize = 3;
const CMD_MONITOR: usize = 4;
const CMD_NOTE: usize = 5;
const CMD_DRAW: usize = 6;
const CMD_ABOUT: usize = 7;
const CMD_QUIT: usize = 8;

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

        // ── Top section: master toggle + blackbox ──────────────────

        let suspended = crate::hook::is_suspended();
        let mhd_flags = if !suspended {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        let mhd_text: Vec<u16> = "mhd on/off\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 0, mhd_flags, CMD_TOGGLE_SUSPEND, PCWSTR::from_raw(mhd_text.as_ptr()));

        #[cfg(feature = "blackbox")]
        {
            let bb_enabled = state.app.blackbox_enabled();
            let bb_flags = if bb_enabled {
                MF_BYPOSITION | MF_STRING | MF_CHECKED
            } else {
                MF_BYPOSITION | MF_STRING
            };
            let bb_text: Vec<u16> = "Blackbox on/off\0".encode_utf16().collect();
            let _ = InsertMenuW(menu, 1, bb_flags, CMD_BLACKBOX_TOGGLE, PCWSTR::from_raw(bb_text.as_ptr()));
        }

        // Separator
        let _ = InsertMenuW(menu, 2, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

        // ── Control group ──────────────────────────────────────────

        let vol_text: Vec<u16> = "Volume\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 3, MF_BYPOSITION | MF_STRING, CMD_VOLUME, PCWSTR::from_raw(vol_text.as_ptr()));

        let mon_text: Vec<u16> = "Monitor\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 4, MF_BYPOSITION | MF_STRING, CMD_MONITOR, PCWSTR::from_raw(mon_text.as_ptr()));

        // Separator
        let _ = InsertMenuW(menu, 5, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

        // ── Actions group ──────────────────────────────────────────

        let note_text: Vec<u16> = "Note\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 6, MF_BYPOSITION | MF_STRING, CMD_NOTE, PCWSTR::from_raw(note_text.as_ptr()));

        let draw_text: Vec<u16> = "Draw\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 7, MF_BYPOSITION | MF_STRING, CMD_DRAW, PCWSTR::from_raw(draw_text.as_ptr()));

        // Separator
        let _ = InsertMenuW(menu, 8, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

        // ── Bottom section ─────────────────────────────────────────

        let settings_text: Vec<u16> = "Settings\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 9, MF_BYPOSITION | MF_STRING, CMD_EDIT_CONFIG, PCWSTR::from_raw(settings_text.as_ptr()));

        let about_text: Vec<u16> = "About\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 10, MF_BYPOSITION | MF_STRING, CMD_ABOUT, PCWSTR::from_raw(about_text.as_ptr()));

        let exit_text: Vec<u16> = "Exit\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 11, MF_BYPOSITION | MF_STRING, CMD_QUIT, PCWSTR::from_raw(exit_text.as_ptr()));

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
                    CMD_TOGGLE_SUSPEND => {
                        crate::hook::toggle_suspended();
                    }
                    #[cfg(feature = "blackbox")]
                    CMD_BLACKBOX_TOGGLE => {
                        state.app.toggle_blackbox();
                    }
                    CMD_VOLUME => {
                        volume::show(state.app.theme());
                    }
                    CMD_MONITOR => {
                        monitor::show(state.app.theme());
                    }
                    CMD_NOTE => {
                        let cfg = state.app.quicknote_config();
                        #[cfg(feature = "blackbox")]
                        let bb = state.app.blackbox_enabled();
                        #[cfg(not(feature = "blackbox"))]
                        let bb = false;
                        note::show(state.app.theme(), cfg.notes_dir.clone(), bb);
                    }
                    CMD_DRAW => {
                        draw::show(state.app.theme());
                    }
                    CMD_EDIT_CONFIG => {
                        crate::config_editor::show_config_editor(state.app.clone());
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
