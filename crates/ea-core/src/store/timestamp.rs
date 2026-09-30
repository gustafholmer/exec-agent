//! How store timestamps are written out as JSON.
//!
//! chrono's default `Serialize` renders a UTC instant with a `Z` suffix
//! (`2026-09-20T18:00:00.123456Z`). Before the Postgres port every timestamp
//! was a stored `to_rfc3339()` string, which ends `+00:00`, and that is the
//! text the daemon's IPC responses, `ea pending`, `ea log`, `ea facts` and the
//! `propose` tool have always carried. Every serialised row type uses these
//! two helpers (`#[serde(serialize_with = "...")]`) so that stays true, and so
//! there is one definition of the format rather than one per store.

use chrono::{DateTime, Utc};
use serde::Serializer;

/// `to_rfc3339()`, as a serde field serializer.
pub(crate) fn serialize_rfc3339<S: Serializer>(
    dt: &DateTime<Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&dt.to_rfc3339())
}

/// [`serialize_rfc3339`] for a nullable column; `None` stays `null`.
pub(crate) fn serialize_rfc3339_opt<S: Serializer>(
    dt: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match dt {
        Some(dt) => serializer.serialize_str(&dt.to_rfc3339()),
        None => serializer.serialize_none(),
    }
}
