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
    migrate(&conn).context("migrating the database schema")?;
    Ok(conn)
}

/// Columns added to a table that already exists in somebody's database.
///
/// `CREATE TABLE IF NOT EXISTS` does nothing to a table that is already there,
/// so a column added to `schema.sql` reaches a fresh database and no other. A
/// daemon that has been running for weeks is exactly the case that matters, so
/// every added column is also listed here. `ALTER TABLE ... ADD COLUMN` is the
/// one schema change SQLite does cheaply and in place, and `pragma_table_info`
/// says whether it is needed, so this is idempotent without a version table.
///
/// The table and column names come from this constant and never from data, so
/// the `format!` below is not a string-built query in the dangerous sense.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("events", "triage_attempts", "INTEGER NOT NULL DEFAULT 0"),
    ("events", "triage_error", "TEXT"),
];

/// Indexes over columns from [`ADDED_COLUMNS`]. They cannot live in
/// `schema.sql`, which is applied *before* the migration and would fail on a
/// database that does not have the columns yet.
const LATE_INDEXES: &[&str] = &["CREATE INDEX IF NOT EXISTS events_untriaged
       ON events (triaged_at, triage_attempts, id)"];

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    for (table, column, decl) in ADDED_COLUMNS {
        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            rusqlite::params![table, column],
            |row| row.get(0),
        )?;
        if existing == 0 {
            tracing::info!(table, column, "adding a column to an existing database");
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    for statement in LATE_INDEXES {
        conn.execute_batch(statement)?;
    }
    Ok(())
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

    /// The case the migration exists for: a database created before a column
    /// was added to `schema.sql`. `CREATE TABLE IF NOT EXISTS` would leave it
    /// without the column and every query naming it would fail.
    #[test]
    fn a_database_predating_a_column_gains_it_on_open() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.db");
        {
            // The events table as it was before triage attempts existed.
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE events (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   source TEXT NOT NULL, external_id TEXT NOT NULL,
                   kind TEXT NOT NULL, payload TEXT NOT NULL,
                   salience INTEGER, triaged_at TEXT, created_at TEXT NOT NULL,
                   UNIQUE (source, external_id))",
            )
            .unwrap();
            old.execute_batch(
                "INSERT INTO events (source, external_id, kind, payload, created_at)
                 VALUES ('canvas','e1','assignment','{}','2026-09-01T00:00:00Z')",
            )
            .unwrap();
        }

        let conn = open(&path).expect("opening an older database must migrate it");
        let attempts: i64 = conn
            .query_row("SELECT triage_attempts FROM events WHERE id = 1", [], |r| {
                r.get(0)
            })
            .expect("the added column must exist and default for existing rows");
        assert_eq!(attempts, 0);
        let error: Option<String> = conn
            .query_row("SELECT triage_error FROM events WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(error, None);
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
