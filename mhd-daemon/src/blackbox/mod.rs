#![cfg(feature = "blackbox")]

//! Blackbox — minimal behaviour logger backed by SQLite.
//!
//! Records activity to `~/.config/mhd/blackbox/blackbox.db`:
//! input actions (keyboard/mouse), session boundaries, foreground window
//! changes, and monitoring lifecycle.
//!
//! Config section `[blackbox]`; disabled by default.
//! All DB I/O happens in the blackbox worker thread — never in the
//! low‑level hook hot path.

mod db;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
#[cfg(test)]
use windows::Win32::System::Time::GetTimeZoneInformation;
#[cfg(test)]
use windows::Win32::System::Time::TIME_ZONE_ID_INVALID;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

// Manual FFI — QueryFullProcessImageNameW is not in windows 0.58 crate features.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryFullProcessImageNameW(
        hProcess: HANDLE,
        dwFlags: u32,
        lpExeName: *mut u16,
        lpdwSize: *mut u32,
    ) -> i32;
}

use crate::config::path::home_dir;

use self::db::Db;

// ── Config (parsed by config/mod.rs) ─────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BlackboxConfig {
    pub enabled: bool,
    pub idle_seconds: u64,
}

impl Default for BlackboxConfig {
    fn default() -> Self {
        BlackboxConfig {
            enabled: false,
            idle_seconds: 300,
        }
    }
}

// ── Event types (hook → blackbox worker) ────────────────────────────────

#[derive(Debug, Clone)]
pub enum BlackboxEvent {
    Input { kind: InputKind, ts: u64 },
    /// Quick Note was saved (ts = save time, text = note content).
    QuickNote { ts: u64, text: String },
    /// Custom log event (e.g. pomodoro) with key-value pairs.
    LogCustom { ts: u64, event: String, kv: Vec<(String, String)> },
    Shutdown,
    /// Toggle enabled state from tray.
    ToggleEnabled,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputKind {
    Keyboard,
    MouseButton,
    Wheel,
}

// ── Global sender for hook → blackbox ───────────────────────────────────

static BLACKBOX_TX: LazyLock<Mutex<Option<mpsc::Sender<BlackboxEvent>>>> =
    LazyLock::new(|| Mutex::new(None));

pub fn send_event(event: BlackboxEvent) {
    if let Ok(guard) = BLACKBOX_TX.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(event);
        }
    }
}

/// Current enabled state (used by tray to show on/off).
static BLACKBOX_ENABLED: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
pub fn is_active() -> bool {
    BLACKBOX_TX.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Returns the current enabled/disabled state (for tray).
pub fn is_logging() -> bool {
    BLACKBOX_ENABLED.load(Ordering::Relaxed)
}

// ── Log path helpers ─────────────────────────────────────────────────────

fn blackbox_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("blackbox")
}

fn db_path() -> PathBuf {
    blackbox_dir().join("blackbox.db")
}

/// Timezone bias from UTC in seconds (positive = east of UTC, i.e. local time = utc + bias).
#[cfg(test)]
fn timezone_bias_secs() -> i64 {
    unsafe {
        let mut tzi = std::mem::zeroed();
        let ret = GetTimeZoneInformation(&mut tzi);
        if ret == TIME_ZONE_ID_INVALID {
            0
        } else {
            // Bias is minutes WEST of UTC -> we want seconds EAST
            -(tzi.Bias as i64) * 60
        }
    }
}

/// Convert a UTC epoch second to local epoch second (accounting for DST).
#[cfg(test)]
fn utc_to_local(utc: u64) -> u64 {
    let bias = timezone_bias_secs();
    if bias >= 0 {
        utc + bias as u64
    } else {
        utc.saturating_sub(bias.unsigned_abs())
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Time-of-day string (HH:MM:SS) for a given LOCAL epoch second.
#[cfg(test)]
fn time_str(ts_local: u64) -> String {
    let s = (ts_local % 86400) as i64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

// ── Batch constants (same as old txt-based flush logic) ─────────────────

const FLUSH_EVENTS: u32 = 50;
const FLUSH_SECS: u64 = 20;

// ── Session state machine ────────────────────────────────────────────────

struct SessionState {
    active: bool,
    started_ts: u64,
    last_action_at: u64,
    keyboard_count: u64,
    mouse_count: u64,
    last_window_title: Option<String>,
    /// Last app name (exe without path).
    last_app_name: Option<String>,
}

impl SessionState {
    fn new() -> Self {
        SessionState {
            active: false,
            started_ts: 0,
            last_action_at: 0,
            keyboard_count: 0,
            mouse_count: 0,
            last_window_title: None,
            last_app_name: None,
        }
    }

    fn on_input(&mut self, kind: InputKind, ts: u64) {
        if !self.active {
            self.active = true;
            self.started_ts = ts;
            self.last_action_at = ts;
            self.keyboard_count = 0;
            self.mouse_count = 0;
        } else {
            self.last_action_at = ts;
        }
        match kind {
            InputKind::Keyboard => self.keyboard_count += 1,
            InputKind::MouseButton | InputKind::Wheel => self.mouse_count += 1,
        }
    }

    fn end_session(&mut self, ts: u64, reason: Option<&str>) -> Option<SessionEndData> {
        if !self.active { return None; }
        let duration = ts.saturating_sub(self.started_ts);
        let data = SessionEndData {
            started_ts: self.started_ts,
            duration_sec: duration,
            keyboard: self.keyboard_count,
            mouse: self.mouse_count,
            end_reason: reason.map(|s| s.to_string()),
        };
        self.active = false;
        Some(data)
    }
}

#[allow(dead_code)]
struct SessionEndData {
    started_ts: u64,
    duration_sec: u64,
    keyboard: u64,
    mouse: u64,
    end_reason: Option<String>,
}

// ── Foreground window helpers ────────────────────────────────────────────

fn get_foreground_title() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() { return String::new(); }
        let mut buf = [0u16; 512];
        let len = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buf);
        if len > 0 { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }
    }
}

/// Extract the executable name (without path / extension) of the foreground
/// window's process.
fn get_app_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() { return None; }
        let mut pid = 0u32;
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 { return None; }
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        if handle.is_invalid() { return None; }
        let mut buf = [0u16; 260];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) != 0;
        let _ = CloseHandle(handle);
        if !ok || size == 0 { return None; }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        // Extract filename without extension
        let stem = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(path);
        Some(stem)
    }
}

/// Serialize key-value pairs into a compact payload string.
fn kv_payload(kv: &[(String, String)]) -> String {
    kv.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Worker thread ────────────────────────────────────────────────────────

pub struct BlackboxHandle {
    tx: mpsc::Sender<BlackboxEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl BlackboxHandle {
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(BlackboxEvent::Shutdown);
        if let Some(j) = self.join.take() {
            let _: () = j.join().unwrap_or(());
        }
    }

    /// Toggle logging on/off at runtime (from tray).
    pub fn toggle(&self) {
        let _ = self.tx.send(BlackboxEvent::ToggleEnabled);
    }
}

/// Start blackbox monitoring.
pub fn start(config: BlackboxConfig) -> Result<BlackboxHandle, String> {
    let idle_seconds = config.idle_seconds;

    let (tx, rx) = mpsc::channel::<BlackboxEvent>();
    {
        let mut guard = BLACKBOX_TX.lock().unwrap();
        *guard = Some(tx.clone());
    }

    let join = thread::Builder::new()
        .name("blackbox".into())
        .spawn(move || {
            // Open DB (ensure directory exists)
            if let Err(e) = std::fs::create_dir_all(&blackbox_dir()) {
                eprintln!("mhd: blackbox: cannot create dir: {e}");
                return;
            }
            let db = match Db::open(&db_path()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("mhd: blackbox: {e}");
                    return;
                }
            };

            let mut session = SessionState::new();
            let mut enabled = true;
            BLACKBOX_ENABLED.store(true, Ordering::Relaxed);

            // Batch tracking
            let mut events_since_flush: u32 = 0;
            let mut last_flush_ts: u64 = epoch_secs();

            // Snapshot initial context
            let now = epoch_secs();
            let initial_title = get_foreground_title();
            if !initial_title.is_empty() {
                session.last_window_title = Some(initial_title.clone());
            }
            let initial_app = get_app_name();
            if let Some(ref app) = initial_app {
                session.last_app_name = Some(app.clone());
            }

            // Write daemon_start event with current context
            ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
            if let Err(e) = db.insert_event(now, "daemon_start",
                initial_app.as_deref(),
                if initial_title.is_empty() { None } else { Some(initial_title.as_str()) },
                None,
            ) {
                eprintln!("mhd: blackbox: insert daemon_start: {e}");
            } else {
                events_since_flush += 1;
            }
            check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);

            loop {
                let timeout = Duration::from_secs(if session.active {
                    idle_seconds.min(2)
                } else {
                    2
                });

                let event = match rx.recv_timeout(timeout) {
                    Ok(e) => e,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if enabled && session.active {
                            let now = epoch_secs();
                            if now >= session.last_action_at + idle_seconds {
                                end_session_and_insert(&db, &mut session, now, None,
                                    &mut events_since_flush, &mut last_flush_ts);
                            }
                        }
                        if enabled {
                            check_app_and_title(&db, &mut session,
                                &mut events_since_flush, &mut last_flush_ts);
                        }
                        check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };

                match event {
                    BlackboxEvent::Input { kind, ts } => {
                        if enabled {
                            session.on_input(kind, ts);
                            ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
                            check_app_and_title(&db, &mut session,
                                &mut events_since_flush, &mut last_flush_ts);
                            check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                        }
                    }
                    BlackboxEvent::Shutdown => {
                        let now = epoch_secs();
                        if enabled {
                            end_session_and_insert(&db, &mut session, now, Some("stop"),
                                &mut events_since_flush, &mut last_flush_ts);
                        }
                        let _ = db.insert_event(now, "daemon_stop", None, None, None);
                        // Final flush
                        if events_since_flush > 0 {
                            let _ = db.commit();
                        }
                        break;
                    }
                    BlackboxEvent::QuickNote { ts, text } => {
                        if enabled {
                            ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
                            if let Ok(eid) = db.insert_event(ts, "note",
                                session.last_app_name.as_deref(),
                                session.last_window_title.as_deref(),
                                None,
                            ) {
                                if let Err(e) = db.insert_note(eid, &text) {
                                    eprintln!("mhd: blackbox: insert note: {e}");
                                }
                                events_since_flush += 1;
                            }
                            check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                        }
                    }
                    BlackboxEvent::LogCustom { ts, event, kv } => {
                        if enabled {
                            ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
                            let payload = if kv.is_empty() { None } else { Some(kv_payload(&kv)) };
                            if let Err(e) = db.insert_event(ts, &event,
                                session.last_app_name.as_deref(),
                                session.last_window_title.as_deref(),
                                payload.as_deref(),
                            ) {
                                eprintln!("mhd: blackbox: insert {event}: {e}");
                            } else {
                                events_since_flush += 1;
                            }
                            check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                        }
                    }
                    BlackboxEvent::ToggleEnabled => {
                        enabled = !enabled;
                        BLACKBOX_ENABLED.store(enabled, Ordering::Relaxed);
                        let now = epoch_secs();
                        ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
                        if enabled {
                            let title = get_foreground_title();
                            let app = get_app_name();
                            if let Some(ref a) = app {
                                session.last_app_name = Some(a.clone());
                            }
                            if !title.is_empty() {
                                session.last_window_title = Some(title.clone());
                            }
                            if let Err(e) = db.insert_event(now, "logging_on",
                                app.as_deref(),
                                if title.is_empty() { None } else { Some(title.as_str()) },
                                None,
                            ) {
                                eprintln!("mhd: blackbox: insert logging_on: {e}");
                            } else {
                                events_since_flush += 1;
                            }
                        } else {
                            let title = get_foreground_title();
                            let app = get_app_name();
                            if let Err(e) = db.insert_event(now, "logging_off",
                                app.as_deref(),
                                if title.is_empty() { None } else { Some(title.as_str()) },
                                None,
                            ) {
                                eprintln!("mhd: blackbox: insert logging_off: {e}");
                            } else {
                                events_since_flush += 1;
                            }
                        }
                        check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                    }
                }
            }

            BLACKBOX_ENABLED.store(false, Ordering::Relaxed);
            clear_sender();
        })
        .map_err(|e| format!("cannot spawn blackbox thread: {e}"))?;

    Ok(BlackboxHandle { tx, join: Some(join) })
}

// ── Transaction / flush helpers ────────────────────────────────────────

/// Begin a transaction if no events have been written yet in the current batch.
fn ensure_tx(db: &Db, events_since_flush: &mut u32, last_flush_ts: &mut u64) {
    if *events_since_flush == 0 {
        if let Err(e) = db.begin() {
            eprintln!("mhd: blackbox: begin tx: {e}");
        }
        *last_flush_ts = epoch_secs();
    }
}

/// Commit + begin next transaction when the batch threshold is reached.
fn check_flush_inner(db: &Db, events_since_flush: &mut u32, last_flush_ts: &mut u64) {
    let now = epoch_secs();
    if *events_since_flush >= FLUSH_EVENTS || now.saturating_sub(*last_flush_ts) >= FLUSH_SECS {
        if let Err(e) = db.commit() {
            eprintln!("mhd: blackbox: commit: {e}");
        }
        *events_since_flush = 0;
        *last_flush_ts = now;
    }
}

/// End the current session and insert a sessions row + ses_end event.
fn end_session_and_insert(
    db: &Db,
    session: &mut SessionState,
    ts: u64,
    reason: Option<&str>,
    events_since_flush: &mut u32,
    last_flush_ts: &mut u64,
) {
    if let Some(data) = session.end_session(ts, reason) {
        if let Ok(event_id) = db.insert_event(ts, "ses_end",
            session.last_app_name.as_deref(),
            session.last_window_title.as_deref(),
            None,
        ) {
            *events_since_flush += 1;
            let _ = db.insert_session(
                event_id, data.started_ts, data.duration_sec, data.keyboard, data.mouse,
                reason,
            );
        }
        check_flush_inner(db, events_since_flush, last_flush_ts);
    }
}

/// Check both app name and window title, log changes.
fn check_app_and_title(
    db: &Db,
    session: &mut SessionState,
    events_since_flush: &mut u32,
    last_flush_ts: &mut u64,
) {
    // App name
    if let Some(app) = get_app_name() {
        let prev = session.last_app_name.as_deref().unwrap_or("");
        if app != prev {
            let ts = epoch_secs();
            session.last_app_name = Some(app.clone());
            ensure_tx(db, events_since_flush, last_flush_ts);
            let _ = db.insert_event(ts, "app", Some(&app), None, None);
            *events_since_flush += 1;
        }
    }
    // Window title
    let title = get_foreground_title();
    let prev = session.last_window_title.as_deref().unwrap_or("");
    if title != prev {
        let ts = epoch_secs();
        session.last_window_title = Some(title.clone());
        if !title.is_empty() {
            ensure_tx(db, events_since_flush, last_flush_ts);
            let _ = db.insert_event(ts, "win", None, Some(&title), None);
            *events_since_flush += 1;
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn clear_sender() {
    if let Ok(mut guard) = BLACKBOX_TX.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blackbox::db::Db;

    #[test]
    fn test_session_smoke() {
        let mut s = SessionState::new();
        assert!(!s.active);
        s.on_input(InputKind::Keyboard, 1000);
        assert!(s.active);
        assert_eq!(s.keyboard_count, 1);
        assert_eq!(s.mouse_count, 0);
        s.on_input(InputKind::MouseButton, 1005);
        assert_eq!(s.mouse_count, 1);
        s.on_input(InputKind::Wheel, 1010);
        assert_eq!(s.mouse_count, 2);

        let data = s.end_session(1005, None);
        assert!(data.is_some());
        let data = data.unwrap();
        assert_eq!(data.started_ts, 1000);
        assert_eq!(data.duration_sec, 5);
        assert_eq!(data.keyboard, 1);
        assert_eq!(data.mouse, 2);
        assert!(!s.active);
    }

    #[test]
    fn test_time_str() {
        let ts = 9*3600 + 5*60 + 3; // 09:05:03
        assert_eq!(time_str(ts), "09:05:03");
    }

    #[test]
    fn test_time_str_midnight() {
        assert_eq!(time_str(0), "00:00:00");
        assert_eq!(time_str(86399), "23:59:59");
    }
}
