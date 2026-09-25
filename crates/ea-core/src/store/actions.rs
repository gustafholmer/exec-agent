use std::str::FromStr;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Duration, Utc};
use serde::{Serialize, Serializer};
use sqlx::{postgres::PgRow, types::Json, PgPool, Row};

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
    // Serialized explicitly as RFC 3339 (rather than left to chrono's default
    // `Serialize`, which renders a UTC offset as `Z`) so that the daemon's IPC
    // responses, `ea pending`, and the `propose` tool keep the exact timestamp
    // format the old SQLite-backed store produced with `to_rfc3339()`.
    #[serde(serialize_with = "serialize_rfc3339")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub expires_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub decided_at: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub executed_at: Option<DateTime<Utc>>,
}

fn serialize_rfc3339<S: Serializer>(dt: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&dt.to_rfc3339())
}

fn serialize_rfc3339_opt<S: Serializer>(
    dt: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match dt {
        Some(dt) => serializer.serialize_str(&dt.to_rfc3339()),
        None => serializer.serialize_none(),
    }
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

#[derive(Clone)]
pub struct ActionStore {
    pool: PgPool,
}

/// Builds an [`Action`] from a `SELECT *` / `RETURNING *` row, reading every
/// column by name so the table's column order never matters.
///
/// An unrecognised `status` hydrates as `Failed`, as it always has: the CHECK
/// constraint makes that unreachable, and a row the code cannot classify must
/// never look like something still waiting to run.
fn hydrate(row: &PgRow) -> anyhow::Result<Action> {
    let args: Json<serde_json::Value> = row.try_get("args")?;
    let status: String = row.try_get("status")?;
    Ok(Action {
        id: row.try_get("id")?,
        connector: row.try_get("connector")?,
        tool: row.try_get("tool")?,
        args: args.0,
        preview: row.try_get("preview")?,
        rationale: row.try_get("rationale")?,
        status: status.parse().unwrap_or(ActionStatus::Failed),
        reason: row.try_get("reason")?,
        result: row.try_get("result")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        decided_at: row.try_get("decided_at")?,
        executed_at: row.try_get("executed_at")?,
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
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn propose(&self, input: ProposeInput) -> anyhow::Result<Action> {
        let now = Utc::now();
        // `DateTime + Duration` panics on overflow. The daemon validates
        // `ttl_secs` against a sane bound before it ever reaches here, but
        // this is the store's own defense: a checked add so a future or
        // different caller gets an error instead of taking the whole
        // daemon down.
        let expires = now
            .checked_add_signed(input.ttl)
            .ok_or_else(|| anyhow!("propose: ttl overflows when added to the current time"))?;
        // `created_at` is bound rather than left to the column default so that
        // `expires_at - created_at` is exactly the requested ttl.
        let row = sqlx::query(
            "INSERT INTO actions
               (connector, tool, args, preview, rationale, status, created_at, expires_at)
             VALUES ($1,$2,$3,$4,$5,'proposed',$6,$7)
             RETURNING *",
        )
        .bind(&input.connector)
        .bind(&input.tool)
        .bind(Json(&input.args))
        .bind(&input.preview)
        .bind(&input.rationale)
        .bind(now)
        .bind(expires)
        .fetch_one(&self.pool)
        .await
        .context("proposing an action")?;
        hydrate(&row)
    }

    pub async fn get(&self, id: i64) -> anyhow::Result<Option<Action>> {
        let row = sqlx::query("SELECT * FROM actions WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("reading an action")?;
        row.as_ref().map(hydrate).transpose()
    }

    pub async fn pending(&self) -> anyhow::Result<Vec<Action>> {
        let rows = sqlx::query("SELECT * FROM actions WHERE status = 'proposed' ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .context("listing pending actions")?;
        rows.iter().map(hydrate).collect()
    }

    /// One conditional `UPDATE ... RETURNING`, so two concurrent callers
    /// cannot both succeed, and the winner gets the updated row back from the
    /// same statement rather than a second read that could observe a later
    /// write. This is what makes a double-tapped Telegram button safe.
    ///
    /// `reason` and `result` are optional per-transition: when a caller
    /// doesn't supply one, `COALESCE($N, column)` preserves whatever the
    /// column already held instead of the `SET` clause being conditionally
    /// omitted, so the statement text is fixed per [`Stamp`] and every
    /// placeholder is always bound.
    ///
    /// Only when no row matched does this read the row back, purely to say
    /// *why* in the error; that read decides nothing.
    async fn transition(
        &self,
        id: i64,
        from: ActionStatus,
        to: ActionStatus,
        reason: Option<&str>,
        result: Option<&str>,
        stamp: Stamp,
    ) -> anyhow::Result<Action> {
        const DECIDED_SQL: &str = "UPDATE actions SET \
             status = $2, \
             reason = COALESCE($4, reason), \
             result = COALESCE($5, result), \
             decided_at = now() \
             WHERE id = $1 AND status = $3 RETURNING *";
        const EXECUTED_SQL: &str = "UPDATE actions SET \
             status = $2, \
             reason = COALESCE($4, reason), \
             result = COALESCE($5, result), \
             executed_at = now() \
             WHERE id = $1 AND status = $3 RETURNING *";
        let sql = match stamp {
            Stamp::Decided => DECIDED_SQL,
            Stamp::Executed => EXECUTED_SQL,
        };

        let updated = sqlx::query(sql)
            .bind(id)
            .bind(to.as_str())
            .bind(from.as_str())
            .bind(reason)
            .bind(result)
            .fetch_optional(&self.pool)
            .await
            .context("transitioning an action")?;

        match updated {
            Some(row) => hydrate(&row),
            // No row matched: either it does not exist, or it is in another
            // status. Both get the same helpful error they always have.
            None => match self.get(id).await? {
                None => Err(anyhow!("action {id} does not exist")),
                Some(current) => Err(anyhow!(
                    "action {id} is {}, not {}; cannot move to {}",
                    current.status.as_str(),
                    from.as_str(),
                    to.as_str()
                )),
            },
        }
    }

    pub async fn approve(&self, id: i64) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Proposed,
            ActionStatus::Approved,
            None,
            None,
            Stamp::Decided,
        )
        .await
    }

    pub async fn reject(&self, id: i64, reason: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Proposed,
            ActionStatus::Rejected,
            Some(reason),
            None,
            Stamp::Decided,
        )
        .await
    }

    pub async fn mark_executed(&self, id: i64, result: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Approved,
            ActionStatus::Executed,
            None,
            Some(result),
            Stamp::Executed,
        )
        .await
    }

    pub async fn mark_failed(&self, id: i64, reason: &str) -> anyhow::Result<Action> {
        self.transition(
            id,
            ActionStatus::Approved,
            ActionStatus::Failed,
            Some(reason),
            None,
            Stamp::Executed,
        )
        .await
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
    /// No new status variant and no schema change: `executed_at` is a plain
    /// nullable `TIMESTAMPTZ` column, and `mark_executed`/`mark_failed` stamp
    /// it anyway, so a claimed action that completes normally is indistinguishable from one
    /// that was never claimed. `pending()` and `expire_stale()` only look at
    /// `proposed` rows and are unaffected.
    pub async fn claim_for_execution(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE actions SET executed_at = now()
             WHERE id = $1 AND status = 'approved' AND executed_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .context("claiming an action for execution")?;
        Ok(result.rows_affected() == 1)
    }

    /// Every action left in the half-state [`claim_for_execution`] creates:
    /// `approved`, with `executed_at` already stamped.
    ///
    /// Reachable only by a crash — the process dying, or being killed, between
    /// the claim and `mark_executed`/`mark_failed`. Such a row is invisible
    /// everywhere else: [`ActionStore::pending`] and [`ActionStore::expire_stale`]
    /// look only at `proposed`, `ea status` counts only pending, and every
    /// retention DELETE lists terminal statuses that `approved` is not one of.
    /// The owner approved something and it silently never happened.
    ///
    /// Oldest first, so a report of them reads chronologically.
    ///
    /// [`claim_for_execution`]: ActionStore::claim_for_execution
    pub async fn stranded(&self) -> anyhow::Result<Vec<Action>> {
        let rows = sqlx::query(
            "SELECT * FROM actions
             WHERE status = 'approved' AND executed_at IS NOT NULL
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("listing stranded actions")?;
        rows.iter().map(hydrate).collect()
    }

    /// Resolve one stranded row as `failed`, with `reason`, **keeping the
    /// `executed_at` stamp that recorded when it was claimed**.
    ///
    /// Deliberately not `mark_failed`: that one stamps `executed_at = now`,
    /// which would overwrite the only record of when the lost attempt actually
    /// started. The `executed_at IS NOT NULL` conjunct is what keeps this
    /// method from being a way to fail an action that is legitimately mid-
    /// flight in a live executor — that row has no stamp yet.
    ///
    /// Returns `false` when the row was not (or is no longer) stranded.
    pub async fn fail_stranded(&self, id: i64, reason: &str) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE actions SET status = 'failed', reason = $2
             WHERE id = $1 AND status = 'approved' AND executed_at IS NOT NULL",
        )
        .bind(id)
        .bind(reason)
        .execute(&self.pool)
        .await
        .context("resolving a stranded action")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn expire_stale(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let result = sqlx::query(
            "UPDATE actions SET status = 'expired', reason = 'proposal expired'
             WHERE status = 'proposed' AND expires_at <= $1",
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .context("expiring stale proposals")?;
        Ok(result.rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn stores_a_proposal_and_round_trips_args(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        assert_eq!(a.status, ActionStatus::Proposed);
        assert_eq!(a.args["term"], "HT26");
        assert_eq!(
            store.get(a.id).await.unwrap().unwrap().preview,
            "List courses for HT26"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn pending_lists_only_proposals(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.propose(input()).await.unwrap();
        store.reject(a.id, "not now").await.unwrap();
        let pending = store.pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, ActionStatus::Proposed);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn approving_twice_fails_the_second_time(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        assert_eq!(
            store.approve(a.id).await.unwrap().status,
            ActionStatus::Approved
        );
        assert!(store.approve(a.id).await.is_err());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_rejected_action_cannot_be_approved(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.reject(a.id, "no").await.unwrap();
        assert!(store.approve(a.id).await.is_err());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn execution_requires_approval_first(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        assert!(store.mark_executed(a.id, "ok").await.is_err());
        store.approve(a.id).await.unwrap();
        let done = store.mark_executed(a.id, "ok").await.unwrap();
        assert_eq!(done.status, ActionStatus::Executed);
        assert_eq!(done.result.as_deref(), Some("ok"));
        assert!(done.executed_at.is_some());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn failure_records_the_reason(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        let failed = store
            .mark_failed(a.id, "connector timed out")
            .await
            .unwrap();
        assert_eq!(failed.status, ActionStatus::Failed);
        assert_eq!(failed.reason.as_deref(), Some("connector timed out"));
    }

    /// A crash between the claim and the outcome leaves exactly this row, and
    /// before `stranded()` nothing in the system could see it.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_claimed_but_unfinished_action_is_stranded(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        assert!(store.claim_for_execution(a.id).await.unwrap());
        // ... and now the process dies.

        assert!(
            store.pending().await.unwrap().is_empty(),
            "a stranded row is invisible to pending()"
        );
        assert_eq!(
            store
                .expire_stale(Utc::now() + Duration::days(365))
                .await
                .unwrap(),
            0,
            "a stranded row is invisible to expire_stale"
        );

        let stranded = store.stranded().await.unwrap();
        assert_eq!(stranded.len(), 1);
        assert_eq!(stranded[0].id, a.id);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_approved_action_nobody_has_claimed_is_not_stranded(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        assert!(
            store.stranded().await.unwrap().is_empty(),
            "an approved-but-unclaimed action is waiting for the executor, not stranded"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finished_actions_are_not_stranded(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        for outcome in ["executed", "failed"] {
            let a = store.propose(input()).await.unwrap();
            store.approve(a.id).await.unwrap();
            assert!(store.claim_for_execution(a.id).await.unwrap());
            if outcome == "executed" {
                store.mark_executed(a.id, "ok").await.unwrap();
            } else {
                store.mark_failed(a.id, "nope").await.unwrap();
            }
        }
        assert!(store.stranded().await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn resolving_a_stranded_action_keeps_the_claim_stamp(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        store.claim_for_execution(a.id).await.unwrap();
        let claimed_at = store.get(a.id).await.unwrap().unwrap().executed_at.unwrap();

        assert!(store
            .fail_stranded(a.id, "the daemon crashed")
            .await
            .unwrap());

        let row = store.get(a.id).await.unwrap().unwrap();
        assert_eq!(row.status, ActionStatus::Failed);
        assert_eq!(row.reason.as_deref(), Some("the daemon crashed"));
        assert_eq!(
            row.executed_at,
            Some(claimed_at),
            "the claim stamp is the only record of when the lost attempt started"
        );
        assert!(store.stranded().await.unwrap().is_empty());
    }

    /// The conjunct that matters: an approved action with no claim stamp is
    /// one the executor may be about to run, and must not be failed from under
    /// it by a recovery sweep.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn fail_stranded_refuses_an_unclaimed_approved_action(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        assert!(!store.fail_stranded(a.id, "crash").await.unwrap());
        assert_eq!(
            store.get(a.id).await.unwrap().unwrap().status,
            ActionStatus::Approved
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn expire_stale_only_touches_expired_proposals(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let stale = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .await
            .unwrap();
        let fresh = store.propose(input()).await.unwrap();

        let later = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(store.expire_stale(later).await.unwrap(), 1);
        assert_eq!(
            store.get(stale.id).await.unwrap().unwrap().status,
            ActionStatus::Expired
        );
        assert_eq!(
            store.get(fresh.id).await.unwrap().unwrap().status,
            ActionStatus::Proposed
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn claiming_for_execution_succeeds_once_and_then_refuses(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();

        assert!(store.claim_for_execution(a.id).await.unwrap());
        let claimed = store.get(a.id).await.unwrap().unwrap();
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
            !store.claim_for_execution(a.id).await.unwrap(),
            "an approved row with a non-null executed_at is a half-state, not a fresh action"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_unapproved_action_cannot_be_claimed_for_execution(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        assert!(!store.claim_for_execution(a.id).await.unwrap());
        assert!(store
            .get(a.id)
            .await
            .unwrap()
            .unwrap()
            .executed_at
            .is_none());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_claimed_action_still_completes_normally(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        store.approve(a.id).await.unwrap();
        assert!(store.claim_for_execution(a.id).await.unwrap());

        // mark_executed matches on status alone, so the claim does not block it.
        let done = store.mark_executed(a.id, "ok").await.unwrap();
        assert_eq!(done.status, ActionStatus::Executed);
        assert!(done.executed_at.is_some());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn claiming_and_expiring_do_not_see_each_other(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .await
            .unwrap();
        store.approve(a.id).await.unwrap();
        assert!(store.claim_for_execution(a.id).await.unwrap());

        // expire_stale only touches `proposed` rows, so a claimed approval is
        // untouched and pending() never showed it in the first place.
        let later = chrono::Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(store.expire_stale(later).await.unwrap(), 0);
        assert_eq!(
            store.get(a.id).await.unwrap().unwrap().status,
            ActionStatus::Approved
        );
        assert!(store.pending().await.unwrap().is_empty());
    }

    /// The claim is the difference between a recovery pass and a second
    /// voucher posted to real company accounting. It must succeed exactly
    /// once, including when several callers race: eight tasks, each on its
    /// own pooled connection, one approved action, one conditional UPDATE.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn only_one_concurrent_caller_claims_an_action(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool.clone());
        let action = store.propose(input()).await.unwrap();
        store.approve(action.id).await.unwrap();

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let store = store.clone();
            set.spawn(async move { store.claim_for_execution(action.id).await.unwrap() });
        }
        let mut claims = 0;
        while let Some(won) = set.join_next().await {
            if won.unwrap() {
                claims += 1;
            }
        }
        assert_eq!(claims, 1, "exactly one caller may own the execution");
    }

    /// Review Focus #4: the CHECK constraint must accept everything
    /// `ActionStatus::as_str` can produce, or an ordinary transition becomes a
    /// database error.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn every_action_status_is_accepted_by_the_database(pool: sqlx::PgPool) {
        for status in [
            ActionStatus::Proposed,
            ActionStatus::Approved,
            ActionStatus::Rejected,
            ActionStatus::Expired,
            ActionStatus::Executed,
            ActionStatus::Failed,
        ] {
            sqlx::query(
                "INSERT INTO actions (connector, tool, args, preview, rationale, status, expires_at)
                 VALUES ('c','t','{}','p','r',$1, now())",
            )
            .bind(status.as_str())
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("status {:?} was refused: {e}", status));
        }
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_transition_from_the_wrong_status_names_the_current_one(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let action = store.propose(input()).await.unwrap();
        store.approve(action.id).await.unwrap();
        let err = format!("{:#}", store.approve(action.id).await.unwrap_err());
        assert!(
            err.contains("approved"),
            "the error must name the current status: {err}"
        );
        assert_eq!(
            err,
            format!(
                "action {} is approved, not proposed; cannot move to approved",
                action.id
            ),
            "the message text is unchanged from the SQLite store"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn transitioning_a_missing_action_says_it_does_not_exist(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let err = format!("{:#}", store.approve(987_654).await.unwrap_err());
        assert_eq!(err, "action 987654 does not exist");
    }

    /// The row a transition returns is the row the UPDATE wrote, stamp
    /// included, and the untouched optional column survives the COALESCE.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_transition_returns_the_row_it_wrote(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        assert!(a.decided_at.is_none());
        assert_eq!(a.expires_at - a.created_at, chrono::Duration::hours(24));
        let rejected = store.reject(a.id, "not now").await.unwrap();
        assert_eq!(rejected.status, ActionStatus::Rejected);
        assert_eq!(rejected.reason.as_deref(), Some("not now"));
        assert!(rejected.result.is_none());
        assert!(rejected.decided_at.is_some());
        let reread = store.get(a.id).await.unwrap().unwrap();
        assert_eq!(reread.decided_at, rejected.decided_at);
    }

    /// `Action` serializes its timestamps with `to_rfc3339()`, exactly the
    /// text the SQLite store kept in its TEXT columns, so `ea pending` and the
    /// IPC responses are unchanged.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn timestamps_serialize_as_rfc3339(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store.propose(input()).await.unwrap();
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["expires_at"], a.expires_at.to_rfc3339());
        assert_eq!(json["created_at"], a.created_at.to_rfc3339());
        assert!(json["expires_at"].as_str().unwrap().ends_with("+00:00"));
        assert!(json["decided_at"].is_null());
        assert!(json["executed_at"].is_null());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_expired_proposal_cannot_be_approved(pool: sqlx::PgPool) {
        let store = ActionStore::new(pool);
        let a = store
            .propose(ProposeInput {
                ttl: chrono::Duration::seconds(1),
                ..input()
            })
            .await
            .unwrap();
        store
            .expire_stale(chrono::Utc::now() + chrono::Duration::seconds(5))
            .await
            .unwrap();
        assert!(store.approve(a.id).await.is_err());
    }
}
