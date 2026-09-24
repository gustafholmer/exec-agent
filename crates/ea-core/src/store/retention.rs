//! Deleting old rows — the only place in the workspace that does.
//!
//! Everything else here is append-only, which for a daemon meant to run for
//! years is a slow leak rather than a durable audit trail: `events` grows with
//! every poll, `runs` with every session, `messages` with every line of chat.
//! Nothing here is large per row, but nothing ever leaves either, and an
//! unbounded SQLite file on a laptop eventually becomes somebody's problem at
//! the worst moment.
//!
//! # What must never be pruned
//!
//! This module deletes *terminal* rows only, and the exclusions are the whole
//! design:
//!
//! * **An action a human has not decided.** `proposed` and `approved` rows are
//!   never touched at any age. A proposal older than the window has not been
//!   forgotten about — it is *waiting*, and deleting it would silently drop
//!   something the owner was asked to look at. (An expired one is terminal and
//!   is prunable; expiry is a decision the system already made and recorded.)
//! * **The conversation the owner is talking in.** The newest conversation —
//!   the one `ConversationStore::current` returns — is exempt regardless of
//!   age, and so is any conversation with a message inside the window. A quiet
//!   week must not delete the thread the next message continues.
//! * **An untriaged event.** Triage has not looked at it yet; deleting it
//!   would mean the owner never hears about something the system did fetch.
//! * **A `running` run.** An unfinished run row is the only lead on a session
//!   that was killed mid-flight, which is exactly what one wants to find
//!   afterwards.
//!
//! # Why string comparison on the timestamps
//!
//! Every timestamp in this database is written with `Utc::now().to_rfc3339()`,
//! so they are all the same fixed-width UTC form and lexicographic order is
//! chronological order. The cutoff is built the same way. The only imprecision
//! is sub-second (a value with no fractional part sorts just before one with),
//! which does not matter at a granularity of days.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

/// How long each kind of terminal row is kept. Days, because every window here
/// is a human-scale "how far back might I want to look".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Triaged events. 90 days: a term's worth of coursework, which is the
    /// longest span over which "did this ever come in?" is a live question.
    pub events_days: i64,
    /// Finished runs — the model-spend ledger. 90 days, so a quarter of cost
    /// history is always available to look back over.
    pub runs_days: i64,
    /// Terminal actions. 180 days, twice the rest: this is the record of the
    /// things the system actually *did to the world*, and it is the one an
    /// auditor, an accountant, or a puzzled owner comes back to long after.
    pub actions_days: i64,
    /// Conversations, with their messages. 90 days.
    pub conversations_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            events_days: 90,
            runs_days: 90,
            actions_days: 180,
            conversations_days: 90,
        }
    }
}

/// What one pass deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PruneSummary {
    pub events: usize,
    pub runs: usize,
    pub actions: usize,
    pub conversations: usize,
    pub messages: usize,
}

impl PruneSummary {
    pub fn total(&self) -> usize {
        self.events + self.runs + self.actions + self.conversations + self.messages
    }
}

/// Terminal action statuses. `proposed` and `approved` are deliberately
/// absent: see the module docs.
const TERMINAL_ACTION_STATUSES: &str = "('rejected','expired','executed','failed')";

#[derive(Clone)]
pub struct RetentionStore {
    conn: Arc<Mutex<Connection>>,
}

impl RetentionStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Delete everything older than `policy` allows, as one transaction.
    ///
    /// One transaction so a pass is all-or-nothing: a prune interrupted
    /// half-way through would otherwise be able to delete a conversation's
    /// messages and leave the conversation, which is a worse state than either
    /// end of the operation.
    pub fn prune(
        &self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
    ) -> anyhow::Result<PruneSummary> {
        let cutoff = |days: i64| -> String {
            let days = days.max(1);
            (now - Duration::try_days(days).unwrap_or_else(|| Duration::days(1))).to_rfc3339()
        };

        // Conversations: old, not the current one, and with nothing recent in
        // them.
        const STALE: &str = "SELECT c.id FROM conversations c
             WHERE c.created_at < ?1
               AND c.id <> (SELECT MAX(id) FROM conversations)
               AND NOT EXISTS (
                 SELECT 1 FROM messages m
                 WHERE m.conversation_id = c.id AND m.created_at >= ?1
               )";
        let conversation_cutoff = cutoff(policy.conversations_days);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let summary = PruneSummary {
            // Events: triaged only. An untriaged one has not been looked at.
            events: tx.execute(
                "DELETE FROM events WHERE triaged_at IS NOT NULL AND created_at < ?1",
                params![cutoff(policy.events_days)],
            )?,
            // Runs: finished only. A `running` row is an orphan worth keeping.
            runs: tx.execute(
                "DELETE FROM runs WHERE finished_at IS NOT NULL AND started_at < ?1",
                params![cutoff(policy.runs_days)],
            )?,
            // Actions: terminal only. Nothing a human still has to decide.
            actions: tx.execute(
                &format!(
                    "DELETE FROM actions
                     WHERE status IN {TERMINAL_ACTION_STATUSES} AND created_at < ?1"
                ),
                params![cutoff(policy.actions_days)],
            )?,
            // Messages before conversations, because of the foreign key.
            messages: tx.execute(
                &format!("DELETE FROM messages WHERE conversation_id IN ({STALE})"),
                params![conversation_cutoff],
            )?,
            conversations: tx.execute(
                &format!("DELETE FROM conversations WHERE id IN ({STALE})"),
                params![conversation_cutoff],
            )?,
        };

        tx.commit()?;
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    fn ago(days: i64) -> String {
        (Utc::now() - Duration::days(days)).to_rfc3339()
    }

    fn now_str() -> String {
        Utc::now().to_rfc3339()
    }

    fn store() -> (tempfile::TempDir, Arc<Mutex<Connection>>, RetentionStore) {
        let (dir, conn) = temp_store();
        let store = RetentionStore::new(Arc::clone(&conn));
        (dir, conn, store)
    }

    fn count(conn: &Arc<Mutex<Connection>>, table: &str) -> i64 {
        conn.lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn insert_event(conn: &Arc<Mutex<Connection>>, id: &str, created: &str, triaged: Option<&str>) {
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO events (source, external_id, kind, payload, created_at, triaged_at)
                 VALUES ('canvas', ?1, 'assignment', '{}', ?2, ?3)",
                params![id, created, triaged],
            )
            .unwrap();
    }

    fn insert_action(conn: &Arc<Mutex<Connection>>, status: &str, created: &str) {
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO actions
                   (connector, tool, args, preview, rationale, status, created_at, expires_at)
                 VALUES ('canvas','x','{}','p','r',?1,?2,?2)",
                params![status, created],
            )
            .unwrap();
    }

    fn insert_run(conn: &Arc<Mutex<Connection>>, started: &str, finished: Option<&str>) {
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO runs (kind, prompt, outcome, started_at, finished_at)
                 VALUES ('chat','p','ok',?1,?2)",
                params![started, finished],
            )
            .unwrap();
    }

    fn insert_conversation(conn: &Arc<Mutex<Connection>>, created: &str) -> i64 {
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (created_at) VALUES (?1)",
            params![created],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_message(conn: &Arc<Mutex<Connection>>, conversation: i64, created: &str) {
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO messages (conversation_id, role, surface, body, created_at)
                 VALUES (?1,'user','cli','hi',?2)",
                params![conversation, created],
            )
            .unwrap();
    }

    #[test]
    fn an_old_triaged_event_goes_and_an_untriaged_one_stays() {
        let (_dir, conn, store) = store();
        insert_event(&conn, "old-triaged", &ago(200), Some(&ago(199)));
        insert_event(&conn, "old-untriaged", &ago(200), None);
        insert_event(&conn, "recent-triaged", &ago(2), Some(&ago(1)));

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.events, 1);
        assert_eq!(count(&conn, "events"), 2);
        let remaining: Vec<String> = {
            let locked = conn.lock().unwrap();
            let mut stmt = locked
                .prepare("SELECT external_id FROM events ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([], |row| row.get(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(remaining, vec!["old-untriaged", "recent-triaged"]);
    }

    /// The exclusion that matters most: a proposal nobody has decided is not
    /// old data, it is a question still on the table.
    #[test]
    fn an_undecided_action_is_never_pruned_however_old() {
        let (_dir, conn, store) = store();
        insert_action(&conn, "proposed", &ago(1000));
        insert_action(&conn, "approved", &ago(1000));
        insert_action(&conn, "executed", &ago(1000));
        insert_action(&conn, "rejected", &ago(1000));
        insert_action(&conn, "expired", &ago(1000));
        insert_action(&conn, "failed", &ago(1000));
        insert_action(&conn, "executed", &ago(10));

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.actions, 4, "only the four terminal old ones");
        let left: Vec<String> = {
            let locked = conn.lock().unwrap();
            let mut stmt = locked
                .prepare("SELECT status FROM actions ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([], |row| row.get(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(left, vec!["proposed", "approved", "executed"]);
    }

    #[test]
    fn a_running_run_survives_and_an_old_finished_one_does_not() {
        let (_dir, conn, store) = store();
        insert_run(&conn, &ago(200), Some(&ago(200)));
        insert_run(&conn, &ago(200), None);
        insert_run(&conn, &ago(1), Some(&ago(1)));

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.runs, 1);
        assert_eq!(count(&conn, "runs"), 2);
    }

    /// The conversation the owner is currently talking in must survive, even
    /// if it was started a year ago and has been quiet since.
    #[test]
    fn the_current_conversation_survives_at_any_age() {
        let (_dir, conn, store) = store();
        let ancient = insert_conversation(&conn, &ago(400));
        insert_message(&conn, ancient, &ago(400));
        let current = insert_conversation(&conn, &ago(365));
        insert_message(&conn, current, &ago(365));

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.conversations, 1);
        assert_eq!(summary.messages, 1);
        let left: i64 = conn
            .lock()
            .unwrap()
            .query_row("SELECT id FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, current, "the newest conversation is the live one");
    }

    /// An old conversation that is still being used is still being used.
    #[test]
    fn an_old_conversation_with_a_recent_message_survives() {
        let (_dir, conn, store) = store();
        let old_but_active = insert_conversation(&conn, &ago(400));
        insert_message(&conn, old_but_active, &ago(400));
        insert_message(&conn, old_but_active, &now_str());
        // A newer conversation, so `old_but_active` is not saved merely by
        // being the current one.
        insert_conversation(&conn, &now_str());

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.conversations, 0);
        assert_eq!(summary.messages, 0);
        assert_eq!(count(&conn, "conversations"), 2);
    }

    #[test]
    fn a_stale_conversation_takes_its_messages_with_it() {
        let (_dir, conn, store) = store();
        let stale = insert_conversation(&conn, &ago(400));
        for _ in 0..3 {
            insert_message(&conn, stale, &ago(399));
        }
        insert_conversation(&conn, &now_str());

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();

        assert_eq!(summary.conversations, 1);
        assert_eq!(summary.messages, 3);
        assert_eq!(count(&conn, "messages"), 0);
    }

    #[test]
    fn a_fresh_database_loses_nothing_and_says_so() {
        let (_dir, _conn, store) = store();
        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .unwrap();
        assert_eq!(summary, PruneSummary::default());
        assert_eq!(summary.total(), 0);
    }

    /// Running it twice must not delete anything the second time: a prune is a
    /// convergence, not a rolling cost.
    #[test]
    fn a_second_pass_deletes_nothing() {
        let (_dir, conn, store) = store();
        insert_event(&conn, "old", &ago(200), Some(&ago(199)));
        insert_run(&conn, &ago(200), Some(&ago(200)));
        insert_action(&conn, "executed", &ago(1000));

        assert_eq!(
            store
                .prune(&RetentionPolicy::default(), Utc::now())
                .unwrap()
                .total(),
            3
        );
        assert_eq!(
            store
                .prune(&RetentionPolicy::default(), Utc::now())
                .unwrap()
                .total(),
            0
        );
    }
}
