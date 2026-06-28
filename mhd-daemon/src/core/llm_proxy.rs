//! Embedded LLM proxy control.
//!
//! The proxy runs **in-process** (inside the mhd daemon) via the `llm_proxy`
//! library — no separate executable. We hold a [`ProxyControl`] handle and
//! switch model routing by writing the shared state directly; the change takes
//! effect on the proxy's next request, so in-flight Claude Code work is never
//! interrupted.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

use llm_proxy::ProxyControl;
use llm_proxy::state::{TraceEntry, VisionTraceEntry};

use crate::config::LlmProxyConfig;

/// The embedded proxy handle, present while the proxy is running.
static CONTROL: Mutex<Option<ProxyControl>> = Mutex::new(None);

/// Last port the proxy was started on (used for port-change detection).
static LAST_PORT: AtomicU16 = AtomicU16::new(0);

/// Whether the embedded proxy is currently running.
pub fn is_running() -> bool {
    CONTROL.lock().unwrap().is_some()
}

/// Start the embedded proxy if not already running. Returns the running state.
pub fn start(cfg: &LlmProxyConfig) -> bool {
    let mut guard = CONTROL.lock().unwrap();
    if guard.is_some() {
        return true;
    }
    let main = cfg.providers.first();
    let pcfg = llm_proxy::Config {
        anthropic_key: cfg.anthropic_key.clone(),
        upstream_base_url: main.map(|p| p.endpoint.clone()).unwrap_or_default(),
        upstream_key: cfg.upstream_key.clone(),
        opus_target: cfg.opus.clone(),
        sonnet_target: cfg.sonnet.clone(),
        haiku_target: cfg.haiku.clone(),
        fable_target: cfg.fable.clone(),
        log_level: cfg.log_level.clone(),
        opus_downgrade_enabled: cfg.opus_downgrade_enabled,
        sonnet_downgrade_enabled: cfg.sonnet_downgrade_enabled,
        trim_enabled: cfg.trim_enabled,
        db_log_enabled: true, // DB log is always on; verbosity is a separate knob
    };
    match llm_proxy::start_embedded_with(pcfg, cfg.port, false, &cfg.bind_address) {
        Ok(control) => {
            LAST_PORT.store(cfg.port, Ordering::Relaxed);
            *guard = Some(control);
            true
        }
        Err(e) => {
            eprintln!(
                "mhd: failed to start embedded llm-proxy on port {}: {e}",
                cfg.port
            );
            false
        }
    }
}

/// Stop the embedded proxy (graceful shutdown).
pub fn stop() {
    if let Some(mut control) = CONTROL.lock().unwrap().take() {
        control.stop();
    }
}

/// Toggle the proxy on/off. Returns the new running state.
pub fn toggle(cfg: &LlmProxyConfig) -> bool {
    if is_running() {
        stop();
        false
    } else {
        start(cfg)
    }
}

/// Soft reload the embedded proxy with new config. Runtime targets and log
/// level are updated without restarting. Only a port change triggers a full
/// stop + start cycle.
///
/// If the proxy is off and `enabled` is true, starts it. No-op if the proxy
/// is off and enabled is false.
pub fn reload(cfg: &LlmProxyConfig) -> bool {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref control) = *guard {
        control.set_target("opus", &cfg.opus);
        control.set_target("sonnet", &cfg.sonnet);
        control.set_target("haiku", &cfg.haiku);
        control.set_target("fable", &cfg.fable);
        control.set_log_level(&cfg.log_level);
        control.set_trim_enabled(cfg.trim_enabled);

        if cfg.port != LAST_PORT.load(Ordering::Relaxed) {
            drop(guard);
            stop();
            return start(cfg);
        }
        true
    } else {
        drop(guard);
        if cfg.enabled { start(cfg) } else { false }
    }
}

/// Current per-tier targets (opus, sonnet, haiku, fable). None if the proxy is off.
pub fn get_targets() -> Option<(String, String, String, String)> {
    CONTROL.lock().unwrap().as_ref().map(|c| c.targets())
}

/// Snapshot of recent proxy routing decisions.
pub fn get_trace() -> Vec<TraceEntry> {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.trace())
        .unwrap_or_default()
}

/// Snapshot of recent vision screenshot requests.
pub fn get_vision_trace() -> Vec<VisionTraceEntry> {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.vision_trace())
        .unwrap_or_default()
}

/// Record a vision screenshot request in the proxy's trace buffer and SQLite log.
/// Silently no-ops if the proxy is not running.
pub fn log_vision(entry: VisionTraceEntry, db_event: Option<llm_proxy::LogEvent>) {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref c) = *guard {
        c.push_vision_trace(entry);
        if let Some(event) = db_event {
            c.log_event(event);
        }
    }
}

/// Log a structured event to the proxy's SQLite database.
/// Silently no-ops if the proxy is not running.
pub fn log_event(event: llm_proxy::LogEvent) {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref c) = *guard {
        c.log_event(event);
    }
}

/// Write a free-text note to the proxy's SQLite database.
/// Silently no-ops if the proxy is not running.
pub fn log_note(text: &str) {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref c) = *guard {
        c.log_note(text);
    }
}

/// Whether debug logging is enabled on the proxy.
pub fn is_debug_logging() -> bool {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.log_level() != "none")
        .unwrap_or(false)
}

/// Toggle debug logging on the proxy (none ↔ detailed).
/// Also enables/disables the SQLite database log.
pub fn toggle_debug_logging() -> bool {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref c) = *guard {
        let current = c.log_level();
        let new = if current == "none" {
            "detailed"
        } else {
            "none"
        };
        c.set_log_level(new);
        let enabled = new != "none";
        c.set_db_log_enabled(enabled);
        enabled
    } else {
        false
    }
}

/// Enable or disable request compression via llmtrim.
pub fn set_trim_enabled(enabled: bool) -> bool {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| {
            c.set_trim_enabled(enabled);
            true
        })
        .unwrap_or(false)
}

/// Current trim toggle state.
pub fn is_trim_enabled() -> bool {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.is_trim_enabled())
        .unwrap_or(false)
}

/// Toggle trim on/off. Returns the new state.
pub fn toggle_trim() -> bool {
    let guard = CONTROL.lock().unwrap();
    if let Some(ref c) = *guard {
        let enabled = c.is_trim_enabled();
        let new = !enabled;
        c.set_trim_enabled(new);
        // Persist the change via settings save
        if let Ok(mut settings) = llm_proxy::config::load_settings() {
            settings.trim_enabled = new;
            let _ = llm_proxy::config::save_settings(&settings);
        }
        new
    } else {
        false
    }
}

/// Set a tier's target. `slot` is "opus"/"sonnet"/"haiku"/"fable"; `target` is "native"
/// or an upstream model id. The selection is persisted to settings.json so a chosen
/// model survives daemon restarts instead of reverting to the previously saved value.
/// Returns false if the proxy is off or the slot is unknown (the value is persisted
/// regardless of the running state).
pub fn set_target(slot: &str, target: &str) -> bool {
    // Persist the selection so it is restored on the next start.
    if let Ok(mut settings) = llm_proxy::config::load_settings() {
        match slot {
            "opus" => settings.opus_target = target.to_string(),
            "sonnet" => settings.sonnet_target = target.to_string(),
            "haiku" => settings.haiku_target = target.to_string(),
            "fable" => settings.fable_target = target.to_string(),
            _ => {}
        }
        let _ = llm_proxy::config::save_settings(&settings);
    }

    // Apply to the live proxy if it is running.
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.set_target(slot, target))
        .unwrap_or(false)
}
