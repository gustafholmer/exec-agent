use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use chrono::Utc;
use rusqlite::{params, Connection, Row};
use serde::Serialize;

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
    pub triaged_at: Option<String>,
    pub created_at: String,
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

#[derive(Clone)]
pub struct EventStore {
    conn: Arc<Mutex<Connection>>,
}

fn hydrate(row: &Row<'_>) -> rusqlite::Result<Event> {
    let payload: String = row.get("payload")?;
    Ok(Event {
        id: row.get("id")?,
        source: row.get("source")?,
        external_id: row.get("external_id")?,
        kind: row.get("kind")?,
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        salience: row.get("salience")?,
        triaged_at: row.get("triaged_at")?,
        created_at: row.get("created_at")?,
        triage_attempts: row.get("triage_attempts")?,
        triage_error: row.get("triage_error")?,
    })
}

impl EventStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Idempotent per `(source, external_id)`. Returns `(event, is_new)`.
    /// A changed payload updates in place and clears triage, so a moved
    /// meeting or a part-paid invoice gets re-scored rather than going stale.
    ///
    /// The whole read-decide-write sequence runs under a single lock
    /// acquisition so two concurrent calls for a brand-new key can't both
    /// observe "not found" and both attempt the insert.
    pub fn record(&self, input: RecordInput) -> anyhow::Result<(Event, bool)> {
        let payload = serde_json::to_string(&input.payload)?;
        let conn = self.conn.lock().unwrap();

        let existing: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, payload FROM events WHERE source = ?1 AND external_id = ?2",
                params![input.source, input.external_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        if let Some((id, old_payload)) = existing {
            if old_payload != payload {
                // A changed payload is a new question, so the attempt count
                // and the give-up reason go with the old answer: an event the
                // model could not score as an empty stub deserves a fresh
                // three tries once it has actually got content.
                conn.execute(
                    "UPDATE events SET payload = ?1, salience = NULL, triaged_at = NULL,
                       triage_attempts = 0, triage_error = NULL
                     WHERE id = ?2",
                    params![payload, id],
                )?;
            }
            let event =
                Self::get_locked(&conn, id)?.ok_or_else(|| anyhow!("event {id} vanished"))?;
            return Ok((event, false));
        }

        conn.execute(
            "INSERT INTO events (source, external_id, kind, payload, created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                input.source,
                input.external_id,
                input.kind,
                payload,
                Utc::now().to_rfc3339()
            ],
        )?;
        let id = conn.last_insert_rowid();
        let event = Self::get_locked(&conn, id)?.ok_or_else(|| anyhow!("event {id} vanished"))?;
        Ok((event, true))
    }

    /// Reads by id using an already-locked connection. `record()` needs this
    /// to read back the row it just wrote without releasing and re-acquiring
    /// the (non-reentrant) mutex, which would reopen the very race this
    /// store exists to close.
    fn get_locked(conn: &Connection, id: i64) -> anyhow::Result<Option<Event>> {
        let mut stmt = conn.prepare("SELECT * FROM events WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate)?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Event>> {
        let conn = self.conn.lock().unwrap();
        Self::get_locked(&conn, id)
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
    pub fn untriaged(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM events WHERE triaged_at IS NULL
             ORDER BY triage_attempts, id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    pub fn by_kind(&self, kind: &str, limit: i64) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM events WHERE kind = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![kind, limit], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    /// **Comparison is lexicographic over the stored text**, which is what
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
    pub fn in_payload_range(
        &self,
        kinds: &[&str],
        keys: &[&str],
        from: &str,
        to: &str,
    ) -> anyhow::Result<Vec<Event>> {
        if kinds.is_empty() || keys.is_empty() {
            return Ok(Vec::new());
        }
        // Both lists are `&str` slots bound as parameters — including the JSON
        // paths, which `json_extract` takes as a bound value — so nothing a
        // caller passes is interpolated into the SQL text.
        let extracts = keys
            .iter()
            .map(|_| "json_extract(payload, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let kind_slots = vec!["?"; kinds.len()].join(", ");
        // The trailing `NULL` is not decoration: SQLite's `COALESCE` requires
        // at least two arguments, and a caller passing a single key is the
        // ordinary case. It changes no result — coalescing to NULL is what a
        // row with none of the keys does anyway.
        let sql = format!(
            "SELECT * FROM (
               SELECT *, COALESCE({extracts}, NULL) AS at_value
               FROM events WHERE kind IN ({kind_slots})
             )
             WHERE at_value >= ? AND at_value < ?
             ORDER BY at_value, id"
        );

        let paths: Vec<String> = keys.iter().map(|key| format!("$.{key}")).collect();
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for path in &paths {
            args.push(path);
        }
        for kind in kinds {
            args.push(kind);
        }
        args.push(&from);
        args.push(&to);

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(args.as_slice(), hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Events triaged after `since` and scored at or above `min_salience`,
    /// newest first.
    ///
    /// What the morning briefing means by "what mattered since the last one".
    /// `triaged_at`, not `created_at`: an event recorded a fortnight ago and
    /// only scored this morning is news this morning.
    pub fn scored_since(
        &self,
        min_salience: i64,
        since: chrono::DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM events
             WHERE salience >= ?1 AND triaged_at IS NOT NULL AND triaged_at > ?2
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![min_salience, since.to_rfc3339(), limit], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record that `id` was submitted to a tier-1 batch that came back without
    /// a score for it. Returns the new attempt count.
    ///
    /// Counted on *submission that produced an answer*, not on every pass: a
    /// session that fails outright (the model is down, the budget is spent) is
    /// not this event's fault and must not burn its attempts.
    pub fn record_triage_attempt(&self, id: i64) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE events SET triage_attempts = triage_attempts + 1 WHERE id = ?1",
            params![id],
        )?;
        let count: i64 = conn.query_row(
            "SELECT triage_attempts FROM events WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Give up on an event: stamp `triaged_at` so it leaves the scan window,
    /// and record `reason` so it is visible rather than vanished.
    ///
    /// `salience` is deliberately left `NULL`. Writing a zero would make an
    /// event nobody could score indistinguishable from an event that was
    /// scored and found unimportant, which is the difference a human looking
    /// into "why did I not hear about this" actually needs.
    pub fn abandon(&self, id: i64, reason: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE events SET triaged_at = ?1, triage_error = ?2 WHERE id = ?3",
            params![Utc::now().to_rfc3339(), reason, id],
        )?;
        Ok(())
    }

    /// Events triage gave up on. `ea status` reports the count, which is what
    /// keeps this from being a silent failure.
    pub fn abandoned(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM events WHERE triage_error IS NOT NULL ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn abandoned_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM events WHERE triage_error IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Record a score. Clears any `triage_error`: an event that was given up
    /// on and then scored anyway (because its payload changed and it came back
    /// round) is no longer a failure.
    pub fn set_salience(&self, id: i64, salience: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE events SET salience = ?1, triaged_at = ?2, triage_error = NULL
             WHERE id = ?3",
            params![salience, Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    fn input(source: &str, external_id: &str, payload: serde_json::Value) -> RecordInput {
        RecordInput {
            source: source.into(),
            external_id: external_id.into(),
            kind: kinds::ASSIGNMENT.into(),
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

    #[test]
    fn by_kind_returns_only_that_kind_newest_first_and_respects_the_limit() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        store
            .record(of_kind("google", "c1", kinds::CALENDAR_EVENT))
            .unwrap();
        store.record(of_kind("google", "m1", kinds::MAIL)).unwrap();
        store
            .record(of_kind("google", "c2", kinds::CALENDAR_EVENT))
            .unwrap();

        let found = store.by_kind(kinds::CALENDAR_EVENT, 10).unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, vec!["c2", "c1"], "newest first, and no mail");

        assert_eq!(store.by_kind(kinds::CALENDAR_EVENT, 1).unwrap().len(), 1);
        assert!(store
            .by_kind("nothing_records_this", 10)
            .unwrap()
            .is_empty());
    }

    fn at(source: &str, external_id: &str, kind: &str, payload: serde_json::Value) -> RecordInput {
        RecordInput {
            source: source.into(),
            external_id: external_id.into(),
            kind: kind.into(),
            payload,
        }
    }

    /// The regression `in_payload_range` exists for. With `by_kind`'s
    /// `ORDER BY id DESC LIMIT n`, a row recorded before `n` others is gone
    /// before any date filter sees it — and a calendar row is recorded up to a
    /// week before it happens, so that is the ordinary case, not a corner one.
    #[test]
    fn a_row_in_the_window_is_found_however_many_rows_were_recorded_after_it() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        store
            .record(at(
                "google",
                "wanted",
                kinds::CALENDAR_EVENT,
                serde_json::json!({ "start": "2026-09-25T08:00:00Z" }),
            ))
            .unwrap();
        for i in 0..200 {
            store
                .record(at(
                    "google",
                    &format!("later-{i}"),
                    kinds::CALENDAR_EVENT,
                    serde_json::json!({ "start": "2026-10-02T08:00:00Z" }),
                ))
                .unwrap();
        }

        let found = store
            .in_payload_range(
                &[kinds::CALENDAR_EVENT],
                &["start"],
                "2026-09-25",
                "2026-09-26",
            )
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, vec!["wanted"]);

        // The control: `by_kind` with the same generous limit does lose it.
        assert!(
            !store
                .by_kind(kinds::CALENDAR_EVENT, 60)
                .unwrap()
                .iter()
                .any(|e| e.external_id == "wanted"),
            "the id-ordered read is the thing this method replaces"
        );
    }

    /// The window is half-open, several kinds can be asked for at once, the
    /// keys are tried in order, and a row with none of them is simply absent
    /// rather than an error.
    #[test]
    fn in_payload_range_is_half_open_over_several_kinds_and_keys() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
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
            store.record(at("google", id, kind, payload)).unwrap();
        }

        let found = store
            .in_payload_range(
                &[kinds::CALENDAR_EVENT, kinds::CALENDAR_CONFLICT],
                &["start", "overlap_start"],
                "2026-09-25",
                "2026-09-26",
            )
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["first", "clash"],
            "inclusive lower bound, exclusive upper, ordered by the start value"
        );

        assert!(store
            .in_payload_range(&[], &["start"], "2026-09-25", "2026-09-26")
            .unwrap()
            .is_empty());
        assert!(store
            .in_payload_range(&[kinds::CALENDAR_EVENT], &[], "2026-09-25", "2026-09-26")
            .unwrap()
            .is_empty());
    }

    /// The morning briefing asks "what was scored above the threshold since
    /// the last briefing" — so the cut is on `triaged_at`, not `created_at`,
    /// and an unscored event is never in the answer however old it is.
    #[test]
    fn scored_since_cuts_on_triaged_at_and_ignores_the_unscored() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (low, _) = store
            .record(of_kind("canvas", "low", "assignment"))
            .unwrap();
        let (high, _) = store
            .record(of_kind("canvas", "high", "assignment"))
            .unwrap();
        let (untouched, _) = store
            .record(of_kind("canvas", "raw", "assignment"))
            .unwrap();

        let before = Utc::now();
        store.set_salience(low.id, 20).unwrap();
        store.set_salience(high.id, 80).unwrap();

        let found = store.scored_since(60, before, 10).unwrap();
        let ids: Vec<i64> = found.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![high.id],
            "below the threshold and unscored are both out"
        );
        assert!(store.get(untouched.id).unwrap().unwrap().salience.is_none());

        // A cut *after* the scoring returns nothing: yesterday's news is not
        // today's.
        assert!(store.scored_since(60, Utc::now(), 10).unwrap().is_empty());
    }

    #[test]
    fn records_a_new_event() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (event, is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .unwrap();
        assert!(is_new);
        assert_eq!(event.source, "canvas");
        assert_eq!(event.external_id, "assign-1");
        assert_eq!(event.payload["due"], "2026-10-01");
    }

    #[test]
    fn ten_identical_polls_produce_one_row() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        for _ in 0..10 {
            store
                .record(input(
                    "canvas",
                    "assign-1",
                    serde_json::json!({ "due": "2026-10-01" }),
                ))
                .unwrap();
        }
        let untriaged = store.untriaged(100).unwrap();
        assert_eq!(untriaged.len(), 1);
    }

    #[test]
    fn is_new_is_false_on_a_repeat_and_the_id_matches() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (first, first_is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .unwrap();
        assert!(first_is_new);
        let (second, second_is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .unwrap();
        assert!(!second_is_new);
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn same_external_id_from_a_different_source_is_distinct() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (a, _) = store
            .record(input("canvas", "1", serde_json::json!({ "a": 1 })))
            .unwrap();
        let (b, is_new) = store
            .record(input("gmail", "1", serde_json::json!({ "a": 1 })))
            .unwrap();
        assert!(is_new);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn a_changed_payload_updates_in_place() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (first, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .unwrap();
        let (second, is_new) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-08" }),
            ))
            .unwrap();
        assert!(!is_new);
        assert_eq!(first.id, second.id);
        assert_eq!(second.payload["due"], "2026-10-08");
    }

    #[test]
    fn a_changed_payload_clears_salience_and_triaged_at() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let (first, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-01" }),
            ))
            .unwrap();
        store.set_salience(first.id, 5).unwrap();
        assert!(store.untriaged(100).unwrap().is_empty());

        let (second, _) = store
            .record(input(
                "canvas",
                "assign-1",
                serde_json::json!({ "due": "2026-10-08" }),
            ))
            .unwrap();
        assert_eq!(second.salience, None);
        assert!(second.triaged_at.is_none());

        let untriaged = store.untriaged(100).unwrap();
        assert_eq!(untriaged.len(), 1);
        assert_eq!(untriaged[0].id, first.id);
    }

    #[test]
    fn concurrent_record_of_a_brand_new_key_is_idempotent() {
        use std::sync::Barrier;
        use std::thread;

        let (_dir, conn) = temp_store();
        let store_a = EventStore::new(conn.clone());
        let store_b = EventStore::new(conn.clone());
        // A barrier makes both threads arrive at `record()` at the same
        // instant instead of serialising by luck of thread-spawn order,
        // widening the window in which a check-then-act race could show up.
        let barrier = Arc::new(Barrier::new(2));

        let ba = barrier.clone();
        let handle_a = thread::spawn(move || {
            ba.wait();
            store_a.record(input("canvas", "race-1", serde_json::json!({ "n": 1 })))
        });
        let bb = barrier.clone();
        let handle_b = thread::spawn(move || {
            bb.wait();
            store_b.record(input("canvas", "race-1", serde_json::json!({ "n": 1 })))
        });

        let result_a = handle_a.join().unwrap();
        let result_b = handle_b.join().unwrap();

        let (event_a, is_new_a) = result_a.expect("first concurrent record() must not error");
        let (event_b, is_new_b) = result_b.expect("second concurrent record() must not error");

        assert_eq!(event_a.id, event_b.id);
        assert_ne!(
            is_new_a, is_new_b,
            "exactly one of the two concurrent calls must report is_new == true"
        );

        let locked = conn.lock().unwrap();
        let count: i64 = locked
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// The ordering the triage wedge turned on: an event that has already
    /// failed a tier-1 batch must not keep a brand-new event out of the next
    /// one. With `ORDER BY id` it did, forever.
    #[test]
    fn untriaged_puts_fresh_events_ahead_of_ones_that_have_failed() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let old_id = store
            .record(input("canvas", "old", serde_json::json!({ "n": 1 })))
            .unwrap()
            .0
            .id;
        let new_id = store
            .record(input("canvas", "new", serde_json::json!({ "n": 2 })))
            .unwrap()
            .0
            .id;
        assert!(old_id < new_id);

        // Before any attempt, id order holds.
        let order: Vec<i64> = store.untriaged(10).unwrap().iter().map(|e| e.id).collect();
        assert_eq!(order, vec![old_id, new_id]);

        assert_eq!(store.record_triage_attempt(old_id).unwrap(), 1);
        let order: Vec<i64> = store.untriaged(10).unwrap().iter().map(|e| e.id).collect();
        assert_eq!(
            order,
            vec![new_id, old_id],
            "an event that already failed must sort behind one that has not"
        );
    }

    #[test]
    fn abandon_takes_an_event_out_of_the_scan_window_and_says_why() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let id = store
            .record(input("canvas", "a", serde_json::json!({ "n": 1 })))
            .unwrap()
            .0
            .id;

        store.abandon(id, "the model never scored it").unwrap();

        assert!(store.untriaged(10).unwrap().is_empty());
        let event = store.get(id).unwrap().unwrap();
        assert!(event.triaged_at.is_some());
        assert_eq!(
            event.salience, None,
            "an abandoned event must not look like a scored zero"
        );
        assert_eq!(
            event.triage_error.as_deref(),
            Some("the model never scored it")
        );
        assert_eq!(store.abandoned_count().unwrap(), 1);
        assert_eq!(store.abandoned(10).unwrap()[0].id, id);
    }

    /// A changed payload is a new question. An event abandoned as an empty
    /// stub deserves a fresh set of attempts once it has content.
    #[test]
    fn a_changed_payload_clears_the_attempt_count_and_the_give_up() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        let id = store
            .record(input("canvas", "a", serde_json::json!({ "n": 1 })))
            .unwrap()
            .0
            .id;
        store.record_triage_attempt(id).unwrap();
        store.record_triage_attempt(id).unwrap();
        store.abandon(id, "gave up").unwrap();

        let (event, is_new) = store
            .record(input("canvas", "a", serde_json::json!({ "n": 2 })))
            .unwrap();
        assert!(!is_new);
        assert_eq!(event.triage_attempts, 0);
        assert_eq!(event.triage_error, None);
        assert!(event.triaged_at.is_none());
        assert_eq!(store.abandoned_count().unwrap(), 0);
    }

    #[test]
    fn untriaged_honours_its_limit() {
        let (_dir, conn) = temp_store();
        let store = EventStore::new(conn);
        for i in 0..5 {
            store
                .record(input(
                    "canvas",
                    &format!("assign-{i}"),
                    serde_json::json!({ "n": i }),
                ))
                .unwrap();
        }
        let limited = store.untriaged(2).unwrap();
        assert_eq!(limited.len(), 2);
    }
}
