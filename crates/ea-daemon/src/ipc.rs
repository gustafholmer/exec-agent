use std::collections::HashMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ea_core::ipc::{Request, Response};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;

pub type BoxFuture = Pin<Box<dyn Future<Output = anyhow::Result<serde_json::Value>> + Send>>;
pub type Handler = Arc<dyn Fn(serde_json::Value) -> BoxFuture + Send + Sync>;

/// Cap on a single protocol line (request or response). Generous for this
/// protocol -- the largest legitimate request is a `propose` carrying a
/// preview string -- and small enough that a client that never sends a
/// newline cannot grow a connection task's buffer without bound.
const MAX_LINE_BYTES: usize = 1024 * 1024;

pub struct Server {
    path: PathBuf,
    handlers: HashMap<String, Handler>,
}

pub struct ServerHandle {
    task: tokio::task::JoinHandle<()>,
    connections: Arc<Mutex<JoinSet<()>>>,
    path: PathBuf,
}

impl ServerHandle {
    /// Stop accepting new connections and wait for every in-flight
    /// connection task to actually finish (forcing them closed if needed)
    /// before returning, so the caller can rely on the socket being fully
    /// quiescent.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;

        // Take ownership of the connection set so we don't hold the
        // std::sync::Mutex guard across an `.await` below.
        let mut connections = {
            let mut guard = self.connections.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        connections.abort_all();
        while connections.join_next().await.is_some() {}

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

    /// Every method registered, sorted.
    ///
    /// Exists so that "what can be asked of this daemon" is a list a test can
    /// assert on by name rather than a claim in a comment. The gate argument
    /// in `daemon`'s module docs is about this exact set: a method that
    /// appears here without appearing there is the thing to catch.
    pub fn methods(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.handlers.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    pub async fn spawn(self) -> anyhow::Result<ServerHandle> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)
                .with_context(|| format!("removing stale socket {}", self.path.display()))?;
        }

        // Defence in depth against a world-readable window between bind()
        // and chmod(): tighten the process umask for exactly the duration
        // of the bind() call, then restore it via the guard's Drop -- even
        // if bind() fails. This is on top of (not instead of) the 0600
        // chmod below and the 0700 state directory, since umask is
        // process-global and this window should stay as small as possible.
        //
        // 0o077 (strip all group/other bits, leave owner bits alone) rather
        // than a value that also touches the owner bits: umask applies to
        // every thread in the process, and stripping the owner's execute
        // bit would corrupt any directory another thread happens to create
        // concurrently (observed directly -- it made unrelated concurrent
        // tests fail to bind with EACCES because their freshly-created
        // TempDir lost its own owner-execute bit mid-creation).
        let listener = {
            let _umask_guard = UmaskGuard::tighten(0o077);
            UnixListener::bind(&self.path)
        }
        .with_context(|| format!("binding {}", self.path.display()))?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;

        let path = self.path.clone();
        let handlers = Arc::new(self.handlers);
        let connections: Arc<Mutex<JoinSet<()>>> = Arc::new(Mutex::new(JoinSet::new()));
        let task = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let handlers = Arc::clone(&handlers);
                            let mut guard = connections.lock().unwrap();
                            // Opportunistically reap finished connections so
                            // the set doesn't grow without bound over the
                            // daemon's lifetime.
                            while guard.try_join_next().is_some() {}
                            guard.spawn(async move {
                                if let Err(err) = serve_connection(stream, handlers).await {
                                    tracing::debug!("ipc connection ended: {err:#}");
                                }
                            });
                        }
                        Err(err) => tracing::warn!("ipc accept failed: {err}"),
                    }
                }
            }
        });

        Ok(ServerHandle {
            task,
            connections,
            path,
        })
    }
}

/// Tightens the process umask for its lifetime and restores the previous
/// value on drop. Umask is process-global, so callers must keep the guard's
/// scope as small as possible.
struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    fn tighten(mask: libc::mode_t) -> Self {
        // SAFETY: umask(2) has no preconditions and cannot fail.
        let previous = unsafe { libc::umask(mask) };
        Self(previous)
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: same as above; restores whatever the process had before.
        unsafe {
            libc::umask(self.0);
        }
    }
}

/// Outcome of reading one protocol line.
enum Line {
    Some(String),
    /// The connection was closed (cleanly) before a full line arrived.
    Eof,
    /// A line exceeded `MAX_LINE_BYTES` before a newline was found.
    TooLong,
}

/// Reads a single newline-delimited line, refusing to buffer more than
/// `limit` bytes. Unlike `AsyncBufReadExt::lines()`, this bails out the
/// moment the limit is crossed instead of continuing to accumulate bytes
/// while waiting for a `\n` that may never come.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> anyhow::Result<Line> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(Line::Eof);
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let over_limit = buf.len() + pos > limit;
            if over_limit {
                reader.consume(pos + 1);
                return Ok(Line::TooLong);
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            let line = String::from_utf8(buf)
                .map_err(|err| anyhow::anyhow!("invalid utf-8 in request line: {err}"))?;
            return Ok(Line::Some(line));
        }
        let n = available.len();
        if buf.len() + n > limit {
            reader.consume(n);
            return Ok(Line::TooLong);
        }
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

async fn serve_connection(
    stream: UnixStream,
    handlers: Arc<HashMap<String, Handler>>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let line = match read_line_capped(&mut reader, MAX_LINE_BYTES).await? {
            Line::Eof => break,
            Line::TooLong => {
                let response = Response::err(
                    "?",
                    format!("request line exceeds {MAX_LINE_BYTES} byte limit"),
                );
                if let Ok(encoded) = serde_json::to_string(&response) {
                    // Best-effort: the client may already be gone.
                    let _ = write_half
                        .write_all(format!("{encoded}\n").as_bytes())
                        .await;
                }
                break;
            }
            Line::Some(line) => line,
        };

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

    #[tokio::test]
    async fn over_long_line_without_newline_is_rejected_and_connection_closed() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();

        {
            let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
            // Comfortably over the cap, but small enough to not risk the
            // test itself blocking on kernel socket-buffer limits.
            let payload = vec![b'a'; MAX_LINE_BYTES + 4096];
            let _ = stream.write_all(&payload).await;

            let mut response = String::new();
            let mut reader = BufReader::new(&mut stream);
            let _ = reader.read_line(&mut response).await;
            assert!(
                response.contains("exceeds") && response.contains("limit"),
                "expected an over-long-line error response, got {response:?}"
            );
        }

        // A fresh connection must still work: the daemon itself is fine,
        // only the offending connection was closed.
        assert_eq!(
            call(&path, "ping", json!(null)).await.unwrap(),
            json!("pong")
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_completes_with_an_open_connection() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();

        // Open a connection and leave it idle so its connection task is
        // parked reading when shutdown() runs.
        let _stream = tokio::net::UnixStream::connect(&path).await.unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown() must drain the open connection instead of hanging");
    }

    #[tokio::test]
    async fn framing_survives_a_message_split_across_two_writes() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();

        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let req = ea_core::ipc::Request {
            id: "frag-1".into(),
            method: "ping".into(),
            params: json!(null),
        };
        let line = format!("{}\n", serde_json::to_string(&req).unwrap());
        let mid = line.len() / 2;

        // Write half the message, flush, then the rest -- proving the
        // reader waits for the newline instead of treating the first
        // write as a complete request.
        stream.write_all(&line.as_bytes()[..mid]).await.unwrap();
        stream.flush().await.unwrap();
        stream.write_all(&line.as_bytes()[mid..]).await.unwrap();

        let mut response = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut response)
            .await
            .unwrap();
        let res: ea_core::ipc::Response = serde_json::from_str(&response).unwrap();
        assert!(res.ok);
        assert_eq!(res.data.unwrap(), json!("pong"));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn framing_survives_two_messages_and_a_trailing_fragment_in_one_write() {
        let dir = TempDir::new().unwrap();
        let (server, path) = server_with(&dir);
        let handle = server.spawn().await.unwrap();

        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();

        let req1 = ea_core::ipc::Request {
            id: "a".into(),
            method: "echo".into(),
            params: json!(1),
        };
        let req2 = ea_core::ipc::Request {
            id: "b".into(),
            method: "echo".into(),
            params: json!(2),
        };
        let req3 = ea_core::ipc::Request {
            id: "c".into(),
            method: "echo".into(),
            params: json!(3),
        };

        let line1 = format!("{}\n", serde_json::to_string(&req1).unwrap());
        let line2 = format!("{}\n", serde_json::to_string(&req2).unwrap());
        let full3 = format!("{}\n", serde_json::to_string(&req3).unwrap());
        let split = full3.len() - 5;
        let (fragment3, rest3) = full3.split_at(split);

        // Two whole messages plus a trailing fragment of a third, all in a
        // single write -- the fragment must not corrupt the two complete
        // requests, nor the request it belongs to once completed.
        let mut payload = Vec::new();
        payload.extend_from_slice(line1.as_bytes());
        payload.extend_from_slice(line2.as_bytes());
        payload.extend_from_slice(fragment3.as_bytes());
        stream.write_all(&payload).await.unwrap();

        {
            let mut reader = BufReader::new(&mut stream);
            let mut resp1 = String::new();
            reader.read_line(&mut resp1).await.unwrap();
            let mut resp2 = String::new();
            reader.read_line(&mut resp2).await.unwrap();

            let res1: ea_core::ipc::Response = serde_json::from_str(&resp1).unwrap();
            let res2: ea_core::ipc::Response = serde_json::from_str(&resp2).unwrap();
            assert_eq!(res1.id, "a");
            assert_eq!(res1.data.unwrap(), json!(1));
            assert_eq!(res2.id, "b");
            assert_eq!(res2.data.unwrap(), json!(2));
        }

        stream.write_all(rest3.as_bytes()).await.unwrap();
        let mut resp3 = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut resp3)
            .await
            .unwrap();
        let res3: ea_core::ipc::Response = serde_json::from_str(&resp3).unwrap();
        assert_eq!(res3.id, "c");
        assert_eq!(res3.data.unwrap(), json!(3));

        handle.shutdown().await;
    }
}
