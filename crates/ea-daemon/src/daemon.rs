//! The daemon's IPC surface: the methods the control socket answers, and the
//! state they are answered from.
//!
//! Every method [`Daemon::register`] installs is a thin adapter — parse the
//! params, call into a store or the executor, hand back JSON — so that the
//! rules about what may happen live in one place (`ea_core::policy` behind
//! [`Executor::submit`]) rather than being restated per endpoint.
//!
//! # The gate, stated as a property of this file
//!
//! Every method here is exactly one of two things:
//!
//! * **a read of local state** — `status`, `queue`, `log` — which touches the
//!   SQLite database and the scheduler's flags and nothing else; or
//! * **a path through [`Executor`]** — `propose`, `approve` — where
//!   `Policy::decide` has already run and, for `approve`, a human has said yes
//!   to a specific stored action.
//!
//! `reject`, `pause`, `resume` and `resume_job` reach no connector at all by
//! construction: rejection is a status transition, and pausing or clearing a
//! breaker is a flag the scheduler reads. `chat` starts a model session whose only write tool is
//! `mcp__ea-propose__propose_action`, which comes back to `propose` on this
//! same socket and through the same gate.
//!
//! What must stay absent is a method that names a connector and a tool and
//! calls it. `connectors.call` was removed from this socket once already for
//! exactly that reason; do not put it back. A tool call is an action, and an
//! action reaches a connector only through the gate.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use ea_core::store::actions::{ActionStore, ProposeInput};
use ea_core::store::conversations::ConversationStore;
use ea_core::store::events::EventStore;
use ea_core::store::runs::RunStore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::executor::{Executor, ToolCaller};
use crate::ipc;
use crate::jobs::Pusher;
use crate::scheduler::Scheduler;
use crate::session::SessionRequest;
use crate::triage::SessionBoundary;

/// How long a proposal waits for a human before it expires. The brief's
/// default, and the only one there is until a caller asks for another.
pub const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

/// The longest TTL a caller may ask for: 30 days.
///
/// A proposal is a thing a human taps within days, so 30 days is already
/// generous headroom over the default. It also keeps `Utc::now() + ttl`
/// (`ActionStore::propose`) nowhere near `DateTime<Utc>`'s range limit, so
/// the bound closes both panic sites a wire-supplied `ttl_secs` could hit:
/// `Duration::seconds` overflowing on a value like `i64::MAX`, and the later
/// `DateTime + Duration` overflowing on a value (like `9e15`) that
/// `try_seconds` itself accepts.
pub const MAX_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Default number of `runs` rows `log` returns.
pub const DEFAULT_LOG_LIMIT: i64 = 20;

/// Cap on `log`'s `n`, so a client cannot ask the daemon to serialise the
/// entire ledger into one IPC line (the protocol's own line cap would then
/// truncate it into unparseable JSON).
pub const MAX_LOG_LIMIT: i64 = 500;

/// `kind` recorded on the `runs` row of a chat session.
pub const CHAT_RUN_KIND: &str = "chat";

/// What a chat session is told it is.
pub const CHAT_SYSTEM_PROMPT: &str = concat!(
    "You are the user's executive assistant, answering over a text interface. ",
    "Be brief and concrete. You cannot act directly: the only way to change ",
    "anything in the world is the propose_action tool, which records a proposal ",
    "for the user to approve. Never claim to have done something you only proposed."
);

/// Params of the `propose` method.
///
/// `connector` and `tool` are required: without them there is no action to
/// speak of, and a request missing either is a bug in the caller that should be
/// reported as one. Everything else has a default, because a proposal arriving
/// with a thin `preview` is still a proposal a human can look at, and refusing
/// it would lose information rather than protect anything.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposeParams {
    pub connector: String,
    pub tool: String,
    #[serde(default = "empty_object")]
    pub args: Value,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub rationale: String,
    /// Overrides [`DEFAULT_TTL_SECS`]. Must be in `1..=MAX_TTL_SECS`.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

impl ProposeParams {
    fn into_input(self) -> anyhow::Result<ProposeInput> {
        let ttl_secs = self.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
        // Validate before constructing anything from `ttl_secs`: this is a
        // wire-supplied i64, and both `Duration::seconds` and the later
        // `DateTime + Duration` in `ActionStore::propose` can overflow and
        // panic on values that reach them unchecked (i64::MAX; 9e15, which
        // survives `try_seconds` but overflows once added to `Utc::now()`).
        if ttl_secs <= 0 || ttl_secs > MAX_TTL_SECS {
            bail!(
                "propose: ttl_secs must be between 1 and {MAX_TTL_SECS} (30 days), got {ttl_secs}"
            );
        }
        let ttl = chrono::Duration::try_seconds(ttl_secs).ok_or_else(|| {
            anyhow!("propose: ttl_secs {ttl_secs} is out of range (must be between 1 and {MAX_TTL_SECS})")
        })?;
        Ok(ProposeInput {
            connector: self.connector,
            tool: self.tool,
            args: self.args,
            preview: self.preview,
            rationale: self.rationale,
            ttl,
        })
    }
}

/// Params of `approve`, `reject`.
#[derive(Debug, Clone, Deserialize)]
struct IdParams {
    id: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LogParams {
    #[serde(default)]
    n: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatParams {
    message: String,
}

/// Params of `resume_job`.
#[derive(Debug, Clone, Deserialize)]
struct JobParams {
    job: String,
}

/// Everything the daemon answers from.
///
/// A struct rather than a long argument list because it is assembled once, in
/// `main`, and every field is optional-by-configuration in a different way:
/// `sessions` is absent when `claude` could not be resolved, `pusher` when
/// Telegram is not configured.
pub struct Deps<C: ToolCaller> {
    pub executor: Arc<Executor<C>>,
    pub actions: ActionStore,
    pub conversations: ConversationStore,
    pub events: EventStore,
    pub runs: RunStore,
    pub scheduler: Arc<Scheduler>,
    pub sessions: Option<Arc<dyn SessionBoundary>>,
    pub pusher: Option<Arc<dyn Pusher>>,
    pub connectors: Vec<String>,
    pub daily_session_budget: u32,
}

/// The daemon's state, shared by every connection the IPC server accepts.
pub struct Daemon<C: ToolCaller> {
    executor: Arc<Executor<C>>,
    actions: ActionStore,
    conversations: ConversationStore,
    events: EventStore,
    runs: RunStore,
    scheduler: Arc<Scheduler>,
    sessions: Option<Arc<dyn SessionBoundary>>,
    pusher: Option<Arc<dyn Pusher>>,
    connectors: Vec<String>,
    daily_session_budget: u32,
    started_at: DateTime<Utc>,
}

impl<C: ToolCaller + Send + Sync + 'static> Daemon<C> {
    pub fn build(deps: Deps<C>) -> Arc<Self> {
        Arc::new(Self {
            executor: deps.executor,
            actions: deps.actions,
            conversations: deps.conversations,
            events: deps.events,
            runs: deps.runs,
            scheduler: deps.scheduler,
            sessions: deps.sessions,
            pusher: deps.pusher,
            connectors: deps.connectors,
            daily_session_budget: deps.daily_session_budget,
            started_at: Utc::now(),
        })
    }

    /// Register every method this daemon answers on `server`.
    ///
    /// One place, so that "what can be asked of the daemon over its socket" is
    /// a list you can read in ten seconds rather than something scattered
    /// through `main`. Ten methods; see the module docs for how each one
    /// relates to the gate.
    pub fn register(self: &Arc<Self>, server: &mut ipc::Server) {
        macro_rules! method {
            ($name:literal, $call:ident) => {
                let this = Arc::clone(self);
                server.register($name, move |params| {
                    let this = Arc::clone(&this);
                    Box::pin(async move { this.$call(params).await })
                });
            };
        }

        method!("status", status);
        method!("queue", queue);
        method!("approve", approve);
        method!("reject", reject);
        method!("log", log);
        method!("pause", pause);
        method!("resume", resume);
        method!("resume_job", resume_job);
        method!("chat", chat);
        method!("propose", propose);
    }

    // ----------------------------------------------------------------------
    // Reads of local state
    // ----------------------------------------------------------------------

    /// `status` — is the daemon up, is it paused, what is waiting, and what is
    /// broken.
    ///
    /// The `jobs` array is the part worth caring about. A connector whose
    /// token has lapsed fails every poll until its breaker trips, and then
    /// goes completely silent; without `tripped` and `last_error` here the
    /// only symptom is that nothing ever happens again. Both are reported for
    /// every job, so a failing-but-not-yet-tripped connector is visible too.
    pub async fn status(&self, _params: Value) -> anyhow::Result<Value> {
        let pending = self.actions.pending()?;
        let jobs: Vec<Value> = self
            .scheduler
            .names()
            .into_iter()
            .map(|name| {
                let tripped = self.scheduler.is_tripped(&name);
                let last_error = self.scheduler.last_error(&name);
                // Seconds until the breaker's own half-open retry. Present
                // only while tripped, and the answer to the question a tripped
                // job immediately raises: do I have to do something about
                // this, or will it fix itself?
                let retry_in = self.scheduler.retry_in(&name).map(|d| d.as_secs());
                json!({
                    "name": name,
                    "tripped": tripped,
                    "last_error": last_error,
                    "retry_in_secs": retry_in,
                })
            })
            .collect();

        Ok(json!({
            "status": "ok",
            "paused": self.scheduler.is_paused(),
            "pending_actions": pending.len(),
            // Events triage could not score and gave up on. Nonzero means the
            // daemon has decided not to look at something; that must be
            // visible here rather than only in the log, or it is exactly the
            // silent failure the rest of this endpoint exists to prevent.
            "unscorable_events": self.events.abandoned_count().unwrap_or_default(),
            "jobs": jobs,
            "connectors": self.connectors,
            "sessions_available": self.sessions.is_some(),
            "notifier_configured": self.pusher.is_some(),
            "daily_session_budget": self.daily_session_budget,
            "started_at": self.started_at.to_rfc3339(),
        }))
    }

    /// `queue` — the proposals waiting for a human.
    pub async fn queue(&self, _params: Value) -> anyhow::Result<Value> {
        let pending = self.actions.pending()?;
        serde_json::to_value(pending).context("queue: serialising the pending actions")
    }

    /// `log` — the most recent `runs` rows, newest first.
    pub async fn log(&self, params: Value) -> anyhow::Result<Value> {
        let params: LogParams = parse_params("log", params)?;
        let n = params
            .n
            .unwrap_or(DEFAULT_LOG_LIMIT)
            .clamp(1, MAX_LOG_LIMIT);
        let runs = self.runs.recent(n)?;
        serde_json::to_value(runs).context("log: serialising the runs")
    }

    /// `pause` — stop the scheduler spawning anything new.
    ///
    /// Does not touch work already in flight: a poll that is halfway through
    /// writing events finishes. Reaches no connector.
    pub async fn pause(&self, _params: Value) -> anyhow::Result<Value> {
        self.scheduler.pause();
        tracing::info!("scheduler paused over IPC");
        Ok(json!({ "paused": true }))
    }

    /// `resume` — undo `pause`.
    pub async fn resume(&self, _params: Value) -> anyhow::Result<Value> {
        self.scheduler.resume();
        tracing::info!("scheduler resumed over IPC");
        Ok(json!({ "paused": false }))
    }

    /// `resume_job` — clear one job's tripped circuit breaker.
    ///
    /// The manual half of breaker recovery, and the reason it exists: five
    /// consecutive failures disable a job, and before this there was no way to
    /// re-enable it short of restarting the daemon. The automatic half is the
    /// scheduler's half-open retry, which means a transient outage heals with
    /// nobody watching; this is for the case where the owner has just *fixed*
    /// something and does not want to wait out the cooldown.
    ///
    /// Reaches no connector. It clears three atomics and a timestamp in the
    /// scheduler; whether the job then does anything is decided entirely by
    /// the job itself on the next tick, and a `watch_poll` job is still
    /// subject to the policy check in `jobs::run_watch_poll`.
    ///
    /// An unknown job name is an error naming the jobs that do exist, because
    /// the alternative — answering "ok" to `ea resume canavs` — is the kind of
    /// silence this whole review is about.
    pub async fn resume_job(&self, params: Value) -> anyhow::Result<Value> {
        let params: JobParams = parse_params("resume_job", params)?;
        let was_tripped = self.scheduler.is_tripped(&params.job);
        if !self.scheduler.reset(&params.job) {
            bail!(
                "resume: there is no job called {:?}. Jobs: {}",
                params.job,
                self.scheduler.names().join(", ")
            );
        }
        tracing::info!(job = %params.job, was_tripped, "breaker cleared over IPC");
        Ok(json!({
            "job": params.job,
            "was_tripped": was_tripped,
            "tripped": false,
        }))
    }

    // ----------------------------------------------------------------------
    // Paths through the executor
    // ----------------------------------------------------------------------

    /// `propose` — put an action through the policy gate.
    ///
    /// Returns the resulting `Action`, serialised. The caller reads its
    /// `status` to learn which of the three things happened: `executed` (policy
    /// said auto), `proposed` (a human must approve it), `rejected` (policy
    /// said no).
    ///
    /// An action naming a connector nobody has ever heard of is *not* an error
    /// here. `Policy::decide` defaults an unknown connector to `approve`, so it
    /// is recorded and queued for a human to look at — which is the correct
    /// response to a model hallucinating a tool: a person sees the nonsense,
    /// nothing is executed, and the session does not crash. Erroring instead
    /// would teach the model to retry; executing would be catastrophic.
    ///
    /// A proposal that needs a human is pushed to Telegram from here, because
    /// this is the moment it starts waiting. A failed push is logged and does
    /// not fail the proposal: the action is recorded, and `ea queue` still
    /// shows it.
    pub async fn propose(&self, params: Value) -> anyhow::Result<Value> {
        let params: ProposeParams = serde_json::from_value(params)
            .context("propose: invalid params; `connector` and `tool` are required strings")?;
        let (action, _executed) = self.executor.submit(params.into_input()?).await?;

        if action.status == ea_core::store::actions::ActionStatus::Proposed {
            self.push(action.id).await;
        }

        serde_json::to_value(action).context("propose: serialising the action")
    }

    /// `approve` — the human said yes to a specific stored action.
    ///
    /// Two steps, in this order and no other: the `proposed -> approved`
    /// transition (one conditional UPDATE, so a double approval cannot produce
    /// two executions), then the executor. This method never decides anything
    /// — it carries a decision a human already made about a row that
    /// `Policy::decide` put in front of them.
    pub async fn approve(&self, params: Value) -> anyhow::Result<Value> {
        let params: IdParams = parse_params("approve", params)?;
        // Propagates "action N does not exist" and "action N is executed, not
        // proposed" straight from the store, which is what the CLI shows.
        self.actions.approve(params.id)?;
        let action = self.executor.execute_approved(params.id).await?;
        serde_json::to_value(action).context("approve: serialising the action")
    }

    /// `reject` — the human said no. No connector is reached on this path; it
    /// is a status transition and nothing else.
    pub async fn reject(&self, params: Value) -> anyhow::Result<Value> {
        let params: IdParams = parse_params("reject", params)?;
        let reason = params
            .reason
            .unwrap_or_else(|| "rejected by the user".into());
        let action = self.actions.reject(params.id, &reason)?;
        serde_json::to_value(action).context("reject: serialising the action")
    }

    /// `chat` — say something to the assistant.
    ///
    /// The message is appended to the current conversation first and
    /// unconditionally, so nothing the human said is lost even if the session
    /// then fails, is skipped for budget, or the daemon is running without a
    /// `claude` binary at all. The conversation id comes back either way.
    ///
    /// A chat session gets no connectors and one write tool
    /// (`mcp__ea-propose__propose_action`, via `session::ALLOWED_TOOLS`), which
    /// re-enters this daemon at `propose` and goes through the gate like
    /// anything else. Nothing it can do reaches a connector directly.
    pub async fn chat(&self, params: Value) -> anyhow::Result<Value> {
        let params: ChatParams = parse_params("chat", params)?;
        let message = params.message.trim().to_string();
        if message.is_empty() {
            bail!("chat: `message` must not be empty");
        }

        let conversation_id = self.conversations.current()?;
        let stored = self
            .conversations
            .append(conversation_id, "user", "cli", &message)?;

        let (reply, note) = match self.reply_to(conversation_id, &message).await {
            Ok(Some(reply)) => (Some(reply), None),
            Ok(None) => (None, Some(self.why_no_reply())),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "chat session failed");
                (None, Some(format!("the session failed: {err:#}")))
            }
        };

        if let Some(reply) = &reply {
            self.conversations
                .append(conversation_id, "assistant", "cli", reply)?;
        }

        Ok(json!({
            "conversation_id": conversation_id,
            "message_id": stored.id,
            "reply": reply,
            "note": note,
        }))
    }

    /// Run one chat session, or `None` when there is no session runner or the
    /// day's budget is spent.
    async fn reply_to(
        &self,
        conversation_id: i64,
        message: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(sessions) = self.sessions.as_ref() else {
            return Ok(None);
        };
        if crate::jobs::session_budget_spent(&self.runs, self.daily_session_budget, Utc::now())? {
            return Ok(None);
        }

        let mut request = SessionRequest::new(CHAT_RUN_KIND, message, CHAT_SYSTEM_PROMPT)
            // Read tools are not in `session::ALLOWED_TOOLS` anyway; handing a
            // chat session connector servers would spawn children it cannot
            // call.
            .with_connectors(Vec::<String>::new());
        if let Some(previous) = self.conversations.claude_session(conversation_id)? {
            request = request.with_resume(previous);
        }

        let outcome = sessions.run_session(request).await?;
        if let Some(session_id) = &outcome.session_id {
            // Recorded so the next message continues the same thread rather
            // than starting from nothing.
            self.conversations
                .set_claude_session(conversation_id, session_id)?;
        }
        Ok(Some(outcome.text))
    }

    fn why_no_reply(&self) -> String {
        if self.sessions.is_none() {
            "recorded, but this daemon has no session runner (is `claude` on PATH?), \
             so there is no reply"
                .to_string()
        } else {
            format!(
                "recorded, but the daily session budget of {} is spent, so there is no reply",
                self.daily_session_budget
            )
        }
    }

    /// Put a proposal in front of the human, if there is a way to.
    async fn push(&self, id: i64) {
        let Some(pusher) = self.pusher.as_ref() else {
            tracing::debug!(action = id, "no notifier configured; not pushing");
            return;
        };
        if let Err(err) = pusher.push_action(id).await {
            tracing::warn!(
                action = id,
                error = %format!("{err:#}"),
                "could not push the proposal; it is still in `ea queue`"
            );
        }
    }
}

/// Parse a method's params, with an error that names the method.
///
/// `null` is treated as `{}`: `ea pause` sends no params, and a method whose
/// fields are all optional should accept that rather than making every CLI
/// subcommand send an empty object.
fn parse_params<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> anyhow::Result<T> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).with_context(|| format!("{method}: invalid params"))
}

/// How long the daemon waits for in-flight scheduler work on the way out.
///
/// `launchd` sends SIGTERM and then SIGKILL after its own grace period, so
/// this has to be comfortably shorter than that. Long enough for a connector
/// poll to finish writing what it has read.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ea_core::policy::Policy;
    use ea_core::store::runs::RunStore;
    use tempfile::TempDir;

    use super::*;
    use crate::scheduler::Job;
    use crate::session::SessionOutcome;
    use crate::triage::BoxedSession;

    type CallLog = Arc<Mutex<Vec<(String, String)>>>;

    /// Records every connector call it is asked to make, so a test can assert
    /// the far stronger property than "the status looks right": that the
    /// connector was never reached at all.
    ///
    /// The log is shared rather than owned because `Executor` deliberately does
    /// not hand its caller back out.
    #[derive(Default, Clone)]
    struct SpyCaller {
        calls: CallLog,
    }

    impl ToolCaller for SpyCaller {
        async fn call(&self, connector: &str, tool: &str, _args: Value) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string()));
            Ok("done".to_string())
        }
    }

    /// A session runner that never spawns anything. No test in the default
    /// suite may start a real `claude`: it costs money and its answer is not
    /// deterministic.
    struct FakeSessions {
        reply: String,
        session_id: Option<String>,
        seen: Arc<Mutex<Vec<SessionRequest>>>,
        /// Written to exactly as the real `SessionRunner` does, because the
        /// daily budget is counted from `runs` rows: a fake that skipped this
        /// would make the budget test pass for the wrong reason.
        runs: RunStore,
    }

    impl FakeSessions {
        fn new(reply: &str, runs: RunStore) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                session_id: Some("sess-1".to_string()),
                seen: Arc::new(Mutex::new(Vec::new())),
                runs,
            })
        }
    }

    impl SessionBoundary for FakeSessions {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            let run_id = self.runs.start(&req.kind, &req.prompt, &[]).unwrap();
            self.seen.lock().unwrap().push(req);
            let reply = self.reply.clone();
            let session_id = self.session_id.clone();
            let runs = self.runs.clone();
            Box::pin(async move {
                runs.finish(run_id, "ok", Some(&reply), &[], Some(0.001))?;
                Ok(SessionOutcome {
                    text: reply,
                    structured: None,
                    session_id,
                    cost_usd: Some(0.001),
                })
            })
        }
    }

    #[derive(Default)]
    struct FakePusher {
        pushed: Mutex<Vec<i64>>,
    }

    impl FakePusher {
        fn pushed(&self) -> Vec<i64> {
            self.pushed.lock().unwrap().clone()
        }
    }

    impl Pusher for FakePusher {
        fn notify<'a>(
            &'a self,
            _text: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }

        fn push_action(
            &self,
            id: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            self.pushed.lock().unwrap().push(id);
            Box::pin(async { Ok(()) })
        }
    }

    fn policy() -> Policy {
        Policy::parse(
            r#"
[canvas]
list_courses = "auto"

[fortnox]
record_voucher = "approve"
"#,
        )
        .unwrap()
    }

    struct Fixture {
        _dir: TempDir,
        daemon: Arc<Daemon<SpyCaller>>,
        log: CallLog,
        scheduler: Arc<Scheduler>,
        pusher: Arc<FakePusher>,
        sessions: Option<Arc<FakeSessions>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::build(true, 60)
        }

        /// Without a session runner: the shape a daemon has when `claude`
        /// could not be resolved at startup.
        fn without_sessions() -> Self {
            Self::build(false, 60)
        }

        fn build(with_sessions: bool, budget: u32) -> Self {
            let dir = TempDir::new().unwrap();
            let conn = Arc::new(Mutex::new(
                ea_core::db::open(&dir.path().join("state.db")).unwrap(),
            ));
            let sessions = with_sessions.then(|| {
                FakeSessions::new("here is your answer", RunStore::new(Arc::clone(&conn)))
            });
            let caller = SpyCaller::default();
            let log = Arc::clone(&caller.calls);
            let executor = Arc::new(Executor::new(
                ActionStore::new(Arc::clone(&conn)),
                RunStore::new(Arc::clone(&conn)),
                policy(),
                caller,
            ));
            let scheduler = Arc::new(Scheduler::new(3));
            scheduler.add(Job::new("canvas", Duration::from_secs(1800), || async {
                Ok(())
            }));
            scheduler.add(Job::new("triage", Duration::from_secs(300), || async {
                Ok(())
            }));
            let pusher = Arc::new(FakePusher::default());

            let daemon = Daemon::build(Deps {
                executor,
                actions: ActionStore::new(Arc::clone(&conn)),
                conversations: ConversationStore::new(Arc::clone(&conn)),
                events: EventStore::new(Arc::clone(&conn)),
                runs: RunStore::new(conn),
                scheduler: Arc::clone(&scheduler),
                sessions: sessions.clone().map(|s| s as Arc<dyn SessionBoundary>),
                pusher: Some(Arc::clone(&pusher) as Arc<dyn Pusher>),
                connectors: vec!["canvas".to_string()],
                daily_session_budget: budget,
            });

            Self {
                _dir: dir,
                daemon,
                log,
                scheduler,
                pusher,
                sessions,
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.log.lock().unwrap().clone()
        }

        async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
            match method {
                "status" => self.daemon.status(params).await,
                "queue" => self.daemon.queue(params).await,
                "approve" => self.daemon.approve(params).await,
                "reject" => self.daemon.reject(params).await,
                "log" => self.daemon.log(params).await,
                "pause" => self.daemon.pause(params).await,
                "resume" => self.daemon.resume(params).await,
                "resume_job" => self.daemon.resume_job(params).await,
                "chat" => self.daemon.chat(params).await,
                "propose" => self.daemon.propose(params).await,
                other => panic!("no such method {other}"),
            }
        }
    }

    fn params(connector: &str, tool: &str) -> Value {
        json!({
            "connector": connector,
            "tool": tool,
            "args": { "term": "HT26" },
            "preview": format!("{connector}.{tool}"),
            "rationale": "the agent asked",
        })
    }

    // -- the brief's IPC tests ----------------------------------------------

    #[tokio::test]
    async fn status_reports_a_running_unpaused_daemon_with_an_empty_queue() {
        let f = Fixture::new();
        let status = f.call("status", Value::Null).await.unwrap();
        assert_eq!(status["status"], "ok");
        assert_eq!(status["paused"], false);
        assert_eq!(status["pending_actions"], 0);
    }

    #[tokio::test]
    async fn queue_lists_a_proposal() {
        let f = Fixture::new();
        f.call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();

        let queue = f.call("queue", Value::Null).await.unwrap();
        let rows = queue.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["connector"], "fortnox");
        assert_eq!(rows[0]["tool"], "record_voucher");
        assert_eq!(rows[0]["status"], "proposed");

        let status = f.call("status", Value::Null).await.unwrap();
        assert_eq!(status["pending_actions"], 1);
    }

    #[tokio::test]
    async fn approve_executes_through_ipc() {
        let f = Fixture::new();
        let proposed = f
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        let id = proposed["id"].as_i64().unwrap();
        assert!(f.calls().is_empty(), "nothing runs before approval");

        let action = f.call("approve", json!({ "id": id })).await.unwrap();
        assert_eq!(action["status"], "executed");
        assert_eq!(action["result"], "done");
        assert_eq!(
            f.calls(),
            vec![("fortnox".to_string(), "record_voucher".to_string())],
            "approval is the only thing that reaches the connector"
        );

        // And it leaves the queue.
        assert!(
            f.call("queue", Value::Null).await.unwrap()[0].is_null()
                || f.call("queue", Value::Null)
                    .await
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .is_empty()
        );
    }

    #[tokio::test]
    async fn reject_rejects_and_never_calls_the_connector() {
        let f = Fixture::new();
        let id = f
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap();

        let action = f
            .call("reject", json!({ "id": id, "reason": "not this month" }))
            .await
            .unwrap();
        assert_eq!(action["status"], "rejected");
        assert_eq!(action["reason"], "not this month");
        assert!(f.calls().is_empty(), "rejection must reach no connector");
    }

    #[tokio::test]
    async fn approving_an_unknown_id_says_it_does_not_exist() {
        let f = Fixture::new();
        let err = f.call("approve", json!({ "id": 9999 })).await.unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"), "{err:#}");
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn pause_then_status_shows_paused_and_resume_clears_it() {
        let f = Fixture::new();
        f.call("pause", Value::Null).await.unwrap();
        assert_eq!(f.call("status", Value::Null).await.unwrap()["paused"], true);
        assert!(f.scheduler.is_paused());

        f.call("resume", Value::Null).await.unwrap();
        assert_eq!(
            f.call("status", Value::Null).await.unwrap()["paused"],
            false
        );
        assert!(!f.scheduler.is_paused());
    }

    #[tokio::test]
    async fn chat_appends_a_message_and_returns_the_conversation_id() {
        let f = Fixture::new();
        let first = f
            .call("chat", json!({ "message": "what is due this week?" }))
            .await
            .unwrap();
        let conversation_id = first["conversation_id"].as_i64().unwrap();
        assert!(conversation_id > 0);
        assert!(first["message_id"].as_i64().unwrap() > 0);
        assert_eq!(first["reply"], "here is your answer");

        // The same conversation, and the session was resumed rather than
        // started again.
        let second = f
            .call("chat", json!({ "message": "and next?" }))
            .await
            .unwrap();
        assert_eq!(second["conversation_id"].as_i64().unwrap(), conversation_id);

        let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].resume.is_none());
        assert_eq!(seen[1].resume.as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn log_returns_the_runs_newest_first() {
        let f = Fixture::new();
        // An auto action writes one `runs` row through the executor.
        f.call("propose", params("canvas", "list_courses"))
            .await
            .unwrap();
        f.call("chat", json!({ "message": "hello" })).await.unwrap();

        let rows = f.call("log", json!({ "n": 10 })).await.unwrap();
        let rows = rows.as_array().unwrap();
        assert!(rows.len() >= 2, "{rows:?}");
        assert!(rows[0]["id"].as_i64().unwrap() > rows[1]["id"].as_i64().unwrap());
    }

    #[tokio::test]
    async fn log_clamps_a_silly_limit() {
        let f = Fixture::new();
        assert!(f.call("log", json!({ "n": 10_000_000 })).await.is_ok());
        assert!(f.call("log", json!({ "n": -5 })).await.is_ok());
        assert!(f.call("log", Value::Null).await.is_ok());
    }

    // -- Addition 4: a tripped breaker is visible, with its reason ----------

    #[tokio::test]
    async fn status_names_every_job_and_says_which_are_tripped_and_why() {
        let f = Fixture::new();
        let status = f.call("status", Value::Null).await.unwrap();
        let jobs = status["jobs"].as_array().unwrap();
        let names: Vec<&str> = jobs.iter().map(|j| j["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["canvas", "triage"]);
        assert!(jobs.iter().all(|j| j["tripped"] == false));
        assert!(jobs.iter().all(|j| j["last_error"].is_null()));

        // Trip the canvas breaker the way a lapsed token would.
        let failing = Arc::new(Scheduler::new(2));
        failing.add(Job::new("canvas", Duration::from_secs(1), || async {
            Err(anyhow!(
                "canvas: 401 Unauthorized (the access token has expired)"
            ))
        }));
        for _ in 0..2 {
            for handle in failing.tick(std::time::Instant::now()) {
                handle.await.unwrap();
            }
            // Let the interval elapse so the second tick actually runs.
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        assert!(failing.is_tripped("canvas"));

        let f2 = Fixture::new();
        let daemon = Daemon::build(Deps {
            executor: Arc::clone(&f2.daemon.executor),
            actions: f2.daemon.actions.clone(),
            conversations: f2.daemon.conversations.clone(),
            events: f2.daemon.events.clone(),
            runs: f2.daemon.runs.clone(),
            scheduler: Arc::clone(&failing),
            sessions: None,
            pusher: None,
            connectors: vec!["canvas".to_string()],
            daily_session_budget: 60,
        });
        let status = daemon.status(Value::Null).await.unwrap();
        let canvas = &status["jobs"][0];
        assert_eq!(canvas["tripped"], true);
        let reason = canvas["last_error"].as_str().unwrap();
        assert!(
            reason.contains("401"),
            "a tripped breaker must say why: {reason}"
        );
        assert!(
            canvas["retry_in_secs"].as_u64().is_some(),
            "a tripped breaker must say when it will retry itself: {canvas}"
        );
    }

    /// An event triage could not score must be countable from `ea status`.
    /// The alternative is that the daemon quietly stops looking at something
    /// and the only record is a log line nobody reads.
    #[tokio::test]
    async fn status_counts_the_events_triage_gave_up_on() {
        let f = Fixture::new();
        let before = f.call("status", Value::Null).await.unwrap();
        assert_eq!(before["unscorable_events"], 0);

        let id = f
            .daemon
            .events
            .record(ea_core::store::events::RecordInput {
                source: "canvas".into(),
                external_id: "e1".into(),
                kind: "assignment".into(),
                payload: json!({ "title": "essay" }),
            })
            .unwrap()
            .0
            .id;
        f.daemon
            .events
            .abandon(id, "tier 1 never scored it")
            .unwrap();

        let after = f.call("status", Value::Null).await.unwrap();
        assert_eq!(after["unscorable_events"], 1);
    }

    // -- clearing a tripped breaker without a restart -----------------------

    /// A scheduler with one job that always fails and a breaker threshold of
    /// one, plus a daemon wired to it. The finding this covers: before
    /// `resume_job`, the only way out of this state was to restart the daemon.
    async fn tripped_fixture() -> (Fixture, Arc<Scheduler>, Arc<Daemon<SpyCaller>>) {
        let f = Fixture::new();
        let scheduler = Arc::new(Scheduler::new(1));
        scheduler.add(Job::new("canvas", Duration::from_secs(1800), || async {
            Err(anyhow!("canvas: no usable credentials"))
        }));
        for handle in scheduler.tick(std::time::Instant::now()) {
            handle.await.unwrap();
        }
        assert!(scheduler.is_tripped("canvas"));

        let daemon = Daemon::build(Deps {
            executor: Arc::clone(&f.daemon.executor),
            actions: f.daemon.actions.clone(),
            conversations: f.daemon.conversations.clone(),
            events: f.daemon.events.clone(),
            runs: f.daemon.runs.clone(),
            scheduler: Arc::clone(&scheduler),
            sessions: None,
            pusher: None,
            connectors: vec!["canvas".to_string()],
            daily_session_budget: 60,
        });
        (f, scheduler, daemon)
    }

    #[tokio::test]
    async fn resume_job_clears_a_tripped_breaker_and_status_agrees() {
        let (_f, scheduler, daemon) = tripped_fixture().await;

        let before = daemon.status(Value::Null).await.unwrap();
        assert_eq!(before["jobs"][0]["tripped"], true);

        let answer = daemon
            .resume_job(json!({ "job": "canvas" }))
            .await
            .expect("resuming a known job must succeed");
        assert_eq!(answer["job"], "canvas");
        assert_eq!(answer["was_tripped"], true);
        assert_eq!(answer["tripped"], false);
        assert!(!scheduler.is_tripped("canvas"));

        let after = daemon.status(Value::Null).await.unwrap();
        assert_eq!(after["jobs"][0]["tripped"], false);
        assert!(after["jobs"][0]["last_error"].is_null());
        assert!(after["jobs"][0]["retry_in_secs"].is_null());
    }

    /// Answering "ok" to a typo would be exactly the silent failure this
    /// review is about: the owner would think they had fixed it.
    #[tokio::test]
    async fn resume_job_refuses_a_name_no_job_has() {
        let (_f, scheduler, daemon) = tripped_fixture().await;
        let err = daemon
            .resume_job(json!({ "job": "canavs" }))
            .await
            .expect_err("an unknown job must be an error");
        let text = format!("{err:#}");
        assert!(text.contains("canavs"), "{text}");
        assert!(
            text.contains("canvas"),
            "it must list the real jobs: {text}"
        );
        assert!(
            scheduler.is_tripped("canvas"),
            "a typo must not clear anything"
        );
    }

    #[tokio::test]
    async fn resume_job_reaches_no_connector() {
        let (f, _scheduler, daemon) = tripped_fixture().await;
        daemon.resume_job(json!({ "job": "canvas" })).await.unwrap();
        assert!(
            f.calls().is_empty(),
            "clearing a breaker must not call anything: {:?}",
            f.calls()
        );
    }

    // -- the gate -----------------------------------------------------------

    #[tokio::test]
    async fn an_auto_tool_comes_back_executed() {
        let f = Fixture::new();
        let action = f
            .call("propose", params("canvas", "list_courses"))
            .await
            .unwrap();
        assert_eq!(action["status"], "executed");
        assert_eq!(action["result"], "done");
        assert_eq!(
            f.calls(),
            vec![("canvas".to_string(), "list_courses".to_string())]
        );
    }

    #[tokio::test]
    async fn an_approve_tool_comes_back_proposed() {
        let f = Fixture::new();
        let action = f
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(f.calls().is_empty(), "the connector must not be reached");
    }

    /// A proposal that needs a human must actually reach the human, or the
    /// phone half of this system does nothing.
    #[tokio::test]
    async fn a_proposal_awaiting_a_human_is_pushed_and_an_auto_one_is_not() {
        let f = Fixture::new();
        let id = f
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap()["id"]
            .as_i64()
            .unwrap();
        assert_eq!(f.pusher.pushed(), vec![id]);

        f.call("propose", params("canvas", "list_courses"))
            .await
            .unwrap();
        assert_eq!(
            f.pusher.pushed(),
            vec![id],
            "an auto action has already run; there is nothing to approve"
        );
    }

    /// Review focus. A model that invents a connector must not crash the loop
    /// and must not be executed: the gate's `approve` default queues the
    /// nonsense for a human to see.
    #[tokio::test]
    async fn an_unknown_connector_is_queued_for_a_human_not_executed_and_not_an_error() {
        let f = Fixture::new();
        let action = f
            .call(
                "propose",
                params("definitely-not-a-connector", "transfer_everything"),
            )
            .await
            .expect("an unknown connector must not be an error");
        assert_eq!(action["status"], "proposed");
        assert_eq!(action["connector"], "definitely-not-a-connector");
        assert!(
            f.calls().is_empty(),
            "no connector call may be attempted for a connector that does not exist"
        );

        let next = f
            .call("propose", params("canvas", "list_courses"))
            .await
            .unwrap();
        assert_eq!(next["status"], "executed");
    }

    #[tokio::test]
    async fn an_unknown_tool_on_a_known_connector_is_also_queued() {
        let f = Fixture::new();
        let action = f
            .call("propose", params("canvas", "invented_tool"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn the_default_ttl_is_twenty_four_hours() {
        let f = Fixture::new();
        let action = f
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        assert_eq!(ttl_of(&action), DEFAULT_TTL_SECS);
    }

    fn ttl_of(action: &Value) -> i64 {
        let created: DateTime<Utc> = action["created_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<chrono::FixedOffset>>()
            .unwrap()
            .into();
        let expires: DateTime<Utc> = action["expires_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<chrono::FixedOffset>>()
            .unwrap()
            .into();
        (expires - created).num_seconds()
    }

    #[tokio::test]
    async fn a_non_positive_ttl_is_refused() {
        let f = Fixture::new();
        let mut p = params("fortnox", "record_voucher");
        p["ttl_secs"] = json!(0);
        let err = f.call("propose", p).await.unwrap_err();
        assert!(format!("{err:#}").contains("ttl_secs"), "{err:#}");
    }

    #[tokio::test]
    async fn a_sensible_ttl_still_works() {
        let f = Fixture::new();
        let mut p = params("fortnox", "record_voucher");
        p["ttl_secs"] = json!(3600);
        let action = f.call("propose", p).await.unwrap();
        assert_eq!(ttl_of(&action), 3600);
    }

    /// Review focus. `ttl_secs` is a wire-supplied i64 that used to reach
    /// `Duration::seconds` (panics on `i64::MAX`) and, past that, the
    /// `Utc::now() + ttl` in `ActionStore::propose` (panics on a value like
    /// `9e15`, which survives `try_seconds`). Both panics used to happen
    /// inside the connection task, so the caller saw a hang-up instead of an
    /// error. Every out-of-bounds value here must instead come back as a
    /// clean IPC error naming the bound, over a real socket, with the socket
    /// still serving afterwards.
    #[tokio::test]
    async fn an_out_of_bounds_ttl_is_a_clean_ipc_error_and_the_daemon_keeps_serving() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();

        let client = ea_core::ipc::Client::new(&path);

        for bad_ttl in [i64::MAX, 9_000_000_000_000_000i64, i64::MIN, 0i64, -1i64] {
            let mut p = params("fortnox", "record_voucher");
            p["ttl_secs"] = json!(bad_ttl);
            let err = client
                .call("propose", p)
                .await
                .expect_err(&format!("ttl_secs {bad_ttl} must be refused, not accepted"));
            let text = format!("{err:#}");
            assert!(text.contains("ttl_secs"), "{bad_ttl}: {text}");
            assert!(
                text.contains(&MAX_TTL_SECS.to_string()),
                "{bad_ttl}: error must name the bound: {text}"
            );
        }

        let action = client
            .call("propose", params("canvas", "list_courses"))
            .await
            .expect("the daemon must survive every out-of-bounds ttl_secs");
        assert_eq!(action["status"], "executed");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn params_missing_the_tool_are_an_error() {
        let f = Fixture::new();
        let err = f
            .call(
                "propose",
                json!({ "connector": "fortnox", "preview": "x", "rationale": "y" }),
            )
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("invalid params"), "{text}");
        assert!(text.contains("tool"), "{text}");
        assert!(f.calls().is_empty());
    }

    /// The same malformed request over a real socket, because "returns an IPC
    /// error" and "the daemon stays up" are properties of the server, not of
    /// the handler.
    #[tokio::test]
    async fn malformed_params_are_an_ipc_error_and_the_daemon_keeps_serving() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();

        let client = ea_core::ipc::Client::new(&path);

        let err = client
            .call(
                "propose",
                json!({ "connector": "fortnox", "preview": "x", "rationale": "y" }),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid params"), "{err:#}");

        let action = client
            .call("propose", params("canvas", "list_courses"))
            .await
            .expect("the daemon must survive a malformed request");
        assert_eq!(action["status"], "executed");

        handle.shutdown().await;
    }

    /// Every registered method, over a real socket, in one place — and an
    /// unknown one refused. This is the list the report's gating table is
    /// checked against.
    #[tokio::test]
    async fn every_registered_method_answers_over_the_socket() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();
        let client = ea_core::ipc::Client::new(&path);

        let proposed = client
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        let id = proposed["id"].as_i64().unwrap();

        for (method, params) in [
            ("status", Value::Null),
            ("queue", Value::Null),
            ("log", json!({ "n": 5 })),
            ("pause", Value::Null),
            ("resume", Value::Null),
            ("resume_job", json!({ "job": "canvas" })),
            ("chat", json!({ "message": "hi" })),
            ("reject", json!({ "id": id })),
        ] {
            client
                .call(method, params)
                .await
                .unwrap_or_else(|err| panic!("{method} failed: {err:#}"));
        }

        let err = client
            .call("connectors.call", json!({}))
            .await
            .expect_err("an unregistered method must be refused");
        assert!(format!("{err:#}").contains("connectors.call"), "{err:#}");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_proposal_round_trips_over_the_socket() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();

        let action = ea_core::ipc::Client::new(&path)
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(action["id"].as_i64().unwrap() > 0);

        handle.shutdown().await;
    }

    // -- chat without a session runner, and against the budget --------------

    #[tokio::test]
    async fn chat_still_records_the_message_with_no_session_runner() {
        let f = Fixture::without_sessions();
        let reply = f.call("chat", json!({ "message": "hello" })).await.unwrap();
        assert!(reply["conversation_id"].as_i64().unwrap() > 0);
        assert!(reply["reply"].is_null());
        assert!(reply["note"].as_str().unwrap().contains("claude"));
    }

    #[tokio::test]
    async fn chat_refuses_an_empty_message() {
        let f = Fixture::new();
        let err = f
            .call("chat", json!({ "message": "   " }))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("must not be empty"), "{err:#}");
    }

    /// The budget is a ceiling on model spend, and it has to actually stop a
    /// session rather than merely being reported.
    #[tokio::test]
    async fn chat_stops_starting_sessions_once_the_daily_budget_is_spent() {
        let f = Fixture::build(true, 2);
        for _ in 0..2 {
            let reply = f.call("chat", json!({ "message": "hi" })).await.unwrap();
            assert_eq!(reply["reply"], "here is your answer");
        }
        let third = f
            .call("chat", json!({ "message": "hi again" }))
            .await
            .unwrap();
        assert!(third["reply"].is_null(), "{third}");
        assert!(third["note"].as_str().unwrap().contains("budget"));
        assert_eq!(
            f.sessions.as_ref().unwrap().seen.lock().unwrap().len(),
            2,
            "no third session may be started"
        );
        // The message is still recorded, budget or no budget.
        assert!(third["message_id"].as_i64().unwrap() > 0);
    }

    /// A chat session must not be handed connector servers, and must reach the
    /// world only through propose_action.
    #[tokio::test]
    async fn a_chat_session_is_scoped_to_no_connectors() {
        let f = Fixture::new();
        f.call("chat", json!({ "message": "hi" })).await.unwrap();
        let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
        assert!(seen[0].connectors.is_empty());
        assert_eq!(
            crate::session::ALLOWED_TOOLS,
            "mcp__ea-propose__propose_action"
        );
    }
}
