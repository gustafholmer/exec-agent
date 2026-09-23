use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use chrono::Utc;
use rusqlite::{params, Connection, Row};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub surface: String,
    pub body: String,
    pub created_at: String,
}

fn hydrate_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get("id")?,
        conversation_id: row.get("conversation_id")?,
        role: row.get("role")?,
        surface: row.get("surface")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
    })
}

pub struct ConversationStore {
    conn: Arc<Mutex<Connection>>,
}

impl ConversationStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// The newest conversation's id, creating one if the table is empty.
    pub fn current(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM conversations ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = existing {
            return Ok(id);
        }
        conn.execute(
            "INSERT INTO conversations (created_at) VALUES (?1)",
            params![Utc::now().to_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn append(
        &self,
        conversation_id: i64,
        role: &str,
        surface: &str,
        body: &str,
    ) -> anyhow::Result<Message> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (conversation_id, role, surface, body, created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                conversation_id,
                role,
                surface,
                body,
                Utc::now().to_rfc3339()
            ],
        )?;
        let id = conn.last_insert_rowid();
        let mut stmt = conn.prepare("SELECT * FROM messages WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate_message)?;
        match rows.next() {
            Some(row) => Ok(row?),
            None => Err(anyhow!("message {id} vanished after insert")),
        }
    }

    /// The last `limit` messages in this conversation, oldest first.
    pub fn recent(&self, conversation_id: i64, limit: i64) -> anyhow::Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM messages WHERE conversation_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![conversation_id, limit], hydrate_message)?;
        let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    pub fn claude_session(&self, id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let session: Option<String> = conn
            .query_row(
                "SELECT claude_session FROM conversations WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        Ok(session)
    }

    pub fn set_claude_session(&self, id: i64, session: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET claude_session = ?1 WHERE id = ?2",
            params![session, id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    #[test]
    fn current_creates_a_conversation_then_reuses_it() {
        let (_dir, conn) = temp_store();
        let store = ConversationStore::new(conn);
        let first = store.current().unwrap();
        let second = store.current().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn both_surfaces_interleave_in_order() {
        let (_dir, conn) = temp_store();
        let store = ConversationStore::new(conn);
        let convo = store.current().unwrap();
        store.append(convo, "user", "telegram", "hi").unwrap();
        store
            .append(convo, "assistant", "cli", "hello there")
            .unwrap();
        store
            .append(convo, "user", "telegram", "what's due today?")
            .unwrap();

        let recent = store.recent(convo, 10).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].body, "hi");
        assert_eq!(recent[0].surface, "telegram");
        assert_eq!(recent[1].body, "hello there");
        assert_eq!(recent[1].surface, "cli");
        assert_eq!(recent[2].body, "what's due today?");
    }

    #[test]
    fn recent_returns_the_last_n_chronologically() {
        let (_dir, conn) = temp_store();
        let store = ConversationStore::new(conn);
        let convo = store.current().unwrap();
        for i in 0..5 {
            store
                .append(convo, "user", "cli", &format!("message {i}"))
                .unwrap();
        }
        let recent = store.recent(convo, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].body, "message 3");
        assert_eq!(recent[1].body, "message 4");
    }

    #[test]
    fn a_stored_session_id_round_trips() {
        let (_dir, conn) = temp_store();
        let store = ConversationStore::new(conn);
        let convo = store.current().unwrap();
        assert_eq!(store.claude_session(convo).unwrap(), None);
        store.set_claude_session(convo, "sess-abc123").unwrap();
        assert_eq!(
            store.claude_session(convo).unwrap().as_deref(),
            Some("sess-abc123")
        );
    }
}
