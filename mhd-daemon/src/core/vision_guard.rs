//! Shared single-flight guard for vision operations.
//!
//! Both `vision_screenshot` and `vision_snip` use this guard to ensure only
//! one vision operation runs at a time. The guard is held for the entire
//! operation lifecycle: capture, overlay interaction, network request, and
//! clipboard copy.
//!
//! # Usage
//!
//! ```ignore
//! let _guard = VisionGuard::acquire(&osd)?;
//! // ... do work ...
//! // Guard released on drop
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use crate::osd::OsdHandle;

/// Global running flag — prevents concurrent vision operations.
static VISION_RUNNING: AtomicBool = AtomicBool::new(false);

/// RAII guard that holds the vision busy flag.
///
/// When dropped, the flag is cleared so another vision operation can start.
pub struct VisionGuard;

impl VisionGuard {
    /// Try to acquire the vision lock. Returns `None` if another vision
    /// operation is already running, showing the given OSD message.
    pub fn acquire(osd: &OsdHandle) -> Option<Self> {
        if VISION_RUNNING.swap(true, Ordering::Acquire) {
            osd.show_notify("Vision action already running", 2000);
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for VisionGuard {
    fn drop(&mut self) {
        VISION_RUNNING.store(false, Ordering::Release);
    }
}
