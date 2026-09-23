use ea_daemon::ipc;
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let socket_path = ea_core::paths::socket_path();
    let mut server = ipc::Server::new(&socket_path);
    server.register("status", |_| {
        Box::pin(async { Ok(json!({ "status": "ok" })) })
    });

    tracing::info!("ea-daemon listening on {}", socket_path.display());
    let handle = server.spawn().await?;

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.shutdown().await;

    Ok(())
}
