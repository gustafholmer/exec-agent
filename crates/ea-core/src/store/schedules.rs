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

use std::sync::{Arc, Mutex};

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};

/// One row of the `schedules` table.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    conn: Arc<Mutex<Connection>>,
}

fn hydrate(row: &Row<'_>) -> rusqlite::Result<ScheduleRow> {
    let enabled: i64 = row.get("enabled")?;
    let last_run_at: Option<String> = row.get("last_run_at")?;
    Ok(ScheduleRow {
        name: row.get("name")?,
        cron: row.get("cron")?,
        enabled: enabled != 0,
        // An unparseable timestamp reads as "never run" rather than taking the
        // daemon down. The cost is one re-fired briefing; the alternative is a
        // startup that fails on a hand-edited row.
        last_run_at: last_run_at.and_then(|text| {
            text.parse::<DateTime<Utc>>()
                .map_err(|err| {
                    tracing::warn!(value = %text, error = %err, "unparseable schedules.last_run_at");
                })
                .ok()
        }),
    })
}

impl ScheduleStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Register a built-in schedule, and return the row as it now stands.
    ///
    /// Idempotent, and called on every start-up. On first insert `last_run_at`
    /// is anchored to `now` — see the module docs. On a later start-up the
    /// stored `last_run_at` is left exactly as it is, and only the `cron`
    /// column is brought up to date, so that changing a built-in's expression
    /// in the source takes effect without a migration and without re-firing
    /// the job.
    pub fn ensure(
        &self,
        name: &str,
        cron: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<ScheduleRow> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO schedules (name, cron, enabled, last_run_at)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT (name) DO UPDATE SET cron = excluded.cron",
                params![name, cron, now.to_rfc3339()],
            )
            .with_context(|| format!("registering schedule {name}"))?;
        }
        self.get(name)?
            .ok_or_else(|| anyhow::anyhow!("schedule {name} vanished immediately after insert"))
    }

    pub fn get(&self, name: &str) -> anyhow::Result<Option<ScheduleRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM schedules WHERE name = ?1")?;
        let mut rows = stmt.query_map(params![name], hydrate)?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    /// Every schedule, by name.
    pub fn all(&self) -> anyhow::Result<Vec<ScheduleRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM schedules ORDER BY name")?;
        let rows = stmt.query_map([], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stamp a schedule as having run at `at`.
    ///
    /// `at` is the instant the job fired, **not** the nominal cron time it
    /// fired for. That is what collapses a backlog: every occurrence between
    /// the old `last_run_at` and `at` is behind the new anchor, so a laptop
    /// shut over a long weekend produces one morning briefing on Monday rather
    /// than three. See `ea_daemon::schedules::Schedule::due`.
    pub fn mark_run(&self, name: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE schedules SET last_run_at = ?1 WHERE name = ?2",
            params![at.to_rfc3339(), name],
        )?;
        if changed == 0 {
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
    pub fn set_enabled(&self, name: &str, enabled: bool) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE schedules SET enabled = ?1 WHERE name = ?2",
            params![i64::from(enabled), name],
        )?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn a_new_schedule_is_anchored_to_now_rather_than_left_null() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let now = utc("2026-09-24T13:00:00Z");

        let row = store
            .ensure("morning_briefing", "0 0 7 * * *", now)
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
    #[test]
    fn ensure_is_idempotent_and_never_moves_last_run_at() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T13:00:00Z");
        store
            .ensure("morning_briefing", "0 0 7 * * *", installed)
            .unwrap();
        store
            .mark_run("morning_briefing", utc("2026-09-25T05:00:00Z"))
            .unwrap();

        let again = store
            .ensure(
                "morning_briefing",
                "0 0 7 * * *",
                utc("2026-09-26T09:00:00Z"),
            )
            .unwrap();

        assert_eq!(again.last_run_at, Some(utc("2026-09-25T05:00:00Z")));
        assert_eq!(store.all().unwrap().len(), 1, "one row, not three");
    }

    /// Changing a built-in's expression in the source must take effect without
    /// a migration — and without re-firing the job.
    #[test]
    fn ensure_updates_the_expression_but_not_the_anchor() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T13:00:00Z");
        store.ensure("vat_prep", "0 0 9 1 * *", installed).unwrap();

        let updated = store
            .ensure("vat_prep", "0 30 8 1 * *", utc("2026-10-01T09:00:00Z"))
            .unwrap();

        assert_eq!(updated.cron, "0 30 8 1 * *");
        assert_eq!(updated.last_run_at, Some(installed));
    }

    #[test]
    fn mark_run_stamps_and_an_unknown_name_is_an_error() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        store
            .ensure(
                "bookkeeping_pass",
                "0 0 9 * * MON",
                utc("2026-09-24T13:00:00Z"),
            )
            .unwrap();

        let ran = utc("2026-09-28T07:00:00Z");
        store.mark_run("bookkeeping_pass", ran).unwrap();
        assert_eq!(
            store.get("bookkeeping_pass").unwrap().unwrap().last_run_at,
            Some(ran)
        );

        assert!(store.mark_run("no_such_job", ran).is_err());
    }

    #[test]
    fn a_schedule_can_be_disabled_and_re_enabled_without_losing_its_anchor() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T13:00:00Z");
        store.ensure("vat_prep", "0 0 9 1 * *", installed).unwrap();

        assert!(store.set_enabled("vat_prep", false).unwrap());
        let row = store.get("vat_prep").unwrap().unwrap();
        assert!(!row.enabled);
        assert_eq!(row.last_run_at, Some(installed));

        assert!(store.set_enabled("vat_prep", true).unwrap());
        assert!(store.get("vat_prep").unwrap().unwrap().enabled);
        assert!(!store.set_enabled("no_such_job", true).unwrap());
    }

    /// A hand-edited or corrupted timestamp must read as "never run", not take
    /// the daemon down at start-up.
    #[test]
    fn an_unparseable_anchor_reads_as_never_run() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(Arc::clone(&conn));
        store
            .ensure("vat_prep", "0 0 9 1 * *", utc("2026-09-24T13:00:00Z"))
            .unwrap();
        conn.lock()
            .unwrap()
            .execute(
                "UPDATE schedules SET last_run_at = 'last tuesday' WHERE name = 'vat_prep'",
                [],
            )
            .unwrap();

        assert_eq!(store.get("vat_prep").unwrap().unwrap().last_run_at, None);
    }

    #[test]
    fn get_of_an_unknown_name_is_none_not_an_error() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        assert_eq!(store.get("nothing").unwrap(), None);
        assert!(store.all().unwrap().is_empty());
    }
}
