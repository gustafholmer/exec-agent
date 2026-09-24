//! What the three scheduled briefings actually say, and where the material
//! comes from.
//!
//! [`crate::schedules`] decides *when*; this module decides *what*. Each
//! briefing is the same three steps — gather, think once, send once — and the
//! shape is deliberate: the gathering is a plain function returning a
//! [`Material`], so what a briefing is made of can be asserted without a model
//! and without Telegram.
//!
//! # The gate, restated, because a briefing is a new caller
//!
//! Phase 1's rule is that a session reads and proposes, and the only way
//! anything reaches the world is [`propose_action`](crate::session::PROPOSE_TOOL)
//! through `ea_core::policy`. Nothing here weakens it:
//!
//! * `session::ALLOWED_TOOLS` is untouched. A briefing session, like every
//!   other, may call exactly the two `ea-propose` tools and nothing
//!   else. It is given no connector servers at all
//!   ([`SessionRequest::with_connectors`] with an empty list), so there is not
//!   even a server for a tool call to land on.
//! * The connector reads a briefing needs are made by the **daemon**, before
//!   the session starts, through the same path `watch_poll` uses: a tool named
//!   by a constant in this file, never by a caller and never by the model, and
//!   refused unless the merged policy rates that tool `auto` for that connector
//!   (see [`read_auto`]). A connector whose `unpaid_invoices` were graded
//!   `approve` would simply not be read, and the refusal says so.
//! * Nothing here is reachable from the IPC socket.
//!
//! So the set of things that call a connector without an `actions` row grows
//! from "`watch_poll`" to "`watch_poll` and a fixed list of read-only tools the
//! policy already grades `auto`", and it grows under the same three
//! constraints. The material then travels into the prompt as text. A briefing
//! that wants to *change* something must propose it like anything else — and
//! `vat_prep` is told in its system prompt not even to do that.
//!
//! # The model on every session is set explicitly
//!
//! See [`BRIEFING_MODEL`]. Phase 1 measured that a session with no `--model`
//! inherits the owner's own interactive setting — `opus[1m]` on this machine,
//! about 2.3x the price for work that does not need it — and that
//! `--setting-sources ''` does not clear it. Every session spawned from this
//! module names its model.

use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use ea_core::policy::{Mode, Policy};
use ea_core::store::events::{kinds, Event, EventStore};
use ea_core::store::runs::RunStore;
use serde_json::{json, Value};

use crate::executor::ToolCaller;
use crate::jobs::Pusher;
use crate::notify::log::NotificationLog;
use crate::session::SessionRequest;
use crate::triage::SessionBoundary;

/// The model every briefing session runs on.
///
/// Sonnet, set explicitly, for all three — and the reasoning differs per
/// briefing even though the answer does not:
///
/// * **`morning_briefing`** is the owner's one daily contact with this system.
///   Its job is to turn a calendar, a backlog and a digest into a few lines a
///   person reads on a phone before breakfast, and the quality of that prose
///   *is* the product. Haiku is the right tool for tier-1 classification, which
///   fires every five minutes forever; this fires 365 times a year and its
///   output is read rather than compared against a threshold.
/// * **`bookkeeping_pass`** has the highest reasoning demand of the three:
///   deciding which of a pile of invoices and ledger rows deserves attention is
///   judgement over numbers, and a missed overdue invoice costs real money.
///   52 runs a year.
/// * **`vat_prep`** reports VAT balances and deadlines and drafts nothing. It
///   is the most consequential subject and the least demanding task — the
///   figures are already computed by `vat_summary`; the session arranges and
///   explains them. 12 runs a year, and being wrong about tax is expensive
///   enough that the small model's flatter reading of an unusual balance is not
///   worth the saving.
///
/// Not Opus: nothing here needs it, and letting the field go unset would
/// *give* it Opus by inheritance, which is the bug the field exists to close.
/// One constant rather than three, because three constants with the same value
/// would be a promise of independence that nothing here keeps; when one of
/// these genuinely wants a different model, it gets its own constant and its
/// own paragraph.
pub const BRIEFING_MODEL: &str = "claude-sonnet-4-5";

/// How many events of one kind a briefing will read. A bound on the prompt,
/// not on the world: a briefing that would be a thousand lines long is not a
/// briefing.
const MATERIAL_LIMIT: i64 = 60;

/// How many characters of one connector read reach the prompt.
///
/// `unpaid_invoices` returns Fortnox's own rows, every page of them, and a
/// business with two hundred open invoices would otherwise send a novel to the
/// model. Truncation is marked in the text so the session knows it is seeing a
/// prefix rather than the whole ledger.
const READ_LIMIT: usize = 8_000;

// --------------------------------------------------------------------------
// The kinds the briefings read
// --------------------------------------------------------------------------
//
// Imported from [`ea_core::store::events::kinds`], not restated here. The
// daemon still has no compile-time dependency on any connector crate —
// connectors are child processes reached over MCP — but every connector and
// the daemon already depend on `ea-core`, which owns the `events` table these
// strings are persisted in. So the emitter and this reader now name the same
// constant, and renaming `calendar_event` is a compile error on both sides
// instead of an empty calendar section nobody is told about.

/// The connector and tool a briefing reads, always named by a constant.
const FORTNOX: &str = "fortnox";
const UNPAID_INVOICES_TOOL: &str = "unpaid_invoices";
const VAT_SUMMARY_TOOL: &str = "vat_summary";
const ACCOUNT_LEDGER_TOOL: &str = "account_ledger";

// --------------------------------------------------------------------------
// Dependencies and outcome
// --------------------------------------------------------------------------

/// Everything the briefings read, write and send through.
///
/// Generic over the caller for the same reason [`crate::jobs::watch_job`] is:
/// [`ToolCaller`] returns an `impl Future` and so is not `dyn`-compatible.
pub struct BriefingDeps<C: ToolCaller> {
    pub events: EventStore,
    pub runs: RunStore,
    pub log: NotificationLog,
    /// Absent when `claude` could not be resolved at start-up. A briefing then
    /// reports that rather than silently not happening.
    pub sessions: Option<Arc<dyn SessionBoundary>>,
    /// Absent when Telegram is not configured.
    pub pusher: Option<Arc<dyn Pusher>>,
    pub caller: Arc<C>,
    pub policy: Policy,
    /// The owner's zone — the same one quiet hours are read in. "Today's
    /// calendar" means today where the owner is.
    pub time_zone: Tz,
    /// `NotifyConfig::threshold`: the line above which an event was worth an
    /// interruption. The morning briefing reports what crossed it.
    pub threshold: u8,
    pub daily_session_budget: u32,
}

/// What one briefing did. Returned rather than logged so tests can assert it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BriefingOutcome {
    /// Whether a model session ran. `false` when there is no session runner or
    /// the day's budget is spent.
    pub thought: bool,
    /// Whether a Telegram message went out.
    pub sent: bool,
    /// Digest lines folded into the message and then cleared.
    pub digest_lines: usize,
    /// Why nothing was sent, when nothing was.
    pub note: Option<String>,
}

/// The gathered material for one briefing, before any model sees it.
///
/// A named type with named sections rather than one pre-rendered string, so a
/// test can assert *what was gathered* separately from how it reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Material {
    pub sections: Vec<(String, String)>,
}

impl Material {
    fn push(&mut self, heading: impl Into<String>, body: impl Into<String>) {
        let body = body.into();
        let body = body.trim();
        self.sections.push((
            heading.into(),
            if body.is_empty() {
                "(nothing)".to_string()
            } else {
                body.to_string()
            },
        ));
    }

    /// The prompt body: every section, headed.
    pub fn render(&self) -> String {
        self.sections
            .iter()
            .map(|(heading, body)| format!("## {heading}\n{body}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn heading(&self, name: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(heading, _)| heading == name)
            .map(|(_, body)| body.as_str())
    }
}

// --------------------------------------------------------------------------
// The one place a briefing touches a connector
// --------------------------------------------------------------------------

/// Call one read-only connector tool, refusing anything the policy does not
/// grade `auto`.
///
/// The same refusal `run_watch_poll` makes, for the same reason: this is a
/// connector call with no `actions` row behind it, and the only thing that
/// makes that acceptable is that the tool is named by a constant here and the
/// policy has already graded it as needing no human. A tool graded `approve`
/// is not quietly downgraded — it is not called, and the error says which
/// grade stopped it.
async fn read_auto<C: ToolCaller>(
    caller: &C,
    policy: &Policy,
    connector: &str,
    tool: &str,
    args: Value,
) -> anyhow::Result<String> {
    let decision = policy.decide(connector, tool);
    if decision.mode != Mode::Auto {
        bail!(
            "refusing to read {connector}.{tool} for a briefing: policy rates it {:?}, not auto ({})",
            decision.mode,
            decision.reason
        );
    }
    let raw = caller
        .call(connector, tool, args)
        .await
        .with_context(|| format!("reading {connector}.{tool} for a briefing"))?;
    Ok(truncate(&raw, READ_LIMIT))
}

/// A connector read that failed is reported *into the briefing* rather than
/// propagated.
///
/// A briefing whose bank ledger is unreachable is still worth sending with the
/// invoices in it, and an owner who is told "Fortnox would not answer" knows
/// more than one who is told nothing. The failure is not swallowed: it is a
/// line in the message the owner reads, which is a louder channel than the log.
async fn read_or_report<C: ToolCaller>(
    caller: &C,
    policy: &Policy,
    connector: &str,
    tool: &str,
    args: Value,
) -> String {
    match read_auto(caller, policy, connector, tool, args).await {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(connector, tool, error = %format!("{err:#}"), "a briefing read failed");
            format!("(could not be read: {err:#})")
        }
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n… (truncated at {limit} characters)")
}

// --------------------------------------------------------------------------
// Gathering
// --------------------------------------------------------------------------

/// The payload keys a calendar row's start time can live under: `start` on an
/// event, `overlap_start` on a conflict.
const START_KEYS: [&str; 2] = ["start", "overlap_start"];

/// Today's calendar, in the owner's zone.
///
/// Filtered on the event's own `start`, not on when it was recorded: a meeting
/// entered a fortnight ago is on today's calendar and a meeting recorded this
/// morning for next week is not.
///
/// # Why this is a window query and not a limit with a filter after it
///
/// The obvious shape — read the newest `MATERIAL_LIMIT` calendar rows and keep
/// the ones starting today — is wrong, and wrong *silently*, which is worse
/// than wrong loudly. `by_kind` orders by id, i.e. by when a row was first
/// recorded; a poll records events up to `LOOKAHEAD` (a week) ahead and the id
/// is fixed at that first insert, while rows live for the whole retention
/// window. So today's meeting, recorded last Tuesday, sits under every event
/// entered since — and on a calendar with more than sixty of those it is cut
/// before the date filter ever sees it. No error, no empty-looking section: a
/// calendar that reads plausibly and is missing the one meeting that mattered.
///
/// [`EventStore::in_payload_range`] selects on the start value itself, so a
/// row's place here cannot depend on any other row. The window handed to SQL
/// is widened by a day at each end and the exact day test is then done in Rust
/// by [`starts_on`]: string comparison over a start value is only chronological
/// if every writer uses the same RFC 3339 shape, and a day of slack at each end
/// absorbs any offset (the largest in use anywhere is 14 hours) without
/// trusting that. `MATERIAL_LIMIT` still caps the result, but now it caps
/// *today's* events, after the date is known.
pub fn todays_calendar(
    events: &EventStore,
    time_zone: Tz,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<Event>> {
    let today = now.with_timezone(&time_zone).date_naive();
    let from = (today - chrono::Duration::days(1)).to_string();
    let to = (today + chrono::Duration::days(2)).to_string();

    let mut found: Vec<Event> = events
        .in_payload_range(
            &[kinds::CALENDAR_EVENT, kinds::CALENDAR_CONFLICT],
            &START_KEYS,
            &from,
            &to,
        )?
        .into_iter()
        .filter(|event| starts_on(event, today, time_zone))
        .collect();
    found.sort_by(|a, b| start_of(a).cmp(&start_of(b)));
    found.truncate(MATERIAL_LIMIT as usize);
    Ok(found)
}

/// `payload.start` (a calendar event) or `payload.overlap_start` (a conflict),
/// as written.
fn start_of(event: &Event) -> Option<&str> {
    START_KEYS
        .iter()
        .find_map(|key| event.payload.get(*key).and_then(Value::as_str))
}

fn starts_on(event: &Event, day: NaiveDate, time_zone: Tz) -> bool {
    start_of(event)
        .and_then(|text| text.parse::<DateTime<Utc>>().ok())
        .is_some_and(|start| start.with_timezone(&time_zone).date_naive() == day)
}

/// One event as a briefing line. Deliberately the same shape triage's
/// `describe` uses, minus the score, so the two read alike.
fn line(event: &Event) -> String {
    let title = ["title", "subject", "name", "summary", "label"]
        .iter()
        .find_map(|key| event.payload.get(*key).and_then(Value::as_str))
        .unwrap_or(&event.kind);
    let when = start_of(event)
        .or_else(|| event.payload.get("due_on").and_then(Value::as_str))
        .or_else(|| event.payload.get("due_date").and_then(Value::as_str));
    match when {
        Some(when) => format!("- [{}] {when} — {title}", event.source),
        None => format!("- [{}] {title}", event.source),
    }
}

fn lines(events: &[Event]) -> String {
    events.iter().map(line).collect::<Vec<_>>().join("\n")
}

/// Everything the morning briefing is made of.
pub fn morning_material<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
    digest: &[String],
) -> anyhow::Result<Material> {
    let mut material = Material::default();
    let today = now.with_timezone(&deps.time_zone).date_naive();
    material.push(
        format!("Today's calendar ({today})"),
        lines(&todays_calendar(&deps.events, deps.time_zone, now)?),
    );
    // Two readings of the brief's "anything untriaged above the digest
    // threshold", and both are material a morning briefing wants. The literal
    // untriaged backlog is what triage has not yet looked at — a growing one
    // means triage is stuck, which is exactly the thing that is invisible
    // otherwise. The scored-above-threshold list is what crossed the
    // interruption line since the last briefing.
    material.push(
        "Not yet triaged",
        lines(&deps.events.untriaged(MATERIAL_LIMIT)?),
    );
    material.push(
        format!(
            "Scored at or above {} since the last briefing",
            deps.threshold
        ),
        lines(
            &deps
                .events
                .scored_since(i64::from(deps.threshold), since, MATERIAL_LIMIT)?,
        ),
    );
    material.push("Digest since the last briefing", digest.join("\n"));
    Ok(material)
}

/// Everything the bookkeeping pass is made of.
pub async fn bookkeeping_material<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
) -> anyhow::Result<Material> {
    let today = now.with_timezone(&deps.time_zone).date_naive();
    let mut material = Material::default();
    for (label, kind) in [("customer", "customer"), ("supplier", "supplier")] {
        material.push(
            format!("Unpaid {label} invoices"),
            read_or_report(
                deps.caller.as_ref(),
                &deps.policy,
                FORTNOX,
                UNPAID_INVOICES_TOOL,
                json!({ "kind": kind }),
            )
            .await,
        );
    }
    // Fortnox exposes no "uncategorised" flag and no bank-transaction tool, so
    // the closest read this connector actually has is the voucher ledger for
    // the financial year: a bank payment that has not been booked against
    // anything is a row the session can pick out of it. Stated rather than
    // dressed up, because a briefing that implied it had checked a list nobody
    // can produce would be worse than one that says what it looked at.
    material.push(
        "Voucher ledger for the current financial year",
        read_or_report(
            deps.caller.as_ref(),
            &deps.policy,
            FORTNOX,
            ACCOUNT_LEDGER_TOOL,
            json!({ "date": today.to_string() }),
        )
        .await,
    );
    Ok(material)
}

/// Everything the VAT pass is made of.
pub async fn vat_material<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
) -> anyhow::Result<Material> {
    let today = now.with_timezone(&deps.time_zone).date_naive();
    let mut material = Material::default();
    material.push(
        "VAT account balances",
        read_or_report(
            deps.caller.as_ref(),
            &deps.policy,
            FORTNOX,
            VAT_SUMMARY_TOOL,
            json!({ "date": today.to_string() }),
        )
        .await,
    );
    // The deadline list is the Fortnox connector's own `tax_deadline` rows,
    // which `watch_poll` records from `upcoming_deadlines` — the daemon's only
    // source for them, and already in the database.
    //
    // This one stays a plain `by_kind`, unlike `todays_calendar`, and it is not
    // the same shape: nothing is filtered *after* the limit, so no row can be
    // cut by another row's id and then silently fail a test it never reached.
    // The list is small by construction — `deadlines::HORIZON_DAYS` is 45, over
    // which at most a handful of declarations fall due, and the external id is
    // one row per kind per period — so sixty is a bound that the world does not
    // get near rather than one it quietly exceeds. If a deadline window ever
    // does, this wants the same treatment `todays_calendar` got.
    material.push(
        "Upcoming declaration deadlines",
        lines(&deps.events.by_kind(kinds::TAX_DEADLINE, MATERIAL_LIMIT)?),
    );
    Ok(material)
}

// --------------------------------------------------------------------------
// The prompts
// --------------------------------------------------------------------------

const MORNING_SYSTEM_PROMPT: &str = concat!(
    "You are the user's executive assistant writing their morning briefing. ",
    "Answer with the briefing itself and nothing else: no preamble, no ",
    "restating of these instructions. Plain text for a phone screen, at most ",
    "twelve lines. Lead with anything time-critical today. Group the rest. Say ",
    "plainly when a section is empty rather than padding it. You cannot act: ",
    "the only tool you have records a proposal for the user to approve, and a ",
    "briefing rarely needs one."
);

const BOOKKEEPING_SYSTEM_PROMPT: &str = concat!(
    "You are the user's bookkeeper doing a weekly pass over a small Swedish ",
    "company's books. Answer with the review itself and nothing else. Report ",
    "what needs attention: invoices overdue or falling due this week, amounts ",
    "that look wrong, bank movements with no matching voucher. Name document ",
    "numbers and amounts. Be brief and specific; do not restate rows the user ",
    "can read for themselves. If something needs booking, propose it with the ",
    "propose_action tool -- never claim to have booked anything."
);

const VAT_SYSTEM_PROMPT: &str = concat!(
    "You are preparing the user's Swedish VAT (moms) position. This is a ",
    "report and only a report: state the output and input VAT balances, the ",
    "net position, and which declarations fall due and when. Do not draft, ",
    "propose or book anything -- not a voucher, not a correction, not a ",
    "declaration. If a figure looks wrong, say so and say why; deciding what ",
    "to do about it is the user's. Answer with the report itself and nothing ",
    "else."
);

// --------------------------------------------------------------------------
// Running one
// --------------------------------------------------------------------------

/// Run one session over `material` and push the answer as a single message.
///
/// The one path all three briefings share, so the budget check, the explicit
/// model, the empty connector scope and the "exactly one message" rule are
/// stated once.
async fn deliver<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    kind: &str,
    system_prompt: &str,
    material: &Material,
) -> anyhow::Result<BriefingOutcome> {
    let mut outcome = BriefingOutcome::default();

    let Some(sessions) = deps.sessions.as_ref() else {
        outcome.note =
            Some("no session runner (is `claude` on PATH?), so there is no briefing".to_string());
        tracing::warn!(briefing = kind, "no session runner; skipping");
        return Ok(outcome);
    };
    if crate::jobs::session_budget_spent(&deps.runs, deps.daily_session_budget, Utc::now())? {
        outcome.note = Some(format!(
            "the daily session budget of {} is spent, so there is no briefing",
            deps.daily_session_budget
        ));
        tracing::warn!(briefing = kind, "daily session budget spent; skipping");
        return Ok(outcome);
    }

    let request = SessionRequest::new(kind, material.render(), system_prompt)
        // Empty, and explicit. A briefing is written from material the daemon
        // already gathered; handing it connector servers would give the model
        // tools it cannot call anyway (see `session::ALLOWED_TOOLS`) and spawn
        // a child process per connector to do it.
        .with_connectors(Vec::<String>::new())
        // Never left unset. See BRIEFING_MODEL.
        .with_model(BRIEFING_MODEL);

    let answer = sessions.run_session(request).await?;
    outcome.thought = true;

    let text = answer.text.trim();
    if text.is_empty() {
        outcome.note = Some("the session returned nothing to send".to_string());
        tracing::warn!(briefing = kind, "the session returned an empty briefing");
        return Ok(outcome);
    }

    match deps.pusher.as_ref() {
        // One message, whatever the briefing found. A briefing that arrived as
        // six notifications would be the interruption budget this system exists
        // to protect, spent on the one message that was supposed to replace it.
        Some(pusher) => {
            pusher
                .notify(text)
                .await
                .with_context(|| format!("sending the {kind} briefing"))?;
            outcome.sent = true;
        }
        None => {
            outcome.note = Some("no notifier configured, so the briefing was not sent".to_string());
            tracing::warn!(briefing = kind, "no notifier configured; briefing not sent");
        }
    }
    Ok(outcome)
}

/// The daily briefing: today's calendar, what triage has not looked at, what
/// crossed the interruption threshold, and the accumulated digest.
///
/// `since` is the previous anchor — what "since the last briefing" means.
pub async fn run_morning_briefing<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> anyhow::Result<BriefingOutcome> {
    // Read without clearing, and clear exactly this prefix afterwards — see
    // `NotificationLog::drop_digest_prefix`. Clearing first would lose the
    // backlog if the session or the send failed; clearing last would throw away
    // lines triage pushed while the session was running.
    let digest = deps.log.digest()?;
    let material = morning_material(deps, now, since, &digest)?;

    let mut outcome = deliver(
        deps,
        crate::schedules::MORNING_BRIEFING,
        MORNING_SYSTEM_PROMPT,
        &material,
    )
    .await?;

    if outcome.sent {
        deps.log.drop_digest_prefix(digest.len())?;
        outcome.digest_lines = digest.len();
    }
    Ok(outcome)
}

/// The weekly accounting pass.
pub async fn run_bookkeeping_pass<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
) -> anyhow::Result<BriefingOutcome> {
    let material = bookkeeping_material(deps, now).await?;
    deliver(
        deps,
        crate::schedules::BOOKKEEPING_PASS,
        BOOKKEEPING_SYSTEM_PROMPT,
        &material,
    )
    .await
}

/// The monthly VAT pass. Reports; drafts nothing.
pub async fn run_vat_prep<C: ToolCaller>(
    deps: &BriefingDeps<C>,
    now: DateTime<Utc>,
) -> anyhow::Result<BriefingOutcome> {
    let material = vat_material(deps, now).await?;
    deliver(
        deps,
        crate::schedules::VAT_PREP,
        VAT_SYSTEM_PROMPT,
        &material,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono_tz::Europe::Stockholm;
    use ea_core::store::events::RecordInput;
    use ea_core::store::kv::KvStore;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::session::SessionOutcome;
    use crate::triage::BoxedSession;

    // -- doubles -----------------------------------------------------------

    /// Answers every tool call with a scripted reply, recording what was asked.
    #[derive(Default)]
    struct SpyCaller {
        reply: String,
        calls: Mutex<Vec<(String, String, Value)>>,
    }

    impl SpyCaller {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ToolCaller for SpyCaller {
        async fn call(&self, connector: &str, tool: &str, args: Value) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string(), args));
            Ok(self.reply.clone())
        }
    }

    /// Records the requests a briefing makes and answers with fixed text.
    struct SpySessions {
        reply: String,
        requests: Mutex<Vec<SessionRequest>>,
    }

    impl SpySessions {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.to_string(),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<SessionRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl SessionBoundary for SpySessions {
        fn run_session(&self, req: SessionRequest) -> BoxedSession<'_> {
            self.requests.lock().unwrap().push(req);
            let reply = self.reply.clone();
            Box::pin(async move {
                Ok(SessionOutcome {
                    text: reply,
                    structured: None,
                    session_id: None,
                    cost_usd: None,
                })
            })
        }
    }

    #[derive(Default)]
    struct SpyPusher {
        sent: Mutex<Vec<String>>,
        fail: bool,
    }

    impl SpyPusher {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail: true,
            })
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Pusher for SpyPusher {
        fn notify<'a>(
            &'a self,
            text: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            let fail = self.fail;
            let text = text.to_string();
            Box::pin(async move {
                if fail {
                    anyhow::bail!("telegram is down");
                }
                self.sent.lock().unwrap().push(text);
                Ok(())
            })
        }

        fn push_action(
            &self,
            _id: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    // -- fixture -----------------------------------------------------------

    fn policy() -> Policy {
        let toml = r#"
[fortnox]
unpaid_invoices = "auto"
vat_summary = "auto"
account_ledger = "auto"
record_voucher = "approve"
"#;
        Policy::parse(toml).expect("the test policy parses")
    }

    struct Fixture {
        _dir: TempDir,
        conn: Arc<Mutex<Connection>>,
        events: EventStore,
        log: NotificationLog,
        sessions: Arc<SpySessions>,
        pusher: Arc<SpyPusher>,
        caller: Arc<SpyCaller>,
        deps: BriefingDeps<SpyCaller>,
    }

    fn fixture() -> Fixture {
        build(SpyPusher::new(), SpyCaller::new("[]"))
    }

    fn build(pusher: Arc<SpyPusher>, caller: Arc<SpyCaller>) -> Fixture {
        let dir = TempDir::new().unwrap();
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        let events = EventStore::new(Arc::clone(&conn));
        let log = NotificationLog::new(KvStore::new(Arc::clone(&conn)));
        let sessions = SpySessions::new("the briefing");
        let deps = BriefingDeps {
            events: events.clone(),
            runs: RunStore::new(Arc::clone(&conn)),
            log: NotificationLog::new(KvStore::new(Arc::clone(&conn))),
            sessions: Some(Arc::clone(&sessions) as Arc<dyn SessionBoundary>),
            pusher: Some(Arc::clone(&pusher) as Arc<dyn Pusher>),
            caller: Arc::clone(&caller),
            policy: policy(),
            time_zone: Stockholm,
            threshold: 60,
            daily_session_budget: 60,
        };
        Fixture {
            _dir: dir,
            conn,
            events,
            log,
            sessions,
            pusher,
            caller,
            deps,
        }
    }

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn record(events: &EventStore, source: &str, id: &str, kind: &str, payload: Value) -> Event {
        events
            .record(RecordInput {
                source: source.into(),
                external_id: id.into(),
                kind: kind.into(),
                payload,
            })
            .unwrap()
            .0
    }

    // -- the calendar cut ---------------------------------------------------

    /// "Today" is today where the owner is, and the cut is on the event's own
    /// start rather than on when it was recorded.
    #[tokio::test]
    async fn todays_calendar_takes_the_owners_day_not_the_utc_one() {
        let f = fixture();
        // 22:30 UTC on 24 September is 00:30 on the 25th in Stockholm.
        record(
            &f.events,
            "google",
            "late",
            kinds::CALENDAR_EVENT,
            json!({ "title": "midnight standup", "start": "2026-09-24T22:30:00Z" }),
        );
        record(
            &f.events,
            "google",
            "today",
            kinds::CALENDAR_EVENT,
            json!({ "title": "lunch", "start": "2026-09-25T10:00:00Z" }),
        );
        record(
            &f.events,
            "google",
            "tomorrow",
            kinds::CALENDAR_EVENT,
            json!({ "title": "next week", "start": "2026-09-30T10:00:00Z" }),
        );

        // Noon Stockholm on 25 September.
        let found = todays_calendar(&f.events, Stockholm, utc("2026-09-25T10:00:00Z")).unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["late", "today"],
            "the 22:30Z event is on the owner's 25th; the 30th is not"
        );
    }

    /// **The regression this test exists for.** `todays_calendar` used to read
    /// the newest `MATERIAL_LIMIT` calendar rows by id and only then filter on
    /// the start date. Calendar rows are recorded up to `LOOKAHEAD` (7 days)
    /// ahead and keep the id of their first insert, and they live 90 days — so
    /// on any calendar busier than 60 events a week, today's meeting sits
    /// *below* the id cut and vanished from the briefing with no error.
    ///
    /// The row that matters is recorded first and then buried under more than
    /// `MATERIAL_LIMIT` later rows, so a limit-then-filter implementation
    /// cannot pass.
    #[tokio::test]
    async fn todays_meeting_survives_a_hundred_later_calendar_rows() {
        let f = fixture();
        // Recorded a week ago, as a poll's LOOKAHEAD window does: lowest id.
        record(
            &f.events,
            "google",
            "the-one-that-matters",
            kinds::CALENDAR_EVENT,
            json!({ "title": "board meeting", "start": "2026-09-25T08:00:00Z" }),
        );
        // A hundred rows recorded since, none of them today's.
        for i in 0..100 {
            record(
                &f.events,
                "google",
                &format!("later-{i}"),
                kinds::CALENDAR_EVENT,
                json!({ "title": format!("standup {i}"), "start": "2026-10-02T08:00:00Z" }),
            );
        }

        let found = todays_calendar(&f.events, Stockholm, utc("2026-09-25T10:00:00Z")).unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["the-one-that-matters"],
            "an event's place in the briefing must not depend on other rows' ids"
        );
    }

    /// The same hazard on the conflict side, and the same window: a clash
    /// recorded days ago is still today's clash.
    #[tokio::test]
    async fn a_buried_conflict_is_still_on_todays_briefing() {
        let f = fixture();
        record(
            &f.events,
            "google",
            "the-clash",
            kinds::CALENDAR_CONFLICT,
            json!({ "overlap_start": "2026-09-25T09:00:00Z" }),
        );
        for i in 0..100 {
            record(
                &f.events,
                "google",
                &format!("later-{i}"),
                kinds::CALENDAR_CONFLICT,
                json!({ "overlap_start": "2026-10-02T09:00:00Z" }),
            );
        }
        let found = todays_calendar(&f.events, Stockholm, utc("2026-09-25T10:00:00Z")).unwrap();
        let ids: Vec<&str> = found.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(ids, vec!["the-clash"]);
    }

    #[tokio::test]
    async fn a_calendar_conflict_is_on_the_briefing_too() {
        let f = fixture();
        record(
            &f.events,
            "google",
            "clash",
            kinds::CALENDAR_CONFLICT,
            json!({ "overlap_start": "2026-09-25T09:00:00Z" }),
        );
        let found = todays_calendar(&f.events, Stockholm, utc("2026-09-25T10:00:00Z")).unwrap();
        assert_eq!(found.len(), 1);
    }

    // -- the morning briefing ----------------------------------------------

    #[tokio::test]
    async fn the_morning_briefing_carries_the_calendar_the_backlog_and_the_digest() {
        let f = fixture();
        record(
            &f.events,
            "google",
            "today",
            kinds::CALENDAR_EVENT,
            json!({ "title": "lunch with the accountant", "start": "2026-09-25T10:00:00Z" }),
        );
        let scored = record(
            &f.events,
            "canvas",
            "hand-in",
            "assignment",
            json!({ "title": "XX1002 hand-in" }),
        );
        record(
            &f.events,
            "kth",
            "unread",
            "mail",
            json!({ "subject": "examiner mail" }),
        );
        f.events.set_salience(scored.id, 90).unwrap();
        f.log.push_digest("[notion] a page moved (30)").unwrap();

        let outcome = run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();

        assert!(outcome.thought && outcome.sent);
        assert_eq!(outcome.digest_lines, 1);

        let prompt = &f.sessions.requests()[0].prompt;
        assert!(prompt.contains("lunch with the accountant"), "{prompt}");
        assert!(
            prompt.contains("examiner mail"),
            "the untriaged backlog: {prompt}"
        );
        assert!(
            prompt.contains("XX1002 hand-in"),
            "scored above 60: {prompt}"
        );
        assert!(prompt.contains("a page moved"), "the digest: {prompt}");
        assert!(prompt.contains("Scored at or above 60"), "{prompt}");
    }

    /// One message, not one per section.
    #[tokio::test]
    async fn the_morning_briefing_sends_exactly_one_message() {
        let f = fixture();
        for i in 0..5 {
            f.log.push_digest(format!("line {i}")).unwrap();
        }
        run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(f.pusher.sent(), vec!["the briefing".to_string()]);
    }

    #[tokio::test]
    async fn the_morning_briefing_clears_the_digest_it_reported() {
        let f = fixture();
        f.log.push_digest("one").unwrap();
        f.log.push_digest("two").unwrap();

        run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();

        assert_eq!(f.log.digest_len().unwrap(), 0);
    }

    /// **The digest must not be destroyed by a briefing that never arrived.**
    /// The owner would never learn what was in it.
    #[tokio::test]
    async fn a_failed_send_keeps_the_digest_for_the_next_briefing() {
        let f = build(SpyPusher::failing(), SpyCaller::new("[]"));
        f.log.push_digest("something the owner needs").unwrap();

        let err = run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .expect_err("a failed send must reach the breaker");
        assert!(format!("{err:#}").contains("telegram is down"));

        assert_eq!(
            f.log.digest().unwrap(),
            vec!["something the owner needs".to_string()],
            "the backlog survives to be reported tomorrow"
        );
    }

    /// No `claude` on PATH is a degraded daemon, not a failed job: the breaker
    /// must not trip on it, and the digest must survive.
    #[tokio::test]
    async fn without_a_session_runner_the_briefing_says_so_and_keeps_the_digest() {
        let mut f = fixture();
        f.deps.sessions = None;
        f.log.push_digest("held").unwrap();

        let outcome = run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();

        assert!(!outcome.thought && !outcome.sent);
        assert!(outcome.note.unwrap().contains("no session runner"));
        assert_eq!(f.log.digest_len().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_spent_budget_skips_the_briefing_without_failing_it() {
        let mut f = fixture();
        f.deps.daily_session_budget = 0;

        let outcome = run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();

        assert!(!outcome.thought);
        assert!(outcome.note.unwrap().contains("budget"));
        assert!(f.sessions.requests().is_empty());
    }

    // -- the accounting briefings ------------------------------------------

    #[tokio::test]
    async fn the_bookkeeping_pass_reads_both_invoice_sides_and_the_ledger() {
        let f = build(
            SpyPusher::new(),
            SpyCaller::new("[{\"DocumentNumber\":\"12\"}]"),
        );

        let outcome = run_bookkeeping_pass(&f.deps, utc("2026-09-28T07:00:00Z"))
            .await
            .unwrap();
        assert!(outcome.sent);

        let calls: Vec<(String, String)> = f
            .caller
            .calls()
            .into_iter()
            .map(|(connector, tool, _)| (connector, tool))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("fortnox".to_string(), "unpaid_invoices".to_string()),
                ("fortnox".to_string(), "unpaid_invoices".to_string()),
                ("fortnox".to_string(), "account_ledger".to_string()),
            ]
        );
        let kinds: Vec<Value> = f
            .caller
            .calls()
            .into_iter()
            .filter(|(_, tool, _)| tool == "unpaid_invoices")
            .map(|(_, _, args)| args["kind"].clone())
            .collect();
        assert_eq!(kinds, vec![json!("customer"), json!("supplier")]);
    }

    #[tokio::test]
    async fn the_vat_pass_reads_the_vat_accounts_and_the_deadline_list() {
        let f = build(SpyPusher::new(), SpyCaller::new("{\"vat_accounts\":[]}"));
        record(
            &f.events,
            "fortnox",
            "tax-deadline:moms:2026-09",
            kinds::TAX_DEADLINE,
            json!({ "label": "Momsdeklaration", "due_on": "2026-10-12" }),
        );

        run_vat_prep(&f.deps, utc("2026-10-01T07:00:00Z"))
            .await
            .unwrap();

        let calls: Vec<String> = f
            .caller
            .calls()
            .into_iter()
            .map(|(_, tool, _)| tool)
            .collect();
        assert_eq!(calls, vec!["vat_summary".to_string()]);

        let prompt = &f.sessions.requests()[0].prompt;
        assert!(prompt.contains("Momsdeklaration"), "{prompt}");
        assert!(prompt.contains("2026-10-12"), "{prompt}");
    }

    /// `vat_prep` reports and drafts nothing, and the instruction that says so
    /// has to actually be in the system prompt.
    #[tokio::test]
    async fn the_vat_pass_is_told_to_draft_nothing() {
        let f = fixture();
        run_vat_prep(&f.deps, utc("2026-10-01T07:00:00Z"))
            .await
            .unwrap();
        let system = &f.sessions.requests()[0].system_prompt;
        assert!(
            system.contains("Do not draft, propose or book anything"),
            "{system}"
        );
    }

    // -- the gate -----------------------------------------------------------

    /// A read the policy does not grade `auto` is refused, and the refusal
    /// names the grade. This is the same rule `run_watch_poll` holds.
    #[tokio::test]
    async fn a_read_the_policy_does_not_grade_auto_is_refused() {
        let f = fixture();
        let err = read_auto(
            f.caller.as_ref(),
            &f.deps.policy,
            FORTNOX,
            "record_voucher",
            json!({}),
        )
        .await
        .expect_err("an approve-graded tool must not be read automatically");
        let text = format!("{err:#}");
        assert!(text.contains("Approve"), "{text}");
        assert!(
            f.caller.calls().is_empty(),
            "and the connector is never called"
        );
    }

    /// A tool with no rule at all falls through to the gate's `approve`
    /// default, so an unlisted tool is refused rather than assumed harmless.
    #[tokio::test]
    async fn an_unlisted_tool_is_refused_too() {
        let f = fixture();
        assert!(read_auto(
            f.caller.as_ref(),
            &f.deps.policy,
            FORTNOX,
            "some_tool_nobody_declared",
            json!({}),
        )
        .await
        .is_err());
        assert!(f.caller.calls().is_empty());
    }

    /// Every briefing session names its model and is scoped to no connector.
    /// An unset model inherits the owner's `opus[1m]`; a connector scope would
    /// spawn child processes for tools the session cannot call.
    #[tokio::test]
    async fn every_briefing_session_pins_its_model_and_takes_no_connectors() {
        let f = fixture();
        run_morning_briefing(
            &f.deps,
            utc("2026-09-25T05:00:00Z"),
            utc("2026-09-24T05:00:00Z"),
        )
        .await
        .unwrap();
        run_bookkeeping_pass(&f.deps, utc("2026-09-28T07:00:00Z"))
            .await
            .unwrap();
        run_vat_prep(&f.deps, utc("2026-10-01T07:00:00Z"))
            .await
            .unwrap();

        let requests = f.sessions.requests();
        assert_eq!(
            requests.len(),
            3,
            "one session per briefing, not one per section"
        );
        for request in &requests {
            assert_eq!(
                request.model.as_deref(),
                Some(BRIEFING_MODEL),
                "{}: an unset model is inherited from the owner's interactive settings",
                request.kind
            );
            assert!(
                request.connectors.is_empty(),
                "{}: a briefing session reaches no connector",
                request.kind
            );
        }
        let kinds: Vec<&str> = requests.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                crate::schedules::MORNING_BRIEFING,
                crate::schedules::BOOKKEEPING_PASS,
                crate::schedules::VAT_PREP
            ]
        );
    }

    /// A connector that will not answer costs that section, not the briefing.
    #[tokio::test]
    async fn a_connector_read_that_fails_becomes_a_line_rather_than_a_dead_briefing() {
        struct Broken;
        impl ToolCaller for Broken {
            async fn call(&self, _: &str, _: &str, _: Value) -> anyhow::Result<String> {
                anyhow::bail!("401 Unauthorized")
            }
        }
        let f = fixture();
        let deps = BriefingDeps {
            events: f.deps.events.clone(),
            runs: f.deps.runs.clone(),
            log: NotificationLog::new(KvStore::new(Arc::clone(&f.conn))),
            sessions: f.deps.sessions.clone(),
            pusher: f.deps.pusher.clone(),
            caller: Arc::new(Broken),
            policy: policy(),
            time_zone: Stockholm,
            threshold: 60,
            daily_session_budget: 60,
        };

        let outcome = run_vat_prep(&deps, utc("2026-10-01T07:00:00Z"))
            .await
            .unwrap();
        assert!(outcome.sent, "the briefing still goes out");
        let prompt = &f.sessions.requests()[0].prompt;
        assert!(prompt.contains("401 Unauthorized"), "{prompt}");
    }

    #[test]
    fn an_empty_section_says_so_rather_than_vanishing() {
        let mut material = Material::default();
        material.push("Today's calendar", "");
        assert_eq!(material.heading("Today's calendar"), Some("(nothing)"));
        assert!(material.render().contains("## Today's calendar\n(nothing)"));
    }

    #[test]
    fn an_enormous_connector_reply_is_truncated_and_says_so() {
        let huge = "x".repeat(READ_LIMIT + 500);
        let cut = truncate(&huge, READ_LIMIT);
        assert!(cut.contains("truncated"));
        assert!(cut.chars().count() < huge.chars().count());
        assert_eq!(truncate("short", READ_LIMIT), "short");
    }
}
