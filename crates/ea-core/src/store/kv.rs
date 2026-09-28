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

    /// Raise the integer at `key` to `value`, never lowering it: a monotonic
    /// high-water mark in one statement.
    ///
    /// A stored value that is not an integer is treated as absent and
    /// replaced, matching what a read of it has always meant (`None`). The
    /// comparison is in `numeric` so no stored text can overflow the cast.
    pub async fn set_max(&self, key: &str, value: i64) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO kv (key, value) VALUES ($1, $2::text)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value
             WHERE CASE WHEN btrim(kv.value) ~ '^-?[0-9]+$'
                        THEN btrim(kv.value)::numeric < $2::numeric
                        ELSE true END",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .context("raising a kv high-water mark")?;
        Ok(())
    }

    /// Read-modify-write one JSON value atomically.
    ///
    /// `get_json` followed by `set_json` is two statements with an `.await`
    /// between them, and every other writer of the same key can land in that
    /// gap and be overwritten. This runs the pair inside one transaction that
    /// first takes the key's lock (see [`KvTransaction::lock_json`]), so
    /// concurrent updates of one key queue up instead of losing each other.
    /// `f` is synchronous on purpose: nothing may await while the lock is
    /// held.
    pub async fn update_json<T, R>(&self, key: &str, f: impl FnOnce(&mut T) -> R) -> anyhow::Result<R>
    where
        T: serde::de::DeserializeOwned + serde::Serialize + Default,
    {
        let mut tx = self.begin().await?;
        let mut value: T = tx.lock_json(key).await?;
        let out = f(&mut value);
        tx.set_json(key, &value).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Open a transaction for read-modify-writes that span more than one key.
    pub async fn begin(&self) -> anyhow::Result<KvTransaction> {
        let tx = self.pool.begin().await.context("opening a kv transaction")?;
        Ok(KvTransaction { tx })
    }
}

/// A transaction over `kv`, for updates that must not interleave.
///
/// Dropping it without [`KvTransaction::commit`] rolls everything back.
pub struct KvTransaction {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl KvTransaction {
    /// Lock `key` until this transaction ends, then read it as JSON (with the
    /// same default-on-absent-or-unparseable rule as [`KvStore::get_json`]).
    ///
    /// Why an advisory lock rather than `SELECT … FOR UPDATE`: a row lock
    /// needs a row, and a key that has never been written has none, so two
    /// first writers would both see "absent" and race. A transaction-scoped
    /// advisory lock keyed on the key name exists whether or not the row
    /// does. The two-argument form lives in a different lock space from the
    /// one-argument `hashtext(...)` locks elsewhere, and the `'ea.kv'` class
    /// keeps it apart from anything else using the two-argument form. A hash
    /// collision between two keys would only serialise them, never break
    /// anything.
    ///
    /// Callers that lock more than one key must always lock them in the same
    /// order.
    pub async fn lock_json<T: serde::de::DeserializeOwned + Default>(
        &mut self,
        key: &str,
    ) -> anyhow::Result<T> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ea.kv'), hashtext($1))")
            .bind(key)
            .execute(&mut *self.tx)
            .await
            .context("locking a kv entry")?;
        let raw: Option<String> = sqlx::query_scalar("SELECT value FROM kv WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *self.tx)
            .await
            .context("reading a kv entry")?;
        let Some(raw) = raw else {
            return Ok(T::default());
        };
        Ok(serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(key, error = %err, "discarding unparseable kv value");
            T::default()
        }))
    }

    /// Write `value` at `key` inside this transaction.
    pub async fn set_json<T: serde::Serialize>(&mut self, key: &str, value: &T) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO kv (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .execute(&mut *self.tx)
        .await
        .context("writing a kv entry")?;
        Ok(())
    }

    pub async fn commit(self) -> anyhow::Result<()> {
        self.tx.commit().await.context("committing a kv transaction")
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

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn set_max_only_ever_raises(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        kv.set_max("hw", 5).await.unwrap();
        kv.set_max("hw", 3).await.unwrap();
        assert_eq!(kv.get("hw").await.unwrap().as_deref(), Some("5"));
        kv.set_max("hw", 12).await.unwrap();
        assert_eq!(kv.get("hw").await.unwrap().as_deref(), Some("12"));
        kv.set("hw", "garbage").await.unwrap();
        kv.set_max("hw", 1).await.unwrap();
        assert_eq!(kv.get("hw").await.unwrap().as_deref(), Some("1"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn concurrent_set_max_keeps_the_highest(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..16_i64 {
            let kv = kv.clone();
            tasks.spawn(async move { kv.set_max("hw", i).await.unwrap() });
        }
        while let Some(done) = tasks.join_next().await {
            done.unwrap();
        }
        assert_eq!(kv.get("hw").await.unwrap().as_deref(), Some("15"));
    }

    /// Concurrent read-modify-writes of one key must not lose each other's
    /// changes, including the very first ones on a key that has no row yet.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn concurrent_update_json_loses_nothing(pool: sqlx::PgPool) {
        let kv = KvStore::new(pool);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..16_i64 {
            let kv = kv.clone();
            tasks.spawn(async move {
                kv.update_json("counter", |n: &mut Vec<i64>| n.push(i))
                    .await
                    .unwrap()
            });
        }
        while let Some(done) = tasks.join_next().await {
            done.unwrap();
        }
        let mut back: Vec<i64> = kv.get_json("counter").await.unwrap();
        back.sort_unstable();
        assert_eq!(back, (0..16).collect::<Vec<_>>());
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
