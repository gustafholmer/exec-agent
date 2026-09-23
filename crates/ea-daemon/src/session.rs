//! Agent sessions: the daemon's only path to a model.
//!
//! There is no Rust Agent SDK, so `claude -p` in headless mode *is* the
//! interface. Every piece of thinking the daemon does -- triage, drafting,
//! summarising -- is a child process spawned here, handed a prompt and a
//! scoped set of MCP servers, and read back as one JSON object on stdout.
//!
//! Three properties are load-bearing, and each is enforced by a test:
//!
//! * **A session cannot write.** `Write`, `Edit`, `Bash` and `NotebookEdit` are
//!   disallowed on the command line. The only mutation a session can cause is
//!   a `propose_action` call on the `ea-propose` MCP server, which lands in the
//!   approval queue rather than in the world.
//! * **A session sees exactly the connectors it was scoped to.** The MCP config
//!   is built by [`McpConfig::for_session`] from an explicit request list, and
//!   `--strict-mcp-config` stops the CLI adding any of the user's own. A
//!   session handed a connector it was not scoped to is a security bug, not an
//!   inconvenience, so the test asserts the exact set.
//! * **Bad output is an error, never a panic.** `parse_output` shells out to a
//!   binary that updates itself; a banner on stdout or a new top-level key must
//!   degrade to a failed run, not take the daemon down.
//!
//! ## Why not `--bare`
//!
//! `--bare` would skip hooks, plugins, auto-memory and CLAUDE.md discovery --
//! exactly the overhead this daemon does not want. It also takes Anthropic auth
//! strictly from `ANTHROPIC_API_KEY`, never the subscription login, which would
//! put every triage cycle on a metered key and defeat the cost rationale of the
//! whole project. `--safe-mode` is disqualified for a sharper reason: measured
//! against this machine's CLI (2.1.267) it drops the servers passed in
//! `--mcp-config` too, so the session loses `propose_action` -- the one tool it
//! exists to call.
//!
//! What is left is `--setting-sources ''` (load no user, project or local
//! settings: no hooks, no plugins, no custom agents) plus
//! `--strict-mcp-config`, run from a dedicated empty working directory so that
//! no CLAUDE.md or auto-memory is discovered. Measured on a trivial prompt that
//! is ~16.3k prompt tokens against ~19-22k for the default load. The larger
//! effect is determinism: the default load varied between two prompt sizes
//! across consecutive runs, and each flip costs a full cache write (~$0.07 at
//! this machine's configured model) instead of a cache read (~$0.009).
//!
//! Argument order is fixed and the MCP config is a `BTreeMap` for the same
//! reason: a byte-identical prompt prefix is what keeps the cache warm.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ea_core::store::runs::RunStore;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::connectors::{resolve_command, ConnectorManifest};

/// The CLI binary a session is. Resolved like a connector command: beside the
/// running executable first, then `PATH`.
pub const CLAUDE_BIN: &str = "claude";

/// The MCP server every session gets, and the only write path out of one.
pub const PROPOSE_SERVER: &str = "ea-propose";

/// Tools removed from every session. A session reads and proposes; it does not
/// act. See the module docs.
pub const DISALLOWED_TOOLS: &str = "Write,Edit,Bash,NotebookEdit";

/// How long a session may run before the daemon takes it apart. Generous: a
/// triage session that reads a calendar and a mailbox can legitimately take
/// minutes. It exists to stop a wedged child holding a slot forever, not to
/// keep sessions brisk.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long each escalation step waits before the next one. Applied twice:
/// once after `SIGINT`, once after `SIGTERM`.
pub const DEFAULT_SIGNAL_GRACE: Duration = Duration::from_secs(10);

/// How long the pipes get to reach EOF once the child is gone. See [`Pump`].
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Cap on the text stored in a run's `detail`. A session's answer is worth
/// keeping; an unbounded one is not worth growing the database over.
const DETAIL_LIMIT: usize = 8_000;

/// One unit of thinking: a prompt, the context it may use, and the shape the
/// answer must take.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRequest {
    /// `kind` of the `runs` row this session records.
    pub kind: String,
    pub prompt: String,
    /// Appended to the CLI's own system prompt rather than replacing it: the
    /// built-in prompt is what teaches the model to use the tools at all.
    pub system_prompt: String,
    /// Connector names this session is scoped to. Anything not named here is
    /// not reachable from the session, by construction.
    pub connectors: Vec<String>,
    /// When set, the CLI validates the answer against this schema and returns
    /// it in `structured_output`.
    pub json_schema: Option<Value>,
    /// A previous `session_id`, to continue that conversation.
    pub resume: Option<String>,
    pub max_turns: Option<u32>,
    /// Model for this session. Left unset the CLI picks its own, which on a
    /// developer machine means whatever the human last chose -- on this one
    /// `opus-5[1m]`, at roughly ten times the cost of the small models. Any
    /// caller running a loop should set it.
    pub model: Option<String>,
}

impl SessionRequest {
    pub fn new(
        kind: impl Into<String>,
        prompt: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            prompt: prompt.into(),
            system_prompt: system_prompt.into(),
            connectors: Vec::new(),
            json_schema: None,
            resume: None,
            max_turns: None,
            model: None,
        }
    }

    pub fn with_connectors<I, S>(mut self, connectors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.connectors = connectors.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_json_schema(mut self, schema: Value) -> Self {
        self.json_schema = Some(schema);
        self
    }

    pub fn with_resume(mut self, session_id: impl Into<String>) -> Self {
        self.resume = Some(session_id.into());
        self
    }

    pub fn with_max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// What came back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOutcome {
    /// The `result` field: the model's final message as text.
    pub text: String,
    /// The `structured_output` field, present when a `json_schema` was asked
    /// for and the model satisfied it.
    pub structured: Option<Value>,
    /// The `session_id`, for a later [`SessionRequest::with_resume`].
    pub session_id: Option<String>,
    /// `total_cost_usd`, recorded on the `runs` row.
    pub cost_usd: Option<f64>,
}

/// The `--mcp-config` payload for one session: `ea-propose`, plus exactly the
/// connectors that session was scoped to.
///
/// A `BTreeMap` rather than a `HashMap` so that the serialised config -- and
/// therefore the prompt prefix the API caches -- is byte-identical between two
/// sessions with the same scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    servers: BTreeMap<String, Value>,
}

impl McpConfig {
    /// Build the config for a session.
    ///
    /// Fails when a requested connector is not among `available`, rather than
    /// quietly dropping it: a triage session that silently loses its calendar
    /// returns a confident answer about an empty calendar, which is worse than
    /// an error. Fails equally on a connector that shadows [`PROPOSE_SERVER`].
    pub fn for_session(
        propose: &Path,
        available: &[ConnectorManifest],
        requested: &[String],
    ) -> Result<Self> {
        let mut servers = BTreeMap::new();
        servers.insert(PROPOSE_SERVER.to_string(), server_entry(propose, &[]));

        for name in requested {
            if name == PROPOSE_SERVER {
                bail!(
                    "a session cannot request `{PROPOSE_SERVER}` as a connector: \
                     it is the write path, and it is always present"
                );
            }
            let manifest = available
                .iter()
                .find(|m| &m.name == name)
                .ok_or_else(|| anyhow!("session requested unknown connector `{name}`"))?;
            let program = resolve_command(&manifest.command)
                .with_context(|| format!("connector {}", manifest.name))?;
            servers.insert(name.clone(), server_entry(&program, &manifest.args));
        }

        Ok(Self { servers })
    }

    /// The `{"mcpServers": {...}}` document the CLI expects.
    pub fn to_value(&self) -> Value {
        json!({
            "mcpServers": Value::Object(self.servers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        })
    }

    /// Server names, sorted. Every session has at least [`PROPOSE_SERVER`].
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }
}

fn server_entry(program: &Path, args: &[String]) -> Value {
    json!({
        "command": program.to_string_lossy(),
        "args": args,
    })
}

/// The argument vector for one session, minus the program name.
///
/// Pure: no filesystem, no environment, no clock. Everything that needed
/// resolving was resolved when the [`McpConfig`] was built.
pub fn build_argv(req: &SessionRequest, mcp: &McpConfig) -> Vec<String> {
    let mut argv = vec![
        "-p".to_string(),
        req.prompt.clone(),
        "--output-format".into(),
        "json".into(),
        // Sessions read; they never write. The only mutation path is the
        // propose_action tool on the ea-propose MCP server.
        "--disallowedTools".into(),
        DISALLOWED_TOOLS.into(),
        "--append-system-prompt".into(),
        req.system_prompt.clone(),
        "--mcp-config".into(),
        mcp.to_value().to_string(),
        // Only the servers above: never the ones configured in the user's own
        // ~/.claude.
        "--strict-mcp-config".into(),
        // No user, project or local settings: no hooks, no plugins, no custom
        // agents. See the module docs for what this is worth.
        "--setting-sources".into(),
        String::new(),
        "--permission-mode".into(),
        "acceptEdits".into(),
        // Nobody is at a terminal to answer a prompt, so anything that would
        // ask is denied instead of hanging until the timeout.
        "--permission-prompts".into(),
        "none".into(),
    ];
    if let Some(model) = &req.model {
        argv.push("--model".into());
        argv.push(model.clone());
    }
    if let Some(schema) = &req.json_schema {
        argv.push("--json-schema".into());
        // Compact: `Value::to_string` never pretty-prints, and the flag takes
        // the schema as one argument.
        argv.push(schema.to_string());
    }
    if let Some(session) = &req.resume {
        argv.push("--resume".into());
        argv.push(session.clone());
    }
    if let Some(turns) = req.max_turns {
        argv.push("--max-turns".into());
        argv.push(turns.to_string());
    }
    argv
}

/// Read one `--output-format json` document.
///
/// Tolerant by design. The CLI is a self-updating binary: a field that moves,
/// a field that appears, a deprecation banner printed ahead of the JSON -- all
/// of those must produce a failed run that the next cycle retries, never a
/// panic that takes the daemon with it. Only two things are actually required:
/// the output parses as a JSON object, and it does not report an error.
pub fn parse_output(stdout: &str) -> Result<SessionOutcome> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("claude wrote nothing to stdout");
    }
    let value: Value = serde_json::from_str(trimmed)
        .with_context(|| format!("claude stdout was not JSON: {}", preview(trimmed)))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("claude stdout was not a JSON object: {}", preview(trimmed)))?;

    let text = object
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if object.get("is_error").and_then(Value::as_bool) == Some(true) {
        let subtype = object
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        bail!("claude reported an error ({subtype}): {}", preview(&text));
    }

    Ok(SessionOutcome {
        text,
        structured: object.get("structured_output").cloned(),
        session_id: object
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        cost_usd: object.get("total_cost_usd").and_then(Value::as_f64),
    })
}

/// First line, clipped, for an error message. Keeps a 4 MB stdout out of the
/// log and out of the database.
fn preview(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 200 {
        return line.to_string();
    }
    let clipped: String = line.chars().take(200).collect();
    format!("{clipped}...")
}

fn truncate_detail(text: &str) -> String {
    if text.len() <= DETAIL_LIMIT {
        return text.to_string();
    }
    let mut end = DETAIL_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated]", &text[..end])
}

/// Reads one of the child's pipes into a buffer as the bytes arrive.
///
/// A `read_to_end` on the pipe would be simpler and is wrong: the pipe closes
/// when the *last* writer lets go, and the CLI's own MCP server children
/// inherit its stderr. A grandchild that outlives its parent -- which is
/// exactly what happens when a wedged session is killed -- would hold that
/// write end open and hang the runner long past its timeout. So the read is
/// abandonable, and whatever arrived before it was abandoned is kept.
struct Pump {
    task: tokio::task::JoinHandle<()>,
    buffer: Arc<StdMutex<Vec<u8>>>,
}

impl Pump {
    fn start<R>(mut pipe: R) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let buffer = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&chunk[..n]),
                }
            }
        });
        Self { task, buffer }
    }

    /// Wait a bounded moment for EOF, then take what there is.
    async fn finish(self) -> Vec<u8> {
        let Self { mut task, buffer } = self;
        if tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err() {
            tracing::warn!("a child of the claude session still holds its pipe open");
            task.abort();
        }
        let mut guard = buffer.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }
}

/// Spawns sessions and records what they cost.
pub struct SessionRunner {
    runs: RunStore,
    claude: PathBuf,
    propose: PathBuf,
    /// Dedicated working directory for every session. Deliberately empty and
    /// outside any repository: the CLI discovers CLAUDE.md and auto-memory
    /// from its working directory, and a session should inherit neither.
    cwd: PathBuf,
    connectors: Vec<ConnectorManifest>,
    timeout: Duration,
    signal_grace: Duration,
}

impl SessionRunner {
    /// Resolve `claude` and `ea-propose` the way a connector command is
    /// resolved -- beside the running executable, then `PATH` -- and create the
    /// session working directory.
    pub fn discover(
        runs: RunStore,
        cwd: PathBuf,
        connectors: Vec<ConnectorManifest>,
    ) -> Result<Self> {
        let claude = resolve_command(CLAUDE_BIN).context("locating the claude CLI")?;
        let propose = resolve_command(PROPOSE_SERVER).context("locating the ea-propose server")?;
        std::fs::create_dir_all(&cwd)
            .with_context(|| format!("creating session working directory {}", cwd.display()))?;
        Ok(Self::new(runs, claude, propose, cwd, connectors))
    }

    pub fn new(
        runs: RunStore,
        claude: PathBuf,
        propose: PathBuf,
        cwd: PathBuf,
        connectors: Vec<ConnectorManifest>,
    ) -> Self {
        Self {
            runs,
            claude,
            propose,
            cwd,
            connectors,
            timeout: DEFAULT_TIMEOUT,
            signal_grace: DEFAULT_SIGNAL_GRACE,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long each signal in the escalation gets before the next one. The
    /// default is [`DEFAULT_SIGNAL_GRACE`]; tests shorten it.
    pub fn with_signal_grace(mut self, grace: Duration) -> Self {
        self.signal_grace = grace;
        self
    }

    /// Run one session to completion.
    ///
    /// A `runs` row is opened before the child is spawned and closed on every
    /// path out, so a session that fails, times out or returns nonsense is as
    /// visible in the ledger as one that works -- with `cost_usd` filled in
    /// from the CLI's own accounting when there is one.
    pub async fn run(&self, req: SessionRequest) -> Result<SessionOutcome> {
        let mcp = McpConfig::for_session(&self.propose, &self.connectors, &req.connectors)?;
        let argv = build_argv(&req, &mcp);

        let run_id = self.runs.start(&req.kind, &req.prompt, &[])?;
        let result = self
            .spawn(&argv)
            .await
            .and_then(|stdout| parse_output(&stdout));

        match result {
            Ok(outcome) => {
                self.runs.finish(
                    run_id,
                    "ok",
                    Some(&truncate_detail(&outcome.text)),
                    &[],
                    outcome.cost_usd,
                )?;
                Ok(outcome)
            }
            Err(err) => {
                let detail = truncate_detail(&format!("{err:#}"));
                // Recorded before the error is returned: a caller that drops
                // the error still leaves a closed row behind.
                self.runs
                    .finish(run_id, "error", Some(&detail), &[], None)?;
                Err(err)
            }
        }
    }

    /// Spawn the child, collect stdout, and take it apart if it overruns.
    async fn spawn(&self, argv: &[String]) -> Result<String> {
        let mut command = tokio::process::Command::new(&self.claude);
        command
            .args(argv)
            .current_dir(&self.cwd)
            // A session has no operator: anything read from stdin would block
            // forever.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {}", self.claude.display()))?;
        let pid = child.id();
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        // Drained concurrently with the wait: a session that fills a 64 KiB
        // pipe buffer while we wait on exit would deadlock against us.
        let stdout_pump = Pump::start(stdout_pipe);
        let stderr_pump = Pump::start(stderr_pipe);

        let timed_out = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(status) => {
                status.context("waiting for the claude child")?;
                false
            }
            Err(_elapsed) => {
                self.take_apart(&mut child, pid).await;
                true
            }
        };

        let stdout = String::from_utf8_lossy(&stdout_pump.finish().await).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_pump.finish().await).into_owned();

        if timed_out {
            bail!(
                "claude session timed out after {:?}: {}",
                self.timeout,
                preview(&stderr)
            );
        }
        if stdout.trim().is_empty() && !stderr.trim().is_empty() {
            // The CLI reports argument and auth failures on stderr and exits
            // without writing JSON; surfacing that beats "wrote nothing".
            bail!("claude wrote nothing to stdout: {}", preview(&stderr));
        }
        Ok(stdout)
    }

    /// Escalate: `SIGINT`, then `SIGTERM`, then `SIGKILL`.
    ///
    /// `SIGINT` is first because the CLI treats it as "end this turn": it stops
    /// the model, flushes its session transcript and exits cleanly, so the
    /// session stays resumable. `SIGTERM` alone leaves the turn unfinished and
    /// exits 143. `SIGKILL` is the last resort for a child that ignores both.
    async fn take_apart(&self, child: &mut tokio::process::Child, pid: Option<u32>) {
        let Some(pid) = pid else {
            // Already reaped; nothing to signal.
            return;
        };
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: `kill` on a pid this process owns and has not yet reaped
            // is always sound; a failure (the child just exited) is nothing to
            // act on.
            unsafe { libc::kill(pid as libc::pid_t, signal) };
            if tokio::time::timeout(self.signal_grace, child.wait())
                .await
                .is_ok()
            {
                tracing::warn!(pid, signal, "claude session overran its timeout; signalled");
                return;
            }
        }
        tracing::warn!(pid, "claude session ignored SIGINT and SIGTERM; killing");
        let _ = child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use rusqlite::Connection;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, RunStore) {
        let dir = TempDir::new().unwrap();
        let conn: Arc<Mutex<Connection>> = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        (dir, RunStore::new(conn))
    }

    fn manifest(name: &str) -> ConnectorManifest {
        ConnectorManifest {
            name: name.to_string(),
            dir: PathBuf::from("/tmp"),
            // Resolvable on any machine that can run these tests.
            command: "sh".to_string(),
            args: vec!["-c".to_string(), format!("{name}-server")],
            watch_interval: Duration::from_secs(3600),
        }
    }

    fn request() -> SessionRequest {
        SessionRequest::new("triage", "what is on today?", "You are terse.")
    }

    fn config(requested: &[&str]) -> McpConfig {
        let available: Vec<ConnectorManifest> = ["calendar", "mail", "fortnox"]
            .iter()
            .map(|n| manifest(n))
            .collect();
        let requested: Vec<String> = requested.iter().map(|s| s.to_string()).collect();
        McpConfig::for_session(Path::new("/opt/ea/ea-propose"), &available, &requested)
            .expect("building the session mcp config")
    }

    /// Value of `flag` in an argv, if present.
    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(String::as_str)
    }

    // ---- build_argv -----------------------------------------------------

    #[test]
    fn always_prints_and_asks_for_json() {
        let argv = build_argv(&request(), &config(&[]));
        assert!(argv.contains(&"-p".to_string()), "{argv:?}");
        assert_eq!(flag_value(&argv, "-p"), Some("what is on today?"));
        assert_eq!(flag_value(&argv, "--output-format"), Some("json"));
    }

    #[test]
    fn never_passes_bare() {
        // --bare takes auth strictly from ANTHROPIC_API_KEY, which would put
        // every session on a metered key instead of the subscription.
        let mut req = request();
        req.json_schema = Some(json!({"type": "object"}));
        req.resume = Some("abc".into());
        req.max_turns = Some(3);
        req.model = Some("sonnet".into());
        let argv = build_argv(&req, &config(&["calendar"]));
        assert!(!argv.iter().any(|a| a == "--bare"), "{argv:?}");
    }

    #[test]
    fn disallows_every_write_tool() {
        let argv = build_argv(&request(), &config(&[]));
        let value = flag_value(&argv, "--disallowedTools").expect("--disallowedTools is required");
        let listed: Vec<&str> = value.split(',').collect();
        for tool in ["Write", "Edit", "Bash", "NotebookEdit"] {
            assert!(listed.contains(&tool), "{tool} missing from {value}");
        }
    }

    #[test]
    fn appends_the_system_prompt_rather_than_replacing_it() {
        let argv = build_argv(&request(), &config(&[]));
        assert_eq!(
            flag_value(&argv, "--append-system-prompt"),
            Some("You are terse.")
        );
        assert!(
            !argv.iter().any(|a| a == "--system-prompt"),
            "the built-in prompt must stay: {argv:?}"
        );
    }

    #[test]
    fn includes_a_compact_json_schema_only_when_one_is_supplied() {
        let argv = build_argv(&request(), &config(&[]));
        assert!(!argv.iter().any(|a| a == "--json-schema"), "{argv:?}");

        let schema = json!({
            "type": "object",
            "properties": {"fruits": {"type": "array", "items": {"type": "string"}}},
            "required": ["fruits"],
        });
        let argv = build_argv(&request().with_json_schema(schema.clone()), &config(&[]));
        let value = flag_value(&argv, "--json-schema").expect("--json-schema is required");
        assert!(
            !value.contains('\n') && !value.contains(": "),
            "the schema must be compact: {value}"
        );
        assert_eq!(serde_json::from_str::<Value>(value).unwrap(), schema);
    }

    #[test]
    fn resumes_only_when_asked() {
        let argv = build_argv(&request(), &config(&[]));
        assert!(!argv.iter().any(|a| a == "--resume"), "{argv:?}");

        let argv = build_argv(&request().with_resume("9f0e-session"), &config(&[]));
        assert_eq!(flag_value(&argv, "--resume"), Some("9f0e-session"));
    }

    #[test]
    fn passes_max_turns_and_model_only_when_asked() {
        let argv = build_argv(&request(), &config(&[]));
        assert!(!argv.iter().any(|a| a == "--max-turns"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--model"), "{argv:?}");

        let argv = build_argv(
            &request().with_max_turns(4).with_model("haiku"),
            &config(&[]),
        );
        assert_eq!(flag_value(&argv, "--max-turns"), Some("4"));
        assert_eq!(flag_value(&argv, "--model"), Some("haiku"));
    }

    #[test]
    fn mcp_config_holds_propose_and_exactly_the_requested_connectors() {
        let argv = build_argv(&request(), &config(&["calendar", "mail"]));
        let raw = flag_value(&argv, "--mcp-config").expect("--mcp-config is required");
        let value: Value = serde_json::from_str(raw).expect("the mcp config must be JSON");

        let servers = value["mcpServers"].as_object().expect("mcpServers object");
        let mut names: Vec<&str> = servers.keys().map(String::as_str).collect();
        names.sort_unstable();
        // Exact, not "contains": a session handed a connector it was not
        // scoped to is a security bug.
        assert_eq!(names, vec!["calendar", "ea-propose", "mail"]);
        assert_eq!(servers["ea-propose"]["command"], "/opt/ea/ea-propose");
        assert!(servers["calendar"]["command"]
            .as_str()
            .unwrap()
            .ends_with("sh"));
    }

    #[test]
    fn a_session_with_no_connectors_still_gets_propose_and_nothing_else() {
        let argv = build_argv(&request(), &config(&[]));
        let raw = flag_value(&argv, "--mcp-config").unwrap();
        let value: Value = serde_json::from_str(raw).unwrap();
        let names: Vec<&str> = value["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, vec!["ea-propose"]);
    }

    #[test]
    fn the_users_own_mcp_servers_are_locked_out() {
        let argv = build_argv(&request(), &config(&[]));
        assert!(argv.iter().any(|a| a == "--strict-mcp-config"), "{argv:?}");
        assert_eq!(flag_value(&argv, "--setting-sources"), Some(""));
    }

    #[test]
    fn an_unknown_connector_is_refused_rather_than_dropped() {
        let available = vec![manifest("calendar")];
        let err = McpConfig::for_session(
            Path::new("/opt/ea/ea-propose"),
            &available,
            &["ghost".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn a_connector_cannot_shadow_the_propose_server() {
        let available = vec![manifest(PROPOSE_SERVER)];
        let err = McpConfig::for_session(
            Path::new("/opt/ea/ea-propose"),
            &available,
            &[PROPOSE_SERVER.to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains(PROPOSE_SERVER), "{err}");
    }

    #[test]
    fn the_argv_is_byte_identical_between_equivalent_sessions() {
        // Prompt caching is the whole cost model; a config that serialises in
        // a different order every time would never hit the cache.
        let first = build_argv(&request(), &config(&["mail", "calendar"]));
        let second = build_argv(&request(), &config(&["calendar", "mail"]));
        assert_eq!(first, second);
    }

    // ---- parse_output ---------------------------------------------------

    /// The real shape of `claude -p --output-format json` on 2.1.267, trimmed
    /// of the fields this code does not read.
    fn success_json() -> String {
        json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 1331,
            "num_turns": 1,
            "result": "ok",
            "session_id": "959351f8-5700-4515-a224-573e47e0a3bc",
            "total_cost_usd": 0.0092345,
            "usage": {"input_tokens": 2, "output_tokens": 4},
        })
        .to_string()
    }

    #[test]
    fn parses_text_session_id_and_cost() {
        let outcome = parse_output(&success_json()).unwrap();
        assert_eq!(outcome.text, "ok");
        assert_eq!(
            outcome.session_id.as_deref(),
            Some("959351f8-5700-4515-a224-573e47e0a3bc")
        );
        assert_eq!(outcome.cost_usd, Some(0.0092345));
        assert!(outcome.structured.is_none());
    }

    #[test]
    fn parses_a_structured_output_payload() {
        let raw = json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "{\"fruits\":[\"apple\",\"mango\"]}",
            "structured_output": {"fruits": ["apple", "mango"]},
            "session_id": "abc",
            "total_cost_usd": 0.168,
        })
        .to_string();
        let outcome = parse_output(&raw).unwrap();
        assert_eq!(
            outcome.structured,
            Some(json!({"fruits": ["apple", "mango"]}))
        );
        assert_eq!(outcome.cost_usd, Some(0.168));
    }

    #[test]
    fn empty_stdout_is_an_error_not_a_panic() {
        assert!(parse_output("").is_err());
        assert!(parse_output("   \n\t ").is_err());
    }

    #[test]
    fn a_cli_banner_instead_of_json_is_an_error() {
        let err =
            parse_output("A new version of Claude Code is available. Run `claude update`.\nok\n")
                .unwrap_err();
        assert!(err.to_string().contains("not JSON"), "{err}");
    }

    #[test]
    fn json_that_is_not_an_object_is_an_error() {
        assert!(parse_output("[1, 2, 3]").is_err());
        assert!(parse_output("\"just a string\"").is_err());
    }

    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        // What a CLI upgrade looks like: new keys, no warning.
        let raw = json!({
            "type": "result",
            "result": "ok",
            "session_id": "abc",
            "total_cost_usd": 0.01,
            "fast_mode_state": "off",
            "some_field_invented_next_release": {"nested": [1, 2, 3]},
        })
        .to_string();
        let outcome = parse_output(&raw).unwrap();
        assert_eq!(outcome.text, "ok");
        assert_eq!(outcome.cost_usd, Some(0.01));
    }

    #[test]
    fn missing_optional_fields_still_parse() {
        let outcome = parse_output(r#"{"type":"result","result":"ok"}"#).unwrap();
        assert_eq!(outcome.text, "ok");
        assert_eq!(outcome.session_id, None);
        assert_eq!(outcome.cost_usd, None);
    }

    #[test]
    fn a_reported_error_is_an_error() {
        let raw = json!({
            "type": "result",
            "subtype": "error_max_turns",
            "is_error": true,
            "result": "reached the turn limit",
            "session_id": "abc",
        })
        .to_string();
        let err = parse_output(&raw).unwrap_err();
        assert!(err.to_string().contains("error_max_turns"), "{err}");
    }

    // ---- run ------------------------------------------------------------

    /// A stand-in for `claude` that prints `body` on stdout. Offline, free and
    /// deterministic; the real CLI is exercised by the `#[ignore]`d test at
    /// the bottom of this module.
    fn fake_claude(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn runner(store: RunStore, claude: PathBuf, cwd: &Path) -> SessionRunner {
        SessionRunner::new(
            store,
            claude,
            PathBuf::from("/opt/ea/ea-propose"),
            cwd.to_path_buf(),
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn a_successful_session_records_a_run_row_with_its_cost() {
        let (dir, store) = temp_store();
        let body = success_json();
        let claude = fake_claude(
            dir.path(),
            "claude-ok",
            &format!("#!/bin/sh\ncat <<'JSON'\n{body}\nJSON\n"),
        );
        let runner = runner(store, claude, dir.path());

        let outcome = runner.run(request()).await.unwrap();
        assert_eq!(outcome.text, "ok");

        let run = runner.runs.get(1).unwrap().unwrap();
        assert_eq!(run.kind, "triage");
        assert_eq!(run.outcome, "ok");
        assert_eq!(run.cost_usd, Some(0.0092345));
        assert_eq!(run.detail.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn unparseable_output_records_a_failed_run() {
        let (dir, store) = temp_store();
        let claude = fake_claude(
            dir.path(),
            "claude-banner",
            "#!/bin/sh\necho 'command not found: model'\n",
        );
        let runner = runner(store, claude, dir.path());

        let err = runner.run(request()).await.unwrap_err();
        assert!(err.to_string().contains("not JSON"), "{err}");

        let run = runner.runs.get(1).unwrap().unwrap();
        assert_eq!(run.outcome, "error");
        assert_eq!(run.cost_usd, None);
        assert!(run.detail.unwrap().contains("not JSON"));
    }

    #[tokio::test]
    async fn a_silent_failure_reports_what_stderr_said() {
        let (dir, store) = temp_store();
        let claude = fake_claude(
            dir.path(),
            "claude-silent",
            "#!/bin/sh\necho 'error: unknown option --frobnicate' >&2\nexit 1\n",
        );
        let runner = runner(store, claude, dir.path());

        let err = runner.run(request()).await.unwrap_err();
        assert!(err.to_string().contains("--frobnicate"), "{err}");
        assert_eq!(runner.runs.get(1).unwrap().unwrap().outcome, "error");
    }

    #[tokio::test]
    async fn an_overrunning_session_is_interrupted_before_it_is_terminated() {
        let (dir, store) = temp_store();
        let witness = dir.path().join("got-sigint");
        // Traps SIGINT, records that it arrived, and exits cleanly -- the same
        // "end the turn" behaviour the real CLI has.
        let claude = fake_claude(
            dir.path(),
            "claude-slow",
            &format!(
                "#!/bin/sh\ntrap 'echo interrupted > {}; exit 0' INT\nsleep 30 &\nwait\n",
                witness.display()
            ),
        );
        // A second, not milliseconds: the deadline runs from `spawn`, and a
        // shell that has not yet reached its `trap` line dies on the default
        // SIGINT action instead of recording it.
        let runner = runner(store, claude, dir.path()).with_timeout(Duration::from_secs(1));

        let err = runner.run(request()).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            witness.exists(),
            "the child must get SIGINT first: SIGTERM alone leaves the turn unfinished"
        );

        let run = runner.runs.get(1).unwrap().unwrap();
        assert_eq!(run.outcome, "error");
        assert!(run.detail.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_session_that_ignores_sigint_is_still_taken_apart() {
        let (dir, store) = temp_store();
        let claude = fake_claude(
            dir.path(),
            "claude-stubborn",
            "#!/bin/sh\ntrap '' INT TERM\nsleep 30 &\nwait\n",
        );
        let runner = runner(store, claude, dir.path())
            .with_timeout(Duration::from_secs(1))
            .with_signal_grace(Duration::from_millis(200));
        // The escalation must finish rather than hang; the outer deadline is
        // what fails the test if SIGKILL is ever skipped.
        let err = tokio::time::timeout(Duration::from_secs(30), runner.run(request()))
            .await
            .expect("the runner must not hang on a child that ignores signals")
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    // ---- the real CLI ---------------------------------------------------

    /// Builds `ea-propose` on demand, the way the connector tests build
    /// `ea-echo`: `cargo test -p ea-daemon session` does not build a crate
    /// this one does not depend on.
    fn ensure_propose_binary() -> PathBuf {
        static PROPOSE: OnceLock<PathBuf> = OnceLock::new();
        PROPOSE
            .get_or_init(|| {
                if let Ok(path) = resolve_command(PROPOSE_SERVER) {
                    return path;
                }
                let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .canonicalize()
                    .expect("locating the workspace root");
                let status = std::process::Command::new(env!("CARGO"))
                    .args(["build", "-p", "ea-propose", "--manifest-path"])
                    .arg(workspace.join("Cargo.toml"))
                    .status()
                    .expect("running cargo to build ea-propose");
                assert!(status.success(), "failed to build ea-propose");
                resolve_command(PROPOSE_SERVER).expect("ea-propose is missing after building it")
            })
            .clone()
    }

    /// One real session against the installed CLI. Spends a little
    /// subscription usage, so it is not part of the default suite.
    ///
    /// Run it with:
    ///   cargo test -p ea-daemon --lib session -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns the real claude CLI and spends subscription usage"]
    async fn end_to_end_against_the_real_cli() {
        ensure_propose_binary();
        let (dir, store) = temp_store();
        let cwd = dir.path().join("session-cwd");
        let runner = SessionRunner::discover(store, cwd, Vec::new())
            .expect("resolving claude and ea-propose")
            .with_timeout(Duration::from_secs(180));

        let req = SessionRequest::new(
            "smoke",
            "Reply with exactly: ok",
            "You answer in as few words as possible.",
        );
        let outcome = runner.run(req).await.expect("the session must succeed");

        assert!(
            outcome.text.to_lowercase().contains("ok"),
            "unexpected text: {:?}",
            outcome.text
        );
        assert!(outcome.session_id.is_some(), "a session id must come back");

        let run = runner.runs.get(1).unwrap().unwrap();
        assert_eq!(run.outcome, "ok");
        let cost = run.cost_usd.expect("the run row must record a cost");
        assert!(cost > 0.0, "cost should be positive, got {cost}");
    }
}
