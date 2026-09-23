//! Connector registry: the daemon's side of the Model Context Protocol.
//!
//! Every service the agent can touch (calendar, mail, ...) lives behind an MCP
//! server that the daemon spawns as a child process and speaks to over stdio.
//! This module owns the discovery of those servers from disk, the lazy spawn
//! and reuse of one client per connector, and the call path -- including the
//! timeout policy, which is the reason clients are reused rather than spawned
//! per call.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, JsonObject, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// A connected MCP client, driven by `rmcp`'s background service loop. The
/// child process dies when the last handle is dropped.
type Client = RunningService<RoleClient, ()>;

/// How long to wait for `tools/list`. Listing is a cheap, non-user-facing call;
/// callers of [`Registry::call`] choose their own deadline instead.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(30);

/// Slack added to a caller's timeout for the outer guard. `rmcp`'s own
/// per-request timeout should always fire first -- it is the one that also
/// sends `notifications/cancelled` so the server can stop working -- and the
/// outer guard only catches a transport wedged so badly that even the send
/// never completes.
const TIMEOUT_GRACE: Duration = Duration::from_millis(500);

/// Default poll interval when `connector.toml` does not name one.
const DEFAULT_WATCH_INTERVAL_SECS: u64 = 300;

/// One connector as described by its `connector.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorManifest {
    /// Logical name, used as the key everywhere else in the daemon.
    pub name: String,
    /// Directory the manifest was read from; also the child's working
    /// directory, so a connector can keep credentials beside its manifest.
    pub dir: PathBuf,
    /// Program to run. See [`resolve_command`] for how it is located.
    pub command: String,
    pub args: Vec<String>,
    /// How often the daemon should call this connector's watch tool.
    pub watch_interval: Duration,
}

/// One tool as advertised by a connector, flattened so the rest of the daemon
/// need not depend on `rmcp`'s model types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_watch_interval_secs")]
    watch_interval_secs: u64,
}

fn default_watch_interval_secs() -> u64 {
    DEFAULT_WATCH_INTERVAL_SECS
}

/// Find every connector under `root`: a subdirectory qualifies when it holds
/// both a `connector.toml` and a `policy.toml`. A directory with a manifest but
/// no policy is deliberately *not* a connector -- an unpoliced connector would
/// be a hole in the gate -- and is skipped rather than reported.
///
/// Results are sorted by name so discovery order is deterministic.
pub fn discover(root: &Path) -> Result<Vec<ConnectorManifest>> {
    let mut found = Vec::new();
    if !root.exists() {
        return Ok(found);
    }
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("reading connector root {}", root.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", root.display()))?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("connector.toml");
        if !manifest_path.is_file() {
            continue;
        }
        if !dir.join("policy.toml").is_file() {
            // Skipping is fail-closed -- an unpoliced connector simply does not
            // exist -- but it is almost always a misconfiguration, so say so.
            tracing::warn!(
                dir = %dir.display(),
                "ignoring connector: connector.toml present but policy.toml missing"
            );
            continue;
        }
        found.push(load_manifest(&manifest_path)?);
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

fn load_manifest(path: &Path) -> Result<ConnectorManifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading manifest {}", path.display()))?;
    let raw: RawManifest =
        toml::from_str(&text).with_context(|| format!("parsing manifest {}", path.display()))?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("manifest {} has no parent directory", path.display()))?
        .to_path_buf();
    if raw.name.trim().is_empty() {
        bail!("manifest {} has an empty name", path.display());
    }
    if raw.command.trim().is_empty() {
        bail!("manifest {} has an empty command", path.display());
    }
    Ok(ConnectorManifest {
        name: raw.name,
        dir,
        command: raw.command,
        args: raw.args,
        watch_interval: Duration::from_secs(raw.watch_interval_secs),
    })
}

/// Turn a manifest `command` into an absolute program path.
///
/// Resolution order, first hit wins:
///   1. the command itself, if it is already an absolute path;
///   2. `<dir of the running executable>/<command>` -- so a connector shipped
///      alongside the daemon is found without any install step;
///   3. `<parent of that dir>/<command>` -- under `cargo test` the running
///      executable is `target/debug/deps/<test>-<hash>`, so the sibling
///      binaries actually live one level up in `target/debug`;
///   4. each entry of `PATH`.
///
/// Failing to resolve is an error rather than a fall-through to a bare command
/// name: a connector whose binary is missing should say so, not produce an
/// opaque `No such file or directory` from `spawn` some time later.
fn resolve_command(command: &str) -> Result<PathBuf> {
    let as_path = Path::new(command);
    if as_path.is_absolute() {
        if is_executable_file(as_path) {
            return Ok(as_path.to_path_buf());
        }
        bail!("connector command {command} is not an executable file");
    }

    let mut tried: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            tried.push(exe_dir.join(command));
            if let Some(exe_parent) = exe_dir.parent() {
                tried.push(exe_parent.join(command));
            }
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        tried.extend(std::env::split_paths(&path_var).map(|p| p.join(command)));
    }

    for candidate in &tried {
        if is_executable_file(candidate) {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "connector command {command} not found next to the running executable or on PATH \
         ({} candidates tried)",
        tried.len()
    )
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Owns one lazily spawned, long-lived MCP client per connector.
///
/// Clients are kept warm because spawning a child and completing the MCP
/// handshake costs far more than a tool call. The two rules that keep that
/// cache honest:
///
/// * a connection that fails is never cached, so a connector that was down at
///   first touch is retried on the next call;
/// * a call that times out does **not** evict the client, but a call that fails
///   at the transport (the child died) does. A slow call must not cost the
///   daemon a connector for the rest of its life; a dead child must not be
///   talked to forever.
pub struct Registry {
    manifests: HashMap<String, ConnectorManifest>,
    clients: Mutex<HashMap<String, Arc<Client>>>,
}

impl Registry {
    pub fn new(manifests: Vec<ConnectorManifest>) -> Self {
        let manifests = manifests
            .into_iter()
            .map(|m| (m.name.clone(), m))
            .collect::<HashMap<_, _>>();
        Self {
            manifests,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Names of every known connector, sorted.
    pub fn connector_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.manifests.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn manifest(&self, connector: &str) -> Option<&ConnectorManifest> {
        self.manifests.get(connector)
    }

    /// Number of connectors with a live client. Used by tests to prove the
    /// cache is doing its job.
    pub async fn live_client_count(&self) -> usize {
        self.clients.lock().await.len()
    }

    /// Ask a connector what it can do, spawning it if it is not up yet.
    pub async fn list_tools(&self, connector: &str) -> Result<Vec<ToolDescriptor>> {
        let client = self.client_for(connector).await?;
        // `list_all_tools` walks the cursor for us; connectors are expected to
        // advertise a handful of tools, not a paginated catalogue.
        let listed = tokio::time::timeout(LIST_TOOLS_TIMEOUT, client.peer().list_all_tools())
            .await
            .map_err(|_| {
                anyhow!("connector {connector}: tools/list timed out after {LIST_TOOLS_TIMEOUT:?}")
            })?;
        let listed = match listed {
            Ok(listed) => listed,
            Err(err) => {
                self.handle_service_error(connector, &client, &err).await;
                return Err(anyhow!("connector {connector}: tools/list failed: {err}"));
            }
        };
        Ok(listed
            .into_iter()
            .map(|tool| ToolDescriptor {
                name: tool.name.to_string(),
                description: tool.description.map(|d| d.to_string()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }

    /// Call `tool` on `connector`, returning the concatenated text content.
    ///
    /// `timeout` bounds the wait. A tool that reports failure (`is_error` on the
    /// result) and a protocol-level error both come back as `Err`; neither
    /// costs the connector its client.
    pub async fn call(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<String> {
        let client = self.client_for(connector).await?;
        let arguments = json_arguments(args)
            .with_context(|| format!("connector {connector}: arguments for tool {tool}"))?;

        let mut params = CallToolRequestParams::new(Cow::Owned(tool.to_string()));
        params.arguments = arguments;
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));

        // Two nested deadlines. The inner one is `rmcp`'s, which cancels the
        // request politely and leaves the client fully usable; the outer one is
        // a backstop for a transport that cannot even accept the send.
        let outcome = tokio::time::timeout(timeout + TIMEOUT_GRACE, async {
            client
                .peer()
                .send_request_with_option(request, PeerRequestOptions::with_timeout(timeout))
                .await?
                .await_response()
                .await
        })
        .await;

        let result = match outcome {
            Err(_elapsed) => {
                return Err(timeout_error(connector, tool, timeout));
            }
            Ok(Ok(result)) => result,
            Ok(Err(ServiceError::Timeout { .. })) => {
                return Err(timeout_error(connector, tool, timeout));
            }
            Ok(Err(err)) => {
                self.handle_service_error(connector, &client, &err).await;
                return Err(anyhow!("connector {connector}: tool {tool} failed: {err}"));
            }
        };

        let ServerResult::CallToolResult(result) = result else {
            bail!("connector {connector}: tool {tool} returned an unexpected response type");
        };
        let text = result_text(&result);
        if result.is_error.unwrap_or(false) {
            bail!("connector {connector}: tool {tool} reported an error: {text}");
        }
        Ok(text)
    }

    /// Close every live client and drop it. Safe to call more than once.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<Client>> = {
            let mut guard = self.clients.lock().await;
            guard.drain().map(|(_, client)| client).collect()
        };
        for client in clients {
            match Arc::try_unwrap(client) {
                // Sole owner: shut the child down and wait for the loop to end.
                Ok(client) => {
                    let _ = client.cancel().await;
                }
                // A call is still in flight; cancelling the token stops the
                // service loop and the child dies with the last handle.
                Err(shared) => shared.cancellation_token().cancel(),
            }
        }
    }

    /// Get the live client for `connector`, spawning one if needed.
    ///
    /// The lock is held across the spawn so two concurrent first calls cannot
    /// race into two child processes. It is *not* held across tool calls, which
    /// is what lets a `hang` on one connector coexist with work on another.
    async fn client_for(&self, connector: &str) -> Result<Arc<Client>> {
        let manifest = self
            .manifests
            .get(connector)
            .ok_or_else(|| anyhow!("unknown connector: {connector}"))?;

        let mut guard = self.clients.lock().await;
        if let Some(client) = guard.get(connector) {
            if !client.is_closed() {
                return Ok(client.clone());
            }
            // The service loop has ended -- the child is gone. Respawn.
            guard.remove(connector);
        }
        // A failed connection is never inserted, so the next call retries.
        let client = Arc::new(connect(manifest).await?);
        guard.insert(connector.to_string(), client.clone());
        Ok(client)
    }

    /// Evict `client` if -- and only if -- the failure means the child is gone
    /// and the map still points at this exact client.
    async fn handle_service_error(
        &self,
        connector: &str,
        client: &Arc<Client>,
        err: &ServiceError,
    ) {
        let transport_dead = matches!(
            err,
            ServiceError::TransportClosed | ServiceError::TransportSend(_)
        );
        if !transport_dead {
            return;
        }
        let mut guard = self.clients.lock().await;
        if guard
            .get(connector)
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            guard.remove(connector);
            tracing::warn!(connector, error = %err, "evicting connector client after transport failure");
        }
    }
}

fn timeout_error(connector: &str, tool: &str, timeout: Duration) -> anyhow::Error {
    anyhow!("connector {connector}: tool {tool} timed out after {timeout:?}")
}

async fn connect(manifest: &ConnectorManifest) -> Result<Client> {
    let program = resolve_command(&manifest.command)
        .with_context(|| format!("connector {}", manifest.name))?;
    let mut command = tokio::process::Command::new(&program);
    command.args(&manifest.args);
    command.current_dir(&manifest.dir);

    let transport = TokioChildProcess::new(command).with_context(|| {
        format!(
            "connector {}: spawning {}",
            manifest.name,
            program.display()
        )
    })?;
    let client = ().serve(transport).await.with_context(|| {
        format!(
            "connector {}: MCP handshake with {} failed",
            manifest.name,
            program.display()
        )
    })?;
    tracing::info!(connector = %manifest.name, program = %program.display(), "connector client started");
    Ok(client)
}

/// MCP wants tool arguments as a JSON object (or nothing at all).
fn json_arguments(args: Value) -> Result<Option<JsonObject>> {
    match args {
        Value::Null => Ok(None),
        Value::Object(map) => Ok(Some(map)),
        other => bail!(
            "tool arguments must be a JSON object, got {}",
            kind_of(&other)
        ),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Concatenate the text blocks of a result. Non-text blocks (images, embedded
/// resources) are dropped here; when a tool returns only structured content we
/// fall back to its JSON so the caller still gets something meaningful.
fn result_text(result: &CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    if !text.is_empty() {
        return text;
    }
    match &result.structured_content {
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use std::time::Instant;

    /// Absolute path to the fixture connector directory that ships with this
    /// crate's tests.
    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// Make sure `target/<profile>/ea-echo` exists before any test that needs
    /// it. `cargo test --workspace` builds it as a matter of course; a narrower
    /// invocation such as `cargo test -p ea-daemon connectors` does not, so we
    /// build it on demand. A fixture we cannot build is a hard failure -- a
    /// test that quietly skips is worse than no test.
    fn ensure_echo_binary() -> PathBuf {
        static ECHO: OnceLock<PathBuf> = OnceLock::new();
        ECHO.get_or_init(|| {
            if let Ok(path) = resolve_command("ea-echo") {
                return path;
            }
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .expect("locating the workspace root");
            let status = std::process::Command::new(env!("CARGO"))
                .arg("build")
                .arg("-p")
                .arg("ea-echo")
                .arg("--manifest-path")
                .arg(workspace.join("Cargo.toml"))
                .status()
                .expect("running cargo to build the ea-echo fixture");
            assert!(status.success(), "failed to build the ea-echo fixture");
            resolve_command("ea-echo").expect(
                "the ea-echo fixture binary is missing after building it; \
                 run `cargo build -p ea-echo`",
            )
        })
        .clone()
    }

    fn echo_registry() -> Registry {
        ensure_echo_binary();
        let manifests = discover(&fixture_root()).expect("discovering fixture connectors");
        assert!(
            manifests.iter().any(|m| m.name == "echo"),
            "fixture connector `echo` was not discovered under {}",
            fixture_root().display()
        );
        Registry::new(manifests)
    }

    #[test]
    fn discover_reads_the_fixture_manifest() {
        let manifests = discover(&fixture_root()).expect("discover");
        let echo = manifests
            .iter()
            .find(|m| m.name == "echo")
            .expect("echo connector");
        assert_eq!(echo.command, "ea-echo");
        assert!(echo.args.is_empty());
        assert_eq!(echo.watch_interval, Duration::from_secs(3600));
        assert_eq!(echo.dir, fixture_root().join("echo"));
    }

    #[test]
    fn discover_skips_directories_without_both_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A manifest with no policy is not a connector.
        let lonely = dir.path().join("lonely");
        std::fs::create_dir(&lonely).unwrap();
        std::fs::write(
            lonely.join("connector.toml"),
            "name = \"lonely\"\ncommand = \"x\"\n",
        )
        .unwrap();
        // A policy with no manifest is not a connector either.
        let policy_only = dir.path().join("policy-only");
        std::fs::create_dir(&policy_only).unwrap();
        std::fs::write(policy_only.join("policy.toml"), "[x]\n").unwrap();

        assert!(discover(dir.path()).expect("discover").is_empty());
    }

    #[test]
    fn resolve_command_finds_the_fixture_binary() {
        let path = ensure_echo_binary();
        assert!(path.is_absolute(), "{} should be absolute", path.display());
        assert!(path.ends_with("ea-echo"));
    }

    #[test]
    fn resolve_command_reports_a_missing_binary() {
        let err = resolve_command("ea-definitely-not-a-real-connector").unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn lists_the_fixture_tools() {
        let registry = echo_registry();
        let mut names: Vec<String> = registry
            .list_tools("echo")
            .await
            .expect("list_tools")
            .into_iter()
            .map(|t| t.name)
            .collect();
        names.sort();
        assert_eq!(names, vec!["echo", "explode", "hang", "watch_poll"]);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn calls_echo_and_gets_the_text_back() {
        let registry = echo_registry();
        let out = registry
            .call(
                "echo",
                "echo",
                serde_json::json!({ "text": "hello connector" }),
                Duration::from_secs(10),
            )
            .await
            .expect("echo call");
        assert_eq!(out, "hello connector");

        let empty = registry
            .call("echo", "watch_poll", Value::Null, Duration::from_secs(10))
            .await
            .expect("watch_poll call");
        assert_eq!(empty, "[]");
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn two_calls_reuse_one_client() {
        let registry = echo_registry();
        assert_eq!(registry.live_client_count().await, 0);
        for _ in 0..2 {
            registry
                .call(
                    "echo",
                    "echo",
                    serde_json::json!({ "text": "x" }),
                    Duration::from_secs(10),
                )
                .await
                .expect("echo call");
        }
        assert_eq!(registry.live_client_count().await, 1);
        registry.shutdown().await;
        assert_eq!(registry.live_client_count().await, 0);
    }

    #[tokio::test]
    async fn unknown_connector_is_an_error() {
        let registry = echo_registry();
        let err = registry
            .call("nope", "echo", Value::Null, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown connector"),
            "unexpected error: {err}"
        );
        assert_eq!(registry.live_client_count().await, 0);

        let err = registry.list_tools("nope").await.unwrap_err();
        assert!(
            err.to_string().contains("unknown connector"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn explode_surfaces_the_tool_error() {
        let registry = echo_registry();
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            registry.call("echo", "explode", Value::Null, Duration::from_secs(5)),
        )
        .await
        .expect("explode must answer, not hang")
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("explode"), "unexpected error: {message}");
        assert!(message.contains("boom"), "unexpected error: {message}");
        // A failing tool is not a failing connector.
        assert_eq!(registry.live_client_count().await, 1);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_tool_surfaces_the_protocol_error() {
        let registry = echo_registry();
        let err = registry
            .call("echo", "no_such_tool", Value::Null, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no_such_tool"),
            "unexpected error: {err}"
        );
        assert_eq!(registry.live_client_count().await, 1);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn a_timed_out_call_does_not_poison_the_connector() {
        let registry = echo_registry();
        let started = Instant::now();
        let err = registry
            .call("echo", "hang", Value::Null, Duration::from_millis(300))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();

        let message = err.to_string();
        assert!(message.contains("timed out"), "unexpected error: {message}");
        assert!(message.contains("echo"), "unexpected error: {message}");
        assert!(message.contains("hang"), "unexpected error: {message}");
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout took {elapsed:?}, expected well under 2s"
        );

        // The whole point: the connector is still there afterwards.
        assert_eq!(registry.live_client_count().await, 1);
        let out = registry
            .call(
                "echo",
                "echo",
                serde_json::json!({ "text": "still alive" }),
                Duration::from_secs(10),
            )
            .await
            .expect("echo must still work after a timeout");
        assert_eq!(out, "still alive");
        assert_eq!(registry.live_client_count().await, 1);
        registry.shutdown().await;
    }
}
