//! `ea-propose` — the stdio MCP server handed to every agent session.
//!
//! A thin shell: everything of substance is in the library, which is where the
//! tests are. See the crate docs in `lib.rs` for why this is a separate process
//! and why it exposes exactly one tool.

use ea_propose::ProposeServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the protocol. Logs go to stderr, which the session inherits.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let socket_path = ea_core::paths::socket_path();
    tracing::debug!("ea-propose talking to daemon at {}", socket_path.display());

    let service = ProposeServer::new(socket_path).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
