//! The `kv` table: small, durable scraps of daemon state that do not deserve
//! a table of their own.
//!
//! What lives here has to survive a restart and is read by exactly one owner
//! -- the notification log's send history and digest backlog, the Telegram
//! update offset. Anything with more than one reader, or any structure worth
//! querying, belongs in a real table instead.

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

#[derive(Clone)]
pub struct KvStore {
    conn: Arc<Mutex<Connection>>,
}

impl KvStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// The value at `key`, or `None` when it has never been set.
    pub fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let value: Option<String> = conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .ok();
        Ok(value)
    }

    /// Write `value` at `key`, replacing whatever was there.
    pub fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Read a JSON value, falling back to `T::default()` both when the key is
    /// absent and when what is stored no longer parses as a `T`.
    ///
    /// The second half is deliberate. This is bookkeeping, not the ledger: a
    /// shape that changed between releases must not be able to wedge the
    /// daemon at startup, and losing an hour of notification history is a
    /// smaller harm than refusing to run.
    pub fn get_json<T: serde::de::DeserializeOwned + Default>(
        &self,
        key: &str,
    ) -> anyhow::Result<T> {
        let Some(raw) = self.get(key)? else {
            return Ok(T::default());
        };
        Ok(serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(key, error = %err, "discarding unparseable kv value");
            T::default()
        }))
    }

    pub fn set_json<T: serde::Serialize>(&self, key: &str, value: &T) -> anyhow::Result<()> {
        self.set(key, &serde_json::to_string(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    #[test]
    fn a_value_round_trips_and_overwrites() {
        let (_dir, conn) = temp_store();
        let kv = KvStore::new(conn);
        assert_eq!(kv.get("k").unwrap(), None);
        kv.set("k", "one").unwrap();
        assert_eq!(kv.get("k").unwrap().as_deref(), Some("one"));
        kv.set("k", "two").unwrap();
        assert_eq!(kv.get("k").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn json_round_trips() {
        let (_dir, conn) = temp_store();
        let kv = KvStore::new(conn);
        kv.set_json("nums", &vec![1i64, 2, 3]).unwrap();
        let back: Vec<i64> = kv.get_json("nums").unwrap();
        assert_eq!(back, vec![1, 2, 3]);
    }

    /// A value whose shape changed between releases must degrade to the
    /// default, not take the daemon down on the read.
    #[test]
    fn an_unparseable_json_value_falls_back_to_the_default() {
        let (_dir, conn) = temp_store();
        let kv = KvStore::new(conn);
        kv.set("nums", "not json at all").unwrap();
        let back: Vec<i64> = kv.get_json("nums").unwrap();
        assert!(back.is_empty());
    }
}
