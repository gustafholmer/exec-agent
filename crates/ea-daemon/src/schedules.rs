//! Cron evaluation in a real time zone, and the three built-in briefings.
//!
//! Phase 1 shipped [`Scheduler`](crate::scheduler::Scheduler) with **fixed
//! intervals only**: a [`Job`] has a `Duration` and nothing else. The
//! `schedules` table has existed since the same phase and nothing read or
//! wrote it. So this module is not "write three job bodies"; it is the cron
//! evaluator that never existed, plus the bodies.
//!
//! # Why a cron needs more than an interval
//!
//! "Every 86 400 seconds" is not "07:00 daily". The two agree only until the
//! daemon restarts, and then the interval job fires at whatever time the
//! restart happened to leave it on. They also disagree twice a year, because
//! 07:00 local is 23 hours after the previous 07:00 in spring and 25 in
//! autumn. A morning briefing that drifts to 05:00 in April is not a morning
//! briefing.
//!
//! # How a local cron time becomes an instant
//!
//! This is the part with the sharp edges, and the reason the task exists.
//!
//! The cron expression is evaluated against the **wall clock** of
//! [`Schedule::time_zone`], and only then resolved to a UTC instant. The
//! evaluation trick is to hand the `cron` crate a `DateTime<Utc>` whose fields
//! are the *local* naive fields, so its iterator yields local wall-clock
//! candidates; resolving each candidate to a real instant is done here, by
//! [`Schedule::resolve`], where the DST policy can be stated explicitly rather
//! than inherited from whatever a dependency happens to do with an ambiguous
//! local time.
//!
//! The policy, and what each half is for:
//!
//! * **A repeated local hour fires once, at its first occurrence.** On
//!   2026-10-25 Stockholm runs 02:00–03:00 twice. A job at 02:30 has two valid
//!   instants that day, and a scheduler that answered "the next 02:30" from
//!   inside the second pass would fire it again — the owner gets the same
//!   briefing twice, an hour apart. So an ambiguous local time resolves to the
//!   *earliest* instant, and [`Schedule::next_after`] additionally discards any
//!   candidate that resolves at or before `after`. The second filter is not
//!   redundant: the first 02:30 (00:30 UTC) is *earlier* than an anchor set
//!   during the repeated hour (say 01:10 UTC), so without it a job anchored
//!   inside the second pass would find a "next" occurrence in its own past and
//!   fire immediately.
//! * **A local time that does not exist takes the next valid instant.** On
//!   2027-03-28 Stockholm jumps 02:00 → 03:00 and 02:30 never happens. The
//!   naive answer — "no such instant, skip the day" — silently drops a day's
//!   briefing once a year, in the direction nobody checks. [`Schedule::resolve`]
//!   walks forward a minute at a time to the first local time that does exist,
//!   so a 02:30 job fires at 03:00.
//!
//! 02:30 is the hour both tests use deliberately: it is inside the repeated
//! hour in autumn and inside the missing hour in spring. The real briefings run
//! at 07:00 and 09:00 and are untouched by either, but a test at a safe hour
//! would prove nothing about the machinery.
//!
//! # A missed run fires once, not once per occurrence
//!
//! [`ScheduleStore::mark_run`] is stamped with the instant the job **fired**,
//! never with the nominal cron time it fired for. Every occurrence between the
//! old anchor and the new one is therefore behind the anchor, and
//! [`Schedule::due`] only ever asks about the *first* occurrence after it. A
//! laptop shut on Friday afternoon and opened on Monday morning has missed
//! three 07:00s and produces one briefing.
//!
//! That is a deliberate choice rather than a fallout: three morning briefings
//! arriving at once are three interruptions reporting overlapping material,
//! and the third is the only one anybody would read. What the collapse costs is
//! the two intermediate reports, and it is cheap because the material is
//! cumulative — the digest has been accumulating the whole time and the
//! calendar read is of today.
//!
//! One consequence worth naming rather than discovering: a daemon that comes
//! back *before* the day's own occurrence produces two briefings that morning —
//! one catch-up on the tick it returned, then the ordinary one at 07:00. The
//! second is not a missed occurrence, it is today's, and collapsing it into the
//! catch-up would mean a daemon that restarted at 06:55 never briefed that day.
//! Pinned by `coming_back_before_the_days_occurrence_gives_the_catch_up_and_then_today`.
//!
//! # How this reaches the existing scheduler
//!
//! One [`Job`] per schedule, on a fixed [`CRON_CHECK_INTERVAL`], whose body
//! asks the question and usually answers "not yet". That is deliberately *not*
//! a second scheduler:
//!
//! * `wait_for`, the overlap guard, the circuit breaker with its half-open
//!   retry, `pause`/`resume` and the health push all apply unchanged, and none
//!   of that machinery had to be touched. In particular **`wait_for` is not
//!   changed**: its `interval.max(cooldown)` behaviour is correct and was
//!   recently confirmed.
//! * A tick that is not due returns `Ok(())`, so a schedule that is merely
//!   waiting looks healthy. Consecutive *failures* are what trip the breaker,
//!   and for a daily briefing that is five days — which is the right scale for
//!   a job that runs daily.
//! * While a breaker is tripped, `wait_for` raises the check interval to the
//!   cooldown (up to an hour). The due check is not a schedule, so a longer gap
//!   between checks delays a briefing by at most that gap and never loses it.

use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{DateTime, Duration, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use ea_core::store::schedules::ScheduleStore;

use crate::scheduler::Job;

/// How often a cron job's body asks whether it is due.
///
/// One minute, which is also the finest resolution a cron expression written
/// in this project needs: the three built-ins are on the hour. Finer would
/// bound nothing useful — `Scheduler`'s own loop ticks once a second — and
/// coarser would let a briefing land measurably late.
pub const CRON_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How many cron candidates [`Schedule::next_after`] will look at before giving
/// up and reporting no next occurrence.
///
/// An expression can be satisfiable in principle and not in the window the
/// `cron` crate searches, and one that is *unsatisfiable* (31 February) would
/// otherwise spin. Four thousand candidates is more than a decade of daily
/// occurrences, so nothing legitimate reaches the bound.
const MAX_CANDIDATES: usize = 4096;

/// How far [`Schedule::resolve`] walks forward out of a spring-forward gap
/// before giving up.
///
/// A day. Every real gap is an hour (Lord Howe Island's is thirty minutes);
/// a day is a bound rather than an estimate, and reaching it means the zone
/// data says something this code should not guess about.
const MAX_GAP_MINUTES: i64 = 24 * 60;

/// One cron entry, evaluated in one time zone.
///
/// `last_run` is carried on the value rather than read from the store by these
/// methods, so that every rule above is a pure function of
/// `(cron, time_zone, last_run, now)` and testable without a database.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub name: String,
    pub cron: cron::Schedule,
    pub time_zone: Tz,
    /// When this schedule last **fired** — not the cron time it fired for. See
    /// the module docs on missed runs.
    pub last_run: Option<DateTime<Utc>>,
}

impl Schedule {
    /// Parse a cron expression. Seven fields (`sec min hour dom month dow
    /// year`) or six without the year, which is the `cron` crate's dialect.
    pub fn parse(
        name: impl Into<String>,
        expression: &str,
        time_zone: Tz,
        last_run: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Self> {
        let name = name.into();
        let cron: cron::Schedule = expression.parse().with_context(|| {
            format!("schedule {name}: cannot parse cron expression {expression:?}")
        })?;
        Ok(Self {
            name,
            cron,
            time_zone,
            last_run,
        })
    }

    /// The first instant this schedule is due at strictly after `after`.
    ///
    /// `None` only when the expression has no further occurrence inside
    /// [`MAX_CANDIDATES`] — an unsatisfiable expression, or one whose next
    /// occurrence is past the `cron` crate's own search horizon.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // The cron expression is a statement about a wall clock, so it is
        // evaluated against one: `probe` carries the local naive fields of
        // `after` in a `DateTime<Utc>`, which makes the crate's iterator a
        // generator of local wall-clock candidates and keeps every DST
        // decision here rather than inside a dependency.
        let probe = Utc.from_utc_datetime(&after.with_timezone(&self.time_zone).naive_local());
        self.cron
            .after(&probe)
            .take(MAX_CANDIDATES)
            .filter_map(|candidate| self.resolve(candidate.naive_utc()))
            // A repeated local hour puts a *later* wall-clock candidate at an
            // *earlier* instant than an anchor set inside that hour. Without
            // this the job would fire again on an occurrence already behind it.
            .find(|instant| *instant > after)
    }

    /// Turn a local wall-clock time into the instant this schedule means by it.
    ///
    /// The two DST cases are the whole point; see the module docs.
    fn resolve(&self, local: NaiveDateTime) -> Option<DateTime<Utc>> {
        match self.time_zone.from_local_datetime(&local) {
            LocalResult::Single(at) => Some(at.with_timezone(&Utc)),
            // The clock went back and this wall-clock time happens twice. The
            // first one is the job's; `next_after` makes sure the second is
            // never treated as a fresh occurrence.
            LocalResult::Ambiguous(earliest, _latest) => Some(earliest.with_timezone(&Utc)),
            // The clock went forward and this wall-clock time does not exist.
            // The next instant that does is the answer — never "skip the day".
            LocalResult::None => (1..=MAX_GAP_MINUTES).find_map(|minutes| {
                let shifted = local.checked_add_signed(Duration::minutes(minutes))?;
                self.time_zone
                    .from_local_datetime(&shifted)
                    .earliest()
                    .map(|at| at.with_timezone(&Utc))
            }),
        }
    }

    /// Is this schedule due at `now`?
    ///
    /// `false` for a schedule with no anchor. An anchor is written when the
    /// schedule is registered ([`ScheduleStore::ensure`]), so in the running
    /// daemon there always is one; treating its absence as "every past
    /// occurrence is missed" would make a fresh install's first tick fire all
    /// three briefings at once, and treating it as "due now" would do the same.
    /// Refusing to fire is the one answer that cannot surprise the owner, and
    /// it is recoverable — the next `ensure` re-anchors.
    pub fn due(&self, now: DateTime<Utc>) -> bool {
        match self.last_run {
            Some(last) => self.next_after(last).is_some_and(|next| now >= next),
            None => false,
        }
    }
}

// --------------------------------------------------------------------------
// The three built-ins
// --------------------------------------------------------------------------

/// Job name and `runs` kind of the daily briefing.
pub const MORNING_BRIEFING: &str = "morning_briefing";
/// 07:00 every day, local.
pub const MORNING_BRIEFING_CRON: &str = "0 0 7 * * *";

/// Job name and `runs` kind of the weekly accounting pass.
pub const BOOKKEEPING_PASS: &str = "bookkeeping_pass";
/// 09:00 every Monday, local.
pub const BOOKKEEPING_PASS_CRON: &str = "0 0 9 * * MON";

/// Job name and `runs` kind of the monthly VAT pass.
pub const VAT_PREP: &str = "vat_prep";
/// 09:00 on the first of the month, local.
pub const VAT_PREP_CRON: &str = "0 0 9 1 * *";

/// The three built-in schedules, as `(name, expression)`.
///
/// One list, so that "what does this daemon run on a clock" is readable in one
/// place and so the registration loop cannot drift from the constants.
pub const BUILT_IN: [(&str, &str); 3] = [
    (MORNING_BRIEFING, MORNING_BRIEFING_CRON),
    (BOOKKEEPING_PASS, BOOKKEEPING_PASS_CRON),
    (VAT_PREP, VAT_PREP_CRON),
];

// --------------------------------------------------------------------------
// The job wrapper
// --------------------------------------------------------------------------

/// Wrap a cron schedule as a [`Job`] the existing scheduler can drive.
///
/// `body` runs at most once per occurrence. The anchor is stamped **before**
/// `body` is awaited, which is a deliberate trade: a briefing whose session or
/// Telegram send fails is skipped until its next occurrence rather than
/// retried every minute until the breaker trips. A briefing is a summary of a
/// moment, four copies of it are worse than none, and the failure is not lost —
/// it reaches `ea status` through the breaker's `last_error`, and the digest is
/// only cleared on success, so tomorrow's briefing still carries the backlog.
///
/// The expression is parsed on every tick rather than once at construction.
/// That is what lets `schedules.cron` be edited in the database and take effect
/// without a restart, and a row whose expression no longer parses fails *that
/// job* — visibly, through the breaker — instead of the whole daemon's startup.
pub fn cron_job<F, Fut>(name: &str, time_zone: Tz, store: ScheduleStore, body: F) -> Job
where
    F: Fn(Fired) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let name = name.to_string();
    let body = Arc::new(body);
    Job::new(name.clone(), CRON_CHECK_INTERVAL, move || {
        let name = name.clone();
        let store = store.clone();
        let body = Arc::clone(&body);
        async move {
            let now = Utc::now();
            let Some(fired) = claim(&name, time_zone, &store, now)? else {
                return Ok(());
            };
            tracing::info!(
                schedule = %name,
                previous = ?fired.previous.map(|at| at.to_rfc3339()),
                "cron schedule fired"
            );
            body(fired).await
        }
    })
}

/// One firing: when it happened, and when the schedule last fired before it.
///
/// `previous` is what "since the last briefing" means, and it is why [`claim`]
/// returns something richer than a bool: the anchor is overwritten before the
/// body runs, so the body could not read it for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fired {
    pub now: DateTime<Utc>,
    /// `None` only for a schedule whose stored anchor was unreadable, which
    /// [`Schedule::due`] refuses to fire on — so in practice always `Some`.
    pub previous: Option<DateTime<Utc>>,
}

/// Decide whether `name` is due at `now`, and if so take it — stamping the
/// anchor so no later tick can fire the same occurrence.
///
/// Separated from [`cron_job`] so the decision is testable without spawning a
/// job, and returns `Ok(None)` rather than an error for the ordinary "not yet"
/// case, which must not look like a failure to the breaker.
fn claim(
    name: &str,
    time_zone: Tz,
    store: &ScheduleStore,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Fired>> {
    let Some(row) = store.get(name)? else {
        bail!("schedule {name} is not registered in the schedules table");
    };
    if !row.enabled {
        tracing::debug!(schedule = name, "schedule is disabled");
        return Ok(None);
    }
    let schedule = Schedule::parse(name, &row.cron, time_zone, row.last_run_at)?;
    if !schedule.due(now) {
        return Ok(None);
    }
    store.mark_run(name, now)?;
    Ok(Some(Fired {
        now,
        previous: row.last_run_at,
    }))
}

/// Write every built-in schedule into the `schedules` table, anchored to `now`
/// if it is not already there.
///
/// Called once at start-up, before the jobs are registered. Idempotent; see
/// [`ScheduleStore::ensure`].
pub fn register_built_ins(store: &ScheduleStore, now: DateTime<Utc>) -> anyhow::Result<()> {
    for (name, expression) in BUILT_IN {
        // Parsed here as well as stored, so a typo in a built-in expression is
        // a start-up error naming the schedule rather than a job that trips its
        // breaker once a minute for the life of the process.
        Schedule::parse(name, expression, chrono_tz::UTC, None)?;
        store.ensure(name, expression, now)?;
    }
    Ok(())
}

/// The three briefing jobs, ready for [`Scheduler::add`](crate::scheduler::Scheduler::add).
///
/// Here rather than in `main` so that "which schedule runs which briefing" is
/// one readable list, and so the wiring is covered by this crate's tests rather
/// than only by running the binary.
pub fn briefing_jobs<C>(
    store: ScheduleStore,
    time_zone: Tz,
    deps: Arc<crate::briefings::BriefingDeps<C>>,
) -> Vec<Job>
where
    C: crate::executor::ToolCaller + Send + Sync + 'static,
{
    let morning = {
        let deps = Arc::clone(&deps);
        cron_job(
            MORNING_BRIEFING,
            time_zone,
            store.clone(),
            move |fired: Fired| {
                let deps = Arc::clone(&deps);
                async move {
                    // `previous` is the anchor this firing replaced, which for
                    // the morning briefing is literally "the last briefing".
                    let since = fired.previous.unwrap_or(fired.now);
                    let outcome =
                        crate::briefings::run_morning_briefing(&deps, fired.now, since).await?;
                    tracing::info!(?outcome, "morning briefing");
                    Ok(())
                }
            },
        )
    };
    let bookkeeping = {
        let deps = Arc::clone(&deps);
        cron_job(
            BOOKKEEPING_PASS,
            time_zone,
            store.clone(),
            move |fired: Fired| {
                let deps = Arc::clone(&deps);
                async move {
                    let outcome = crate::briefings::run_bookkeeping_pass(&deps, fired.now).await?;
                    tracing::info!(?outcome, "bookkeeping pass");
                    Ok(())
                }
            },
        )
    };
    let vat = cron_job(VAT_PREP, time_zone, store, move |fired: Fired| {
        let deps = Arc::clone(&deps);
        async move {
            let outcome = crate::briefings::run_vat_prep(&deps, fired.now).await?;
            tracing::info!(?outcome, "vat prep");
            Ok(())
        }
    });
    vec![morning, bookkeeping, vat]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use chrono_tz::Europe::Stockholm;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, Arc<Mutex<Connection>>) {
        let dir = TempDir::new().unwrap();
        let conn = ea_core::db::open(&dir.path().join("state.db")).unwrap();
        (dir, Arc::new(Mutex::new(conn)))
    }

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn daily_at(local_time: &str, time_zone: Tz) -> Schedule {
        let (hour, minute) = local_time.split_once(':').expect("HH:MM");
        Schedule::parse(
            "test",
            &format!("0 {minute} {hour} * * *"),
            time_zone,
            Some(utc("2000-01-01T00:00:00Z")),
        )
        .expect("the expression parses")
    }

    /// Tick once a minute across `[from, to)`, firing when due and anchoring to
    /// the tick that fired — exactly what [`cron_job`] does with the store.
    fn count_fires(schedule: &Schedule, from: &str, to: &str) -> usize {
        count_fires_from(schedule, from, from, to)
    }

    /// [`count_fires`] with the anchor set independently of the window, which
    /// is how a daemon that was *down* is modelled: no ticks happened between
    /// `anchor` and `from`, so every occurrence in that gap was missed.
    fn count_fires_from(schedule: &Schedule, anchor: &str, from: &str, to: &str) -> usize {
        let (mut anchor, to) = (utc(anchor), utc(to));
        let mut fires = 0;
        let mut now = utc(from);
        while now < to {
            let mut probe = schedule.clone();
            probe.last_run = Some(anchor);
            if probe.due(now) {
                fires += 1;
                anchor = now;
            }
            now += Duration::minutes(1);
        }
        fires
    }

    // -----------------------------------------------------------------------
    // The premise, checked against chrono-tz rather than assumed
    // -----------------------------------------------------------------------

    /// The two dates the DST tests below rest on, asserted from the zone data
    /// itself. Three dates stated from memory in this project have turned out
    /// to be wrong, so the premise is a test rather than a comment: if the
    /// `chrono-tz` database ever disagrees, this fails first and names why,
    /// instead of the behaviour tests failing for a reason nobody can see.
    #[test]
    fn stockholm_repeats_0230_on_2026_10_25_and_skips_it_on_2027_03_28() {
        let at = |text: &str| {
            let naive: NaiveDateTime = text.parse().expect("a naive datetime");
            Stockholm.from_local_datetime(&naive)
        };

        assert!(
            matches!(at("2026-10-25T02:30:00"), LocalResult::Ambiguous(_, _)),
            "02:30 must happen twice on 2026-10-25: {:?}",
            at("2026-10-25T02:30:00")
        );
        assert!(
            matches!(at("2027-03-28T02:30:00"), LocalResult::None),
            "02:30 must not exist on 2027-03-28: {:?}",
            at("2027-03-28T02:30:00")
        );

        // And on no neighbouring day, so the dates are pinned rather than
        // merely inside the right week.
        for ordinary in [
            "2026-10-24T02:30:00",
            "2026-10-26T02:30:00",
            "2027-03-27T02:30:00",
            "2027-03-29T02:30:00",
        ] {
            assert!(
                matches!(at(ordinary), LocalResult::Single(_)),
                "{ordinary} must be an ordinary local time: {:?}",
                at(ordinary)
            );
        }
    }

    /// The offsets the transitions move between, so a test that starts passing
    /// for the wrong reason is caught.
    #[test]
    fn the_transitions_move_between_cest_and_cet() {
        let ambiguous = Stockholm.from_local_datetime(
            &"2026-10-25T02:30:00"
                .parse::<NaiveDateTime>()
                .expect("naive"),
        );
        let LocalResult::Ambiguous(earliest, latest) = ambiguous else {
            panic!("expected two instants, got {ambiguous:?}");
        };
        assert_eq!(earliest.with_timezone(&Utc), utc("2026-10-25T00:30:00Z"));
        assert_eq!(latest.with_timezone(&Utc), utc("2026-10-25T01:30:00Z"));
    }

    // -----------------------------------------------------------------------
    // Review Focus #4: the DST behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn a_daily_job_fires_once_on_the_day_the_clock_goes_back() {
        let schedule = daily_at("02:30", Stockholm);
        let fires = count_fires(&schedule, "2026-10-25T00:00:00Z", "2026-10-26T00:00:00Z");
        assert_eq!(
            fires, 1,
            "a repeated local hour must not fire the job twice"
        );
    }

    #[test]
    fn a_daily_job_still_fires_on_the_day_the_clock_goes_forward() {
        let schedule = daily_at("02:30", Stockholm);
        let fires = count_fires(&schedule, "2027-03-28T00:00:00Z", "2027-03-29T00:00:00Z");
        assert_eq!(fires, 1, "a skipped local hour must not swallow the job");
    }

    /// The autumn job fires on the *first* 02:30, not the second.
    #[test]
    fn the_repeated_hour_fires_at_its_first_occurrence() {
        let mut schedule = daily_at("02:30", Stockholm);
        schedule.last_run = Some(utc("2026-10-24T00:30:00Z"));
        assert_eq!(
            schedule.next_after(utc("2026-10-24T00:30:00Z")),
            Some(utc("2026-10-25T00:30:00Z")),
            "00:30Z is 02:30 CEST, the first of the two"
        );
    }

    /// The specific double-fire the `> after` filter in `next_after` exists
    /// for: an anchor set *inside* the repeated hour, after the first 02:30 has
    /// already gone by.
    #[test]
    fn an_anchor_inside_the_repeated_hour_does_not_find_an_occurrence_in_its_past() {
        let mut schedule = daily_at("02:30", Stockholm);
        // 01:10Z is 02:10 CET — the second pass through 02:00–03:00, so the
        // local wall clock reads 02:10 and the "next" 02:30 by wall clock is
        // 00:30Z, an hour and a half earlier.
        let inside = utc("2026-10-25T01:10:00Z");
        schedule.last_run = Some(inside);
        assert!(!schedule.due(inside), "must not fire on its own anchor");
        assert_eq!(
            schedule.next_after(inside),
            Some(utc("2026-10-26T01:30:00Z")),
            "the next occurrence is tomorrow, not an hour and a half ago"
        );
    }

    /// Spring forward: 02:30 does not exist, so the job takes 03:00 rather than
    /// skipping the day.
    #[test]
    fn a_missing_local_time_resolves_to_the_next_valid_instant() {
        let schedule = daily_at("02:30", Stockholm);
        assert_eq!(
            schedule.next_after(utc("2027-03-27T01:30:00Z")),
            Some(utc("2027-03-28T01:00:00Z")),
            "01:00Z is 03:00 CEST, the first instant after the gap"
        );
    }

    /// The briefings the owner actually has run at 07:00 and 09:00, and the
    /// point of the DST work is that those are 07:00 and 09:00 *local* on both
    /// transition days rather than drifting an hour.
    #[test]
    fn the_real_briefing_hour_is_local_on_both_transition_days() {
        let morning = Schedule::parse(
            MORNING_BRIEFING,
            MORNING_BRIEFING_CRON,
            Stockholm,
            Some(utc("2026-10-24T05:00:00Z")),
        )
        .unwrap();
        // 2026-10-25 is CET from 03:00 local, so 07:00 local is 06:00 UTC —
        // an hour later in UTC than the day before, which is the whole point.
        assert_eq!(
            morning.next_after(utc("2026-10-24T05:00:00Z")),
            Some(utc("2026-10-25T06:00:00Z"))
        );

        let spring = Schedule::parse(
            MORNING_BRIEFING,
            MORNING_BRIEFING_CRON,
            Stockholm,
            Some(utc("2027-03-27T06:00:00Z")),
        )
        .unwrap();
        assert_eq!(
            spring.next_after(utc("2027-03-27T06:00:00Z")),
            Some(utc("2027-03-28T05:00:00Z")),
            "07:00 local on 2027-03-28 is 05:00 UTC"
        );
    }

    // -----------------------------------------------------------------------
    // The ordinary rules
    // -----------------------------------------------------------------------

    #[test]
    fn a_job_fires_at_its_cron_time_and_not_before() {
        let mut schedule = daily_at("07:00", Stockholm);
        schedule.last_run = Some(utc("2026-09-24T05:00:00Z"));
        // 07:00 Stockholm on 2026-09-25 is 05:00 UTC (CEST, +02:00).
        assert!(!schedule.due(utc("2026-09-25T04:59:00Z")));
        assert!(schedule.due(utc("2026-09-25T05:00:00Z")));
        assert!(schedule.due(utc("2026-09-25T05:01:00Z")));
    }

    /// The anchor is what stops a second fire, and it stops it for the rest of
    /// the minute and the rest of the day.
    #[test]
    fn the_anchor_prevents_a_second_fire_in_the_same_minute_and_the_same_day() {
        let mut schedule = daily_at("07:00", Stockholm);
        schedule.last_run = Some(utc("2026-09-25T05:00:00Z"));
        assert!(!schedule.due(utc("2026-09-25T05:00:30Z")));
        assert!(!schedule.due(utc("2026-09-25T05:01:00Z")));
        assert!(!schedule.due(utc("2026-09-25T23:59:00Z")));
        assert!(schedule.due(utc("2026-09-26T05:00:00Z")));
    }

    /// **The laptop-over-the-weekend test.** Three 07:00s went by with the
    /// daemon down; Monday produces one briefing.
    #[test]
    fn a_weekend_of_missed_runs_produces_one_briefing_not_three() {
        let schedule = daily_at("07:00", Stockholm);
        let mut probe = schedule.clone();
        // Fired on Friday morning, then the lid closed.
        probe.last_run = Some(utc("2026-09-25T05:00:00Z"));

        // Monday 08:00 local, the daemon back up: due once.
        let monday = utc("2026-09-28T06:00:00Z");
        assert!(probe.due(monday));
        probe.last_run = Some(monday);
        assert!(
            !probe.due(monday),
            "Saturday's and Sunday's occurrences are behind the anchor, not queued"
        );
        assert!(!probe.due(utc("2026-09-28T23:59:00Z")));
        assert!(
            probe.due(utc("2026-09-29T05:00:00Z")),
            "and Tuesday still comes"
        );
    }

    /// Same thing measured rather than argued. The anchor is Friday lunchtime,
    /// the lid is shut, and the first tick of the simulation is Monday
    /// morning: Saturday's, Sunday's and Monday's 07:00s are all behind the
    /// anchor when the daemon comes back, and exactly one briefing comes out.
    #[test]
    fn ticking_after_a_missed_weekend_fires_once() {
        let schedule = daily_at("07:00", Stockholm);
        let fires = count_fires_from(
            &schedule,
            "2026-09-25T12:00:00Z", // last up: Friday, after that day's briefing
            "2026-09-28T06:00:00Z", // back up: Monday 08:00 local
            "2026-09-28T23:00:00Z",
        );
        assert_eq!(
            fires, 1,
            "Saturday, Sunday and Monday collapse into one briefing"
        );
    }

    /// The other half of the same rule, pinned so nobody "fixes" it later. A
    /// daemon that comes back *before* the day's own occurrence gets two
    /// briefings that morning: one catch-up for the weekend, on the tick it
    /// came back, and then the ordinary 07:00 one. That is correct — the
    /// second is not a missed occurrence, it is today's — and it is still not
    /// "three briefings at once".
    #[test]
    fn coming_back_before_the_days_occurrence_gives_the_catch_up_and_then_today() {
        let schedule = daily_at("07:00", Stockholm);
        let fires = count_fires_from(
            &schedule,
            "2026-09-25T12:00:00Z", // last up: Friday
            "2026-09-28T04:00:00Z", // back up: Monday 06:00 local, before 07:00
            "2026-09-28T23:00:00Z",
        );
        assert_eq!(
            fires, 2,
            "one catch-up for the whole weekend, then Monday's own 07:00"
        );
    }

    /// The control for the test above: with the daemon *up* the whole time, the
    /// same three occurrences produce three briefings. The collapse is a
    /// property of downtime, not of the evaluator quietly losing occurrences.
    #[test]
    fn the_same_three_days_with_the_daemon_up_produce_three_briefings() {
        let schedule = daily_at("07:00", Stockholm);
        let fires = count_fires(&schedule, "2026-09-25T12:00:00Z", "2026-09-28T23:00:00Z");
        assert_eq!(fires, 3);
    }

    /// An ordinary week: one fire a day, no more and no fewer.
    #[test]
    fn a_daily_job_fires_exactly_once_a_day_across_an_ordinary_week() {
        let schedule = daily_at("07:00", Stockholm);
        let fires = count_fires(&schedule, "2026-09-01T00:00:00Z", "2026-09-08T00:00:00Z");
        assert_eq!(fires, 7);
    }

    /// And across a week containing the autumn transition: still seven.
    #[test]
    fn a_daily_job_fires_seven_times_across_the_week_the_clock_goes_back() {
        let schedule = daily_at("02:30", Stockholm);
        let fires = count_fires(&schedule, "2026-10-21T00:00:00Z", "2026-10-28T00:00:00Z");
        assert_eq!(fires, 7, "one 02:30 per day, transition included");
    }

    /// And across the spring one. The window is seven local days; because the
    /// spring day is 23 hours long it is 167 UTC hours, which is why the bounds
    /// are not simply seven times twenty-four.
    #[test]
    fn a_daily_job_fires_seven_times_across_the_week_the_clock_goes_forward() {
        let schedule = daily_at("02:30", Stockholm);
        let fires = count_fires(&schedule, "2027-03-24T00:00:00Z", "2027-03-30T23:00:00Z");
        assert_eq!(fires, 7);
    }

    #[test]
    fn a_schedule_with_no_anchor_never_fires() {
        let mut schedule = daily_at("07:00", Stockholm);
        schedule.last_run = None;
        assert!(!schedule.due(utc("2026-09-25T05:00:00Z")));
        assert!(!schedule.due(utc("2030-01-01T00:00:00Z")));
    }

    #[test]
    fn a_malformed_expression_is_an_error_naming_the_schedule() {
        let err = Schedule::parse("morning_briefing", "not a cron", Stockholm, None)
            .expect_err("must not parse");
        let text = format!("{err:#}");
        assert!(text.contains("morning_briefing"), "{text}");
        assert!(text.contains("not a cron"), "{text}");
    }

    // -----------------------------------------------------------------------
    // The built-ins say what they claim to say
    // -----------------------------------------------------------------------

    #[test]
    fn the_morning_briefing_is_0700_every_day() {
        let schedule =
            Schedule::parse(MORNING_BRIEFING, MORNING_BRIEFING_CRON, Stockholm, None).unwrap();
        let mut at = utc("2026-09-24T00:00:00Z");
        for expected in [
            "2026-09-24T05:00:00Z",
            "2026-09-25T05:00:00Z",
            "2026-09-26T05:00:00Z",
        ] {
            at = schedule.next_after(at).expect("an occurrence");
            assert_eq!(at, utc(expected));
        }
    }

    #[test]
    fn the_bookkeeping_pass_is_0900_on_mondays_only() {
        let schedule =
            Schedule::parse(BOOKKEEPING_PASS, BOOKKEEPING_PASS_CRON, Stockholm, None).unwrap();
        // 2026-09-24 is a Thursday; the next Monday is 2026-09-28, and 09:00
        // CEST is 07:00 UTC.
        let first = schedule.next_after(utc("2026-09-24T00:00:00Z")).unwrap();
        assert_eq!(first, utc("2026-09-28T07:00:00Z"));
        assert_eq!(
            first
                .with_timezone(&Stockholm)
                .format("%A %H:%M")
                .to_string(),
            "Monday 09:00"
        );
        let second = schedule.next_after(first).unwrap();
        assert_eq!(second, utc("2026-10-05T07:00:00Z"), "a week later");
    }

    #[test]
    fn the_vat_prep_is_0900_on_the_first_of_the_month() {
        let schedule = Schedule::parse(VAT_PREP, VAT_PREP_CRON, Stockholm, None).unwrap();
        let first = schedule.next_after(utc("2026-09-24T00:00:00Z")).unwrap();
        assert_eq!(first, utc("2026-10-01T07:00:00Z"));
        // Across a DST boundary the *local* hour is what holds: 09:00 CET on
        // 1 November is 08:00 UTC, not 07:00.
        let next = schedule.next_after(first).unwrap();
        assert_eq!(next, utc("2026-11-01T08:00:00Z"));
        assert_eq!(
            next.with_timezone(&Stockholm)
                .format("%d %H:%M")
                .to_string(),
            "01 09:00"
        );
    }

    #[test]
    fn every_built_in_expression_parses() {
        for (name, expression) in BUILT_IN {
            Schedule::parse(name, expression, Stockholm, None)
                .unwrap_or_else(|err| panic!("{name}: {err:#}"));
        }
    }

    // -----------------------------------------------------------------------
    // `claim`: the store half
    // -----------------------------------------------------------------------

    #[test]
    fn claim_takes_an_occurrence_once_and_stamps_the_anchor() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        store
            .ensure(
                MORNING_BRIEFING,
                MORNING_BRIEFING_CRON,
                utc("2026-09-24T12:00:00Z"),
            )
            .unwrap();

        let before = utc("2026-09-25T04:59:00Z");
        assert!(claim(MORNING_BRIEFING, Stockholm, &store, before)
            .unwrap()
            .is_none());

        let at = utc("2026-09-25T05:00:00Z");
        let fired = claim(MORNING_BRIEFING, Stockholm, &store, at)
            .unwrap()
            .expect("due");
        assert_eq!(fired.now, at);
        assert_eq!(
            fired.previous,
            Some(utc("2026-09-24T12:00:00Z")),
            "the body is handed the anchor this firing replaced"
        );
        assert_eq!(
            store.get(MORNING_BRIEFING).unwrap().unwrap().last_run_at,
            Some(at)
        );

        // The next tick, a minute later, must not take the same occurrence.
        assert!(claim(
            MORNING_BRIEFING,
            Stockholm,
            &store,
            utc("2026-09-25T05:01:00Z")
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn claim_skips_a_disabled_schedule_without_stamping_it() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T12:00:00Z");
        store
            .ensure(MORNING_BRIEFING, MORNING_BRIEFING_CRON, installed)
            .unwrap();
        store.set_enabled(MORNING_BRIEFING, false).unwrap();

        assert!(claim(
            MORNING_BRIEFING,
            Stockholm,
            &store,
            utc("2026-09-25T05:00:00Z")
        )
        .unwrap()
        .is_none());
        assert_eq!(
            store.get(MORNING_BRIEFING).unwrap().unwrap().last_run_at,
            Some(installed),
            "a disabled schedule is skipped, not silently advanced"
        );
    }

    #[test]
    fn claim_of_an_unregistered_schedule_is_an_error_not_a_silent_no() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let err = claim("never_registered", Stockholm, &store, Utc::now())
            .expect_err("an unregistered schedule must be visible");
        assert!(format!("{err:#}").contains("never_registered"));
    }

    /// A row whose expression has been hand-edited into nonsense fails that
    /// job, visibly, rather than the daemon.
    #[test]
    fn claim_reports_an_unparseable_stored_expression() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(Arc::clone(&conn));
        store
            .ensure(VAT_PREP, "not a cron at all", utc("2026-09-24T12:00:00Z"))
            .unwrap();
        let err = claim(VAT_PREP, Stockholm, &store, Utc::now()).expect_err("must not parse");
        assert!(format!("{err:#}").contains("not a cron at all"));
    }

    /// The anchor `ensure` writes is what keeps a fresh install quiet: it is
    /// installed at 12:00 local and the first briefing is the next 07:00, not
    /// the tick after start-up.
    #[test]
    fn a_fresh_install_does_not_fire_all_three_briefings_at_once() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T10:00:00Z"); // 12:00 Stockholm
        for (name, expression) in BUILT_IN {
            store.ensure(name, expression, installed).unwrap();
        }

        for (name, _) in BUILT_IN {
            assert!(
                claim(name, Stockholm, &store, installed).unwrap().is_none(),
                "{name} must not fire on the tick it was installed"
            );
        }
        // And the first one to come is the morning briefing, tomorrow.
        assert!(claim(
            MORNING_BRIEFING,
            Stockholm,
            &store,
            utc("2026-09-25T05:00:00Z")
        )
        .unwrap()
        .is_some());
        assert!(
            claim(VAT_PREP, Stockholm, &store, utc("2026-09-25T05:00:00Z"))
                .unwrap()
                .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // Registration, and the job as the real scheduler drives it
    // -----------------------------------------------------------------------

    #[test]
    fn register_built_ins_writes_all_three_and_is_idempotent() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        let installed = utc("2026-09-24T10:00:00Z");

        register_built_ins(&store, installed).unwrap();
        register_built_ins(&store, utc("2026-09-25T10:00:00Z")).unwrap();

        let rows = store.all().unwrap();
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert!(row.enabled);
            assert_eq!(
                row.last_run_at,
                Some(installed),
                "{}: the second registration must not re-anchor",
                row.name
            );
        }
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![BOOKKEEPING_PASS, MORNING_BRIEFING, VAT_PREP],
            "alphabetical, from the store"
        );
    }

    /// Driven by the real [`crate::scheduler::Scheduler`], not by calling
    /// `claim` directly: the whole point of the one-job-per-schedule shape is
    /// that Phase 1's machinery drives it unchanged.
    #[tokio::test]
    async fn a_cron_job_fires_once_through_the_real_scheduler() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        // Anchored two days ago, so the schedule is overdue the moment the
        // scheduler looks at it.
        store
            .ensure(
                MORNING_BRIEFING,
                MORNING_BRIEFING_CRON,
                Utc::now() - Duration::days(2),
            )
            .unwrap();

        let ran = Arc::new(AtomicUsize::new(0));
        let job = cron_job(MORNING_BRIEFING, Stockholm, store.clone(), {
            let ran = Arc::clone(&ran);
            move |_fired| {
                let ran = Arc::clone(&ran);
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }
        });

        let scheduler = crate::scheduler::Scheduler::new(3);
        scheduler.add(job);

        let start = std::time::Instant::now();
        for handle in scheduler.tick(start) {
            handle.await.unwrap();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(!scheduler.is_tripped(MORNING_BRIEFING));

        // Two more check intervals go by; the occurrence is already taken.
        for offset in [120, 240] {
            for handle in scheduler.tick(start + std::time::Duration::from_secs(offset)) {
                handle.await.unwrap();
            }
        }
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the anchor stops later ticks re-firing the same occurrence"
        );
    }

    /// A briefing whose body fails must not be retried every minute: the
    /// anchor is stamped before the body runs, so the failure costs that
    /// occurrence and reaches `ea status` through the breaker instead.
    #[tokio::test]
    async fn a_failing_body_still_consumes_its_occurrence() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        store
            .ensure(
                MORNING_BRIEFING,
                MORNING_BRIEFING_CRON,
                Utc::now() - Duration::days(2),
            )
            .unwrap();

        let job = cron_job(MORNING_BRIEFING, Stockholm, store.clone(), |_fired| async {
            anyhow::bail!("telegram is down")
        });
        let scheduler = crate::scheduler::Scheduler::new(3);
        scheduler.add(job);

        let start = std::time::Instant::now();
        for handle in scheduler.tick(start) {
            handle.await.unwrap();
        }

        assert!(scheduler
            .last_error(MORNING_BRIEFING)
            .unwrap_or_default()
            .contains("telegram is down"));
        let anchor = store
            .get(MORNING_BRIEFING)
            .unwrap()
            .unwrap()
            .last_run_at
            .expect("stamped");
        assert!(
            anchor > Utc::now() - Duration::minutes(1),
            "the occurrence is consumed even though the body failed"
        );
    }

    /// A schedule that is merely waiting is not a failure, so a breaker never
    /// trips on a job that has simply not come round yet.
    #[tokio::test]
    async fn ticks_before_the_cron_time_are_successes_not_failures() {
        let (_dir, conn) = temp_store();
        let store = ScheduleStore::new(conn);
        store
            .ensure(MORNING_BRIEFING, MORNING_BRIEFING_CRON, Utc::now())
            .unwrap();
        let job = cron_job(MORNING_BRIEFING, Stockholm, store, |_fired| async {
            panic!("must not run")
        });
        let scheduler = crate::scheduler::Scheduler::new(1);
        scheduler.add(job);

        let start = std::time::Instant::now();
        for offset in [0, 60, 120, 180] {
            for handle in scheduler.tick(start + std::time::Duration::from_secs(offset)) {
                handle.await.unwrap();
            }
        }
        assert!(!scheduler.is_tripped(MORNING_BRIEFING));
        assert_eq!(scheduler.last_error(MORNING_BRIEFING), None);
    }
}
