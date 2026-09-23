//! Newline-delimited JSON protocol shared by `ea-daemon` and `ea-cli`.
//!
//! One connection may carry several requests: each is a single line of JSON
//! terminated by `\n`, and each response is likewise a single line.

use std::path::PathBuf;

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

/// A client for the daemon's unix-socket IPC server.
///
/// Each `call` opens a fresh connection, sends a single request line, and
/// reads a single response line.
pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub async fn call(
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
