//! Blackbox — minimal append‑only behaviour logger.
//!
//! Records daily activity log: input actions (keyboard/mouse), session
//! boundaries, foreground window changes, and monitoring lifecycle.
//!
//! Config section `[blackbox]`; disabled by default.
//! All file I/O happens in the blackbox worker thread — never in the
//! low‑level hook hot path.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;

use crate::config::path::home_dir;

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

/// Event that the hook thread can send to the blackbox worker.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BlackboxEvent {
    /// A counted input action (keyboard press, mouse button down, wheel).
    Input {
        kind: InputKind,
        ts: u64,
    },
    /// Foreground window title changed.
    WindowChanged {
        title: String,
        ts: u64,
    },
    /// Shutdown the blackbox worker.
    Shutdown,
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

/// Send an event from the hook hot‑path (lock‑free after initial setup).
pub fn send_event(event: BlackboxEvent) {
    if let Ok(guard) = BLACKBOX_TX.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(event);
        }
    }
}

/// Returns `true` if blackbox is active (sender installed).
#[allow(dead_code)]
pub fn is_active() -> bool {
    BLACKBOX_TX.lock().map(|g| g.is_some()).unwrap_or(false)
}

// ── Log path helpers ─────────────────────────────────────────────────────

fn blackbox_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("blackbox")
}

fn daily_log_path(date: &str) -> PathBuf {
    blackbox_dir().join(format!("{date}.log"))
}

/// Return today's date as `YYYY-MM-DD`.
fn today_str() -> String {
    let secs = epoch_secs();
    let (y, m, d) = date_from_epoch(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Seconds since UNIX_EPOCH (local time — we don't do timezone math,
/// system clock is assumed local).
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Decompose epoch seconds into (year, month, day).  Naïve Gregorian
/// calendar — good enough for a log timestamp.
fn date_from_epoch(secs: u64) -> (i64, u32, u32) {
    let days = (secs / 86400) as i64;
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if rem < diy {
            break;
        }
        rem -= diy;
        y += 1;
    }
    let mdays = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u32;
    for &md in &mdays {
        if rem < md {
            break;
        }
        rem -= md;
        m += 1;
    }
    if m > 12 {
        m = 12;
        rem = mdays[11] as i64 - 1;
    }
    let d = (rem + 1) as u32;
    (y, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ── Line formatter ───────────────────────────────────────────────────────

/// Escape a string value for the log format.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Format a single log line.
fn format_line(ts: u64, event: &str, kv: &[(&str, String)]) -> String {
    let (y, m, d) = date_from_epoch(ts);
    let time_secs = (ts % 86400) as i64;
    let h = time_secs / 3600;
    let min = (time_secs % 3600) / 60;
    let sec = time_secs % 60;
    let mut line = format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{sec:02} event={event}");

    for (key, val) in kv {
        let needs_quoting = val.contains(' ')
            || val.contains('"')
            || val.contains('\\')
            || val.is_empty();
        if needs_quoting {
            line.push_str(&format!(" {key}=\"{}\"", escape(val)));
        } else {
            line.push_str(&format!(" {key}={}", escape(val)));
        }
    }
    line.push('\n');
    line
}

/// Helper: string pair for format_line.
fn sv(key: &'static str, val: &str) -> (&'static str, String) {
    (key, val.to_string())
}

/// Helper: numeric pair.
fn nv(key: &str, val: u64) -> (&str, String) {
    (key, val.to_string())
}

// ── File writer ──────────────────────────────────────────────────────────

struct LogWriter {
    current_date: String,
    file: Option<std::fs::File>,
    dir_ensured: bool,
}

impl LogWriter {
    fn new() -> Self {
        LogWriter {
            current_date: String::new(),
            file: None,
            dir_ensured: false,
        }
    }

    fn ensure_dir(&mut self) -> Result<(), String> {
        if self.dir_ensured {
            return Ok(());
        }
        let dir = blackbox_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create blackbox dir: {e}"))?;
        self.dir_ensured = true;
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> Result<(), String> {
        self.ensure_dir()?;

        let date = today_str();
        if date != self.current_date || self.file.is_none() {
            self.file.take(); // close previous
            let path = daily_log_path(&date);
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| {
                    format!("cannot open blackbox log '{}': {e}", path.display())
                })?;
            self.file = Some(file);
            self.current_date = date;
        }

        if let Some(ref mut f) = self.file {
            f.write_all(line.as_bytes())
                .map_err(|e| format!("cannot write blackbox log: {e}"))?;
            f.flush()
                .map_err(|e| format!("cannot flush blackbox log: {e}"))?;
        }
        Ok(())
    }
}

// ── Session state machine ────────────────────────────────────────────────

struct SessionState {
    /// Are we in an active session?
    active: bool,
    started_at: u64,
    last_action_at: u64,
    keyboard_count: u64,
    mouse_count: u64,
    /// Title we last wrote to the log (for dedup).
    last_window_title: Option<String>,
}

impl SessionState {
    fn new() -> Self {
        SessionState {
            active: false,
            started_at: 0,
            last_action_at: 0,
            keyboard_count: 0,
            mouse_count: 0,
            last_window_title: None,
        }
    }

    /// Process a counted input action.
    fn on_input(&mut self, kind: InputKind, ts: u64, writer: &mut LogWriter) {
        if !self.active {
            // Start a new session
            self.active = true;
            self.started_at = ts;
            self.last_action_at = ts;
            self.keyboard_count = 0;
            self.mouse_count = 0;
            // Write session_started (ts = first action time)
            let line = format_line(ts, "session_started", &[]);
            let _ = writer.write_line(&line);
        } else {
            self.last_action_at = ts;
        }

        match kind {
            InputKind::Keyboard => self.keyboard_count += 1,
            InputKind::MouseButton | InputKind::Wheel => self.mouse_count += 1,
        }
    }

    /// End the current session (idle timeout or stop).
    fn end_session(&mut self, _ts: u64, writer: &mut LogWriter, reason: Option<&str>) {
        if !self.active {
            return;
        }
        let duration = self.last_action_at.saturating_sub(self.started_at);
        let actions = self.keyboard_count + self.mouse_count;

        let mut kv = vec![
            nv("duration_sec", duration),
            nv("actions", actions),
            nv("keyboard", self.keyboard_count),
            nv("mouse", self.mouse_count),
        ];
        if let Some(r) = reason {
            kv.push(sv("reason", r));
        } else {
            kv.push(nv("idle_sec", 300)); // default idle
        }

        let line = format_line(self.last_action_at, "session_ended", &kv);
        let _ = writer.write_line(&line);

        self.active = false;
    }
}

// ── Foreground window watcher (polling variant) ─────────────────────────

/// Returns the title of the current foreground window.
fn get_foreground_title() -> String {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if hwnd == HWND::default() {
            return String::new();
        }
        let mut buf = [0u16; 512];
        let len = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }
}

// ── Worker thread ────────────────────────────────────────────────────────

/// Start the blackbox monitoring worker.
///
/// Spawns a background thread that:
/// - Listens for input events from the hook
/// - Manages the session state machine
/// - Writes to daily log files
/// - Tracks foreground window (polling, 2 s granularity)
/// - Detects idle deadlines
///
/// Returns a handle whose `shutdown()` can stop the worker.
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
}

/// Start blackbox monitoring.
///
/// Called once when mhd starts, if `config.enabled == true`.
/// The returned handle should be kept alive until shutdown to ensure
/// the worker thread completes its final writes.
pub fn start(config: BlackboxConfig) -> Result<BlackboxHandle, String> {
    let idle_seconds = config.idle_seconds;

    // Channel: hook → worker
    let (tx, rx) = mpsc::channel::<BlackboxEvent>();

    // Store sender for hook thread
    {
        let mut guard = BLACKBOX_TX.lock().unwrap();
        *guard = Some(tx.clone());
    }

    let join = thread::Builder::new()
        .name("blackbox".into())
        .spawn(move || {
            let mut writer = LogWriter::new();
            let mut session = SessionState::new();

            // Write monitoring_started
            let now = epoch_secs();
            let line = format_line(now, "monitoring_started", &[]);
            if let Err(e) = writer.write_line(&line) {
                eprintln!("mhd: blackbox: {e}");
                clear_sender();
                return;
            }

            // Save initial window title
            let initial_title = get_foreground_title();
            if !initial_title.is_empty() {
                session.last_window_title = Some(initial_title.clone());
                let line = format_line(now, "window_changed", &[sv("title", &initial_title)]);
                let _ = writer.write_line(&line);
            }

            // Main event loop
            loop {
                // If session is active, use timeout for idle detection
                let timeout = Duration::from_secs(if session.active {
                    idle_seconds.min(2) // 2s for responsive shutdown + window polling
                } else {
                    2 // 2s polling for window changes when idle
                });

                let event = match rx.recv_timeout(timeout) {
                    Ok(e) => e,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Check idle deadline if active
                        if session.active {
                            let now = epoch_secs();
                            if now >= session.last_action_at + idle_seconds {
                                session.end_session(session.last_action_at, &mut writer, None);
                            }
                        }
                        // Poll foreground window
                        check_window_change(&mut session, &mut writer);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };

                match event {
                    BlackboxEvent::Input { kind, ts } => {
                        session.on_input(kind, ts, &mut writer);
                        // Also check window (may have changed during idle)
                        check_window_change(&mut session, &mut writer);
                    }
                    BlackboxEvent::WindowChanged { title, ts } => {
                        let prev = session.last_window_title.as_deref().unwrap_or("");
                        if title == prev {
                            continue; // dedup
                        }
                        session.last_window_title = Some(title.clone());
                        let line = format_line(ts, "window_changed", &[sv("title", &title)]);
                        let _ = writer.write_line(&line);
                        // Window change does NOT start a session
                    }
                    BlackboxEvent::Shutdown => {
                        // End active session if any
                        let now = epoch_secs();
                        session.end_session(now, &mut writer, Some("stop"));
                        // Write monitoring_stopped
                        let line = format_line(now, "monitoring_stopped", &[sv("reason", "quit")]);
                        let _ = writer.write_line(&line);
                        break;
                    }
                }
            }

            clear_sender();
        })
        .map_err(|e| format!("cannot spawn blackbox thread: {e}"))?;

    Ok(BlackboxHandle {
        tx,
        join: Some(join),
    })
}

/// Check if foreground window changed and log if it did.
fn check_window_change(session: &mut SessionState, writer: &mut LogWriter) {
    let title = get_foreground_title();
    let prev = session.last_window_title.as_deref().unwrap_or("");
    if title != prev {
        let ts = epoch_secs();
        session.last_window_title = Some(title.clone());
        if !title.is_empty() {
            let line = format_line(ts, "window_changed", &[sv("title", &title)]);
            let _ = writer.write_line(&line);
        }
    }
}

/// Clear the global sender (called on shutdown).
fn clear_sender() {
    if let Ok(mut guard) = BLACKBOX_TX.lock() {
        *guard = None;
    }
}
