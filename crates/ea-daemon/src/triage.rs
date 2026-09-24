//! Triage: deciding which of the day's events are worth a model's attention,
//! and then what each of them is worth.
//!
//! Two tiers, and the split between them is economic rather than aesthetic.
//!
//! * **Tier 0** is pure Rust: a mute list and a keyword rescue. It costs
//!   nothing, runs on every event, and exists to keep the routine noise --
//!   newsletters, calendar chatter, a connector that polls itself -- from ever
//!   reaching a model. Nothing here is clever, and it must not become clever:
//!   the moment tier 0 needs judgement it is tier 1's job.
//! * **Tier 1** is one `claude -p` session that scores a *batch* of up to
//!   [`TIER1_BATCH`] events in a single call. Batching is the whole point. The
//!   dominant cost of a session is its cached prompt prefix -- the CLI's own
//!   system prompt and tool definitions, ~16k tokens -- which is paid once per
//!   *session*, not once per event. Forty events in one session pay it once;
//!   forty sessions pay it forty times for the same work. There is a test that
//!   asserts the call count for exactly this reason.
//!
//! ## The model
//!
//! Tier 1 runs on [`TIER1_MODEL`] (Haiku), set explicitly rather than left to
//! the CLI. Task 8 measured that an unset model is inherited from the human's
//! `~/.claude/settings.json` -- `opus[1m]` on this machine -- and that
//! `--setting-sources ''` does *not* clear it: $0.0432 per trivial call on
//! opus-5[1m] against $0.0189 on Haiku. Tier 1 is the highest-frequency model
//! call in the system; it fires every few minutes, forever, and the task it
//! performs is classification, which is what the small models are for. Leaving
//! the model unset here would make the cheapest job in the system run on the
//! most expensive model available, and would silently change price whenever the
//! human changed their own editor setting.
//!
//! ## No connectors
//!
//! Tier 1 is scoped to an empty connector set. It scores events that are
//! already in the database; it does not fetch, and it does not act. A session
//! with no connectors cannot do either, which is cheaper to guarantee than to
//! check.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use ea_core::store::events::Event;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::{SessionOutcome, SessionRequest, SessionRunner};

/// How many events one tier-1 session scores. See the module docs for why this
/// is a batch at all.
///
/// Forty is a compromise between the fixed cost of a session (paid once per
/// batch, so bigger is cheaper) and the quality of the answer (a model asked to
/// score two hundred items in one turn starts skipping them). It is a starting
/// value, to be tuned against real batches.
pub const TIER1_BATCH: usize = 40;

/// The model tier 1 runs on.
///
/// Set deliberately. See the module docs: unset means "whatever the human last
/// picked for interactive work", which on this machine is opus-5[1m] at roughly
/// 2.3x the price for a classification task.
pub const TIER1_MODEL: &str = "claude-haiku-4-5";

/// `kind` recorded on the `runs` row of a tier-1 session.
pub const TIER1_RUN_KIND: &str = "triage.tier1";

/// Per-event payload budget in the tier-1 prompt, in characters.
///
/// An event payload is whatever a connector chose to store; one runaway email
/// thread should not push the other thirty-nine events out of the prompt (or
/// out of the model's attention). Truncation is marked so the model can see
/// that it is reading a fragment.
const PAYLOAD_BUDGET: usize = 1_200;

// ---------------------------------------------------------------------------
// Tier 0
// ---------------------------------------------------------------------------

/// The free filter: what never reaches a model.
///
/// `keywords` is a *rescue* list, not a select list. An event matching a
/// keyword survives a muted source or a muted kind -- "mute the newsletter,
/// except when it says `invoice`". It does not rescue an already-triaged event,
/// which is dropped because the work is done, not because it is noise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier0Rules {
    pub muted_sources: Vec<String>,
    pub muted_kinds: Vec<String>,
    pub keywords: Vec<String>,
}

impl Tier0Rules {
    /// Does any keyword appear anywhere in the event's payload?
    ///
    /// Matching is case-insensitive and runs over the payload serialised as
    /// JSON, so it sees nested values and object keys alike. That is
    /// deliberately blunt: a keyword list is a human's escape hatch, and a
    /// human writing `invoice` means "if this event mentions an invoice
    /// anywhere", not "if the top-level `subject` field contains it".
    ///
    /// Note what it does *not* search: `source` and `kind`. If it did, muting
    /// the kind `newsletter` and keywording `newsletter` would cancel out.
    fn payload_matches(&self, event: &Event) -> Option<&str> {
        if self.keywords.is_empty() {
            return None;
        }
        let haystack = serde_json::to_string(&event.payload)
            .unwrap_or_default()
            .to_lowercase();
        self.keywords
            .iter()
            .find(|keyword| {
                let needle = keyword.to_lowercase();
                !needle.is_empty() && haystack.contains(&needle)
            })
            .map(String::as_str)
    }
}

/// Partition events into those worth scoring and those dropped, each with the
/// reason it was dropped.
///
/// The reasons are kept rather than discarded because "why did I not hear about
/// that?" is the first question anyone asks of a triage system, and a mute rule
/// that quietly eats an event is indistinguishable from a bug.
pub fn tier0(events: Vec<Event>, rules: &Tier0Rules) -> (Vec<Event>, Vec<(Event, String)>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();

    for event in events {
        // First and unconditionally: a triaged event is finished, and a
        // keyword does not un-finish it. (The store clears `triaged_at` when a
        // payload changes, so a genuinely new version comes back through here.)
        if event.triaged_at.is_some() {
            dropped.push((event, "already triaged".to_string()));
            continue;
        }

        let muted = if rules.muted_sources.contains(&event.source) {
            Some(format!("muted source `{}`", event.source))
        } else if rules.muted_kinds.contains(&event.kind) {
            Some(format!("muted kind `{}`", event.kind))
        } else {
            None
        };

        match muted {
            None => kept.push(event),
            Some(reason) => match rules.payload_matches(&event) {
                Some(_keyword) => kept.push(event),
                None => dropped.push((event, reason)),
            },
        }
    }

    (kept, dropped)
}

// ---------------------------------------------------------------------------
// Tier 1
// ---------------------------------------------------------------------------

/// One event's score, as tier 1 returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Salience {
    pub event_id: i64,
    /// 0-100. Clamped on the way in: see [`tier1`].
    pub salience: u8,
    pub why: String,
    pub suggested_next_step: String,
}

/// The JSON Schema handed to `--json-schema`, which makes the CLI validate the
/// model's answer and hand it back in `structured_output` as a real JSON value
/// (Task 8 verified both).
///
/// The schema states the 0-100 bound, and [`tier1`] clamps to it anyway. That
/// is not redundancy for its own sake: the CLI validates against whatever
/// version of the schema *it* understands, and a model that answers 150 must
/// produce a usable batch rather than an error that discards the other
/// thirty-nine scores.
pub const SALIENCE_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "scores": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "event_id": {
            "type": "integer",
            "description": "The id of the event being scored, copied exactly from the input."
          },
          "salience": {
            "type": "integer",
            "minimum": 0,
            "maximum": 100,
            "description": "How much this deserves the human's attention right now. 0 is noise, 100 is drop-everything."
          },
          "why": {
            "type": "string",
            "description": "One sentence, in plain language, for the human."
          },
          "suggested_next_step": {
            "type": "string",
            "description": "The single concrete action that would resolve this, or an empty string if none is needed."
          }
        },
        "required": ["event_id", "salience", "why", "suggested_next_step"],
        "additionalProperties": false
      }
    }
  },
  "required": ["scores"],
  "additionalProperties": false
}"#;

/// [`SALIENCE_SCHEMA`] as a `Value`, for [`SessionRequest::with_json_schema`].
pub fn salience_schema() -> Value {
    serde_json::from_str(SALIENCE_SCHEMA).expect("SALIENCE_SCHEMA is a compile-time constant")
}

/// What tier 1 tells the model it is.
pub const TIER1_SYSTEM_PROMPT: &str = concat!(
    "You are the triage stage of a personal executive assistant. ",
    "You are given a batch of events that have already been collected; you do not fetch anything ",
    "and you do not act. Score every event in the batch and nothing else. ",
    "Score on how much this deserves the human's attention right now: a hard deadline close at ",
    "hand, money, or a person waiting on a reply scores high; an FYI, a routine notification or ",
    "something already handled scores low. Copy each event_id exactly as given. ",
    "Return one entry per event, in the order the events were given.",
);

/// The boundary between triage and a real `claude` subprocess.
///
/// A trait rather than a concrete [`SessionRunner`] so that the tier-1 tests
/// can count calls and hand back canned answers without spawning anything. No
/// test in this module may start a real session: the cost is real money and the
/// answer is nondeterministic.
///
/// Boxed future rather than `async fn` in the trait so the trait stays
/// dyn-compatible -- the scheduler will hold one of these behind a pointer.
pub trait SessionBoundary: Send + Sync {
    fn run_session(&self, req: SessionRequest) -> BoxedSession<'_>;
}

/// The future [`SessionBoundary::run_session`] returns.
pub type BoxedSession<'a> = Pin<Box<dyn Future<Output = Result<SessionOutcome>> + Send + 'a>>;

impl SessionBoundary for SessionRunner {
    fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
        Box::pin(self.run(req))
    }
}

/// The raw shape of the model's answer.
///
/// Deliberately looser than [`Salience`]: `salience` arrives as an `f64` so
/// that `150`, `-3` and `72.4` all deserialise and get clamped, rather than
/// failing the whole batch on one bad number. `why` and `suggested_next_step`
/// default to empty for the same reason.
#[derive(Debug, Deserialize)]
struct RawScores {
    #[serde(default)]
    scores: Vec<RawSalience>,
}

#[derive(Debug, Deserialize)]
struct RawSalience {
    event_id: i64,
    salience: f64,
    #[serde(default)]
    why: String,
    #[serde(default)]
    suggested_next_step: String,
}

impl RawSalience {
    fn into_salience(self) -> Salience {
        // NaN clamps to 0: `f64::clamp` panics on a NaN bound but not on a NaN
        // value, and `as u8` on a NaN is 0 -- relying on that silently would be
        // a trap for the next reader, so it is spelled out.
        let score = if self.salience.is_nan() {
            0.0
        } else {
            self.salience.clamp(0.0, 100.0)
        };
        Salience {
            event_id: self.event_id,
            salience: score.round() as u8,
            why: self.why,
            suggested_next_step: self.suggested_next_step,
        }
    }
}

/// Build the prompt for one batch. Pure, and separate from [`tier1`] so a test
/// can read it.
pub fn tier1_prompt(events: &[Event]) -> String {
    let mut prompt = String::from(
        "Score each of the following events. Return one entry per event, using the event_id \
         exactly as given.\n\n",
    );
    for event in events {
        let payload = serde_json::to_string(&event.payload).unwrap_or_else(|_| "{}".to_string());
        prompt.push_str(&format!(
            "event_id: {}\nsource: {}\nkind: {}\ncreated_at: {}\npayload: {}\n\n",
            event.id,
            event.source,
            event.kind,
            event.created_at,
            clip(&payload, PAYLOAD_BUDGET),
        ));
    }
    prompt
}

fn clip(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let clipped: String = text.chars().take(budget).collect();
    format!("{clipped}... [payload truncated]")
}

/// Score a batch of events with **one** session.
///
/// At most [`TIER1_BATCH`] events are scored; anything beyond that is left for
/// the next cycle rather than silently dropped by the model when the prompt
/// gets long. An empty batch runs no session at all and costs nothing.
///
/// Two guards on the answer, both of which have happened to every structured
/// output that has ever been asked of a model:
///
/// * a score for an `event_id` that was not in the batch is **dropped** -- it
///   is either a hallucinated id or a transposed digit, and writing a salience
///   onto an unrelated event is worse than losing one score;
/// * a score outside 0-100 is **clamped** rather than rejected, so one bad
///   number does not cost the other thirty-nine.
///
/// A missing or unparseable `structured_output` is an error, not a panic: the
/// scheduler retries the next cycle.
/// The slice of `events` one call to [`tier1`] will actually submit.
///
/// Exposed because the caller has to know which events were *asked about* in
/// order to tell "the model omitted this one" from "the model was never shown
/// it" — the distinction the triage attempt count is built on.
pub fn tier1_batch(events: &[Event]) -> &[Event] {
    &events[..events.len().min(TIER1_BATCH)]
}

pub async fn tier1<S>(events: &[Event], sessions: &S) -> Result<Vec<Salience>>
where
    S: SessionBoundary + ?Sized,
{
    let batch = tier1_batch(events);
    if batch.is_empty() {
        return Ok(Vec::new());
    }

    let request = SessionRequest::new(TIER1_RUN_KIND, tier1_prompt(batch), TIER1_SYSTEM_PROMPT)
        // Empty, and explicit. Tier 1 scores what is already in the database.
        .with_connectors(Vec::<String>::new())
        .with_json_schema(salience_schema())
        .with_model(TIER1_MODEL);

    let outcome = sessions.run_session(request).await?;
    let structured = outcome
        .structured
        .ok_or_else(|| anyhow::anyhow!("tier 1 session returned no structured_output"))?;

    let raw: RawScores = serde_json::from_value(structured)
        .context("tier 1 structured_output did not match the salience schema")?;

    let known: BTreeSet<i64> = batch.iter().map(|event| event.id).collect();
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    let mut scores = Vec::with_capacity(raw.scores.len());
    for entry in raw.scores {
        if !known.contains(&entry.event_id) {
            tracing::warn!(
                event_id = entry.event_id,
                "tier 1 scored an event that was not in the batch; dropping it"
            );
            continue;
        }
        // A repeated id is the same failure mode as an unknown one; keep the
        // first answer rather than letting the last one win silently.
        if !seen.insert(entry.event_id) {
            tracing::warn!(event_id = entry.event_id, "tier 1 scored an event twice");
            continue;
        }
        scores.push(entry.into_salience());
    }

    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn event(id: i64, source: &str, kind: &str, payload: Value) -> Event {
        Event {
            id,
            source: source.to_string(),
            external_id: format!("ext-{id}"),
            kind: kind.to_string(),
            payload,
            salience: None,
            triaged_at: None,
            created_at: "2026-09-24T09:00:00Z".to_string(),
            triage_attempts: 0,
            triage_error: None,
        }
    }

    fn plain(id: i64) -> Event {
        event(
            id,
            "canvas",
            "assignment",
            serde_json::json!({ "title": "essay" }),
        )
    }

    // -- tier 0 ------------------------------------------------------------

    #[test]
    fn an_ordinary_event_is_kept() {
        let (kept, dropped) = tier0(vec![plain(1)], &Tier0Rules::default());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, 1);
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_muted_source_is_dropped() {
        let rules = Tier0Rules {
            muted_sources: vec!["newsletter".into()],
            ..Tier0Rules::default()
        };
        let (kept, dropped) = tier0(
            vec![
                event(
                    1,
                    "newsletter",
                    "email",
                    serde_json::json!({ "subject": "weekly" }),
                ),
                plain(2),
            ],
            &rules,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, 2);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0.id, 1);
        assert!(
            dropped[0].1.contains("newsletter"),
            "the reason must name the rule that dropped it, got {:?}",
            dropped[0].1
        );
    }

    #[test]
    fn a_muted_kind_is_dropped() {
        let rules = Tier0Rules {
            muted_kinds: vec!["announcement".into()],
            ..Tier0Rules::default()
        };
        let (kept, dropped) = tier0(
            vec![event(
                1,
                "canvas",
                "announcement",
                serde_json::json!({ "body": "the library closes early" }),
            )],
            &rules,
        );
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].1.contains("announcement"));
    }

    #[test]
    fn a_keyword_rescues_a_muted_kind() {
        let rules = Tier0Rules {
            muted_kinds: vec!["announcement".into()],
            keywords: vec!["exam".into()],
            ..Tier0Rules::default()
        };
        let (kept, dropped) = tier0(
            vec![
                event(
                    1,
                    "canvas",
                    "announcement",
                    serde_json::json!({ "body": "the exam has moved to Friday" }),
                ),
                event(
                    2,
                    "canvas",
                    "announcement",
                    serde_json::json!({ "body": "the library closes early" }),
                ),
            ],
            &rules,
        );
        assert_eq!(kept.len(), 1, "the keyword event must survive the mute");
        assert_eq!(kept[0].id, 1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0.id, 2);
    }

    #[test]
    fn a_keyword_also_rescues_a_muted_source() {
        let rules = Tier0Rules {
            muted_sources: vec!["newsletter".into()],
            keywords: vec!["invoice".into()],
            ..Tier0Rules::default()
        };
        let (kept, _) = tier0(
            vec![event(
                1,
                "newsletter",
                "email",
                serde_json::json!({ "subject": "your invoice is due" }),
            )],
            &rules,
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let rules = Tier0Rules {
            muted_kinds: vec!["announcement".into()],
            keywords: vec!["ExAm".into()],
            ..Tier0Rules::default()
        };
        let (kept, _) = tier0(
            vec![event(
                1,
                "canvas",
                "announcement",
                serde_json::json!({ "body": "The EXAM has moved" }),
            )],
            &rules,
        );
        assert_eq!(
            kept.len(),
            1,
            "neither side of the match may be case-sensitive"
        );
    }

    #[test]
    fn keyword_matching_searches_the_whole_payload() {
        let rules = Tier0Rules {
            muted_kinds: vec!["announcement".into()],
            keywords: vec!["exam".into()],
            ..Tier0Rules::default()
        };
        // The keyword is nowhere near the top level: it is inside an object
        // inside an array, three keys deep.
        let (kept, dropped) = tier0(
            vec![event(
                1,
                "canvas",
                "announcement",
                serde_json::json!({
                    "body": "see below",
                    "attachments": [
                        { "name": "syllabus.pdf" },
                        { "name": "notes", "sections": { "week_9": "exam practice" } }
                    ]
                }),
            )],
            &rules,
        );
        assert_eq!(
            kept.len(),
            1,
            "a keyword buried in the payload must still rescue; dropped: {dropped:?}"
        );
    }

    #[test]
    fn an_already_triaged_event_is_dropped() {
        let mut triaged = plain(1);
        triaged.triaged_at = Some("2026-09-24T08:00:00Z".to_string());
        triaged.salience = Some(70);
        let (kept, dropped) = tier0(vec![triaged], &Tier0Rules::default());
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].1.contains("triaged"));
    }

    #[test]
    fn a_keyword_does_not_rescue_an_already_triaged_event() {
        let mut triaged = event(
            1,
            "canvas",
            "announcement",
            serde_json::json!({ "b": "exam" }),
        );
        triaged.triaged_at = Some("2026-09-24T08:00:00Z".to_string());
        let rules = Tier0Rules {
            keywords: vec!["exam".into()],
            ..Tier0Rules::default()
        };
        let (kept, dropped) = tier0(vec![triaged], &rules);
        assert!(kept.is_empty(), "triage is finished work, not noise");
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn empty_input_returns_empty_without_error() {
        let (kept, dropped) = tier0(Vec::new(), &Tier0Rules::default());
        assert!(kept.is_empty());
        assert!(dropped.is_empty());
    }

    // -- tier 1 ------------------------------------------------------------

    /// Stands in for a real `claude` subprocess. Counts calls -- the count is
    /// the point of the batching -- and records the requests it was given.
    struct FakeSessions {
        calls: AtomicUsize,
        requests: Mutex<Vec<SessionRequest>>,
        reply: Mutex<Result<SessionOutcome, String>>,
    }

    impl FakeSessions {
        fn returning(structured: Option<Value>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
                reply: Mutex::new(Ok(SessionOutcome {
                    text: String::new(),
                    structured,
                    session_id: Some("fake-session".into()),
                    cost_usd: Some(0.0),
                })),
            }
        }

        fn failing(message: &str) -> Self {
            let fake = Self::returning(None);
            *fake.reply.lock().unwrap() = Err(message.to_string());
            fake
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_request(&self) -> SessionRequest {
            self.requests
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("a request")
        }
    }

    impl SessionBoundary for FakeSessions {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(req);
            let reply = self
                .reply
                .lock()
                .unwrap()
                .as_ref()
                .map(Clone::clone)
                .map_err(|message| anyhow::anyhow!(message.clone()));
            Box::pin(async move { reply })
        }
    }

    fn scores(entries: Value) -> Option<Value> {
        Some(serde_json::json!({ "scores": entries }))
    }

    #[tokio::test]
    async fn scores_map_back_to_the_right_events() {
        let events = vec![plain(11), plain(22), plain(33)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([
            { "event_id": 33, "salience": 90, "why": "due tonight", "suggested_next_step": "start it" },
            { "event_id": 11, "salience": 10, "why": "fyi", "suggested_next_step": "" }
        ])));

        let out = tier1(&events, &sessions).await.unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event_id, 33);
        assert_eq!(out[0].salience, 90);
        assert_eq!(out[0].why, "due tonight");
        assert_eq!(out[0].suggested_next_step, "start it");
        assert_eq!(out[1].event_id, 11);
        assert_eq!(out[1].salience, 10);
    }

    #[tokio::test]
    async fn scores_for_events_outside_the_batch_are_dropped() {
        let events = vec![plain(1), plain(2)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([
            { "event_id": 1, "salience": 50, "why": "a", "suggested_next_step": "" },
            { "event_id": 999, "salience": 100, "why": "hallucinated", "suggested_next_step": "" }
        ])));

        let out = tier1(&events, &sessions).await.unwrap();

        assert_eq!(out.len(), 1, "a score for an unknown id must not be kept");
        assert_eq!(out[0].event_id, 1);
    }

    #[tokio::test]
    async fn scores_outside_the_range_are_clamped() {
        let events = vec![plain(1), plain(2), plain(3)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([
            { "event_id": 1, "salience": 150, "why": "", "suggested_next_step": "" },
            { "event_id": 2, "salience": -20, "why": "", "suggested_next_step": "" },
            { "event_id": 3, "salience": 100, "why": "", "suggested_next_step": "" }
        ])));

        let out = tier1(&events, &sessions).await.unwrap();

        assert_eq!(out[0].salience, 100);
        assert_eq!(out[1].salience, 0);
        assert_eq!(out[2].salience, 100);
    }

    #[tokio::test]
    async fn a_missing_structured_output_is_an_error_not_a_panic() {
        let events = vec![plain(1)];
        let sessions = FakeSessions::returning(None);
        let err = tier1(&events, &sessions).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("structured_output"),
            "got {err:#}"
        );
    }

    #[tokio::test]
    async fn a_malformed_structured_output_is_an_error_not_a_panic() {
        let events = vec![plain(1)];
        for malformed in [
            serde_json::json!({ "scores": "not an array" }),
            serde_json::json!({ "scores": [{ "event_id": "one", "salience": 5 }] }),
            serde_json::json!(["bare", "array"]),
            serde_json::json!("a string"),
        ] {
            let sessions = FakeSessions::returning(Some(malformed.clone()));
            assert!(
                tier1(&events, &sessions).await.is_err(),
                "{malformed} should have failed cleanly"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_scores_array_is_an_empty_result_not_an_error() {
        let events = vec![plain(1)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([])));
        assert!(tier1(&events, &sessions).await.unwrap().is_empty());
        // And a document with no `scores` key at all is the same thing.
        let sessions = FakeSessions::returning(Some(serde_json::json!({})));
        assert!(tier1(&events, &sessions).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_session_surfaces_its_error() {
        let events = vec![plain(1)];
        let sessions = FakeSessions::failing("claude session timed out");
        let err = tier1(&events, &sessions).await.unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[tokio::test]
    async fn no_events_runs_no_session_at_all() {
        let sessions = FakeSessions::returning(scores(serde_json::json!([])));
        assert!(tier1(&[], &sessions).await.unwrap().is_empty());
        assert_eq!(sessions.calls(), 0, "an empty batch must cost nothing");
    }

    #[tokio::test]
    async fn the_batch_is_capped_at_forty() {
        let events: Vec<Event> = (1..=100).map(plain).collect();
        let all: Vec<Value> = (1..=100)
            .map(|id| serde_json::json!({ "event_id": id, "salience": 50, "why": "", "suggested_next_step": "" }))
            .collect();
        let sessions = FakeSessions::returning(scores(serde_json::json!(all)));

        let out = tier1(&events, &sessions).await.unwrap();

        // Only the first forty are in the batch, so only the first forty of the
        // hundred returned scores are known ids; the rest are dropped by the
        // same guard that drops hallucinated ids.
        assert_eq!(out.len(), TIER1_BATCH);
        assert_eq!(out.last().unwrap().event_id, TIER1_BATCH as i64);

        let prompt = sessions.last_request().prompt;
        assert!(prompt.contains("event_id: 40"));
        assert!(
            !prompt.contains("event_id: 41"),
            "event 41 must not be in the prompt"
        );
    }

    #[tokio::test]
    async fn a_batch_of_forty_runs_exactly_one_session() {
        // The economic premise of the whole tiering: the fixed prompt prefix of
        // a session is paid once per session, so forty events must cost one
        // session, not forty. If this ever reads 40, triage got ~40x more
        // expensive without anyone noticing.
        let events: Vec<Event> = (1..=40).map(plain).collect();
        let all: Vec<Value> = (1..=40)
            .map(|id| serde_json::json!({ "event_id": id, "salience": 5, "why": "", "suggested_next_step": "" }))
            .collect();
        let sessions = FakeSessions::returning(scores(serde_json::json!(all)));

        let out = tier1(&events, &sessions).await.unwrap();

        assert_eq!(out.len(), 40);
        assert_eq!(sessions.calls(), 1, "one batch is one session");
    }

    #[tokio::test]
    async fn a_duplicate_id_keeps_the_first_answer() {
        let events = vec![plain(1)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([
            { "event_id": 1, "salience": 80, "why": "first", "suggested_next_step": "" },
            { "event_id": 1, "salience": 5, "why": "second", "suggested_next_step": "" }
        ])));
        let out = tier1(&events, &sessions).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].why, "first");
    }

    #[tokio::test]
    async fn tier1_asks_for_haiku_and_no_connectors() {
        let events = vec![plain(1)];
        let sessions = FakeSessions::returning(scores(serde_json::json!([])));
        tier1(&events, &sessions).await.unwrap();

        let req = sessions.last_request();
        assert_eq!(
            req.model.as_deref(),
            Some(TIER1_MODEL),
            "an unset model is inherited from the human's settings: see the module docs"
        );
        assert!(
            req.connectors.is_empty(),
            "tier 1 scores; it does not fetch and it does not act"
        );
        assert_eq!(req.json_schema, Some(salience_schema()));
        assert_eq!(req.kind, TIER1_RUN_KIND);
    }

    #[test]
    fn the_salience_schema_is_valid_json_and_bounds_the_score() {
        let schema = salience_schema();
        let item = &schema["properties"]["scores"]["items"];
        assert_eq!(item["properties"]["salience"]["minimum"], 0);
        assert_eq!(item["properties"]["salience"]["maximum"], 100);
        assert_eq!(item["additionalProperties"], serde_json::json!(false));
        let required: Vec<&str> = item["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            required,
            vec!["event_id", "salience", "why", "suggested_next_step"]
        );
    }

    #[test]
    fn the_prompt_carries_every_event_and_clips_a_runaway_payload() {
        let big = "x".repeat(PAYLOAD_BUDGET * 3);
        let events = vec![
            plain(1),
            event(2, "gmail", "email", serde_json::json!({ "body": big })),
        ];
        let prompt = tier1_prompt(&events);
        assert!(prompt.contains("event_id: 1"));
        assert!(prompt.contains("event_id: 2"));
        assert!(prompt.contains("payload truncated"));
        assert!(prompt.len() < PAYLOAD_BUDGET * 3);
    }
}
