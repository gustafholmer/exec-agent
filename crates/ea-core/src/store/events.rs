use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use chrono::Utc;
use rusqlite::{params, Connection, Row};
use serde::Serialize;

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
                conn.execute(
                    "UPDATE events SET payload = ?1, salience = NULL, triaged_at = NULL
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

    pub fn untriaged(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM events WHERE triaged_at IS NULL ORDER BY id LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_salience(&self, id: i64, salience: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE events SET salience = ?1, triaged_at = ?2 WHERE id = ?3",
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
            kind: "assignment".into(),
            payload,
        }
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
