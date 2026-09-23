mod connectors;
mod ipc;

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

/// Fallback deadline for a tool call whose caller did not name one.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct CallParams {
    connector: String,
    tool: String,
    #[serde(default)]
    args: Value,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ConnectorParams {
    connector: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let connector_root = ea_core::paths::config_dir();
    let manifests = connectors::discover(&connector_root)?;
    tracing::info!(
        root = %connector_root.display(),
        connectors = ?manifests.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        "discovered connectors"
    );
    let registry = Arc::new(connectors::Registry::new(manifests));

    let socket_path = ea_core::paths::socket_path();
    let mut server = ipc::Server::new(&socket_path);
    server.register("status", |_| {
        Box::pin(async { Ok(json!({ "status": "ok" })) })
    });

    let list = registry.clone();
    server.register("connectors.list", move |_| {
        let registry = list.clone();
        Box::pin(async move {
            let live = registry.live_client_count().await;
            let connectors: Vec<Value> = registry
                .connector_names()
                .into_iter()
                .map(|name| {
                    let watch_interval_secs = registry
                        .manifest(&name)
                        .map(|m| m.watch_interval.as_secs())
                        .unwrap_or_default();
                    json!({ "name": name, "watch_interval_secs": watch_interval_secs })
                })
                .collect();
            Ok(json!({ "connectors": connectors, "live_clients": live }))
        })
    });

    let tools = registry.clone();
    server.register("connectors.tools", move |params| {
        let registry = tools.clone();
        Box::pin(async move {
            let params: ConnectorParams = serde_json::from_value(params)?;
            let tools: Vec<Value> = registry
                .list_tools(&params.connector)
                .await?
                .into_iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect();
            Ok(json!({ "tools": tools }))
        })
    });

    let call = registry.clone();
    server.register("connectors.call", move |params| {
        let registry = call.clone();
        Box::pin(async move {
            let params: CallParams = serde_json::from_value(params)?;
            let timeout = params
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_CALL_TIMEOUT);
            let text = registry
                .call(&params.connector, &params.tool, params.args, timeout)
                .await?;
            Ok(json!({ "text": text }))
        })
    });

    tracing::info!("ea-daemon listening on {}", socket_path.display());
    let handle = server.spawn().await?;

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    handle.shutdown().await;
    registry.shutdown().await;

    Ok(())
}
