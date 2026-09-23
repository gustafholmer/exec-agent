use std::collections::HashMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use ea_core::ipc::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub type BoxFuture = Pin<Box<dyn Future<Output = anyhow::Result<serde_json::Value>> + Send>>;
pub type Handler = Arc<dyn Fn(serde_json::Value) -> BoxFuture + Send + Sync>;

pub struct Server {
    path: PathBuf,
    handlers: HashMap<String, Handler>,
}

pub struct ServerHandle {
    task: tokio::task::JoinHandle<()>,
    path: PathBuf,
}

impl ServerHandle {
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Server {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            handlers: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, method: &str, handler: F)
    where
        F: Fn(serde_json::Value) -> BoxFuture + Send + Sync + 'static,
    {
        self.handlers.insert(method.to_string(), Arc::new(handler));
    }

    pub async fn spawn(self) -> anyhow::Result<ServerHandle> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)
                .with_context(|| format!("removing stale socket {}", self.path.display()))?;
        }
        let listener = UnixListener::bind(&self.path)
            .with_context(|| format!("binding {}", self.path.display()))?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;

        let path = self.path.clone();
        let handlers = Arc::new(self.handlers);
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let handlers = Arc::clone(&handlers);
                        tokio::spawn(async move {
                            if let Err(err) = serve_connection(stream, handlers).await {
                                tracing::debug!("ipc connection ended: {err:#}");
                            }
                        });
                    }
                    Err(err) => tracing::warn!("ipc accept failed: {err}"),
                }
            }
        });

        Ok(ServerHandle { task, path })
    }
}

async fn serve_connection(
    stream: UnixStream,
    handlers: Arc<HashMap<String, Handler>>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(err) => Response::err("?", format!("malformed request: {err}")),
            Ok(request) => match handlers.get(&request.method) {
                None => Response::err(&request.id, format!("unknown method {:?}", request.method)),
                Some(handler) => match handler(request.params.clone()).await {
                    Ok(data) => Response::ok(&request.id, data),
                    Err(err) => Response::err(&request.id, format!("{err:#}")),
                },
            },
        };
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    async fn call(
        path: &std::path::Path,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = tokio::net::UnixStream::connect(path).await?;
        let req = ea_core::ipc::Request {
            id: "1".into(),
            method: method.into(),
            params,
        };
        stream
            .write_all(format!("{}\n", serde_json::to_string(&req)?).as_bytes())
            .await?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        let res: ea_core::ipc::Response = serde_json::from_str(&line)?;
        if res.ok {
            Ok(res.data.unwrap_or(serde_json::Value::Null))
        } else {
            Err(anyhow::anyhow!(res.error.unwrap_or_default()))
        }
    }

    fn server_with(dir: &TempDir) -> (Server, std::path::PathBuf) {
        let path = dir.path().join("d.sock");
        let mut server = Server::new(&path);
        server.register("ping", |_| Box::pin(async { Ok(json!("pong")) }));
        server.register("echo", |p| Box::pin(async move { Ok(p) }));
        server.register("boom", |_| {
            Box::pin(async { Err(anyhow::anyhow!("kaboom")) })
        });
        (server, path)
    }

    #[tokio::test]
    async fn round_trips_a_call() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        assert_eq!(
            call(&path, "ping", json!(null)).await.unwrap(),
            json!("pong")
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn passes_params_through() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        assert_eq!(
            call(&path, "echo", json!({"a":1})).await.unwrap(),
            json!({"a":1})
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_method_errors() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        let err = call(&path, "nope", json!(null)).await.unwrap_err();
        assert!(format!("{err}").contains("unknown method"));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_failing_handler_does_not_take_the_server_down() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        assert!(call(&path, "boom", json!(null)).await.is_err());
        assert_eq!(
            call(&path, "ping", json!(null)).await.unwrap(),
            json!("pong")
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn binds_over_a_stale_socket_file() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        std::fs::write(&path, b"").unwrap();
        let handle = server.spawn().await.unwrap();
        assert_eq!(
            call(&path, "ping", json!(null)).await.unwrap(),
            json!("pong")
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn the_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn handles_concurrent_calls() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();
        let (a, b) = tokio::join!(call(&path, "echo", json!(1)), call(&path, "echo", json!(2)));
        assert_eq!((a.unwrap(), b.unwrap()), (json!(1), json!(2)));
        handle.shutdown().await;
    }
}
