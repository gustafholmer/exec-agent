//! The two kinds of periodic work the daemon actually does: polling each
//! connector for changes, and triaging what those polls recorded.
//!
//! Both are written as plain `async fn`s taking their dependencies explicitly,
//! with thin [`Job`] wrappers at the bottom. That is what makes them testable:
//! a scheduler job is a closure returning a future, which is an awkward thing
//! to assert against, while `run_triage(&deps, now)` returns a summary.
//!
//! # What these may and may not do
//!
//! `watch_poll` is the one place in the daemon that calls a connector without
//! an `actions` row behind it, and it is worth being explicit about why that
//! is not a hole in the gate:
//!
//! * it calls exactly one tool, named by the constant [`WATCH_TOOL`], never a
//!   caller-supplied name;
//! * it refuses unless the merged policy rates that tool `auto` for that
//!   connector — so a connector whose `watch_poll` is `approve` or `deny` is
//!   simply not polled, and the refusal says so;
//! * it is not reachable from the IPC socket. Nothing a client can send
//!   chooses the connector, the tool, or the arguments.
//!
//! Triage never calls a connector at all. It reads events, spends at most one
//! model session, writes salience back, and sends messages.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use ea_core::policy::{Mode, Policy};
use ea_core::store::actions::ActionStore;
use ea_core::store::events::{Event, EventStore, RecordInput};
use ea_core::store::runs::RunStore;
use serde::Deserialize;

use crate::executor::ToolCaller;
use crate::notify::log::NotificationLog;
use crate::notify::policy::NotificationPolicy;
use crate::notify::telegram::{Notifier, Transport};
use crate::scheduler::Job;
use crate::triage::{tier0, tier1, tier1_batch, SessionBoundary, Tier0Rules};

/// The tool every connector must expose, and the only one the scheduler calls.
pub const WATCH_TOOL: &str = "watch_poll";

/// How long one `watch_poll` may take.
///
/// Longer than the executor's 30 seconds: a poll walks every course, every
/// mailbox folder, whatever the connector's world is, and it is running on its
/// own clock with nobody waiting. Still bounded, because the scheduler's
/// overlap guard means a wedged poll would stop that connector entirely.
///
/// **Ninety seconds, and strictly less than any connector's poll interval.**
/// It was 120, which is exactly the Google connector's `watch_interval_secs`:
/// a poll that used its whole budget would finish precisely as the next tick
/// was due, so a connector that is merely slow would run polls back to back
/// forever with no idle gap — and, because the overlap guard skips a tick
/// whose predecessor is still running, the effective interval would silently
/// double rather than the slowness being reported. A deadline below the
/// interval means a slow poll is *cut off and recorded as a failure*, which is
/// the outcome that produces a breaker trip and an entry in `ea status`
/// instead of a connector that quietly falls behind. The gap is checked
/// against every connector's manifest by
/// `no_connector_polls_faster_than_a_poll_is_allowed_to_take`.
pub const WATCH_TIMEOUT: Duration = Duration::from_secs(90);

/// How many untriaged events one triage pass considers. The brief's number.
pub const TRIAGE_SCAN_LIMIT: i64 = 200;

/// Salience written to an event tier 0 dropped.
///
/// Zero, and `set_salience` stamps `triaged_at` along with it — which is the
/// point. Without the stamp a muted event stays untriaged forever and keeps
/// filling the 200-row scan window, and after a few weeks of newsletters a
/// real deadline never gets looked at.
pub const DROPPED_SALIENCE: i64 = 0;

/// How many tier-1 batches an event may come back from unscored before triage
/// gives up on it.
///
/// Three. A model that omitted an event once may well score it in a different
/// batch — the batch composition changes, and one bad completion is not a
/// verdict — so a single miss must not be fatal. But an event the model
/// *cannot* score (a payload that trips a content filter, a row so malformed
/// the prompt renders it unusably) will never be scored on the fourth try
/// either, and every retry costs a slot in a 40-event batch. Three retries
/// spread over three passes is fifteen minutes of grace at the default triage
/// interval, and the give-up is recorded rather than silent.
pub const TRIAGE_MAX_ATTEMPTS: i64 = 3;

/// Recorded in `events.triage_error` when triage gives up.
pub const UNSCORABLE_REASON: &str =
    "tier 1 returned no score for this event in 3 consecutive batches";

// --------------------------------------------------------------------------
// watch_poll
// --------------------------------------------------------------------------

/// One entry of a connector's `watch_poll` array.
///
/// `source` is deliberately absent: it is the connector's own name, which the
/// daemon already knows and a connector must not be able to claim otherwise.
/// A connector that could set `source` could write events attributed to
/// another connector.
#[derive(Debug, Clone, Deserialize)]
struct WatchItem {
    external_id: String,
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// What one poll recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PollOutcome {
    pub seen: usize,
    pub new: usize,
}

/// Poll one connector and record what it reports.
///
/// Errors — a connector that is down, a lapsed token, a reply that is not the
/// documented array — propagate, which is how the scheduler's breaker learns
/// anything. Swallowing them and returning an empty poll would leave a
/// connector with an expired token looking healthy forever.
pub async fn run_watch_poll<C: ToolCaller>(
    connector: &str,
    caller: &C,
    events: &EventStore,
    policy: &Policy,
) -> anyhow::Result<PollOutcome> {
    let decision = policy.decide(connector, WATCH_TOOL);
    if decision.mode != Mode::Auto {
        bail!(
            "refusing to poll {connector}: policy rates {WATCH_TOOL} as {:?}, not auto ({})",
            decision.mode,
            decision.reason
        );
    }

    // [`WATCH_TIMEOUT`], not the executor's generic 30 seconds. A poll is a
    // whole connector's world — Google's is up to 54 HTTP round trips across
    // two accounts — and 30 seconds is a budget it cannot meet on a slow link,
    // which would time out every tick and trip the breaker on a connector
    // that is working.
    let raw = caller
        .call_with_timeout(connector, WATCH_TOOL, serde_json::json!({}), WATCH_TIMEOUT)
        .await
        .with_context(|| format!("polling connector {connector}"))?;

    let items: Vec<WatchItem> = serde_json::from_str(&raw).with_context(|| {
        format!("connector {connector}: {WATCH_TOOL} must return a JSON array of changes")
    })?;

    let mut outcome = PollOutcome {
        seen: items.len(),
        new: 0,
    };
    for item in items {
        let (_event, is_new) = events
            .record(RecordInput {
                source: connector.to_string(),
                external_id: item.external_id,
                kind: item.kind,
                payload: item.payload,
            })
            .with_context(|| format!("recording an event from {connector}"))?;
        if is_new {
            outcome.new += 1;
        }
    }

    if outcome.new > 0 {
        tracing::info!(connector, new = outcome.new, seen = outcome.seen, "polled");
    } else {
        tracing::debug!(connector, seen = outcome.seen, "polled; nothing new");
    }
    Ok(outcome)
}

// --------------------------------------------------------------------------
// triage
// --------------------------------------------------------------------------

/// Somewhere to push a notification. Object-safe, unlike
/// [`Transport`](crate::notify::telegram::Transport), because the triage job
/// holds one behind an `Arc<dyn _>` and must not be generic over the whole
/// Telegram stack to do it.
pub trait Pusher: Send + Sync {
    fn notify<'a>(
        &'a self,
        text: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn push_action(
        &self,
        id: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>;
}

impl<T: Transport + Send + Sync, C: ToolCaller + Send + Sync> Pusher for Notifier<T, C> {
    fn notify<'a>(
        &'a self,
        text: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(Notifier::notify(self, text))
    }

    fn push_action(
        &self,
        id: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(Notifier::push_action(self, id))
    }
}

/// Everything one triage pass needs.
pub struct TriageDeps {
    pub events: EventStore,
    pub actions: ActionStore,
    pub runs: RunStore,
    /// Absent when `claude` could not be resolved at startup: triage then does
    /// tier 0 and stops, rather than the daemon refusing to run at all.
    pub sessions: Option<Arc<dyn SessionBoundary>>,
    /// Absent when Telegram is not configured. Everything that would have been
    /// sent is held for the digest instead.
    pub pusher: Option<Arc<dyn Pusher>>,
    pub log: NotificationLog,
    pub notify: NotificationPolicy,
    pub rules: Tier0Rules,
    pub daily_session_budget: u32,
}

/// What one triage pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TriageSummary {
    /// Proposals that ran out of time and were expired.
    pub expired: usize,
    /// Untriaged events the pass looked at.
    pub scanned: usize,
    /// Events tier 0 dropped.
    pub dropped: usize,
    /// Events tier 1 scored.
    pub scored: usize,
    /// Events tier 1 was asked about and said nothing about. They are retried;
    /// see `abandoned`.
    pub unscored: usize,
    /// Events given up on this pass, having gone unscored
    /// [`TRIAGE_MAX_ATTEMPTS`] times.
    pub abandoned: usize,
    /// Notifications actually sent.
    pub sent: usize,
    /// Scores held for the digest.
    pub digested: usize,
    /// True when the tier-1 session was skipped because the day's budget is
    /// spent. Visible rather than silent: "triage stopped scoring" with no
    /// reason is indistinguishable from a bug.
    pub budget_exhausted: bool,
}

/// One triage pass, in the order the brief specifies: expire, tier 0, **one**
/// tier-1 session, then a salience write and a notification verdict per score.
pub async fn run_triage(deps: &TriageDeps, now: DateTime<Utc>) -> anyhow::Result<TriageSummary> {
    let mut summary = TriageSummary {
        expired: deps
            .actions
            .expire_stale(now)
            .context("expiring stale proposals")?,
        ..TriageSummary::default()
    };

    let events = deps
        .events
        .untriaged(TRIAGE_SCAN_LIMIT)
        .context("reading untriaged events")?;
    summary.scanned = events.len();

    let (kept, dropped) = tier0(events, &deps.rules);
    summary.dropped = dropped.len();
    for (event, reason) in dropped {
        tracing::debug!(event = event.id, reason, "tier 0 dropped an event");
        // Stamped as triaged so it leaves the scan window. See DROPPED_SALIENCE.
        deps.events.set_salience(event.id, DROPPED_SALIENCE)?;
    }

    if kept.is_empty() {
        return Ok(summary);
    }

    let Some(sessions) = deps.sessions.as_ref() else {
        tracing::warn!(
            kept = kept.len(),
            "no session runner: tier 1 skipped and these events stay untriaged"
        );
        return Ok(summary);
    };

    if self::session_budget_spent(&deps.runs, deps.daily_session_budget, now)? {
        tracing::warn!(
            budget = deps.daily_session_budget,
            "daily session budget spent; tier 1 skipped this pass"
        );
        summary.budget_exhausted = true;
        return Ok(summary);
    }

    // Exactly one session per pass, whatever the batch size: `tier1` takes the
    // first TIER1_BATCH and leaves the rest for the next cycle. It also pins
    // the model to claude-haiku-4-5 rather than inheriting whatever the human
    // last used interactively.
    // `submitted` is what the model was actually shown: `tier1` takes only the
    // first TIER1_BATCH and leaves the rest for the next pass, and an event
    // that was never in the prompt must not be charged an attempt for not
    // coming back in the answer.
    let submitted: Vec<i64> = tier1_batch(&kept).iter().map(|event| event.id).collect();
    let scores = tier1(&kept, sessions.as_ref()).await?;
    summary.scored = scores.len();

    // Anything submitted that came back without a score. Before this, such an
    // event was never stamped `triaged_at` at all: it sat at the head of
    // `untriaged` forever, and two hundred of them wedged triage permanently
    // while still spending one session every five minutes. Now each miss is
    // counted, and after TRIAGE_MAX_ATTEMPTS the event is given up on with a
    // recorded reason.
    let scored_ids: std::collections::BTreeSet<i64> =
        scores.iter().map(|score| score.event_id).collect();
    for id in submitted
        .iter()
        .copied()
        .filter(|id| !scored_ids.contains(id))
    {
        summary.unscored += 1;
        let attempts = deps
            .events
            .record_triage_attempt(id)
            .with_context(|| format!("recording a failed triage attempt for event {id}"))?;
        if attempts >= TRIAGE_MAX_ATTEMPTS {
            deps.events
                .abandon(id, UNSCORABLE_REASON)
                .with_context(|| format!("giving up on event {id}"))?;
            summary.abandoned += 1;
            // Warned, not debugged: this is the daemon deciding not to look at
            // something the owner might have cared about, and `ea status`
            // reports the running total for the same reason.
            tracing::warn!(
                event = id,
                attempts,
                "triage gave up on this event; it will not be scored or notified about"
            );
            deps.log.push_digest(format!(
                "[triage] gave up on event {id}: {UNSCORABLE_REASON}"
            ))?;
        } else {
            tracing::debug!(
                event = id,
                attempts,
                "tier 1 returned no score for this event"
            );
        }
    }

    // Read once, then extended locally as sends happen: the rate limit has to
    // apply *within* one batch as well as across restarts, and re-reading the
    // store per score would be both slower and no more correct.
    let mut recent = deps.log.recent(now)?;

    for score in scores {
        deps.events
            .set_salience(score.event_id, i64::from(score.salience))?;

        let verdict = deps.notify.evaluate(score.salience, now, &recent);
        let line = describe(&kept, &score);

        if verdict.send {
            match deps.pusher.as_ref() {
                Some(pusher) => match pusher.notify(&line).await {
                    Ok(()) => {
                        deps.log.record(now)?;
                        recent.push(now);
                        summary.sent += 1;
                        tracing::info!(event = score.event_id, reason = verdict.reason, "notified");
                        continue;
                    }
                    Err(err) => {
                        // A failed send is not a spent interruption: it is not
                        // recorded against the rate limit, and the line still
                        // reaches the digest so it is not simply lost.
                        tracing::warn!(
                            event = score.event_id,
                            error = %format!("{err:#}"),
                            "notification failed; holding it for the digest"
                        );
                    }
                },
                None => tracing::debug!(
                    event = score.event_id,
                    "no notifier configured; holding for the digest"
                ),
            }
        } else {
            tracing::debug!(event = score.event_id, reason = verdict.reason, "held back");
        }

        deps.log.push_digest(line)?;
        summary.digested += 1;
    }

    Ok(summary)
}

/// Has the day's session budget been spent?
///
/// Counted from midnight UTC over every `runs` row that is not the executor's
/// own — see [`RunStore::count_since`](ea_core::store::runs::RunStore::count_since).
pub fn session_budget_spent(
    runs: &RunStore,
    budget: u32,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    if budget == 0 {
        return Ok(true);
    }
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a time")
        .and_utc();
    let spent = runs.count_since(midnight, Some(crate::executor::RUN_KIND))?;
    Ok(spent >= i64::from(budget))
}

/// The one-line summary a human reads, on the phone or in the digest.
fn describe(events: &[Event], score: &crate::triage::Salience) -> String {
    let event = events.iter().find(|event| event.id == score.event_id);
    let (source, kind) = match event {
        Some(event) => (event.source.as_str(), event.kind.as_str()),
        None => ("unknown", "event"),
    };
    let title = event
        .and_then(|event| {
            ["title", "subject", "name", "summary"]
                .iter()
                .find_map(|key| event.payload.get(*key).and_then(|v| v.as_str()))
        })
        .unwrap_or(kind);

    let mut line = format!("[{}] {title} ({})", source, score.salience);
    if !score.why.is_empty() {
        line.push_str(&format!("\n{}", score.why));
    }
    if !score.suggested_next_step.is_empty() {
        line.push_str(&format!("\nNext: {}", score.suggested_next_step));
    }
    line
}

// --------------------------------------------------------------------------
// Job wrappers
// --------------------------------------------------------------------------

/// A `watch_poll` job for one connector, named after it — which is the name
/// `ea status` shows and the breaker is keyed on.
pub fn watch_job<C>(
    connector: String,
    interval: Duration,
    caller: Arc<C>,
    events: EventStore,
    policy: Policy,
) -> Job
where
    C: ToolCaller + Send + Sync + 'static,
{
    let name = connector.clone();
    Job::new(name, interval, move || {
        let connector = connector.clone();
        let caller = Arc::clone(&caller);
        let events = events.clone();
        let policy = policy.clone();
        async move {
            run_watch_poll(&connector, caller.as_ref(), &events, &policy).await?;
            Ok(())
        }
    })
}

/// The name of the triage job, as `ea status` shows it and `ea pause` affects.
pub const TRIAGE_JOB: &str = "triage";

pub fn triage_job(interval: Duration, deps: Arc<TriageDeps>) -> Job {
    Job::new(TRIAGE_JOB, interval, move || {
        let deps = Arc::clone(&deps);
        async move {
            let summary = run_triage(&deps, Utc::now()).await?;
            tracing::debug!(?summary, "triage pass");
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Duration as ChronoDuration;
    use ea_core::store::actions::ProposeInput;
    use ea_core::store::kv::KvStore;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::notify::policy::NotifyConfig;
    use crate::session::{SessionOutcome, SessionRequest};
    use crate::triage::{BoxedSession, Salience, TIER1_MODEL};

    // -- doubles ------------------------------------------------------------

    /// Answers `watch_poll` with a scripted reply and records every call, so a
    /// test can assert that a refused poll reached no connector at all.
    #[derive(Default)]
    struct FakeConnector {
        replies: Mutex<Vec<anyhow::Result<String>>>,
        calls: Mutex<Vec<(String, String)>>,
        /// The deadline each call arrived with — `None` for a call that came
        /// through `ToolCaller::call`, which carries none.
        deadlines: Mutex<Vec<Option<Duration>>>,
    }

    impl FakeConnector {
        fn with(replies: Vec<anyhow::Result<String>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into_iter().rev().collect()),
                calls: Mutex::new(Vec::new()),
                deadlines: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn deadlines(&self) -> Vec<Option<Duration>> {
            self.deadlines.lock().unwrap().clone()
        }

        fn answer(
            &self,
            connector: &str,
            tool: &str,
            deadline: Option<Duration>,
        ) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string()));
            self.deadlines.lock().unwrap().push(deadline);
            self.replies
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok("[]".to_string()))
        }
    }

    impl ToolCaller for Arc<FakeConnector> {
        async fn call(
            &self,
            connector: &str,
            tool: &str,
            _args: serde_json::Value,
        ) -> anyhow::Result<String> {
            self.answer(connector, tool, None)
        }

        async fn call_with_timeout(
            &self,
            connector: &str,
            tool: &str,
            _args: serde_json::Value,
            timeout: Duration,
        ) -> anyhow::Result<String> {
            self.answer(connector, tool, Some(timeout))
        }
    }

    /// Scores every event it is given, without spawning anything.
    struct FakeTier1 {
        scores: Vec<Salience>,
        seen: Arc<Mutex<Vec<SessionRequest>>>,
        runs: RunStore,
    }

    impl FakeTier1 {
        fn new(scores: Vec<Salience>, runs: RunStore) -> Arc<Self> {
            Arc::new(Self {
                scores,
                seen: Arc::new(Mutex::new(Vec::new())),
                runs,
            })
        }

        fn sessions(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl SessionBoundary for FakeTier1 {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            let run_id = self.runs.start(&req.kind, &req.prompt, &[]).unwrap();
            self.seen.lock().unwrap().push(req);
            let structured = serde_json::json!({ "scores": self.scores });
            let runs = self.runs.clone();
            Box::pin(async move {
                runs.finish(run_id, "ok", None, &[], Some(0.002))?;
                Ok(SessionOutcome {
                    text: String::new(),
                    structured: Some(structured),
                    session_id: None,
                    cost_usd: Some(0.002),
                })
            })
        }
    }

    #[derive(Default)]
    struct SpyPusher {
        sent: Mutex<Vec<String>>,
        fail: bool,
    }

    impl SpyPusher {
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Pusher for SpyPusher {
        fn notify<'a>(
            &'a self,
            text: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            if self.fail {
                return Box::pin(async { Err(anyhow::anyhow!("telegram is down")) });
            }
            self.sent.lock().unwrap().push(text.to_string());
            Box::pin(async { Ok(()) })
        }

        fn push_action(
            &self,
            _id: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    // -- fixtures -----------------------------------------------------------

    fn db(dir: &TempDir) -> Arc<Mutex<Connection>> {
        Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ))
    }

    fn canvas_policy() -> Policy {
        Policy::parse("[canvas]\nwatch_poll = \"auto\"\n").unwrap()
    }

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    /// Stockholm afternoon: outside quiet hours, so the notification policy's
    /// verdict turns on salience and the rate limit rather than the clock.
    fn daytime() -> DateTime<Utc> {
        utc("2026-09-24T12:30:00Z")
    }

    // -- watch_poll ---------------------------------------------------------

    #[tokio::test]
    async fn a_poll_records_what_the_connector_reports() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(Arc::clone(&conn));
        let caller = FakeConnector::with(vec![Ok(r#"[
            {"external_id":"a1","kind":"assignment","payload":{"title":"Essay"}},
            {"external_id":"a2","kind":"assignment","payload":{"title":"Lab"}}
        ]"#
        .to_string())]);

        let outcome = run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap();
        assert_eq!(outcome, PollOutcome { seen: 2, new: 2 });
        assert_eq!(caller.calls(), vec![("canvas".into(), WATCH_TOOL.into())]);

        let stored = events.untriaged(10).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored[0].source, "canvas",
            "the source is the daemon's, not the connector's"
        );
        assert_eq!(stored[0].payload["title"], "Essay");
    }

    /// The constant was dead. `WATCH_TIMEOUT` existed from Phase 1 and
    /// nothing referenced it: every poll got the executor's generic 30
    /// seconds instead, which is not a budget a connector that walks two
    /// Google accounts can meet on a slow link. Thirty seconds of timeouts,
    /// five ticks apart, is a tripped breaker on a connector that works.
    #[tokio::test]
    async fn a_poll_gets_the_watch_deadline_and_not_the_executors_default() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(conn);
        let caller = FakeConnector::with(vec![Ok("[]".to_string())]);

        run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap();

        assert_eq!(
            caller.deadlines(),
            vec![Some(WATCH_TIMEOUT)],
            "the poll must name its own deadline; `None` means it went through the \
             deadline-less `call` and took the executor's 30 seconds"
        );
    }

    /// Polling is idempotent: the same assignment reported twice is one event.
    #[tokio::test]
    async fn a_repeated_poll_records_nothing_new() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(conn);
        let body = r#"[{"external_id":"a1","kind":"assignment","payload":{"title":"Essay"}}]"#;
        let caller = FakeConnector::with(vec![Ok(body.to_string()), Ok(body.to_string())]);

        let first = run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap();
        let second = run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap();
        assert_eq!(first.new, 1);
        assert_eq!(second, PollOutcome { seen: 1, new: 0 });
    }

    /// The gate, restated for the one connector call that has no `actions`
    /// row: a `watch_poll` the policy does not rate `auto` is not made.
    #[tokio::test]
    async fn a_poll_the_policy_does_not_allow_is_refused_without_calling_anything() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(conn);
        let caller = FakeConnector::with(vec![Ok("[]".to_string())]);

        for policy in [
            Policy::parse("[canvas]\nwatch_poll = \"approve\"\n").unwrap(),
            Policy::parse("[canvas]\nwatch_poll = \"deny\"\n").unwrap(),
            // No rule at all: the gate's default is `approve`, not `auto`.
            Policy::parse("[canvas]\nlist_courses = \"auto\"\n").unwrap(),
            Policy::default(),
        ] {
            let err = run_watch_poll("canvas", &caller, &events, &policy)
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("not auto"), "{err:#}");
        }
        assert!(
            caller.calls().is_empty(),
            "a refused poll must not reach the connector"
        );
    }

    /// A connector that is down must look down. Returning an empty poll would
    /// leave a lapsed token green for the rest of term and the breaker would
    /// never trip.
    #[tokio::test]
    async fn a_connector_error_propagates_so_the_breaker_can_see_it() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(conn);
        let caller = FakeConnector::with(vec![Err(anyhow::anyhow!("canvas: 401 Unauthorized"))]);

        let err = run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("401"), "{text}");
        assert!(text.contains("canvas"), "{text}");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_the_documented_array_is_an_error() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(conn);
        let caller = FakeConnector::with(vec![Ok("sorry, no".to_string())]);
        let err = run_watch_poll("canvas", &caller, &events, &canvas_policy())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("JSON array"), "{err:#}");
    }

    // -- triage -------------------------------------------------------------

    struct TriageFixture {
        _dir: TempDir,
        conn: Arc<Mutex<Connection>>,
        deps: TriageDeps,
        events: EventStore,
        actions: ActionStore,
        log: NotificationLog,
        pusher: Arc<SpyPusher>,
        tier1: Option<Arc<FakeTier1>>,
    }

    fn build_triage(
        scores: Vec<Salience>,
        rules: Tier0Rules,
        budget: u32,
        with_sessions: bool,
        failing_pusher: bool,
    ) -> TriageFixture {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let events = EventStore::new(Arc::clone(&conn));
        let actions = ActionStore::new(Arc::clone(&conn));
        let runs = RunStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        let pusher = Arc::new(SpyPusher {
            sent: Mutex::new(Vec::new()),
            fail: failing_pusher,
        });
        let tier1 = with_sessions.then(|| FakeTier1::new(scores, runs.clone()));

        let deps = TriageDeps {
            events: events.clone(),
            actions: actions.clone(),
            runs: runs.clone(),
            sessions: tier1.clone().map(|t| t as Arc<dyn SessionBoundary>),
            pusher: Some(Arc::clone(&pusher) as Arc<dyn Pusher>),
            log: NotificationLog::new(KvStore::new(Arc::clone(&conn))),
            notify: NotificationPolicy::new(NotifyConfig::default()),
            rules,
            daily_session_budget: budget,
        };

        TriageFixture {
            _dir: dir,
            conn,
            deps,
            events,
            actions,
            log,
            pusher,
            tier1,
        }
    }

    fn record(
        events: &EventStore,
        external_id: &str,
        source: &str,
        kind: &str,
        title: &str,
    ) -> i64 {
        events
            .record(RecordInput {
                source: source.to_string(),
                external_id: external_id.to_string(),
                kind: kind.to_string(),
                payload: serde_json::json!({ "title": title }),
            })
            .unwrap()
            .0
            .id
    }

    fn score(event_id: i64, salience: u8) -> Salience {
        Salience {
            event_id,
            salience,
            why: "the deadline is tomorrow".to_string(),
            suggested_next_step: "start the essay".to_string(),
        }
    }

    #[tokio::test]
    async fn triage_expires_stale_proposals_first() {
        let f = build_triage(Vec::new(), Tier0Rules::default(), 60, true, false);
        let action = f
            .actions
            .propose(ProposeInput {
                connector: "fortnox".into(),
                tool: "record_voucher".into(),
                args: serde_json::json!({}),
                preview: "x".into(),
                rationale: "y".into(),
                ttl: ChronoDuration::seconds(1),
            })
            .unwrap();

        let summary = run_triage(&f.deps, Utc::now() + ChronoDuration::hours(2))
            .await
            .unwrap();
        assert_eq!(summary.expired, 1);
        assert_eq!(
            f.actions.get(action.id).unwrap().unwrap().status.as_str(),
            "expired"
        );
    }

    /// A muted event must leave the scan window, or after a few weeks of
    /// newsletters the 200-row window holds nothing else and a real deadline
    /// is never looked at.
    #[tokio::test]
    async fn tier0_drops_muted_events_and_stamps_them_so_they_stop_coming_back() {
        let rules = Tier0Rules {
            muted_sources: vec!["newsletter".to_string()],
            ..Tier0Rules::default()
        };
        let f = build_triage(Vec::new(), rules, 60, true, false);
        record(&f.events, "n1", "newsletter", "email", "50% off");
        let kept = record(&f.events, "a1", "canvas", "assignment", "Essay");

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.dropped, 1);

        let still_untriaged = f.events.untriaged(10).unwrap();
        assert_eq!(
            still_untriaged.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![kept],
            "the dropped event must be stamped as triaged"
        );
    }

    /// The brief: **one** tier-1 session per pass, on the cheap model, over
    /// the tier-0 survivors.
    #[tokio::test]
    async fn triage_runs_exactly_one_tier1_session_on_the_cheap_model() {
        let f = build_triage(Vec::new(), Tier0Rules::default(), 60, true, false);
        for i in 0..5 {
            record(&f.events, &format!("a{i}"), "canvas", "assignment", "Essay");
        }

        run_triage(&f.deps, daytime()).await.unwrap();

        let tier1 = f.tier1.as_ref().unwrap();
        assert_eq!(tier1.sessions(), 1, "exactly one session per pass");
        let seen = tier1.seen.lock().unwrap();
        assert_eq!(
            seen[0].model.as_deref(),
            Some(TIER1_MODEL),
            "tier 1 must not inherit the human's interactive model"
        );
        assert_eq!(seen[0].model.as_deref(), Some("claude-haiku-4-5"));
        assert!(seen[0].connectors.is_empty(), "tier 1 does not fetch");
    }

    #[tokio::test]
    async fn a_high_score_is_sent_and_a_low_one_is_held_for_the_digest() {
        let dir_scores = |a: i64, b: i64| vec![score(a, 90), score(b, 10)];
        let f = build_triage(dir_scores(1, 2), Tier0Rules::default(), 60, true, false);
        let high = record(&f.events, "a1", "canvas", "assignment", "Essay due");
        let low = record(&f.events, "a2", "canvas", "assignment", "Reading");
        assert_eq!((high, low), (1, 2), "ids the fake scores were built with");

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.scored, 2);
        assert_eq!(summary.sent, 1);
        assert_eq!(summary.digested, 1);

        assert_eq!(f.pusher.sent().len(), 1);
        assert!(
            f.pusher.sent()[0].contains("Essay due"),
            "{:?}",
            f.pusher.sent()
        );
        assert!(f.log.digest().unwrap()[0].contains("Reading"));

        // And the scores were written back.
        assert_eq!(f.events.get(high).unwrap().unwrap().salience, Some(90));
        assert_eq!(f.events.get(low).unwrap().unwrap().salience, Some(10));
        assert!(f.events.untriaged(10).unwrap().is_empty());
    }

    /// Addition 3, in the loop rather than in the store: the rate limit has to
    /// bite *within* one batch too, not only across passes.
    #[tokio::test]
    async fn the_rate_limit_bites_inside_one_batch_and_the_rest_go_to_the_digest() {
        let scores = (1..=5).map(|id| score(id, 95)).collect();
        let f = build_triage(scores, Tier0Rules::default(), 60, true, false);
        for i in 1..=5 {
            record(&f.events, &format!("a{i}"), "canvas", "assignment", "Essay");
        }

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(
            summary.sent, 3,
            "the default max_per_hour is 3, and all five scored above the threshold"
        );
        assert_eq!(summary.digested, 2);
        assert_eq!(f.log.recent(daytime()).unwrap().len(), 3);

        // And the history is durable: a second pass an instant later sends
        // nothing at all.
        for i in 6..=7 {
            record(&f.events, &format!("a{i}"), "canvas", "assignment", "Essay");
        }
        let again = build_triage_over(&f, vec![score(6, 95), score(7, 95)]);
        let summary = run_triage(&again, daytime()).await.unwrap();
        assert_eq!(summary.sent, 0, "the hour's budget is already spent");
        assert_eq!(summary.digested, 2);
    }

    /// Rebuild the deps over the same database with different tier-1 scores,
    /// which is what "the next pass" looks like.
    fn build_triage_over(f: &TriageFixture, scores: Vec<Salience>) -> TriageDeps {
        TriageDeps {
            events: f.deps.events.clone(),
            actions: f.deps.actions.clone(),
            runs: f.deps.runs.clone(),
            sessions: Some(FakeTier1::new(scores, f.deps.runs.clone()) as Arc<dyn SessionBoundary>),
            pusher: Some(Arc::clone(&f.pusher) as Arc<dyn Pusher>),
            log: NotificationLog::new(KvStore::new(Arc::clone(&f.conn))),
            notify: NotificationPolicy::new(NotifyConfig::default()),
            rules: Tier0Rules::default(),
            daily_session_budget: 60,
        }
    }

    #[tokio::test]
    async fn a_failed_send_is_not_charged_against_the_rate_limit_and_is_not_lost() {
        let f = build_triage(vec![score(1, 90)], Tier0Rules::default(), 60, true, true);
        record(&f.events, "a1", "canvas", "assignment", "Essay due");

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.sent, 0);
        assert_eq!(summary.digested, 1, "the line must still reach the digest");
        assert!(
            f.log.recent(daytime()).unwrap().is_empty(),
            "a send that failed is not an interruption the human received"
        );
    }

    #[tokio::test]
    async fn triage_skips_tier1_once_the_daily_budget_is_spent() {
        let f = build_triage(vec![score(1, 90)], Tier0Rules::default(), 1, true, false);
        record(&f.events, "a1", "canvas", "assignment", "Essay");

        // First pass spends the day's single session.
        let first = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(first.scored, 1);

        record(&f.events, "a2", "canvas", "assignment", "Lab");
        let second = run_triage(&f.deps, daytime()).await.unwrap();
        assert!(second.budget_exhausted);
        assert_eq!(second.scored, 0);
        assert_eq!(f.tier1.as_ref().unwrap().sessions(), 1);
    }

    #[tokio::test]
    async fn triage_without_a_session_runner_still_does_tier0() {
        let rules = Tier0Rules {
            muted_kinds: vec!["ping".to_string()],
            ..Tier0Rules::default()
        };
        let f = build_triage(Vec::new(), rules, 60, false, false);
        record(&f.events, "p1", "canvas", "ping", "noise");
        record(&f.events, "a1", "canvas", "assignment", "Essay");

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.dropped, 1);
        assert_eq!(summary.scored, 0);
        assert!(f.pusher.sent().is_empty());
    }

    #[tokio::test]
    async fn an_empty_database_is_a_cheap_no_op() {
        let f = build_triage(Vec::new(), Tier0Rules::default(), 60, true, false);
        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary, TriageSummary::default());
        assert_eq!(f.tier1.as_ref().unwrap().sessions(), 0);
    }

    #[test]
    fn a_zero_budget_stops_everything() {
        let dir = TempDir::new().unwrap();
        let runs = RunStore::new(db(&dir));
        assert!(session_budget_spent(&runs, 0, daytime()).unwrap());
        assert!(!session_budget_spent(&runs, 1, daytime()).unwrap());
    }

    // -- the triage wedge ---------------------------------------------------
    //
    // The failure these cover: an event the model simply leaves out of
    // `scores` was never stamped `triaged_at`, so with `ORDER BY id` it stayed
    // at the head of `untriaged` forever. Two hundred of them and triage was
    // wedged permanently while still spending one Haiku session every five
    // minutes, for nothing, indefinitely.

    /// A tier 1 that answers — successfully, with well-formed structured
    /// output — but leaves events out of it. That is the real shape of this
    /// failure: not an error, which the breaker would see, but a reply that
    /// silently omits rows.
    struct PartialTier1 {
        /// Event ids this model is willing to score. `None` means none at all.
        scores_ids: Option<Vec<i64>>,
        /// The batch of event ids submitted on each call, in order.
        batches: Arc<Mutex<Vec<Vec<i64>>>>,
        runs: RunStore,
    }

    impl PartialTier1 {
        fn new(scores_ids: Option<Vec<i64>>, runs: RunStore) -> Arc<Self> {
            Arc::new(Self {
                scores_ids,
                batches: Arc::new(Mutex::new(Vec::new())),
                runs,
            })
        }

        fn batches(&self) -> Vec<Vec<i64>> {
            self.batches.lock().unwrap().clone()
        }

        /// The event ids `tier1_prompt` wrote into the prompt.
        fn ids_in(prompt: &str) -> Vec<i64> {
            prompt
                .lines()
                .filter_map(|line| line.strip_prefix("event_id: "))
                .filter_map(|id| id.trim().parse().ok())
                .collect()
        }
    }

    impl SessionBoundary for PartialTier1 {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            let run_id = self.runs.start(&req.kind, &req.prompt, &[]).unwrap();
            let submitted = Self::ids_in(&req.prompt);
            self.batches.lock().unwrap().push(submitted.clone());
            let answered: Vec<Salience> = match &self.scores_ids {
                None => Vec::new(),
                Some(allowed) => submitted
                    .iter()
                    .filter(|id| allowed.contains(id))
                    .map(|id| score(*id, 50))
                    .collect(),
            };
            let structured = serde_json::json!({ "scores": answered });
            let runs = self.runs.clone();
            Box::pin(async move {
                runs.finish(run_id, "ok", None, &[], Some(0.002))?;
                Ok(SessionOutcome {
                    text: String::new(),
                    structured: Some(structured),
                    session_id: None,
                    cost_usd: Some(0.002),
                })
            })
        }
    }

    /// Build a triage fixture whose tier 1 is a [`PartialTier1`].
    fn build_partial_triage(scores_ids: Option<Vec<i64>>) -> (TriageFixture, Arc<PartialTier1>) {
        let mut f = build_triage(Vec::new(), Tier0Rules::default(), 1000, true, false);
        let model = PartialTier1::new(scores_ids, f.deps.runs.clone());
        f.deps.sessions = Some(Arc::clone(&model) as Arc<dyn SessionBoundary>);
        (f, model)
    }

    fn record_n(events: &EventStore, n: usize) -> Vec<i64> {
        (0..n)
            .map(|i| record(events, &format!("e{i}"), "canvas", "assignment", "Essay"))
            .collect()
    }

    /// The finding, stated directly: a model that returns no scores at all
    /// must not prevent later events from ever being triaged.
    #[tokio::test]
    async fn a_model_that_scores_nothing_does_not_wedge_triage_forever() {
        let (f, model) = build_partial_triage(None);
        // More than one batch holds, so there is a "behind the head of the
        // queue" to be starved in the first place.
        let ids = record_n(&f.events, 45);

        // Four passes is enough to spend three attempts on all 45 (40 slots a
        // pass, 135 attempts needed).
        let mut abandoned = 0;
        for _ in 0..4 {
            let summary = run_triage(&f.deps, daytime()).await.unwrap();
            assert_eq!(summary.scored, 0);
            abandoned += summary.abandoned;
        }
        assert_eq!(abandoned, 45, "every unscorable event must be given up on");

        assert!(
            f.events.untriaged(500).unwrap().is_empty(),
            "triage must not still be scanning the same doomed rows"
        );
        assert_eq!(f.events.abandoned_count().unwrap(), 45);

        // Given up on, but not vanished: the reason is on the row, and the
        // salience is left NULL so it cannot be mistaken for "scored, boring".
        let event = f.events.get(ids[0]).unwrap().unwrap();
        assert_eq!(event.triage_attempts, TRIAGE_MAX_ATTEMPTS);
        assert!(event.triaged_at.is_some());
        assert_eq!(event.salience, None);
        assert_eq!(event.triage_error.as_deref(), Some(UNSCORABLE_REASON));

        // And it is in the digest, where a human reading the backlog sees it.
        let digest = f.log.digest().unwrap();
        assert!(
            digest.iter().any(|line| line.contains("gave up on event")),
            "{digest:?}"
        );

        // A fifth pass spends no session at all: there is nothing left to ask
        // about. Before the fix this would have been session number five of an
        // unbounded series, every five minutes, forever.
        let before = model.batches().len();
        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.scanned, 0);
        assert_eq!(
            model.batches().len(),
            before,
            "no session for an empty scan"
        );
    }

    /// The other half: events behind the doomed ones must get scored *while*
    /// the doomed ones are still being retried, not only once they are gone.
    #[tokio::test]
    async fn events_behind_an_unscorable_batch_are_still_triaged() {
        let (f, model) = build_partial_triage(Some((41..=45).collect()));
        let ids = record_n(&f.events, 45);
        let later: Vec<i64> = ids[40..].to_vec();

        // Pass one submits the first forty by id and gets nothing back.
        let first = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(first.scored, 0);
        assert_eq!(first.unscored, 40);
        assert_eq!(model.batches()[0].len(), 40);
        assert!(
            !model.batches()[0].contains(&later[0]),
            "the later events cannot have been in the first batch"
        );

        // Pass two: the forty that failed sort behind the five that have not
        // been tried, so the later events reach the model and are scored.
        let second = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(second.scored, 5);
        for id in &later {
            let event = f.events.get(*id).unwrap().unwrap();
            assert_eq!(
                event.salience,
                Some(50),
                "event {id} must have been scored by the second pass"
            );
        }
    }

    /// One miss is not a verdict: a model that skips an event in one batch and
    /// scores it in the next must leave no trace of having given up.
    #[tokio::test]
    async fn one_missed_batch_does_not_give_up_on_an_event() {
        let (f, _model) = build_partial_triage(None);
        let id = record(&f.events, "e0", "canvas", "assignment", "Essay");

        let summary = run_triage(&f.deps, daytime()).await.unwrap();
        assert_eq!(summary.unscored, 1);
        assert_eq!(summary.abandoned, 0);

        let event = f.events.get(id).unwrap().unwrap();
        assert_eq!(event.triage_attempts, 1);
        assert!(
            event.triaged_at.is_none(),
            "one miss must leave it in the queue"
        );

        // The next pass scores it, and the attempt count stops mattering.
        f.events.set_salience(id, 80).unwrap();
        let event = f.events.get(id).unwrap().unwrap();
        assert_eq!(event.salience, Some(80));
        assert_eq!(event.triage_error, None);
        assert_eq!(f.events.abandoned_count().unwrap(), 0);
    }

    /// A session that fails outright is not the event's fault. If failures
    /// were counted here, a week of `claude` being unreachable would abandon
    /// every event in the database.
    #[tokio::test]
    async fn a_failing_session_does_not_burn_an_event_s_attempts() {
        struct Broken;
        impl SessionBoundary for Broken {
            fn run_session(&self, _req: SessionRequest) -> BoxedSession<'_> {
                Box::pin(async { Err(anyhow::anyhow!("claude is not reachable")) })
            }
        }

        let mut f = build_triage(Vec::new(), Tier0Rules::default(), 1000, true, false);
        f.deps.sessions = Some(Arc::new(Broken) as Arc<dyn SessionBoundary>);
        let id = record(&f.events, "e0", "canvas", "assignment", "Essay");

        for _ in 0..5 {
            run_triage(&f.deps, daytime())
                .await
                .expect_err("a broken session must surface to the breaker");
        }

        let event = f.events.get(id).unwrap().unwrap();
        assert_eq!(event.triage_attempts, 0);
        assert!(event.triaged_at.is_none());
        assert_eq!(f.events.abandoned_count().unwrap(), 0);
    }
}
