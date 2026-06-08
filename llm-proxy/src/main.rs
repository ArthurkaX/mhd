//! llm-proxy — динамический LLM-прокси для Claude Code.
//!
//! Единственная точка входа для Claude Code. Маршрутизирует запросы по модели:
//!   - opus            → официальный Anthropic API (passthrough)
//!   - sonnet / haiku  → OpenAI-совместимый апстрим (SVA / Bifrost)
//! Какая модель апстрима стоит на sonnet/haiku — переключается на лету.
//!
//! Конфиг и ключи: ~/.config/mhd/llm-proxy/config.toml
//!
//! Использование:
//!   llm-proxy                    # порт 3456, конфиг из ~/.config/mhd/llm-proxy
//!   llm-proxy --port 8080        # другой порт
//!
//! Переключение модели на лету:
//!   curl "http://localhost:3456/set_model/sonnet?id=sva-opencode/qwen3.7-max"
//!   curl "http://localhost:3456/set_model/haiku?id=sva-ollama/deepseek-v4-flash"
//!   curl http://localhost:3456/config

mod config;
mod handlers;
mod providers;
pub mod state;
pub mod transform;

use std::net::SocketAddr;

use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

use crate::handlers::*;
use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(name = "llm-proxy", version, about = "Dynamic LLM proxy for Claude Code")]
struct Cli {
    /// Listen port (default: 3456)
    #[arg(short, long, default_value = "3456")]
    port: u16,

    /// Listen address (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Enable debug request/response dump
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Load config from ~/.config/mhd/llm-proxy/config.toml (created if missing).
    let mut cfg = config::load()?;

    // Env vars override empty config values (handy for quick runs / CI).
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

    if cfg.anthropic_key.is_empty() {
        tracing::warn!("anthropic_key not set — opus passthrough will fail");
    }
    if cfg.upstream_key.is_empty() {
        tracing::warn!("upstream_key not set — sonnet/haiku routing will fail");
    }

    let state = AppState::from_config(&cfg);
    *state.debug_dump.write().await = cli.debug;

    let app = Router::new()
        .route("/v1/messages", post(post_messages))
        .route("/v1/chat/completions", post(post_chat_completions))
        .route("/set_model/{slot}", get(set_model))
        .route("/config", get(get_config))
        .route("/debug", get(toggle_debug))
        .route("/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    tracing::info!(
        "🚀 llm-proxy v{} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        addr
    );
    tracing::info!("   config:  {}", config::config_path().display());
    tracing::info!("   opus           → official Anthropic");
    tracing::info!("   sonnet         → {} @ {}", cfg.sonnet_model, cfg.upstream_base_url);
    tracing::info!("   haiku          → {} @ {}", cfg.haiku_model, cfg.upstream_base_url);
    tracing::info!(
        "   switch:  curl \"http://{}/set_model/sonnet?id=sva-opencode/qwen3.7-max\"",
        addr
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
