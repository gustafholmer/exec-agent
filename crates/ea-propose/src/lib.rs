//! `ea-propose` — the one MCP server that can change anything.
//!
//! An agent session runs as a `claude -p` subprocess. Unlike the TypeScript
//! Agent SDK, a subprocess cannot host an in-process MCP server, so the single
//! tool that is allowed to *act* has to be a real process on the other end of a
//! stdio pipe. That process is this one.
//!
//! A session is handed read-only connector tools plus this server. Everything
//! it wants to change in the world goes through [`ProposeServer::propose_action`],
//! which does no work of its own: it forwards the proposal to the daemon over
//! the unix control socket and renders the daemon's verdict as text. The policy
//! gate on the other side decides whether the action executes now, waits for a
//! human tap in Telegram, or is refused.
//!
//! Two properties matter more than anything else here.
//!
//! **Exactly one tool.** This crate is what stands between a language model and
//! the ability to post a voucher to company accounting or send mail. A second
//! tool appearing in this server is a widening of that surface, and
//! [`tests::the_server_exposes_exactly_one_tool`] exists to make that a
//! deliberate act rather than an accident.
//!
//! **Never panic.** A panicking stdio MCP server takes the whole agent session
//! down with it — the model loses its tools mid-task and the run dies. Every
//! failure mode (no daemon, a socket that vanishes mid-call, a reply that is not
//! JSON, no reply at all) is turned into an MCP *error result* whose text says
//! what happened, whether anything was recorded, and what to do about it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};

/// The IPC method the daemon exposes for this.
pub const PROPOSE_METHOD: &str = "propose";

/// How long to wait for the daemon's answer before giving up.
///
/// An `auto` action is executed *inside* the daemon's `propose` handler, and a
/// connector call there is itself bounded at 30s, so this has to be comfortably
/// larger than a connector round trip; it is a backstop against a wedged daemon,
/// not a latency budget.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// What the daemon said happened to a proposal.
///
/// Deserialised straight from the serialised `Action` the daemon returns; the
/// other columns of that row are of no interest to a model and are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    #[serde(rename = "id", alias = "action_id")]
    pub action_id: i64,
    pub status: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Turn a verdict into the text the model reads.
///
/// This is the whole of the interesting logic in the crate, which is why it is
/// a pure function rather than something buried in the tool handler.
///
/// It is total by construction: `status` is a string that came off the wire, so
/// an unrecognised value must produce a neutral sentence naming it rather than a
/// panic. The daemon and this binary are versioned separately, and a status this
/// build has never heard of is a reason to say so, not to kill the session.
pub fn render(verdict: &Verdict) -> String {
    let id = verdict.action_id;
    match verdict.status.as_str() {
        "executed" => format!(
            "Action {id} was auto-approved by policy and has already executed. \
             The connector returned: {}",
            verdict
                .result
                .as_deref()
                .unwrap_or("(nothing; the tool reported success with no output)")
        ),
        "proposed" => format!(
            "Action {id} is queued and awaiting human approval. \
             Nothing has happened yet and nothing will happen until a human approves it \
             in Telegram. Do not retry it and do not propose it again — a second proposal \
             becomes a second action for the human to decide. Carry on with the rest of \
             your work, or stop if there is nothing else to do."
        ),
        "rejected" => format!(
            "Action {id} was rejected and will not run. Reason: {}. \
             This is a policy decision, not a transient failure: proposing the same action \
             again will be rejected again. If it genuinely needs to happen, say so in your \
             answer and leave it to a human.",
            reason_or(verdict, "no reason was given")
        ),
        "failed" => format!(
            "Action {id} was approved and executed, but the connector failed: {}. \
             The action is recorded as failed. Whether anything partially happened depends \
             on the connector, so check before proposing the same thing again.",
            reason_or(verdict, "no reason was given")
        ),
        other => format!(
            "Action {id} came back in the state \"{other}\", which this version of \
             ea-propose does not recognise.{}{} Treat the outcome as unknown: do not assume \
             it ran, and do not propose it again without checking.",
            verdict
                .reason
                .as_deref()
                .map(|r| format!(" Reason: {r}."))
                .unwrap_or_default(),
            verdict
                .result
                .as_deref()
                .map(|r| format!(" Result: {r}."))
                .unwrap_or_default(),
        ),
    }
}

fn reason_or<'a>(verdict: &'a Verdict, fallback: &'a str) -> &'a str {
    verdict.reason.as_deref().unwrap_or(fallback)
}

/// The arguments of the one tool.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposeArgs {
    /// The connector that owns the tool, e.g. "fortnox".
    pub connector: String,
    /// The tool to invoke on that connector, e.g. "record_voucher".
    pub tool: String,
    /// The arguments to pass to that tool.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
    /// One or two lines a human can read in Telegram to decide, in plain
    /// language, describing exactly what will happen if this is approved.
    pub preview: String,
    /// Why you are proposing this now.
    pub rationale: String,
}

/// The stdio MCP server. One tool, and no state beyond where the daemon lives.
#[derive(Clone)]
pub struct ProposeServer {
    socket_path: Arc<PathBuf>,
    timeout: Duration,
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl ProposeServer {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: Arc::new(socket_path.into()),
            timeout: DEFAULT_TIMEOUT,
            tool_router: Self::tool_router(),
        }
    }

    /// Override the deadline on the daemon round trip. Used by the tests to
    /// exercise the timeout path without waiting two minutes.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[tool_router]
impl ProposeServer {
    /// Returning `Err(String)` makes `rmcp` answer with a successful JSON-RPC
    /// response whose `CallToolResult` carries `is_error: true` and the message
    /// as text — the MCP way of saying "the tool ran and failed". That is what
    /// every failure here becomes. Nothing in this function may panic or
    /// propagate: the session on the other end of the pipe depends on this
    /// process staying alive.
    #[tool(
        description = "Propose an action that changes something in the outside world. \
                       You cannot act directly; this is the only way. The proposal goes to \
                       the policy gate, which either runs it now, queues it for a human to \
                       approve in Telegram, or refuses it. The reply tells you which happened."
    )]
    pub async fn propose_action(
        &self,
        Parameters(args): Parameters<ProposeArgs>,
    ) -> Result<String, String> {
        match ask_daemon(&self.socket_path, self.timeout, &args).await {
            Ok(verdict) => Ok(render(&verdict)),
            Err(message) => Err(message),
        }
    }
}

#[tool_handler]
impl ServerHandler for ProposeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "exec-agent's write path. You have no other way to change anything: every \
             side effect — posting a voucher, sending mail, writing to a calendar — is \
             proposed through `propose_action` and decided by the policy gate. Write the \
             `preview` for the human who will read it on their phone.",
        )
    }
}

/// Send one proposal to the daemon and parse the verdict.
///
/// Every error return is a sentence aimed at whoever (or whatever) is reading
/// the session transcript: what failed, whether the action might nevertheless
/// have been recorded, and what to do next. The distinction is not cosmetic —
/// "the daemon is not running, nothing was recorded" and "the daemon answered
/// something I could not parse, the action may exist" call for opposite
/// responses from the model, and getting it wrong means either a lost action or
/// a duplicated one.
async fn ask_daemon(
    socket_path: &Path,
    timeout: Duration,
    args: &ProposeArgs,
) -> Result<Verdict, String> {
    let params = serde_json::json!({
        "connector": args.connector,
        "tool": args.tool,
        "args": args.args,
        "preview": args.preview,
        "rationale": args.rationale,
    });

    let client = ea_core::ipc::Client::new(socket_path);
    let call = client.call(PROPOSE_METHOD, params);

    let response = match tokio::time::timeout(timeout, call).await {
        Err(_elapsed) => {
            return Err(format!(
                "propose_action: the exec-agent daemon did not answer within {}s \
                 (socket: {}). The action MAY have been recorded, and if policy \
                 auto-approved it, it may even have run. Do not propose it again — \
                 report the timeout and let a human check the pending queue.",
                timeout.as_secs(),
                socket_path.display()
            ));
        }
        Ok(response) => response,
    };

    let value = match response {
        Ok(value) => value,
        Err(err) => {
            // `Client::call` folds two very different things into one error:
            // a transport failure (no socket, connection refused, the daemon
            // hung up) and an `ok: false` reply, which is the daemon having
            // considered the request and declined it. Only the first means
            // "nothing was recorded", so they get different text.
            let detail = format!("{err:#}");
            return Err(if socket_path.exists() {
                format!(
                    "propose_action: the exec-agent daemon refused or could not \
                     complete this proposal: {detail}. Nothing was executed. If the \
                     message points at your arguments, fix them and try once more; \
                     otherwise report it and move on."
                )
            } else {
                format!(
                    "propose_action: the exec-agent daemon is not reachable — there is \
                     no socket at {}. Nothing was proposed, nothing was recorded and \
                     nothing ran. The daemon is not running (start it with `ea-daemon`, \
                     or check that $EA_STATE_DIR matches the one it uses). Do not try to \
                     perform this action any other way: say clearly in your answer that \
                     it could not be proposed. Underlying error: {detail}",
                    socket_path.display()
                )
            });
        }
    };

    serde_json::from_value::<Verdict>(value.clone()).map_err(|err| {
        format!(
            "propose_action: the exec-agent daemon answered with something this build \
             does not understand ({err}). The action MAY have been recorded and may even \
             have run, so do NOT propose it again — report this and let a human check the \
             pending queue. Raw reply: {}",
            truncate(&value.to_string(), 500)
        )
    })
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}… ({} chars total)", text.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn verdict(status: &str) -> Verdict {
        Verdict {
            action_id: 42,
            status: status.to_string(),
            result: None,
            reason: None,
        }
    }

    // --- render ---------------------------------------------------------

    #[test]
    fn executed_names_the_action_and_carries_the_result() {
        let text = render(&Verdict {
            result: Some("voucher V-2026-114 posted".into()),
            ..verdict("executed")
        });
        assert!(text.contains("42"), "{text}");
        assert!(text.contains("voucher V-2026-114 posted"), "{text}");
    }

    #[test]
    fn proposed_says_queued_and_awaiting_human_approval() {
        let text = render(&verdict("proposed"));
        assert!(text.contains("42"), "{text}");
        assert!(text.contains("queued"), "{text}");
        assert!(text.contains("awaiting human approval"), "{text}");
    }

    #[test]
    fn rejected_says_rejected_and_carries_the_reason() {
        let text = render(&Verdict {
            reason: Some("policy denies this tool: a human submits their own coursework".into()),
            ..verdict("rejected")
        });
        assert!(text.contains("rejected"), "{text}");
        assert!(text.contains("submits their own coursework"), "{text}");
    }

    #[test]
    fn failed_says_failed_and_carries_the_reason() {
        let text = render(&Verdict {
            reason: Some("fortnox returned 502".into()),
            ..verdict("failed")
        });
        assert!(text.contains("failed"), "{text}");
        assert!(text.contains("fortnox returned 502"), "{text}");
    }

    #[test]
    fn an_unrecognised_status_is_a_neutral_sentence_naming_it() {
        let text = render(&verdict("quantum-superposed"));
        assert!(text.contains("quantum-superposed"), "{text}");
        assert!(text.contains("42"), "{text}");
        // Neutral: it must not claim either outcome.
        assert!(!text.contains("has already executed"), "{text}");
        assert!(!text.contains("awaiting human approval"), "{text}");
    }

    #[test]
    fn every_status_renders_without_a_result_or_a_reason() {
        // The daemon may legitimately omit both; none of these may panic.
        for status in [
            "executed", "proposed", "rejected", "failed", "approved", "expired", "", "PROPOSED",
        ] {
            let text = render(&verdict(status));
            assert!(!text.is_empty(), "empty render for {status:?}");
            assert!(text.contains("42"), "{status:?} -> {text}");
        }
    }

    #[test]
    fn a_verdict_parses_from_a_serialised_action() {
        // The shape `ea_core::store::actions::Action` serialises to, with the
        // columns this crate ignores left in.
        let value = serde_json::json!({
            "id": 7,
            "connector": "fortnox",
            "tool": "record_voucher",
            "args": { "amount": 1250 },
            "preview": "post a voucher",
            "rationale": "the receipt arrived",
            "status": "proposed",
            "reason": null,
            "result": null,
            "created_at": "2026-09-24T09:00:00Z",
            "expires_at": "2026-09-25T09:00:00Z",
            "decided_at": null,
            "executed_at": null,
        });
        let parsed: Verdict = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.action_id, 7);
        assert_eq!(parsed.status, "proposed");
    }

    // --- the tool surface ------------------------------------------------

    /// The guard rail. `#[tool_handler]` answers `tools/list` with exactly
    /// `tool_router().list_all()`, so this is the list a session sees.
    ///
    /// This crate is the entire write path of the system. If a second tool ever
    /// turns up here, that must be a decision somebody made on purpose, with
    /// this test in front of them — not a line that slipped in.
    #[test]
    fn the_server_exposes_exactly_one_tool() {
        let tools = ProposeServer::tool_router().list_all();
        assert_eq!(
            tools.len(),
            1,
            "ea-propose must expose exactly one tool, found: {:?}",
            tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
        );
        assert_eq!(tools[0].name.as_ref(), "propose_action");
    }

    #[test]
    fn the_tool_schema_requires_the_five_fields() {
        let tools = ProposeServer::tool_router().list_all();
        let schema = serde_json::to_value(&*tools[0].input_schema).unwrap();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for field in ["connector", "tool", "preview", "rationale"] {
            assert!(
                required.contains(&field),
                "{field} must be required: {schema}"
            );
        }
        assert!(
            schema["properties"].get("args").is_some(),
            "args must be in the schema: {schema}"
        );
    }

    // --- the handler never panics ----------------------------------------

    fn args() -> ProposeArgs {
        ProposeArgs {
            connector: "fortnox".into(),
            tool: "record_voucher".into(),
            args: serde_json::Map::new(),
            preview: "post a 1250 SEK voucher".into(),
            rationale: "the receipt arrived".into(),
        }
    }

    /// The failure a session will actually hit: the daemon is not running.
    #[tokio::test]
    async fn a_missing_daemon_socket_is_an_error_result_not_a_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nothing-here.sock");
        let server = ProposeServer::new(&missing);

        let error = server
            .propose_action(Parameters(args()))
            .await
            .expect_err("a missing socket must be reported, not succeed");

        // The text is the whole point: this is what a human reads in the
        // transcript when they ask why nothing happened.
        assert!(error.contains("propose_action"), "{error}");
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains("not running"), "{error}");
        assert!(
            error.contains("Nothing was proposed"),
            "the model must be told the action does not exist: {error}"
        );
    }

    /// Accept the connection, then hang up without answering — a daemon that
    /// dies mid-call.
    #[tokio::test]
    async fn a_daemon_that_hangs_up_mid_call_is_an_error_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });

        let error = ProposeServer::new(&path)
            .propose_action(Parameters(args()))
            .await
            .expect_err("a hang-up must be reported");
        assert!(error.contains("propose_action"), "{error}");
        assert!(error.contains("Nothing was executed"), "{error}");
    }

    /// A reply that is not a verdict. The action may exist, so the text must
    /// not tell the model to retry.
    #[tokio::test]
    async fn an_unparseable_reply_is_an_error_result_that_forbids_a_retry() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read_half)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: ea_core::ipc::Request = serde_json::from_str(&line).unwrap();
            let reply = ea_core::ipc::Response::ok(
                request.id,
                serde_json::json!({ "not": "an action at all" }),
            );
            let _ = write_half
                .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                .await;
        });

        let error = ProposeServer::new(&path)
            .propose_action(Parameters(args()))
            .await
            .expect_err("a reply that is not a verdict must be reported");
        assert!(error.contains("does not understand"), "{error}");
        assert!(error.contains("do NOT propose it again"), "{error}");
        assert!(
            error.contains("an action at all"),
            "the raw reply must be quoted so a human can see what came back: {error}"
        );
    }

    /// A daemon that accepts but never answers.
    #[tokio::test]
    async fn a_daemon_that_never_answers_times_out_into_an_error_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let held = tokio::spawn(async move {
            // Accept, then answer nothing at all, holding the connection open
            // until the test aborts this task.
            let _accepted = listener.accept().await;
            std::future::pending::<()>().await
        });

        let error = ProposeServer::new(&path)
            .with_timeout(Duration::from_millis(200))
            .propose_action(Parameters(args()))
            .await
            .expect_err("a silent daemon must time out into an error");
        assert!(error.contains("did not answer within"), "{error}");
        assert!(error.contains("MAY have been recorded"), "{error}");
        held.abort();
    }

    /// The happy path, against a socket that speaks the real protocol.
    #[tokio::test]
    async fn a_verdict_from_a_real_socket_is_rendered() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut line = String::new();
            BufReader::new(read_half)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: ea_core::ipc::Request = serde_json::from_str(&line).unwrap();
            let reply = ea_core::ipc::Response::ok(
                request.id.clone(),
                serde_json::json!({ "id": 9, "status": "proposed" }),
            );
            let _ = write_half
                .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                .await;
            request
        });

        let text = ProposeServer::new(&path)
            .propose_action(Parameters(args()))
            .await
            .expect("a well-formed verdict must render");
        assert!(text.contains("queued"), "{text}");
        assert!(text.contains("9"), "{text}");

        // And the request the daemon received is the documented shape.
        let request = served.await.unwrap();
        assert_eq!(request.method, PROPOSE_METHOD);
        assert_eq!(request.params["connector"], "fortnox");
        assert_eq!(request.params["tool"], "record_voucher");
        assert_eq!(request.params["preview"], "post a 1250 SEK voucher");
        assert_eq!(request.params["rationale"], "the receipt arrived");
        assert!(request.params["args"].is_object());
    }

    #[test]
    fn truncate_leaves_short_text_alone_and_marks_long_text() {
        assert_eq!(truncate("short", 10), "short");
        let long = truncate(&"x".repeat(50), 10);
        assert!(long.starts_with(&"x".repeat(10)));
        assert!(long.contains("50 chars total"));
    }
}
