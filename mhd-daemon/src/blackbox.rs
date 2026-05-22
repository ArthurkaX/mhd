#![cfg(feature = "blackbox")]

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

fn daily_log_path(date: &str) -> PathBuf {
    blackbox_dir().join(format!("{date}.log"))
}

fn today_str() -> String {
    let secs = epoch_secs();
    let (y, m, d) = date_from_epoch(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Time-of-day string (HH:MM:SS) for a given epoch second.
fn time_str(ts: u64) -> String {
    let s = (ts % 86400) as i64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn date_from_epoch(secs: u64) -> (i64, u32, u32) {
    let days = (secs / 86400) as i64;
    let mut y = 1970i64;
    let mut rem = days;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if rem < diy { break; }
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
        if rem < md { break; }
        rem -= md;
        m += 1;
    }
    if m > 12 { m = 12; rem = mdays[11] as i64 - 1; }
    (y, m, (rem + 1) as u32)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ── Line formatter (compact) ─────────────────────────────────────────────

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

/// Format: `HH:MM:SS event=short_name k=v …`   (date is in filename).
fn format_line(ts: u64, event: &str, kv: &[(&str, String)]) -> String {
    let mut line = format!("{} {}", time_str(ts), event);
    for (key, val) in kv {
        let q = val.contains(' ') || val.contains('"') || val.contains('\\') || val.is_empty();
        if q {
            line.push_str(&format!(" {}=\"{}\"", key, escape(val)));
        } else {
            line.push_str(&format!(" {}={}", key, escape(val)));
        }
    }
    line.push('\n');
    line
}

fn sv(key: &'static str, val: &str) -> (&'static str, String) {
    (key, val.to_string())
}
fn nv(key: &str, val: u64) -> (&str, String) {
    (key, val.to_string())
}

// ── File writer ──────────────────────────────────────────────────────────

struct LogWriter {
    current_date: String,
    file: Option<std::fs::File>,
    dir_ensured: bool,
    /// Lines written since last flush.
    lines_since_flush: u32,
    /// Epoch seconds of last flush.
    last_flush_ts: u64,
}

const FLUSH_LINES: u32 = 50;
const FLUSH_SECS: u64 = 20;

impl LogWriter {
    fn new() -> Self {
        LogWriter {
            current_date: String::new(),
            file: None,
            dir_ensured: false,
            lines_since_flush: 0,
            last_flush_ts: 0,
        }
    }
    fn ensure_dir(&mut self) -> Result<(), String> {
        if self.dir_ensured { return Ok(()); }
        std::fs::create_dir_all(&blackbox_dir())
            .map_err(|e| format!("cannot create blackbox dir: {e}"))?;
        self.dir_ensured = true;
        Ok(())
    }
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        self.ensure_dir()?;
        let date = today_str();
        if date != self.current_date || self.file.is_none() {
            self.file.take();
            let path = daily_log_path(&date);
            let file = std::fs::OpenOptions::new()
                .create(true).append(true)
                .open(&path)
                .map_err(|e| format!("cannot open blackbox log '{}': {e}", path.display()))?;
            self.file = Some(file);
            self.current_date = date;
        }
        if let Some(ref mut f) = self.file {
            f.write_all(line.as_bytes())
                .map_err(|e| format!("cannot write blackbox log: {e}"))?;
            self.lines_since_flush += 1;
            let now = epoch_secs();
            if self.lines_since_flush >= FLUSH_LINES
                || now.saturating_sub(self.last_flush_ts) >= FLUSH_SECS
            {
                f.flush().map_err(|e| format!("cannot flush blackbox log: {e}"))?;
                self.lines_since_flush = 0;
                self.last_flush_ts = now;
            }
        }
        Ok(())
    }
}

// ── Session state machine ────────────────────────────────────────────────

struct SessionState {
    active: bool,
    started_at: u64,
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
            started_at: 0,
            last_action_at: 0,
            keyboard_count: 0,
            mouse_count: 0,
            last_window_title: None,
            last_app_name: None,
        }
    }

    fn on_input(&mut self, kind: InputKind, ts: u64, writer: &mut LogWriter) {
        if !self.active {
            self.active = true;
            self.started_at = ts;
            self.last_action_at = ts;
            self.keyboard_count = 0;
            self.mouse_count = 0;
            let _ = writer.write_line(&format_line(ts, "ses+", &[]));
        } else {
            self.last_action_at = ts;
        }
        match kind {
            InputKind::Keyboard => self.keyboard_count += 1,
            InputKind::MouseButton | InputKind::Wheel => self.mouse_count += 1,
        }
    }

    fn end_session(&mut self, _ts: u64, writer: &mut LogWriter, reason: Option<&str>) {
        if !self.active { return; }
        let duration = self.last_action_at.saturating_sub(self.started_at);
        let acts = self.keyboard_count + self.mouse_count;
        let mut kv = vec![nv("d", duration), nv("a", acts), nv("k", self.keyboard_count), nv("m", self.mouse_count)];
        if let Some(r) = reason {
            kv.push(sv("r", r));
        } else {
            kv.push(nv("i", 300));
        }
        let _ = writer.write_line(&format_line(self.last_action_at, "ses-", &kv));
        self.active = false;
    }
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
            let mut writer = LogWriter::new();
            let mut session = SessionState::new();
            let mut enabled = true;
            BLACKBOX_ENABLED.store(true, Ordering::Relaxed);

            let now = epoch_secs();

            let initial_title = get_foreground_title();
            if !initial_title.is_empty() {
                session.last_window_title = Some(initial_title.clone());
            }
            let initial_app = get_app_name();
            if let Some(ref app) = initial_app {
                session.last_app_name = Some(app.clone());
            }

            // Write legend header once per run
            let _ = writer.write_line(&format_line(now, "# legend: start=monitoring_start blackbox_on/off=logging_toggle ses+=session_start ses-=session_end d=duration_sec a=actions k=keyboard m=mouse i=idle_sec r=reason win=window_title app=app_name n=app_name t=title", &[]));

            // Write start event with current context
            let mut kv = Vec::new();
            if let Some(ref app) = initial_app {
                kv.push(sv("n", app));
            }
            if !initial_title.is_empty() {
                kv.push(sv("t", &initial_title));
            }
            let _ = writer.write_line(&format_line(now, "start", &kv));

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
                                session.end_session(session.last_action_at, &mut writer, None);
                            }
                        }
                        if enabled {
                            check_app_and_title(&mut session, &mut writer);
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };

                match event {
                    BlackboxEvent::Input { kind, ts } => {
                        if enabled {
                            session.on_input(kind, ts, &mut writer);
                            check_app_and_title(&mut session, &mut writer);
                        }
                    }
                    BlackboxEvent::Shutdown => {
                        let now = epoch_secs();
                        if enabled {
                            session.end_session(now, &mut writer, Some("stop"));
                        }
                        let _ = writer.write_line(&format_line(now, "stop", &[sv("r", "quit")]));
                        break;
                    }
                    BlackboxEvent::ToggleEnabled => {
                        enabled = !enabled;
                        BLACKBOX_ENABLED.store(enabled, Ordering::Relaxed);
                        let now = epoch_secs();
                        if enabled {
                            // Snapshot current context for on event
                            let title = get_foreground_title();
                            let app = get_app_name();
                            let mut kv = Vec::new();
                            if let Some(ref a) = app {
                                kv.push(sv("n", a));
                                session.last_app_name = Some(a.clone());
                            }
                            if !title.is_empty() {
                                kv.push(sv("t", &title));
                                session.last_window_title = Some(title.clone());
                            }
                            let _ = writer.write_line(&format_line(now, "blackbox_on", &kv));
                        } else {
                            let title = get_foreground_title();
                            let app = get_app_name();
                            let mut kv = Vec::new();
                            if let Some(ref a) = app {
                                kv.push(sv("n", a));
                            }
                            if !title.is_empty() {
                                kv.push(sv("t", &title));
                            }
                            let _ = writer.write_line(&format_line(now, "blackbox_off", &kv));
                        }
                    }
                }
            }

            BLACKBOX_ENABLED.store(false, Ordering::Relaxed);
            clear_sender();
        })
        .map_err(|e| format!("cannot spawn blackbox thread: {e}"))?;

    Ok(BlackboxHandle { tx, join: Some(join) })
}

/// Check both app name and window title, log changes.
fn check_app_and_title(session: &mut SessionState, writer: &mut LogWriter) {
    // App name
    if let Some(app) = get_app_name() {
        let prev = session.last_app_name.as_deref().unwrap_or("");
        if app != prev {
            let ts = epoch_secs();
            session.last_app_name = Some(app.clone());
            let _ = writer.write_line(&format_line(ts, "app", &[sv("n", &app)]));
        }
    }
    // Window title
    let title = get_foreground_title();
    let prev = session.last_window_title.as_deref().unwrap_or("");
    if title != prev {
        let ts = epoch_secs();
        session.last_window_title = Some(title.clone());
        if !title.is_empty() {
            let _ = writer.write_line(&format_line(ts, "win", &[sv("t", &title)]));
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
    fn test_escape_normal() {
        assert_eq!(escape("hello"), "hello");
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_escape_double_quote() {
        assert_eq!(escape("a\"b"), "a\\\"b");
    }

    #[test]
    fn test_escape_newline_becomes_space() {
        assert_eq!(escape("a\nb"), "a b");
    }

    #[test]
    fn test_escape_carriage_return_becomes_space() {
        assert_eq!(escape("a\rb"), "a b");
    }

    #[test]
    fn test_escape_combined() {
        assert_eq!(
            escape("has \"quotes\" and \\ back"),
            "has \\\"quotes\\\" and \\\\ back"
        );
    }

    #[test]
    fn test_format_line_time_only() {
        let line = format_line(1716371523, "tst", &[sv("k", "v")]);
        // format_line outputs `HH:MM:SS tst k=v` (без event=)
        assert_eq!(&line[..8], "09:52:03", "line: {line}");
        assert!(line.starts_with("09:52:03 tst"), "line: {line}");
        assert!(line.contains("k=v"), "line: {line}");
        assert!(line.ends_with('\n'), "line: {line}");
    }

    #[test]
    fn test_format_line_quoted_value() {
        let line = format_line(1716371523, "w", &[sv("t", "my window")]);
        assert!(line.contains(r##"t="my window""##), "line: {line}");
    }

    #[test]
    fn test_format_line_numeric() {
        let line = format_line(1716371523, "ses-", &[nv("d", 60), nv("k", 42)]);
        assert!(line.contains("d=60"), "line: {line}");
        assert!(line.contains("k=42"), "line: {line}");
    }

    #[test]
    fn test_session_smoke() {
        let mut s = SessionState::new();
        let mut w = LogWriter::new();
        assert!(!s.active);
        s.on_input(InputKind::Keyboard, 1000, &mut w);
        assert!(s.active);
        assert_eq!(s.keyboard_count, 1);
        assert_eq!(s.mouse_count, 0);
        s.on_input(InputKind::MouseButton, 1005, &mut w);
        assert_eq!(s.mouse_count, 1);
        s.on_input(InputKind::Wheel, 1010, &mut w);
        assert_eq!(s.mouse_count, 2);
        s.end_session(1005, &mut w, None);
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

    #[test]
    fn test_today_str_format() {
        let s = today_str();
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    #[test]
    fn test_date_from_epoch_1970() {
        let (y, m, d) = date_from_epoch(0);
        assert_eq!(y, 1970);
        assert_eq!(m, 1);
        assert_eq!(d, 1);
    }


}
