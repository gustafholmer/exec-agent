//! Where the state database lives, and how its credential is read.
//!
//! A Postgres URL carries a password, which makes it the same class of value
//! as the Telegram bot token: it is read through `read_secret`, which refuses
//! a file readable by anyone but the owner, and it never reaches a log line.

use std::path::PathBuf;

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Clone, Deserialize, Default)]
pub struct DatabaseSettings {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub url_file: Option<PathBuf>,
}

/// Hand-written so a connection string cannot reach a log line through `{:?}`.
impl std::fmt::Debug for DatabaseSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSettings")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("url_file", &self.url_file)
            .finish()
    }
}

impl DatabaseSettings {
    /// The connection URL, from `url_file` if given, else inline `url`.
    ///
    /// `url_file` wins when both are set: a file held to mode `0600` is the
    /// safer of the two, so naming one is taken as meaning it.
    pub fn resolve(&self) -> anyhow::Result<String> {
        if let Some(file) = &self.url_file {
            return crate::notify::telegram::read_secret(file)
                .with_context(|| format!("reading the database URL from {}", file.display()));
        }
        match &self.url {
            Some(url) => Ok(url.clone()),
            None => bail!(
                "no state database configured: set `database.url_file` to a \
                 0600 file holding the connection URL, or `database.url` inline"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_url() {
        let settings = DatabaseSettings {
            url: Some("postgres://ea:hunter2@localhost/exec_agent".into()),
            url_file: None,
        };
        let printed = format!("{settings:?}");
        assert!(!printed.contains("hunter2"), "password leaked: {printed}");
        assert!(printed.contains("redacted"), "expected a redaction marker: {printed}");
    }

    #[test]
    fn an_inline_url_resolves() {
        let settings = DatabaseSettings {
            url: Some("postgres:///exec_agent".into()),
            url_file: None,
        };
        assert_eq!(settings.resolve().unwrap(), "postgres:///exec_agent");
    }

    #[test]
    fn neither_url_nor_file_is_an_error_naming_both_options() {
        let settings = DatabaseSettings { url: None, url_file: None };
        let err = format!("{:#}", settings.resolve().unwrap_err());
        assert!(err.contains("url"), "the error must name `url`: {err}");
        assert!(err.contains("url_file"), "the error must name `url_file`: {err}");
    }

    /// Review Focus #5: Postgres going away under a running daemon.
    ///
    /// Two halves. An unreachable server must surface a clear error within
    /// the retry budget rather than hanging. And a pool whose connection the
    /// server killed must not be permanently poisoned: the *same* pool serves
    /// the next query on a fresh connection.
    #[tokio::test]
    async fn a_pool_pointed_at_a_dead_server_fails_fast_and_recovers() {
        // Port 1 is reserved and never listening: a stand-in for "Postgres is down".
        let dead = ea_core::db::connect_with_retry(
            "postgres://ea@127.0.0.1:1/exec_agent",
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(dead.is_err(), "an unreachable database must not hang");

        // Like every other database test in the suite, this needs a live
        // server; passing silently without one would claim coverage it
        // does not have.
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set: this test needs a live Postgres");
        let pool = ea_core::db::connect(&url).await.expect("a live database");

        let victim: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .unwrap();
        // Kill that connection from outside the pool, the way a server
        // restart or an idle-session timeout would.
        {
            use sqlx::Connection;
            let mut admin = sqlx::PgConnection::connect(&url).await.unwrap();
            let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
                .bind(victim)
                .fetch_one(&mut admin)
                .await
                .unwrap();
            assert!(killed, "the pool's connection should have been terminated");
            admin.close().await.unwrap();
        }

        let (one, pid): (i64, i32) = sqlx::query_as("SELECT 1::bigint, pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .expect("the same pool must recover from a killed connection");
        assert_eq!(one, 1);
        assert_ne!(pid, victim, "the answer must come from a fresh connection");
    }
}
