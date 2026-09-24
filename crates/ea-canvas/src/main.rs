//! `ea-canvas` — the stdio MCP server the daemon spawns.
//!
//! A shell: the client is in `client.rs`, the tools in `tools.rs`, and so are
//! the tests. See the crate docs in `lib.rs`.

use ea_canvas::client::{CanvasClient, Credentials};
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

    // A missing or unreadable credentials file is not a reason to refuse to
    // start. Exiting here would reach the daemon as "handshake failed", which
    // tells nobody what to do; starting and failing each call with the path to
    // create reaches the operator through `ea status` and the logs.
    let server = match Credentials::load() {
        Ok(credentials) => match CanvasClient::new(&credentials.base_url, credentials.token()) {
            Ok(client) => {
                tracing::debug!(base_url = %credentials.base_url, "canvas connector ready");
                CanvasServer::new(client)
            }
            Err(err) => {
                let reason = format!("{err:#}");
                tracing::error!(error = %reason, "canvas connector cannot build an HTTP client");
                CanvasServer::unconfigured(reason)
            }
        },
        Err(err) => {
            // `{err:#}` keeps the "create it with..." lines. It names a path,
            // never a token.
            let reason = format!("{err:#}");
            tracing::error!(error = %reason, "canvas connector is unconfigured");
            CanvasServer::unconfigured(reason)
        }
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
