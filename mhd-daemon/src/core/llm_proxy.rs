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
        opus_downgrade_target: cfg.opus_downgrade_target.clone(),
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

/// Set a tier's target. `slot` is "opus"/"sonnet"/"haiku"/"fable"; `target` is "native"
/// or an upstream model id. Returns false if the proxy is off or the slot is unknown.
pub fn set_target(slot: &str, target: &str) -> bool {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.set_target(slot, target))
        .unwrap_or(false)
}
