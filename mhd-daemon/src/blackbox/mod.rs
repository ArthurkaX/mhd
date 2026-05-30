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
    Move,
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

pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    click_count: u64,
    wheel_count: u64,
    move_count: u64,
    last_window_title: Option<String>,
    /// Last app name (exe without path).
    last_app_name: Option<String>,
    // Current span (valid only while `active`):
    span_started_ts: u64,
    span_app: Option<String>,
    span_win: Option<String>,
    span_keyboard: u64,
    span_clicks: u64,
    span_wheel: u64,
    span_moves: u64,
    /// Completed spans waiting for session-end event id.
    closed_spans: Vec<SpanData>,
}

impl SessionState {
    fn new() -> Self {
        SessionState {
            active: false,
            started_ts: 0,
            last_action_at: 0,
            keyboard_count: 0,
            click_count: 0,
            wheel_count: 0,
            move_count: 0,
            last_window_title: None,
            last_app_name: None,
            span_started_ts: 0,
            span_app: None,
            span_win: None,
            span_keyboard: 0,
            span_clicks: 0,
            span_wheel: 0,
            span_moves: 0,
            closed_spans: Vec::new(),
        }
    }

    fn on_input(&mut self, kind: InputKind, ts: u64) {
        if !self.active {
            self.active = true;
            self.started_ts = ts;
            self.last_action_at = ts;
            self.keyboard_count = 0;
            self.click_count = 0;
            self.wheel_count = 0;
            self.move_count = 0;
            // Start first span
            self.span_started_ts = ts;
            self.span_app = self.last_app_name.clone();
            self.span_win = self.last_window_title.clone();
            self.span_keyboard = 0;
            self.span_clicks = 0;
            self.span_wheel = 0;
            self.span_moves = 0;
        } else {
            self.last_action_at = ts;
        }
        match kind {
            InputKind::Keyboard => { self.keyboard_count += 1; self.span_keyboard += 1; }
            InputKind::MouseButton => { self.click_count += 1; self.span_clicks += 1; }
            InputKind::Wheel => { self.wheel_count += 1; self.span_wheel += 1; }
            InputKind::Move => { self.move_count += 1; self.span_moves += 1; }
        }
    }

    fn end_session(&mut self, ts: u64, reason: Option<&str>) -> Option<SessionEndData> {
        if !self.active { return None; }
        let duration = ts.saturating_sub(self.started_ts);
        let active_sec = self.last_action_at.saturating_sub(self.started_ts);
        let data = SessionEndData {
            started_ts: self.started_ts,
            duration_sec: duration,
            active_sec,
            keyboard: self.keyboard_count,
            clicks: self.click_count,
            wheel: self.wheel_count,
            moves: self.move_count,
            end_reason: reason.map(|s| s.to_string()),
        };
        self.active = false;
        Some(data)
    }

    /// Snapshot the current span, reset for a fresh span, and return the data.
    fn take_span(&mut self, end_ts: u64) -> Option<SpanData> {
        if !self.active { return None; }
        let data = SpanData {
            app: self.span_app.clone(),
            win: self.span_win.clone(),
            started_ts: self.span_started_ts,
            duration_sec: end_ts.saturating_sub(self.span_started_ts),
            keyboard: self.span_keyboard,
            clicks: self.span_clicks,
            wheel: self.span_wheel,
            moves: self.span_moves,
        };
        // Reset for a new span
        self.span_started_ts = end_ts;
        self.span_keyboard = 0;
        self.span_clicks = 0;
        self.span_wheel = 0;
        self.span_moves = 0;
        self.span_app = self.last_app_name.clone();
        self.span_win = self.last_window_title.clone();
        Some(data)
    }
}

struct SessionEndData {
    started_ts: u64,
    duration_sec: u64,
    active_sec: u64,
    keyboard: u64,
    clicks: u64,
    wheel: u64,
    moves: u64,
    end_reason: Option<String>,
}

struct SpanData {
    app: Option<String>,
    win: Option<String>,
    started_ts: u64,
    duration_sec: u64,
    keyboard: u64,
    clicks: u64,
    wheel: u64,
    moves: u64,
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
///
/// Values containing spaces or `=` are shell-style quoted: `key="val ue"`.
/// Backslashes and double-quotes inside values are escaped.
///
/// Only used internally for `LogCustom` events (pomodoro) where keys/values
/// are controlled — but this ensures correct round-tripping regardless.
fn kv_payload(kv: &[(String, String)]) -> String {
    kv.iter()
        .map(|(k, v)| {
            let needs_quote = v.contains(' ') || v.contains('"') || v.contains('\\') || v.contains('=') || v.is_empty();
            if needs_quote {
                let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
                format!("{k}=\"{escaped}\"")
            } else {
                format!("{k}={v}")
            }
        })
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

            // Heartbeat tracking
            let mut last_heartbeat_ts: u64 = epoch_secs();
            const HEARTBEAT_SECS: u64 = 300; // 5 minutes

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

            // Write daemon_start event with current context + version stamp
            ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
            let payload = format!("v={} schema=2", env!("CARGO_PKG_VERSION"));
            if let Err(e) = db.insert_event(now, "daemon_start",
                initial_app.as_deref(),
                if initial_title.is_empty() { None } else { Some(initial_title.as_str()) },
                Some(&payload),
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
                                end_session_and_insert(&db, &mut session, now, Some("idle"),
                                    &mut events_since_flush, &mut last_flush_ts);
                            }
                        }
                        if enabled {
                            check_app_and_title(&db, &mut session,
                                &mut events_since_flush, &mut last_flush_ts);

                            // Heartbeat with foreground app + idle flag
                            let now = epoch_secs();
                            if now.saturating_sub(last_heartbeat_ts) >= HEARTBEAT_SECS {
                                last_heartbeat_ts = now;
                                let hb_app = get_app_name();
                                let idle_flag = if session.active { 0 } else { 1 };
                                let payload = format!("idle={idle_flag}");
                                ensure_tx(&db, &mut events_since_flush, &mut last_flush_ts);
                                let _ = db.insert_event(now, "heartbeat",
                                    hb_app.as_deref(), None, Some(&payload));
                                events_since_flush += 1;
                                check_flush_inner(&db, &mut events_since_flush, &mut last_flush_ts);
                            }
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

/// End the current session, flush all spans, and insert ses_end + sessions rows.
fn end_session_and_insert(
    db: &Db,
    session: &mut SessionState,
    ts: u64,
    reason: Option<&str>,
    events_since_flush: &mut u32,
    last_flush_ts: &mut u64,
) {
    // 1. Snapshot the still-open span BEFORE ending the session
    let final_span = session.take_span(ts);

    // 2. End the session (sets active=false)
    let Some(data) = session.end_session(ts, reason) else { return };

    // 3. Begin transaction if needed
    ensure_tx(db, events_since_flush, last_flush_ts);

    // 4. Insert the ses_end event — this owns all spans
    let event_id = match db.insert_event(ts, "ses_end",
        session.last_app_name.as_deref(),
        session.last_window_title.as_deref(),
        None,
    ) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("mhd: blackbox: insert ses_end: {e}");
            return;
        }
    };
    *events_since_flush += 1;

    // 5. Insert the session row (new split counters)
    let _ = db.insert_session(
        event_id, data.started_ts, data.duration_sec, data.active_sec,
        data.keyboard, data.clicks, data.wheel, data.moves,
        data.end_reason.as_deref(),
    );

    // 6. Flush all buffered spans + the final one, all referencing event_id
    for sp in session.closed_spans.drain(..).chain(final_span.into_iter()) {
        let _ = db.insert_app_span(
            event_id,
            sp.app.as_deref(),
            sp.win.as_deref(),
            sp.started_ts,
            sp.duration_sec,
            sp.keyboard,
            sp.clicks,
            sp.wheel,
            sp.moves,
        );
        *events_since_flush += 1;
    }

    // 7. Maybe flush batch
    check_flush_inner(db, events_since_flush, last_flush_ts);
}

/// Check both app name and window title, log a single combined "win" event.
/// On app change inside an active session, split the span.
fn check_app_and_title(
    db: &Db,
    session: &mut SessionState,
    events_since_flush: &mut u32,
    last_flush_ts: &mut u64,
) {
    let app   = get_app_name();
    let title = get_foreground_title();

    let app_changed   = app.as_deref() != session.last_app_name.as_deref();
    let title_changed = title != session.last_window_title.as_deref().unwrap_or("");

    if app_changed || title_changed {
        let ts = epoch_secs();

        // If app changed inside an active session, split the current span
        if app_changed && session.active {
            let span_end = ts;
            if let Some(sp) = session.take_span(span_end) {
                session.closed_spans.push(sp);
            }
            // The new span has already been started by take_span (fields reset)
            // Re-point the fresh span to the new app/title
            session.span_app = app.clone();
            session.span_win = if title.is_empty() { None } else { Some(title.clone()) };
        }

        // Update cached state
        session.last_app_name = app.clone();
        if !title.is_empty() {
            session.last_window_title = Some(title.clone());
        }

        // Ensure the app has a category row
        if let Some(ref a) = app {
            if let Err(e) = db.ensure_app_category(a) {
                eprintln!("mhd: blackbox: ensure_app_category: {e}");
            }
        }

        // Emit single combined event carrying both fields
        if app.is_some() || !title.is_empty() {
            ensure_tx(db, events_since_flush, last_flush_ts);
            let _ = db.insert_event(
                ts, "win",
                app.as_deref(),
                if title.is_empty() { None } else { Some(title.as_str()) },
                None,
            );
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

    #[test]
    fn test_session_smoke() {
        let mut s = SessionState::new();
        assert!(!s.active);
        s.on_input(InputKind::Keyboard, 1000);
        assert!(s.active);
        assert_eq!(s.keyboard_count, 1);
        assert_eq!(s.click_count, 0);
        assert_eq!(s.wheel_count, 0);
        assert_eq!(s.move_count, 0);
        s.on_input(InputKind::MouseButton, 1005);
        assert_eq!(s.click_count, 1);
        s.on_input(InputKind::Wheel, 1010);
        assert_eq!(s.wheel_count, 1);
        s.on_input(InputKind::Move, 1012);
        assert_eq!(s.move_count, 1);

        let data = s.end_session(1014, None);
        assert!(data.is_some());
        let data = data.unwrap();
        assert_eq!(data.started_ts, 1000);
        assert_eq!(data.duration_sec, 14);
        assert_eq!(data.active_sec, 12); // last_action_at 1012 - started_ts 1000
        assert_eq!(data.keyboard, 1);
        assert_eq!(data.clicks, 1);
        assert_eq!(data.wheel, 1);
        assert_eq!(data.moves, 1);
        assert!(!s.active);
    }

}
