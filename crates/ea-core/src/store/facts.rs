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
//!   thing it corrected. The uniqueness is enforced by the `facts_topic_lower`
//!   unique index (case-insensitive on `topic`), and `remember` is a single
//!   `INSERT ... ON CONFLICT` against it, so a repeated topic can only ever
//!   update the one row already there.
//! * **An empty body is refused.** `remember("tenta", "")` is a fact that says
//!   nothing and would silently destroy the one it overwrote.
//! * **Injection into a prompt is selective.** [`FactStore::matching`] returns
//!   the facts whose topic shares a word with what the user just said, so a
//!   conversation about the tenta does not carry the invoicing address, and a
//!   conversation that matches nothing carries no facts block at all — and it
//!   is bounded at [`MAX_MATCHING_FACTS`], newest first, so a table that grows
//!   cannot grow the prompt with it.

use std::collections::BTreeSet;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::timestamp::{serialize_rfc3339, serialize_rfc3339_opt};
use sqlx::PgPool;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Fact {
    pub id: i64,
    pub topic: String,
    pub body: String,
    /// `+00:00`, not `Z`, on the wire: the `facts`/`remember` IPC responses
    /// and `ea facts` print what they always printed. See `store::timestamp`.
    #[serde(serialize_with = "serialize_rfc3339")]
    pub created_at: DateTime<Utc>,
    /// When `remember` last overwrote the body. `None` on a fact that has
    /// never been corrected, and on any row predating the column.
    #[serde(serialize_with = "serialize_rfc3339_opt")]
    pub updated_at: Option<DateTime<Utc>>,
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
    pool: PgPool,
}

impl FactStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a fact, replacing any fact already under that topic.
    ///
    /// Topic matching is case-insensitive: "Tenta" and "tenta" are the same
    /// shelf, because the model will not spell it the same way twice. This is
    /// one upsert against the `facts_topic_lower` unique index: `ON CONFLICT`
    /// rewrites `topic` and `body` and stamps `updated_at`, leaving
    /// `created_at` untouched so it keeps saying when the fact was first
    /// learned.
    pub async fn remember(&self, topic: &str, body: &str) -> anyhow::Result<Fact> {
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

        let fact = sqlx::query_as::<_, Fact>(
            "INSERT INTO facts (topic, body) VALUES ($1, $2)
             ON CONFLICT (lower(topic)) DO UPDATE SET
               topic      = EXCLUDED.topic,
               body       = EXCLUDED.body,
               updated_at = now()
             RETURNING id, topic, body, created_at, updated_at",
        )
        .bind(topic)
        .bind(body)
        .fetch_one(&self.pool)
        .await
        .context("remembering a fact")?;
        Ok(fact)
    }

    /// Every fact, oldest first — what `ea facts` prints.
    pub async fn all(&self) -> anyhow::Result<Vec<Fact>> {
        sqlx::query_as::<_, Fact>(
            "SELECT id, topic, body, created_at, updated_at FROM facts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("listing facts")
    }

    /// Every fact, most recently written or corrected first.
    ///
    /// The order is total and deterministic: `updated_at` when the fact has
    /// been corrected and `created_at` when it has not, and the id to break a
    /// tie between two facts written in the same instant. Without the
    /// tie-break, [`MAX_MATCHING_FACTS`] would cut a different set of facts
    /// from one run to the next.
    async fn by_recency(&self) -> anyhow::Result<Vec<Fact>> {
        sqlx::query_as::<_, Fact>(
            "SELECT id, topic, body, created_at, updated_at FROM facts
             ORDER BY COALESCE(updated_at, created_at) DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("listing facts by recency")
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
    pub async fn matching(&self, text: &str) -> anyhow::Result<Vec<Fact>> {
        let wanted = match_tokens(text);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .by_recency()
            .await?
            .into_iter()
            .filter(|fact| match_tokens(&fact.topic).iter().any(|t| wanted.contains(t)))
            .take(MAX_MATCHING_FACTS)
            .collect())
    }

    pub async fn get(&self, id: i64) -> anyhow::Result<Option<Fact>> {
        sqlx::query_as::<_, Fact>(
            "SELECT id, topic, body, created_at, updated_at FROM facts WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("reading a fact")
    }

    /// Delete one fact. `false` when there was nothing under that id, so the
    /// CLI can say "no such fact" rather than claiming a deletion.
    pub async fn forget(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM facts WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("deleting a fact")?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Important 4 of the final review: chrono's default would print `Z`;
    /// the IPC responses and `ea facts` have always printed `+00:00`.
    #[test]
    fn timestamps_serialize_as_rfc3339() {
        let at: DateTime<Utc> = "2026-09-20T18:00:00.123456Z".parse().unwrap();
        let fact = Fact {
            id: 1,
            topic: "tenta".into(),
            body: "on the 14th".into(),
            created_at: at,
            updated_at: Some(at),
        };
        let json = serde_json::to_value(&fact).unwrap();
        assert_eq!(json["created_at"], "2026-09-20T18:00:00.123456+00:00");
        assert_eq!(json["updated_at"], "2026-09-20T18:00:00.123456+00:00");

        let never = Fact { updated_at: None, ..fact };
        assert!(serde_json::to_value(&never).unwrap()["updated_at"].is_null());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_remembered_fact_comes_back(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        let stored = facts
            .remember("tenta", "the databases tenta is on the 14th")
            .await
            .unwrap();
        assert!(stored.id > 0);
        assert_eq!(stored.topic, "tenta");
        assert_eq!(stored.updated_at, None, "a new fact has not been corrected");

        let all = facts.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].body, "the databases tenta is on the 14th");
    }

    /// Repeated corrections must converge, not pile up: two rows for one topic
    /// would both be injected and the model would see a contradiction.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn the_same_topic_twice_updates_rather_than_duplicating(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        let first = facts.remember("tenta", "on the 14th").await.unwrap();
        let second = facts
            .remember("Tenta", "moved to the 21st")
            .await
            .unwrap();

        assert_eq!(second.id, first.id, "the same topic is the same row");
        let all = facts.all().await.unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].body, "moved to the 21st");
        assert!(all[0].updated_at.is_some(), "a correction is dated");
        assert_eq!(all[0].created_at, first.created_at, "first learned is kept");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn remember_replaces_the_body_under_the_same_topic_ignoring_case(
        pool: sqlx::PgPool,
    ) {
        let store = FactStore::new(pool);
        let first = store.remember("Tenta", "on the 14th").await.unwrap();
        let second = store.remember("tenta", "moved to the 21st").await.unwrap();
        assert_eq!(first.id, second.id, "a correction must reuse the row");
        assert_eq!(second.body, "moved to the 21st");
        assert_eq!(store.all().await.unwrap().len(), 1);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_first_remember_leaves_updated_at_null(pool: sqlx::PgPool) {
        let store = FactStore::new(pool);
        let fact = store.remember("tenta", "on the 14th").await.unwrap();
        assert!(fact.updated_at.is_none(), "nothing has corrected this yet");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_correction_stamps_updated_at_and_keeps_created_at(pool: sqlx::PgPool) {
        let store = FactStore::new(pool);
        let first = store.remember("tenta", "on the 14th").await.unwrap();
        let second = store.remember("tenta", "the 21st").await.unwrap();
        assert!(second.updated_at.is_some(), "a correction is stamped");
        crate::store::test_support::assert_same_instant(first.created_at, second.created_at);
    }

    /// The topic is stored as last written, not as first written: correcting
    /// "tenta" as "Tenta" should update the display casing.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_correction_updates_the_topics_casing(pool: sqlx::PgPool) {
        let store = FactStore::new(pool);
        store.remember("tenta", "on the 14th").await.unwrap();
        let second = store.remember("Tenta", "the 21st").await.unwrap();
        assert_eq!(second.topic, "Tenta");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_empty_body_is_refused(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        facts.remember("tenta", "on the 14th").await.unwrap();

        let err = facts
            .remember("tenta", "   ")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("body"), "{err}");

        // And the fact it would have erased is still there.
        assert_eq!(facts.all().await.unwrap()[0].body, "on the 14th");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_empty_topic_is_refused(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        let err = facts
            .remember(" ", "something")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("topic"), "{err}");
        assert!(facts.all().await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_oversized_fact_is_refused_rather_than_stored(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        let err = facts
            .remember("tenta", &"x".repeat(MAX_BODY_CHARS + 1))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
        let err = facts
            .remember(&"t".repeat(MAX_TOPIC_CHARS + 1), "body")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
        assert!(facts.all().await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn matching_returns_the_facts_whose_topic_shares_a_word(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        facts
            .remember("tenta", "the databases tenta is on the 14th")
            .await
            .unwrap();
        facts
            .remember("invoicing address", "Ekonomi AB, Box 12")
            .await
            .unwrap();

        let hit = facts
            .matching("remind me what I said about the tenta")
            .await
            .unwrap();
        assert_eq!(hit.len(), 1, "{hit:?}");
        assert_eq!(hit[0].topic, "tenta");

        // A multi-word topic matches on any of its words.
        let hit = facts
            .matching("what is the invoicing situation?")
            .await
            .unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].topic, "invoicing address");
    }

    /// A large fact table must not splice an unbounded block into a chat
    /// system prompt, and the bound must keep the facts most likely to be
    /// worth having: the most recently written or corrected ones.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn matching_is_bounded_and_keeps_the_most_recently_touched_facts(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        for i in 0..(MAX_MATCHING_FACTS + 5) {
            facts
                .remember(&format!("tenta {i}"), &format!("body {i}"))
                .await
                .unwrap();
        }

        let hit = facts.matching("what about the tenta").await.unwrap();
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
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_corrected_fact_is_kept_over_newer_but_untouched_ones(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        for i in 0..(MAX_MATCHING_FACTS + 1) {
            facts
                .remember(&format!("tenta {i}"), &format!("body {i}"))
                .await
                .unwrap();
        }
        // The oldest fact falls outside the bound...
        let hit = facts.matching("the tenta").await.unwrap();
        assert!(!hit.iter().any(|fact| fact.topic == "tenta 0"), "{hit:?}");

        // ...until it is corrected, which is the moment it matters most.
        facts
            .remember("tenta 0", "moved to the 21st")
            .await
            .unwrap();
        let hit = facts.matching("the tenta").await.unwrap();
        assert_eq!(hit.len(), MAX_MATCHING_FACTS);
        assert_eq!(hit.first().unwrap().topic, "tenta 0");
        assert_eq!(hit.first().unwrap().body, "moved to the 21st");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn matching_nothing_returns_nothing(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        facts.remember("tenta", "on the 14th").await.unwrap();
        assert!(facts.matching("how is the weather").await.unwrap().is_empty());
        assert!(facts.matching("").await.unwrap().is_empty());
    }

    /// A one- or two-letter word in a topic must not turn that fact into a
    /// fact about everything.
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn short_words_are_not_matched_on(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        facts
            .remember("on the bus", "the 4A goes past")
            .await
            .unwrap();
        assert!(
            facts.matching("is it on?").await.unwrap().is_empty(),
            "`on` and `it` are too short to match"
        );
        assert_eq!(facts.matching("where is the bus").await.unwrap().len(), 1);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn forget_removes_one_and_says_whether_it_existed(pool: sqlx::PgPool) {
        let facts = FactStore::new(pool);
        let id = facts.remember("tenta", "on the 14th").await.unwrap().id;
        assert!(facts.get(id).await.unwrap().is_some());
        assert!(facts.forget(id).await.unwrap());
        assert!(facts.all().await.unwrap().is_empty());
        assert!(
            !facts.forget(id).await.unwrap(),
            "a second forget deletes nothing"
        );
        assert!(facts.get(id).await.unwrap().is_none());
    }
}
