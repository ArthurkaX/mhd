//! Shared constants used across the mhd codebase.
//!
//! Centralises magic numbers to a single location so they are easy to
//! find and change.  Module‑specific constants (layout geometry, etc.)
//! remain in their respective modules.

// ── DDC / VCP feature codes ─────────────────────────────────────────

/// Monitor brightness (continuous, 0–100).
pub const VCP_BRIGHTNESS: u8 = 0x10;
/// Monitor contrast (continuous).
pub const VCP_CONTRAST: u8 = 0x12;
/// Monitor input source (non‑continuous / table).
pub const VCP_INPUT_SOURCE: u8 = 0x60;
/// Monitor audio volume (continuous).
pub const VCP_AUDIO_VOLUME: u8 = 0x62;
/// Monitor speaker volume (alternative).  Rarely used.
#[allow(dead_code)]
pub const VCP_SPEAKER_VOLUME: u8 = 0x64;

// ── Win32 message constants ─────────────────────────────────────────

/// `WM_MOUSELEAVE` — not defined in all Windows SDK versions shipped
/// with older Rust toolchains.  We define it here once.
pub const WM_MOUSELEAVE: u32 = 0x02A3;

// ── Timeouts & intervals ────────────────────────────────────────────

/// Maximum number of DDC/CI retry attempts before giving up.
pub const DDC_MAX_RETRIES: u32 = 3;

/// Base delay (ms) before the first DDC/CI retry; doubles each attempt.
pub const DDC_RETRY_BASE_MS: u64 = 10;

/// Default volume step for `media_volume_up` / `media_volume_down`.
#[allow(dead_code)]
pub const DEFAULT_VOLUME_STEP: u32 = 1;

/// Default brightness step for `brightness_up` / `brightness_down`.
#[allow(dead_code)]
pub const DEFAULT_BRIGHTNESS_STEP: u32 = 5;

// ── DPI configuration ───────────────────────────────────────────────

/// Assumed DPI at which all layout constants are defined.
#[allow(dead_code)]
pub const BASE_DPI: f32 = 96.0;
