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
//! breaker is a flag the scheduler reads. `remember`, `facts` and `forget` are
//! the same kind of thing one layer in: they read and write the `facts` table
//! and touch nothing else, which is exactly why `remember` is `auto` — a fact
//! is internal state, not an effect on the world. `chat` starts a model
//! session whose only write tools are `mcp__ea-propose__propose_action` and
//! `mcp__ea-propose__remember`, both of which come back to this same socket:
//! the first through the gate, the second into the `facts` table.
//!
//! What must stay absent is a method that names a connector and a tool and
//! calls it. `connectors.call` was removed from this socket once already for
//! exactly that reason; do not put it back. A tool call is an action, and an
//! action reaches a connector only through the gate.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use ea_core::store::actions::{ActionStore, ProposeInput};
use ea_core::store::events::EventStore;
use ea_core::store::kv::KvStore;
use ea_core::store::runs::RunStore;
use ea_core::store::schedules::ScheduleStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::chat::ChatService;
use crate::executor::{Executor, ToolCaller};
use crate::ipc;
use crate::jobs::Pusher;
use crate::notify::log::NotificationLog;
use crate::scheduler::Scheduler;
use crate::schedules::Schedule;

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

/// The `kv` key the owner's pause intent is recorded under.
///
/// Written by [`Daemon::pause`] and [`Daemon::resume`] -- the IPC operations
/// the owner actually invokes -- and by nothing else. In particular never by
/// [`Scheduler::pause`]; see the note on [`Daemon::pause`].
pub const PAUSED_KEY: &str = "scheduler.paused";

/// How long a pause may stand before `status` starts saying it is expensive.
///
/// Thirty days, against the 45 the Fortnox OAuth refresh token survives
/// unused: a fortnight of headroom is enough for the owner to see the warning
/// on a trip and act on it, and short enough that a pause set and forgotten in
/// the first week of a long absence is still recoverable.
const PAUSE_WARN_AFTER_DAYS: i64 = 30;

/// How long the Fortnox refresh token survives unused, in days. Only used to
/// phrase the warning.
const FORTNOX_REFRESH_LAPSE_DAYS: i64 = 45;

/// The owner's pause intent, as persisted in `kv` under [`PAUSED_KEY`].
///
/// `since` exists for the trade-off a durable pause buys: before this, a
/// reboot accidentally rescued a forgotten pause, and now nothing does. The
/// answer is not an expiry that silently un-pauses -- that would give the
/// original defect back -- but making the state legible, so `status` can say
/// how long the daemon has been down and what that is about to cost.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct PauseState {
    pub paused: bool,
    /// When the pause was recorded. `None` whenever `paused` is false.
    pub since: Option<DateTime<Utc>>,
}

/// Apply the persisted pause intent to `scheduler`, and answer what it was.
///
/// `main` calls this before `scheduler.start()`, so that a daemon that was
/// paused when the machine rebooted comes back paused rather than quietly
/// resuming polling and spending -- the plist sets `KeepAlive`, so the restart
/// the owner never sees is the normal case, and "away from the machine" is
/// exactly the situation `pause` exists for.
///
/// Unreadable or unparseable state degrades to "not paused" (via
/// [`KvStore::get_json`]): refusing to start would be a worse failure than
/// running, and the owner can see the daemon is running.
pub fn restore_pause(kv: &KvStore, scheduler: &Scheduler) -> PauseState {
    let state: PauseState = kv.get_json(PAUSED_KEY).unwrap_or_default();
    if state.paused {
        scheduler.pause();
    }
    state
}

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
    /// Where the message came from. Defaults to the terminal, because that is
    /// the client that does not say: `ea chat` is one process and one socket,
    /// while Telegram messages do not reach this method at all — they go
    /// straight to the same [`ChatService`] from the update loop.
    #[serde(default = "default_surface")]
    surface: String,
}

fn default_surface() -> String {
    crate::chat::SURFACE_CLI.to_string()
}

/// Params of `remember`.
#[derive(Debug, Clone, Deserialize)]
struct RememberParams {
    topic: String,
    body: String,
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
    pub events: EventStore,
    pub runs: RunStore,
    pub scheduler: Arc<Scheduler>,
    /// The cron entries, read-only here: `status` answers "when is the next
    /// briefing", which is the one question a scheduled job raises that the
    /// interval jobs never did.
    pub schedules: ScheduleStore,
    /// The zone the cron entries are evaluated in — the owner's, the same one
    /// quiet hours use.
    pub time_zone: Tz,
    /// The conversation, shared with the Telegram update loop: both surfaces
    /// are clients of one thread, and one turn runs at a time. It owns the
    /// session runner, the chat model, the daily budget and the `facts` table
    /// it injects, so this struct no longer carries any of them separately.
    pub chat: Arc<ChatService>,
    pub pusher: Option<Arc<dyn Pusher>>,
    /// Read-only here: `status` reports how much is waiting for a digest
    /// nobody delivers yet, and how much the cap has thrown away.
    pub notify_log: NotificationLog,
    /// Where `pause` and `resume` record the owner's intent so it survives a
    /// restart, and where `status` reads it back from. The same `kv` table
    /// `notify_log` uses, under [`PAUSED_KEY`].
    pub kv: KvStore,
    pub connectors: Vec<String>,
}

/// The daemon's state, shared by every connection the IPC server accepts.
pub struct Daemon<C: ToolCaller> {
    executor: Arc<Executor<C>>,
    actions: ActionStore,
    events: EventStore,
    runs: RunStore,
    scheduler: Arc<Scheduler>,
    schedules: ScheduleStore,
    time_zone: Tz,
    chat: Arc<ChatService>,
    pusher: Option<Arc<dyn Pusher>>,
    notify_log: NotificationLog,
    kv: KvStore,
    connectors: Vec<String>,
    started_at: DateTime<Utc>,
}

impl<C: ToolCaller + Send + Sync + 'static> Daemon<C> {
    pub fn build(deps: Deps<C>) -> Arc<Self> {
        Arc::new(Self {
            executor: deps.executor,
            actions: deps.actions,
            events: deps.events,
            runs: deps.runs,
            scheduler: deps.scheduler,
            schedules: deps.schedules,
            time_zone: deps.time_zone,
            chat: deps.chat,
            pusher: deps.pusher,
            notify_log: deps.notify_log,
            kv: deps.kv,
            connectors: deps.connectors,
            started_at: Utc::now(),
        })
    }

    /// Register every method this daemon answers on `server`.
    ///
    /// One place, so that "what can be asked of the daemon over its socket" is
    /// a list you can read in ten seconds rather than something scattered
    /// through `main`. Thirteen methods; see the module docs for how each one
    /// relates to the gate. The three added with the chat surface — `remember`,
    /// `facts`, `forget` — are reads and writes of the `facts` table, and none
    /// of them reaches a connector.
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
        method!("remember", remember);
        method!("facts", facts);
        method!("forget", forget);
    }

    // ----------------------------------------------------------------------
    // Reads of local state
    // ----------------------------------------------------------------------

    /// `status` — is the daemon up, is it paused, what is waiting, and what is
    /// broken.
    ///
    /// `paused` is the live flag; `paused_since`, `paused_for_days` and
    /// `pause_warning` come from the persisted record, so the owner can tell a
    /// paused daemon from a broken one, and can see a pause they set weeks ago
    /// and forgot.
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

        let pause: PauseState = self.kv.get_json(PAUSED_KEY).unwrap_or_default();
        let paused_for_days = pause.since.map(|since| (Utc::now() - since).num_days());

        Ok(json!({
            "status": "ok",
            "paused": self.scheduler.is_paused(),
            // From the persisted record rather than the in-memory flag, so
            // that a `paused: true` with a null `paused_since` is visible for
            // what it is: a pause that did not reach disk and will not
            // survive the next restart.
            "paused_since": pause.since.map(|t| t.to_rfc3339()),
            "paused_for_days": paused_for_days,
            "pause_warning": self.pause_warning(&pause),
            "pending_actions": pending.len(),
            // Events triage could not score and gave up on. Nonzero means the
            // daemon has decided not to look at something; that must be
            // visible here rather than only in the log, or it is exactly the
            // silent failure the rest of this endpoint exists to prevent.
            "unscorable_events": self.events.abandoned_count().unwrap_or_default(),
            // Scores held back by the threshold, by quiet hours or by the
            // hourly rate limit. The morning briefing now delivers them, so
            // `digest_pending` is normally the hours since the last briefing
            // rather than a permanent backlog — which is exactly why it is
            // still here: a number that keeps climbing past a day means the
            // briefing has stopped, and `digest_dropped` counts what the
            // 200-line cap has destroyed in the meantime.
            "digest_pending": self.notify_log.digest_len().unwrap_or_default(),
            "digest_dropped": self.notify_log.digest_dropped().unwrap_or_default(),
            "jobs": jobs,
            "schedules": self.schedule_status(),
            "connectors": self.connectors,
            "sessions_available": self.chat.sessions_available(),
            "notifier_configured": self.pusher.is_some(),
            // Spent against the ceiling, over the owner's day rather than
            // UTC's. Together with `digest_pending` above, these are the two
            // numbers that say at a glance whether the daemon is working or
            // quietly stuck: a budget at its ceiling means triage has stopped
            // scoring and the briefings have stopped writing, and a digest
            // that keeps climbing past a day means the briefing that drains
            // it is not running.
            "sessions_today": self.chat.budget().status_line(Utc::now()),
            "sessions_spent_today": self.chat.budget().spent_today(Utc::now()).unwrap_or_default(),
            "daily_session_budget": self.chat.budget().limit(),
            "chat_model": self.chat.model(),
            "facts": self.chat.facts().all().map(|f| f.len()).unwrap_or_default(),
            "started_at": self.started_at.to_rfc3339(),
        }))
    }

    /// The cron entries, with the next time each is due.
    ///
    /// `next_run_at` is the part worth having. A briefing that has silently
    /// stopped and one that is simply not due until tomorrow look identical
    /// from outside, and the difference between "07:00 tomorrow" and `null` is
    /// the difference between a healthy schedule and a row whose expression no
    /// longer parses.
    ///
    /// Read errors degrade to an empty list rather than failing `status`: this
    /// endpoint is what someone reaches for when things are already wrong, and
    /// it must answer.
    fn schedule_status(&self) -> Vec<Value> {
        let rows = match self.schedules.all() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "could not read the schedules table");
                return Vec::new();
            }
        };
        rows.into_iter()
            .map(|row| {
                let next = row.last_run_at.and_then(|last| {
                    Schedule::parse(&row.name, &row.cron, self.time_zone, Some(last))
                        .ok()?
                        .next_after(last)
                });
                json!({
                    "name": row.name,
                    "cron": row.cron,
                    "time_zone": self.time_zone.name(),
                    "enabled": row.enabled,
                    "last_run_at": row.last_run_at.map(|at| at.to_rfc3339()),
                    "next_run_at": next.map(|at| at.to_rfc3339()),
                })
            })
            .collect()
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

    /// What `status` says about a pause that has stood long enough to cost
    /// something.
    ///
    /// Making the pause durable removed the accident that used to rescue a
    /// forgotten one, and the concrete casualty is the Fortnox OAuth grant:
    /// its refresh token dies after 45 days unused, and re-authorising is a
    /// manual browser round trip. So say so, out loud, in the one place the
    /// owner looks — and only say it when there is a Fortnox connector to
    /// lose. Deliberately *only* a warning: nothing here un-pauses the daemon
    /// on its own, because a pause that expires by itself is the defect this
    /// whole change fixes.
    fn pause_warning(&self, pause: &PauseState) -> Option<String> {
        let since = pause.since?;
        if !pause.paused || !self.connectors.iter().any(|c| c == "fortnox") {
            return None;
        }
        let days = (Utc::now() - since).num_days();
        (days >= PAUSE_WARN_AFTER_DAYS).then(|| {
            format!(
                "paused for {days} days: the Fortnox refresh token lapses after \
                 {FORTNOX_REFRESH_LAPSE_DAYS} days unused, and re-authorising is manual. \
                 Run `ea resume` before then."
            )
        })
    }

    /// `pause` — stop the scheduler spawning anything new, durably.
    ///
    /// Does not touch work already in flight: a poll that is halfway through
    /// writing events finishes. Reaches no connector.
    ///
    /// The durable write belongs *here*, in the operation the owner invokes,
    /// and must never move down into [`Scheduler::pause`]: `main` calls that
    /// primitive as the first step of the shutdown drain, so persisting there
    /// would make every clean shutdown record `paused = true` and, under the
    /// plist's `KeepAlive`, bring the daemon back paused and never running
    /// again. See `a_clean_shutdown_does_not_persist_a_pause`.
    pub async fn pause(&self, _params: Value) -> anyhow::Result<Value> {
        // In memory first, and unconditionally: "stop spending" must take
        // effect even if the durable write then fails. The error is still
        // propagated rather than swallowed, because a pause the owner thinks
        // will outlive a reboot and does not is the defect this fixes.
        self.scheduler.pause();
        let since = Utc::now();
        self.kv
            .set_json(
                PAUSED_KEY,
                &PauseState {
                    paused: true,
                    since: Some(since),
                },
            )
            .context("recording the pause so it survives a restart")?;
        tracing::info!(%since, "scheduler paused over IPC, and persisted");
        Ok(json!({ "paused": true, "since": since.to_rfc3339() }))
    }

    /// `resume` — undo `pause`, including the persisted part of it.
    pub async fn resume(&self, _params: Value) -> anyhow::Result<Value> {
        self.scheduler.resume();
        self.kv
            .set_json(PAUSED_KEY, &PauseState::default())
            .context("clearing the persisted pause")?;
        tracing::info!("scheduler resumed over IPC, and the persisted pause cleared");
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
    /// A thin adapter over [`ChatService::say`], which both surfaces share: the
    /// Telegram update loop calls the same method on the same instance, so a
    /// thread started on the phone continues here mid-sentence and two
    /// messages can never run two sessions against one conversation.
    ///
    /// A chat session gets no connectors and two write tools — `propose_action`
    /// and `remember`, via `session::ToolScope::ProposeAndRemember` -- the only
    /// scope that includes the second, and chat is the only kind that uses it,
    /// because a chat prompt is the owner's own words. The first re-enters this
    /// daemon at `propose` and goes through the gate like anything else; the
    /// second writes a row to `facts`. Nothing a session can do reaches a
    /// connector directly.
    ///
    /// A session that *fails* is an error to the caller, not a note: a turn
    /// that produced no answer must not leave a phantom reply in the thread.
    pub async fn chat(&self, params: Value) -> anyhow::Result<Value> {
        let params: ChatParams = parse_params("chat", params)?;
        let turn = self.chat.say(&params.surface, &params.message).await?;
        serde_json::to_value(turn).context("chat: serialising the turn")
    }

    // ----------------------------------------------------------------------
    // Memory
    //
    // Three methods that read and write the `facts` table and nothing else.
    // No connector is reachable from any of them, which is the whole reason
    // `remember` is `auto` rather than a proposal: a fact is internal state,
    // reversible with `forget`, and gating it behind a human tap would mean
    // the assistant never learns anything.
    // ----------------------------------------------------------------------

    /// `remember` — store a fact under a topic, replacing what was there.
    ///
    /// Called by the `remember` tool on `ea-propose` and by nothing else in
    /// production. Same topic twice updates rather than duplicating; see
    /// [`ea_core::store::facts`].
    pub async fn remember(&self, params: Value) -> anyhow::Result<Value> {
        let params: RememberParams = parse_params("remember", params)?;
        let fact = self.chat.facts().remember(&params.topic, &params.body)?;
        serde_json::to_value(fact).context("remember: serialising the fact")
    }

    /// `facts` — everything the assistant has been told to remember.
    pub async fn facts(&self, _params: Value) -> anyhow::Result<Value> {
        let facts = self.chat.facts().all()?;
        serde_json::to_value(facts).context("facts: serialising the facts")
    }

    /// `forget` — delete one fact. Says plainly when there was nothing there,
    /// rather than reporting a deletion that did not happen.
    pub async fn forget(&self, params: Value) -> anyhow::Result<Value> {
        let params: IdParams = parse_params("forget", params)?;
        let existed = self.chat.facts().forget(params.id)?;
        Ok(json!({ "id": params.id, "forgotten": existed }))
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
    use ea_core::store::conversations::ConversationStore;
    use ea_core::store::facts::FactStore;
    use ea_core::store::kv::KvStore;
    use ea_core::store::runs::RunStore;
    use tempfile::TempDir;

    use super::*;
    use crate::budget::Budget;
    use crate::chat::ChatService;
    use crate::scheduler::Job;
    use crate::session::{SessionOutcome, SessionRequest};
    use crate::triage::{BoxedSession, SessionBoundary};

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
        /// Kept so a test can build a second `Daemon` over the same database.
        conn: Arc<Mutex<rusqlite::Connection>>,
        daemon: Arc<Daemon<SpyCaller>>,
        log: CallLog,
        scheduler: Arc<Scheduler>,
        pusher: Arc<FakePusher>,
        sessions: Option<Arc<FakeSessions>>,
        conversations: ConversationStore,
        facts: FactStore,
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
            let conversations = ConversationStore::new(Arc::clone(&conn));
            let facts = FactStore::new(Arc::clone(&conn));
            let chat = Arc::new(ChatService::new(
                conversations.clone(),
                facts.clone(),
                sessions.clone().map(|s| s as Arc<dyn SessionBoundary>),
                Budget::new(
                    RunStore::new(Arc::clone(&conn)),
                    budget,
                    crate::notify::policy::DEFAULT_TIME_ZONE,
                ),
                crate::config::DEFAULT_CHAT_MODEL,
                crate::notify::policy::DEFAULT_TIME_ZONE,
            ));

            let daemon = Daemon::build(Deps {
                executor,
                actions: ActionStore::new(Arc::clone(&conn)),
                events: EventStore::new(Arc::clone(&conn)),
                runs: RunStore::new(Arc::clone(&conn)),
                scheduler: Arc::clone(&scheduler),
                schedules: ScheduleStore::new(Arc::clone(&conn)),
                time_zone: crate::notify::policy::DEFAULT_TIME_ZONE,
                chat: Arc::clone(&chat),
                pusher: Some(Arc::clone(&pusher) as Arc<dyn Pusher>),
                notify_log: NotificationLog::new(KvStore::new(Arc::clone(&conn))),
                kv: KvStore::new(Arc::clone(&conn)),
                connectors: vec!["canvas".to_string()],
            });

            Self {
                _dir: dir,
                conn,
                daemon,
                log,
                scheduler,
                pusher,
                sessions,
                conversations,
                facts,
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
                "remember" => self.daemon.remember(params).await,
                "facts" => self.daemon.facts(params).await,
                "forget" => self.daemon.forget(params).await,
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

    // -- a pause that survives a restart ------------------------------------

    /// What `main` builds on the next start: a fresh `Scheduler` -- the old
    /// process's `AtomicBool` went with it -- over the same database, plus the
    /// startup restore `main` runs before `scheduler.start()`.
    fn restart(f: &Fixture, connectors: &[&str]) -> (Arc<Scheduler>, Arc<Daemon<SpyCaller>>) {
        let scheduler = Arc::new(Scheduler::new(3));
        let kv = KvStore::new(Arc::clone(&f.conn));
        let daemon = Daemon::build(Deps {
            executor: Arc::clone(&f.daemon.executor),
            actions: f.daemon.actions.clone(),
            events: f.daemon.events.clone(),
            runs: f.daemon.runs.clone(),
            scheduler: Arc::clone(&scheduler),
            schedules: ScheduleStore::new(Arc::clone(&f.conn)),
            time_zone: crate::notify::policy::DEFAULT_TIME_ZONE,
            chat: Arc::clone(&f.daemon.chat),
            pusher: None,
            notify_log: NotificationLog::new(kv.clone()),
            kv: kv.clone(),
            connectors: connectors.iter().map(|c| (*c).to_string()).collect(),
        });
        restore_pause(&kv, &scheduler);
        (scheduler, daemon)
    }

    /// The defect: `paused` was an in-memory flag and the plist sets
    /// `KeepAlive`, so the reboot the owner is away for un-paused the daemon
    /// and it resumed polling and spending on its own.
    #[tokio::test]
    async fn a_pause_survives_a_restart() {
        let f = Fixture::new();
        f.call("pause", Value::Null).await.unwrap();

        let (scheduler, daemon) = restart(&f, &["canvas"]);
        assert!(
            scheduler.is_paused(),
            "a restart while paused must come back paused"
        );
        let status = daemon.status(Value::Null).await.unwrap();
        assert_eq!(status["paused"], true);
        assert!(
            status["paused_since"].is_string(),
            "status must report the persisted truth, not just the flag: {status}"
        );
    }

    /// The other half, and the one that matters more: nothing may come back
    /// paused that was not paused on purpose.
    #[tokio::test]
    async fn a_restart_while_running_stays_running() {
        let f = Fixture::new();
        let (scheduler, daemon) = restart(&f, &["canvas"]);
        assert!(!scheduler.is_paused());
        let status = daemon.status(Value::Null).await.unwrap();
        assert_eq!(status["paused"], false);
        assert!(status["paused_since"].is_null());
    }

    #[tokio::test]
    async fn resume_clears_the_persisted_pause() {
        let f = Fixture::new();
        f.call("pause", Value::Null).await.unwrap();
        f.call("resume", Value::Null).await.unwrap();

        let (scheduler, daemon) = restart(&f, &["canvas"]);
        assert!(
            !scheduler.is_paused(),
            "resume must outlive the process too"
        );
        assert_eq!(daemon.status(Value::Null).await.unwrap()["paused"], false);
    }

    /// Constraint on the fix, as a test. `main` calls `Scheduler::pause` as
    /// the first step of the shutdown drain, so if persistence is ever moved
    /// down into that primitive, every clean shutdown records `paused = true`
    /// and, under the plist's `KeepAlive`, the daemon comes back paused and
    /// never runs again. That is a total, silent failure of the product; this
    /// test fails the moment someone "simplifies" the fix that way.
    #[tokio::test]
    async fn a_clean_shutdown_does_not_persist_a_pause() {
        let f = Fixture::new();

        // The shutdown sequence from `main`, in order.
        f.scheduler.pause();
        f.scheduler.drain(Duration::from_secs(1)).await;

        let kv = KvStore::new(Arc::clone(&f.conn));
        assert_eq!(
            kv.get(PAUSED_KEY).unwrap(),
            None,
            "the shutdown drain's pause must never be recorded as the owner's intent"
        );

        let (scheduler, daemon) = restart(&f, &["canvas"]);
        assert!(
            !scheduler.is_paused(),
            "a daemon restarted after a clean shutdown must run"
        );
        assert_eq!(daemon.status(Value::Null).await.unwrap()["paused"], false);
    }

    /// The cost of making the pause durable: a forgotten pause now stays. The
    /// mitigation is visibility, not an expiry -- `status` says how long, and
    /// says out loud that the Fortnox grant is on a clock.
    #[tokio::test]
    async fn status_says_how_long_a_pause_has_stood_and_warns_about_fortnox() {
        let f = Fixture::new();
        let kv = KvStore::new(Arc::clone(&f.conn));
        kv.set_json(
            PAUSED_KEY,
            &PauseState {
                paused: true,
                since: Some(Utc::now() - chrono::Duration::days(40)),
            },
        )
        .unwrap();

        let (_scheduler, daemon) = restart(&f, &["canvas", "fortnox"]);
        let status = daemon.status(Value::Null).await.unwrap();
        assert_eq!(status["paused"], true);
        assert_eq!(status["paused_for_days"], 40);
        let warning = status["pause_warning"].as_str().unwrap_or_default();
        assert!(
            warning.contains("45") && warning.to_lowercase().contains("fortnox"),
            "a month-old pause must name the grant it is about to kill: {status}"
        );

        // No Fortnox wired, nothing to lose to a lapse, no warning.
        let (_scheduler, daemon) = restart(&f, &["canvas"]);
        let status = daemon.status(Value::Null).await.unwrap();
        assert_eq!(status["paused_for_days"], 40);
        assert!(status["pause_warning"].is_null(), "{status}");
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
            events: f2.daemon.events.clone(),
            runs: f2.daemon.runs.clone(),
            scheduler: Arc::clone(&failing),
            schedules: ScheduleStore::new(Arc::clone(&f2.conn)),
            time_zone: crate::notify::policy::DEFAULT_TIME_ZONE,
            chat: Arc::clone(&f2.daemon.chat),
            pusher: None,
            notify_log: NotificationLog::new(KvStore::new(Arc::clone(&f2.conn))),
            kv: KvStore::new(Arc::clone(&f2.conn)),
            connectors: vec!["canvas".to_string()],
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

    /// The digest is written by triage and, until the morning briefing exists
    /// to send it, read by nobody. A backlog quietly growing — and then just
    /// as quietly falling off the 200-line cap — is data the owner cared about
    /// disappearing with no trace at all, so `status` carries both numbers.
    #[tokio::test]
    async fn status_surfaces_the_digest_backlog_and_what_the_cap_discarded() {
        let f = Fixture::new();
        let before = f.call("status", Value::Null).await.unwrap();
        assert_eq!(before["digest_pending"], 0);
        assert_eq!(before["digest_dropped"], 0);

        let log = NotificationLog::new(KvStore::new(Arc::clone(&f.conn)));
        log.push_digest("[canvas] a quiet-hours score").unwrap();
        let held = f.call("status", Value::Null).await.unwrap();
        assert_eq!(held["digest_pending"], 1);
        assert_eq!(held["digest_dropped"], 0);

        for i in 0..(crate::notify::log::MAX_DIGEST_LINES + 2) {
            log.push_digest(format!("[canvas] score {i}")).unwrap();
        }
        let overflowed = f.call("status", Value::Null).await.unwrap();
        assert_eq!(
            overflowed["digest_pending"],
            crate::notify::log::MAX_DIGEST_LINES
        );
        assert_eq!(
            overflowed["digest_dropped"], 3,
            "every discarded line must be counted where the owner can see it"
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
            events: f.daemon.events.clone(),
            runs: f.daemon.runs.clone(),
            scheduler: Arc::clone(&scheduler),
            schedules: ScheduleStore::new(Arc::clone(&f.conn)),
            time_zone: crate::notify::policy::DEFAULT_TIME_ZONE,
            chat: Arc::clone(&f.daemon.chat),
            pusher: None,
            notify_log: NotificationLog::new(KvStore::new(Arc::clone(&f.conn))),
            kv: KvStore::new(Arc::clone(&f.conn)),
            connectors: vec!["canvas".to_string()],
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
    /// `status` answers "when is the next briefing". A schedule that has
    /// silently stopped and one that is simply not due yet are otherwise
    /// indistinguishable from outside.
    #[tokio::test]
    async fn status_reports_every_schedule_and_when_it_next_runs() {
        let f = Fixture::new();
        let store = ScheduleStore::new(Arc::clone(&f.conn));
        crate::schedules::register_built_ins(&store, "2026-09-24T10:00:00Z".parse().unwrap())
            .unwrap();

        let status = f.call("status", Value::Null).await.unwrap();
        let schedules = status["schedules"].as_array().expect("an array").clone();
        assert_eq!(schedules.len(), 3);

        let morning = schedules
            .iter()
            .find(|s| s["name"] == crate::schedules::MORNING_BRIEFING)
            .expect("the morning briefing");
        assert_eq!(morning["cron"], crate::schedules::MORNING_BRIEFING_CRON);
        assert_eq!(morning["enabled"], true);
        assert_eq!(morning["time_zone"], "Europe/Stockholm");
        assert_eq!(morning["last_run_at"], "2026-09-24T10:00:00+00:00");
        // Installed at noon Stockholm on the 24th, so the next 07:00 local is
        // the 25th, which is 05:00 UTC.
        assert_eq!(morning["next_run_at"], "2026-09-25T05:00:00+00:00");
    }

    /// A row whose expression has been edited into nonsense reports a `null`
    /// next run rather than taking `status` down with it.
    #[tokio::test]
    async fn a_broken_schedule_row_does_not_break_status() {
        let f = Fixture::new();
        let store = ScheduleStore::new(Arc::clone(&f.conn));
        store
            .ensure(
                "wrong",
                "not a cron",
                "2026-09-24T10:00:00Z".parse().unwrap(),
            )
            .unwrap();

        let status = f.call("status", Value::Null).await.unwrap();
        assert_eq!(status["status"], "ok");
        assert_eq!(status["schedules"][0]["next_run_at"], Value::Null);
    }

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

        let fact = client
            .call(
                "remember",
                json!({ "topic": "tenta", "body": "on the 14th" }),
            )
            .await
            .unwrap();

        for (method, params) in [
            ("status", Value::Null),
            ("queue", Value::Null),
            ("log", json!({ "n": 5 })),
            ("pause", Value::Null),
            ("resume", Value::Null),
            ("resume_job", json!({ "job": "canvas" })),
            ("chat", json!({ "message": "hi" })),
            ("facts", Value::Null),
            ("forget", json!({ "id": fact["id"] })),
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

    /// The IPC surface, by name. Ten methods since Phase 1; the chat surface
    /// adds `remember`, `facts` and `forget`, which read and write the local
    /// `facts` table and reach no connector — the test below proves the last
    /// part by driving all three and asserting the spy caller saw nothing.
    ///
    /// A method appearing here that is not in the module docs' gating account
    /// is exactly what this is for. `connectors.call` was on this socket once
    /// and was removed; nothing of that shape may return.
    #[tokio::test]
    async fn the_ipc_surface_is_exactly_these_thirteen_methods() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let mut server = ipc::Server::new(&dir.path().join("d.sock"));
        f.daemon.register(&mut server);

        assert_eq!(
            server.methods(),
            vec![
                "approve",
                "chat",
                "facts",
                "forget",
                "log",
                "pause",
                "propose",
                "queue",
                "reject",
                "remember",
                "resume",
                "resume_job",
                "status",
            ]
        );
        assert_eq!(server.methods().len(), 13);
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

    /// **The owner's own message is never refused.**
    ///
    /// The budget stops the work the daemon decided to do on its own — triage
    /// degrades to tier 0, the briefings wait for tomorrow — and neither of
    /// those has a human sitting in front of it. A typed message does, it has
    /// no cheaper tier to fall back to, and the moment the owner reaches for
    /// chat is usually the moment they are asking why the daemon has gone
    /// quiet. Refusing then leaves them editing a config file to get a
    /// sentence out of it.
    ///
    /// What the budget does instead is say so on every turn past the ceiling,
    /// while still counting the spend, so `ea status` stays honest and the
    /// unattended work is what stops first.
    #[tokio::test]
    async fn chat_answers_the_owner_even_when_the_daily_budget_is_spent() {
        let f = Fixture::build(true, 2);

        let first = f.call("chat", json!({ "message": "hi" })).await.unwrap();
        assert_eq!(first["reply"], "here is your answer");
        assert!(first["note"].is_null(), "under the ceiling: {first}");

        // The turn that spends the last session says so on itself rather than
        // waiting for the next one to be surprised.
        let second = f.call("chat", json!({ "message": "hi" })).await.unwrap();
        assert_eq!(second["reply"], "here is your answer");
        assert!(
            second["note"].as_str().unwrap().contains("budget"),
            "{second}"
        );

        let third = f
            .call("chat", json!({ "message": "hi again" }))
            .await
            .unwrap();
        assert_eq!(
            third["reply"], "here is your answer",
            "the owner is answered: {third}"
        );
        let note = third["note"].as_str().unwrap();
        assert!(note.contains("budget"), "{note}");
        assert!(note.contains("Europe/Stockholm"), "{note}");
        assert_eq!(
            f.sessions.as_ref().unwrap().seen.lock().unwrap().len(),
            3,
            "the third session runs"
        );
        assert!(third["message_id"].as_i64().unwrap() > 0);

        // And the spend is visible rather than hidden: over the ceiling, and
        // saying so.
        let status = f.call("status", Value::Null).await.unwrap();
        assert_eq!(status["sessions_today"], "3/2");
        assert_eq!(status["sessions_spent_today"], 3);
    }

    /// The two numbers `ea status` exists to put next to each other.
    #[tokio::test]
    async fn status_reports_the_sessions_spent_today_against_the_ceiling() {
        let f = Fixture::build(true, 60);
        let before = f.call("status", Value::Null).await.unwrap();
        assert_eq!(before["sessions_today"], "0/60");
        assert_eq!(before["daily_session_budget"], 60);
        assert_eq!(before["digest_pending"], 0);

        f.call("chat", json!({ "message": "hi" })).await.unwrap();
        f.daemon.notify_log.push_digest("held back").unwrap();

        let after = f.call("status", Value::Null).await.unwrap();
        assert_eq!(after["sessions_today"], "1/60");
        assert_eq!(after["sessions_spent_today"], 1);
        assert_eq!(after["digest_pending"], 1);
    }

    /// A chat session must not be handed connector servers, and must reach the
    /// world only through propose_action. `remember` is the other tool on the
    /// list and writes a local row; neither names a connector.
    #[tokio::test]
    async fn a_chat_session_is_scoped_to_no_connectors() {
        let f = Fixture::new();
        f.call("chat", json!({ "message": "hi" })).await.unwrap();
        let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
        assert!(seen[0].connectors.is_empty());
        assert_eq!(
            seen[0].tools.allowed_tools(),
            "mcp__ea-propose__propose_action,mcp__ea-propose__remember"
        );
    }

    /// The finding: `ea chat` built its request with no `.with_model`, so it
    /// inherited the human's interactive model — `opus-5[1m]`, about 30x tier
    /// 1's rate — and charged it against the same daily session budget. The
    /// model must be explicit, and it must be the configured one.
    #[tokio::test]
    async fn a_chat_session_names_its_model_rather_than_inheriting_one() {
        let f = Fixture::new();
        f.call("chat", json!({ "message": "hi" })).await.unwrap();
        let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
        assert_eq!(
            seen[0].model.as_deref(),
            Some(crate::config::DEFAULT_CHAT_MODEL),
            "an unset model is inherited from the human's own settings"
        );
        // By literal too, so a rename of the constant cannot quietly change
        // what this daemon spends.
        assert_eq!(seen[0].model.as_deref(), Some("claude-sonnet-4-5"));
    }

    /// Triage's pin is a separate decision and must not follow chat's.
    #[test]
    fn triage_and_chat_are_pinned_to_different_models_on_purpose() {
        assert_eq!(crate::triage::TIER1_MODEL, "claude-haiku-4-5");
        assert_ne!(
            crate::triage::TIER1_MODEL,
            crate::config::DEFAULT_CHAT_MODEL
        );
    }

    /// A model the owner configured must actually be used, or the setting is
    /// decoration.
    #[tokio::test]
    async fn a_configured_chat_model_is_the_one_used() {
        let f = Fixture::new();
        let chat = Arc::new(ChatService::new(
            f.conversations.clone(),
            f.facts.clone(),
            f.sessions.clone().map(|s| s as Arc<dyn SessionBoundary>),
            Budget::new(
                f.daemon.runs.clone(),
                60,
                crate::notify::policy::DEFAULT_TIME_ZONE,
            ),
            "claude-opus-4-5",
            crate::notify::policy::DEFAULT_TIME_ZONE,
        ));
        let daemon = Daemon::build(Deps {
            executor: Arc::clone(&f.daemon.executor),
            actions: f.daemon.actions.clone(),
            events: f.daemon.events.clone(),
            runs: f.daemon.runs.clone(),
            scheduler: Arc::clone(&f.scheduler),
            schedules: ScheduleStore::new(Arc::clone(&f.conn)),
            time_zone: crate::notify::policy::DEFAULT_TIME_ZONE,
            chat,
            pusher: None,
            notify_log: NotificationLog::new(KvStore::new(Arc::clone(&f.conn))),
            kv: KvStore::new(Arc::clone(&f.conn)),
            connectors: Vec::new(),
        });
        daemon.chat(json!({ "message": "hi" })).await.unwrap();

        let model = {
            let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
            seen[0].model.clone()
        };
        assert_eq!(model.as_deref(), Some("claude-opus-4-5"));

        let status = daemon.status(Value::Null).await.unwrap();
        assert_eq!(status["chat_model"], "claude-opus-4-5");
    }

    // -- memory --------------------------------------------------------------

    /// The whole point of `remember`: a fact written by a session comes back
    /// on the next conversation that mentions it.
    #[tokio::test]
    async fn a_remembered_fact_round_trips_through_the_socket_methods() {
        let f = Fixture::new();
        let stored = f
            .call(
                "remember",
                json!({ "topic": "tenta", "body": "the databases tenta is on the 14th" }),
            )
            .await
            .unwrap();
        assert!(stored["id"].as_i64().unwrap() > 0);
        assert_eq!(stored["topic"], "tenta");

        let listed = f.call("facts", Value::Null).await.unwrap();
        let rows = listed.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["body"], "the databases tenta is on the 14th");

        let forgotten = f
            .call("forget", json!({ "id": stored["id"] }))
            .await
            .unwrap();
        assert_eq!(forgotten["forgotten"], true);
        assert!(f
            .call("facts", Value::Null)
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn remembering_the_same_topic_twice_updates_rather_than_duplicating() {
        let f = Fixture::new();
        let first = f
            .call(
                "remember",
                json!({ "topic": "tenta", "body": "on the 14th" }),
            )
            .await
            .unwrap();
        let second = f
            .call(
                "remember",
                json!({ "topic": "tenta", "body": "moved to the 21st" }),
            )
            .await
            .unwrap();

        assert_eq!(first["id"], second["id"]);
        let rows = f.call("facts", Value::Null).await.unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["body"], "moved to the 21st");
    }

    #[tokio::test]
    async fn remembering_an_empty_body_is_refused() {
        let f = Fixture::new();
        let err = f
            .call("remember", json!({ "topic": "tenta", "body": "  " }))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("body"), "{err:#}");
        assert!(f
            .call("facts", Value::Null)
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_fact_that_is_not_there_says_so() {
        let f = Fixture::new();
        let answer = f.call("forget", json!({ "id": 9999 })).await.unwrap();
        assert_eq!(answer["forgotten"], false);
    }

    /// The gate, restated for the three methods this task added: memory is
    /// local state, and none of it may reach a connector.
    #[tokio::test]
    async fn the_memory_methods_reach_no_connector() {
        let f = Fixture::new();
        let stored = f
            .call(
                "remember",
                json!({ "topic": "tenta", "body": "on the 14th" }),
            )
            .await
            .unwrap();
        f.call("facts", Value::Null).await.unwrap();
        f.call("forget", json!({ "id": stored["id"] }))
            .await
            .unwrap();
        f.call("chat", json!({ "message": "hello" })).await.unwrap();

        assert!(
            f.calls().is_empty(),
            "memory and chat must reach no connector: {:?}",
            f.calls()
        );
    }

    /// A message from the phone and one from the terminal are two turns in one
    /// thread, and the IPC surface is how the terminal joins it.
    #[tokio::test]
    async fn chat_records_the_surface_it_was_told() {
        let f = Fixture::new();
        f.call(
            "chat",
            json!({ "message": "from the phone", "surface": "telegram" }),
        )
        .await
        .unwrap();
        f.call("chat", json!({ "message": "from the terminal" }))
            .await
            .unwrap();

        let id = f.conversations.current().unwrap();
        let messages = f.conversations.recent(id, 10).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m.surface.as_str())
                .collect::<Vec<_>>(),
            ["telegram", "telegram", "cli", "cli"],
            "an absent surface defaults to the terminal"
        );
    }

    #[tokio::test]
    async fn chat_refuses_a_surface_nobody_has() {
        let f = Fixture::new();
        let err = f
            .call("chat", json!({ "message": "hi", "surface": "sms" }))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("sms"), "{err:#}");
    }

    /// The prompt a chat session is given must carry the facts that match, and
    /// only those.
    #[tokio::test]
    async fn a_chat_prompt_carries_the_matching_facts() {
        let f = Fixture::new();
        f.facts
            .remember("tenta", "the databases tenta is on the 14th")
            .unwrap();
        f.facts
            .remember("invoicing", "invoices go to Ekonomi AB")
            .unwrap();

        f.call("chat", json!({ "message": "when is the tenta?" }))
            .await
            .unwrap();

        let seen = f.sessions.as_ref().unwrap().seen.lock().unwrap();
        assert!(
            seen[0].system_prompt.contains("databases tenta"),
            "{}",
            seen[0].system_prompt
        );
        assert!(
            !seen[0].system_prompt.contains("Ekonomi AB"),
            "{}",
            seen[0].system_prompt
        );
    }
}
