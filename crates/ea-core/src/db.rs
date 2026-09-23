use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

const SCHEMA: &str = include_str!("schema.sql");

/// Opens (creating if needed) the state database with WAL and a busy timeout,
/// applying the schema idempotently.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening the database at {}", path.display()))?;

    // WAL lets the CLI read while the daemon writes; the busy timeout absorbs
    // the brief exclusive locks a checkpoint still takes.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)
        .context("applying the database schema")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_db() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("state.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn creates_every_table() {
        let (_dir, conn) = temp_db();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for table in [
            "events",
            "actions",
            "conversations",
            "messages",
            "facts",
            "runs",
            "schedules",
            "kv",
        ] {
            assert!(names.contains(&table.to_string()), "missing {table}");
        }
    }

    #[test]
    fn is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        drop(open(&path).unwrap());
        open(&path).expect("reopening an existing database must succeed");
    }

    #[test]
    fn enables_wal() {
        let (_dir, conn) = temp_db();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    // Review Focus #4: a concurrent reader must not hit SQLITE_BUSY.
    #[test]
    fn concurrent_reader_is_not_blocked_by_an_open_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        let writer = open(&path).unwrap();
        let reader = open(&path).unwrap();

        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute(
                "INSERT INTO events (source, external_id, kind, payload, created_at)
                 VALUES ('canvas','e1','assignment','{}','2026-09-23T00:00:00Z')",
                [],
            )
            .unwrap();

        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("read during an open write transaction must succeed");
        assert_eq!(count, 0, "the uncommitted row must not be visible");

        writer.execute_batch("COMMIT").unwrap();
    }
}
