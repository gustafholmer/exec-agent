//! Claude Code session handoff notes and searchable transcripts.
//!
//! One row per Claude Code `session_id`: a session that ends, resumes and ends
//! again updates the same row and goes back to `pending` until the summary is
//! rewritten. The row type deliberately carries no transcript (it can be
//! 400 kB); [`SessionHit`] carries the summary and a short excerpt instead.

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::timestamp::serialize_rfc3339;

/// Transcript cap in bytes; the tail is kept.
pub const MAX_TRANSCRIPT_BYTES: usize = 400_000;
/// Summary cap in bytes; the head is kept.
pub const MAX_SUMMARY_BYTES: usize = 8_000;
/// Longest search query accepted, in characters.
pub const MAX_QUERY_CHARS: usize = 500;
/// Largest `limit` accepted by [`ClaudeSessionStore::search`].
pub const MAX_LIMIT: i64 = 20;
/// `limit` used when the caller does not give one.
pub const DEFAULT_LIMIT: i64 = 5;

/// Allowed values of `claude_sessions.status`.
pub const STATUSES: [&str; 4] = ["pending", "done", "skipped", "failed"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ClaudeSession {
    pub id: i64,
    pub session_id: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub reason: String,
    pub status: String,
    pub summary: Option<String>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSession {
    pub session_id: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuery {
    /// Empty or whitespace-only means "no text filter, newest first".
    pub query: String,
    pub cwd: Option<String>,
    pub since: Option<DateTime<Utc>>,
    /// `None` means [`DEFAULT_LIMIT`]; must be within `1..=`[`MAX_LIMIT`].
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct SessionHit {
    pub session_id: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub status: String,
    pub summary: Option<String>,
    /// `ts_headline` of the transcript; `None` for an empty query.
    pub excerpt: Option<String>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "serialize_rfc3339")]
    pub updated_at: DateTime<Utc>,
}

/// The last `max_bytes` bytes of `s`, moved forward to a char boundary.
pub fn cap_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// The first `max_bytes` bytes of `s`, cut back to a char boundary.
fn cap_head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

const SESSION_COLUMNS: &str =
    "id, session_id, cwd, git_branch, reason, status, summary, created_at, updated_at";

#[derive(Clone)]
pub struct ClaudeSessionStore {
    pool: PgPool,
}

impl ClaudeSessionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record that a session ended. Inserts, or on an existing `session_id`
    /// updates cwd/branch/reason and resets the row to `pending`.
    pub async fn upsert(&self, new: &NewSession) -> anyhow::Result<ClaudeSession> {
        if new.session_id.trim().is_empty() {
            bail!("upsert: `session_id` must not be empty");
        }
        if new.cwd.trim().is_empty() {
            bail!("upsert: `cwd` must not be empty");
        }
        sqlx::query_as::<_, ClaudeSession>(&format!(
            "INSERT INTO claude_sessions (session_id, cwd, git_branch, reason) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (session_id) DO UPDATE SET \
               cwd = EXCLUDED.cwd, git_branch = EXCLUDED.git_branch, \
               reason = EXCLUDED.reason, status = 'pending', updated_at = now() \
             RETURNING {SESSION_COLUMNS}"
        ))
        .bind(&new.session_id)
        .bind(&new.cwd)
        .bind(&new.git_branch)
        .bind(&new.reason)
        .fetch_one(&self.pool)
        .await
        .context("upsert: writing claude_sessions row")
    }

    /// Store the outcome of summarising. Returns `false` when no such session.
    /// The summary keeps its head (8000 bytes), the transcript its tail
    /// (400000 bytes).
    pub async fn finish(
        &self,
        session_id: &str,
        status: &str,
        summary: Option<&str>,
        transcript: Option<&str>,
    ) -> anyhow::Result<bool> {
        if !STATUSES.contains(&status) {
            bail!(
                "finish: `status` must be one of {}, got `{status}`",
                STATUSES.join(", ")
            );
        }
        let summary = summary.map(|s| cap_head(s, MAX_SUMMARY_BYTES));
        let transcript = transcript.map(|t| cap_tail(t, MAX_TRANSCRIPT_BYTES));
        let result = sqlx::query(
            "UPDATE claude_sessions \
             SET status = $2, summary = $3, transcript = $4, updated_at = now() \
             WHERE session_id = $1",
        )
        .bind(session_id)
        .bind(status)
        .bind(summary)
        .bind(transcript)
        .execute(&self.pool)
        .await
        .context("finish: updating claude_sessions row")?;
        Ok(result.rows_affected() > 0)
    }

    /// Newest session in `cwd`, optionally skipping the session that is
    /// asking. By default only finished sessions with a summary count; with
    /// `include_pending` the newest pending or done row wins, so a caller can
    /// wait for a summary that is still being written.
    pub async fn latest_for_cwd(
        &self,
        cwd: &str,
        exclude_session: Option<&str>,
        include_pending: bool,
    ) -> anyhow::Result<Option<ClaudeSession>> {
        sqlx::query_as::<_, ClaudeSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM claude_sessions \
             WHERE cwd = $1 \
               AND (($3 AND status IN ('pending', 'done')) \
                    OR (status = 'done' AND summary IS NOT NULL)) \
               AND ($2::text IS NULL OR session_id <> $2) \
             ORDER BY updated_at DESC, id DESC LIMIT 1"
        ))
        .bind(cwd)
        .bind(exclude_session)
        .bind(include_pending)
        .fetch_optional(&self.pool)
        .await
        .context("latest_for_cwd: querying claude_sessions")
    }

    /// Full-text search over summary (weight A) and transcript (weight B).
    /// An empty query lists the newest sessions matching the filters.
    pub async fn search(&self, q: &SessionQuery) -> anyhow::Result<Vec<SessionHit>> {
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            bail!("search: `limit` must be between 1 and {MAX_LIMIT}, got {limit}");
        }
        if q.query.chars().count() > MAX_QUERY_CHARS {
            bail!("search: `query` is longer than {MAX_QUERY_CHARS} characters");
        }
        let text = q.query.trim();
        let sql = if text.is_empty() {
            "SELECT session_id, cwd, git_branch, status, summary, \
                    NULL::text AS excerpt, created_at, updated_at \
             FROM claude_sessions \
             WHERE ($2::text IS NULL OR cwd = $2) \
               AND ($3::timestamptz IS NULL OR created_at >= $3) \
             ORDER BY updated_at DESC, id DESC LIMIT $4"
        } else {
            "SELECT s.session_id, s.cwd, s.git_branch, s.status, s.summary, \
                    ts_headline('simple', coalesce(s.transcript, ''), q.tsq, \
                      'StartSel=«, StopSel=», MaxFragments=2,MaxWords=30,MinWords=10') AS excerpt, \
                    s.created_at, s.updated_at \
             FROM claude_sessions s, websearch_to_tsquery('simple', $1) AS q(tsq) \
             WHERE s.search @@ q.tsq \
               AND ($2::text IS NULL OR s.cwd = $2) \
               AND ($3::timestamptz IS NULL OR s.created_at >= $3) \
             ORDER BY ts_rank(s.search, q.tsq) DESC, s.created_at DESC, s.id DESC \
             LIMIT $4"
        };
        let mut query = sqlx::query_as::<_, SessionHit>(sql);
        if !text.is_empty() {
            query = query.bind(text);
        } else {
            // Keep placeholder numbering identical across both statements.
            query = query.bind(None::<String>);
        }
        query
            .bind(&q.cwd)
            .bind(q.since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .context("search: querying claude_sessions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(id: &str, cwd: &str) -> NewSession {
        NewSession {
            session_id: id.into(),
            cwd: cwd.into(),
            git_branch: Some("main".into()),
            reason: "other".into(),
        }
    }

    fn q(query: &str) -> SessionQuery {
        SessionQuery {
            query: query.into(),
            cwd: None,
            since: None,
            limit: None,
        }
    }

    #[test]
    fn cap_tail_keeps_short_strings_and_the_tail_of_long_ones() {
        assert_eq!(cap_tail("hello", 10), "hello");
        assert_eq!(cap_tail("hello", 3), "llo");
        assert_eq!(cap_tail("hello", 0), "");
    }

    #[test]
    fn cap_tail_moves_forward_to_a_char_boundary() {
        // "aéb" is 4 bytes: a, (2-byte é), b. Last 2 bytes start inside é.
        assert_eq!(cap_tail("aéb", 2), "b");
        assert_eq!(cap_tail("aéb", 3), "éb");
        assert_eq!(cap_tail("€€", 4), "€");
    }

    #[test]
    fn cap_head_cuts_back_to_a_char_boundary() {
        assert_eq!(cap_head("aéb", 2), "a");
        assert_eq!(cap_head("aéb", 3), "aé");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn upsert_is_idempotent_and_resets_status(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        let first = store.upsert(&new("s1", "/a")).await.unwrap();
        assert_eq!(first.status, "pending");
        assert!(store.finish("s1", "done", Some("sum"), None).await.unwrap());
        let mut again = new("s1", "/b");
        again.git_branch = None;
        again.reason = "clear".into();
        let second = store.upsert(&again).await.unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.created_at, first.created_at);
        assert_eq!(second.status, "pending");
        assert_eq!(second.cwd, "/b");
        assert_eq!(second.git_branch, None);
        assert_eq!(second.reason, "clear");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM claude_sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn upsert_rejects_empty_session_id_and_cwd(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        let err = store.upsert(&new(" ", "/a")).await.unwrap_err().to_string();
        assert!(err.starts_with("upsert:"), "{err}");
        assert!(store.upsert(&new("s", "")).await.is_err());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finish_stores_summary_and_transcript_with_caps(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        store.upsert(&new("s1", "/a")).await.unwrap();
        let summary = "s".repeat(MAX_SUMMARY_BYTES + 100);
        let transcript = format!("{}TAIL", "t".repeat(MAX_TRANSCRIPT_BYTES));
        assert!(store
            .finish("s1", "done", Some(&summary), Some(&transcript))
            .await
            .unwrap());
        let (status, sum, tr): (String, String, String) = sqlx::query_as(
            "SELECT status, summary, transcript FROM claude_sessions WHERE session_id = 's1'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(status, "done");
        assert_eq!(sum.len(), MAX_SUMMARY_BYTES);
        assert_eq!(tr.len(), MAX_TRANSCRIPT_BYTES);
        assert!(tr.ends_with("TAIL"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn finish_rejects_bad_status_and_reports_unknown_session(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        assert!(!store.finish("nope", "done", None, None).await.unwrap());
        store.upsert(&new("s1", "/a")).await.unwrap();
        let err = store
            .finish("s1", "bogus", None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("finish:"), "{err}");
        for ok in STATUSES {
            assert!(store.finish("s1", ok, None, None).await.unwrap());
        }
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn latest_for_cwd_excludes_current_and_ignores_pending(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        assert!(store
            .latest_for_cwd("/a", None, false)
            .await
            .unwrap()
            .is_none());
        store.upsert(&new("old", "/a")).await.unwrap();
        store
            .finish("old", "done", Some("old note"), None)
            .await
            .unwrap();
        store.upsert(&new("cur", "/a")).await.unwrap();
        store
            .finish("cur", "done", Some("cur note"), None)
            .await
            .unwrap();
        store.upsert(&new("pend", "/a")).await.unwrap();
        store.upsert(&new("other", "/b")).await.unwrap();
        store
            .finish("other", "done", Some("other"), None)
            .await
            .unwrap();

        let latest = store
            .latest_for_cwd("/a", None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.session_id, "cur");
        let latest = store
            .latest_for_cwd("/a", Some("cur"), false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.session_id, "old");
        assert!(store
            .latest_for_cwd("/a", Some("old"), false)
            .await
            .unwrap()
            .is_some());
        // Done but no summary does not count.
        store.finish("cur", "done", None, None).await.unwrap();
        store
            .finish("old", "skipped", Some("x"), None)
            .await
            .unwrap();
        assert!(store
            .latest_for_cwd("/a", None, false)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn latest_for_cwd_orders_by_last_update_so_a_resumed_session_wins(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        store.upsert(&new("a", "/a")).await.unwrap();
        store
            .finish("a", "done", Some("first"), None)
            .await
            .unwrap();
        store.upsert(&new("b", "/a")).await.unwrap();
        store
            .finish("b", "done", Some("second"), None)
            .await
            .unwrap();
        // Resume A: the upsert keeps created_at but bumps updated_at.
        store.upsert(&new("a", "/a")).await.unwrap();
        let latest = store
            .latest_for_cwd("/a", None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.session_id, "a");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn latest_for_cwd_include_pending_sees_pending_rows(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        store.upsert(&new("old", "/a")).await.unwrap();
        store
            .finish("old", "done", Some("old note"), None)
            .await
            .unwrap();
        store.upsert(&new("pend", "/a")).await.unwrap();
        store.upsert(&new("skip", "/a")).await.unwrap();
        store.finish("skip", "skipped", None, None).await.unwrap();

        let plain = store.latest_for_cwd("/a", None, false).await.unwrap();
        assert_eq!(plain.unwrap().session_id, "old");
        let with = store
            .latest_for_cwd("/a", None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with.session_id, "pend");
        assert_eq!(with.status, "pending");
        let excl = store
            .latest_for_cwd("/a", Some("pend"), true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(excl.session_id, "old");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn search_ranks_summary_match_above_transcript_only(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        store.upsert(&new("t", "/a")).await.unwrap();
        store
            .finish(
                "t",
                "done",
                Some("unrelated"),
                Some("we discussed zebra crossing"),
            )
            .await
            .unwrap();
        store.upsert(&new("s", "/a")).await.unwrap();
        store
            .finish("s", "done", Some("zebra handling"), Some("nothing"))
            .await
            .unwrap();
        // Make the transcript-only row the newer one so recency cannot explain the order.
        sqlx::query("UPDATE claude_sessions SET created_at = now() + interval '1 hour' WHERE session_id = 't'")
            .execute(&store.pool)
            .await
            .unwrap();
        let hits = store.search(&q("zebra")).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, ["s", "t"]);
        let t = &hits[1];
        assert!(t.excerpt.as_deref().unwrap().contains("«zebra»"));
        assert!(store.search(&q("giraffe")).await.unwrap().is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn search_filters_by_cwd_and_since(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        for (id, cwd) in [("a", "/a"), ("b", "/b")] {
            store.upsert(&new(id, cwd)).await.unwrap();
            store
                .finish(id, "done", Some("apple pie"), None)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE claude_sessions SET created_at = now() - interval '10 days' WHERE session_id = 'b'")
            .execute(&store.pool)
            .await
            .unwrap();

        let mut query = q("apple");
        query.cwd = Some("/b".into());
        let hits = store.search(&query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "b");

        let mut query = q("apple");
        query.since = Some(Utc::now() - chrono::Duration::days(1));
        let hits = store.search(&query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "a");
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn empty_query_returns_newest_first_without_excerpt(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        for id in ["one", "two", "three"] {
            store.upsert(&new(id, "/a")).await.unwrap();
            store
                .finish(id, "done", Some("x"), Some("y"))
                .await
                .unwrap();
        }
        let mut query = q("   ");
        query.limit = Some(2);
        let hits = store.search(&query).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, ["three", "two"]);
        assert!(hits.iter().all(|h| h.excerpt.is_none()));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn search_rejects_bad_limit_and_long_query(pool: PgPool) {
        let store = ClaudeSessionStore::new(pool);
        for bad in [0, -1, 21] {
            let mut query = q("x");
            query.limit = Some(bad);
            let err = store.search(&query).await.unwrap_err().to_string();
            assert!(err.starts_with("search:"), "{err}");
        }
        let mut query = q("x");
        query.limit = Some(20);
        assert!(store.search(&query).await.is_ok());
        assert!(store.search(&q(&"x".repeat(501))).await.is_err());
        assert!(store.search(&q(&"x".repeat(500))).await.is_ok());
    }

    #[sqlx::test(migrations = false)]
    async fn migration_copies_latest_row_per_session_and_drops_old_schema(pool: PgPool) {
        sqlx::raw_sql(
            "CREATE SCHEMA claude_code;
             CREATE TABLE claude_code.session_summaries (
               id bigserial PRIMARY KEY,
               session_id text NOT NULL,
               cwd text NOT NULL,
               git_branch text,
               reason text NOT NULL,
               transcript_path text NOT NULL,
               summary text,
               status text NOT NULL DEFAULT 'pending',
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             INSERT INTO claude_code.session_summaries
               (session_id, cwd, reason, transcript_path, summary, status) VALUES
               ('s1', '/a', 'other', '', 'first', 'done'),
               ('s1', '/a2', 'clear', '', 'second', 'done'),
               ('s2', '/b', 'other', '', 'only', 'done'),
               ('', '/c', 'other', '', 'blank id', 'done');",
        )
        .execute(&pool)
        .await
        .unwrap();

        crate::db::MIGRATOR.run(&pool).await.unwrap();

        let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT session_id, cwd, summary, transcript FROM claude_sessions ORDER BY session_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                ("s1".into(), "/a2".into(), Some("second".into()), None),
                ("s2".into(), "/b".into(), Some("only".into()), None),
            ]
        );
        let schema: Option<String> = sqlx::query_scalar(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'claude_code'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(schema.is_none());
    }
}
