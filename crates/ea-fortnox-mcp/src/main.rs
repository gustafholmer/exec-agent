//! `ea-fortnox-mcp` — the stdio MCP server the daemon spawns.
//!
//! A shell: the tools are in `tools/`, the credentials loader in `config.rs`,
//! and so are the tests. See the crate docs in `lib.rs`.

use std::sync::Arc;

use ea_fortnox::auth::{FileTokenStore, OAuthClient, TokenManager};
use ea_fortnox::FortnoxClient;
use ea_fortnox_mcp::config::AppConfig;
use ea_fortnox_mcp::tools::FortnoxServer;
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
            // name paths and commands, never a secret — see `config.rs`.
            let reason = format!("{err:#}");
            tracing::error!(error = %reason, "fortnox connector is unconfigured");
            FortnoxServer::unconfigured(reason)
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
/// nobody what to do; starting and failing each Fortnox-touching call with the
/// path to create reaches the operator through `ea status` and the logs — and
/// the `preview_*` tools keep working meanwhile, since they post nothing and
/// need no credentials.
///
/// Note what is deliberately *not* checked: whether `tokens.json` exists. The
/// token store is read on each call, so running the authorize command while
/// the daemon is up needs no restart, and a call with no stored grant already
/// fails with "No stored Fortnox tokens. Re-run ea-fortnox-authorize".
fn build() -> anyhow::Result<FortnoxServer> {
    let config = AppConfig::load()?;
    let oauth = OAuthClient::new(&config.client_id, &config.client_secret)?;
    let tokens = Arc::new(TokenManager::new(
        Arc::new(FileTokenStore::at_default_path()),
        Arc::new(oauth),
    ));
    let client = FortnoxClient::new(tokens)?;
    tracing::debug!("fortnox connector ready");
    Ok(FortnoxServer::new(client))
}
