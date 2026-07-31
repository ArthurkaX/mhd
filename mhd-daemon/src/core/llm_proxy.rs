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
use llm_proxy::QuotaSnapshot;
use llm_proxy::state::{TraceEntry, VisionTraceEntry};

use crate::config::LlmProxyConfig;
use crate::core::quota_watcher;

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
        corpus_capture: cfg.corpus_capture,
        db_log_enabled: true, // DB log is always on; verbosity is a separate knob
        trim_tool_desc_chars: cfg.trim_tool_desc_chars,
        trim_toolresult_head: cfg.trim_toolresult_head,
        trim_head_haiku: cfg.trim_head_haiku,
        trim_head_harness: cfg.trim_head_harness,
        trim_toolresult_tail: cfg.trim_toolresult_tail,
        trim_ws_enabled: cfg.trim_ws_enabled,
        trim_free_target: cfg.trim_free_target.clone(),
        trim_strip_thinking: cfg.trim_strip_thinking,
        trim_fence_requires_code: cfg.trim_fence_requires_code,
        trim_arrow_density_min: cfg.trim_arrow_density_min,
        corpus_max_rows: cfg.corpus_max_rows,
        retry_enabled: cfg.retry_enabled,
        retry_max_attempts: cfg.retry_max_attempts,
        retry_base_delay_ms: cfg.retry_base_delay_ms,
        retry_max_delay_ms: cfg.retry_max_delay_ms,
        throttle_enabled: cfg.throttle_enabled,
        throttle_rate_per_sec: cfg.throttle_rate_per_sec,
        throttle_burst: cfg.throttle_burst,
    };
    match llm_proxy::start_embedded_with(pcfg, cfg.port, false, &cfg.bind_address) {
        Ok(control) => {
            LAST_PORT.store(cfg.port, Ordering::Relaxed);

            // Why: the quota watcher may have started before the proxy was
            // running (on daemon boot in App::new). Its earlier call to
            // set_quota_poll_enabled(true) would have hit a None guard and
            // been silently dropped. Seeding the flag here closes that
            // ordering gap. Do this through the newly-created control before
            // publishing it: `guard` already holds CONTROL, so routing through
            // `set_quota_poll_enabled` here would attempt to lock CONTROL
            // again and deadlock every proxy start.
            control.set_quota_poll_enabled(quota_watcher::is_running());
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
        control.set_trim_tool_desc_chars(cfg.trim_tool_desc_chars);
        control.set_trim_toolresult_head(cfg.trim_toolresult_head);
        control.set_trim_head_haiku(cfg.trim_head_haiku);
        control.set_trim_head_harness(cfg.trim_head_harness);
        control.set_trim_toolresult_tail(cfg.trim_toolresult_tail);
        control.set_trim_ws_enabled(cfg.trim_ws_enabled);
        control.set_trim_free_target(&cfg.trim_free_target);
        control.set_trim_strip_thinking(cfg.trim_strip_thinking);
        control.set_trim_fence_requires_code(cfg.trim_fence_requires_code);
        control.set_trim_arrow_density_min(cfg.trim_arrow_density_min);
        control.set_corpus_capture_enabled(cfg.corpus_capture);
        control.set_retry_enabled(cfg.retry_enabled);
        control.set_retry_max_attempts(cfg.retry_max_attempts);
        control.set_retry_base_delay_ms(cfg.retry_base_delay_ms);
        control.set_retry_max_delay_ms(cfg.retry_max_delay_ms);
        control.set_throttle_enabled(cfg.throttle_enabled);
        control.set_throttle_rate(cfg.throttle_rate_per_sec, cfg.throttle_burst);

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

/// Current proxy run id, or None if the proxy is off.
pub fn get_run_id() -> Option<u64> {
    CONTROL.lock().unwrap().as_ref().map(|c| c.run_id())
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

/// Latest Anthropic quota snapshot from the running proxy, if any.
pub fn get_quota() -> Option<QuotaSnapshot> {
    CONTROL.lock().unwrap().as_ref().and_then(|c| c.quota())
}

/// Run the Anthropic trim A/B bench with the currently-live native knobs.
/// Returns Ok(None) when the corpus has no anthropic rows. Blocking (caller
/// should run it off the UI thread).
pub fn run_bench() -> Result<Option<llm_proxy::bench::BenchResult>, String> {
    let knobs = {
        let guard = CONTROL.lock().unwrap();
        match guard.as_ref() {
            Some(c) => c.native_knobs(),
            None => return Err("proxy not running".into()),
        }
    };
    let db = llm_proxy::config::config_dir().join("proxy.db");
    let out = llm_proxy::bench::run_anthropic_bench(&db, &knobs)?;
    if let Some(ref result) = out {
        // analytic headroom: how much more work fits under the same cap.
        let headroom_pct = if result.avg_trim_pct >= 99.0 {
            f64::INFINITY
        } else {
            (1.0 / (1.0 - result.avg_trim_pct / 100.0) - 1.0) * 100.0
        };
        let headroom_pct = if headroom_pct.is_finite() {
            headroom_pct
        } else {
            9999.0
        };
        let knobs_json = serde_json::to_string(&knobs).unwrap_or_else(|_| "{}".to_string());
        // Re-acquire the control handle to record (don't hold the lock across the bench run).
        if let Ok(guard) = CONTROL.lock()
            && let Some(c) = guard.as_ref()
        {
            c.record_bench_run(result, headroom_pct, &knobs_json);
        }
    }
    Ok(out)
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

/// Enable or disable request compression via the native engine.
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

/// Enable or disable the OAuth quota poller on the embedded proxy.
/// Silent no-op when the proxy is not running.
pub fn set_quota_poll_enabled(enabled: bool) {
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        c.set_quota_poll_enabled(enabled);
    }
}

/// Current free/cheap-tier target string ("" if unset), or None if the proxy is off.
pub fn get_trim_free_target() -> Option<String> {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.trim_free_target())
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

/// Set the designated free/cheap-tier target ("" disables the light-trim feature).
/// Persisted to settings.json so the choice survives daemon restarts. Returns false
/// if the proxy is off (the value is persisted regardless of the running state).
pub fn set_free_target(target: &str) -> bool {
    // Persist the selection so it is restored on the next start.
    if let Ok(mut settings) = llm_proxy::config::load_settings() {
        settings.trim_free_target = target.to_string();
        let _ = llm_proxy::config::save_settings(&settings);
    }

    // Apply to the live proxy if it is running.
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| {
            c.set_trim_free_target(target);
            true
        })
        .unwrap_or(false)
}
