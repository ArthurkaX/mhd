//! llm-proxy as a library.
//!
//! Exposes the router and routing state so the proxy can run either as a
//! standalone binary (`main.rs`) or **embedded inside another process** (the
//! mhd daemon) via [`start_embedded`]. When embedded, model switching is done
//! directly on the shared [`AppState`] — no self-HTTP needed.

pub mod bench;
pub mod config;
pub mod db_log;
pub mod handlers;
pub mod measure;
pub mod native_trim;
pub mod prefix;
pub mod providers;
pub mod state;
pub mod throttle;
pub mod transform;
pub mod trim;
pub mod tune;
pub mod vision;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use tower_http::catch_panic::CatchPanicLayer;

pub use config::{
    Config, ModelRef, Provider, ResolvedModelEndpoint, Secrets, Settings, normalize_endpoint,
    normalize_vision_endpoint, resolve_model_endpoint,
};
pub use db_log::{LogEvent, QuotaSnapshot};
pub use state::{AppState, Target, Tier, TraceEntry, VisionTraceEntry};

/// Build the Axum router for a given shared state.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(handlers::post_messages))
        .route(
            "/v1/chat/completions",
            post(handlers::post_chat_completions),
        )
        .route("/v1/models", get(handlers::get_models))
        .route("/set_model/{slot}", post(handlers::set_model))
        .route("/config", get(handlers::get_config))
        .route("/stats/routes", get(handlers::get_route_stats))
        .route("/debug", post(handlers::toggle_debug))
        .route("/health", get(handlers::health))
        .with_state(state)
        .layer(CatchPanicLayer::new())
}

/// Apply environment-variable fallbacks for API keys not set in the config.
/// Checks `ANTHROPIC_API_KEY`, `SVA_API_KEY`, and `OPENCODE_API_KEY`.
fn apply_env_fallbacks(cfg: &mut Config) {
    if cfg.anthropic_key.is_empty()
        && let Ok(k) = std::env::var("ANTHROPIC_API_KEY")
    {
        cfg.anthropic_key = k;
    }
    if cfg.upstream_key.is_empty()
        && let Ok(k) = std::env::var("SVA_API_KEY").or_else(|_| std::env::var("OPENCODE_API_KEY"))
    {
        cfg.upstream_key = k;
    }
}

/// Load config from `~/.config/mhd/llm-proxy/settings.json` +
/// `secrets.json`, applying env-var fallbacks for the API keys.
pub fn load_config() -> anyhow::Result<Config> {
    let mut cfg = config::load()?;
    apply_env_fallbacks(&mut cfg);
    Ok(cfg)
}

// ── In-process embedding ────────────────────────────────────────────────

/// Handle to an embedded proxy server running on its own thread + runtime.
///
/// Dropping it (or calling [`ProxyControl::stop`]) shuts the server down
/// gracefully. Routing changes are applied directly on the shared state and
/// take effect on the proxy's next request — in-flight requests are untouched.
pub struct ProxyControl {
    state: Arc<AppState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
    /// When true, target changes are written to the proxy's own config file.
    /// Embedded callers (the daemon) keep their config elsewhere, so they set
    /// this to false.
    persist: bool,
}

impl ProxyControl {
    /// Set a tier's routing target. `slot` is "opus"/"sonnet"/"haiku"/"fable"; `target`
    /// is "native" or an upstream model id. Persists to the proxy config.
    pub fn set_target(&self, slot: &str, target: &str) -> bool {
        let ok = self.state.set_target(slot, Target::parse(target));
        if ok
            && self.persist
            && let Err(e) = config::save(&self.state.to_config())
        {
            tracing::warn!("failed to persist proxy config: {e}");
        }
        ok
    }

    /// Current (opus, sonnet, haiku, fable) targets.
    pub fn targets(&self) -> (String, String, String, String) {
        (
            self.state.target_for(Tier::Opus).as_str().to_string(),
            self.state.target_for(Tier::Sonnet).as_str().to_string(),
            self.state.target_for(Tier::Haiku).as_str().to_string(),
            self.state.target_for(Tier::Fable).as_str().to_string(),
        )
    }

    /// The current proxy run id (process-start ms). Combine with a TraceEntry's
    /// `seq` to identify a captured request `(run_id, seq)`.
    pub fn run_id(&self) -> u64 {
        self.state.run_id
    }

    /// Set the debug log level on the embedded state (no disk persist).
    pub fn set_log_level(&self, level: &str) {
        *self
            .state
            .log_level
            .write()
            .unwrap_or_else(|e| e.into_inner()) = crate::state::DebugLevel::parse(level);
    }

    /// Current debug log level.
    pub fn log_level(&self) -> String {
        self.state
            .log_level
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_str()
            .to_string()
    }

    /// Enable or disable native request compression.
    pub fn set_trim_enabled(&self, enabled: bool) {
        *self
            .state
            .trim_enabled
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Whether trim is currently enabled.
    pub fn is_trim_enabled(&self) -> bool {
        *self
            .state
            .trim_enabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Set the max chars per tool description for the native trim engine.
    pub fn set_trim_tool_desc_chars(&self, v: usize) {
        *self
            .state
            .trim_tool_desc_chars
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the head budget for tool_result blocks in the native trim engine.
    pub fn set_trim_toolresult_head(&self, v: usize) {
        *self
            .state
            .trim_toolresult_head
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the head budget for tool_result blocks on native Haiku requests.
    pub fn set_trim_head_haiku(&self, v: usize) {
        *self
            .state
            .trim_head_haiku
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the head budget for tool_result blocks on OpenAI-harness requests.
    pub fn set_trim_head_harness(&self, v: usize) {
        *self
            .state
            .trim_head_harness
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the tail budget for tool_result blocks in the native trim engine.
    pub fn set_trim_toolresult_tail(&self, v: usize) {
        *self
            .state
            .trim_toolresult_tail
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Enable or disable whitespace compression in the native trim engine.
    pub fn set_trim_ws_enabled(&self, enabled: bool) {
        *self
            .state
            .trim_ws_enabled
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Set the designated free/cheap-tier target (target string, e.g. "native"
    /// or a model id). Requests resolving to this exact target get the light,
    /// non-lossy trim profile. Empty string disables the feature.
    pub fn set_trim_free_target(&self, target: &str) {
        *self
            .state
            .trim_free_target
            .write()
            .unwrap_or_else(|e| e.into_inner()) = target.to_string();
    }

    /// Current free/cheap-tier target string. Empty means the feature is off.
    pub fn trim_free_target(&self) -> String {
        self.state
            .trim_free_target
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Enable or disable stripping of thinking blocks from old assistant turns.
    pub fn set_trim_strip_thinking(&self, enabled: bool) {
        *self
            .state
            .trim_strip_thinking
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Set whether Layer 2 fence protection requires fenced content to look
    /// code-like. Default: true (new behavior). Set false to revert to the
    /// legacy behavior where a fence alone protects.
    pub fn set_trim_fence_requires_code(&self, requires_code: bool) {
        *self
            .state
            .trim_fence_requires_code
            .write()
            .unwrap_or_else(|e| e.into_inner()) = requires_code;
    }

    /// Set the minimum glyph density threshold for the arrow-driven diagram
    /// detector. Default: 0.01 (1%). Live-tunable via settings.json watcher.
    pub fn set_trim_arrow_density_min(&self, val: f64) {
        *self
            .state
            .trim_arrow_density_min
            .write()
            .unwrap_or_else(|e| e.into_inner()) = val;
    }

    /// Enable or disable transparent retry on Anthropic 429/529.
    /// Live-tunable via settings.json watcher.
    pub fn set_retry_enabled(&self, enabled: bool) {
        *self
            .state
            .retry_enabled
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Set the total retry attempts (including the first) for 429/529 backoff.
    pub fn set_retry_max_attempts(&self, v: usize) {
        *self
            .state
            .retry_max_attempts
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the base backoff delay (ms) for the 429/529 retry loop.
    pub fn set_retry_base_delay_ms(&self, v: u64) {
        *self
            .state
            .retry_base_delay_ms
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Set the per-wait backoff cap (ms) for the 429/529 retry loop.
    pub fn set_retry_max_delay_ms(&self, v: u64) {
        *self
            .state
            .retry_max_delay_ms
            .write()
            .unwrap_or_else(|e| e.into_inner()) = v;
    }

    /// Enable or disable the outbound token-bucket throttle on the native
    /// Anthropic path. Live-tunable via settings.json watcher.
    pub fn set_throttle_enabled(&self, enabled: bool) {
        *self
            .state
            .throttle_enabled
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Set the throttle refill rate (requests/sec) and burst capacity,
    /// applying both to the RwLocks AND the live token bucket so the new
    /// rate takes effect immediately. Live-tunable via settings.json watcher.
    pub fn set_throttle_rate(&self, rate_per_sec: f64, burst: f64) {
        *self
            .state
            .throttle_rate_per_sec
            .write()
            .unwrap_or_else(|e| e.into_inner()) = rate_per_sec;
        *self
            .state
            .throttle_burst
            .write()
            .unwrap_or_else(|e| e.into_inner()) = burst;
        self.state.throttle_bucket.set_rate(burst, rate_per_sec);
    }

    /// Snapshot the live native-trim knobs (same reads the request path uses).
    pub fn native_knobs(&self) -> crate::native_trim::NativeKnobs {
        crate::native_trim::NativeKnobs {
            tool_max_desc_chars: *self
                .state
                .trim_tool_desc_chars
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_head: *self
                .state
                .trim_toolresult_head
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_tail: *self
                .state
                .trim_toolresult_tail
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ws_enabled: *self
                .state
                .trim_ws_enabled
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            strip_thinking: *self
                .state
                .trim_strip_thinking
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_fence_requires_code: *self
                .state
                .trim_fence_requires_code
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            tool_result_arrow_density_min: *self
                .state
                .trim_arrow_density_min
                .read()
                .unwrap_or_else(|e| e.into_inner()),
            ..Default::default()
        }
    }

    /// Enable or disable replay-corpus capture (stores full pre-trim request bodies in proxy.db).
    pub fn set_corpus_capture_enabled(&self, enabled: bool) {
        *self
            .state
            .corpus_capture_enabled
            .write()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Whether replay-corpus capture is currently enabled.
    pub fn is_corpus_capture_enabled(&self) -> bool {
        *self
            .state
            .corpus_capture_enabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Snapshot of recent routing decisions.
    pub fn trace(&self) -> Vec<TraceEntry> {
        self.state.trace_snapshot()
    }

    /// Latest Anthropic quota snapshot, if any has been recorded.
    pub fn quota(&self) -> Option<crate::db_log::QuotaSnapshot> {
        self.state.get_quota()
    }

    /// Record a vision screenshot request in the live trace buffer.
    pub fn push_vision_trace(&self, entry: VisionTraceEntry) {
        self.state.push_vision_trace(entry);
    }

    /// Snapshot of recent vision screenshot requests.
    pub fn vision_trace(&self) -> Vec<VisionTraceEntry> {
        self.state.vision_trace_snapshot()
    }

    /// Log a structured event to the SQLite database (if debug logging is on).
    pub fn log_event(&self, event: crate::db_log::LogEvent) {
        self.state.log_event(event);
    }

    /// Enable or disable the database log at runtime.
    pub fn set_db_log_enabled(&self, enabled: bool) {
        self.state.set_db_log_enabled(enabled);
    }

    /// Check whether the database log is currently enabled.
    pub fn is_db_log_enabled(&self) -> bool {
        self.state.is_db_log_enabled()
    }

    /// Write a free-text note to the `notes` table in proxy.db.
    /// Best-effort: no-ops if the log is disabled or the write fails.
    pub fn log_note(&self, text: &str) {
        self.state.log_note(text);
    }

    /// Record a finished Bench run to proxy.db history.
    pub fn record_bench_run(
        &self,
        result: &crate::bench::BenchResult,
        headroom_pct: f64,
        knobs_json: &str,
    ) {
        self.state
            .record_bench_run(result, headroom_pct, knobs_json);
    }

    /// Shut the server down gracefully and join its thread.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
impl Drop for ProxyControl {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the proxy embedded in the current process, loading config from disk
/// (`~/.config/mhd/llm-proxy/settings.json` + `secrets.json`) and persisting
/// target changes there.
pub fn start_embedded(port: u16) -> std::io::Result<ProxyControl> {
    let cfg = load_config().map_err(|e| std::io::Error::other(e.to_string()))?;
    start_embedded_with(cfg, port, true, "127.0.0.1")
}

/// Start the proxy embedded in the current process from an explicit config,
/// listening on `<bind_address>:<port>`. Spawns a dedicated thread with its own Tokio
/// runtime and blocks only until the listener is bound (or fails).
///
/// `persist` controls whether runtime target changes are written back to the
/// proxy's own config file. Callers that own the config elsewhere (the daemon)
/// pass `false`.
pub fn start_embedded_with(
    mut cfg: Config,
    port: u16,
    persist: bool,
    bind_address: &str,
) -> std::io::Result<ProxyControl> {
    // Env-var fallbacks for keys (handy in dev / CI).
    apply_env_fallbacks(&mut cfg);

    let state = AppState::from_config(&cfg);

    // Open the database log at startup when configured (independent of verbosity).
    if cfg.db_log_enabled {
        state.set_db_log_enabled(true);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // Reports whether the listener bound successfully.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

    let server_state = state.clone();
    let bind_addr = bind_address.to_string();
    let join = std::thread::Builder::new()
        .name("llm-proxy-server".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };

            rt.block_on(async move {
                let ip: std::net::IpAddr = if bind_addr.is_empty() {
                    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                } else {
                    match bind_addr.parse() {
                        Ok(ip) => ip,
                        Err(_) => {
                            let _ = ready_tx.send(Err(std::io::Error::other(format!(
                                "invalid bind address: {bind_addr}"
                            ))));
                            return;
                        }
                    }
                };
                let addr = std::net::SocketAddr::new(ip, port);
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));

                let app = build_router(server_state);
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });
        })?;

    // Wait for the bind result.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ProxyControl {
            state,
            shutdown: Some(shutdown_tx),
            join: Some(join),
            persist,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::other(
            "proxy server thread died during startup",
        )),
    }
}
