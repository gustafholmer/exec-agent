//! The executor: the single place where a proposed action may reach a
//! connector.
//!
//! A language model proposes; it never performs. Every proposal passes through
//! [`Executor::submit`], which asks [`Policy::decide`] *before* the action is
//! even recorded, and then does exactly one of three things:
//!
//! * `Deny` — reject the action on the spot, recording the policy's own reason.
//!   The connector is never touched.
//! * `Approve` — leave the action `proposed` for a human to decide in Telegram.
//!   The connector is never touched.
//! * `Auto` — approve and execute now.
//!
//! The decision is the *only* input that determines whether a connector is
//! reached. This crate holds OAuth tokens for company accounting, mail and
//! calendars; an action that executes without the gate's consent posts a
//! voucher or sends mail that nobody authorised.

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::{anyhow, bail};
use ea_core::policy::{Mode, Policy};
use ea_core::store::actions::{Action, ActionStatus, ActionStore, ProposeInput};
use ea_core::store::runs::RunStore;
use serde_json::Value;

use crate::connectors::{Registry, DEFAULT_CALL_TIMEOUT};

/// The `kind` recorded on every `runs` row the executor writes.
pub const RUN_KIND: &str = "execute";

/// The one thing the executor is allowed to do to the outside world.
///
/// Deliberately narrow: no retry policy, no connector discovery. The executor
/// decides *whether* a call happens, never *how*.
///
/// [`ToolCaller::call`] carries no deadline, because the executor has no
/// opinion about one: choosing how long a connector may take is the
/// registry's business, and a test double should not have to pretend to have
/// a clock. [`ToolCaller::call_with_timeout`] exists for the one caller that
/// *does* have an opinion — the scheduler's `watch_poll`, whose work is a
/// whole connector's world rather than one action, and whose deadline
/// ([`crate::jobs::WATCH_TIMEOUT`]) is therefore its own. Its default body
/// ignores the deadline and defers to `call`, which is right for a test
/// double and wrong for anything that really reaches a process, so
/// [`Registry`] overrides it.
///
/// Native `async fn` in a trait, not `#[async_trait]`: the executor is generic
/// over its caller, so the trait never needs to be object-safe, and the
/// returned future is spelled out as `Send` so an `Executor` can be driven from
/// a spawned task.
pub trait ToolCaller {
    fn call(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;

    /// [`ToolCaller::call`] with a caller-chosen deadline.
    fn call_with_timeout(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
        _timeout: std::time::Duration,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        self.call(connector, tool, args)
    }
}

/// Sharing one caller between the executor and the scheduler's poll jobs.
///
/// The executor takes its caller by value, and the `watch_poll` jobs need the
/// very same [`Registry`] — a second registry would mean a second child
/// process per connector, two warm clients, and two sets of credentials open
/// on the same account. One `Arc`, two owners.
impl<T: ToolCaller + Send + Sync + ?Sized> ToolCaller for std::sync::Arc<T> {
    fn call(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        T::call(self, connector, tool, args)
    }

    /// Forwarded explicitly. Inheriting the default here would silently drop
    /// the caller's deadline on the way through the `Arc` — and the daemon
    /// only ever holds its registry behind one, so the default would mean
    /// *nothing in production* ever got the deadline it asked for.
    fn call_with_timeout(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
        timeout: std::time::Duration,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        T::call_with_timeout(self, connector, tool, args, timeout)
    }
}

impl ToolCaller for Registry {
    fn call(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        Registry::call(self, connector, tool, args, DEFAULT_CALL_TIMEOUT)
    }

    fn call_with_timeout(
        &self,
        connector: &str,
        tool: &str,
        args: Value,
        timeout: std::time::Duration,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        Registry::call(self, connector, tool, args, timeout)
    }
}

pub struct Executor<C: ToolCaller> {
    actions: ActionStore,
    runs: RunStore,
    policy: Policy,
    caller: C,
    /// Action ids with a connector call in flight right now — the *first* of
    /// two claim layers.
    ///
    /// The store's status transitions already stop a second execution from
    /// *recording* a second outcome, but that guard only fires after the
    /// connector has been called — too late for a voucher that has already been
    /// posted. A double-tapped Telegram button arrives as two concurrent
    /// [`Executor::execute_approved`] calls in this one process, so this set is
    /// what makes the second one a cheap no-op rather than a second side
    /// effect, with an error that names the real reason.
    ///
    /// It is deliberately **not** the authority. It lives in memory and is
    /// released on drop, which means it is gone the instant the `run` future is
    /// dropped by a `tokio::time::timeout` or a `JoinHandle::abort`, and gone
    /// entirely if the process dies. The authority is
    /// [`ActionStore::claim_for_execution`], taken immediately before the
    /// connector call and never released. See [`Executor::run`].
    in_flight: Mutex<HashSet<i64>>,
}

/// Releases the in-memory in-flight claim however the execution ends, panic
/// included. It does **not** release the durable claim, which is one-way by
/// design.
struct Claim<'a> {
    in_flight: &'a Mutex<HashSet<i64>>,
    id: i64,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut guard = match self.in_flight.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.remove(&self.id);
    }
}

impl<C: ToolCaller> Executor<C> {
    pub fn new(actions: ActionStore, runs: RunStore, policy: Policy, caller: C) -> Self {
        Self {
            actions,
            runs,
            policy,
            caller,
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Put a proposed action through the gate.
    ///
    /// Returns the action in its resulting state and whether it executed.
    pub async fn submit(&self, input: ProposeInput) -> anyhow::Result<(Action, bool)> {
        // Decided before the action exists: nothing about the stored row can
        // influence whether the connector is reached.
        let decision = self.policy.decide(&input.connector, &input.tool);
        let action = self.actions.propose(input)?;

        match decision.mode {
            Mode::Deny => Ok((
                self.actions.reject(
                    action.id,
                    &format!("policy denies this tool: {}", decision.reason),
                )?,
                false,
            )),
            Mode::Approve => Ok((action, false)),
            Mode::Auto => {
                self.actions.approve(action.id)?;
                Ok((self.run(action.id).await?, true))
            }
        }
    }

    /// Execute an action a human has already approved — the path behind the
    /// Telegram button and `ea approve`.
    ///
    /// Refuses anything that is not currently `approved`, and never approves
    /// anything itself: the approval is the human's to give.
    pub async fn execute_approved(&self, id: i64) -> anyhow::Result<Action> {
        self.run(id).await
    }

    /// Call the connector for an approved action and record what happened.
    ///
    /// A connector failure is an outcome, not a fault: it is written to the
    /// action and to the `runs` row, and `Ok` is still returned. Letting a
    /// connector error escape here would let one bad tool call abort a whole
    /// scheduled job.
    ///
    /// # The two claim layers
    ///
    /// 1. [`Executor::claim`] — an in-memory set, released on every exit path.
    ///    It stops the ordinary concurrent double tap cheaply and gives the
    ///    loser a clear error. It is *not* authoritative: dropping this future
    ///    releases it, and a process restart forgets it.
    /// 2. [`ActionStore::claim_for_execution`] — one conditional UPDATE
    ///    stamping `executed_at` while the row is still `approved`. Taken
    ///    immediately before the connector call and **never released**. This is
    ///    the authority: after it succeeds, no second call to this action can
    ///    ever reach the connector, whatever happens next — a store fault on
    ///    `mark_executed`, a `timeout` dropping this future with the voucher
    ///    already posted, or the process dying mid-call.
    ///
    /// The residue in those cases is an `approved` row with a non-null
    /// `executed_at`: a half-state that says "started, outcome unknown". That
    /// is the point. It is not re-executable, and it is recognisable to a
    /// future recovery pass in a way that a plain `approved` row is not.
    ///
    /// The `runs` row is opened *before* the durable claim so that even a crash
    /// between the two leaves a trace, and it carries the action id from the
    /// start rather than only at `finish` — an abandoned `running` row with no
    /// id cannot be tied back to the money it was spending.
    async fn run(&self, id: i64) -> anyhow::Result<Action> {
        let _claim = self.claim(id)?;

        let action = self
            .actions
            .get(id)?
            .ok_or_else(|| anyhow!("action {id} does not exist"))?;
        if action.status != ActionStatus::Approved {
            bail!(
                "action {id} is {}, not approved; it cannot be executed",
                action.status.as_str()
            );
        }

        let run_id = self.runs.start(
            RUN_KIND,
            &format!("{}.{}", action.connector, action.tool),
            &[id],
        )?;

        // The last thing before the side effect, and the only thing between
        // this line and the connector call.
        if !self.actions.claim_for_execution(id)? {
            let detail = format!(
                "action {id} is already claimed for execution \
                 (approved with a non-null executed_at); refusing to call the connector again"
            );
            self.runs
                .finish(run_id, "error", Some(&detail), &[id], None)?;
            bail!(detail);
        }

        let called = self
            .caller
            .call(&action.connector, &action.tool, action.args.clone())
            .await;

        let (updated, outcome, detail) = match called {
            Ok(result) => {
                let updated = self.actions.mark_executed(id, &result);
                (updated, "ok", result)
            }
            Err(err) => {
                let message = format!("{err:#}");
                let updated = self.actions.mark_failed(id, &message);
                (updated, "error", message)
            }
        };

        // Finished before `updated` is unwrapped, so a store failure on the
        // action still leaves a closed run row behind rather than a `running`
        // one that never resolves.
        self.runs
            .finish(run_id, outcome, Some(&detail), &[id], None)?;
        updated
    }

    /// Take the in-memory in-flight claim on `id`, or refuse because a call is
    /// already in flight in this process. Not authoritative — see
    /// [`Executor::run`].
    fn claim(&self, id: i64) -> anyhow::Result<Claim<'_>> {
        let mut guard = match self.in_flight.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !guard.insert(id) {
            bail!("action {id} is already being executed");
        }
        drop(guard);
        Ok(Claim {
            in_flight: &self.in_flight,
            id,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ea_core::store::actions::ActionStatus;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;

    /// `ea_core::store::test_support::temp_store` is `#[cfg(test)]
    /// pub(crate)`, so it does not exist in the compiled `ea-core` this crate
    /// links against. Rather than widen ea-core's public surface for a test
    /// helper, this is the same three lines against the public `db::open`.
    fn temp_store() -> (TempDir, Arc<Mutex<Connection>>) {
        let dir = TempDir::new().unwrap();
        let conn = ea_core::db::open(&dir.path().join("state.db")).unwrap();
        (dir, Arc::new(Mutex::new(conn)))
    }

    #[derive(Clone)]
    enum Behaviour {
        Ok(String),
        Err(String),
    }

    /// Records every call it is asked to make. The assertions that matter are
    /// the ones on `calls()` being empty: "the connector was never reached" is
    /// the property the gate exists to provide, and a status check alone does
    /// not prove it.
    struct FakeCaller {
        calls: Mutex<Vec<(String, String, Value)>>,
        behaviour: Behaviour,
        /// Held across the await so two concurrent executions genuinely
        /// overlap in the concurrency test.
        delay: Duration,
        /// The database, for the two tests that need to look at it — or break
        /// it — from *inside* the connector call. That instant is the one the
        /// findings are about: the side effect has landed and nothing has been
        /// recorded yet.
        db: Option<Arc<Mutex<Connection>>>,
        /// `runs` rows as they looked mid-call: (outcome, action_ids).
        runs_mid_call: Mutex<Vec<(String, Vec<i64>)>>,
        /// Flip the connection read-only just before returning, so every store
        /// write after the call fails the way a real store fault would.
        poison_db: bool,
    }

    impl FakeCaller {
        fn ok() -> Self {
            Self::with(Behaviour::Ok("done".into()))
        }

        fn failing(message: &str) -> Self {
            Self::with(Behaviour::Err(message.into()))
        }

        fn with(behaviour: Behaviour) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                behaviour,
                delay: Duration::ZERO,
                db: None,
                runs_mid_call: Mutex::new(Vec::new()),
                poison_db: false,
            }
        }

        fn slow(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        /// Snapshot the `runs` table while the call is in flight.
        fn watching_db(mut self, conn: Arc<Mutex<Connection>>) -> Self {
            self.db = Some(conn);
            self
        }

        /// Snapshot the `runs` table, then make the database read-only, so the
        /// store writes that follow a *successful* call all fail.
        fn poisoning_db(mut self, conn: Arc<Mutex<Connection>>) -> Self {
            self.db = Some(conn);
            self.poison_db = true;
            self
        }

        fn calls(&self) -> Vec<(String, String, Value)> {
            self.calls.lock().unwrap().clone()
        }

        fn runs_mid_call(&self) -> Vec<(String, Vec<i64>)> {
            self.runs_mid_call.lock().unwrap().clone()
        }
    }

    impl ToolCaller for FakeCaller {
        async fn call(&self, connector: &str, tool: &str, args: Value) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string(), args));

            if let Some(db) = &self.db {
                let conn = db.lock().unwrap();
                let mut stmt = conn
                    .prepare("SELECT outcome, action_ids FROM runs ORDER BY id")
                    .unwrap();
                let rows: Vec<(String, Vec<i64>)> = stmt
                    .query_map([], |row| {
                        let ids: Option<String> = row.get(1)?;
                        Ok((
                            row.get::<_, String>(0)?,
                            ids.and_then(|s| serde_json::from_str(&s).ok())
                                .unwrap_or_default(),
                        ))
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                drop(stmt);
                *self.runs_mid_call.lock().unwrap() = rows;
                if self.poison_db {
                    conn.pragma_update(None, "query_only", true).unwrap();
                }
            }

            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            match &self.behaviour {
                Behaviour::Ok(result) => Ok(result.clone()),
                Behaviour::Err(message) => Err(anyhow!("{message}")),
            }
        }
    }

    impl<C: ToolCaller> Executor<C> {
        fn caller(&self) -> &C {
            &self.caller
        }
    }

    fn policy() -> Policy {
        Policy::parse(
            r#"
[canvas]
list_courses = "auto"
submit_assignment = { mode = "deny", note = "a human submits their own coursework" }

[fortnox]
record_voucher = "approve"
"#,
        )
        .unwrap()
    }

    fn input(connector: &str, tool: &str) -> ProposeInput {
        ProposeInput {
            connector: connector.into(),
            tool: tool.into(),
            args: serde_json::json!({ "term": "HT26" }),
            preview: format!("{connector}.{tool}"),
            rationale: "the agent asked".into(),
            ttl: chrono::Duration::hours(24),
        }
    }

    struct Harness {
        _dir: TempDir,
        actions: ActionStore,
        conn: Arc<Mutex<Connection>>,
    }

    fn harness() -> (Harness, ActionStore, RunStore) {
        let (dir, conn) = temp_store();
        let h = Harness {
            _dir: dir,
            actions: ActionStore::new(conn.clone()),
            conn: conn.clone(),
        };
        (h, ActionStore::new(conn.clone()), RunStore::new(conn))
    }

    impl Harness {
        /// Every `runs` row, newest last, as (kind, outcome, action_ids).
        fn runs_rows(&self) -> Vec<(String, String, Vec<i64>)> {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT kind, outcome, action_ids FROM runs ORDER BY id")
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    let ids: Option<String> = row.get(2)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        ids.and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or_default(),
                    ))
                })
                .unwrap();
            rows.map(Result::unwrap).collect()
        }
    }

    #[tokio::test]
    async fn an_auto_tool_executes_and_calls_the_connector_once() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (action, executed) = ex.submit(input("canvas", "list_courses")).await.unwrap();

        assert!(executed);
        assert_eq!(action.status, ActionStatus::Executed);
        assert_eq!(action.result.as_deref(), Some("done"));
        assert_eq!(
            h.actions.get(action.id).unwrap().unwrap().status,
            ActionStatus::Executed
        );

        let calls = ex.caller().calls();
        assert_eq!(calls.len(), 1, "expected exactly one connector call");
        assert_eq!(calls[0].0, "canvas");
        assert_eq!(calls[0].1, "list_courses");
        assert_eq!(calls[0].2["term"], "HT26");
    }

    #[tokio::test]
    async fn an_approve_tool_stays_proposed_and_never_reaches_the_connector() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (action, executed) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();

        assert!(!executed);
        assert_eq!(action.status, ActionStatus::Proposed);
        assert!(
            ex.caller().calls().is_empty(),
            "an action awaiting a human must not touch the connector"
        );
        assert_eq!(h.actions.pending().unwrap().len(), 1);
        assert!(
            h.runs_rows().is_empty(),
            "nothing ran, so nothing to record"
        );
    }

    #[tokio::test]
    async fn a_deny_tool_is_rejected_with_the_policy_reason_and_never_called() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (action, executed) = ex
            .submit(input("canvas", "submit_assignment"))
            .await
            .unwrap();

        assert!(!executed);
        assert_eq!(action.status, ActionStatus::Rejected);
        let reason = action.reason.expect("a rejection must say why");
        assert!(reason.contains("policy denies this tool"), "{reason}");
        assert!(
            reason.contains("a human submits their own coursework"),
            "the policy's own words must survive into the queue: {reason}"
        );
        assert!(
            ex.caller().calls().is_empty(),
            "a denied action must never reach the connector"
        );
        assert!(h.runs_rows().is_empty());
    }

    #[tokio::test]
    async fn an_unlisted_tool_queues_rather_than_auto_running() {
        let (_h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (action, executed) = ex.submit(input("canvas", "invented_tool")).await.unwrap();
        assert!(!executed);
        assert_eq!(action.status, ActionStatus::Proposed);

        let (unknown, executed) = ex.submit(input("stripe", "charge_card")).await.unwrap();
        assert!(!executed);
        assert_eq!(unknown.status, ActionStatus::Proposed);

        assert!(
            ex.caller().calls().is_empty(),
            "nothing unlisted may run without a human"
        );
    }

    #[tokio::test]
    async fn a_connector_error_fails_the_action_without_erroring_out() {
        let (_h, actions, runs) = harness();
        let ex = Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::failing("connector exploded"),
        );

        let result = ex.submit(input("canvas", "list_courses")).await;
        let (action, executed) = result.expect("a connector failure is an outcome, not a fault");

        assert!(executed);
        assert_eq!(action.status, ActionStatus::Failed);
        assert!(
            action
                .reason
                .as_deref()
                .unwrap()
                .contains("connector exploded"),
            "{:?}",
            action.reason
        );
    }

    #[tokio::test]
    async fn a_connector_timeout_fails_the_action_with_the_message_preserved() {
        let (_h, actions, runs) = harness();
        let ex = Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::failing("connector canvas: tool list_courses timed out after 30000ms"),
        );

        let (action, _) = ex
            .submit(input("canvas", "list_courses"))
            .await
            .expect("a timeout must not propagate upward");

        assert_eq!(action.status, ActionStatus::Failed);
        let reason = action.reason.unwrap();
        assert!(reason.contains("timed out"), "{reason}");
        assert!(reason.contains("30000ms"), "{reason}");
    }

    #[tokio::test]
    async fn a_successful_execution_writes_a_run_row() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (action, _) = ex.submit(input("canvas", "list_courses")).await.unwrap();

        let rows = h.runs_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "execute");
        assert_eq!(rows[0].1, "ok");
        assert_eq!(rows[0].2, vec![action.id]);
    }

    #[tokio::test]
    async fn a_failed_execution_also_writes_a_run_row() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::failing("boom"));

        let (action, _) = ex.submit(input("canvas", "list_courses")).await.unwrap();

        let rows = h.runs_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "execute");
        assert_eq!(rows[0].1, "error");
        assert_eq!(rows[0].2, vec![action.id]);
    }

    #[tokio::test]
    async fn execute_approved_runs_a_queued_action_once_a_human_approves() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (queued, executed) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        assert!(!executed);
        assert!(ex.caller().calls().is_empty());

        // The human taps approve; only then does the executor get to run it.
        h.actions.approve(queued.id).unwrap();
        let done = ex.execute_approved(queued.id).await.unwrap();

        assert_eq!(done.status, ActionStatus::Executed);
        assert_eq!(done.result.as_deref(), Some("done"));
        assert_eq!(ex.caller().calls().len(), 1);
    }

    #[tokio::test]
    async fn execute_approved_refuses_an_action_no_human_approved() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (queued, _) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        let err = ex
            .execute_approved(queued.id)
            .await
            .expect_err("executing an unapproved action must fail");

        assert!(format!("{err:#}").contains("not approved"), "{err:#}");
        assert!(
            ex.caller().calls().is_empty(),
            "an unapproved action must never reach the connector"
        );
        assert_eq!(
            h.actions.get(queued.id).unwrap().unwrap().status,
            ActionStatus::Proposed,
            "and the executor must not approve it on the way past"
        );
    }

    #[tokio::test]
    async fn execute_approved_refuses_an_action_the_policy_already_rejected() {
        let (_h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (denied, _) = ex
            .submit(input("canvas", "submit_assignment"))
            .await
            .unwrap();
        assert!(ex.execute_approved(denied.id).await.is_err());
        assert!(ex.caller().calls().is_empty());
    }

    #[tokio::test]
    async fn execute_approved_refuses_an_action_whose_durable_claim_is_taken() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(actions, runs, policy(), FakeCaller::ok());

        let (queued, _) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        h.actions.approve(queued.id).unwrap();

        // Somebody already owns this execution: the previous process before it
        // died, a sibling daemon, a recovery job. The row is still `approved`,
        // which is exactly the state that used to look safe to re-run.
        assert!(h.actions.claim_for_execution(queued.id).unwrap());

        let err = ex
            .execute_approved(queued.id)
            .await
            .expect_err("a claimed action must not be executed a second time");
        assert!(format!("{err:#}").contains("already claimed"), "{err:#}");
        assert!(
            ex.caller().calls().is_empty(),
            "a claimed action must make zero connector calls"
        );
        assert_eq!(
            h.actions.get(queued.id).unwrap().unwrap().status,
            ActionStatus::Approved,
            "and the refusal must not rewrite the half-state"
        );

        let rows = h.runs_rows();
        assert_eq!(rows.len(), 1, "the refusal is itself worth recording");
        assert_eq!(rows[0].1, "error");
        assert_eq!(rows[0].2, vec![queued.id]);
    }

    /// Finding 1. The connector call lands, then the store write fails. The
    /// action is left `approved` — the status it had before — and the old code
    /// released its only claim on the way out, so the next retry posted the
    /// voucher again.
    #[tokio::test]
    async fn a_store_fault_after_a_successful_call_does_not_make_the_action_claimable_again() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::ok().poisoning_db(h.conn.clone()),
        );

        let (queued, _) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        h.actions.approve(queued.id).unwrap();

        let err = ex
            .execute_approved(queued.id)
            .await
            .expect_err("the store writes after the call all failed");
        assert!(format!("{err:#}").contains("readonly"), "{err:#}");
        assert_eq!(
            ex.caller().calls().len(),
            1,
            "the voucher was posted before the store broke"
        );

        // The operator fixes the store / the daemon restarts.
        h.conn
            .lock()
            .unwrap()
            .pragma_update(None, "query_only", false)
            .unwrap();

        let row = h.actions.get(queued.id).unwrap().unwrap();
        assert_eq!(
            row.status,
            ActionStatus::Approved,
            "the outcome never got recorded"
        );
        assert!(
            row.executed_at.is_some(),
            "but the claim did, before the call — that is the whole fix"
        );
        assert!(
            !h.actions.claim_for_execution(queued.id).unwrap(),
            "nobody may claim it again"
        );

        let again = ex
            .execute_approved(queued.id)
            .await
            .expect_err("a retry must refuse");
        assert!(
            format!("{again:#}").contains("already claimed"),
            "{again:#}"
        );
        assert_eq!(
            ex.caller().calls().len(),
            1,
            "exactly one voucher posted, ever"
        );
    }

    /// Finding 2. A later caller wraps the execution in a timeout. The `run`
    /// future is dropped mid-call, which releases the in-memory claim while the
    /// connector call may already have landed.
    #[tokio::test]
    async fn a_dropped_execution_stays_claimed_and_cannot_be_retried() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::ok().slow(Duration::from_secs(10)),
        );

        let (queued, _) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        h.actions.approve(queued.id).unwrap();

        let outcome =
            tokio::time::timeout(Duration::from_millis(30), ex.execute_approved(queued.id)).await;
        assert!(
            outcome.is_err(),
            "the timeout must fire and drop the future"
        );
        assert_eq!(
            ex.caller().calls().len(),
            1,
            "the connector call had already started"
        );

        let row = h.actions.get(queued.id).unwrap().unwrap();
        assert_eq!(row.status, ActionStatus::Approved);
        assert!(
            row.executed_at.is_some(),
            "the durable claim outlives the dropped future"
        );

        let err = ex
            .execute_approved(queued.id)
            .await
            .expect_err("a retry after the drop must refuse");
        let message = format!("{err:#}");
        assert!(
            message.contains("already claimed"),
            "the durable layer, not the in-memory one, must be what refuses: {message}"
        );
        assert!(
            !message.contains("already being executed"),
            "the in-memory claim really was released by the drop: {message}"
        );
        assert_eq!(
            ex.caller().calls().len(),
            1,
            "a dropped execution must never become a second side effect"
        );
    }

    /// Finding 3. A crash mid-call leaves only the `running` row behind. If it
    /// does not name the action, nothing can tie `"fortnox.record_voucher"`
    /// back to the money it was spending.
    #[tokio::test]
    async fn a_run_row_carries_the_action_id_before_finish_runs() {
        let (h, actions, runs) = harness();
        let ex = Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::ok().watching_db(h.conn.clone()),
        );

        let (action, _) = ex.submit(input("canvas", "list_courses")).await.unwrap();

        let mid = ex.caller().runs_mid_call();
        assert_eq!(mid.len(), 1, "one run row existed during the call");
        assert_eq!(mid[0].0, "running", "and it had not finished yet");
        assert_eq!(
            mid[0].1,
            vec![action.id],
            "yet it already named the action it was executing"
        );
    }

    /// A double-tapped Telegram button. Two taps, one voucher.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_executions_make_exactly_one_connector_call() {
        let (h, actions, runs) = harness();
        let ex = Arc::new(Executor::new(
            actions,
            runs,
            policy(),
            FakeCaller::ok().slow(Duration::from_millis(100)),
        ));

        let (queued, _) = ex.submit(input("fortnox", "record_voucher")).await.unwrap();
        h.actions.approve(queued.id).unwrap();

        let succeeded = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let ex = Arc::clone(&ex);
            let succeeded = Arc::clone(&succeeded);
            tasks.push(tokio::spawn(async move {
                if ex.execute_approved(queued.id).await.is_ok() {
                    succeeded.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(
            ex.caller().calls().len(),
            1,
            "a double tap must post one voucher, not two"
        );
        assert_eq!(succeeded.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.actions.get(queued.id).unwrap().unwrap().status,
            ActionStatus::Executed
        );
        let rows = h.runs_rows();
        assert_eq!(rows.len(), 1, "one execution, one run row");
    }

    // -- the deadline seam --------------------------------------------------

    /// Records the deadline it was called with, and nothing else.
    #[derive(Default)]
    struct DeadlineSpy {
        seen: Mutex<Vec<Option<Duration>>>,
    }

    impl ToolCaller for DeadlineSpy {
        async fn call(
            &self,
            _connector: &str,
            _tool: &str,
            _args: Value,
        ) -> anyhow::Result<String> {
            self.seen.lock().unwrap().push(None);
            Ok(String::new())
        }

        async fn call_with_timeout(
            &self,
            _connector: &str,
            _tool: &str,
            _args: Value,
            timeout: Duration,
        ) -> anyhow::Result<String> {
            self.seen.lock().unwrap().push(Some(timeout));
            Ok(String::new())
        }
    }

    /// The daemon holds its registry behind an `Arc` and nothing else. If the
    /// `Arc` impl inherited the trait's default `call_with_timeout` — which
    /// drops the deadline and defers to `call` — then every caller-chosen
    /// deadline in the daemon would be silently discarded on the way through,
    /// and `watch_poll` would be back on the executor's 30 seconds with no
    /// test to say so.
    #[tokio::test]
    async fn an_arc_forwards_the_callers_deadline_rather_than_swallowing_it() {
        let spy = std::sync::Arc::new(DeadlineSpy::default());
        let caller: std::sync::Arc<DeadlineSpy> = std::sync::Arc::clone(&spy);

        ToolCaller::call_with_timeout(
            &caller,
            "canvas",
            "watch_poll",
            Value::Null,
            Duration::from_secs(90),
        )
        .await
        .unwrap();

        assert_eq!(
            spy.seen.lock().unwrap().clone(),
            vec![Some(Duration::from_secs(90))]
        );
    }
}
