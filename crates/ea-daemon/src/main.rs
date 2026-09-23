//! The `ea-daemon` binary: a shell around [`ea_daemon::daemon::Daemon`].
//!
//! Everything the socket answers is registered by `Daemon::register`, so the
//! list of methods lives in one file rather than growing here.

use ea_daemon::daemon::Daemon;
use ea_daemon::ipc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let daemon = Daemon::from_config()?;

    let socket_path = ea_core::paths::socket_path();
    let mut server = ipc::Server::new(&socket_path);
    daemon.register(&mut server);

    tracing::info!("ea-daemon listening on {}", socket_path.display());
    let handle = server.spawn().await?;

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.shutdown().await;

    Ok(())
}
