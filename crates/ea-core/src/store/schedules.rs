//! The `schedules` table: what the daemon runs on a cron, and when it last ran.
//!
//! The table has existed since Phase 1 and nothing read or wrote it: Phase 1
//! shipped the scheduler with fixed intervals only. This is the first reader.
//!
//! # `last_run_at` is the whole point
//!
//! An interval job needs no durable state — it has run "recently enough" or it
//! has not, and a restart losing that costs one extra poll. A cron job is
//! different in both directions. Without a durable `last_run_at`, a daemon
//! that restarts at 07:05 either re-fires the 07:00 briefing (because nothing
//! remembers that it already went out) or skips it forever (because the
//! in-memory anchor was reset to start-up). Both are wrong, and the owner
//! notices: one sends the same briefing twice, the other sends nothing and
//! looks exactly like a system that had nothing to say.
//!
//! # A new row is anchored to *now*, not to the epoch
//!
//! [`ScheduleStore::ensure`] writes `last_run_at = now` when it inserts. A
//! `NULL` anchor would mean "every past occurrence is missed", and the first
//! tick after a fresh install at 15:00 would fire the morning briefing, the
//! bookkeeping pass and the VAT prep at once — a new user's first experience
//! of the system being three reports they did not ask for. Anchoring to the
//! install instant means the first run is the first genuine occurrence.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One row of the `schedules` table.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ScheduleRow {
    pub name: String,
    /// The cron expression, as written. Parsing is the caller's business: this
    /// store does not know what a cron expression means and must not refuse to
    /// hand back a row it cannot parse, or a bad expression would be
    /// unreadable and therefore unfixable.
    pub cron: String,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct ScheduleStore {
    pool: PgPool,
}

impl ScheduleStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register a built-in schedule, and return the row as it now stands.
    ///
    /// Idempotent, and called on every start-up. On first insert `last_run_at`
    /// is anchored to `now` — see the module docs. On a later start-up the
    /// stored `last_run_at` is left exactly as it is, and only the `cron`
    /// column is brought up to date, so that changing a built-in's expression
    /// in the source takes effect without a migration and without re-firing
    /// the job.
    ///
    /// One statement, via `RETURNING`, rather than an insert followed by a
    /// separate read: there is no read-then-write here to race.
    pub async fn ensure(
        &self,
        name: &str,
        cron: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ScheduleRow> {
        sqlx::query_as::<_, ScheduleRow>(
            "INSERT INTO schedules (name, cron, enabled, last_run_at)
             VALUES ($1, $2, TRUE, $3)
             ON CONFLICT (name) DO UPDATE SET cron = EXCLUDED.cron
             RETURNING name, cron, enabled, last_run_at",
        )
        .bind(name)
        .bind(cron)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("registering schedule {name}"))
    }

    pub async fn get(&self, name: &str) -> anyhow::Result<Option<ScheduleRow>> {
        sqlx::query_as::<_, ScheduleRow>(
            "SELECT name, cron, enabled, last_run_at FROM schedules WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("reading schedule {name}"))
    }

    /// Every schedule, by name.
    pub async fn all(&self) -> anyhow::Result<Vec<ScheduleRow>> {
        sqlx::query_as::<_, ScheduleRow>(
            "SELECT name, cron, enabled, last_run_at FROM schedules ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading all schedules")
    }

    /// Stamp a schedule as having run at `at`.
    ///
    /// `at` is the instant the job fired, **not** the nominal cron time it
    /// fired for. That is what collapses a backlog: every occurrence between
    /// the old `last_run_at` and `at` is behind the new anchor, so a laptop
    /// shut over a long weekend produces one morning briefing on Monday rather
    /// than three. See `ea_daemon::schedules::Schedule::due`.
    pub async fn mark_run(&self, name: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE schedules SET last_run_at = $2 WHERE name = $1")
            .bind(name)
            .bind(at)
            .execute(&self.pool)
            .await
            .with_context(|| format!("stamping schedule {name}"))?;
        if result.rows_affected() == 0 {
            anyhow::bail!("no schedule named {name}");
        }
        Ok(())
    }

    /// Turn a schedule on or off. Returns `false` for an unknown name.
    ///
    /// Nothing calls this yet from the daemon's own code; it is the honest
    /// reader of the `enabled` column the schema has always had, and the hook
    /// an `ea` subcommand will use. A disabled schedule is skipped by the
    /// runner rather than deleted, so turning it back on does not lose
    /// `last_run_at`.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE schedules SET enabled = $2 WHERE name = $1")
            .bind(name)
            .bind(enabled)
            .execute(&self.pool)
            .await
            .with_context(|| format!("setting enabled for schedule {name}"))?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_new_schedule_is_anchored_to_now_rather_than_left_null(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        let now = utc("2026-09-24T13:00:00Z");

        let row = store
            .ensure("morning_briefing", "0 0 7 * * *", now)
            .await
            .unwrap();

        assert_eq!(row.name, "morning_briefing");
        assert_eq!(row.cron, "0 0 7 * * *");
        assert!(row.enabled);
        assert_eq!(
            row.last_run_at,
            Some(now),
            "a fresh install must not treat every past 07:00 as missed"
        );
    }

    /// `ensure` runs on every start-up. It must not keep re-anchoring, or the
    /// job would only ever fire when the daemon happened to stay up across its
    /// cron time from one start to the next.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn ensure_is_idempotent_and_never_moves_last_run_at(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        let installed = utc("2026-09-24T13:00:00Z");
        store
            .ensure("morning_briefing", "0 0 7 * * *", installed)
            .await
            .unwrap();
        store
            .mark_run("morning_briefing", utc("2026-09-25T05:00:00Z"))
            .await
            .unwrap();

        let again = store
            .ensure(
                "morning_briefing",
                "0 0 7 * * *",
                utc("2026-09-26T09:00:00Z"),
            )
            .await
            .unwrap();

        assert_eq!(again.last_run_at, Some(utc("2026-09-25T05:00:00Z")));
        assert_eq!(store.all().await.unwrap().len(), 1, "one row, not three");
    }

    /// Changing a built-in's expression in the source must take effect without
    /// a migration — and without re-firing the job.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn ensure_updates_the_expression_but_not_the_anchor(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        let installed = utc("2026-09-24T13:00:00Z");
        store
            .ensure("vat_prep", "0 0 9 1 * *", installed)
            .await
            .unwrap();

        let updated = store
            .ensure("vat_prep", "0 30 8 1 * *", utc("2026-10-01T09:00:00Z"))
            .await
            .unwrap();

        assert_eq!(updated.cron, "0 30 8 1 * *");
        assert_eq!(updated.last_run_at, Some(installed));
    }

    /// The brief's own version of the test above: a second `ensure` under a
    /// new `cron` must not create a second row, and must leave the store with
    /// the updated expression.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn ensure_updates_the_cron_of_an_existing_name(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        store
            .ensure("vat_prep", "0 9 1 * *", Utc::now())
            .await
            .unwrap();
        store
            .ensure("vat_prep", "0 10 1 * *", Utc::now())
            .await
            .unwrap();
        let rows = store.all().await.unwrap();
        assert_eq!(rows.len(), 1, "ensure must not create a second row");
        assert_eq!(rows[0].cron, "0 10 1 * *");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn mark_run_stamps_and_an_unknown_name_is_an_error(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        store
            .ensure(
                "bookkeeping_pass",
                "0 0 9 * * MON",
                utc("2026-09-24T13:00:00Z"),
            )
            .await
            .unwrap();

        let ran = utc("2026-09-28T07:00:00Z");
        store.mark_run("bookkeeping_pass", ran).await.unwrap();
        assert_eq!(
            store
                .get("bookkeeping_pass")
                .await
                .unwrap()
                .unwrap()
                .last_run_at,
            Some(ran)
        );

        assert!(store.mark_run("no_such_job", ran).await.is_err());
    }

    /// The brief's own version: `mark_run` stores the exact instant it is
    /// handed, to microsecond resolution.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn mark_run_stores_the_instant_it_was_given(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        store
            .ensure("morning_briefing", "0 7 * * *", Utc::now())
            .await
            .unwrap();
        let at = chrono::Utc::now();
        store.mark_run("morning_briefing", at).await.unwrap();
        let row = store.get("morning_briefing").await.unwrap().unwrap();
        crate::store::test_support::assert_same_instant(row.last_run_at.unwrap(), at);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_schedule_can_be_disabled_and_re_enabled_without_losing_its_anchor(
        pool: sqlx::PgPool,
    ) {
        let store = ScheduleStore::new(pool);
        let installed = utc("2026-09-24T13:00:00Z");
        store
            .ensure("vat_prep", "0 0 9 1 * *", installed)
            .await
            .unwrap();

        assert!(store.set_enabled("vat_prep", false).await.unwrap());
        let row = store.get("vat_prep").await.unwrap().unwrap();
        assert!(!row.enabled);
        assert_eq!(row.last_run_at, Some(installed));

        assert!(store.set_enabled("vat_prep", true).await.unwrap());
        assert!(store.get("vat_prep").await.unwrap().unwrap().enabled);
        assert!(!store.set_enabled("no_such_job", true).await.unwrap());
    }

    /// The brief's own version: `set_enabled` reports whether a row matched,
    /// independent of any anchor.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn set_enabled_reports_whether_a_row_matched(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        assert!(!store.set_enabled("absent", false).await.unwrap());
        store
            .ensure("vat_prep", "0 9 1 * *", Utc::now())
            .await
            .unwrap();
        assert!(store.set_enabled("vat_prep", false).await.unwrap());
        assert!(!store.get("vat_prep").await.unwrap().unwrap().enabled);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn get_of_an_unknown_name_is_none_not_an_error(pool: sqlx::PgPool) {
        let store = ScheduleStore::new(pool);
        assert_eq!(store.get("nothing").await.unwrap(), None);
        assert!(store.all().await.unwrap().is_empty());
    }
}
