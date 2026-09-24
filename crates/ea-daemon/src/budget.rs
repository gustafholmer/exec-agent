//! The daily session budget: how much model spend the daemon is allowed to
//! start on its own, and what happens when it runs out.
//!
//! # Why this is derived and not counted
//!
//! There is no counter in this struct. [`Budget::spent_today`] runs a `SELECT
//! COUNT(*)` over the `runs` table every time it is asked, and that is the
//! whole design:
//!
//! * The daemon runs under `launchd` with `KeepAlive`. "The process came back"
//!   is a routine event, not an exotic one, and an in-memory counter would
//!   reset with it — so a crash loop would buy an unlimited number of free
//!   sessions, which is exactly the situation a spend ceiling exists for.
//! * A `runs` row is opened by [`SessionRunner::run`](crate::session::SessionRunner::run)
//!   *before the child is spawned*, on every path, and closed on every path
//!   out. So a session that timed out, failed, or returned nonsense is counted
//!   the same as one that worked. Money was spent either way.
//! * There is therefore nothing to reset at midnight. The brief sketched a
//!   `reset_if_new_day(now)`; it does not exist here, because the day boundary
//!   is recomputed from `now` on every read ([`Budget::day_start`]) and a
//!   derived count cannot drift out of step with it. A method that exists only
//!   to be called at the right moment is a method that will eventually not be.
//!
//! # The day starts at local midnight, not UTC midnight
//!
//! The owner is in `Europe/Stockholm`, which is one or two hours ahead of UTC
//! depending on the season. A budget counted from UTC midnight resets at 01:00
//! or 02:00 local — in the middle of the owner's night in winter, and at a
//! different wall-clock time in summer, which is the tell that the zone is
//! being handled by arithmetic rather than by a zone database. The boundary
//! goes through [`crate::schedules::local_instant`], the same DST-correct
//! helper the cron schedules use; there is exactly one of those in this
//! daemon and this is not a fourth.
//!
//! # What the budget covers, and what it does to each caller
//!
//! Every `runs` row that is not the executor's own counts — the executor
//! writes one per connector call, and those spend no model tokens (see
//! [`RunStore::count_since`]). There are three places in the daemon that start
//! a session, and the budget does something different to each, on purpose:
//!
//! * **Triage** ([`crate::jobs::run_triage`]) degrades to tier 0. It keeps
//!   running, for free, on the mute list and the keyword rescue; only the
//!   Haiku scoring pass stops. Nothing is lost, it just stops being ranked.
//! * **Briefings** ([`crate::briefings`]) are skipped and say so. A briefing
//!   is a recurring message; the next one is tomorrow.
//! * **Chat** ([`crate::chat::ChatService`]) is **not** refused. See
//!   [`Budget::is_spent`] and the note on `ChatService::say`: the owner's own
//!   typed message is the one thing in this system that must not be met with
//!   silence, and it is the only session kind with no cheaper tier and no
//!   next occurrence. Chat sessions still *count*, which is what makes a
//!   chatty day shut down the unattended work first.

use anyhow::Result;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use ea_core::store::runs::RunStore;

/// The daily ceiling on sessions the daemon starts, counted from the `runs`
/// table over the owner's own day.
///
/// Cheap to clone: a `RunStore` is an `Arc<Mutex<Connection>>` behind a
/// handle, and every dependency struct that needs a budget owns one rather
/// than sharing a reference.
#[derive(Clone)]
pub struct Budget {
    runs: RunStore,
    daily_sessions: u32,
    time_zone: Tz,
}

impl Budget {
    pub fn new(runs: RunStore, daily_sessions: u32, time_zone: Tz) -> Self {
        Self {
            runs,
            daily_sessions,
            time_zone,
        }
    }

    /// The ceiling itself, as `ea status` reports it.
    pub fn limit(&self) -> u32 {
        self.daily_sessions
    }

    pub fn time_zone(&self) -> Tz {
        self.time_zone
    }

    /// The instant the owner's current day began.
    ///
    /// Midnight where the owner is, resolved through
    /// [`crate::schedules::local_instant`] so that a midnight which is
    /// ambiguous (clocks back) or absent (clocks forward) is still an instant
    /// rather than a panic. If the zone database says something this code
    /// should not guess about, the fall-back is UTC midnight — a boundary in
    /// the wrong place for a few hours beats a budget that stops answering.
    pub fn day_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let local_midnight = now
            .with_timezone(&self.time_zone)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a time");
        crate::schedules::local_instant(self.time_zone, local_midnight).unwrap_or_else(|| {
            tracing::warn!(
                zone = self.time_zone.name(),
                "no instant for local midnight; counting the session budget from UTC midnight"
            );
            now.date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a time")
                .and_utc()
        })
    }

    /// How many sessions have been started since [`Budget::day_start`].
    ///
    /// Read from the database on every call. See the module docs for why
    /// there is no counter.
    pub fn spent_today(&self, now: DateTime<Utc>) -> Result<u32> {
        let spent = self
            .runs
            .count_since(self.day_start(now), Some(crate::executor::RUN_KIND))?;
        Ok(u32::try_from(spent.max(0)).unwrap_or(u32::MAX))
    }

    /// How many sessions are left today. Saturates at zero.
    pub fn remaining(&self, now: DateTime<Utc>) -> Result<u32> {
        Ok(self.daily_sessions.saturating_sub(self.spent_today(now)?))
    }

    /// Is the day's budget gone?
    ///
    /// A ceiling of zero is spent before anything runs, which is the
    /// documented way to turn every unattended session off without
    /// uninstalling the daemon.
    ///
    /// This is a *report*, not a gate. Read it when the answer is "and then do
    /// something cheaper or say why"; use [`Budget::try_consume`] when the
    /// answer is "and then do not run a session".
    pub fn is_spent(&self, now: DateTime<Utc>) -> Result<bool> {
        Ok(self.spent_today(now)? >= self.daily_sessions)
    }

    /// May a session start now?
    ///
    /// `false` means **do not run one** — every caller of this either takes a
    /// cheaper path or returns a note saying why it did nothing. A budget that
    /// logged and continued would be worse than no budget, because `ea status`
    /// would then report protection that does not exist.
    ///
    /// Named `try_consume` because that is the shape of the decision, but note
    /// what does the consuming: the `runs` row the session itself opens before
    /// it spawns. There is no counter here to increment, which is precisely
    /// why the count survives a restart. The consequence is that this is a
    /// ceiling rather than an atomic semaphore — two callers checking in the
    /// same instant could both be told yes and take the day one session over.
    /// That is acceptable here and would not be if the two were unbounded:
    /// triage runs one session per pass under the scheduler's overlap guard,
    /// the briefings are cron entries minutes apart, and chat holds a mutex
    /// for the whole of a turn.
    pub fn try_consume(&self, now: DateTime<Utc>) -> Result<bool> {
        Ok(!self.is_spent(now)?)
    }

    /// `"7/60"` — the pair `ea status` shows, and the reason it is one string:
    /// a count without its ceiling says nothing, and reading two fields and
    /// dividing them is work the owner should not be doing at a glance.
    pub fn status_line(&self, now: DateTime<Utc>) -> String {
        match self.spent_today(now) {
            Ok(spent) => format!("{spent}/{}", self.daily_sessions),
            // `status` is what someone reaches for when things are already
            // wrong; it must answer.
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "could not count today's sessions");
                format!("?/{}", self.daily_sessions)
            }
        }
    }

    /// The sentence a caller puts in front of a human when it did nothing.
    ///
    /// One wording, in one place, so that "why is it quiet?" has the same
    /// answer whichever surface asks — and so that the answer names the
    /// boundary the owner has to wait for rather than leaving them to guess
    /// whether it is UTC.
    pub fn spent_note(&self) -> String {
        format!(
            "the daily session budget of {} is spent; it resets at midnight in {}",
            self.daily_sessions,
            self.time_zone.name()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono_tz::Europe::Stockholm;
    use tempfile::TempDir;

    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    type Conn = Arc<std::sync::Mutex<rusqlite::Connection>>;

    struct Fixture {
        _dir: TempDir,
        path: std::path::PathBuf,
        conn: Conn,
        runs: RunStore,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        let conn = open(&path);
        Fixture {
            _dir: dir,
            path,
            runs: RunStore::new(Arc::clone(&conn)),
            conn,
        }
    }

    fn open(path: &std::path::Path) -> Conn {
        Arc::new(std::sync::Mutex::new(ea_core::db::open(path).unwrap()))
    }

    /// A `runs` row with a chosen `started_at`.
    ///
    /// `RunStore::start` stamps `Utc::now()`, and it should: back-dating is
    /// not something the daemon ever wants to do, so the store has no API for
    /// it and the test reaches the column directly. Every other property here
    /// — which kinds count, what survives a restart — goes through the real
    /// store.
    fn run_at(f: &Fixture, kind: &str, at: DateTime<Utc>) {
        let id = f.runs.start(kind, "prompt", &[]).unwrap();
        f.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE runs SET started_at = ?1 WHERE id = ?2",
                rusqlite::params![at.to_rfc3339(), id],
            )
            .unwrap();
    }

    #[test]
    fn consuming_below_the_limit_succeeds_and_the_call_at_the_limit_fails() {
        let f = fixture();
        let budget = Budget::new(f.runs.clone(), 3, Stockholm);
        let now = utc("2026-09-24T12:00:00Z");

        for expected in 0..3u32 {
            assert_eq!(budget.spent_today(now).unwrap(), expected);
            assert!(budget.try_consume(now).unwrap(), "session {expected}");
            run_at(&f, "triage.tier1", now);
        }

        assert_eq!(budget.spent_today(now).unwrap(), 3);
        assert!(
            !budget.try_consume(now).unwrap(),
            "the call at the limit must fail"
        );
        assert_eq!(budget.remaining(now).unwrap(), 0);
    }

    /// A ceiling of zero is the documented off switch.
    #[test]
    fn a_zero_budget_is_spent_before_anything_runs() {
        let f = fixture();
        assert!(!Budget::new(f.runs.clone(), 0, Stockholm)
            .try_consume(utc("2026-09-24T12:00:00Z"))
            .unwrap());
        assert!(Budget::new(f.runs, 1, Stockholm)
            .try_consume(utc("2026-09-24T12:00:00Z"))
            .unwrap());
    }

    /// The executor writes a `runs` row per connector call. Those are not
    /// sessions and cost no model tokens; counting them would exhaust the
    /// budget on a day the daemon merely approved a lot of actions.
    #[test]
    fn connector_calls_are_not_sessions() {
        let f = fixture();
        let budget = Budget::new(f.runs.clone(), 2, Stockholm);
        let now = utc("2026-09-24T12:00:00Z");
        for _ in 0..50 {
            run_at(&f, crate::executor::RUN_KIND, now);
        }
        assert_eq!(budget.spent_today(now).unwrap(), 0);
        assert!(budget.try_consume(now).unwrap());
    }

    /// **The boundary is the owner's midnight, not UTC's.**
    ///
    /// Stockholm is UTC+2 in September. A session at 23:30 local on the 24th
    /// is 21:30Z, and at 00:30 local on the 25th the budget must be fresh —
    /// while a UTC-midnight boundary would still be counting that session for
    /// another hour and a half.
    #[test]
    fn the_day_resets_at_local_midnight_and_not_at_utc_midnight() {
        let f = fixture();
        let budget = Budget::new(f.runs.clone(), 1, Stockholm);

        // 23:30 Stockholm on the 24th.
        let late = utc("2026-09-24T21:30:00Z");
        run_at(&f, "chat", late);
        assert!(!budget.try_consume(late).unwrap(), "spent for the 24th");

        // 00:30 Stockholm on the 25th — still 22:30Z on the 24th, so a
        // UTC-midnight boundary would say the budget is still spent.
        let just_after_local_midnight = utc("2026-09-24T22:30:00Z");
        assert_eq!(
            budget.day_start(just_after_local_midnight),
            utc("2026-09-24T22:00:00Z"),
            "the day begins at 00:00 Stockholm, which is 22:00Z the day before"
        );
        assert!(
            budget.try_consume(just_after_local_midnight).unwrap(),
            "a new local day is a new budget"
        );

        // And the same instant under a UTC budget is still spent, which is
        // the bug this test exists to keep out.
        assert!(!Budget::new(f.runs.clone(), 1, chrono_tz::UTC)
            .try_consume(just_after_local_midnight)
            .unwrap());
    }

    /// Winter and summer boundaries differ by an hour in UTC and by nothing at
    /// all on the owner's clock, which is the point of using a zone rather
    /// than an offset.
    #[test]
    fn the_boundary_follows_daylight_saving() {
        let f = fixture();
        let budget = Budget::new(f.runs, 60, Stockholm);
        // CEST, UTC+2.
        assert_eq!(
            budget.day_start(utc("2026-07-15T12:00:00Z")),
            utc("2026-07-14T22:00:00Z")
        );
        // CET, UTC+1.
        assert_eq!(
            budget.day_start(utc("2026-01-15T12:00:00Z")),
            utc("2026-01-14T23:00:00Z")
        );
    }

    /// The autumn day with two 02:00s is 25 hours long, and all 25 of them are
    /// one budget: the boundary is the *earlier* of the two candidate
    /// instants for the following midnight, not a 24-hour offset.
    #[test]
    fn the_long_and_short_dst_days_are_still_one_day_each() {
        let f = fixture();
        let budget = Budget::new(f.runs, 60, Stockholm);
        // 2026-10-25 is the autumn transition; local noon that day is 11:00Z.
        assert_eq!(
            budget.day_start(utc("2026-10-25T11:00:00Z")),
            utc("2026-10-24T22:00:00Z"),
            "the long day starts at the CEST midnight"
        );
        // 2027-03-28 is the spring transition.
        assert_eq!(
            budget.day_start(utc("2027-03-28T11:00:00Z")),
            utc("2027-03-27T23:00:00Z"),
            "the short day starts at the CET midnight"
        );
    }

    /// **The count has to survive a restart**, because the daemon restarts
    /// whenever it crashes and an in-memory counter would hand a crash loop an
    /// unlimited budget.
    #[test]
    fn the_count_survives_a_daemon_restart() {
        let f = fixture();
        let now = utc("2026-09-24T12:00:00Z");
        {
            let budget = Budget::new(f.runs.clone(), 2, Stockholm);
            run_at(&f, "triage.tier1", now);
            run_at(&f, "chat", now);
            assert!(!budget.try_consume(now).unwrap());
        }

        // A brand-new process, a brand-new connection, the same database.
        let reopened = Budget::new(RunStore::new(open(&f.path)), 2, Stockholm);
        assert_eq!(reopened.spent_today(now).unwrap(), 2);
        assert!(
            !reopened.try_consume(now).unwrap(),
            "a restart must not refund the day's spend"
        );
    }

    #[test]
    fn the_status_line_is_the_pair() {
        let f = fixture();
        let now = utc("2026-09-24T12:00:00Z");
        let budget = Budget::new(f.runs.clone(), 60, Stockholm);
        assert_eq!(budget.status_line(now), "0/60");
        run_at(&f, "chat", now);
        assert_eq!(budget.status_line(now), "1/60");
    }

    #[test]
    fn the_spent_note_names_the_zone_the_owner_has_to_wait_for() {
        let f = fixture();
        let note = Budget::new(f.runs, 60, Stockholm).spent_note();
        assert!(note.contains("budget"), "{note}");
        assert!(note.contains("Europe/Stockholm"), "{note}");
    }
}
