//! gdkit MCP server entry point.
//!
//! Initializes stderr-only tracing (stdout is the JSON-RPC channel — a stray write there
//! corrupts the protocol), resolves Godot/project discovery, then serves the tool router —
//! over stdio by default, or over bearer-authenticated HTTP when opted in (see `http.rs`) —
//! until the client disconnects or Ctrl-C. On shutdown any still-running Godot child
//! processes are killed.

use rmcp::{transport::stdio, ServiceExt};

mod config;
mod docs;
mod export;
mod http;
mod process;
mod server;
mod settings;
mod testrun;
#[cfg(test)]
mod testutil;

use config::Config;
use http::Transport;
use server::GdkitServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;

    // stderr-ONLY logging. RUST_LOG overrides; default to info.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("gdkit-mcp starting");

    // Transport resolution happens before anything listens — a bad flag or a missing HTTP
    // token aborts here (fail closed).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let transport = http::resolve_transport(&args, |k| std::env::var(k).ok())
        .map_err(|e| anyhow::anyhow!(e))?;

    let cfg = Config::resolve();
    cfg.log_summary();

    let server = GdkitServer::new(cfg);

    match transport {
        Transport::Stdio => {
            let running = server
                .clone()
                .serve(stdio())
                .await
                .inspect_err(|e| tracing::error!("serve error: {e:?}"))?;

            // Serve until the client disconnects (stdin closes) or we get Ctrl-C.
            tokio::select! {
                r = running.waiting() => { r?; }
                _ = tokio::signal::ctrl_c() => { tracing::info!("ctrl-c, shutting down"); }
            }
        }
        Transport::Http(opts) => {
            http::serve_http(server.clone(), opts)
                .await
                .inspect_err(|e| tracing::error!("http serve error: {e:?}"))?;
        }
    }

    server.shutdown().await;
    Ok(())
}
