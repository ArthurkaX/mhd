//! Embedded LLM proxy control.
//!
//! The proxy runs **in-process** (inside the mhd daemon) via the `llm_proxy`
//! library — no separate executable. We hold a [`ProxyControl`] handle and
//! switch model routing by writing the shared state directly; the change takes
//! effect on the proxy's next request, so in-flight Claude Code work is never
//! interrupted.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

use llm_proxy::ClientKind;
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
    // The listener runs only while at least one client is enabled. This guard
    // keeps `start` (boot path, reload, toggle) from ever opening a port when
    // the user has switched every client off.
    if !any_client_enabled(cfg) {
        return false;
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
        codex_target: cfg.codex.clone(),
        // Per-client switches from the daemon config, defaulting on so an
        // upgrade never silently kills a route the user already relies on.
        client_claude_code_enabled: cfg.client_claude_code_enabled,
        client_codex_enabled: cfg.client_codex_enabled,
        client_openai_enabled: cfg.client_openai_enabled,
        trim_codex_enabled: cfg.trim_codex_enabled,
        trim_openai_enabled: cfg.trim_openai_enabled,
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

            // Why: the Anthropic OAuth quota poller follows the Claude Code
            // client switch, not the Codex watcher thread. The proxy may start
            // before or after App::new seeds the watcher; seeding the flag here
            // closes that ordering gap. Do this through the newly-created
            // control before publishing it: `guard` already holds CONTROL, so
            // routing through `set_quota_poll_enabled` here would attempt to
            // lock CONTROL again and deadlock every proxy start.
            control.set_quota_poll_enabled(cfg.client_claude_code_enabled);
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
/// is off and enabled is false. Also reconciles the per-client switches on the
/// live state and drives the background usage pollers from them.
pub fn reload(cfg: &LlmProxyConfig) -> bool {
    let port_changed;
    {
        let guard = CONTROL.lock().unwrap();
        if let Some(ref control) = *guard {
            control.set_target("opus", &cfg.opus);
            control.set_target("sonnet", &cfg.sonnet);
            control.set_target("haiku", &cfg.haiku);
            control.set_target("fable", &cfg.fable);
            control.set_codex_target(&cfg.codex);
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

            // Apply the per-client switches so an edited settings.json takes
            // effect without a restart.
            control.set_client_enabled(ClientKind::ClaudeCode, cfg.client_claude_code_enabled);
            control.set_client_enabled(ClientKind::Codex, cfg.client_codex_enabled);
            control.set_client_enabled(ClientKind::OpenAi, cfg.client_openai_enabled);
            control.set_trim_enabled_for(ClientKind::Codex, cfg.trim_codex_enabled);
            control.set_trim_enabled_for(ClientKind::OpenAi, cfg.trim_openai_enabled);

            port_changed = cfg.port != LAST_PORT.load(Ordering::Relaxed);
        } else {
            drop(guard);
            let started = if cfg.enabled { start(cfg) } else { false };
            // Pollers follow their client even when the proxy is off (the Codex
            // thread runs in-daemon; the OAuth poll flag is a no-op until the
            // proxy starts, and start() seeds it from the same switch).
            reconcile_pollers(cfg);
            return started;
        }
    }
    if port_changed {
        stop();
        let started = start(cfg);
        // start() seeds the OAuth poll flag from the Claude Code switch, but the
        // Codex thread still needs a reconcile after the restart.
        reconcile_pollers(cfg);
        return started;
    }
    // Run the listener rule after any client-switch change in the config, and
    // drive the pollers. Done after dropping the lock: both call into functions
    // that lock CONTROL.
    reconcile_listener(cfg);
    reconcile_pollers(cfg);
    true
}

/// Current per-tier targets (opus, sonnet, haiku, fable). None if the proxy is off.
pub fn get_targets() -> Option<(String, String, String, String)> {
    CONTROL.lock().unwrap().as_ref().map(|c| c.targets())
}

/// Current gpt-5.4 Codex override target, or None if the proxy is off.
pub fn get_codex_target() -> Option<String> {
    CONTROL.lock().unwrap().as_ref().map(|c| c.codex_target())
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

/// Freshest Anthropic quota reading with its age, from either recording path.
pub fn get_quota_fresh() -> Option<(QuotaSnapshot, std::time::Duration)> {
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|c| c.quota_fresh())
}

/// Ask the quota poller for a fresh reading (throttled by the poller itself).
/// No-op when the proxy is not running.
pub fn request_quota_refresh() {
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        c.request_quota_refresh();
    }
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

/// Set and persist the gpt-5.4 Codex override. The change applies to the next
/// matching request; other Codex models remain native.
pub fn set_codex_target(target: &str) -> bool {
    if let Ok(mut settings) = llm_proxy::config::load_settings() {
        settings.codex_target = target.to_string();
        let _ = llm_proxy::config::save_settings(&settings);
    }

    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| {
            c.set_codex_target(target);
            true
        })
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

// ── Per-client switches ────────────────────────────────────────────────

/// Whether at least one client is enabled. The embedded listener runs only
/// while this is true (see [`reconcile_listener`]).
pub fn any_client_enabled(cfg: &LlmProxyConfig) -> bool {
    cfg.client_claude_code_enabled || cfg.client_codex_enabled || cfg.client_openai_enabled
}

/// The listener lifetime rule, implemented in exactly one place: the proxy
/// listens while at least one client is enabled and stops when all are
/// disabled. Call after any client-switch change. All-on = running, all-off
/// = not listening at all.
pub fn reconcile_listener(cfg: &LlmProxyConfig) -> bool {
    let running = is_running();
    if any_client_enabled(cfg) && !running {
        start(cfg)
    } else if !any_client_enabled(cfg) && running {
        stop();
        false
    } else {
        running
    }
}

/// Drive the background usage pollers from the per-client switches:
/// - The Anthropic OAuth quota poller (gated by `quota_poll_enabled` on the
///   proxy state) runs only when Claude Code is enabled.
/// - The in-daemon Codex usage-polling thread runs only when Codex is enabled.
pub fn reconcile_pollers(cfg: &LlmProxyConfig) {
    set_quota_poll_enabled(cfg.client_claude_code_enabled);
    if cfg.client_codex_enabled {
        quota_watcher::start();
    } else {
        quota_watcher::stop();
    }
}

/// Set a client's per-client switch. Persisted to settings.json so the choice
/// survives daemon restarts (mirroring `set_target`), then applied to the live
/// proxy. The listener and the background usage pollers are reconciled by the
/// caller once the daemon config cache has been updated.
pub fn set_client_enabled(client: ClientKind, on: bool) -> bool {
    // Persist the selection so it is restored on the next start.
    if let Ok(mut settings) = llm_proxy::config::load_settings() {
        match client {
            ClientKind::ClaudeCode => settings.client_claude_code_enabled = on,
            ClientKind::Codex => settings.client_codex_enabled = on,
            ClientKind::OpenAi => settings.client_openai_enabled = on,
        }
        let _ = llm_proxy::config::save_settings(&settings);
    }

    // Apply to the live proxy if it is running.
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| {
            c.set_client_enabled(client, on);
            true
        })
        .unwrap_or(false)
}

/// Current state of a client's switch. Reads the live proxy when running and
/// falls back to the persisted settings.json otherwise (e.g. all clients off).
pub fn client_enabled(client: ClientKind) -> bool {
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        return c.client_enabled(client);
    }
    llm_proxy::config::load_settings()
        .ok()
        .map_or(false, |s| match client {
            ClientKind::ClaudeCode => s.client_claude_code_enabled,
            ClientKind::Codex => s.client_codex_enabled,
            ClientKind::OpenAi => s.client_openai_enabled,
        })
}

/// Set a client's trim switch. Persisted to settings.json so the choice
/// survives daemon restarts (mirroring `set_target`), then applied to the live
/// proxy. Each `ClientKind` has its own flag: Claude Code keeps the
/// `trim_enabled` master under that name so existing settings.json files keep
/// working, OpenAI and Codex use their own keys.
///
/// Currently unused: the only trim UI is `Settings -> LLM Trim`, which writes
/// settings.json directly and lets the file watcher apply it through `reload`.
/// Kept because it is the write half of `trim_enabled_for` and the natural
/// entry point for any future non-file caller.
#[allow(dead_code)]
pub fn set_trim_enabled_for(client: ClientKind, on: bool) -> bool {
    // Persist the selection so it is restored on the next start.
    if let Ok(mut settings) = llm_proxy::config::load_settings() {
        match client {
            ClientKind::Codex => settings.trim_codex_enabled = on,
            ClientKind::ClaudeCode => settings.trim_enabled = on,
            // The on-disk key is Option<bool>; a bare `on` would be a type error.
            ClientKind::OpenAi => settings.trim_openai_enabled = Some(on),
        }
        let _ = llm_proxy::config::save_settings(&settings);
    }

    // Apply to the live proxy if it is running.
    CONTROL
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| {
            c.set_trim_enabled_for(client, on);
            true
        })
        .unwrap_or(false)
}

/// Current trim state for a client. Reads the live proxy when running and
/// falls back to the persisted settings.json otherwise. Unused for the same
/// reason as `set_trim_enabled_for` above.
#[allow(dead_code)]
pub fn trim_enabled_for(client: ClientKind) -> bool {
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        return c.trim_enabled_for(client);
    }
    llm_proxy::config::load_settings()
        .ok()
        .map_or(false, |s| match client {
            ClientKind::Codex => s.trim_codex_enabled,
            ClientKind::ClaudeCode => s.trim_enabled,
            ClientKind::OpenAi => s.trim_openai(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listener lifetime rule: running iff at least one client is enabled,
    /// regardless of the proxy `enabled` master (the per-client switches now
    /// drive the listener). Uses port 0 so the test binds an ephemeral port and
    /// never collides with a user's real proxy on 3456.
    ///
    /// `#[ignore]` on purpose: `start` hardcodes `db_log_enabled: true` with no
    /// seam to turn it off, so running this spawns a real embedded server that
    /// opens — and migrates — the user's live `proxy.db`. Racing a schema
    /// migration against a running daemon is not worth one `if`'s worth of
    /// coverage; the decision predicate itself is covered purely below. Run
    /// deliberately with `cargo test -p mhd-daemon -- --ignored` when the
    /// daemon is stopped.
    #[test]
    #[ignore = "spawns a real proxy and migrates the user's live proxy.db"]
    fn listener_runs_iff_at_least_one_client_is_enabled() {
        let mut cfg = LlmProxyConfig::default();
        cfg.port = 0;

        // Start from a clean slate.
        stop();
        assert!(!is_running());

        // All clients on (default) → running.
        assert!(reconcile_listener(&cfg));
        assert!(is_running());

        // One client off keeps the listener running.
        let mut one_off = cfg.clone();
        one_off.client_claude_code_enabled = false;
        assert!(reconcile_listener(&one_off));
        assert!(is_running());

        // All clients off → listener stops and stays stopped (start is a no-op).
        let mut all_off = cfg.clone();
        all_off.client_claude_code_enabled = false;
        all_off.client_codex_enabled = false;
        all_off.client_openai_enabled = false;
        assert!(!reconcile_listener(&all_off));
        assert!(!is_running());
        assert!(!start(&all_off));
        assert!(!is_running());

        // One client back on → listener resumes.
        all_off.client_codex_enabled = true;
        assert!(reconcile_listener(&all_off));
        assert!(is_running());

        // Clean up so other tests never observe a running proxy.
        stop();
        assert!(!is_running());
    }

    #[test]
    fn any_client_enabled_reflects_the_client_switches() {
        let cfg = LlmProxyConfig::default();
        assert!(any_client_enabled(&cfg));

        let mut off = cfg.clone();
        off.client_claude_code_enabled = false;
        off.client_codex_enabled = false;
        off.client_openai_enabled = false;
        assert!(!any_client_enabled(&off));

        off.client_codex_enabled = true;
        assert!(any_client_enabled(&off));

        // The OpenAI switch participates like the other two: with it as the
        // only one left on, the listener rule stays satisfied.
        off.client_codex_enabled = false;
        off.client_openai_enabled = true;
        assert!(any_client_enabled(&off));
        assert!(!off.client_claude_code_enabled && !off.client_codex_enabled);
    }
}
