use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::timestamp::{serialize_rfc3339, serialize_rfc3339_opt};
use sqlx::PgPool;

/// The `kind` strings that go into the `events` table, owned by the crate that
/// owns the table.
///
/// **Why here and not in each connector.** These strings cross a process
/// boundary: a connector writes one into a watch entry, the daemon hands it
/// back to [`EventStore::record`], and the briefings and triage rules then
/// read rows out by it. Until this module existed, each side held its own
/// `const` with the same literal in it, so renaming `calendar_event` in
/// `ea-google` left the morning briefing reading a kind nothing emits any
/// more — an empty calendar section, no error, and no failing test, because
/// the reader's tests wrote events using the reader's own copy of the string
/// and so stayed self-consistent while being wrong.
///
/// The daemon still has no compile-time dependency on any connector crate —
/// connectors are child processes reached over MCP — but every connector *and*
/// the daemon already depend on `ea-core`, and `ea-core` owns the store these
/// strings are persisted in. Naming them here makes a rename a compile error
/// on both sides of the boundary instead of a silent hole in a briefing.
///
/// A connector may still emit a kind that is not listed here; what it must not
/// do is keep a second private constant for one that is.
pub mod kinds {
    /// An upcoming calendar event. Emitted by `ea-google`.
    pub const CALENDAR_EVENT: &str = "calendar_event";
    /// Two calendar events that overlap. Emitted by `ea-google`.
    pub const CALENDAR_CONFLICT: &str = "calendar_conflict";
    /// An unread message. Emitted by `ea-google` (Gmail) and `ea-kth` (Graph);
    /// deliberately the same kind from both, because a mail is a mail.
    pub const MAIL: &str = "mail";
    /// A coursework assignment. Emitted by `ea-canvas`.
    pub const ASSIGNMENT: &str = "assignment";
    /// A row in a watched Notion database. Emitted by `ea-notion`.
    pub const DATABASE_ITEM: &str = "database_item";
    /// An invoice that is still open. Emitted by `ea-fortnox-mcp`.
    pub const UNPAID_INVOICE: &str = "unpaid_invoice";
    /// A declaration falling due. Emitted by `ea-fortnox-mcp`.
    pub const TAX_DEADLINE: &str = "tax_deadline";
    /// The synthetic row a poll emits for an account it could not read.
    /// Emitted by `ea-google` and `ea-kth`.
    pub const CONNECTOR_ERROR: &str = "connector_error";
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub source: String,
    pub external_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub salience: Option<i64>,
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub triaged_at: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub created_at: DateTime<Utc>,
    /// How many tier-1 batches this event has been submitted to without a
    /// score coming back for it.
    pub triage_attempts: i64,
    /// Why triage gave up on this event, if it did. Set together with
    /// `triaged_at` while `salience` stays `None`.
    pub triage_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecordInput {
    pub source: String,
    pub external_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// The columns every query below selects, in the order [`hydrate`] reads
/// them.
const EVENT_COLUMNS: &str = "id, source, external_id, kind, payload, salience, \
     triaged_at, created_at, triage_attempts, triage_error";

fn hydrate(row: &sqlx::postgres::PgRow) -> anyhow::Result<Event> {
    use sqlx::Row;
    let payload: sqlx::types::Json<serde_json::Value> = row.try_get("payload")?;
    // `salience` and `triage_attempts` are Postgres `INTEGER` (int4); the
    // struct keeps them `i64` for parity with every other id/count in this
    // crate, so the narrower wire type is decoded and widened here.
    let salience: Option<i32> = row.try_get("salience")?;
    let triage_attempts: i32 = row.try_get("triage_attempts")?;
    Ok(Event {
        id: row.try_get("id")?,
        source: row.try_get("source")?,
        external_id: row.try_get("external_id")?,
        kind: row.try_get("kind")?,
        payload: payload.0,
        salience: salience.map(i64::from),
        triaged_at: row.try_get("triaged_at")?,
        created_at: row.try_get("created_at")?,
        triage_attempts: i64::from(triage_attempts),
        triage_error: row.try_get("triage_error")?,
    })
}

/// True when `err`'s chain bottoms out at a Postgres error in SQLSTATE class
/// `22` — a data exception, where the fault is the *content* of the row a
/// caller tried to write, not the connection, the pool, or the query. The
/// two class-`22` codes a NUL byte used to produce — `22021` in a plain
/// `TEXT` column, `22P05` inside a `JSONB` string — can no longer reach the
/// database, because [`EventStore::record`] replaces NULs first; this remains
/// the backstop for any other data exception.
///
/// This is the line a caller like `run_watch_poll` needs: a connector handing
/// back one malformed item is not a reason to abandon the rest of the poll,
/// but a database that is down or a pool that has timed out is exactly the
/// "connector looks healthy forever" failure the poll's breaker exists to
/// catch, and must never be treated the same way. Everything that is not a
/// class-`22` database error — `PoolTimedOut`, `PoolClosed`, `Io`, a
/// constraint violation from a different class, or no `sqlx::Error` in the
/// chain at all — is *not* rejected content, and the caller must propagate it.
pub fn is_rejected_content(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<sqlx::Error>())
        .and_then(sqlx::Error::as_database_error)
        .and_then(|db_err| db_err.code())
        .is_some_and(|code| code.starts_with("22"))
}

/// Replace every NUL byte with U+FFFD (the replacement character).
///
/// Postgres `TEXT` and `JSONB` both refuse U+0000, and connector data is
/// untrusted: a mail subject with a NUL in it would otherwise be rejected on
/// every poll and never reach triage — a third party able to keep an item out
/// of the owner's sight. SQLite stored such text as-is. Replacing the byte
/// keeps the event, visibly marked where the NUL was. `watch_poll`'s
/// skip of class-`22` errors ([`is_rejected_content`]) stays as the backstop
/// for any other data exception.
fn sanitize_nul(text: String) -> String {
    if text.contains('\0') {
        text.replace('\0', "\u{FFFD}")
    } else {
        text
    }
}

/// [`sanitize_nul`] over every string in a JSON value, object keys included.
fn sanitize_nul_json(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(sanitize_nul(text)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_nul_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (sanitize_nul(key), sanitize_nul_json(value)))
                .collect(),
        ),
        other => other,
    }
}

#[derive(Clone)]
pub struct EventStore {
    pool: PgPool,
}

impl EventStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent per `(source, external_id)`. Returns `(event, is_new)`.
    /// A changed payload updates in place and clears triage, so a moved
    /// meeting or a part-paid invoice gets re-scored rather than going stale.
    ///
    /// One statement, because it has to be atomic and there is no longer a
    /// global mutex making it so. A changed payload is a new question, so the
    /// attempt count and the give-up reason go with the old answer: an event
    /// the model could not score as an empty stub deserves a fresh three tries
    /// once it has actually got content. An unchanged payload keeps all of it.
    ///
    /// `xmax = 0` is how Postgres distinguishes an insert from an update in an
    /// upsert's RETURNING. It is an MVCC implementation detail rather than
    /// documented SQL, which is why both branches are covered by tests.
    ///
    /// NUL bytes are replaced with U+FFFD first (see [`sanitize_nul`]).
    pub async fn record(&self, input: RecordInput) -> anyhow::Result<(Event, bool)> {
        let input = RecordInput {
            source: sanitize_nul(input.source),
            external_id: sanitize_nul(input.external_id),
            kind: sanitize_nul(input.kind),
            payload: sanitize_nul_json(input.payload),
        };
        let row = sqlx::query(
            "INSERT INTO events (source, external_id, kind, payload)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (source, external_id) DO UPDATE SET
               payload         = EXCLUDED.payload,
               salience        = CASE WHEN events.payload IS DISTINCT FROM EXCLUDED.payload
                                      THEN NULL ELSE events.salience END,
               triaged_at      = CASE WHEN events.payload IS DISTINCT FROM EXCLUDED.payload
                                      THEN NULL ELSE events.triaged_at END,
               triage_attempts = CASE WHEN events.payload IS DISTINCT FROM EXCLUDED.payload
                                      THEN 0 ELSE events.triage_attempts END,
               triage_error    = CASE WHEN events.payload IS DISTINCT FROM EXCLUDED.payload
                                      THEN NULL ELSE events.triage_error END
             RETURNING id, source, external_id, kind, payload, salience,
                       triaged_at, created_at, triage_attempts, triage_error,
                       (xmax = 0) AS inserted",
        )
        .bind(&input.source)
        .bind(&input.external_id)
        .bind(&input.kind)
        .bind(sqlx::types::Json(&input.payload))
        .fetch_one(&self.pool)
        .await
        .context("recording an event")?;

        let inserted: bool = sqlx::Row::try_get(&row, "inserted")?;
        Ok((hydrate(&row)?, inserted))
    }

    pub async fn get(&self, id: i64) -> anyhow::Result<Option<Event>> {
        let row = sqlx::query(&format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .context("reading an event")?;
        row.as_ref().map(hydrate).transpose()
    }

    /// Events still waiting to be triaged, fewest failed attempts first and
    /// then oldest first.
    ///
    /// The ordering is the fix for a real wedge. With a plain `ORDER BY id`, an
    /// event the tier-1 model omits from its answer is never stamped and so
    /// stays at the head of this list forever; enough of them and every pass
    /// scans the same doomed rows, spending a session every five minutes and
    /// never reaching anything new. Ordering by `triage_attempts` first means a
    /// brand-new event always overtakes one that has already failed, so the
    /// head of the queue can never be permanently occupied — and
    /// [`EventStore::abandon`] eventually takes the failures out of it
    /// altogether.
    pub async fn untriaged(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE triaged_at IS NULL
             ORDER BY triage_attempts, id LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("reading untriaged events")?;
        rows.iter().map(hydrate).collect()
    }

    /// The most recent events of one `kind`, newest first — newest as
    /// *recorded*, because the order is by id.
    ///
    /// **Do not filter the result on a date.** The limit is applied before any
    /// filter a caller writes in Rust, so a row that matches the filter can be
    /// cut by rows that do not, and the caller gets a short answer rather than
    /// an error. That is not hypothetical: it is how the morning briefing lost
    /// today's meetings. Selecting on a payload timestamp is
    /// [`EventStore::in_payload_range`]'s job. This method is for "the last n
    /// of these, whatever they are".
    pub async fn by_kind(&self, kind: &str, limit: i64) -> anyhow::Result<Vec<Event>> {
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE kind = $1 ORDER BY id DESC LIMIT $2"
        ))
        .bind(kind)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("reading events by kind")?;
        rows.iter().map(hydrate).collect()
    }

    /// Events of any of `kinds` whose payload carries a timestamp — under the
    /// first of `keys` that is present — sorting into `[from, to)`.
    ///
    /// **Why this exists, and why it is not [`by_kind`] with a filter after
    /// it.** `by_kind` is `ORDER BY id DESC LIMIT n`, which is newest *as
    /// recorded*. A calendar row is first recorded up to a week before the
    /// meeting happens and keeps that id when later polls re-upsert it, and
    /// rows live for the whole retention window. So "take the newest 60 rows,
    /// then keep the ones starting today" silently loses today's meeting on
    /// any calendar that records more than 60 events in a week — the row is
    /// there, it is correct, and it is below the cut because of other rows'
    /// ids. Selecting on the start value instead means no row can be pushed
    /// out of the answer by a row that does not belong in it.
    ///
    /// **No `LIMIT`.** The window *is* the bound: a caller asks for a day or a
    /// month, not for "the most recent n". A limit here would reintroduce
    /// exactly the failure above, one window smaller.
    ///
    /// **Comparison is bytewise over the stored text** (`COLLATE "C"`, as
    /// SQLite's `BINARY` was; the database's own collation may order `+` and
    /// `-` the other way round), which is what
    /// makes this safe without teaching the store a date format: RFC 3339
    /// timestamps in a fixed shape sort chronologically as strings, and a
    /// caller that cannot promise one shape (an offset instead of `Z`, say)
    /// widens the window by a day at each end and does the exact test in Rust
    /// — which is what [`ea-daemon`'s `todays_calendar`] does. Rows whose
    /// payload has none of `keys`, or holds a non-string there, are simply not
    /// in the answer.
    ///
    /// The payload keys are the *caller's*, passed in rather than hardcoded,
    /// so one connector's payload shape still does not end up written into the
    /// schema.
    pub async fn in_payload_range(
        &self,
        kinds: &[&str],
        keys: &[&str],
        from: &str,
        to: &str,
    ) -> anyhow::Result<Vec<Event>> {
        if kinds.is_empty() || keys.is_empty() {
            return Ok(Vec::new());
        }
        // Both lists are bound as parameters — including the JSON keys, which
        // `payload->>$n` takes as a bound value — so nothing a caller passes
        // is interpolated into the SQL text. See the deliberate absence of an
        // expression index in the commit message: the keys arrive at runtime,
        // so no static btree expression index could serve this query, and a
        // GIN index on `payload` supports containment, not `->>` extraction.
        let mut bind_index = 1;
        let extracts = keys
            .iter()
            .map(|_| {
                let placeholder = format!("payload->>${bind_index}");
                bind_index += 1;
                placeholder
            })
            .collect::<Vec<_>>()
            .join(", ");
        let kind_slots = kinds
            .iter()
            .map(|_| {
                let placeholder = format!("${bind_index}");
                bind_index += 1;
                placeholder
            })
            .collect::<Vec<_>>()
            .join(", ");
        let from_index = bind_index;
        let to_index = bind_index + 1;

        let sql = format!(
            "SELECT * FROM (
               SELECT {EVENT_COLUMNS}, COALESCE({extracts}) AS at_value
               FROM events WHERE kind IN ({kind_slots})
             ) AS ranged
             WHERE at_value COLLATE \"C\" >= ${from_index}
               AND at_value COLLATE \"C\" < ${to_index}
             ORDER BY at_value COLLATE \"C\", id"
        );

        let mut query = sqlx::query(&sql);
        for key in keys {
            query = query.bind(*key);
        }
        for kind in kinds {
            query = query.bind(*kind);
        }
        query = query.bind(from).bind(to);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .context("reading events in a payload range")?;
        rows.iter().map(hydrate).collect()
    }

    /// Events triaged after `since` and scored at or above `min_salience`,
    /// newest first.
    ///
    /// What the morning briefing means by "what mattered since the last one".
    /// `triaged_at`, not `created_at`: an event recorded a fortnight ago and
    /// only scored this morning is news this morning.
    pub async fn scored_since(
        &self,
        min_salience: i64,
        since: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        // `salience` is Postgres `INTEGER` (int4); see the note on
        // `set_salience`.
        let min_salience = i32::try_from(min_salience)
            .with_context(|| format!("salience {min_salience} does not fit in a database column"))?;
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE salience >= $1 AND triaged_at IS NOT NULL AND triaged_at > $2
             ORDER BY id DESC LIMIT $3"
        ))
        .bind(min_salience)
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("reading events scored since a time")?;
        rows.iter().map(hydrate).collect()
    }

    /// Record that `id` was submitted to a tier-1 batch that came back without
    /// a score for it. Returns the new attempt count.
    ///
    /// Counted on *submission that produced an answer*, not on every pass: a
    /// session that fails outright (the model is down, the budget is spent) is
    /// not this event's fault and must not burn its attempts.
    pub async fn record_triage_attempt(&self, id: i64) -> anyhow::Result<i64> {
        let attempts: i32 = sqlx::query_scalar(
            "UPDATE events SET triage_attempts = triage_attempts + 1
             WHERE id = $1 RETURNING triage_attempts",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .context("recording a triage attempt")?;
        Ok(i64::from(attempts))
    }

    /// Give up on an event: stamp `triaged_at` so it leaves the scan window,
    /// and record `reason` so it is visible rather than vanished.
    ///
    /// `salience` is deliberately left `NULL`. Writing a zero would make an
    /// event nobody could score indistinguishable from an event that was
    /// scored and found unimportant, which is the difference a human looking
    /// into "why did I not hear about this" actually needs.
    pub async fn abandon(&self, id: i64, reason: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE events SET triaged_at = now(), triage_error = $2 WHERE id = $1")
            .bind(id)
            .bind(reason)
            .execute(&self.pool)
            .await
            .context("abandoning an event")?;
        Ok(())
    }

    /// Events triage gave up on. `ea status` reports the count, which is what
    /// keeps this from being a silent failure.
    pub async fn abandoned(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE triage_error IS NOT NULL ORDER BY id DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("reading abandoned events")?;
        rows.iter().map(hydrate).collect()
    }

    pub async fn abandoned_count(&self) -> anyhow::Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE triage_error IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .context("counting abandoned events")
    }

    /// Record a score. Clears any `triage_error`: an event that was given up
    /// on and then scored anyway (because its payload changed and it came back
    /// round) is no longer a failure.
    pub async fn set_salience(&self, id: i64, salience: i64) -> anyhow::Result<()> {
        // `salience` is Postgres `INTEGER` (int4); every caller passes a
        // 0-100 score or DROPPED_SALIENCE, so this narrows cleanly, but a
        // checked conversion means a caller error becomes an `Err` rather
        // than a silently truncated value.
        let salience = i32::try_from(salience)
            .with_context(|| format!("salience {salience} does not fit in a database column"))?;
        sqlx::query(
            "UPDATE events SET salience = $2, triaged_at = now(), triage_error = NULL
             WHERE id = $1",
        )
        .bind(id)
        .bind(salience)
        .execute(&self.pool)
        .await
        .context("setting an event's salience")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(source: &str, external_id: &str, payload: serde_json::Value) -> RecordInput {
        RecordInput {
            source: source.into(),
            external_id: external_id.into(),
            kind: "assignment".into(),
            payload,
        }
    }

    fn of_kind(source: &str, external_id: &str, kind: &str) -> RecordInput {
        RecordInput {
            source: source.into(),
            external_id: external_id.into(),
            kind: kind.into(),
            payload: serde_json::json!({}),
        }
    }

    fn at(source: &str, external_id: &str, kind: &str, payload: serde_json::Value) -> RecordInput {
        RecordInput {
            source: source.into(),
            external_id: external_id.into(),
            kind: kind.into(),
            payload,
        }
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn by_kind_returns_only_that_kind_newest_first_and_respects_the_limit(
        pool: sqlx::PgPool,
    ) {
        let store = EventStore::new(pool);
        store
            .record(of_kind("google", "c1", kinds::CALENDAR_EVENT))
            .await
            .unwrap();
        store
            .record(of_kind("google", "m1", kinds::MAIL))
            .await
            .unwrap();
        store
            .record(of_kind("google", "c2", kinds::CALENDAR_EVENT))
            .await
            .unwrap();

        let found = store.by_kind(kinds::CALENDAR_EVENT, 10).await.unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, vec!["c2", "c1"], "newest first, and no mail");

        assert_eq!(
            store.by_kind(kinds::CALENDAR_EVENT, 1).await.unwrap().len(),
            1
        );
        assert!(store
            .by_kind("nothing_records_this", 10)
            .await
            .unwrap()
            .is_empty());
    }

    /// The regression `in_payload_range` exists for. With `by_kind`'s
    /// `ORDER BY id DESC LIMIT n`, a row recorded before `n` others is gone
    /// before any date filter sees it — and a calendar row is recorded up to a
    /// week before it happens, so that is the ordinary case, not a corner one.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_row_in_the_window_is_found_however_many_rows_were_recorded_after_it(
        pool: sqlx::PgPool,
    ) {
        let store = EventStore::new(pool);
        store
            .record(at(
                "google",
                "wanted",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "start": "2026-09-25T08:00:00Z" }),
            ))
            .await
            .unwrap();
        for i in 0..200 {
            store
                .record(at(
                    "google",
                    &format!("later-{i}"),
                    kinds::CALENDAR_EVENT,
                    serde_json::json!({ "start": "2026-10-02T08:00:00Z" }),
                ))
                .await
                .unwrap();
        }

        let found = store
            .in_payload_range(
                &[kinds::CALENDAR_EVENT],
                &["start"],
                "2026-09-25",
                "2026-09-26",
            )
            .await
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, vec!["wanted"]);

        // The control: `by_kind` with the same generous limit does lose it.
        assert!(
            !store
                .by_kind(kinds::CALENDAR_EVENT, 60)
                .await
                .unwrap()
                .iter()
                .any(|e| e.external_id == "wanted"),
            "the id-ordered read is the thing this method replaces"
        );
    }

    /// The window is half-open, several kinds can be asked for at once, the
    /// keys are tried in order, and a row with none of them is simply absent
    /// rather than an error.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn in_payload_range_is_half_open_over_several_kinds_and_keys(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        for (id, kind, payload) in [
            (
                "before",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "start": "2026-09-24T23:59:59Z" }),
            ),
            (
                "first",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "start": "2026-09-25T00:00:00Z" }),
            ),
            (
                "clash",
                kinds::CALENDAR_CONFLICT,
                serde_json::json!({ "overlap_start": "2026-09-25T09:00:00Z" }),
            ),
            (
                "at-the-upper-bound",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "start": "2026-09-26T00:00:00Z" }),
            ),
            (
                "no-start-at-all",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "title": "undated" }),
            ),
            (
                "other-kind",
                kinds::MAIL,
                serde_json::json!({ "start": "2026-09-25T10:00:00Z" }),
            ),
        ] {
            store.record(at("google", id, kind, payload)).await.unwrap();
        }

        let found = store
            .in_payload_range(
                &[kinds::CALENDAR_EVENT, kinds::CALENDAR_CONFLICT],
                &["start", "overlap_start"],
                "2026-09-25",
                "2026-09-26",
            )
            .await
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["first", "clash"],
            "inclusive lower bound, exclusive upper, ordered by the start value"
        );

        assert!(store
            .in_payload_range(&[], &["start"], "2026-09-25", "2026-09-26")
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .in_payload_range(&[kinds::CALENDAR_EVENT], &[], "2026-09-25", "2026-09-26")
            .await
            .unwrap()
            .is_empty());
    }

    /// Minor 1 of the final review: ordering and bounds are bytewise, as they
    /// were in SQLite. Under a locale collation `-05:00` can sort before
    /// `+02:00`; bytewise `+` (0x2B) comes before `-` (0x2D).
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn in_payload_range_compares_bytewise(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        for (id, start) in [
            ("minus", "2026-09-25T10:00:00-05:00"),
            ("plus", "2026-09-25T10:00:00+02:00"),
        ] {
            store
                .record(at("google", id, kinds::CALENDAR_EVENT, json!({ "start": start })))
                .await
                .unwrap();
        }
        let ids = |found: Vec<Event>| -> Vec<String> {
            found.into_iter().map(|e| e.external_id).collect()
        };

        let all = store
            .in_payload_range(&[kinds::CALENDAR_EVENT], &["start"], "2026-09-25", "2026-09-26")
            .await
            .unwrap();
        assert_eq!(ids(all), vec!["plus", "minus"], "ordered bytewise");

        let upper = store
            .in_payload_range(
                &[kinds::CALENDAR_EVENT],
                &["start"],
                "2026-09-25",
                "2026-09-25T10:00:00-",
            )
            .await
            .unwrap();
        assert_eq!(ids(upper), vec!["plus"], "bounded bytewise");
    }

    /// The morning briefing asks "what was scored above the threshold since
    /// the last briefing" — so the cut is on `triaged_at`, not `created_at`,
    /// and an unscored event is never in the answer however old it is.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn scored_since_cuts_on_triaged_at_and_ignores_the_unscored(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (low, _) = store
            .record(of_kind("canvas", "low", "assignment"))
            .await
            .unwrap();
        let (high, _) = store
            .record(of_kind("canvas", "high", "assignment"))
            .await
            .unwrap();
        let (untouched, _) = store
            .record(of_kind("canvas", "raw", "assignment"))
            .await
            .unwrap();

        let before = Utc::now();
        store.set_salience(low.id, 20).await.unwrap();
        store.set_salience(high.id, 80).await.unwrap();

        let found = store.scored_since(60, before, 10).await.unwrap();
        let ids: Vec<i64> = found.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![high.id],
            "below the threshold and unscored are both out"
        );
        assert!(store
            .get(untouched.id)
            .await
            .unwrap()
            .unwrap()
            .salience
            .is_none());

        // A cut *after* the scoring returns nothing: yesterday's news is not
        // today's.
        assert!(store
            .scored_since(60, Utc::now(), 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn records_a_new_event(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .await
            .unwrap();
        assert!(is_new);
        assert_eq!(event.source, "canvas");
        assert_eq!(event.external_id, "assign-1");
        assert_eq!(event.payload["due"], "2026-10-01");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn ten_identical_polls_produce_one_row(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        for _ in 0..10 {
            store
                .record(input(
                    "canvas",
                    "assign-1",
                    serde_json::json!({ "due": "2026-10-01" }),
                ))
                .await
                .unwrap();
        }
        let untriaged = store.untriaged(100).await.unwrap();
        assert_eq!(untriaged.len(), 1);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn is_new_is_false_on_a_repeat_and_the_id_matches(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (first, first_is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .await
            .unwrap();
        assert!(first_is_new);
        let (second, second_is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .await
            .unwrap();
        assert!(!second_is_new);
        assert_eq!(first.id, second.id);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn same_external_id_from_a_different_source_is_distinct(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (a, _) = store
            .record(input("canvas", "1", serde_json::json!({ "a": 1 })))
            .await
            .unwrap();
        let (b, is_new) = store
            .record(input("gmail", "1", serde_json::json!({ "a": 1 })))
            .await
            .unwrap();
        assert!(is_new);
        assert_ne!(a.id, b.id);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_changed_payload_updates_in_place(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (first, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .await
            .unwrap();
        let (second, is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-08" }),
            ))
            .await
            .unwrap();
        assert!(!is_new);
        assert_eq!(first.id, second.id);
        assert_eq!(second.payload["due"], "2026-10-08");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_changed_payload_clears_salience_and_triaged_at(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (first, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .await
            .unwrap();
        store.set_salience(first.id, 5).await.unwrap();
        assert!(store.untriaged(100).await.unwrap().is_empty());

        let (second, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-08" }),
            ))
            .await
            .unwrap();
        assert_eq!(second.salience, None);
        assert!(second.triaged_at.is_none());

        let untriaged = store.untriaged(100).await.unwrap();
        assert_eq!(untriaged.len(), 1);
        assert_eq!(untriaged[0].id, first.id);
    }

    /// The ordering the triage wedge turned on: an event that has already
    /// failed a tier-1 batch must not keep a brand-new event out of the next
    /// one. With `ORDER BY id` it did, forever.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn untriaged_puts_fresh_events_ahead_of_ones_that_have_failed(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let old_id = store
            .record(input("canvas", "old", serde_json::json!({ "n": 1 })))
            .await
            .unwrap()
            .0
            .id;
        let new_id = store
            .record(input("canvas", "new", serde_json::json!({ "n": 2 })))
            .await
            .unwrap()
            .0
            .id;
        assert!(old_id < new_id);

        // Before any attempt, id order holds.
        let order: Vec<i64> = store
            .untriaged(10)
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(order, vec![old_id, new_id]);

        assert_eq!(store.record_triage_attempt(old_id).await.unwrap(), 1);
        let order: Vec<i64> = store
            .untriaged(10)
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            order,
            vec![new_id, old_id],
            "an event that already failed must sort behind one that has not"
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn abandon_takes_an_event_out_of_the_scan_window_and_says_why(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let id = store
            .record(input("canvas", "a", serde_json::json!({ "n": 1 })))
            .await
            .unwrap()
            .0
            .id;

        store.abandon(id, "the model never scored it").await.unwrap();

        assert!(store.untriaged(10).await.unwrap().is_empty());
        let event = store.get(id).await.unwrap().unwrap();
        assert!(event.triaged_at.is_some());
        assert_eq!(
            event.salience, None,
            "an abandoned event must not look like a scored zero"
        );
        assert_eq!(
            event.triage_error.as_deref(),
            Some("the model never scored it")
        );
        assert_eq!(store.abandoned_count().await.unwrap(), 1);
        assert_eq!(store.abandoned(10).await.unwrap()[0].id, id);
    }

    /// A changed payload is a new question. An event abandoned as an empty
    /// stub deserves a fresh set of attempts once it has content.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_changed_payload_clears_the_attempt_count_and_the_give_up(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let id = store
            .record(input("canvas", "a", serde_json::json!({ "n": 1 })))
            .await
            .unwrap()
            .0
            .id;
        store.record_triage_attempt(id).await.unwrap();
        store.record_triage_attempt(id).await.unwrap();
        store.abandon(id, "gave up").await.unwrap();

        let (event, is_new) = store
            .record(input("canvas", "a", serde_json::json!({ "n": 2 })))
            .await
            .unwrap();
        assert!(!is_new);
        assert_eq!(event.triage_attempts, 0);
        assert_eq!(event.triage_error, None);
        assert!(event.triaged_at.is_none());
        assert_eq!(store.abandoned_count().await.unwrap(), 0);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn untriaged_honours_its_limit(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        for i in 0..5 {
            store
                .record(input(
                    "canvas",
                    &format!("assign-{i}"),
                    serde_json::json!({ "n": i }),
                ))
                .await
                .unwrap();
        }
        let limited = store.untriaged(2).await.unwrap();
        assert_eq!(limited.len(), 2);
    }

    // -- the upsert, restated as the brief asks for it ----------------------

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn record_inserts_once_and_reports_it(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, inserted) = store.record(input("canvas", "e1", json!({"a": 1}))).await.unwrap();
        assert!(inserted, "the first record of an id is an insert");
        assert_eq!(event.source, "canvas");

        let (_, inserted) = store.record(input("canvas", "e1", json!({"a": 1}))).await.unwrap();
        assert!(!inserted, "the same payload again is not an insert");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_changed_payload_resets_the_triage_state(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store.record(input("canvas", "e1", json!({"a": 1}))).await.unwrap();
        store.set_salience(event.id, 80).await.unwrap();
        store.record_triage_attempt(event.id).await.unwrap();

        let (event, _) = store.record(input("canvas", "e1", json!({"a": 2}))).await.unwrap();
        assert_eq!(event.salience, None, "a new question deserves a fresh answer");
        assert_eq!(event.triaged_at, None);
        assert_eq!(event.triage_attempts, 0);
        assert_eq!(event.triage_error, None);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_unchanged_payload_keeps_the_triage_state(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store.record(input("canvas", "e1", json!({"a": 1}))).await.unwrap();
        store.set_salience(event.id, 80).await.unwrap();

        let (event, _) = store.record(input("canvas", "e1", json!({"a": 1}))).await.unwrap();
        assert_eq!(event.salience, Some(80), "nothing changed, so nothing is reset");
    }

    /// JSONB compares semantically, not as serialized text, so a connector
    /// re-emitting the same object with its keys in a different order does
    /// not look like a changed payload and does not reset triage.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn reordered_payload_keys_are_not_a_change(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store
            .record(input("canvas", "e1", json!({"a": 1, "b": 2})))
            .await
            .unwrap();
        store.set_salience(event.id, 80).await.unwrap();

        let (event, _) = store
            .record(input("canvas", "e1", json!({"b": 2, "a": 1})))
            .await
            .unwrap();
        assert_eq!(event.salience, Some(80), "same object, different key order");
    }

    /// Review Focus #2: Postgres TEXT rejects the NUL byte. Connector data is
    /// untrusted — a mail subject can contain one — and an event refused for
    /// it would be refused on every poll, forever. So the byte is replaced
    /// and the event kept.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_nul_byte_in_a_text_field_is_replaced_and_the_event_kept(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let mut with_nul = input("canvas", "e\u{0}1", json!({"a": 1}));
        with_nul.kind = "assign\u{0}ment".into();
        let (event, inserted) = store.record(with_nul).await.expect("a NUL byte must not drop the event");
        assert!(inserted);
        assert_eq!(event.external_id, "e\u{FFFD}1");
        assert_eq!(event.kind, "assign\u{FFFD}ment");

        // And a re-poll of the same item is the same event, not a new one.
        let (again, inserted) = store
            .record(input("canvas", "e\u{0}1", json!({"a": 1})))
            .await
            .unwrap();
        assert!(!inserted);
        assert_eq!(again.id, event.id);
    }

    /// The same hazard inside a JSONB value, and in a key, at any depth:
    /// Postgres's `jsonb` also rejects `\u0000` in a string, and a connector
    /// payload is exactly the kind of untrusted text that can contain one (an
    /// email body pasted with a stray control character, say).
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_nul_byte_anywhere_in_the_payload_is_replaced(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store
            .record(input(
                "canvas",
                "e1",
                json!({"body": "before\u{0}after", "k\u{0}ey": [1, {"deep": "x\u{0}"}]}),
            ))
            .await
            .expect("a NUL byte inside the payload must not drop the event");
        assert_eq!(
            event.payload,
            json!({"body": "before\u{FFFD}after", "k\u{FFFD}ey": [1, {"deep": "x\u{FFFD}"}]})
        );
    }

    /// The spec's Risks table: every audited read-then-write gets a test under
    /// contention. Concurrent records of one brand-new key (each task on its
    /// own pooled connection) make exactly one row, and exactly one caller is
    /// told it inserted — the `xmax = 0` branch under a real race.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn concurrent_record_of_a_brand_new_key_is_idempotent(pool: sqlx::PgPool) {
        let store = EventStore::new(pool.clone());
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let store = store.clone();
            tasks.spawn(async move {
                store
                    .record(input("canvas", "same", json!({"title": "Essay"})))
                    .await
                    .unwrap()
            });
        }
        let mut ids = Vec::new();
        let mut inserted = 0;
        while let Some(done) = tasks.join_next().await {
            let (event, is_new) = done.unwrap();
            ids.push(event.id);
            inserted += usize::from(is_new);
        }
        assert_eq!(inserted, 1, "exactly one caller inserted");
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// The backstop `watch_poll` still relies on: a genuine class-22 data
    /// exception from Postgres is recognised as rejected content.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn is_rejected_content_is_true_for_a_genuine_class_22_error(pool: sqlx::PgPool) {
        let err = sqlx::query("SELECT 'not a number'::int")
            .execute(&pool)
            .await
            .expect_err("an invalid cast is a data exception");
        let err = anyhow::Error::new(err).context("recording an event");
        assert!(is_rejected_content(&err), "{err:#}");
    }

    /// The negative case `is_rejected_content` exists to draw: an error with
    /// no Postgres database error in its chain at all — the shape a pool
    /// timeout or a closed pool takes — must never be mistaken for rejected
    /// content, or a database outage would look like "one bad row" and the
    /// poll it came from would report success.
    #[test]
    fn is_rejected_content_is_false_for_an_error_with_no_database_error_in_it() {
        let err = anyhow::anyhow!("the pool is closed").context("recording an event");
        assert!(!is_rejected_content(&err));
    }

    /// Review Focus #3: `JSONB NOT NULL` accepts JSON `null` — it is a JSON
    /// value, not a SQL NULL — and it must round-trip as `Value::Null`.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_null_payload_round_trips(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store
            .record(input("canvas", "e1", serde_json::Value::Null))
            .await
            .unwrap();
        assert_eq!(event.payload, serde_json::Value::Null);
        let read = store.get(event.id).await.unwrap().unwrap();
        assert_eq!(read.payload, serde_json::Value::Null);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn record_triage_attempt_returns_the_new_count(pool: sqlx::PgPool) {
        let store = EventStore::new(pool);
        let (event, _) = store.record(input("canvas", "e1", json!({}))).await.unwrap();
        assert_eq!(store.record_triage_attempt(event.id).await.unwrap(), 1);
        assert_eq!(store.record_triage_attempt(event.id).await.unwrap(), 2);
    }
}
