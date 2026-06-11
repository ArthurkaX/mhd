//! Embedded LLM proxy control.
//!
//! The proxy runs **in-process** (inside the mhd daemon) via the `llm_proxy`
//! library — no separate executable. We hold a [`ProxyControl`] handle and
//! switch model routing by writing the shared state directly; the change takes
//! effect on the proxy's next request, so in-flight Claude Code work is never
//! interrupted.

use std::sync::Mutex;

use llm_proxy::ProxyControl;

use crate::config::LlmProxyConfig;

/// The embedded proxy handle, present while the proxy is running.
static CONTROL: Mutex<Option<ProxyControl>> = Mutex::new(None);

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
    // Build the proxy config entirely from the daemon config — single source
    // of truth, no separate llm-proxy config file.
    let pcfg = llm_proxy::Config {
        anthropic_key: cfg.anthropic_key.clone(),
        upstream_base_url: cfg.endpoint.clone(),
        upstream_key: cfg.api_key.clone(),
        opus_target: cfg.opus.clone(),
        sonnet_target: cfg.sonnet.clone(),
        haiku_target: cfg.haiku.clone(),
    };
    // `persist = false`: runtime switches stay in memory; the daemon config is
    // the source of truth for defaults.
    match llm_proxy::start_embedded_with(pcfg, cfg.port, false) {
        Ok(control) => {
            *guard = Some(control);
            true
        }
        Err(e) => {
            eprintln!("mhd: failed to start embedded llm-proxy on port {}: {e}", cfg.port);
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

/// Restart the embedded proxy with new config. If the proxy is off, starts it.
/// In-flight requests are aborted by the underlying `stop()` + `start()` cycle.
/// No-op if the proxy is off and `start` returns false.
///
/// The caller is responsible for detecting whether the config has actually
/// changed before calling this.
pub fn reload(cfg: &LlmProxyConfig) -> bool {
    stop();
    start(cfg)
}

/// Current per-tier targets (opus, sonnet, haiku). None if the proxy is off.
pub fn get_targets() -> Option<(String, String, String)> {
    CONTROL.lock().unwrap().as_ref().map(|c| c.targets())
}

/// Set a tier's target. `slot` is "opus"/"sonnet"/"haiku"; `target` is "native"
/// or an upstream model id. Returns false if the proxy is off or the slot is
/// unknown.
pub fn set_target(slot: &str, target: &str) -> bool {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.set_target(slot, target))
        .unwrap_or(false)
}
