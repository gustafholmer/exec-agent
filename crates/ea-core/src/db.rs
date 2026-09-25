//! The state database.
//!
//! `connect` opens the Postgres pool every store module targets, and applies
//! the migrations in `crates/ea-core/migrations/` before handing it back.

use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// The schema, embedded at compile time so the daemon carries its own
/// migrations and nothing extra installs on the target machine.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Connect, run any pending migrations, and hand back the pool.
///
/// Eight connections is ample: the daemon's real concurrency is a handful of
/// tasks, and a personal assistant polls rather than serves.
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
        .context("connecting to the state database")?;
    MIGRATOR
        .run(&pool)
        .await
        .context("applying database migrations")?;
    Ok(pool)
}

/// [`connect`], retried with backoff for up to `budget`.
///
/// `launchd` starts the daemon at boot and will sometimes start it before
/// Postgres is accepting connections. A daemon that cannot reach its store is
/// dead rather than degraded, so this gives up cleanly and lets `launchd`'s
/// existing restart policy handle it, instead of hanging forever.
pub async fn connect_with_retry(url: &str, budget: Duration) -> anyhow::Result<PgPool> {
    let deadline = std::time::Instant::now() + budget;
    let mut wait = Duration::from_millis(250);
    loop {
        match connect(url).await {
            Ok(pool) => return Ok(pool),
            Err(err) if std::time::Instant::now() + wait < deadline => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    retry_in_ms = wait.as_millis() as u64,
                    "the state database is not reachable yet"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn connect_returns_a_usable_pool(pool: sqlx::PgPool) {
        // Cast to bigint: an untyped integer literal comes back as Postgres's
        // `int4`, which does not decode into `i64`.
        let one: i64 = sqlx::query_scalar("SELECT 1::bigint")
            .fetch_one(&pool)
            .await
            .expect("a connected pool must answer a trivial query");
        assert_eq!(one, 1);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn creates_every_table(pool: sqlx::PgPool) {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for table in [
            "events", "actions", "conversations", "messages",
            "facts", "runs", "schedules", "kv",
        ] {
            assert!(names.contains(&table.to_string()), "missing {table}");
        }
    }

    /// Review Focus #4 belongs to Task 10, but the constraint itself is created
    /// here, so its shape is pinned here too.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn the_status_check_rejects_an_unknown_status(pool: sqlx::PgPool) {
        let err = sqlx::query(
            "INSERT INTO actions (connector, tool, args, preview, rationale, status, expires_at)
             VALUES ('x','y','{}','p','r','nonsense', now())",
        )
        .execute(&pool)
        .await
        .expect_err("an unknown status must be refused by the database");
        assert!(
            format!("{err}").contains("actions_status_check"),
            "expected the CHECK constraint to be named in the error, got: {err}"
        );
    }
}
