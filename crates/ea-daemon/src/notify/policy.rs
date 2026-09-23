//! When an event may interrupt a human, and when it must wait.
//!
//! Three gates, checked in this order, each of which can only hold the
//! notification back:
//!
//! 1. **Threshold.** Below it, the event is not worth an interruption. It is
//!    still worth *knowing*, so it is held for the digest rather than dropped.
//! 2. **Quiet hours.** A wall-clock window in the human's own time zone, which
//!    normally wraps midnight.
//! 3. **Rate limit.** At most `max_per_hour` interruptions in the trailing
//!    hour, counted from the timestamps the caller passes in.
//!
//! Everything here is a pure function of `(salience, now, recent)`. There is no
//! clock read and no stored state, which is what makes the DST behaviour below
//! testable at all.
//!
//! # Why the time zone is a `Tz` and not an offset
//!
//! The obvious implementation stores the human's UTC offset -- `+02:00` for
//! Stockholm -- and adds it to `now`. It is wrong twice a year, in the
//! direction that matters: through the last week of October it would place
//! every event one hour later than it really is locally, so the daemon would go
//! quiet at 21:00 local and stay silent for an hour of the human's evening, and
//! in spring the mirror-image error would have it send at 06:00.
//!
//! [`chrono_tz`] carries the full zone history, so
//! `now.with_timezone(&Europe::Stockholm).time()` is the actual wall clock on
//! the actual date. The offset is *derived* from the zone and the instant,
//! every time, and is never stored. Two tests pin this either side of
//! 2026-10-25, the date Stockholm leaves CEST; a fixed `+02:00` passes the one
//! before and fails both after.

use chrono::{DateTime, Duration, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;

/// Salience at or above which an event may interrupt. A starting value, to be
/// tuned against real batches.
pub const DEFAULT_THRESHOLD: u8 = 60;

/// Interruptions allowed per trailing hour. Also a starting value: the number
/// that matters is the one the human stops resenting.
pub const DEFAULT_MAX_PER_HOUR: usize = 3;

/// The zone the quiet window is expressed in.
pub const DEFAULT_TIME_ZONE: Tz = chrono_tz::Europe::Stockholm;

/// The rate-limit window. Named because "an hour" appears in the type, the
/// config field and the doc comments, and they must not drift apart.
pub const RATE_WINDOW: Duration = Duration::hours(1);

/// How the human wants to be interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    /// Salience at or above which an event may interrupt. The comparison is
    /// `>=`: a threshold of 60 means 60 sends.
    pub threshold: u8,
    /// Start of the quiet window, local wall clock, inclusive.
    pub quiet_start: NaiveTime,
    /// End of the quiet window, local wall clock, exclusive. When it is
    /// *before* `quiet_start` the window wraps midnight, which is the normal
    /// case (22:00 -> 07:00).
    pub quiet_end: NaiveTime,
    pub max_per_hour: usize,
    /// The zone `quiet_start` and `quiet_end` are read in. A zone, never an
    /// offset -- see the module docs.
    pub time_zone: Tz,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            quiet_start: NaiveTime::from_hms_opt(22, 0, 0).expect("22:00 is a time"),
            quiet_end: NaiveTime::from_hms_opt(7, 0, 0).expect("07:00 is a time"),
            max_per_hour: DEFAULT_MAX_PER_HOUR,
            time_zone: DEFAULT_TIME_ZONE,
        }
    }
}

/// What to do with one scored event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Interrupt now.
    pub send: bool,
    /// Hold it for the next digest. Never true at the same time as `send`.
    pub digest: bool,
    /// Why, in words, for the log and for the human who asks "why did I not
    /// hear about that?".
    pub reason: String,
}

impl Verdict {
    fn send(reason: impl Into<String>) -> Self {
        Self {
            send: true,
            digest: false,
            reason: reason.into(),
        }
    }

    fn hold(reason: impl Into<String>) -> Self {
        Self {
            send: false,
            digest: true,
            reason: reason.into(),
        }
    }
}

/// Applies a [`NotifyConfig`]. Holds no state and reads no clock: `now` and the
/// recent-send history are arguments.
#[derive(Debug, Clone, Default)]
pub struct NotificationPolicy {
    config: NotifyConfig,
}

impl NotificationPolicy {
    pub fn new(config: NotifyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &NotifyConfig {
        &self.config
    }

    /// The human's wall-clock time at `now`.
    ///
    /// Derived from the zone and the instant, every call. Never from a stored
    /// offset -- see the module docs.
    pub fn local_time(&self, now: DateTime<Utc>) -> NaiveTime {
        now.with_timezone(&self.config.time_zone).time()
    }

    /// Is `now` inside the quiet window?
    pub fn is_quiet(&self, now: DateTime<Utc>) -> bool {
        let t = self.local_time(now);
        let (start, end) = (self.config.quiet_start, self.config.quiet_end);
        if start > end {
            // The window wraps midnight (22:00 -> 07:00): the normal case.
            t >= start || t < end
        } else {
            t >= start && t < end
        }
    }

    /// How many of `recent` fall inside the trailing hour ending at `now`.
    ///
    /// Strictly newer than `now - 1h`, so a send exactly an hour old has
    /// expired. Timestamps in the future are counted rather than ignored: a
    /// clock that has jumped should make the daemon quieter, not chattier.
    fn recent_sends(&self, now: DateTime<Utc>, recent: &[DateTime<Utc>]) -> usize {
        let cutoff = now - RATE_WINDOW;
        recent.iter().filter(|sent| **sent > cutoff).count()
    }

    /// Decide.
    ///
    /// `recent` is the timestamps of interruptions already delivered; only the
    /// trailing hour of it is looked at, so the caller may pass a longer tail
    /// without filtering it first.
    pub fn evaluate(&self, salience: u8, now: DateTime<Utc>, recent: &[DateTime<Utc>]) -> Verdict {
        if salience < self.config.threshold {
            return Verdict::hold(format!(
                "salience {salience} is below the threshold of {}",
                self.config.threshold
            ));
        }

        if self.is_quiet(now) {
            let local = self.local_time(now);
            return Verdict::hold(format!(
                "quiet hours ({}-{}): local time is {} in {}",
                hhmm(self.config.quiet_start),
                hhmm(self.config.quiet_end),
                hhmm(local),
                self.config.time_zone.name(),
            ));
        }

        let sent = self.recent_sends(now, recent);
        if sent >= self.config.max_per_hour {
            return Verdict::hold(format!(
                "rate limit: {sent} of {} notifications already sent in the last hour",
                self.config.max_per_hour
            ));
        }

        Verdict::send(format!(
            "salience {salience} at or above the threshold of {}, outside quiet hours, \
             {sent} of {} sent in the last hour",
            self.config.threshold, self.config.max_per_hour
        ))
    }
}

fn hhmm(time: NaiveTime) -> String {
    format!("{:02}:{:02}", time.hour(), time.minute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> NotificationPolicy {
        NotificationPolicy::default()
    }

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse()
            .unwrap_or_else(|err| panic!("{text} is not an RFC 3339 instant: {err}"))
    }

    /// 2026-09-24 is a Thursday in CEST, so this is 14:30 in Stockholm.
    fn daytime() -> DateTime<Utc> {
        utc("2026-09-24T12:30:00Z")
    }

    #[test]
    fn the_defaults_are_the_documented_starting_values() {
        let config = NotifyConfig::default();
        assert_eq!(config.threshold, 60);
        assert_eq!(
            config.quiet_start,
            NaiveTime::from_hms_opt(22, 0, 0).unwrap()
        );
        assert_eq!(config.quiet_end, NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert_eq!(config.max_per_hour, 3);
        assert_eq!(config.time_zone, chrono_tz::Europe::Stockholm);
    }

    #[test]
    fn a_high_salience_event_sends_in_the_daytime() {
        let verdict = policy().evaluate(95, daytime(), &[]);
        assert!(verdict.send, "{}", verdict.reason);
        assert!(!verdict.digest);
    }

    #[test]
    fn a_below_threshold_event_is_held_for_the_digest() {
        let verdict = policy().evaluate(59, daytime(), &[]);
        assert!(!verdict.send);
        assert!(verdict.digest, "a quiet event is still worth knowing later");
        assert!(verdict.reason.contains("threshold"), "{}", verdict.reason);
    }

    #[test]
    fn exactly_the_threshold_sends() {
        // The comparison is `>=`, and the boundary is the one thing a
        // threshold gets wrong.
        let verdict = policy().evaluate(60, daytime(), &[]);
        assert!(verdict.send, "{}", verdict.reason);
    }

    #[test]
    fn quiet_hours_hold_even_a_top_score() {
        // 2026-09-24T21:30:00Z is 23:30 in Stockholm (CEST).
        let verdict = policy().evaluate(100, utc("2026-09-24T21:30:00Z"), &[]);
        assert!(!verdict.send);
        assert!(verdict.digest);
        assert!(verdict.reason.contains("quiet"), "{}", verdict.reason);
    }

    #[test]
    fn just_after_quiet_hours_end_it_sends() {
        // 07:00 local is the exclusive end of the window, so 07:01 is awake.
        // 2026-09-25T05:01:00Z is 07:01 in Stockholm (CEST).
        let verdict = policy().evaluate(80, utc("2026-09-25T05:01:00Z"), &[]);
        assert!(verdict.send, "{}", verdict.reason);
        // And one minute earlier is still quiet.
        assert!(!policy().evaluate(80, utc("2026-09-25T04:59:00Z"), &[]).send);
    }

    #[test]
    fn the_quiet_window_boundaries_are_start_inclusive_and_end_exclusive() {
        // 2026-09-24T20:00:00Z is exactly 22:00 in Stockholm: quiet.
        assert!(policy().is_quiet(utc("2026-09-24T20:00:00Z")));
        // 2026-09-24T19:59:00Z is 21:59: awake.
        assert!(!policy().is_quiet(utc("2026-09-24T19:59:00Z")));
        // 2026-09-25T05:00:00Z is exactly 07:00: awake.
        assert!(!policy().is_quiet(utc("2026-09-25T05:00:00Z")));
    }

    #[test]
    fn a_spent_hourly_limit_holds_the_notification() {
        let now = daytime();
        let recent = [
            now - Duration::minutes(5),
            now - Duration::minutes(20),
            now - Duration::minutes(50),
        ];
        let verdict = policy().evaluate(99, now, &recent);
        assert!(!verdict.send);
        assert!(verdict.digest);
        assert!(verdict.reason.contains("rate limit"), "{}", verdict.reason);
    }

    #[test]
    fn one_slot_left_still_sends() {
        let now = daytime();
        let recent = [now - Duration::minutes(5), now - Duration::minutes(50)];
        assert!(policy().evaluate(99, now, &recent).send);
    }

    #[test]
    fn sends_older_than_an_hour_are_ignored() {
        let now = daytime();
        let recent = [
            now - Duration::minutes(61),
            now - Duration::minutes(90),
            now - Duration::hours(6),
            // Exactly an hour old has expired: the window is the trailing hour,
            // not the trailing hour and one instant.
            now - Duration::hours(1),
        ];
        let verdict = policy().evaluate(99, now, &recent);
        assert!(verdict.send, "{}", verdict.reason);
    }

    #[test]
    fn the_threshold_is_checked_before_the_rate_limit() {
        // A held-for-digest event reports why it was held, and "below the
        // threshold" is the more useful answer than "rate limited".
        let now = daytime();
        let recent = [now, now, now];
        let verdict = policy().evaluate(10, now, &recent);
        assert!(verdict.reason.contains("threshold"), "{}", verdict.reason);
    }

    // -- Review Focus #1: DST ------------------------------------------------
    //
    // Stockholm leaves CEST at 03:00 local on 2026-10-25 (01:00 UTC). Each
    // instant below was checked against chrono_tz before the assertion was
    // written; the local times in the comments are what the zone actually
    // reports.

    #[test]
    fn quiet_hours_are_correct_in_cest_before_the_dst_change() {
        // 2026-10-20T20:30:00Z is 22:30 in Stockholm (CEST, +02:00) -- quiet.
        assert!(!policy().evaluate(95, utc("2026-10-20T20:30:00Z"), &[]).send);
    }

    #[test]
    fn quiet_hours_are_correct_in_cet_after_the_dst_change() {
        // 2026-10-27T20:30:00Z is 21:30 in Stockholm (CET, +01:00) -- NOT quiet.
        // A hard-coded +02:00 would read 22:30 here and wrongly go silent.
        assert!(policy().evaluate(95, utc("2026-10-27T20:30:00Z"), &[]).send);
        // 2026-10-27T21:30:00Z is 22:30 in Stockholm (CET) -- quiet.
        assert!(!policy().evaluate(95, utc("2026-10-27T21:30:00Z"), &[]).send);
    }

    #[test]
    fn the_offset_is_derived_from_the_zone_and_the_instant() {
        // The same wall-clock reading, an hour apart in UTC, either side of the
        // change. This is the assertion a stored offset cannot satisfy at all.
        let policy = policy();
        assert_eq!(
            policy.local_time(utc("2026-10-20T20:30:00Z")),
            NaiveTime::from_hms_opt(22, 30, 0).unwrap()
        );
        assert_eq!(
            policy.local_time(utc("2026-10-27T21:30:00Z")),
            NaiveTime::from_hms_opt(22, 30, 0).unwrap()
        );
        assert_eq!(
            policy.local_time(utc("2026-10-27T20:30:00Z")),
            NaiveTime::from_hms_opt(21, 30, 0).unwrap()
        );
    }

    #[test]
    fn the_repeated_local_hour_of_the_dst_change_is_handled_without_panicking() {
        // 02:00-03:00 local happens twice on 2026-10-25. Both instants map to
        // 02:30 local, both are inside the quiet window, and neither is
        // ambiguous in this direction (UTC -> local is always total).
        let policy = policy();
        for instant in ["2026-10-25T00:30:00Z", "2026-10-25T01:30:00Z"] {
            assert_eq!(
                policy.local_time(utc(instant)),
                NaiveTime::from_hms_opt(2, 30, 0).unwrap(),
                "{instant}"
            );
            assert!(policy.is_quiet(utc(instant)), "{instant}");
        }
    }

    #[test]
    fn a_non_wrapping_quiet_window_is_supported() {
        // Not the configuration anyone will use, but `is_quiet` branches on it
        // and an untested branch is a broken branch.
        let config = NotifyConfig {
            quiet_start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            quiet_end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            ..NotifyConfig::default()
        };
        let policy = NotificationPolicy::new(config);
        // 2026-09-24T10:00:00Z is 12:00 in Stockholm: inside 09:00-17:00.
        assert!(policy.is_quiet(utc("2026-09-24T10:00:00Z")));
        // 2026-09-24T18:00:00Z is 20:00: outside.
        assert!(!policy.is_quiet(utc("2026-09-24T18:00:00Z")));
    }

    #[test]
    fn a_different_zone_gets_a_different_answer_for_the_same_instant() {
        // Proof that the zone is actually consulted rather than the process's
        // local time or a constant.
        let stockholm = policy();
        let utc_zone = NotificationPolicy::new(NotifyConfig {
            time_zone: chrono_tz::UTC,
            ..NotifyConfig::default()
        });
        // 2026-09-24T21:30:00Z: 23:30 in Stockholm (quiet), 21:30 in UTC (not).
        let instant = utc("2026-09-24T21:30:00Z");
        assert!(stockholm.is_quiet(instant));
        assert!(!utc_zone.is_quiet(instant));
    }
}
