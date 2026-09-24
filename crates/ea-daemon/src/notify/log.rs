//! What has already been sent, and what is waiting for the digest.
//!
//! [`NotificationPolicy::evaluate`](crate::notify::policy::NotificationPolicy::evaluate)
//! takes the recent-send history as an argument and keeps no state of its own,
//! which is what makes it testable — and also what makes `max_per_hour`
//! decorative unless *somebody* remembers the sends. This is that somebody.
//!
//! It is durable on purpose. A rate limit held in memory resets every time the
//! daemon restarts, and a daemon under `launchd` with `KeepAlive` restarts
//! whenever it crashes — precisely the situation in which a loop could
//! otherwise machine-gun the owner's phone. The history lives in the `kv`
//! table, so a restart changes nothing.
//!
//! Retention is a day. Only the trailing hour is ever consulted, but keeping a
//! day of it costs nothing and leaves something to look at when someone asks
//! why they were interrupted six times this morning.

use chrono::{DateTime, Duration, Utc};
use ea_core::store::kv::KvStore;

/// `kv` key holding the send history: a JSON array of RFC 3339 instants.
pub const SENT_KEY: &str = "notify.sent";

/// `kv` key holding the digest backlog: a JSON array of lines.
pub const DIGEST_KEY: &str = "notify.digest";

/// How much history is kept. See the module docs.
pub const RETENTION: Duration = Duration::hours(24);

/// Cap on the digest backlog, so a connector that suddenly produces ten
/// thousand low-salience events cannot grow one `kv` row without bound. The
/// oldest lines are dropped first: a digest is a summary, and the newest
/// things in it are the ones still worth acting on.
pub const MAX_DIGEST_LINES: usize = 200;

pub struct NotificationLog {
    kv: KvStore,
}

impl NotificationLog {
    pub fn new(kv: KvStore) -> Self {
        Self { kv }
    }

    /// Every send recorded within [`RETENTION`] of `now`, oldest first.
    ///
    /// This is exactly the slice `NotificationPolicy::evaluate` wants; it does
    /// its own hour-window filtering, so nothing narrower is needed here.
    pub fn recent(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<DateTime<Utc>>> {
        let raw: Vec<String> = self.kv.get_json(SENT_KEY)?;
        let cutoff = now - RETENTION;
        let mut parsed: Vec<DateTime<Utc>> = raw
            .iter()
            .filter_map(|text| {
                text.parse::<DateTime<Utc>>()
                    .map_err(|err| {
                        tracing::warn!(value = %text, error = %err, "dropping unparseable notification timestamp");
                    })
                    .ok()
            })
            .filter(|sent| *sent > cutoff)
            .collect();
        parsed.sort_unstable();
        Ok(parsed)
    }

    /// Record that an interruption was delivered at `at`, pruning anything
    /// older than [`RETENTION`].
    pub fn record(&self, at: DateTime<Utc>) -> anyhow::Result<()> {
        let mut history = self.recent(at)?;
        history.push(at);
        history.sort_unstable();
        let encoded: Vec<String> = history.iter().map(|t| t.to_rfc3339()).collect();
        self.kv.set_json(SENT_KEY, &encoded)
    }

    /// Hold a line for the next digest.
    pub fn push_digest(&self, line: impl Into<String>) -> anyhow::Result<()> {
        let mut lines: Vec<String> = self.kv.get_json(DIGEST_KEY)?;
        lines.push(line.into());
        if lines.len() > MAX_DIGEST_LINES {
            let overflow = lines.len() - MAX_DIGEST_LINES;
            lines.drain(..overflow);
        }
        self.kv.set_json(DIGEST_KEY, &lines)
    }

    /// Everything held for the digest, without clearing it.
    pub fn digest(&self) -> anyhow::Result<Vec<String>> {
        self.kv.get_json(DIGEST_KEY)
    }

    /// Everything held for the digest, clearing it.
    pub fn take_digest(&self) -> anyhow::Result<Vec<String>> {
        let lines = self.digest()?;
        self.kv.set_json(DIGEST_KEY, &Vec::<String>::new())?;
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::notify::policy::NotificationPolicy;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    /// 2026-09-24 14:30 in Stockholm: a weekday afternoon, outside quiet hours.
    fn daytime() -> DateTime<Utc> {
        utc("2026-09-24T12:30:00Z")
    }

    fn store(dir: &TempDir) -> NotificationLog {
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        NotificationLog::new(KvStore::new(conn))
    }

    #[test]
    fn an_empty_log_reads_as_no_sends() {
        let dir = TempDir::new().unwrap();
        assert!(store(&dir).recent(daytime()).unwrap().is_empty());
    }

    #[test]
    fn records_are_returned_oldest_first() {
        let dir = TempDir::new().unwrap();
        let log = store(&dir);
        log.record(utc("2026-09-24T12:20:00Z")).unwrap();
        log.record(utc("2026-09-24T12:00:00Z")).unwrap();
        let recent = log.recent(daytime()).unwrap();
        assert_eq!(
            recent,
            vec![utc("2026-09-24T12:00:00Z"), utc("2026-09-24T12:20:00Z")]
        );
    }

    #[test]
    fn anything_older_than_retention_is_pruned() {
        let dir = TempDir::new().unwrap();
        let log = store(&dir);
        log.record(utc("2026-09-22T12:00:00Z")).unwrap();
        log.record(daytime()).unwrap();
        assert_eq!(log.recent(daytime()).unwrap(), vec![daytime()]);
    }

    /// Addition 3, the whole point: the rate limit has to survive a restart.
    /// The daemon runs under `launchd` with `KeepAlive`, so "the process came
    /// back" is a routine event, not an exotic one — and if the history went
    /// with it, three notifications an hour would become three per crash.
    #[test]
    fn the_rate_limit_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let now = daytime();
        let policy = NotificationPolicy::default(); // max_per_hour = 3

        {
            let log = store(&dir);
            for minutes in [5, 10, 15] {
                log.record(now - Duration::minutes(minutes)).unwrap();
            }
            let verdict = policy.evaluate(90, now, &log.recent(now).unwrap());
            assert!(!verdict.send, "the third send should already be the limit");
        }

        // A brand-new process, a brand-new connection, the same database.
        let reopened = store(&dir);
        let recent = reopened.recent(now).unwrap();
        assert_eq!(recent.len(), 3, "history must survive the restart");
        let verdict = policy.evaluate(90, now, &recent);
        assert!(
            !verdict.send && verdict.digest,
            "a restart must not reset the hourly limit: {}",
            verdict.reason
        );
        assert!(verdict.reason.contains("rate limit"), "{}", verdict.reason);

        // And an hour later the window has genuinely moved on.
        let later = now + Duration::hours(2);
        assert!(
            policy
                .evaluate(90, later, &reopened.recent(later).unwrap())
                .send
        );
    }

    #[test]
    fn the_digest_accumulates_and_drains() {
        let dir = TempDir::new().unwrap();
        let log = store(&dir);
        log.push_digest("one").unwrap();
        log.push_digest("two").unwrap();
        assert_eq!(log.digest().unwrap(), vec!["one", "two"]);
        assert_eq!(log.take_digest().unwrap(), vec!["one", "two"]);
        assert!(log.digest().unwrap().is_empty());
    }

    #[test]
    fn the_digest_is_bounded_and_drops_the_oldest() {
        let dir = TempDir::new().unwrap();
        let log = store(&dir);
        for i in 0..(MAX_DIGEST_LINES + 5) {
            log.push_digest(format!("line {i}")).unwrap();
        }
        let lines = log.digest().unwrap();
        assert_eq!(lines.len(), MAX_DIGEST_LINES);
        assert_eq!(lines[0], "line 5");
        assert_eq!(
            lines[MAX_DIGEST_LINES - 1],
            format!("line {}", MAX_DIGEST_LINES + 4)
        );
    }
}
