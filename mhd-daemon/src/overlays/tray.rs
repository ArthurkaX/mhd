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
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, HICON, IMAGE_ICON, InsertMenuW,
    LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadImageW, MF_BYPOSITION, MF_CHECKED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MSG, PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TrackPopupMenu, TranslateMessage, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_MOUSEMOVE, WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

use llm_proxy::ClientKind;

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
const CMD_CODEX_MODELS: usize = 25;
const CMD_LLM_ACTIVITY: usize = 17;
const CMD_PROXY_TRACE: usize = 12;
const CMD_QUIET_TOGGLE: usize = 19;
const CMD_KEYCAST_TOGGLE: usize = 14;
const CMD_BREATHE: usize = 15;
// Per-client switches. IDs 13 (CMD_LLM_PROXY_TOGGLE), 18
// (CMD_QUOTA_WATCHER_TOGGLE), 21 (CMD_TRIM_CLAUDE_CODE) and 23
// (CMD_TRIM_CODEX) were freed — the trim toggles moved to Settings -> LLM Trim —
// and are deliberately NOT reused for a different meaning.
const CMD_CLIENT_CLAUDE_CODE: usize = 20;
const CMD_CLIENT_CODEX: usize = 22;
const CMD_CLIENT_OPENAI: usize = 24;
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

/// Launch the LLM Monitor (mhd-inspector --monitor) as a separate process.
fn launch_llm_monitor(app: &AppHandle) {
    let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mhd-inspector.exe")))
    else {
        app.osd
            .show_notify("LLM Activity: could not locate mhd-inspector", 2500);
        return;
    };

    if !exe.is_file() {
        app.osd.show_notify(
            format!("LLM Activity unavailable: {} is missing", exe.display()),
            3500,
        );
        return;
    }

    if let Err(error) = std::process::Command::new(&exe).arg("--monitor").spawn() {
        app.osd.show_notify(format!("LLM Activity could not open: {error}"), 3500);
    }
}

pub(crate) fn load_tray_icon() -> HICON {
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

        item(menu, MF_BYPOSITION | MF_SEPARATOR, 0, "");

        // ── Per-client switches ────────────────────────────────
        // One switch per client covers both its route and its background usage
        // polling — mhd does no work for a client the user does not use. The
        // OpenAI switch covers Zed / opencode / pi. Trim toggles live on the
        // Settings -> LLM Trim page, not here.

        let claude_proxy = crate::llm_proxy::client_enabled(ClientKind::ClaudeCode);
        let claude_proxy_flags = if claude_proxy {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(
            menu,
            claude_proxy_flags,
            CMD_CLIENT_CLAUDE_CODE,
            "Claude Code: proxy",
        );

        let codex_proxy = crate::llm_proxy::client_enabled(ClientKind::Codex);
        let codex_proxy_flags = if codex_proxy {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, codex_proxy_flags, CMD_CLIENT_CODEX, "Codex: proxy");

        let openai_proxy = crate::llm_proxy::client_enabled(ClientKind::OpenAi);
        let openai_proxy_flags = if openai_proxy {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, openai_proxy_flags, CMD_CLIENT_OPENAI, "OpenAI: proxy");

        item(menu, MF_BYPOSITION | MF_SEPARATOR, 0, "");

        // ── Control group ──────────────────────────────────────────

        item(menu, MF_BYPOSITION | MF_STRING, CMD_VOLUME, "Volume");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_MONITOR, "Monitor");
        item(menu, MF_BYPOSITION | MF_STRING, CMD_CPU_PANEL, "CPU Power");
        item(
            menu,
            MF_BYPOSITION | MF_STRING,
            CMD_LLM_MODELS,
            "Claude Code Models",
        );
        item(
            menu,
            MF_BYPOSITION | MF_STRING,
            CMD_CODEX_MODELS,
            "Codex Models",
        );
        item(
            menu,
            MF_BYPOSITION | MF_STRING,
            CMD_LLM_ACTIVITY,
            "LLM Activity",
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
        let quiet_flags = if crate::overlays::quiet::is_active() {
            MF_BYPOSITION | MF_STRING | MF_CHECKED
        } else {
            MF_BYPOSITION | MF_STRING
        };
        item(menu, quiet_flags, CMD_QUIET_TOGGLE, "Quiet Mode on/off");

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
            if STATE.get().is_some() {
                let mut nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    uCallbackMessage: WM_TRAYICON,
                    hIcon: load_tray_icon(),
                    ..Default::default()
                };

                nid.szTip = tray_tip_text();

                unsafe {
                    let _ = Shell_NotifyIconW(NIM_ADD, &nid as *const _ as *mut _);
                }
            }

            LRESULT(0)
        }

        WM_TRAYICON => {
            if lparam.0 == WM_RBUTTONUP as isize {
                show_menu(hwnd);
            } else if lparam.0 == WM_MOUSEMOVE as isize {
                update_tray_tip(hwnd);
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
                        note::show(
                            state.app.theme(),
                            note::NoteSink::File(cfg.notes_dir.clone()),
                            bb,
                        );
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
                    CMD_QUIET_TOGGLE => {
                        crate::overlays::quiet::toggle(&state.app.quiet_config(), &state.app.osd);
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
                    CMD_CODEX_MODELS => {
                        crate::overlays::llm_models::show_codex(
                            state.app.theme(),
                            state.app.llm_proxy_config(),
                        );
                    }
                    CMD_LLM_ACTIVITY => {
                        launch_llm_monitor(&state.app);
                    }
                    CMD_PROXY_TRACE => {
                        crate::overlays::proxy_trace::show(&state.app.theme());
                    }
                    CMD_CLIENT_CLAUDE_CODE => {
                        let on = !crate::llm_proxy::client_enabled(ClientKind::ClaudeCode);
                        state.app.set_client_enabled(ClientKind::ClaudeCode, on);
                    }
                    CMD_CLIENT_CODEX => {
                        let on = !crate::llm_proxy::client_enabled(ClientKind::Codex);
                        state.app.set_client_enabled(ClientKind::Codex, on);
                    }
                    CMD_CLIENT_OPENAI => {
                        let on = !crate::llm_proxy::client_enabled(ClientKind::OpenAi);
                        state.app.set_client_enabled(ClientKind::OpenAi, on);
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
                let nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid as *const _ as *mut _);
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Build the fixed-size UTF-16 buffer required by `NOTIFYICONDATAW::szTip`.
fn tray_tip_text() -> [u16; 128] {
    let text = state_ref()
        .map(|state| crate::overlays::quota_pace::tray_tooltip(&state.app))
        .unwrap_or_default();
    let encoded: Vec<u16> = text.encode_utf16().collect();
    let mut out = [0u16; 128];
    let len = encoded.len().min(out.len() - 1);
    out[..len].copy_from_slice(&encoded[..len]);
    out
}

/// Refresh the system tooltip just before Windows displays it on hover.
fn update_tray_tip(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    nid.szTip = tray_tip_text();
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid as *const _ as *mut _);
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
