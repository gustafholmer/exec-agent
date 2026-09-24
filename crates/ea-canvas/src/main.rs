//! `ea-canvas` — the stdio MCP server the daemon spawns.
//!
//! A shell: the client is in `client.rs`, the tools in `tools.rs`, and so are
//! the tests. See the crate docs in `lib.rs`.

use ea_canvas::client::CONNECTOR;
use ea_canvas::tools::CanvasServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the MCP protocol. Logs go to stderr, which the daemon inherits.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    // Nothing about the credentials is read here, and that is the point: the
    // server loads `credentials.json` on every call, so a regenerated Canvas
    // token takes effect without a daemon restart — the same shape as
    // `ea-notion` and `ea-kth`, which read their token stores per call.
    //
    // A missing or unreadable file is still not a reason to refuse to start.
    // Exiting here would reach the daemon as "handshake failed", which tells
    // nobody what to do; failing each call with the path to create reaches the
    // operator through `ea status` and the logs — and the moment the file
    // appears, the next call works.
    let dir = ea_core::paths::connector_config_dir(CONNECTOR);
    tracing::debug!(dir = %dir.display(), "canvas connector ready");
    let server = CanvasServer::new(dir);

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
