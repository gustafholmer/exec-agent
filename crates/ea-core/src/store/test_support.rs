//! Test helpers shared by the store modules.
//!
//! `#[sqlx::test]` hands each test its own freshly migrated database, so
//! there is no shared fixture to build for a converted store -- but
//! `temp_store` stays here for the store modules that have not converted yet
//! (until Task 12), alongside the timestamp convention every converted
//! store's tests depend on.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tempfile::TempDir;

pub fn temp_store() -> (TempDir, Arc<Mutex<Connection>>) {
    let dir = TempDir::new().unwrap();
    let conn = crate::db::sqlite::open(&dir.path().join("state.db")).unwrap();
    (dir, Arc::new(Mutex::new(conn)))
}

/// Assert two timestamps are equal *as Postgres stores them*.
///
/// `TIMESTAMPTZ` has microsecond resolution and `chrono::DateTime<Utc>` has
/// nanosecond, so a value written and read back is very nearly never `==` to
/// the original. Comparing truncated to microseconds is the real contract.
#[track_caller]
pub(crate) fn assert_same_instant(
    left: chrono::DateTime<chrono::Utc>,
    right: chrono::DateTime<chrono::Utc>,
) {
    let micros = |t: chrono::DateTime<chrono::Utc>| t.timestamp_micros();
    assert_eq!(
        micros(left),
        micros(right),
        "timestamps differ beyond microsecond resolution: {left} vs {right}"
    );
}
