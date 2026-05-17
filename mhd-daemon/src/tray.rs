//! Tray module — system tray icon + context menu.
//!
//! Lives in the same process as the daemon core. Communicates with the
//! daemon core via [`AppHandle`] — no named pipe needed.

use std::env;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetCursorPos,
    GetMessageW, InsertMenuW, LoadImageW, MessageBoxW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HICON, IMAGE_ICON, LR_LOADFROMFILE, MB_ICONINFORMATION, MB_OK, MF_BYPOSITION, MF_GRAYED,
    MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::app::AppHandle;

const WM_TRAYICON: u32 = WM_USER + 1;

const CMD_STATUS: usize = 1;
const CMD_EDIT_CONFIG: usize = 2;
const CMD_RELOAD: usize = 3;
const CMD_ABOUT: usize = 4;
const CMD_QUIT: usize = 5;

// ── State ──────────────────────────────────────────────────────────────

struct TrayState {
    nid: NOTIFYICONDATAW,
    app: AppHandle,
}

// Leaked raw pointer — safe because:
// - Set once in WM_CREATE, never replaced.
// - Freed in WM_DESTROY.
// - WM_RBUTTONUP and WM_COMMAND only read from it while the window lives.
static STATE: AtomicPtr<TrayState> = AtomicPtr::new(ptr::null_mut());

unsafe fn state_ref<'a>() -> Option<&'a TrayState> {
    unsafe { STATE.load(Ordering::SeqCst).as_ref() }
}

// ── Icon loading ───────────────────────────────────────────────────────

fn to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

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

        let about: Vec<u16> = "About\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            3,
            MF_BYPOSITION | MF_STRING,
            CMD_ABOUT,
            PCWSTR::from_raw(about.as_ptr()),
        );

        let quit: Vec<u16> = "Quit mhd\0".encode_utf16().collect();
        let _ = InsertMenuW(
            menu,
            4,
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

// ── Config editing ─────────────────────────────────────────────────────

fn open_config_in_editor(app: &AppHandle) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;

    let config_path = &app.config_path;
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let wide_path: Vec<u16> = config_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR::null(),
            PCWSTR(wide_path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOW,
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
            // Extract TrayState pointer from CreateWindowExW's lpParam.
            // In WM_CREATE, lparam points at CREATESTRUCT which has
            // lpCreateParams as its first field (offset 0).
            #[repr(C)]
            struct CreateStruct {
                lp_create_params: *mut TrayState,
            }
            let cs = unsafe { &*(lparam.0 as *const CreateStruct) };
            let state_ptr = cs.lp_create_params;

            // Safety: we haven't stored it yet, no concurrent access.
            let state = unsafe { &*state_ptr };

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
                // Store nid back into the leaked struct
                let ptr = STATE.load(Ordering::SeqCst);
                if !ptr.is_null() {
                    (*ptr).nid = nid;
                    let _ = Shell_NotifyIconW(NIM_ADD, &(*ptr).nid as *const _ as *mut _);
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
            if let Some(state) = unsafe { state_ref() } {
                match cmd {
                    CMD_EDIT_CONFIG => open_config_in_editor(&state.app),
                    CMD_RELOAD => {
                        if let Err(e) = state.app.reload_config() {
                            eprintln!("mhd: reload error: {e}");
                        }
                    }
                    CMD_ABOUT => {
                        let title = to_utf16("mhd\0");
                        let text = to_utf16("Mouse & Hotkey Daemon for Windows\n\nLightweight, single-binary, DDC/CI support.\0");
                        unsafe {
                            let _ = MessageBoxW(HWND::default(), PCWSTR::from_raw(text.as_ptr()), PCWSTR::from_raw(title.as_ptr()), MB_OK | MB_ICONINFORMATION);
                        }
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
            let state_ptr = STATE.swap(ptr::null_mut(), Ordering::SeqCst);
            if !state_ptr.is_null() {
                let state = unsafe { Box::from_raw(state_ptr) };
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &state.nid as *const _ as *mut _);
                }
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── Entry point ────────────────────────────────────────────────────────

/// Run the tray UI in the current thread. Blocks until the window is destroyed.
///
/// The `app` handle gives the tray direct access to the daemon core.
pub fn run(app: AppHandle) {
    let class: Vec<u16> = "mhdTrayClass\0".encode_utf16().collect();
    let title: Vec<u16> = "mhd-tray\0".encode_utf16().collect();

    // Check if another tray window already exists
    unsafe {
        if let Ok(h) = FindWindowW(
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
        )
            && h != HWND::default() {
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

    // Create the leaked state before the window so we can pass it via lpParam
    let tray_state = Box::into_raw(Box::new(TrayState {
        nid: NOTIFYICONDATAW::default(),
        app,
    }));
    STATE.store(tray_state, Ordering::SeqCst);

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
            Some(tray_state as *const _ as *mut _),
        )
    };

    let Ok(hwnd) = hwnd else {
        // Clean up leaked state
        let ptr = STATE.swap(ptr::null_mut(), Ordering::SeqCst);
        if !ptr.is_null() {
            let _ = unsafe { Box::from_raw(ptr) };
        }
        return;
    };
    if hwnd == HWND::default() {
        let ptr = STATE.swap(ptr::null_mut(), Ordering::SeqCst);
        if !ptr.is_null() {
            let _ = unsafe { Box::from_raw(ptr) };
        }
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
