use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, Row};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub kind: String,
    pub prompt: String,
    pub outcome: String,
    pub detail: Option<String>,
    pub action_ids: Vec<i64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

pub struct RunStore {
    conn: Arc<Mutex<Connection>>,
}

fn hydrate(row: &Row<'_>) -> rusqlite::Result<Run> {
    let action_ids: Option<String> = row.get("action_ids")?;
    let action_ids = action_ids
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(Run {
        id: row.get("id")?,
        kind: row.get("kind")?,
        prompt: row.get("prompt")?,
        outcome: row.get("outcome")?,
        detail: row.get("detail")?,
        action_ids,
        cost_usd: row.get("cost_usd")?,
        duration_ms: row.get("duration_ms")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

impl RunStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Starts a run, recording it as `running`. Returns the new run's id, so
    /// the caller can pass it back to [`RunStore::finish`] once the agent
    /// session completes.
    ///
    /// `action_ids` is written **at insert**, not only at
    /// [`RunStore::finish`]. A run that is abandoned -- the process killed
    /// mid-connector-call, the future dropped -- never reaches `finish`, and a
    /// `running` row whose prompt is `"fortnox.record_voucher"` with no id
    /// cannot be tied back to the action it was spending money on. Recording
    /// the ids up front is what makes an orphaned `running` row a usable lead
    /// instead of a shrug. Pass `&[]` for runs that are not about a specific
    /// action (a scheduled agent session, say); `finish` overwrites the column
    /// with whatever the run actually touched.
    pub fn start(&self, kind: &str, prompt: &str, action_ids: &[i64]) -> anyhow::Result<i64> {
        let action_ids_json = serde_json::to_string(action_ids)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (kind, prompt, outcome, action_ids, started_at)
             VALUES (?1,?2,'running',?3,?4)",
            params![kind, prompt, action_ids_json, Utc::now().to_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Run>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM runs WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], hydrate)?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    /// Closes out a run: records the outcome, freeform detail, the action ids
    /// it touched, its cost, and a `duration_ms` computed from `started_at`.
    pub fn finish(
        &self,
        id: i64,
        outcome: &str,
        detail: Option<&str>,
        action_ids: &[i64],
        cost_usd: Option<f64>,
    ) -> anyhow::Result<Run> {
        let started_at: String = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT started_at FROM runs WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|_| anyhow!("run {id} does not exist"))?
        };
        let started: DateTime<Utc> = started_at
            .parse()
            .map_err(|e| anyhow!("run {id} has an unparseable started_at: {e}"))?;
        let now = Utc::now();
        let duration_ms = (now - started).num_milliseconds().max(0);
        let action_ids_json = serde_json::to_string(action_ids)?;

        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE runs SET outcome = ?1, detail = ?2, action_ids = ?3, cost_usd = ?4,
                 duration_ms = ?5, finished_at = ?6 WHERE id = ?7",
                params![
                    outcome,
                    detail,
                    action_ids_json,
                    cost_usd,
                    duration_ms,
                    now.to_rfc3339(),
                    id,
                ],
            )?;
        }
        self.get(id)?.ok_or_else(|| anyhow!("run {id} vanished"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::temp_store;

    #[test]
    fn start_records_a_running_run() {
        let (_dir, conn) = temp_store();
        let store = RunStore::new(conn);
        let id = store.start("scheduled", "check canvas", &[]).unwrap();
        let run = store.get(id).unwrap().unwrap();
        assert_eq!(run.kind, "scheduled");
        assert_eq!(run.prompt, "check canvas");
        assert_eq!(run.outcome, "running");
        assert!(run.finished_at.is_none());
        assert!(run.duration_ms.is_none());
        assert!(run.action_ids.is_empty());
    }

    #[test]
    fn start_records_the_action_ids_before_finish_runs() {
        let (_dir, conn) = temp_store();
        let store = RunStore::new(conn);
        let id = store
            .start("execute", "fortnox.record_voucher", &[7])
            .unwrap();

        // Nothing has finished; this is the state a crash mid-call leaves.
        let run = store.get(id).unwrap().unwrap();
        assert_eq!(run.outcome, "running");
        assert!(run.finished_at.is_none());
        assert_eq!(
            run.action_ids,
            vec![7],
            "an orphaned running row must still name the action it was executing"
        );
    }

    #[test]
    fn finish_records_outcome_detail_actions_and_cost() {
        let (_dir, conn) = temp_store();
        let store = RunStore::new(conn);
        let id = store.start("scheduled", "check canvas", &[]).unwrap();
        let run = store
            .finish(
                id,
                "ok",
                Some("found 2 new assignments"),
                &[1, 2],
                Some(0.014),
            )
            .unwrap();
        assert_eq!(run.outcome, "ok");
        assert_eq!(run.detail.as_deref(), Some("found 2 new assignments"));
        assert_eq!(run.action_ids, vec![1, 2]);
        assert_eq!(run.cost_usd, Some(0.014));
        assert!(run.finished_at.is_some());
    }

    #[test]
    fn finish_computes_a_nonnegative_duration() {
        let (_dir, conn) = temp_store();
        let store = RunStore::new(conn);
        let id = store.start("scheduled", "check canvas", &[]).unwrap();
        let run = store.finish(id, "ok", None, &[], None).unwrap();
        assert!(run.duration_ms.unwrap() >= 0);
    }

    #[test]
    fn finishing_an_unknown_run_fails() {
        let (_dir, conn) = temp_store();
        let store = RunStore::new(conn);
        assert!(store.finish(999, "ok", None, &[], None).is_err());
    }
}
