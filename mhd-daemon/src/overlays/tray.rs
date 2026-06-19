//! Tray module — system tray icon + context menu.
//!
//! Lives in the same process as the daemon core. Communicates with the
//! daemon core via [`AppHandle`] — no named pipe needed.

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, HICON, IMAGE_ICON, InsertMenuW,
    LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadImageW, MF_BYPOSITION, MF_CHECKED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TrackPopupMenu, TranslateMessage, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

use crate::app::{AppHandle, DaemonControl};
use crate::cpu_plan;
use crate::draw;
use crate::monitor;
use crate::note;
use crate::volume;

const WM_TRAYICON: u32 = WM_USER + 1;

const CMD_TOGGLE_SUSPEND: usize = 1;
#[cfg(feature = "blackbox")]
const CMD_BLACKBOX_TOGGLE: usize = 2;
const CMD_EDIT_CONFIG: usize = 9;
const CMD_VOLUME: usize = 3;
const CMD_MONITOR: usize = 4;
const CMD_NOTE: usize = 5;
const CMD_DRAW: usize = 6;
const CMD_CPU_PANEL: usize = 10;
const CMD_LLM_MODELS: usize = 11;
const CMD_PROXY_TRACE: usize = 12;
const CMD_LLM_PROXY_TOGGLE: usize = 13;
const CMD_KEYCAST_TOGGLE: usize = 14;
const CMD_BREATHE: usize = 15;
const CMD_POWER_PLAN_BASE: usize = 100;
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
        const IDI_MHD: u32 = 1;
        if let Ok(h) = LoadImageW(
            hinst,
            PCWSTR(IDI_MHD as *const u16),
            IMAGE_ICON,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::IMAGE_FLAGS(0),
        ) {
            return HICON(h.0);
        }

        // Fallback to mhd.ico next to the exe (copied by build.rs)
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let icon_path = exe_dir.join("mhd.ico");
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
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
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

        let _state = match state_ref() {
            Some(s) => s,
            None => return,
        };

        // Running position counter so items can be added/reordered freely.
        let mut pos: u32 = 0;
        let mut item = |menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
                        flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS,
                        id: usize,
                        text: &str| {
            let wt: Vec<u16> = format!("{text}\0").encode_utf16().collect();
            let _ = InsertMenuW(menu, pos, flags, id, PCWSTR::from_raw(wt.as_ptr()));
            pos += 1;
        };

        // ── Top section: master toggle + blackbox ──────────────────

        let suspended = crate::hook::is_suspended();
        let mhd_flags = if !suspended {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, mhd_flags, CMD_TOGGLE_SUSPEND, "Key Shortcuts on/off");

        #[cfg(feature = "blackbox")]
        {
            let bb_enabled = _state.app.blackbox_enabled();
            let bb_flags = if bb_enabled {
                MF_BYPOSITION | MF_STRING | MF_CHECKED
            } else {
                MF_BYPOSITION | MF_STRING
            };
            item(menu, bb_flags, CMD_BLACKBOX_TOGGLE, "Blackbox on/off");
        }

        // LLM proxy on/off (checked when the embedded proxy is running).
        let llm_running = crate::llm_proxy::is_running();
        let llm_flags = if llm_running {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, llm_flags, CMD_LLM_PROXY_TOGGLE, "LLM Proxy on/off");

        item(menu, MF_BYPOSITION | MF_SEPARATOR, 0, "");

        // ── Control group ──────────────────────────────────────────

        item(menu, MF_BYPOSITION | MF_STRING, CMD_VOLUME, "Volume");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_MONITOR, "Monitor");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_CPU_PANEL, "CPU Power");
        item(
            menu,
            MF_BYPOSITION | MF_STRING,
            CMD_LLM_MODELS,
            "LLM Models",
        );
        item(
            menu,
            MF_BYPOSITION | MF_STRING,
            CMD_PROXY_TRACE,
            "Proxy Trace",
        );

        item(menu, MF_BYPOSITION | MF_SEPARATOR, 0, "");

        // ── Actions group ──────────────────────────────────────────

        item(menu, MF_BYPOSITION | MF_STRING, CMD_NOTE, "Note");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_DRAW, "Draw");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_BREATHE, "Breathe");
        let keycast_flags = if crate::keycast::is_enabled() {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, keycast_flags, CMD_KEYCAST_TOGGLE, "KeyCast on/off");

        item(menu, MF_BYPOSITION | MF_SEPARATOR, 0, "");

        // ── Power Plan submenu ─────────────────────────────────────
        let schemes = crate::cpu_plan::enumerate_schemes();
        let active_guid = crate::cpu_plan::get_active_scheme_guid();
        let pp_menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        for (i, (guid, name)) in schemes.iter().enumerate() {
            let flags = if *guid == active_guid {
                MF_BYPOSITION | MF_STRING | MF_CHECKED
            } else {
                MF_BYPOSITION | MF_STRING
            };
            let item_text: Vec<u16> = format!("{}\0", name).encode_utf16().collect();
            let _ = InsertMenuW(
                pp_menu,
                i as u32,
                flags,
                CMD_POWER_PLAN_BASE + i,
                PCWSTR::from_raw(item_text.as_ptr()),
            );
        }
        item(
            menu,
            MF_BYPOSITION | MF_POPUP,
            pp_menu.0 as usize,
            "Power Plan",
        );

        // ── Bottom section ─────────────────────────────────────────

        item(menu, MF_BYPOSITION | MF_STRING, CMD_EDIT_CONFIG, "Settings");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_ABOUT, "About");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_QUIT, "Exit");

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
                        draw::show(state.app.theme(), state.app.draw_dir());
                    }
                    CMD_BREATHE => {
                        let bb = {
                            #[cfg(feature = "blackbox")]
                            {
                                state.app.blackbox_enabled()
                            }
                            #[cfg(not(feature = "blackbox"))]
                            {
                                false
                            }
                        };
                        let config = crate::overlays::breathe::auto_preset();
                        crate::overlays::breathe::show(state.app.theme(), config, bb);
                    }
                    CMD_KEYCAST_TOGGLE => {
                        let on =
                            crate::keycast::toggle(state.app.theme(), state.app.keycast_config());
                        state
                            .app
                            .osd
                            .show_notify(format!("KeyCast {}", if on { "on" } else { "off" }), 900);
                    }
                    CMD_EDIT_CONFIG => {
                        crate::config::editor::show_config_editor(state.app.clone());
                    }
                    CMD_CPU_PANEL => {
                        cpu_plan::show_panel(state.app.theme());
                    }
                    CMD_LLM_MODELS => {
                        crate::overlays::llm_models::show(
                            state.app.theme(),
                            state.app.llm_proxy_config(),
                        );
                    }
                    CMD_PROXY_TRACE => {
                        crate::overlays::proxy_trace::show(&state.app.theme());
                    }
                    CMD_LLM_PROXY_TOGGLE => {
                        let cfg = state.app.llm_proxy_config();
                        crate::llm_proxy::toggle(&cfg);
                    }
                    cmd if (CMD_POWER_PLAN_BASE..CMD_POWER_PLAN_BASE + 20).contains(&cmd) => {
                        let index = cmd - CMD_POWER_PLAN_BASE;
                        cpu_plan::switch_plan_by_index(index, &state.app.osd);
                    }
                    CMD_ABOUT => {
                        crate::about::show_about(state.app.theme());
                    }
                    CMD_QUIT => {
                        state.app.shutdown();
                        // Sleep a tiny bit to let the hook thread process WM_QUIT,
                        // then post our own quit to exit the tray message loop.
                        std::thread::sleep(Duration::from_millis(200));
                        unsafe { PostQuitMessage(0) };
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
            unsafe { PostQuitMessage(0) };
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
        ) && h != HWND::default()
        {
            return;
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

    let _ = STATE.set(Box::new(TrayState { app }));

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
