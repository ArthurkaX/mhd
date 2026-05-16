//! Named pipe IPC server — listens for external commands.
//!
//! Commands are forwarded to [`AppHandle`]. The named pipe is kept for
//! headless-mode external control and legacy compatibility.

use std::thread;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Pipes::{CreateNamedPipeW, DisconnectNamedPipe, NAMED_PIPE_MODE};

use crate::app::AppHandle;

const PIPE_NAME: &str = "\\\\.\\pipe\\mhd_ipc_pipe";
const BUF_SIZE: usize = 256;

const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
const PIPE_TYPE_MESSAGE: u32 = 0x00000004;
const PIPE_READMODE_MESSAGE: u32 = 0x00000002;
const PIPE_WAIT: u32 = 0x00000000;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run the IPC server on a background thread.
///
/// The `AppHandle` is used directly — no polling or busy loops.
pub fn run_ipc_server(app: AppHandle) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let wide_name = to_wide(PIPE_NAME);

        loop {
            if !app.status() {
                break;
            }

            let pipe = {
                let h = unsafe {
                    CreateNamedPipeW(
                        PCWSTR::from_raw(wide_name.as_ptr()),
                        FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX),
                        NAMED_PIPE_MODE(PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT),
                        1,
                        BUF_SIZE as u32,
                        BUF_SIZE as u32,
                        0,
                        None,
                    )
                };
                if h == INVALID_HANDLE_VALUE {
                    if !app.quiet {
                        eprintln!("mhd: ipc pipe creation failed");
                    }
                    thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
                h
            };

            // Blocking ReadFile — waits for a client to connect and send data
            let mut buf = vec![0u8; BUF_SIZE];
            let mut bytes_read = 0u32;

            if unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut bytes_read), None) }.is_ok()
                && bytes_read > 0
            {
                let cmd_str = String::from_utf8_lossy(&buf[..bytes_read as usize]);
                let cmd_lower = cmd_str.trim().to_lowercase();

                let response = match cmd_lower.as_str() {
                    "status" => {
                        if app.status() {
                            "running\n"
                        } else {
                            "stopped\n"
                        }
                    }
                    "reload" => {
                        if !app.quiet {
                            println!("mhd: ipc: reload requested");
                        }
                        match app.reload_config() {
                            Ok(()) => "reloaded\n",
                            Err(e) => {
                                eprintln!("mhd: ipc reload error: {e}");
                                "error: reload failed\n"
                            }
                        }
                    }
                    "shutdown" => {
                        if !app.quiet {
                            println!("mhd: ipc: shutdown requested");
                        }
                        app.shutdown();
                        "shutting_down\n"
                    }
                    _ => "unknown_command\n",
                };

                let mut written = 0u32;
                unsafe {
                    let _ = WriteFile(pipe, Some(response.as_bytes()), Some(&mut written), None);
                    let _ = FlushFileBuffers(pipe);
                }
            }

            unsafe {
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
            }
        }
    })
}
