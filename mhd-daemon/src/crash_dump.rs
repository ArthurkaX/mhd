//! Optional crash dump support (`--features debug-dump`).
//!
//! Writes panic logs and Windows minidumps to `%TEMP%` without polluting
//! normal builds. Useful when diagnosing hard crashes in Win32 callbacks.

#![cfg(feature = "debug-dump")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
    MiniDumpWithFullMemoryInfo, MiniDumpWithHandleData, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

pub fn install() {
    // Capture Rust panic backtraces even in release when possible.
    unsafe { std::env::set_var("RUST_BACKTRACE", "full"); }

    std::panic::set_hook(Box::new(|info| {
        let ts = epoch_secs();
        let msg = panic_message(info);
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let bt = std::backtrace::Backtrace::force_capture();

        let text = format!(
            "mhd panic\n timestamp: {ts}\n location: {loc}\n message: {msg}\n\n{bt}\n"
        );
        write_debug_log(&text);
        let _ = write_minidump(None, "panic");
    }));

    unsafe { let _ = SetUnhandledExceptionFilter(Some(unhandled_exception_filter)); }
    write_debug_log("debug-dump installed\n");
}

unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    write_debug_log("unhandled SEH exception\n");
    let _ = write_minidump(Some(info as *mut EXCEPTION_POINTERS), "seh");
    EXCEPTION_EXECUTE_HANDLER
}

fn write_minidump(exception: Option<*mut EXCEPTION_POINTERS>, kind: &str) -> std::io::Result<()> {
    let ts = epoch_secs();
    let path = std::env::temp_dir().join(format!("mhd_{kind}_{ts}.dmp"));
    let file = OpenOptions::new().create(true).write(true).truncate(true).open(&path)?;
    let hfile = HANDLE(file.as_raw_handle() as *mut _);

    let mut ex_info = exception.map(|ptr| MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: unsafe { GetCurrentThreadId() },
        ExceptionPointers: ptr,
        ClientPointers: BOOL(0),
    });

    let dump_type = MiniDumpWithFullMemoryInfo
        | MiniDumpWithHandleData
        | MiniDumpWithThreadInfo
        | MiniDumpWithUnloadedModules;

    let result = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            hfile,
            dump_type,
            ex_info.as_mut().map(|x| x as *mut _ as *const _),
            None,
            None,
        )
    };

    match result {
        Ok(()) => write_debug_log(&format!("wrote minidump: {}\n", path.display())),
        Err(e) => write_debug_log(&format!("MiniDumpWriteDump failed: {e}\n")),
    }
    Ok(())
}

fn write_debug_log(text: &str) {
    let path = std::env::temp_dir().join("mhd_debug_dump.log");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn clear_old_logs() {
    let _ = fs::remove_file(std::env::temp_dir().join("mhd_debug_dump.log"));
}
