use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::timestamp::{serialize_rfc3339, serialize_rfc3339_opt};
use sqlx::{types::Json, FromRow, PgPool};

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub kind: String,
    pub prompt: String,
    pub outcome: String,
    pub detail: Option<String>,
    pub action_ids: Vec<i64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    // Serialized explicitly as RFC 3339 (rather than left to chrono's default
    // `Serialize`, which renders a UTC offset as `Z`) so that `ea log` and the
    // daemon's `log` IPC response keep the exact timestamp format
    // `to_rfc3339()` produces.
    #[serde(serialize_with = "serialize_rfc3339")]
    pub started_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// The row shape as it comes back from Postgres. `action_ids` decodes as
/// `JSONB` here and is unwrapped into the plain `Vec<i64>` [`Run`] holds --
/// callers should never have to think about the `Json` wrapper.
#[derive(FromRow)]
struct RunRow {
    id: i64,
    kind: String,
    prompt: String,
    outcome: String,
    detail: Option<String>,
    action_ids: Json<Vec<i64>>,
    cost_usd: Option<f64>,
    duration_ms: Option<i64>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<RunRow> for Run {
    fn from(row: RunRow) -> Self {
        Run {
            id: row.id,
            kind: row.kind,
            prompt: row.prompt,
            outcome: row.outcome,
            detail: row.detail,
            action_ids: row.action_ids.0,
            cost_usd: row.cost_usd,
            duration_ms: row.duration_ms,
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}

const RUN_COLUMNS: &str =
    "id, kind, prompt, outcome, detail, action_ids, cost_usd, duration_ms, started_at, finished_at";

#[derive(Clone)]
pub struct RunStore {
    pool: PgPool,
}

impl RunStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Starts a run, recording it as `running`. Returns the new run's id, so
    /// the caller can pass it back to [`RunStore::finish`] once the agent
    /// session completes.
    ///
    /// `action_ids` is written **at insert**, not only at
    /// [`RunStore::finish`]. A run that is abandoned -- the process killed
    /// mid-connector-call, the future dropped -- never reaches `finish`, and a
    /// `running` row whose prompt is `"fortnox.record_voucher"` with no id
    /// cannot be tied back to the action it was spending money on. Recording
    /// the ids up front is what makes an orphaned `running` row a usable lead
    /// instead of a shrug. Pass `&[]` for runs that are not about a specific
    /// action (a scheduled agent session, say); `finish` overwrites the column
    /// with whatever the run actually touched.
    pub async fn start(&self, kind: &str, prompt: &str, action_ids: &[i64]) -> anyhow::Result<i64> {
        sqlx::query_scalar(
            "INSERT INTO runs (kind, prompt, outcome, action_ids)
             VALUES ($1, $2, 'running', $3) RETURNING id",
        )
        .bind(kind)
        .bind(prompt)
        .bind(Json(action_ids))
        .fetch_one(&self.pool)
        .await
        .context("starting a run")
    }

    pub async fn get(&self, id: i64) -> anyhow::Result<Option<Run>> {
        let row = sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("reading a run")?;
        Ok(row.map(Run::from))
    }

    /// Closes out a run: records the outcome, freeform detail, the action ids
    /// it touched, its cost, and a `duration_ms` computed from `started_at`.
    ///
    /// One statement: the update and the duration computation both happen in
    /// Postgres (`now() - started_at`, clamped at zero) and the closed row
    /// comes back via `RETURNING`, rather than reading `started_at` back into
    /// Rust and writing a second statement.
    pub async fn finish(
        &self,
        id: i64,
        outcome: &str,
        detail: Option<&str>,
        action_ids: &[i64],
        cost_usd: Option<f64>,
    ) -> anyhow::Result<Run> {
        let row = sqlx::query_as::<_, RunRow>(&format!(
            "UPDATE runs SET outcome = $2, detail = $3, action_ids = $4, cost_usd = $5,
                    duration_ms = GREATEST(0, EXTRACT(EPOCH FROM (now() - started_at)) * 1000)::bigint,
                    finished_at = now()
             WHERE id = $1
             RETURNING {RUN_COLUMNS}"
        ))
        .bind(id)
        .bind(outcome)
        .bind(detail)
        .bind(Json(action_ids))
        .bind(cost_usd)
        .fetch_optional(&self.pool)
        .await
        .context("finishing a run")?;
        row.map(Run::from)
            .ok_or_else(|| anyhow!("run {id} does not exist"))
    }

    /// The `limit` most recent runs, newest first. What `ea log` shows.
    pub async fn recent(&self, limit: i64) -> anyhow::Result<Vec<Run>> {
        let rows = sqlx::query_as::<_, RunRow>(&format!(
            "SELECT {RUN_COLUMNS} FROM runs ORDER BY id DESC LIMIT $1"
        ))
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .context("listing recent runs")?;
        Ok(rows.into_iter().map(Run::from).collect())
    }

    /// How many runs started at or after `since`, optionally ignoring one
    /// `kind`.
    ///
    /// This is what the daily session budget is counted from, and the
    /// exclusion is why it takes a kind at all: the executor writes a `runs`
    /// row for every connector call, and those are not sessions and cost no
    /// model tokens. Counting them would exhaust the budget on a day the
    /// daemon merely approved a lot of actions.
    pub async fn count_since(
        &self,
        since: DateTime<Utc>,
        exclude_kind: Option<&str>,
    ) -> anyhow::Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs
             WHERE started_at >= $1 AND ($2::text IS NULL OR kind <> $2)",
        )
        .bind(since)
        .bind(exclude_kind)
        .fetch_one(&self.pool)
        .await
        .context("counting runs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn start_records_a_running_run(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store.start("scheduled", "check canvas", &[]).await.unwrap();
        let run = store.get(id).await.unwrap().unwrap();
        assert_eq!(run.kind, "scheduled");
        assert_eq!(run.prompt, "check canvas");
        assert_eq!(run.outcome, "running");
        assert!(run.finished_at.is_none());
        assert!(run.duration_ms.is_none());
        assert!(run.action_ids.is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn start_records_the_action_ids_before_finish_runs(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store
            .start("execute", "fortnox.record_voucher", &[7])
            .await
            .unwrap();

        // Nothing has finished; this is the state a crash mid-call leaves.
        let run = store.get(id).await.unwrap().unwrap();
        assert_eq!(run.outcome, "running");
        assert!(run.finished_at.is_none());
        assert_eq!(
            run.action_ids,
            vec![7],
            "an orphaned running row must still name the action it was executing"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finish_records_outcome_detail_actions_and_cost(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store.start("scheduled", "check canvas", &[]).await.unwrap();
        let run = store
            .finish(
                id,
                "ok",
                Some("found 2 new assignments"),
                &[1, 2],
                Some(0.014),
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, "ok");
        assert_eq!(run.detail.as_deref(), Some("found 2 new assignments"));
        assert_eq!(run.action_ids, vec![1, 2]);
        assert_eq!(run.cost_usd, Some(0.014));
        assert!(run.finished_at.is_some());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finish_computes_a_nonnegative_duration(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store.start("scheduled", "check canvas", &[]).await.unwrap();
        let run = store.finish(id, "ok", None, &[], None).await.unwrap();
        assert!(run.duration_ms.unwrap() >= 0);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finishing_an_unknown_run_fails(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        assert!(store.finish(999, "ok", None, &[], None).await.is_err());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn recent_returns_the_newest_runs_first(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        for i in 0..5 {
            store
                .start("session", &format!("run {i}"), &[])
                .await
                .unwrap();
        }
        let recent = store.recent(3).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].prompt, "run 4");
        assert_eq!(recent[2].prompt, "run 2");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn count_since_ignores_the_excluded_kind_and_older_rows(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        store.start("triage.tier1", "a", &[]).await.unwrap();
        store.start("execute", "canvas.list_courses", &[]).await.unwrap();
        store.start("chat", "b", &[]).await.unwrap();

        let midnight = Utc::now() - chrono::Duration::hours(1);
        assert_eq!(store.count_since(midnight, None).await.unwrap(), 3);
        assert_eq!(
            store.count_since(midnight, Some("execute")).await.unwrap(),
            2
        );

        let tomorrow = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(store.count_since(tomorrow, None).await.unwrap(), 0);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn action_ids_round_trip_through_jsonb(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store.start("triage", "prompt", &[7, 9]).await.unwrap();
        let run = store.get(id).await.unwrap().unwrap();
        assert_eq!(run.action_ids, vec![7, 9]);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_run_with_no_actions_reads_back_as_an_empty_list(pool: sqlx::PgPool) {
        let store = RunStore::new(pool);
        let id = store.start("triage", "prompt", &[]).await.unwrap();
        assert!(store.get(id).await.unwrap().unwrap().action_ids.is_empty());
    }

    /// `count_since` is what `daily_session_budget` is enforced from, counted
    /// over the owner's own day. A run started exactly at the boundary counts.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn count_since_includes_a_run_at_the_boundary(pool: sqlx::PgPool) {
        let store = RunStore::new(pool.clone());
        let id = store.start("triage", "p", &[]).await.unwrap();
        let started: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT started_at FROM runs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(store.count_since(started, None).await.unwrap() >= 1);
    }
}
