#![forbid(unsafe_code)]

//! panel-api: authentication, business logic, and MCP server. Runs
//! unprivileged; all host operations are delegated to panel-agent over a
//! Unix socket (see `agent_client`).

mod auth;
mod config;
mod db;
mod error;
mod mcp;
mod routes;
mod services;
mod state;

use clap::Parser;
use config::ApiConfig;
use state::AppState;

#[derive(Parser, Debug)]
#[command(name = "panel-api")]
struct Args {
    /// Run as an MCP server over stdio instead of starting the HTTP API.
    #[arg(long)]
    mcp_stdio: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = ApiConfig::from_env()?;

    if args.mcp_stdio {
        // Keep stdout clean for JSON-RPC; logs go to stderr.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
            )
            .json()
            .init();
    }

    let pool = db::connect(&config.database_url).await?;
    db::run_migrations(&pool).await?;

    let state = AppState::new(pool, &config);

    if args.mcp_stdio {
        return mcp::stdio::run(state).await;
    }

    let router = routes::build_router(state);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "panel-api listening");
    axum::serve(listener, router).await?;
    Ok(())
}
