//! `ea-google` — the stdio MCP server the daemon spawns.
//!
//! A shell: the OAuth store is in `auth.rs`, the API clients in `calendar.rs`
//! and `gmail.rs`, the tools in `tools.rs`, the poll in `watch.rs`, and so are
//! the tests. See the crate docs in `lib.rs`.

use ea_google::auth::{AppConfig, Auth, TokenStore};
use ea_google::calendar::{CalendarClient, GOOGLE_CALENDAR_BASE};
use ea_google::gmail::{GmailClient, GOOGLE_GMAIL_BASE};
use ea_google::tools::GoogleServer;
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
            tracing::error!(error = %reason, "google connector is unconfigured");
            GoogleServer::unconfigured(reason)
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
/// authorised. Accounts are read fresh on every poll, so authorising `private`
/// while the daemon runs needs no restart — and a poll with no accounts fails
/// loudly on its own (see `watch::poll`).
fn build() -> anyhow::Result<GoogleServer> {
    let config = AppConfig::load()?;
    let store = TokenStore::new(None);
    let auth = Auth::new(config, store)?;
    let calendar = CalendarClient::new(GOOGLE_CALENDAR_BASE)?;
    let gmail = GmailClient::new(GOOGLE_GMAIL_BASE)?;
    tracing::debug!("google connector ready");
    Ok(GoogleServer::new(auth, calendar, gmail))
}
