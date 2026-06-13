//! llm-proxy as a library.
//!
//! Exposes the router and routing state so the proxy can run either as a
//! standalone binary (`main.rs`) or **embedded inside another process** (the
//! mhd daemon) via [`start_embedded`]. When embedded, model switching is done
//! directly on the shared [`AppState`] — no self-HTTP needed.

pub mod config;
pub mod handlers;
pub mod providers;
pub mod state;
pub mod transform;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use tower_http::cors::CorsLayer;

pub use config::{Config, Secrets, Settings};
pub use state::{AppState, Target, Tier};

/// Build the Axum router for a given shared state.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/messages", post(handlers::post_messages))
        .route(
            "/v1/chat/completions",
            post(handlers::post_chat_completions),
        )
        .route("/set_model/{slot}", get(handlers::set_model))
        .route("/config", get(handlers::get_config))
        .route("/debug", get(handlers::toggle_debug))
        .route("/health", get(handlers::health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Load config from `~/.config/mhd/llm-proxy/settings.json` +
/// `secrets.json`, applying env-var fallbacks for the API keys.
pub fn load_config() -> anyhow::Result<Config> {
    let mut cfg = config::load()?;
    if cfg.anthropic_key.is_empty() {
        if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            cfg.anthropic_key = k;
        }
    }
    if cfg.upstream_key.is_empty() {
        if let Ok(k) = std::env::var("SVA_API_KEY").or_else(|_| std::env::var("OPENCODE_API_KEY")) {
            cfg.upstream_key = k;
        }
    }
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
        if ok && self.persist {
            if let Err(e) = config::save(&self.state.to_config()) {
                tracing::warn!("failed to persist proxy config: {e}");
            }
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

    /// Set the debug log level on the embedded state (no disk persist).
    pub fn set_log_level(&self, level: &str) {
        *self.state.log_level.write().unwrap() = crate::state::DebugLevel::parse(level);
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
    start_embedded_with(cfg, port, true)
}

/// Start the proxy embedded in the current process from an explicit config,
/// listening on `127.0.0.1:<port>`. Spawns a dedicated thread with its own Tokio
/// runtime and blocks only until the listener is bound (or fails).
///
/// `persist` controls whether runtime target changes are written back to the
/// proxy's own config file. Callers that own the config elsewhere (the daemon)
/// pass `false`.
pub fn start_embedded_with(
    mut cfg: Config,
    port: u16,
    persist: bool,
) -> std::io::Result<ProxyControl> {
    // Env-var fallbacks for keys (handy in dev / CI).
    if cfg.anthropic_key.is_empty() {
        if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            cfg.anthropic_key = k;
        }
    }
    if cfg.upstream_key.is_empty() {
        if let Ok(k) = std::env::var("SVA_API_KEY").or_else(|_| std::env::var("OPENCODE_API_KEY")) {
            cfg.upstream_key = k;
        }
    }

    let state = AppState::from_config(&cfg);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // Reports whether the listener bound successfully.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

    let server_state = state.clone();
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
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
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
