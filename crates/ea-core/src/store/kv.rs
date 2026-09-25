//! The `kv` table: small, durable scraps of daemon state that do not deserve
//! a table of their own.
//!
//! What lives here has to survive a restart and is read by exactly one owner
//! -- the notification log's send history and digest backlog, the Telegram
//! update offset. Anything with more than one reader, or any structure worth
//! querying, belongs in a real table instead.

use anyhow::Context;
use sqlx::PgPool;

#[derive(Clone)]
pub struct KvStore {
    pool: PgPool,
}

impl KvStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The value at `key`, or `None` when it has never been set.
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        sqlx::query_scalar("SELECT value FROM kv WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .context("reading a kv entry")
    }

    /// Write `value` at `key`, replacing whatever was there.
    pub async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO kv (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .context("writing a kv entry")?;
        Ok(())
    }

    /// Read a JSON value, falling back to `T::default()` both when the key is
    /// absent and when what is stored no longer parses as a `T`.
    ///
    /// The second half is deliberate. This is bookkeeping, not the ledger: a
    /// shape that changed between releases must not be able to wedge the
    /// daemon at startup, and losing an hour of notification history is a
    /// smaller harm than refusing to run.
    pub async fn get_json<T: serde::de::DeserializeOwned + Default>(
        &self,
        key: &str,
    ) -> anyhow::Result<T> {
        let Some(raw) = self.get(key).await? else {
            return Ok(T::default());
        };
        Ok(serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(key, error = %err, "discarding unparseable kv value");
            T::default()
        }))
    }

    pub async fn set_json<T: serde::Serialize>(&self, key: &str, value: &T) -> anyhow::Result<()> {
        self.set(key, &serde_json::to_string(value)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn set_then_get_round_trips(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        assert_eq!(kv.get("absent").await.unwrap(), None);
        kv.set("k", "v").await.unwrap();
        assert_eq!(kv.get("k").await.unwrap(), Some("v".to_string()));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn set_overwrites_rather_than_erroring(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        kv.set("k", "first").await.unwrap();
        kv.set("k", "second").await.unwrap();
        assert_eq!(kv.get("k").await.unwrap(), Some("second".to_string()));
    }

    /// Review Focus #1, pinned once here as the convention: a timestamp is
    /// equal to what came back only to microsecond resolution.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_timestamp_round_trips_to_microsecond_resolution(pool: sqlx::PgPool) {
        let now = chrono::Utc::now();
        let back: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT $1::timestamptz")
                .bind(now)
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::store::test_support::assert_same_instant(now, back);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn get_json_returns_default_for_a_missing_key(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        let value: Vec<i64> = kv.get_json("missing").await.unwrap();
        assert!(value.is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn set_json_then_get_json_round_trips(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        kv.set_json("ids", &vec![1_i64, 2, 3]).await.unwrap();
        let value: Vec<i64> = kv.get_json("ids").await.unwrap();
        assert_eq!(value, vec![1, 2, 3]);
    }

    /// A value whose shape changed between releases must degrade to the
    /// default, not take the daemon down on the read.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_unparseable_json_value_falls_back_to_the_default(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        kv.set("nums", "not json at all").await.unwrap();
        let back: Vec<i64> = kv.get_json("nums").await.unwrap();
        assert!(back.is_empty());
    }
}
