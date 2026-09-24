//! Durable memory: the things the assistant has been told to remember.
//!
//! The `facts` table has existed since the first schema and, until the
//! `remember` tool, nothing wrote to it. It is deliberately not a transcript —
//! `messages` is that — but a small set of stable statements ("the tenta is on
//! the 14th", "invoices go to Ekonomi AB") that are worth carrying into a
//! conversation that starts months later.
//!
//! Three properties are load-bearing, and each has a test:
//!
//! * **A topic is a key, not a label.** Remembering the same topic twice
//!   *updates* it. A memory that appends would accumulate contradictions and
//!   then feed all of them to the model at once, which is worse than having no
//!   memory: the newest correction would carry exactly as much weight as the
//!   thing it corrected. The uniqueness is enforced here, in one `remember`,
//!   rather than by a unique index — a database that somehow already held two
//!   rows for one topic would then fail to *open*, taking the whole daemon
//!   down over a duplicated note.
//! * **An empty body is refused.** `remember("tenta", "")` is a fact that says
//!   nothing and would silently destroy the one it overwrote.
//! * **Injection into a prompt is selective.** [`FactStore::matching`] returns
//!   the facts whose topic shares a word with what the user just said, so a
//!   conversation about the tenta does not carry the invoicing address, and a
//!   conversation that matches nothing carries no facts block at all — and it
//!   is bounded at [`MAX_MATCHING_FACTS`], newest first, so a table that grows
//!   cannot grow the prompt with it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use chrono::Utc;
use rusqlite::{params, Connection, Row};
use serde::Serialize;

/// Longest topic accepted. A topic is a handful of words; anything longer is a
/// body that ended up in the wrong argument.
pub const MAX_TOPIC_CHARS: usize = 120;

/// Longest body accepted. Generous for a note, small enough that every fact
/// this returns can be pasted into a system prompt without thinking about it.
pub const MAX_BODY_CHARS: usize = 4_000;

/// Shortest word [`FactStore::matching`] will match on.
///
/// Two-letter words ("på", "at", "is") match nearly every message, so a fact
/// whose topic contained one would be injected into every conversation and the
/// selectivity would be decoration.
pub const MIN_MATCH_CHARS: usize = 3;

/// How many facts [`FactStore::matching`] will ever return.
///
/// Without a bound, the block spliced into the chat system prompt grows with
/// the fact table: a topic word like "tenta" that a hundred facts share would
/// put all hundred in front of the model, on every turn, at that turn's price.
/// Twenty is far above what the selectivity rule produces in practice (a
/// message shares a topic word with one or two facts) and still bounds the
/// worst case at twenty notes rather than the whole table.
///
/// Which twenty matters as much as the number: see [`FactStore::matching`].
pub const MAX_MATCHING_FACTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fact {
    pub id: i64,
    pub topic: String,
    pub body: String,
    pub created_at: String,
    /// When `remember` last overwrote the body. `None` on a fact that has
    /// never been corrected, and on any row predating the column.
    pub updated_at: Option<String>,
}

fn hydrate(row: &Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: row.get("id")?,
        topic: row.get("topic")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Split text into lowercase words worth matching on.
///
/// Public because the chat prompt's tests assert on the same tokenisation the
/// injection uses; a second copy of this rule would drift.
pub fn match_tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= MIN_MATCH_CHARS)
        .map(str::to_string)
        .collect()
}

#[derive(Clone)]
pub struct FactStore {
    conn: Arc<Mutex<Connection>>,
}

impl FactStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record a fact, replacing any fact already under that topic.
    ///
    /// Topic matching is case-insensitive: "Tenta" and "tenta" are the same
    /// shelf, because the model will not spell it the same way twice.
    pub fn remember(&self, topic: &str, body: &str) -> anyhow::Result<Fact> {
        let topic = topic.trim();
        let body = body.trim();
        if topic.is_empty() {
            bail!("remember: `topic` must not be empty");
        }
        if body.is_empty() {
            bail!("remember: `body` must not be empty — an empty fact would erase the one it replaces");
        }
        if topic.chars().count() > MAX_TOPIC_CHARS {
            bail!(
                "remember: `topic` is {} characters; the limit is {MAX_TOPIC_CHARS}",
                topic.chars().count()
            );
        }
        if body.chars().count() > MAX_BODY_CHARS {
            bail!(
                "remember: `body` is {} characters; the limit is {MAX_BODY_CHARS}",
                body.chars().count()
            );
        }

        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        // Lowest id wins if a database somehow holds two rows for one topic,
        // so repeated corrections converge on a single row rather than
        // alternating between them.
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM facts WHERE lower(topic) = lower(?1) ORDER BY id LIMIT 1",
                params![topic],
                |row| row.get(0),
            )
            .ok();

        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE facts SET topic = ?1, body = ?2, updated_at = ?3 WHERE id = ?4",
                    params![topic, body, now, id],
                )?;
                id
            }
            None => {
                conn.execute(
                    "INSERT INTO facts (topic, body, created_at) VALUES (?1, ?2, ?3)",
                    params![topic, body, now],
                )?;
                conn.last_insert_rowid()
            }
        };

        let mut stmt = conn.prepare("SELECT * FROM facts WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate)?;
        match rows.next() {
            Some(fact) => Ok(fact?),
            None => Err(anyhow!("fact {id} vanished after being written")),
        }
    }

    /// Every fact, oldest first — what `ea facts` prints.
    pub fn all(&self) -> anyhow::Result<Vec<Fact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM facts ORDER BY id")?;
        let rows = stmt.query_map([], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every fact, most recently written or corrected first.
    ///
    /// The order is total and deterministic: `updated_at` when the fact has
    /// been corrected and `created_at` when it has not, and the id to break a
    /// tie between two facts written in the same instant. Without the
    /// tie-break, [`MAX_MATCHING_FACTS`] would cut a different set of facts
    /// from one run to the next.
    fn by_recency(&self) -> anyhow::Result<Vec<Fact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM facts ORDER BY COALESCE(updated_at, created_at) DESC, id DESC",
        )?;
        let rows = stmt.query_map([], hydrate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The facts whose topic shares a word with `text`, newest first, at most
    /// [`MAX_MATCHING_FACTS`] of them.
    ///
    /// Matching happens in Rust rather than in SQL: the table is tens of rows,
    /// and a `LIKE` per token over an unindexed column is not cheaper than
    /// reading it — while the tokenisation rule stays one function that a test
    /// can call directly.
    ///
    /// The cap is applied to [`FactStore::by_recency`] and not to
    /// [`FactStore::all`], which is oldest-first. That ordering matters more
    /// than the number: a cap on the oldest-first order would keep whatever
    /// the owner happened to say first and drop every later correction —
    /// dropping exactly the rows that exist because the earlier ones were
    /// wrong. Recency-first keeps the newest, and a correction moves its fact
    /// back to the front (`remember` writes `updated_at`).
    pub fn matching(&self, text: &str) -> anyhow::Result<Vec<Fact>> {
        let wanted = match_tokens(text);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .by_recency()?
            .into_iter()
            .filter(|fact| match_tokens(&fact.topic).iter().any(|t| wanted.contains(t)))
            .take(MAX_MATCHING_FACTS)
            .collect())
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Fact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM facts WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate)?;
        match rows.next() {
            Some(fact) => Ok(Some(fact?)),
            None => Ok(None),
        }
    }

    /// Delete one fact. `false` when there was nothing under that id, so the
    /// CLI can say "no such fact" rather than claiming a deletion.
    pub fn forget(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute("DELETE FROM facts WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    fn store() -> (tempfile::TempDir, FactStore) {
        let (dir, conn) = temp_store();
        (dir, FactStore::new(conn))
    }

    #[test]
    fn a_remembered_fact_comes_back() {
        let (_dir, facts) = store();
        let stored = facts
            .remember("tenta", "the databases tenta is on the 14th")
            .unwrap();
        assert!(stored.id > 0);
        assert_eq!(stored.topic, "tenta");
        assert_eq!(stored.updated_at, None, "a new fact has not been corrected");

        let all = facts.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].body, "the databases tenta is on the 14th");
    }

    /// Repeated corrections must converge, not pile up: two rows for one topic
    /// would both be injected and the model would see a contradiction.
    #[test]
    fn the_same_topic_twice_updates_rather_than_duplicating() {
        let (_dir, facts) = store();
        let first = facts.remember("tenta", "on the 14th").unwrap();
        let second = facts.remember("Tenta", "moved to the 21st").unwrap();

        assert_eq!(second.id, first.id, "the same topic is the same row");
        let all = facts.all().unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].body, "moved to the 21st");
        assert!(all[0].updated_at.is_some(), "a correction is dated");
        assert_eq!(all[0].created_at, first.created_at, "first learned is kept");
    }

    #[test]
    fn an_empty_body_is_refused() {
        let (_dir, facts) = store();
        facts.remember("tenta", "on the 14th").unwrap();

        let err = facts.remember("tenta", "   ").unwrap_err().to_string();
        assert!(err.contains("body"), "{err}");

        // And the fact it would have erased is still there.
        assert_eq!(facts.all().unwrap()[0].body, "on the 14th");
    }

    #[test]
    fn an_empty_topic_is_refused() {
        let (_dir, facts) = store();
        let err = facts.remember(" ", "something").unwrap_err().to_string();
        assert!(err.contains("topic"), "{err}");
        assert!(facts.all().unwrap().is_empty());
    }

    #[test]
    fn an_oversized_fact_is_refused_rather_than_stored() {
        let (_dir, facts) = store();
        let err = facts
            .remember("tenta", &"x".repeat(MAX_BODY_CHARS + 1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
        let err = facts
            .remember(&"t".repeat(MAX_TOPIC_CHARS + 1), "body")
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
        assert!(facts.all().unwrap().is_empty());
    }

    #[test]
    fn matching_returns_the_facts_whose_topic_shares_a_word() {
        let (_dir, facts) = store();
        facts
            .remember("tenta", "the databases tenta is on the 14th")
            .unwrap();
        facts
            .remember("invoicing address", "Ekonomi AB, Box 12")
            .unwrap();

        let hit = facts
            .matching("remind me what I said about the tenta")
            .unwrap();
        assert_eq!(hit.len(), 1, "{hit:?}");
        assert_eq!(hit[0].topic, "tenta");

        // A multi-word topic matches on any of its words.
        let hit = facts.matching("what is the invoicing situation?").unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].topic, "invoicing address");
    }

    /// A large fact table must not splice an unbounded block into a chat
    /// system prompt, and the bound must keep the facts most likely to be
    /// worth having: the most recently written or corrected ones.
    #[test]
    fn matching_is_bounded_and_keeps_the_most_recently_touched_facts() {
        let (_dir, facts) = store();
        for i in 0..(MAX_MATCHING_FACTS + 5) {
            facts
                .remember(&format!("tenta {i}"), &format!("body {i}"))
                .unwrap();
        }

        let hit = facts.matching("what about the tenta").unwrap();
        assert_eq!(hit.len(), MAX_MATCHING_FACTS, "the block is bounded");
        assert_eq!(
            hit.first().unwrap().topic,
            format!("tenta {}", MAX_MATCHING_FACTS + 4),
            "the newest fact is kept"
        );
        assert_eq!(
            hit.last().unwrap().topic,
            "tenta 5",
            "the five oldest are the ones dropped"
        );
    }

    /// The bound is on recency, not on insertion order: correcting an old
    /// fact is exactly the case where dropping it would be worst, because the
    /// correction is the newest thing the owner said.
    #[test]
    fn a_corrected_fact_is_kept_over_newer_but_untouched_ones() {
        let (_dir, facts) = store();
        for i in 0..(MAX_MATCHING_FACTS + 1) {
            facts
                .remember(&format!("tenta {i}"), &format!("body {i}"))
                .unwrap();
        }
        // The oldest fact falls outside the bound...
        let hit = facts.matching("the tenta").unwrap();
        assert!(!hit.iter().any(|fact| fact.topic == "tenta 0"), "{hit:?}");

        // ...until it is corrected, which is the moment it matters most.
        facts.remember("tenta 0", "moved to the 21st").unwrap();
        let hit = facts.matching("the tenta").unwrap();
        assert_eq!(hit.len(), MAX_MATCHING_FACTS);
        assert_eq!(hit.first().unwrap().topic, "tenta 0");
        assert_eq!(hit.first().unwrap().body, "moved to the 21st");
    }

    #[test]
    fn matching_nothing_returns_nothing() {
        let (_dir, facts) = store();
        facts.remember("tenta", "on the 14th").unwrap();
        assert!(facts.matching("how is the weather").unwrap().is_empty());
        assert!(facts.matching("").unwrap().is_empty());
    }

    /// A one- or two-letter word in a topic must not turn that fact into a
    /// fact about everything.
    #[test]
    fn short_words_are_not_matched_on() {
        let (_dir, facts) = store();
        facts.remember("on the bus", "the 4A goes past").unwrap();
        assert!(
            facts.matching("is it on?").unwrap().is_empty(),
            "`on` and `it` are too short to match"
        );
        assert_eq!(facts.matching("where is the bus").unwrap().len(), 1);
    }

    #[test]
    fn forget_removes_one_and_says_whether_it_existed() {
        let (_dir, facts) = store();
        let id = facts.remember("tenta", "on the 14th").unwrap().id;
        assert!(facts.get(id).unwrap().is_some());
        assert!(facts.forget(id).unwrap());
        assert!(facts.all().unwrap().is_empty());
        assert!(
            !facts.forget(id).unwrap(),
            "a second forget deletes nothing"
        );
        assert!(facts.get(id).unwrap().is_none());
    }
}
