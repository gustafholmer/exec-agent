use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

const SCHEMA: &str = include_str!("schema.sql");

/// Opens (creating if needed) the state database with WAL and a busy timeout,
/// applying the schema idempotently.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    open_with_busy_timeout(path, 5000)
}

/// Same as [`open`], but with an explicit `busy_timeout` in milliseconds.
///
/// Production always goes through [`open`] with the 5-second default; this
/// exists so tests can set the timeout to 0 and have a blocked read surface
/// immediately as `SQLITE_BUSY` instead of being silently absorbed.
pub fn open_with_busy_timeout(path: &Path, busy_timeout_ms: u32) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening the database at {}", path.display()))?;

    // WAL lets the CLI read while the daemon writes; the busy timeout absorbs
    // the brief exclusive locks a checkpoint still takes.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", busy_timeout_ms)?;
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
    //
    // This has to actually exercise WAL's core guarantee (readers don't block
    // on a writer's uncommitted transaction), not just SQLite's normal lock
    // semantics. A single small INSERT never forces dirty pages out of the
    // writer's private page cache before COMMIT, so a reader can succeed
    // there under any journal mode -- that isn't evidence of anything. To
    // force the writer to actually touch the WAL file before commit, insert
    // enough rows (30k, well past the default ~500-page cache limit measured
    // empirically for this schema at the default 4096-byte page size)
    // inside one BEGIN IMMEDIATE transaction to spill its page cache. And to
    // make sure a block would be visible rather than
    // silently absorbed, the reader's busy_timeout is set to 0 so any
    // blocking surfaces immediately as SQLITE_BUSY instead of being retried
    // for up to the production 5-second default.
    //
    // Verified manually: patching `journal_mode` to `DELETE` in `open`
    // (temporarily, for this check only) makes this test fail with
    // `SQLITE_BUSY` on the reader's query, and restoring WAL makes it pass
    // again. See the fix report for the exact command output.
    #[test]
    fn concurrent_reader_is_not_blocked_by_an_open_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        let writer = open(&path).unwrap();
        let reader = open_with_busy_timeout(&path, 0).unwrap();

        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        {
            let mut stmt = writer
                .prepare(
                    "INSERT INTO events (source, external_id, kind, payload, created_at)
                     VALUES ('canvas', ?1, 'assignment', '{}', '2026-09-23T00:00:00Z')",
                )
                .unwrap();
            for i in 0..30_000 {
                stmt.execute([format!("e{i}")]).unwrap();
            }
        }

        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("read during an open write transaction must succeed");
        assert_eq!(count, 0, "the uncommitted rows must not be visible");

        writer.execute_batch("COMMIT").unwrap();
    }
}
