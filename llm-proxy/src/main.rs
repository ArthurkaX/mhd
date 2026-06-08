//! llm-proxy — standalone binary.
//!
//! Thin wrapper over the `llm_proxy` library. Routes Claude Code requests by
//! model tier (opus / sonnet / haiku) to either the official Anthropic API or
//! an OpenAI-compatible upstream gateway. See the library crate for details.
//!
//! Config and keys: ~/.config/mhd/llm-proxy/config.toml
//!
//! The same proxy can be embedded directly in the mhd daemon via
//! `llm_proxy::start_embedded`.

use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use llm_proxy::{build_router, config, load_config, AppState};

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
    let cfg = load_config()?;

    if cfg.anthropic_key.is_empty() {
        tracing::warn!("anthropic_key not set — native passthrough relies on Claude Code's own auth");
    }
    if cfg.upstream_key.is_empty() {
        tracing::warn!("upstream_key not set — non-native tiers will fail");
    }

    let state = AppState::from_config(&cfg);
    *state.debug_dump.write().unwrap() = cli.debug;

    let app = build_router(state);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    tracing::info!(
        "🚀 llm-proxy v{} listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        addr
    );
    tracing::info!("   config:  {}", config::config_path().display());
    tracing::info!("   opus    → {}", cfg.opus_target);
    tracing::info!("   sonnet  → {}", cfg.sonnet_target);
    tracing::info!("   haiku   → {}", cfg.haiku_target);
    tracing::info!("   upstream: {}", cfg.upstream_base_url);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
