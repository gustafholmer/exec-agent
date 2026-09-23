use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, Row};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionStatus {
    Proposed,
    Approved,
    Rejected,
    Expired,
    Executed,
    Failed,
}

impl ActionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Executed => "executed",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for ActionStatus {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "proposed" => Self::Proposed,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "expired" => Self::Expired,
            "executed" => Self::Executed,
            "failed" => Self::Failed,
            other => bail!("unknown action status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: i64,
    pub connector: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub preview: String,
    pub rationale: String,
    pub status: ActionStatus,
    pub reason: Option<String>,
    pub result: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub decided_at: Option<String>,
    pub executed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProposeInput {
    pub connector: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub preview: String,
    pub rationale: String,
    pub ttl: Duration,
}

pub struct ActionStore {
    conn: Arc<Mutex<Connection>>,
}

fn hydrate(row: &Row<'_>) -> rusqlite::Result<Action> {
    let args: String = row.get("args")?;
    let status: String = row.get("status")?;
    Ok(Action {
        id: row.get("id")?,
        connector: row.get("connector")?,
        tool: row.get("tool")?,
        args: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
        preview: row.get("preview")?,
        rationale: row.get("rationale")?,
        status: status.parse().unwrap_or(ActionStatus::Failed),
        reason: row.get("reason")?,
        result: row.get("result")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        decided_at: row.get("decided_at")?,
        executed_at: row.get("executed_at")?,
    })
}

/// Which "decided at this instant" column a transition stamps. Only two
/// columns are ever involved, so the SQL for each is a fixed string
/// selected by this enum rather than assembled at runtime -- no column
/// name is ever built from caller input.
#[derive(Clone, Copy)]
enum Stamp {
    Decided,
    Executed,
}

impl ActionStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub fn propose(&self, input: ProposeInput) -> anyhow::Result<Action> {
        let now = Utc::now();
        let expires = now + input.ttl;
        let id = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO actions
                   (connector, tool, args, preview, rationale, status, created_at, expires_at)
                 VALUES (?1,?2,?3,?4,?5,'proposed',?6,?7)",
                params![
                    input.connector,
                    input.tool,
                    serde_json::to_string(&input.args)?,
                    input.preview,
                    input.rationale,
                    now.to_rfc3339(),
                    expires.to_rfc3339(),
                ],
            )?;
            conn.last_insert_rowid()
        };
        self.get(id)?
            .ok_or_else(|| anyhow!("action {id} vanished after insert"))
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Action>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM actions WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate)?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    pub fn pending(&self) -> anyhow::Result<Vec<Action>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM actions WHERE status = 'proposed' ORDER BY id")?;
        let rows = stmt.query_map([], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One conditional UPDATE, so two concurrent callers cannot both succeed.
    /// This is what makes a double-tapped Telegram button safe.
    ///
    /// The statement always references all six placeholders (`?1`..`?6`) and
    /// `params![]` always supplies six values, so the parameter count rusqlite
    /// validates against the prepared statement never drifts. `reason` and
    /// `result` are optional per-transition: when a caller doesn't supply one,
    /// `COALESCE(?N, column)` preserves whatever the column already held
    /// instead of the `SET` clause being conditionally omitted.
    fn transition(
        &self,
        id: i64,
        from: ActionStatus,
        to: ActionStatus,
        reason: Option<&str>,
        result: Option<&str>,
        stamp: Stamp,
    ) -> anyhow::Result<Action> {
        let now = Utc::now().to_rfc3339();
        const DECIDED_SQL: &str = "UPDATE actions SET \
             status = ?1, \
             reason = COALESCE(?4, reason), \
             result = COALESCE(?5, result), \
             decided_at = ?6 \
             WHERE id = ?2 AND status = ?3";
        const EXECUTED_SQL: &str = "UPDATE actions SET \
             status = ?1, \
             reason = COALESCE(?4, reason), \
             result = COALESCE(?5, result), \
             executed_at = ?6 \
             WHERE id = ?2 AND status = ?3";
        let sql = match stamp {
            Stamp::Decided => DECIDED_SQL,
            Stamp::Executed => EXECUTED_SQL,
        };

        let changed = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                sql,
                params![to.as_str(), id, from.as_str(), reason, result, now],
            )?
        };

        if changed == 0 {
            return match self.get(id)? {
                None => Err(anyhow!("action {id} does not exist")),
                Some(current) => Err(anyhow!(
                    "action {id} is {}, not {}; cannot move to {}",
                    current.status.as_str(),
                    from.as_str(),
                    to.as_str()
                )),
            };
        }
        self.get(id)?.ok_or_else(|| anyhow!("action {id} vanished"))
    }

    pub fn approve(&self, id: i64) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Proposed,
            ActionStatus::Approved,
            None,
            None,
            Stamp::Decided,
        )
    }

    pub fn reject(&self, id: i64, reason: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Proposed,
            ActionStatus::Rejected,
            Some(reason),
            None,
            Stamp::Decided,
        )
    }

    pub fn mark_executed(&self, id: i64, result: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Approved,
            ActionStatus::Executed,
            None,
            Some(result),
            Stamp::Executed,
        )
    }

    pub fn mark_failed(&self, id: i64, reason: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Approved,
            ActionStatus::Failed,
            Some(reason),
            None,
            Stamp::Executed,
        )
    }

    /// Durably claim an approved action for execution, one conditional UPDATE.
    ///
    /// Returns `true` exactly once per action: the caller that gets it owns the
    /// execution and may reach the connector. Every later attempt gets `false`,
    /// including after a crash, because the claim is a stamped `executed_at`
    /// column and not a process-local flag.
    ///
    /// This is deliberately one-way. The executor's in-memory in-flight set
    /// covers the ordinary double-tap and is released when the call finishes;
    /// this covers the cases it cannot see — the process dying mid-call, the
    /// `run` future being dropped by a `timeout` or an abort, or
    /// `mark_executed`/`mark_failed` itself failing. In all of those the row
    /// stays `approved` with a non-null `executed_at`, which is a
    /// *distinguishable half-state*: "somebody started this and we do not know
    /// how it ended". Nothing today re-drives approved actions, but the moment
    /// something does, that half-state is the difference between a recovery
    /// pass and a second voucher posted to real company accounting.
    ///
    /// No new status variant and no schema change: `executed_at` is a bare
    /// `TEXT` column, and `mark_executed`/`mark_failed` stamp it anyway, so a
    /// claimed action that completes normally is indistinguishable from one
    /// that was never claimed. `pending()` and `expire_stale()` only look at
    /// `proposed` rows and are unaffected.
    pub fn claim_for_execution(&self, id: i64) -> anyhow::Result<bool> {
        let now = Utc::now().to_rfc3339();
        let changed = {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE actions SET executed_at = ?2
                 WHERE id = ?1 AND status = 'approved' AND executed_at IS NULL",
                params![id, now],
            )
            .context("claiming an action for execution")?
        };
        Ok(changed == 1)
    }

    pub fn expire_stale(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE actions SET status = 'expired', reason = 'proposal expired'
             WHERE status = 'proposed' AND expires_at <= ?1",
            params![now.to_rfc3339()],
        )
        .context("expiring stale proposals")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    fn input() -> ProposeInput {
        ProposeInput {
            connector: "canvas".into(),
            tool: "list_courses".into(),
            args: serde_json::json!({ "term": "HT26" }),
            preview: "List courses for HT26".into(),
            rationale: "user asked".into(),
            ttl: chrono::Duration::hours(24),
        }
    }

    #[test]
    fn stores_a_proposal_and_round_trips_args() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        assert_eq!(a.status, ActionStatus::Proposed);
        assert_eq!(a.args["term"], "HT26");
        assert_eq!(
            store.get(a.id).unwrap().unwrap().preview,
            "List courses for HT26"
        );
    }

    #[test]
    fn pending_lists_only_proposals() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        store.propose(input()).unwrap();
        store.reject(a.id, "not now").unwrap();
        let pending = store.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, ActionStatus::Proposed);
    }

    #[test]
    fn approving_twice_fails_the_second_time() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        assert_eq!(store.approve(a.id).unwrap().status, ActionStatus::Approved);
        assert!(store.approve(a.id).is_err());
    }

    #[test]
    fn a_rejected_action_cannot_be_approved() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        store.reject(a.id, "no").unwrap();
        assert!(store.approve(a.id).is_err());
    }

    #[test]
    fn execution_requires_approval_first() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        assert!(store.mark_executed(a.id, "ok").is_err());
        store.approve(a.id).unwrap();
        let done = store.mark_executed(a.id, "ok").unwrap();
        assert_eq!(done.status, ActionStatus::Executed);
        assert_eq!(done.result.as_deref(), Some("ok"));
        assert!(done.executed_at.is_some());
    }

    #[test]
    fn failure_records_the_reason() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        store.approve(a.id).unwrap();
        let failed = store.mark_failed(a.id, "connector timed out").unwrap();
        assert_eq!(failed.status, ActionStatus::Failed);
        assert_eq!(failed.reason.as_deref(), Some("connector timed out"));
    }

    #[test]
    fn expire_stale_only_touches_expired_proposals() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let stale = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .unwrap();
        let fresh = store.propose(input()).unwrap();

        let later = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(store.expire_stale(later).unwrap(), 1);
        assert_eq!(
            store.get(stale.id).unwrap().unwrap().status,
            ActionStatus::Expired
        );
        assert_eq!(
            store.get(fresh.id).unwrap().unwrap().status,
            ActionStatus::Proposed
        );
    }

    #[test]
    fn claiming_for_execution_succeeds_once_and_then_refuses() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        store.approve(a.id).unwrap();

        assert!(store.claim_for_execution(a.id).unwrap());
        let claimed = store.get(a.id).unwrap().unwrap();
        assert_eq!(
            claimed.status,
            ActionStatus::Approved,
            "the claim must not change the status -- no new variant, no renderer churn"
        );
        assert!(
            claimed.executed_at.is_some(),
            "the claim is the stamped executed_at; that is what survives a crash"
        );

        assert!(
            !store.claim_for_execution(a.id).unwrap(),
            "an approved row with a non-null executed_at is a half-state, not a fresh action"
        );
    }

    #[test]
    fn an_unapproved_action_cannot_be_claimed_for_execution() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        assert!(!store.claim_for_execution(a.id).unwrap());
        assert!(store.get(a.id).unwrap().unwrap().executed_at.is_none());
    }

    #[test]
    fn a_claimed_action_still_completes_normally() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store.propose(input()).unwrap();
        store.approve(a.id).unwrap();
        assert!(store.claim_for_execution(a.id).unwrap());

        // mark_executed matches on status alone, so the claim does not block it.
        let done = store.mark_executed(a.id, "ok").unwrap();
        assert_eq!(done.status, ActionStatus::Executed);
        assert!(done.executed_at.is_some());
    }

    #[test]
    fn claiming_and_expiring_do_not_see_each_other() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .unwrap();
        store.approve(a.id).unwrap();
        assert!(store.claim_for_execution(a.id).unwrap());

        // expire_stale only touches `proposed` rows, so a claimed approval is
        // untouched and pending() never showed it in the first place.
        let later = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(store.expire_stale(later).unwrap(), 0);
        assert_eq!(
            store.get(a.id).unwrap().unwrap().status,
            ActionStatus::Approved
        );
        assert!(store.pending().unwrap().is_empty());
    }

    /// Two threads, one approved action, one conditional UPDATE. Exactly one
    /// of them is allowed to reach a connector.
    #[test]
    fn two_concurrent_claims_and_exactly_one_wins() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn.clone());
        let a = store.propose(input()).unwrap();
        store.approve(a.id).unwrap();

        let winners = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = ActionStore::new(conn.clone());
            let winners = Arc::clone(&winners);
            let barrier = Arc::clone(&barrier);
            let id = a.id;
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                if store.claim_for_execution(id).unwrap() {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one claimant may own the execution"
        );
    }

    #[test]
    fn an_expired_proposal_cannot_be_approved() {
        let (_dir, conn) = temp_store();
        let store = ActionStore::new(conn);
        let a = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .unwrap();
        store
            .expire_stale(chrono::Utc::now() + chrono::Duration::seconds(5))
            .unwrap();
        assert!(store.approve(a.id).is_err());
    }
}
