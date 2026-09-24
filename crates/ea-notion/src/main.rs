//! `ea-notion` — the stdio MCP server the daemon spawns.
//!
//! A shell: the token store is in `auth.rs`, the REST client in `client.rs`,
//! the tools in `tools.rs`, the poll in `watch.rs`, and so are the tests. See
//! the crate docs in `lib.rs`.

use anyhow::Context;
use ea_notion::auth::TokenStore;
use ea_notion::client::NOTION_API_BASE;
use ea_notion::tools::NotionServer;
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
            tracing::error!(error = %reason, "notion connector is unconfigured");
            NotionServer::unconfigured(reason)
        }
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Everything that can go wrong before the handshake, in one place.
///
/// Note what is deliberately *not* checked here: whether any workspace has a
/// token. An empty token directory is the ordinary state of a fresh install,
/// and the workspaces are read fresh on every call, so dropping `work.json`
/// in while the daemon runs needs no restart. A poll with no workspaces fails
/// loudly on its own, with the setup steps in the message — see
/// `NotionServer::poll_at`.
///
/// What *is* checked is that the directory can be listed at all. A directory
/// that exists but cannot be read (wrong owner, no `x` bit) would otherwise
/// look exactly like a fresh install, and the operator would follow the setup
/// instructions to no effect.
fn build() -> anyhow::Result<NotionServer> {
    let store = TokenStore::new(None);
    let workspaces = store
        .list()
        .with_context(|| format!("listing Notion workspaces in {}", store.dir().display()))?;
    tracing::debug!(
        dir = %store.dir().display(),
        workspaces = workspaces.len(),
        "notion connector ready"
    );
    Ok(NotionServer::new(store, NOTION_API_BASE))
}
