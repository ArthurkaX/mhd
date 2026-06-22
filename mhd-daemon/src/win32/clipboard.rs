//! Windows clipboard — reusable Unicode text writer.
//!
//! Provides [`set_text`] which writes UTF-16 text to the clipboard as
//! `CF_UNICODETEXT` with retry logic for clipboard contention.

use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// Errors that can occur during clipboard operations.
#[derive(Debug)]
pub enum ClipboardError {
    OpenFailed,
    EmptyFailed,
    AllocFailed,
    LockFailed,
    SetFailed,
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed => write!(f, "could not open clipboard"),
            Self::EmptyFailed => write!(f, "could not empty clipboard"),
            Self::AllocFailed => write!(f, "could not allocate clipboard memory"),
            Self::LockFailed => write!(f, "could not lock clipboard memory"),
            Self::SetFailed => write!(f, "could not set clipboard data"),
        }
    }
}

impl std::error::Error for ClipboardError {}

/// Maximum number of retry attempts for `OpenClipboard`.
const MAX_OPEN_RETRIES: u32 = 10;

/// Retry interval between `OpenClipboard` attempts.
const RETRY_INTERVAL_MS: u64 = 10;

// Clipboard format constant.
const CF_UNICODETEXT: u32 = 13;

/// Write a Unicode string to the clipboard (`CF_UNICODETEXT`).
///
/// This function:
/// - Retries `OpenClipboard` briefly because clipboard contention is common.
/// - Allocates movable global memory for the UTF-16 NUL-terminated string.
/// - Transfers ownership to Windows only after `SetClipboardData` succeeds.
pub fn set_text(text: &str) -> Result<(), ClipboardError> {
    // Encode as UTF-16 with NUL terminator
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_count = utf16.len() * 2;

    unsafe {
        // Retry OpenClipboard with backoff
        let mut opened = false;
        for _ in 0..MAX_OPEN_RETRIES {
            if OpenClipboard(HWND::default()).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(RETRY_INTERVAL_MS));
        }
        if !opened {
            return Err(ClipboardError::OpenFailed);
        }

        // Empty the clipboard
        if EmptyClipboard().is_err() {
            let _ = CloseClipboard();
            return Err(ClipboardError::EmptyFailed);
        }

        // Allocate movable global memory
        let hmem = match GlobalAlloc(GMEM_MOVEABLE, byte_count) {
            Ok(h) => h,
            Err(_) => {
                let _ = CloseClipboard();
                return Err(ClipboardError::AllocFailed);
            }
        };

        // Lock and copy data
        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err(ClipboardError::LockFailed);
        }

        std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
        let _ = GlobalUnlock(hmem);

        // Set clipboard data (transfer ownership to Windows)
        let result = SetClipboardData(CF_UNICODETEXT, HANDLE(hmem.0));
        if result.is_err() {
            let _ = CloseClipboard();
            return Err(ClipboardError::SetFailed);
        }

        let _ = CloseClipboard();
    }

    Ok(())
}
