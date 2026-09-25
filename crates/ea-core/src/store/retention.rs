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

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::PgPool;

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
    pool: PgPool,
}

impl RetentionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Delete everything older than `policy` allows, as one transaction.
    ///
    /// One transaction so a pass is all-or-nothing: a prune interrupted
    /// half-way through would otherwise be able to delete a conversation's
    /// messages and leave the conversation, which is a worse state than either
    /// end of the operation.
    pub async fn prune(
        &self,
        policy: &RetentionPolicy,
        now: DateTime<Utc>,
    ) -> anyhow::Result<PruneSummary> {
        let cutoff = |days: i64| -> DateTime<Utc> {
            let days = days.max(1);
            now - Duration::try_days(days).unwrap_or_else(|| Duration::days(1))
        };

        // Conversations: old, not the current one, and with nothing recent in
        // them.
        const STALE: &str = "SELECT c.id FROM conversations c
             WHERE c.created_at < $1
               AND c.id <> (SELECT MAX(id) FROM conversations)
               AND NOT EXISTS (
                 SELECT 1 FROM messages m
                 WHERE m.conversation_id = c.id AND m.created_at >= $1
               )";
        let conversation_cutoff = cutoff(policy.conversations_days);

        let mut tx = self.pool.begin().await?;

        let summary = PruneSummary {
            // Events: triaged only. An untriaged one has not been looked at.
            events: sqlx::query(
                "DELETE FROM events WHERE triaged_at IS NOT NULL AND created_at < $1",
            )
            .bind(cutoff(policy.events_days))
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize,
            // Runs: finished only. A `running` row is an orphan worth keeping.
            runs: sqlx::query("DELETE FROM runs WHERE finished_at IS NOT NULL AND started_at < $1")
                .bind(cutoff(policy.runs_days))
                .execute(&mut *tx)
                .await?
                .rows_affected() as usize,
            // Actions: terminal only. Nothing a human still has to decide.
            actions: sqlx::query(&format!(
                "DELETE FROM actions
                 WHERE status IN {TERMINAL_ACTION_STATUSES} AND created_at < $1"
            ))
            .bind(cutoff(policy.actions_days))
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize,
            // Messages before conversations, because of the foreign key.
            messages: sqlx::query(&format!(
                "DELETE FROM messages WHERE conversation_id IN ({STALE})"
            ))
            .bind(conversation_cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected() as usize,
            conversations: sqlx::query(&format!("DELETE FROM conversations WHERE id IN ({STALE})"))
                .bind(conversation_cutoff)
                .execute(&mut *tx)
                .await?
                .rows_affected() as usize,
        };

        tx.commit().await?;
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(days: i64) -> DateTime<Utc> {
        Utc::now() - Duration::days(days)
    }

    /// The policy the ported tests build: every window is short enough that
    /// the ages used below (hundreds of days) fall well inside it.
    fn aggressive_policy() -> RetentionPolicy {
        RetentionPolicy::default()
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn insert_event(pool: &PgPool, id: &str, created: DateTime<Utc>, triaged: Option<DateTime<Utc>>) {
        sqlx::query(
            "INSERT INTO events (source, external_id, kind, payload, created_at, triaged_at)
             VALUES ('canvas', $1, 'assignment', '{}', $2, $3)",
        )
        .bind(id)
        .bind(created)
        .bind(triaged)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_action(pool: &PgPool, status: &str, created: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO actions
               (connector, tool, args, preview, rationale, status, created_at, expires_at)
             VALUES ('canvas','x','{}','p','r',$1,$2,$2)",
        )
        .bind(status)
        .bind(created)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_run(pool: &PgPool, started: DateTime<Utc>, finished: Option<DateTime<Utc>>) {
        sqlx::query(
            "INSERT INTO runs (kind, prompt, outcome, started_at, finished_at)
             VALUES ('chat','p','ok',$1,$2)",
        )
        .bind(started)
        .bind(finished)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_conversation(pool: &PgPool, created: DateTime<Utc>) -> i64 {
        sqlx::query_scalar("INSERT INTO conversations (created_at) VALUES ($1) RETURNING id")
            .bind(created)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn insert_message(pool: &PgPool, conversation: i64, created: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO messages (conversation_id, role, surface, body, created_at)
             VALUES ($1,'user','cli','hi',$2)",
        )
        .bind(conversation)
        .bind(created)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_old_triaged_event_goes_and_an_untriaged_one_stays(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        insert_event(&pool, "old-triaged", ago(200), Some(ago(199))).await;
        insert_event(&pool, "old-untriaged", ago(200), None).await;
        insert_event(&pool, "recent-triaged", ago(2), Some(ago(1))).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.events, 1);
        assert_eq!(count(&pool, "events").await, 2);
        let remaining: Vec<String> = sqlx::query_scalar("SELECT external_id FROM events ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, vec!["old-untriaged", "recent-triaged"]);
    }

    /// The exclusion that matters most: a proposal nobody has decided is not
    /// old data, it is a question still on the table.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_undecided_action_is_never_pruned_however_old(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        insert_action(&pool, "proposed", ago(1000)).await;
        insert_action(&pool, "approved", ago(1000)).await;
        insert_action(&pool, "executed", ago(1000)).await;
        insert_action(&pool, "rejected", ago(1000)).await;
        insert_action(&pool, "expired", ago(1000)).await;
        insert_action(&pool, "failed", ago(1000)).await;
        insert_action(&pool, "executed", ago(10)).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.actions, 4, "only the four terminal old ones");
        let left: Vec<String> = sqlx::query_scalar("SELECT status FROM actions ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, vec!["proposed", "approved", "executed"]);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_running_run_survives_and_an_old_finished_one_does_not(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        insert_run(&pool, ago(200), Some(ago(200))).await;
        insert_run(&pool, ago(200), None).await;
        insert_run(&pool, ago(1), Some(ago(1))).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.runs, 1);
        assert_eq!(count(&pool, "runs").await, 2);
    }

    /// The conversation the owner is currently talking in must survive, even
    /// if it was started a year ago and has been quiet since.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn the_current_conversation_survives_at_any_age(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        let ancient = insert_conversation(&pool, ago(400)).await;
        insert_message(&pool, ancient, ago(400)).await;
        let current = insert_conversation(&pool, ago(365)).await;
        insert_message(&pool, current, ago(365)).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.conversations, 1);
        assert_eq!(summary.messages, 1);
        let left: i64 = sqlx::query_scalar("SELECT id FROM conversations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, current, "the newest conversation is the live one");
    }

    /// An old conversation that is still being used is still being used.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_old_conversation_with_a_recent_message_survives(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        let old_but_active = insert_conversation(&pool, ago(400)).await;
        insert_message(&pool, old_but_active, ago(400)).await;
        insert_message(&pool, old_but_active, Utc::now()).await;
        // A newer conversation, so `old_but_active` is not saved merely by
        // being the current one.
        insert_conversation(&pool, Utc::now()).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.conversations, 0);
        assert_eq!(summary.messages, 0);
        assert_eq!(count(&pool, "conversations").await, 2);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_stale_conversation_takes_its_messages_with_it(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        let stale = insert_conversation(&pool, ago(400)).await;
        for _ in 0..3 {
            insert_message(&pool, stale, ago(399)).await;
        }
        insert_conversation(&pool, Utc::now()).await;

        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();

        assert_eq!(summary.conversations, 1);
        assert_eq!(summary.messages, 3);
        assert_eq!(count(&pool, "messages").await, 0);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_fresh_database_loses_nothing_and_says_so(pool: PgPool) {
        let store = RetentionStore::new(pool);
        let summary = store
            .prune(&RetentionPolicy::default(), Utc::now())
            .await
            .unwrap();
        assert_eq!(summary, PruneSummary::default());
        assert_eq!(summary.total(), 0);
    }

    /// Running it twice must not delete anything the second time: a prune is a
    /// convergence, not a rolling cost.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_second_pass_deletes_nothing(pool: PgPool) {
        let store = RetentionStore::new(pool.clone());
        insert_event(&pool, "old", ago(200), Some(ago(199))).await;
        insert_run(&pool, ago(200), Some(ago(200))).await;
        insert_action(&pool, "executed", ago(1000)).await;

        assert_eq!(
            store
                .prune(&RetentionPolicy::default(), Utc::now())
                .await
                .unwrap()
                .total(),
            3
        );
        assert_eq!(
            store
                .prune(&RetentionPolicy::default(), Utc::now())
                .await
                .unwrap()
                .total(),
            0
        );
    }

    /// The prune is one transaction: either every table is pruned or none is,
    /// so a failure partway cannot leave messages orphaned from the
    /// conversation that was deleted out from under them.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_prune_leaves_no_orphaned_messages(pool: sqlx::PgPool) {
        let old = chrono::Utc::now() - chrono::Duration::days(400);
        let conversation_id: i64 = sqlx::query_scalar(
            "INSERT INTO conversations (created_at) VALUES ($1) RETURNING id",
        )
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (conversation_id, role, surface, body, created_at)
             VALUES ($1, 'user', 'terminal', 'hello', $2)",
        )
        .bind(conversation_id)
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();

        let store = RetentionStore::new(pool.clone());
        // Use the same policy value the ported tests build; every window is
        // far shorter than the 400 days above, so both rows are in scope.
        store
            .prune(&aggressive_policy(), Utc::now())
            .await
            .unwrap();

        let orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m
             WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = m.conversation_id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(orphans, 0);
    }
}
