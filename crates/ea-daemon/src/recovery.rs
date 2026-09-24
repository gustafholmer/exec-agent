//! What the last run left behind: the startup sweep.
//!
//! One thing in this system survives a crash in a state nothing else can see.
//! [`ActionStore::claim_for_execution`] stamps `executed_at` on an `approved`
//! row and hands the caller the right to reach the connector; the outcome is
//! written by `mark_executed` or `mark_failed`. A process that dies in between
//! — SIGKILL, a panic escaping the runtime, the laptop losing power mid-call —
//! leaves the row `approved` with a stamp and no outcome, which is absent from
//! `pending()`, from `expire_stale`, from `ea status`, and from every retention
//! DELETE (all of which name terminal statuses only). The owner approved
//! something, it never reported back, and nothing will ever mention it again.
//!
//! # Why these are resolved and never re-run
//!
//! The half-state means *"somebody started this and we do not know how it
//! ended"*. It does **not** mean the connector was never called: the crash may
//! have landed after the HTTP request was accepted and before the row was
//! updated. Re-driving it would post the voucher to company accounting a
//! second time, send the email again, submit the assignment again. There is no
//! idempotency key on the far side of an arbitrary MCP tool, so there is no
//! safe automatic retry — and an unnecessary duplicate write to real
//! bookkeeping is a worse outcome than an action the owner has to re-approve.
//!
//! So the sweep makes them *visible* instead: each becomes `failed` with a
//! reason that says what happened and that the outcome is unknown, keeping the
//! claim stamp so the log still shows when the lost attempt started. It lands
//! in `ea log`, in the digest, and in the daemon's own log at `warn`. The owner
//! decides whether to ask for it again.

use std::sync::Arc;

use ea_core::store::actions::{Action, ActionStore};

use crate::jobs::Pusher;
use crate::notify::log::NotificationLog;

/// Recorded on every row the sweep resolves.
pub const STRANDED_REASON: &str =
    "the daemon stopped while this action was running, so the outcome is unknown: \
     it may have reached the connector or it may not. It was NOT retried \
     automatically, because repeating it could duplicate a real-world change. \
     Ask for it again if it still needs doing.";

/// Resolve every action stranded by a previous run, and say so where the owner
/// will see it. Returns the rows it resolved.
///
/// Run once at startup, before the scheduler and before the socket is bound:
/// the executor must not be able to claim anything new while this is deciding
/// what "stranded" means, and the answer is in `ea status`/`ea log` by the time
/// the owner can ask.
pub async fn sweep_stranded(
    actions: &ActionStore,
    log: &NotificationLog,
    pusher: Option<&Arc<dyn Pusher>>,
) -> anyhow::Result<Vec<Action>> {
    let stranded = actions.stranded()?;
    if stranded.is_empty() {
        tracing::debug!("startup sweep: no actions were stranded by a previous run");
        return Ok(Vec::new());
    }

    let mut resolved = Vec::new();
    for action in stranded {
        if !actions.fail_stranded(action.id, STRANDED_REASON)? {
            // Only reachable if something else resolved it between the read
            // and the write, which at startup means a second daemon — and the
            // instance lock rules that out. Logged rather than ignored anyway.
            tracing::warn!(
                action = action.id,
                "a stranded action resolved itself mid-sweep"
            );
            continue;
        }
        tracing::warn!(
            action = action.id,
            connector = %action.connector,
            tool = %action.tool,
            claimed_at = action.executed_at.as_deref().unwrap_or("?"),
            "resolving an action stranded by a previous run: it was NOT retried"
        );
        log.push_digest(format!(
            "[recovery] #{} {}.{} was interrupted mid-execution by a daemon restart; \
             the outcome is unknown and it was not retried",
            action.id, action.connector, action.tool
        ))?;
        resolved.push(action);
    }

    if let (Some(pusher), false) = (pusher, resolved.is_empty()) {
        let text = summary(&resolved);
        // A failed push must not stop the daemon starting: the rows are
        // already resolved and the digest already has them.
        if let Err(err) = pusher.notify(&text).await {
            tracing::warn!(error = %format!("{err:#}"), "could not push the stranded-action summary");
        }
    }

    Ok(resolved)
}

/// The message the owner gets on their phone.
fn summary(resolved: &[Action]) -> String {
    let mut text = format!(
        "exec-agent restarted with {} approved action{} left mid-execution. \
         The outcome of each is unknown and none was retried — repeating one could \
         duplicate a real change. Ask again for anything that still needs doing.",
        resolved.len(),
        if resolved.len() == 1 { "" } else { "s" },
    );
    for action in resolved {
        text.push_str(&format!(
            "\n#{} {}.{}",
            action.id, action.connector, action.tool
        ));
    }
    text.push_str("\n`ea log` for the detail.");
    text
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ea_core::store::actions::{ActionStatus, ProposeInput};
    use ea_core::store::kv::KvStore;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;

    #[derive(Default)]
    struct SpyPusher {
        sent: Mutex<Vec<String>>,
    }

    impl Pusher for SpyPusher {
        fn notify<'a>(
            &'a self,
            text: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
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

    fn db(dir: &TempDir) -> Arc<Mutex<Connection>> {
        Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ))
    }

    fn input() -> ProposeInput {
        ProposeInput {
            connector: "fortnox".into(),
            tool: "record_voucher".into(),
            args: serde_json::json!({ "amount": 1250 }),
            preview: "Record a voucher for 1250 SEK".into(),
            rationale: "the invoice arrived".into(),
            ttl: chrono::Duration::hours(24),
        }
    }

    /// Simulate the crash: approve, claim, and never write an outcome.
    fn strand(actions: &ActionStore) -> i64 {
        let action = actions.propose(input()).unwrap();
        actions.approve(action.id).unwrap();
        assert!(actions.claim_for_execution(action.id).unwrap());
        action.id
    }

    #[tokio::test]
    async fn a_stranded_action_is_resolved_visibly_and_not_re_run() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let actions = ActionStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        let id = strand(&actions);

        let pusher: Arc<dyn Pusher> = Arc::new(SpyPusher::default());
        let resolved = sweep_stranded(&actions, &log, Some(&pusher)).await.unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, id);

        let row = actions.get(id).unwrap().unwrap();
        assert_eq!(row.status, ActionStatus::Failed);
        let reason = row.reason.unwrap();
        assert!(reason.contains("daemon stopped"), "{reason}");
        assert!(reason.contains("NOT retried"), "{reason}");
        assert!(
            row.result.is_none(),
            "nothing was called, so there is no result to record"
        );

        let digest = log.digest().unwrap();
        assert_eq!(digest.len(), 1);
        assert!(digest[0].contains("#1"), "{:?}", digest[0]);
        assert!(digest[0].contains("not retried"), "{:?}", digest[0]);
    }

    #[tokio::test]
    async fn the_owner_is_told_on_their_phone() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let actions = ActionStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        strand(&actions);

        let spy = Arc::new(SpyPusher::default());
        let pusher: Arc<dyn Pusher> = Arc::clone(&spy) as Arc<dyn Pusher>;
        sweep_stranded(&actions, &log, Some(&pusher)).await.unwrap();

        let sent = spy.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("fortnox.record_voucher"), "{}", sent[0]);
        assert!(sent[0].contains("none was retried"), "{}", sent[0]);
    }

    #[tokio::test]
    async fn a_clean_start_resolves_nothing_and_says_nothing() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let actions = ActionStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));

        // A proposal waiting for a human, and an action already executed:
        // neither is stranded.
        let waiting = actions.propose(input()).unwrap();
        let done = actions.propose(input()).unwrap();
        actions.approve(done.id).unwrap();
        actions.claim_for_execution(done.id).unwrap();
        actions.mark_executed(done.id, "ok").unwrap();

        let spy = Arc::new(SpyPusher::default());
        let pusher: Arc<dyn Pusher> = Arc::clone(&spy) as Arc<dyn Pusher>;
        assert!(sweep_stranded(&actions, &log, Some(&pusher))
            .await
            .unwrap()
            .is_empty());

        assert!(spy.sent.lock().unwrap().is_empty());
        assert!(log.digest().unwrap().is_empty());
        assert_eq!(
            actions.get(waiting.id).unwrap().unwrap().status,
            ActionStatus::Proposed,
            "a proposal waiting for a human must be left alone"
        );
        assert_eq!(
            actions.get(done.id).unwrap().unwrap().status,
            ActionStatus::Executed
        );
    }

    #[tokio::test]
    async fn the_sweep_works_without_a_notifier() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let actions = ActionStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        let id = strand(&actions);

        let resolved = sweep_stranded(&actions, &log, None).await.unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(
            actions.get(id).unwrap().unwrap().status,
            ActionStatus::Failed
        );
    }

    /// The second start must find nothing: the sweep is not a recurring
    /// announcement of the same crash.
    #[tokio::test]
    async fn sweeping_twice_resolves_nothing_the_second_time() {
        let dir = TempDir::new().unwrap();
        let conn = db(&dir);
        let actions = ActionStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        strand(&actions);

        assert_eq!(sweep_stranded(&actions, &log, None).await.unwrap().len(), 1);
        assert!(sweep_stranded(&actions, &log, None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(log.digest().unwrap().len(), 1);
    }
}
