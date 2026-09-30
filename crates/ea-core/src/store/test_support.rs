//! Test helpers shared by the store modules.
//!
//! `#[sqlx::test]` hands each test its own freshly migrated database, so
//! there is no shared fixture to build -- just the timestamp convention every
//! store's tests depend on.

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
