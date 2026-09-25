use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub surface: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct ConversationStore {
    pool: PgPool,
}

impl ConversationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The id of the conversation in progress, creating one if there is none.
    ///
    /// The check and the insert have to be one atomic unit or two callers
    /// arriving at an empty table each create a conversation. There is no row
    /// to lock when the table is empty, so row-level locking cannot help; a
    /// transaction-scoped advisory lock on a fixed key is the mechanism that
    /// works on the empty case. It is released by the commit, and it is taken
    /// on this one path only.
    pub async fn current(&self) -> anyhow::Result<i64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ea.conversations.current'))")
            .execute(&mut *tx)
            .await
            .context("taking the conversation lock")?;

        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM conversations ORDER BY id DESC LIMIT 1")
                .fetch_optional(&mut *tx)
                .await
                .context("reading the current conversation")?;

        let id = match existing {
            Some(id) => id,
            None => sqlx::query_scalar("INSERT INTO conversations DEFAULT VALUES RETURNING id")
                .fetch_one(&mut *tx)
                .await
                .context("opening a conversation")?,
        };
        tx.commit().await?;
        Ok(id)
    }

    pub async fn append(
        &self,
        conversation_id: i64,
        role: &str,
        surface: &str,
        body: &str,
    ) -> anyhow::Result<Message> {
        sqlx::query_as::<_, Message>(
            "INSERT INTO messages (conversation_id, role, surface, body)
             VALUES ($1,$2,$3,$4)
             RETURNING id, conversation_id, role, surface, body, created_at",
        )
        .bind(conversation_id)
        .bind(role)
        .bind(surface)
        .bind(body)
        .fetch_one(&self.pool)
        .await
        .context("appending a message")
    }

    /// The last `limit` messages in this conversation, oldest first.
    pub async fn recent(&self, conversation_id: i64, limit: i64) -> anyhow::Result<Vec<Message>> {
        let mut messages = sqlx::query_as::<_, Message>(
            "SELECT id, conversation_id, role, surface, body, created_at
             FROM messages WHERE conversation_id = $1 ORDER BY id DESC LIMIT $2",
        )
        .bind(conversation_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("reading recent messages")?;
        messages.reverse();
        Ok(messages)
    }

    pub async fn claude_session(&self, id: i64) -> anyhow::Result<Option<String>> {
        let session: Option<Option<String>> =
            sqlx::query_scalar("SELECT claude_session FROM conversations WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .context("reading the claude session id")?;
        Ok(session.flatten())
    }

    pub async fn set_claude_session(&self, id: i64, session: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE conversations SET claude_session = $2 WHERE id = $1")
            .bind(id)
            .bind(session)
            .execute(&self.pool)
            .await
            .context("setting the claude session id")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn current_creates_a_conversation_then_reuses_it(pool: sqlx::PgPool) {
        let store = ConversationStore::new(pool);
        let first = store.current().await.unwrap();
        let second = store.current().await.unwrap();
        assert_eq!(first, second);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn both_surfaces_interleave_in_order(pool: sqlx::PgPool) {
        let store = ConversationStore::new(pool);
        let convo = store.current().await.unwrap();
        store
            .append(convo, "user", "telegram", "hi")
            .await
            .unwrap();
        store
            .append(convo, "assistant", "cli", "hello there")
            .await
            .unwrap();
        store
            .append(convo, "user", "telegram", "what's due today?")
            .await
            .unwrap();

        let recent = store.recent(convo, 10).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].body, "hi");
        assert_eq!(recent[0].surface, "telegram");
        assert_eq!(recent[1].body, "hello there");
        assert_eq!(recent[1].surface, "cli");
        assert_eq!(recent[2].body, "what's due today?");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn recent_returns_the_last_n_chronologically(pool: sqlx::PgPool) {
        let store = ConversationStore::new(pool);
        let convo = store.current().await.unwrap();
        for i in 0..5 {
            store
                .append(convo, "user", "cli", &format!("message {i}"))
                .await
                .unwrap();
        }
        let recent = store.recent(convo, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].body, "message 3");
        assert_eq!(recent[1].body, "message 4");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_stored_session_id_round_trips(pool: sqlx::PgPool) {
        let store = ConversationStore::new(pool);
        let convo = store.current().await.unwrap();
        assert_eq!(store.claude_session(convo).await.unwrap(), None);
        store
            .set_claude_session(convo, "sess-abc123")
            .await
            .unwrap();
        assert_eq!(
            store.claude_session(convo).await.unwrap().as_deref(),
            Some("sess-abc123")
        );
    }

    /// `current` was a SELECT-then-INSERT, atomic only because one global
    /// mutex serialized the whole daemon. Without it, two callers arriving at
    /// an empty table would each insert, and `ea chat` would silently fork
    /// into two conversations.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn concurrent_callers_agree_on_one_conversation(pool: sqlx::PgPool) {
        let store = ConversationStore::new(pool.clone());
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let store = store.clone();
            set.spawn(async move { store.current().await.unwrap() });
        }
        let mut ids = Vec::new();
        while let Some(id) = set.join_next().await {
            ids.push(id.unwrap());
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 1, "every caller must get the same id, got {ids:?}");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "exactly one conversation row may exist");
    }
}
