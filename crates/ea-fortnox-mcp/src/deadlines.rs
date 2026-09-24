//! Swedish tax deadlines, computed rather than fetched.
//!
//! Everything here is pure: [`upcoming_deadlines`] and
//! [`adjust_for_weekend_and_holidays`] take dates and return dates. No HTTP
//! call, no Fortnox credentials, no clock of their own. That is deliberate —
//! Fortnox has no "when is my next momsdeklaration due" endpoint, the answer
//! is a calendar rule rather than a fact about this company's books, and a
//! reminder that stops working when a token lapses is the one you needed.
//!
//! # The rule
//!
//! * **Arbetsgivardeklaration (AGI)** and **debiterad preliminärskatt** are
//!   monthly. The ordinary due date is the **12th of the month following the
//!   period**: wages paid in March are declared by 12 April. In **January and
//!   August** the ordinary day is the **17th**, not the 12th — see
//!   [`due_day`].
//! * **Moms** is quarterly here, filed on the ordinary due day of the
//!   **second** month after the quarter ends: Q1 (January–March) falls due
//!   12 May, Q2 on 17 August, Q3 on 12 November, Q4 on 12 February of the
//!   following year.
//!
//! A due date that lands on a Saturday, a Sunday or a Swedish public holiday
//! moves **forward** to the next ordinary working day, which is Skatteverket's
//! own rule (lag 1930:173 om beräkning av lagstadgad tid).
//!
//! # Why the dates have to be the real ones
//!
//! These are reminders, not authority, and every payload says so — see
//! [`REMINDER_NOTE`]. That note is not a licence to be approximate. The
//! payload carries `due_on` as a concrete date, and the external id is
//! `tax-deadline:<kind>:<period>` over a payload with no clock in it, so the
//! daemon raises each deadline **once**: it is triaged once and never
//! re-opened. A date that is merely close is therefore not an early reminder,
//! it is a reminder for a day that does not exist, followed by silence through
//! the real one. An earlier version of this module filed kvartalsmoms in the
//! first month after the quarter and used the 12th in every month; both were
//! wrong in exactly that way, and both are fixed here.
//!
//! # Why no Easter
//!
//! [`FIXED_HOLIDAYS`] holds only the holidays that fall on a fixed calendar
//! date. Sweden's movable feasts — långfredagen, påskdagen, annandag påsk,
//! Kristi himmelsfärdsdag, pingstdagen, midsommarafton and midsommardagen —
//! are computed from Easter or from a floating weekday, and none of them is
//! implemented here. The reason, stated precisely rather than as folklore:
//!
//! * **Midsommar can never be a 12th.** Midsommarafton is the Friday between
//!   19 and 25 June, midsommardagen the Saturday between 20 and 26 June.
//! * **The Easter cluster can be a 12th, but not soon.** This was checked
//!   rather than assumed, and the common claim that these feasts "never land
//!   on the 12th" is simply false: långfredagen falls on 12 April in 2047,
//!   2058 and 2069; annandag påsk on 12 April in 2066; Kristi himmelsfärd on
//!   12 May in 2067. **The first collision of any kind is 12 April 2047.**
//!   `the_omission_of_easter_is_safe_until_2047` pins that window with its own
//!   computus, in test code, and drives its range from `Utc::now().year()`, so
//!   the claim is measured and the test genuinely **fails** once the clock
//!   reaches 2047 rather than passing forever on a hardcoded range.
//!   (The January and August 17ths are outside the Easter cluster entirely —
//!   Easter Sunday ranges 22 March to 25 April, and the latest derived feast,
//!   pingstdagen, is at most 13 June — so the per-month due day does not widen
//!   this exposure.)
//! * **Påskdagen and pingstdagen on a 12th (2093, 2095) are Sundays**, so the
//!   weekend rule already moves them; they were never at risk.
//!
//! So implementing Easter would mean an anonymous-Gregorian computus and five
//! derived dates in the production path, to change the answer on three days
//! over the next fifty years — and the cost of being wrong on one of them is a
//! reminder that arrives on a red day rather than the working day after it, on
//! a payload that already says to check with Skatteverket. Complexity with no
//! caller, for now. Stated here so the omission reads as a decision rather
//! than an oversight, and dated so that the decision can be revisited when it
//! stops being true.

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The sentence every emitted deadline payload carries.
///
/// This connector computes a calendar rule; it does not read Skatteverket. The
/// owner's accountant, not this daemon, is the authority on when a
/// momsdeklaration is due, and the reminder exists so that a deadline never
/// arrives unannounced — not so that anybody stops checking.
pub const REMINDER_NOTE: &str = "Reminder only — verify against Skatteverket before filing.";

/// The ordinary day of the month a declaration falls due, given the month it
/// is *filed* in.
///
/// The 12th in ten months of the year and the **17th in January and August**.
/// That is Skatteverket's own exception, not a rounding: those two months
/// follow a long holiday period, and the deklarationsdag moves with it. It
/// cannot be waved away as "erring early", because a deadline is raised once
/// and never re-opened — printing 12 January as the date means the 17th is
/// never announced at all.
pub fn due_day(month: u32) -> u32 {
    match month {
        1 | 8 => 17,
        _ => 12,
    }
}

/// The months a kvartalsmoms declaration is filed in: the **second** month
/// after the quarter ends.
///
/// Q1 → May, Q2 → August, Q3 → November, Q4 → February of the next year. The
/// quarter being declared is therefore the three months ending two months
/// before the filing month.
const MOMS_FILING_MONTHS: &[u32] = &[2, 5, 8, 11];

/// How many months back from a moms filing month the declared quarter ends.
const MOMS_PERIOD_OFFSET: i32 = -2;

/// How far ahead [`crate::watch`] asks for deadlines.
///
/// Forty-five days, against a daily poll: long enough that every due date is
/// seen well before it falls due, so no deadline is ever first reported on the
/// day it is due — even if the daemon was down for a week. The widest gap
/// between two consecutive due dates is 12 December → 17 January, 36 days,
/// and the weekend-and-holiday rule can push the later of the two forward by
/// at most two days (a 17th that is a Saturday becomes the Monday), giving 38.
/// `the_horizon_always_contains_the_next_due_date` measures that bound rather
/// than trusting this paragraph.
pub const HORIZON_DAYS: i64 = 45;

/// Swedish public holidays that fall on a fixed calendar date, as
/// `(month, day)`.
///
/// Julafton (24 December) and nyårsafton (31 December) are not *helgdagar* in
/// law, but Skatteverket, the banks and the Riksbank's payment system treat
/// them as non-banking days, so a declaration due on one is in practice due
/// the next working day. Including them keeps a declaration off a day the
/// owner could not act on anyway, and the move is always forward, so it can
/// never push a reminder past its real deadline.
///
/// Movable feasts are deliberately absent. See the module docs.
pub const FIXED_HOLIDAYS: &[(u32, u32)] = &[
    (1, 1),   // nyårsdagen
    (1, 6),   // trettondedag jul
    (5, 1),   // första maj
    (6, 6),   // nationaldagen
    (12, 24), // julafton (de facto)
    (12, 25), // juldagen
    (12, 26), // annandag jul
    (12, 31), // nyårsafton (de facto)
];

/// Which declaration a [`Deadline`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineKind {
    /// Arbetsgivardeklaration på individnivå — monthly.
    Agi,
    /// Debiterad preliminärskatt — monthly.
    Preliminarskatt,
    /// Momsdeklaration — quarterly here.
    Moms,
}

impl DeadlineKind {
    /// The token used in an external id and in a payload. Stable: it is half
    /// of `tax-deadline:<kind>:<period>`, which the daemon's event store keys
    /// on, so renaming one of these re-opens every deadline as brand new.
    pub fn as_str(self) -> &'static str {
        match self {
            DeadlineKind::Agi => "agi",
            DeadlineKind::Preliminarskatt => "preliminarskatt",
            DeadlineKind::Moms => "moms",
        }
    }

    /// What a person reads. Swedish, because that is what the form is called.
    pub fn label(self) -> &'static str {
        match self {
            DeadlineKind::Agi => "Arbetsgivardeklaration (AGI)",
            DeadlineKind::Preliminarskatt => "Debiterad preliminärskatt",
            DeadlineKind::Moms => "Momsdeklaration (kvartal)",
        }
    }
}

/// One declaration falling due.
///
/// `period` is the period being declared, not the month it is filed in, and is
/// always `YYYY-MM`: for a quarterly moms period it is the quarter's **last**
/// month, so January–March is `2026-03`. `due_on` is already adjusted past
/// weekends and holidays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deadline {
    pub kind: DeadlineKind,
    pub period: String,
    pub due_on: NaiveDate,
}

impl Deadline {
    /// First and last day of the period being declared.
    fn period_span(&self) -> Option<(NaiveDate, NaiveDate)> {
        let end_month = parse_period(&self.period)?;
        let months_back = match self.kind {
            DeadlineKind::Moms => 2,
            _ => 0,
        };
        let start = add_months(first_of(end_month)?, -months_back)?;
        let end = last_of(end_month)?;
        Some((start, end))
    }

    /// What the daemon records, and what a person eventually reads in
    /// Telegram.
    ///
    /// Deliberately carries **no** "days remaining" field. The daemon keys on
    /// `(source, external_id)` and re-opens an event for triage when the
    /// payload changes; a countdown would change every single day, so every
    /// deadline would nag daily from the moment it entered the horizon. The
    /// fields here are all facts about the deadline itself, so a deadline that
    /// has not moved produces a byte-identical payload on every poll.
    pub fn payload(&self) -> Value {
        let (start, end) = match self.period_span() {
            Some((start, end)) => (Value::from(start.to_string()), Value::from(end.to_string())),
            None => (Value::Null, Value::Null),
        };
        json!({
            "kind": self.kind.as_str(),
            "label": self.kind.label(),
            "period": self.period,
            "period_start": start,
            "period_end": end,
            "due_on": self.due_on.to_string(),
            "note": REMINDER_NOTE,
        })
    }
}

/// The next ordinary working day on or after `date`.
///
/// Weekends and the fixed Swedish holidays in [`FIXED_HOLIDAYS`] move a due
/// date **forward**, never backward, and the move repeats: 25 December 2026 is
/// a Friday, 26 December is annandag jul *and* a Saturday, 27 December is a
/// Sunday, so the answer is Monday 28 December.
///
/// A date that is already a working day is returned unchanged.
pub fn adjust_for_weekend_and_holidays(date: NaiveDate) -> NaiveDate {
    let mut date = date;
    // The longest possible run of consecutive non-working days under this
    // holiday set is the turn of the year (Thu 31 Dec, Fri 1 Jan, Sat, Sun =
    // four), so this terminates well inside the bound. The bound exists only
    // so that a future edit adding a holiday per day could not hang a poll.
    for _ in 0..14 {
        if is_working_day(date) {
            return date;
        }
        date += Duration::days(1);
    }
    date
}

/// Neither a weekend nor a fixed Swedish holiday.
pub fn is_working_day(date: NaiveDate) -> bool {
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
        && !FIXED_HOLIDAYS.contains(&(date.month(), date.day()))
}

/// Every declaration falling due in `[from, from + horizon_days]`, earliest
/// first.
///
/// Both ends are inclusive: a deadline falling exactly today is still a
/// deadline, and a horizon of zero days from the day something is due reports
/// it. A negative horizon is an empty answer rather than a panic — the caller
/// is the daemon's timer, and a clock that has gone backwards should not take
/// the poll down.
///
/// Only the adjusted dates are compared against the horizon, so a 12th that
/// is pushed past the end of the window is correctly absent from it (and will
/// be reported on the next poll that reaches it).
pub fn upcoming_deadlines(from: NaiveDate, horizon_days: i64) -> Vec<Deadline> {
    if horizon_days < 0 {
        return Vec::new();
    }
    let Some(until) = from.checked_add_signed(Duration::days(horizon_days)) else {
        return Vec::new();
    };

    let mut found = Vec::new();

    // The adjustment can only push a date forward, and never by more than a
    // handful of days, so a due day always stays inside its own month (17 → at
    // most 23). Walking the months the window touches is therefore enough.
    let Some(mut month) = first_of((from.year(), from.month())) else {
        return Vec::new();
    };
    while month <= until {
        if let Some(due) =
            NaiveDate::from_ymd_opt(month.year(), month.month(), due_day(month.month()))
        {
            let due = adjust_for_weekend_and_holidays(due);
            if due >= from && due <= until {
                // The period declared on the due day of month M is month M-1.
                if let Some(period) = period_label(add_months(month, -1)) {
                    found.push(Deadline {
                        kind: DeadlineKind::Agi,
                        period: period.clone(),
                        due_on: due,
                    });
                    found.push(Deadline {
                        kind: DeadlineKind::Preliminarskatt,
                        period,
                        due_on: due,
                    });
                }
                // Moms is quarterly, filed on the due day of the *second*
                // month after the quarter ends — February, May, August and
                // November — for the quarter whose last month is two months
                // before that.
                if MOMS_FILING_MONTHS.contains(&month.month()) {
                    if let Some(period) = period_label(add_months(month, MOMS_PERIOD_OFFSET)) {
                        found.push(Deadline {
                            kind: DeadlineKind::Moms,
                            period,
                            due_on: due,
                        });
                    }
                }
            }
        }
        match add_months(month, 1) {
            Some(next) => month = next,
            None => break,
        }
    }

    found.sort_by(|a, b| a.due_on.cmp(&b.due_on).then(a.kind.cmp(&b.kind)));
    found
}

// ---------------------------------------------------------------------------
// Month arithmetic
// ---------------------------------------------------------------------------

fn first_of((year, month): (i32, u32)) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, 1)
}

fn last_of((year, month): (i32, u32)) -> Option<NaiveDate> {
    let first_of_next = add_months(first_of((year, month))?, 1)?;
    first_of_next.checked_sub_signed(Duration::days(1))
}

/// `date`, moved `delta` whole months, landing on the first of that month.
fn add_months(date: NaiveDate, delta: i32) -> Option<NaiveDate> {
    let zero_based = date
        .year()
        .checked_mul(12)?
        .checked_add(date.month0() as i32)?;
    let moved = zero_based.checked_add(delta)?;
    NaiveDate::from_ymd_opt(moved.div_euclid(12), moved.rem_euclid(12) as u32 + 1, 1)
}

fn period_label(date: Option<NaiveDate>) -> Option<String> {
    date.map(|date| format!("{:04}-{:02}", date.year(), date.month()))
}

fn parse_period(period: &str) -> Option<(i32, u32)> {
    let (year, month) = period.split_once('-')?;
    Some((year.parse().ok()?, month.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use std::collections::BTreeSet;

    fn date(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    // --- adjust_for_weekend_and_holidays ----------------------------------

    /// Every date the brief names, with the weekday it claims re-derived from
    /// `chrono` rather than taken on trust. A wrong premise here would make
    /// every assertion below vacuous.
    #[test]
    fn the_premises_hold_a_weekday_at_a_time() {
        for (text, weekday) in [
            ("2026-09-24", Weekday::Thu),
            ("2026-08-15", Weekday::Sat),
            ("2026-08-17", Weekday::Mon),
            ("2026-11-15", Weekday::Sun),
            ("2026-11-16", Weekday::Mon),
            ("2027-01-01", Weekday::Fri),
            ("2027-01-04", Weekday::Mon),
            ("2027-01-06", Weekday::Wed),
            ("2027-01-07", Weekday::Thu),
            ("2026-12-25", Weekday::Fri),
            ("2026-12-28", Weekday::Mon),
            // The four kvartalsmoms dates pinned below, so that
            // `the_four_quarters_file_two_months_after_they_end` is asserting
            // the rule and not accidentally asserting a weekend adjustment.
            ("2026-05-12", Weekday::Tue),
            ("2026-08-17", Weekday::Mon),
            ("2026-11-12", Weekday::Thu),
            ("2027-02-12", Weekday::Fri),
        ] {
            assert_eq!(date(text).weekday(), weekday, "{text}");
        }
    }

    #[test]
    fn a_working_weekday_is_returned_unchanged() {
        let thursday = date("2026-09-24");
        assert_eq!(adjust_for_weekend_and_holidays(thursday), thursday);
    }

    #[test]
    fn a_saturday_moves_to_the_monday() {
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2026-08-15")),
            date("2026-08-17")
        );
    }

    #[test]
    fn a_sunday_moves_to_the_monday() {
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2026-11-15")),
            date("2026-11-16")
        );
    }

    /// Nyårsdagen 2027 is a Friday, so moving past it lands in the weekend and
    /// keeps going: the answer is the Monday.
    #[test]
    fn a_holiday_on_a_friday_moves_past_the_weekend_too() {
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2027-01-01")),
            date("2027-01-04")
        );
    }

    /// Trettondedag jul 2027 is a Wednesday: one day is enough.
    #[test]
    fn a_holiday_mid_week_moves_exactly_one_day() {
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2027-01-06")),
            date("2027-01-07")
        );
    }

    /// Juldagen 2026 is a Friday, annandag jul the Saturday, then a Sunday.
    /// Three non-working days in a row, and the answer is past all of them.
    #[test]
    fn a_holiday_followed_by_a_weekend_moves_past_all_three() {
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2026-12-25")),
            date("2026-12-28")
        );
    }

    /// The turn of the year is the longest run this holiday set can produce,
    /// and it must not run off the end of the loop bound.
    #[test]
    fn the_longest_run_of_red_days_still_terminates_on_a_working_day() {
        // Thu 31 Dec 2026 (nyårsafton), Fri 1 Jan 2027 (nyårsdagen), Sat, Sun.
        assert_eq!(date("2026-12-31").weekday(), Weekday::Thu);
        assert_eq!(
            adjust_for_weekend_and_holidays(date("2026-12-31")),
            date("2027-01-04")
        );
    }

    #[test]
    fn adjustment_is_idempotent() {
        for offset in 0..400 {
            let start = date("2026-01-01") + Duration::days(offset);
            let once = adjust_for_weekend_and_holidays(start);
            assert_eq!(adjust_for_weekend_and_holidays(once), once, "{start}");
            assert!(once >= start, "the adjustment never moves backwards");
        }
    }

    // --- upcoming_deadlines -----------------------------------------------

    #[test]
    fn results_fall_inside_the_horizon() {
        let from = date("2026-09-24");
        for horizon in [0, 1, 7, 30, 45, 120, 400] {
            let until = from + Duration::days(horizon);
            for deadline in upcoming_deadlines(from, horizon) {
                assert!(
                    deadline.due_on >= from && deadline.due_on <= until,
                    "{deadline:?} is outside [{from}, {until}]"
                );
            }
        }
    }

    /// 24 September 2026 is a Thursday with nothing due on it, so a window of
    /// exactly one day is empty.
    #[test]
    fn a_zero_day_horizon_from_a_clear_day_returns_nothing() {
        assert!(upcoming_deadlines(date("2026-09-24"), 0).is_empty());
    }

    /// The other half of the same property: a zero-day horizon on a day
    /// something *is* due reports it, so the emptiness above is about the day
    /// and not about the horizon arithmetic quietly excluding both ends.
    #[test]
    fn a_zero_day_horizon_on_a_due_day_reports_it() {
        // 12 November 2026 is a Thursday, and November files the July–September
        // quarter as well as October's monthly pair.
        let due = date("2026-11-12");
        assert_eq!(due.weekday(), Weekday::Thu);
        let found = upcoming_deadlines(due, 0);
        assert_eq!(
            found.iter().map(|d| d.kind).collect::<Vec<_>>(),
            vec![
                DeadlineKind::Agi,
                DeadlineKind::Preliminarskatt,
                DeadlineKind::Moms
            ],
            "{found:?}"
        );
        assert!(found.iter().all(|d| d.due_on == due));

        // A month that is not a moms filing month gives the monthly pair only.
        let october = upcoming_deadlines(date("2026-10-12"), 0);
        assert_eq!(october.len(), 2, "{october:?}");
    }

    /// **Finding 1.** Kvartalsmoms is filed on the ordinary due day of the
    /// *second* month after the quarter ends, and these are the four real
    /// dates. Under the old first-month-after rule every one of these would
    /// be a month early — and because the payload has no clock in it and the
    /// id is `tax-deadline:moms:<period>`, the row is raised once and never
    /// re-opened, so an early date is not an early reminder: it is a reminder
    /// for a day that does not exist, then silence through the real one.
    #[test]
    fn the_four_quarters_file_two_months_after_they_end() {
        // None of these four needs a weekend or holiday adjustment — pinned by
        // `the_premises_hold_a_weekday_at_a_time` — so the date asserted here
        // is the rule's own answer.
        for (period, due) in [
            ("2026-03", "2026-05-12"), // Q1 → 12 May
            ("2026-06", "2026-08-17"), // Q2 → 17 August (August is a 17th)
            ("2026-09", "2026-11-12"), // Q3 → 12 November
            ("2026-12", "2027-02-12"), // Q4 → 12 February of the next year
        ] {
            let due = date(due);
            let moms: Vec<Deadline> = upcoming_deadlines(due, 0)
                .into_iter()
                .filter(|d| d.kind == DeadlineKind::Moms)
                .collect();
            assert_eq!(
                moms.len(),
                1,
                "exactly one moms filing falls due on {due}: {moms:?}"
            );
            assert_eq!(moms[0].period, period, "{moms:?}");
            assert_eq!(moms[0].due_on, due);
        }
    }

    /// The other half of the same rule: the months that are *not* moms filing
    /// months. If the filing month were still the first after the quarter,
    /// January, April, July and October would carry a moms row.
    #[test]
    fn no_moms_falls_due_in_the_month_after_a_quarter_ends() {
        for due in ["2026-04-13", "2026-07-13", "2026-10-12", "2027-01-18"] {
            let found = upcoming_deadlines(date(due), 0);
            assert!(!found.is_empty(), "{due} is a due day");
            assert!(
                found.iter().all(|d| d.kind != DeadlineKind::Moms),
                "{due} is the month straight after a quarter, not a filing month: {found:?}"
            );
        }
    }

    /// **Finding 2.** January and August fall due on the 17th, every other
    /// month on the 12th.
    #[test]
    fn january_and_august_fall_due_on_the_seventeenth() {
        for month in 1..=12 {
            let expected = if matches!(month, 1 | 8) { 17 } else { 12 };
            assert_eq!(due_day(month), expected, "month {month}");
        }

        // And it reaches the emitted dates, not just the helper. 17 August
        // 2026 is a Monday and 12 August 2026 is a Wednesday, so the 17th here
        // cannot be a weekend adjustment of the 12th.
        assert_eq!(date("2026-08-12").weekday(), Weekday::Wed);
        let august = upcoming_deadlines(date("2026-08-01"), 20);
        assert!(!august.is_empty());
        for deadline in &august {
            assert_eq!(deadline.due_on, date("2026-08-17"), "{deadline:?}");
        }

        // January 2026: the 17th is a Saturday, so the answer is the Monday —
        // the 19th, which is still unreachable from a 12th.
        assert_eq!(date("2026-01-17").weekday(), Weekday::Sat);
        let january = upcoming_deadlines(date("2026-01-01"), 25);
        assert!(!january.is_empty());
        for deadline in &january {
            assert_eq!(deadline.due_on, date("2026-01-19"), "{deadline:?}");
        }
    }

    /// The horizon has to be wide enough that a daily poll can never step over
    /// a due date. Reasoned in [`HORIZON_DAYS`]'s doc comment; measured here.
    #[test]
    fn the_horizon_always_contains_the_next_due_date() {
        let mut dates: Vec<NaiveDate> = Vec::new();
        for offset in 0..(366 * 6) {
            let from = date("2026-01-01") + Duration::days(offset);
            if let Some(next) = upcoming_deadlines(from, HORIZON_DAYS).first() {
                dates.push(next.due_on);
            } else {
                panic!("no deadline within {HORIZON_DAYS} days of {from}");
            }
        }
        dates.dedup();
        let widest = dates
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).num_days())
            .max()
            .expect("at least two due dates in six years");
        assert!(
            widest <= HORIZON_DAYS,
            "consecutive due dates are up to {widest} days apart, \
             which a {HORIZON_DAYS}-day horizon cannot always span"
        );
        assert_eq!(
            widest, 38,
            "the widest is 12 July 2030 to 17 August 2030 pushed to the Monday 19th"
        );
    }

    #[test]
    fn a_negative_horizon_is_empty_rather_than_a_panic() {
        assert!(upcoming_deadlines(date("2026-09-24"), -1).is_empty());
        assert!(upcoming_deadlines(date("2026-09-24"), -10_000).is_empty());
    }

    #[test]
    fn every_period_label_is_year_dash_month() {
        for deadline in upcoming_deadlines(date("2026-01-01"), 800) {
            let period = &deadline.period;
            let (year, month) = period.split_once('-').unwrap_or_else(|| {
                panic!("period {period:?} is not YYYY-MM");
            });
            assert_eq!(year.len(), 4, "period {period:?}");
            assert_eq!(month.len(), 2, "period {period:?}");
            assert!(year.chars().all(|c| c.is_ascii_digit()), "{period:?}");
            assert!(month.chars().all(|c| c.is_ascii_digit()), "{period:?}");
            let month: u32 = month.parse().expect("a month");
            assert!((1..=12).contains(&month), "period {period:?}");
        }
    }

    /// **The sweep.** Every start date across a full year, a horizon that
    /// covers the next one, and not one returned deadline on a Saturday or a
    /// Sunday. This is the assertion the whole weekend-and-holiday rule exists
    /// to satisfy, and it is checked over the results rather than over
    /// hand-picked dates.
    #[test]
    fn no_deadline_in_a_whole_year_ever_falls_on_a_weekend() {
        let mut seen = 0;
        for offset in 0..366 {
            let from = date("2026-01-01") + Duration::days(offset);
            for deadline in upcoming_deadlines(from, 366) {
                assert!(
                    !matches!(deadline.due_on.weekday(), Weekday::Sat | Weekday::Sun),
                    "{:?} falls on a {:?}",
                    deadline,
                    deadline.due_on.weekday()
                );
                assert!(
                    is_working_day(deadline.due_on),
                    "{deadline:?} falls on a Swedish holiday"
                );
                seen += 1;
            }
        }
        assert!(seen > 5_000, "the sweep must actually have swept: {seen}");
    }

    /// Twelve monthly pairs and four quarterly moms filings in a calendar
    /// year, no more and no fewer.
    #[test]
    fn a_calendar_year_holds_twelve_monthly_pairs_and_four_moms_filings() {
        let found = upcoming_deadlines(date("2027-01-01"), 364);
        assert_eq!(
            found.iter().filter(|d| d.kind == DeadlineKind::Agi).count(),
            12
        );
        assert_eq!(
            found
                .iter()
                .filter(|d| d.kind == DeadlineKind::Preliminarskatt)
                .count(),
            12
        );
        let moms: Vec<&str> = found
            .iter()
            .filter(|d| d.kind == DeadlineKind::Moms)
            .map(|d| d.period.as_str())
            .collect();
        assert_eq!(moms, ["2026-12", "2027-03", "2027-06", "2027-09"]);
    }

    /// The period declared is the month before the filing month, which is what
    /// makes the id stable and the payload readable.
    #[test]
    fn the_period_is_the_month_before_the_filing_month() {
        let april = upcoming_deadlines(date("2026-04-01"), 20);
        assert!(!april.is_empty());
        for deadline in &april {
            assert_eq!(deadline.period, "2026-03", "{deadline:?}");
            assert_eq!(
                deadline.due_on,
                date("2026-04-13"),
                "12 April 2026 is a Sunday"
            );
        }
        // January files December of the previous year.
        let january = upcoming_deadlines(date("2027-01-01"), 20);
        assert!(january.iter().all(|d| d.period == "2026-12"), "{january:?}");
    }

    #[test]
    fn a_moms_period_spans_its_whole_quarter() {
        let deadline = Deadline {
            kind: DeadlineKind::Moms,
            period: "2026-03".to_string(),
            due_on: date("2026-04-13"),
        };
        let payload = deadline.payload();
        assert_eq!(payload["period_start"], "2026-01-01");
        assert_eq!(payload["period_end"], "2026-03-31");
    }

    #[test]
    fn a_monthly_period_spans_one_month() {
        let deadline = Deadline {
            kind: DeadlineKind::Agi,
            period: "2026-02".to_string(),
            due_on: date("2026-03-12"),
        };
        let payload = deadline.payload();
        assert_eq!(payload["period_start"], "2026-02-01");
        assert_eq!(
            payload["period_end"], "2026-02-28",
            "2026 is not a leap year"
        );
    }

    // --- the payload ------------------------------------------------------

    #[test]
    fn every_emitted_payload_carries_the_reminder_note() {
        let found = upcoming_deadlines(date("2026-01-01"), 400);
        assert!(!found.is_empty());
        for deadline in found {
            let payload = deadline.payload();
            assert_eq!(
                payload["note"], REMINDER_NOTE,
                "a deadline payload must say it is a reminder: {payload}"
            );
            assert_eq!(payload["due_on"], deadline.due_on.to_string());
            assert_eq!(payload["period"], deadline.period);
        }
    }

    /// The payload must be a function of the deadline alone. If it ever picks
    /// up a countdown, the daemon re-opens every deadline for triage every
    /// single day it sits in the horizon.
    #[test]
    fn the_same_deadline_produces_a_byte_identical_payload_whenever_it_is_polled() {
        let deadline = Deadline {
            kind: DeadlineKind::Moms,
            period: "2026-09".to_string(),
            due_on: date("2026-10-12"),
        };
        let once = deadline.payload().to_string();
        let again = deadline.payload().to_string();
        assert_eq!(once, again);
        assert!(
            !once.contains("days"),
            "a countdown in the payload nags daily: {once}"
        );
    }

    /// Two deadlines due on the same day must still be two rows, because their
    /// kinds differ. The daemon keys on the id, so a collision would silently
    /// drop one of them.
    #[test]
    fn the_three_kinds_have_distinct_tokens() {
        let tokens: BTreeSet<&str> = [
            DeadlineKind::Agi,
            DeadlineKind::Preliminarskatt,
            DeadlineKind::Moms,
        ]
        .into_iter()
        .map(DeadlineKind::as_str)
        .collect();
        assert_eq!(tokens.len(), 3);
    }

    /// Easter Sunday, anonymous Gregorian computus. **Test code only**, and
    /// deliberately so: it exists to measure the claim the production path
    /// makes by omission, not to become part of it.
    fn easter_sunday(year: i32) -> NaiveDate {
        let a = year % 19;
        let b = year / 100;
        let c = year % 100;
        let d = b / 4;
        let e = b % 4;
        let f = (b + 8) / 25;
        let g = (b - f + 1) / 3;
        let h = (19 * a + b - d - g + 15) % 30;
        let i = c / 4;
        let k = c % 4;
        let l = (32 + 2 * e + 2 * i - h - k) % 7;
        let m = (a + 11 * h + 22 * l) / 451;
        let month = ((h + l - 7 * m + 114) / 31) as u32;
        let day = ((h + l - 7 * m + 114) % 31 + 1) as u32;
        NaiveDate::from_ymd_opt(year, month, day).expect("a real Easter")
    }

    /// The year the decision to omit the movable feasts runs out: långfredagen
    /// falls on 12 April 2047, which this module will treat as an ordinary
    /// Friday.
    const EASTER_OMISSION_EXPIRES: i32 = 2047;

    /// The verdict on the omission **as of a given year**, so that the test
    /// below can be driven by the real clock and a second test can drive it
    /// past the threshold without waiting twenty-one years.
    ///
    /// `Err` means the decision recorded in the module docs no longer holds:
    /// either the window has closed, or a feast has turned up inside it.
    fn easter_omission_verdict(today_year: i32) -> Result<(), String> {
        if today_year >= EASTER_OMISSION_EXPIRES {
            return Err(format!(
                "it is {today_year}: the decision to omit the movable feasts expired in \
                 {EASTER_OMISSION_EXPIRES}, when långfredagen falls on 12 April. Read the \
                 module docs and choose again — implement the computus, or re-date this."
            ));
        }

        let mut collisions = Vec::new();
        for year in today_year..EASTER_OMISSION_EXPIRES {
            let easter = easter_sunday(year);
            for offset in [-2, 0, 1, 39, 49] {
                let feast = easter + Duration::days(offset);
                if feast.day() == due_day(feast.month()) {
                    collisions.push((year, offset, feast));
                }
            }
            // Midsommarafton: the Friday between 19 and 25 June.
            for day in 19..=25 {
                let candidate = NaiveDate::from_ymd_opt(year, 6, day).expect("a June day");
                if candidate.weekday() == Weekday::Fri {
                    let midsommardagen = candidate + Duration::days(1);
                    if candidate.day() == due_day(candidate.month()) {
                        collisions.push((year, 100, candidate));
                    }
                    if midsommardagen.day() == due_day(midsommardagen.month()) {
                        collisions.push((year, 101, midsommardagen));
                    }
                }
            }
        }
        if !collisions.is_empty() {
            return Err(format!(
                "a movable feast falls on a due day before {EASTER_OMISSION_EXPIRES}: \
                 {collisions:?}"
            ));
        }
        Ok(())
    }

    /// The omission of the movable feasts, measured rather than asserted — and
    /// **against the real clock**, so that the claim in the module docs that
    /// this "fails the day the window closes" is true rather than decorative.
    ///
    /// The usual justification — "they never land on a due day" — is false,
    /// and this test says exactly when it stops being true: the first movable
    /// Swedish feast to fall on one is långfredagen on 12 April 2047. An
    /// earlier version of this test swept a hardcoded `2026..2047` and never
    /// read a clock, so it would have passed unchanged in 2047 and in 2100.
    #[test]
    fn the_omission_of_easter_is_safe_until_2047() {
        let this_year = Utc::now().year();
        if let Err(why) = easter_omission_verdict(this_year) {
            panic!("{why}");
        }
    }

    /// And the proof that the test above can actually fail: the same verdict,
    /// asked with the clock moved past the threshold. Without this, "it will
    /// fail one day" is an untested claim about a test.
    #[test]
    fn the_easter_expiry_really_expires() {
        assert!(
            easter_omission_verdict(EASTER_OMISSION_EXPIRES - 1).is_ok(),
            "the last year inside the window must still pass"
        );
        let expired = easter_omission_verdict(EASTER_OMISSION_EXPIRES)
            .expect_err("the threshold year must fail, or the expiry is decoration");
        assert!(expired.contains("12 April"), "{expired}");
        assert!(
            easter_omission_verdict(2100).is_err(),
            "and it must stay failed, not lapse back into passing"
        );

        // The date the expiry is about, so it is not a guess.
        let langfredagen_2047 = easter_sunday(EASTER_OMISSION_EXPIRES) - Duration::days(2);
        assert_eq!(langfredagen_2047, date("2047-04-12"));
        assert_eq!(langfredagen_2047.weekday(), Weekday::Fri);
        assert_eq!(langfredagen_2047.day(), due_day(langfredagen_2047.month()));
        assert!(
            is_working_day(langfredagen_2047),
            "this connector will treat 12 April 2047 as an ordinary Friday, and that is \
             the known, dated cost of omitting the computus"
        );
    }

    /// The omission recorded in the module docs, pinned: this set is fixed
    /// dates only. Adding a movable feast here would not work — the entries
    /// are `(month, day)` pairs compared against every year.
    #[test]
    fn the_holiday_set_is_fixed_dates_only() {
        assert_eq!(FIXED_HOLIDAYS.len(), 8);
        for &(month, day) in FIXED_HOLIDAYS {
            assert!((1..=12).contains(&month));
            assert!(NaiveDate::from_ymd_opt(2026, month, day).is_some());
            assert!(
                day != due_day(month),
                "a fixed holiday on that month's due day would move every deadline in it"
            );
        }
    }
}
