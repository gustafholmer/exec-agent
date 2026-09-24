//! Newline-delimited JSON protocol shared by `ea-daemon` and `ea-cli`.
//!
//! One connection may carry several requests: each is a single line of JSON
//! terminated by `\n`, and each response is likewise a single line.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

/// How long a `call` waits for the daemon before giving up.
///
/// Sixty seconds. The slowest thing behind this socket that is not a model
/// session is `approve`, which reaches a connector under the executor's own
/// 30-second deadline, so a minute is generous headroom for every ordinary
/// call — and a bound at all is the point: the daemon is a long-lived process
/// holding a `Mutex<Connection>` and spawning children, and a wedged one used
/// to leave `ea approve` blocked forever with no output and no way out but
/// Ctrl-C. A CLI that hangs is indistinguishable from one that is working.
///
/// `ea chat` overrides this: a `claude -p` session legitimately takes minutes.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A client for the daemon's unix-socket IPC server.
///
/// Each `call` opens a fresh connection, sends a single request line, and
/// reads a single response line, all within [`Client::timeout`].
pub struct Client {
    socket_path: PathBuf,
    timeout: Duration,
}

impl Client {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// [`Client::new`] with a different deadline: longer for `chat`, which
    /// waits on a model session, and very short in tests.
    pub fn with_timeout(socket_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout,
        }
    }

    /// One request, one response, or an error saying the daemon did not answer.
    ///
    /// The deadline covers the whole exchange — connect, write, and read —
    /// rather than each step, because every one of them can be the thing that
    /// hangs: a daemon whose accept loop is alive but whose handler is blocked
    /// on the database mutex connects instantly and then says nothing.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        match tokio::time::timeout(self.timeout, self.exchange(method, params)).await {
            Ok(result) => result,
            Err(_elapsed) => anyhow::bail!(
                "the daemon did not answer {method:?} within {:?} (socket: {}). \
                 It is running but wedged; check the launchd log at \
                 ~/.local/state/exec-agent/daemon.err.log, and restart it if it stays stuck.",
                self.timeout,
                self.socket_path.display(),
            ),
        }
    }

    async fn exchange(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut stream = self.connect().await?;

        let request = Request {
            id: uuid::Uuid::new_v4().to_string(),
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&request).context("serializing IPC request")?;
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .with_context(|| format!("writing to daemon socket {}", self.socket_path.display()))?;

        let mut response_line = String::new();
        let mut reader = BufReader::new(stream);
        let n = reader
            .read_line(&mut response_line)
            .await
            .with_context(|| {
                format!("reading from daemon socket {}", self.socket_path.display())
            })?;
        if n == 0 {
            anyhow::bail!(
                "daemon closed the connection without responding (socket: {})",
                self.socket_path.display()
            );
        }

        let response: Response =
            serde_json::from_str(&response_line).context("parsing daemon response as JSON")?;
        if response.ok {
            Ok(response.data.unwrap_or(serde_json::Value::Null))
        } else {
            Err(anyhow::anyhow!(response
                .error
                .unwrap_or_else(|| "unknown error".to_string())))
        }
    }

    async fn connect(&self) -> anyhow::Result<UnixStream> {
        UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "connecting to daemon socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })
    }
}

/// Convenience constructor using the default daemon socket path.
pub fn client() -> Client {
    Client::new(crate::paths::socket_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The finding: `ea approve` against a daemon that accepts the connection
    /// and then never answers used to hang until the user pressed Ctrl-C.
    #[tokio::test]
    async fn a_daemon_that_never_answers_times_out_with_something_to_act_on() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("wedged.sock");

        // A "daemon" that accepts and then does nothing at all, forever.
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accepting = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let client = Client::with_timeout(&path, Duration::from_millis(200));
        let err = client
            .call("approve", serde_json::json!({ "id": 1 }))
            .await
            .expect_err("a wedged daemon must not hang the caller forever")
            .to_string();

        assert!(err.contains("did not answer"), "{err}");
        assert!(err.contains("approve"), "{err}");
        assert!(err.contains("wedged"), "{err}");
        accepting.abort();
    }

    /// The timeout must not fire on a daemon that is merely doing its job.
    #[tokio::test]
    async fn a_prompt_answer_is_returned_normally() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("live.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read_half)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            let response = Response::ok(request.id, serde_json::json!({ "status": "ok" }));
            let encoded = serde_json::to_string(&response).unwrap();
            write_half
                .write_all(format!("{encoded}\n").as_bytes())
                .await
                .unwrap();
        });

        let client = Client::with_timeout(&path, Duration::from_secs(5));
        let data = client
            .call("status", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(data["status"], "ok");
        serving.await.unwrap();
    }
}
