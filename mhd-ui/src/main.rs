//! mhd-tray — system tray UI for the mhd daemon.

#![windows_subsystem = "windows"]

use std::env;
use std::io;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, CREATE_NO_WINDOW, STARTF_USESHOWWINDOW,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    FindWindowW, GetCursorPos, GetMessageW, InsertMenuW, LoadImageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HICON,
    IMAGE_ICON, LR_LOADFROMFILE,
    MF_BYPOSITION, MF_GRAYED, MF_STRING,
    MSG, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_RBUTTONUP, WM_USER,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

const PIPE_NAME: &str = "\\\\.\\pipe\\mhd_ipc_pipe";
const WM_TRAYICON: u32 = WM_USER + 1;
const BUF_SIZE: usize = 256;

const CMD_STATUS: usize = 1;
const CMD_EDIT_CONFIG: usize = 2;
const CMD_RELOAD: usize = 3;
const CMD_RESTART: usize = 4;
const CMD_QUIT: usize = 5;

// ── IPC ──────────────────────────────────────────────────────────────

fn send_ipc_command(cmd: &str) -> io::Result<String> {
    let wide_name: Vec<u16> = PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = match unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide_name.as_ptr()),
            0x80000000u32 | 0x40000000u32, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            HANDLE::default(),
        )
    } {
        Ok(h) if h != HANDLE::default() => h,
        _ => return Err(io::Error::new(io::ErrorKind::NotConnected, "daemon not running")),
    };

    let mut written = 0u32;
    unsafe {
        let _ = WriteFile(handle, Some(cmd.as_bytes()), Some(&mut written), None);
        let _ = FlushFileBuffers(handle);
    }

    let mut buf = vec![0u8; BUF_SIZE];
    let mut bytes_read = 0u32;
    unsafe { let _ = ReadFile(handle, Some(&mut buf), Some(&mut bytes_read), None); }
    unsafe { let _ = CloseHandle(handle); }

    Ok(String::from_utf8_lossy(&buf[..bytes_read as usize]).to_string())
}

fn is_daemon_running() -> bool {
    send_ipc_command("status").map(|r| r.trim() == "running").unwrap_or(false)
}

// ── Daemon process ───────────────────────────────────────────────────

fn find_daemon_exe() -> PathBuf {
    if let Ok(exe) = env::current_exe() {
        let parent = exe.parent().unwrap_or(std::path::Path::new("."));
        let candidate = parent.join("mhd.exe");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("mhd.exe")
}

fn start_daemon() -> bool {
    let path = find_daemon_exe();
    let cmdline = format!("\"{}\" --quiet", path.display());
    let wide_cmdline: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_SHOW.0 as u16;

    let mut pi = PROCESS_INFORMATION::default();

    let ok = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            windows::core::PWSTR::from_raw(wide_cmdline.as_ptr() as *mut _),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    };

    if ok.is_err() {
        return false;
    }
    unsafe {
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    true
}

// ── Tray icon ────────────────────────────────────────────────────────

fn load_tray_icon() -> HICON {
    let icon_path = find_daemon_exe()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("mHD_32.png");
    let wide_icon: Vec<u16> = icon_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = match unsafe {
        LoadImageW(
            None,
            PCWSTR::from_raw(wide_icon.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE,
        )
    } {
        Ok(h) => HICON(h.0),
        Err(_) => HICON::default(),
    };

    handle
}

fn update_tray(nid: &mut NOTIFYICONDATAW, running: bool) {
    let tip = if running {
        "mhd — running\0"
    } else {
        "mhd — stopped\0"
    };
    let wide_tip: Vec<u16> = tip.encode_utf16().collect();
    let mut tip_arr = [0u16; 128];
    let len = wide_tip.len().min(127);
    tip_arr[..len].copy_from_slice(&wide_tip[..len]);
    nid.szTip = tip_arr;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;

    unsafe { let _ = Shell_NotifyIconW(NIM_MODIFY, nid as *const _ as *mut _); }
}

fn show_menu(hwnd: HWND, _nid: &NOTIFYICONDATAW) {
    unsafe {
        let menu = CreatePopupMenu().unwrap();
        let running = is_daemon_running();

        let status = if running {
            "Status: running\0"
        } else {
            "Status: stopped\0"
        };
        let ws: Vec<u16> = status.encode_utf16().collect();

        let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_STRING | MF_GRAYED, CMD_STATUS, PCWSTR::from_raw(ws.as_ptr()));

        let edit: Vec<u16> = "Edit Config\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 1, MF_BYPOSITION | MF_STRING, CMD_EDIT_CONFIG, PCWSTR::from_raw(edit.as_ptr()));

        let reload: Vec<u16> = "Reload Config\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 2, MF_BYPOSITION | MF_STRING, CMD_RELOAD, PCWSTR::from_raw(reload.as_ptr()));

        let restart: Vec<u16> = "Restart Daemon\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 3, MF_BYPOSITION | MF_STRING, CMD_RESTART, PCWSTR::from_raw(restart.as_ptr()));

        let quit: Vec<u16> = "Quit mhd\0".encode_utf16().collect();
        let _ = InsertMenuW(menu, 4, MF_BYPOSITION | MF_STRING, CMD_QUIT, PCWSTR::from_raw(quit.as_ptr()));

        let _ = SetForegroundWindow(hwnd);

        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);

        let _ = TrackPopupMenu(menu, TPM_BOTTOMALIGN | TPM_LEFTALIGN, pt.x, pt.y, 0, hwnd, None);
    }
}

// ── Config editor ────────────────────────────────────────────────────

fn resolve_config_path() -> PathBuf {
    if let Ok(custom) = env::var("MHD_CONFIG") {
        return PathBuf::from(custom);
    }
    let mut path = home_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push(".config");
    path.push("mhd");
    path.push("config.toml");
    path
}

fn home_dir() -> Option<PathBuf> {
    env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .map(PathBuf::from)
        .ok()
}

fn open_config_in_editor() {
    let config_path = resolve_config_path();
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let wide_path: Vec<u16> = config_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            HWND::default(),
            PCWSTR::null(),
            PCWSTR(wide_path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOW,
        );
    }
}

// ── Window procedure ─────────────────────────────────────────────────

struct State {
    nid: NOTIFYICONDATAW,
    daemon_running: bool,
}

// Leak state to pass it to wnd_proc without static mut references.
static STATE: AtomicPtr<State> = AtomicPtr::new(ptr::null_mut());

unsafe fn state_ref<'a>() -> Option<&'a State> {
    STATE.load(Ordering::SeqCst).as_ref()
}

unsafe fn state_mut<'a>() -> Option<&'a mut State> {
    STATE.load(Ordering::SeqCst).as_mut()
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let daemon_running = start_daemon();

            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAYICON;
            nid.hIcon = load_tray_icon();

            let tip = if daemon_running { "mhd — running\0" } else { "mhd — stopped\0" };
            let wt: Vec<u16> = tip.encode_utf16().collect();
            let mut ta = [0u16; 128];
            let len = wt.len().min(127);
            ta[..len].copy_from_slice(&wt[..len]);
            nid.szTip = ta;

            unsafe { let _ = Shell_NotifyIconW(NIM_ADD, &nid as *const _ as *mut _); }

            let state = Box::into_raw(Box::new(State { nid, daemon_running }));
            STATE.store(state, Ordering::SeqCst);
            LRESULT(0)
        }

        WM_TRAYICON => {
            if lparam.0 == WM_RBUTTONUP as isize {
                if let Some(state) = unsafe { state_ref() } {
                    show_menu(hwnd, &state.nid);
                }
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let cmd = wparam.0 as usize;
            match cmd {
                CMD_EDIT_CONFIG => open_config_in_editor(),
                CMD_RELOAD => { let _ = send_ipc_command("reload"); }
                CMD_RESTART => {
                    let _ = send_ipc_command("shutdown");
                    std::thread::sleep(Duration::from_millis(500));
                    if let Some(state) = unsafe { state_mut() } {
                        state.daemon_running = start_daemon();
                        update_tray(&mut state.nid, state.daemon_running);
                    }
                }
                CMD_QUIT => {
                    let _ = send_ipc_command("shutdown");
                    unsafe { PostQuitMessage(0); }
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            let state_ptr = STATE.swap(ptr::null_mut(), Ordering::SeqCst);
            if !state_ptr.is_null() {
                let state = unsafe { Box::from_raw(state_ptr) };
                unsafe { let _ = Shell_NotifyIconW(NIM_DELETE, &state.nid as *const _ as *mut _); }
            }
            unsafe { PostQuitMessage(0); }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── Entry point ──────────────────────────────────────────────────────

fn main() {
    let class: Vec<u16> = "mhdTrayClass\0".encode_utf16().collect();
    let title: Vec<u16> = "mhd-tray\0".encode_utf16().collect();

    // Check already running
    unsafe {
        if let Ok(h) = FindWindowW(PCWSTR::from_raw(class.as_ptr()), PCWSTR::from_raw(title.as_ptr())) {
            if h != HWND::default() {
                return;
            }
        }
    }

    let hinst = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::null()).unwrap_or_default()
    };

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

    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT,
            HWND::default(), None, hinst, None,
        )
    };

    let Ok(hwnd) = hwnd else { return };
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
