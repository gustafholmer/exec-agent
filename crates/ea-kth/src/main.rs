//! `ea-kth` — the stdio MCP server the daemon spawns.
//!
//! A shell: the OAuth store is in `auth.rs`, the transport seam in `mail.rs`,
//! its Graph implementation in `graph.rs`, the tools in `tools.rs`, the poll
//! in `watch.rs`, and so are the tests. See the crate docs in `lib.rs`.

use std::sync::Arc;

use ea_kth::auth::{AppConfig, Auth, TokenStore};
use ea_kth::graph::{GraphTransport, GRAPH_BASE};
use ea_kth::mail::MailTransport;
use ea_kth::tools::KthServer;
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

    let server = match build() {
        Ok(server) => server,
        Err(err) => {
            // `{err:#}` keeps the "create it with..." lines. These messages
            // name paths and commands, never a token — see `auth.rs`.
            let reason = format!("{err:#}");
            tracing::error!(error = %reason, "kth connector is unconfigured");
            KthServer::unconfigured(reason)
        }
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Everything that can go wrong before the handshake, in one place.
///
/// A missing or unreadable `app.json` is not a reason to refuse to start.
/// Exiting here would reach the daemon as "handshake failed", which tells
/// nobody what to do; starting and failing each call with the path to create
/// reaches the operator through `ea status` and the logs.
///
/// Note what is deliberately *not* checked here: whether any account has been
/// authorised. Accounts are read fresh on every poll, so authorising one while
/// the daemon runs needs no restart — and a poll with no accounts fails loudly
/// on its own (see `watch::poll`).
fn build() -> anyhow::Result<KthServer> {
    let config = AppConfig::load()?;
    let store = TokenStore::new(None);
    let auth = Arc::new(Auth::new(config, store)?);
    let transport: Arc<dyn MailTransport> = Arc::new(GraphTransport::new(GRAPH_BASE, auth)?);
    tracing::debug!("kth connector ready");
    Ok(KthServer::new(transport))
}
