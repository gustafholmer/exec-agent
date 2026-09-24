//! What the daemon sees: `watch_poll`'s three signals, across every
//! authorised account.
//!
//! The daemon calls `watch_poll` on a timer, parses the reply as
//! `[{ external_id, kind, payload }]`, and records each row under
//! `(source = "google", external_id)`. That key is the whole contract: a row
//! whose id it has seen before is an update, a row whose id is new is news.
//! Everything in this module exists to make those ids stable and the payloads
//! small.
//!
//! # Three signals, one poll
//!
//! * **Upcoming calendar events** — `gcal:<account>:<id>`, one per event in
//!   the next [`LOOKAHEAD`].
//! * **Calendar conflicts** — pairs of overlapping timed events, computed over
//!   the *merged* list from every account. The cross-account collision (a
//!   lecture against a private appointment) is the single most valuable one
//!   and is only visible because the accounts are polled into one `Vec`. One
//!   invitation accepted in *both* accounts is not one of them: it is two
//!   copies of one meeting, told apart by `iCalUID` in
//!   [`calendar::find_conflicts`], because reporting it every two minutes
//!   forever is how the owner learns to ignore conflicts.
//! * **Unread mail** — `gmail:<account>:<id>`, the most recent
//!   [`MAX_UNREAD`] messages matching [`UNREAD_QUERY`].
//!
//! # Why a conflict's id sorts its two event ids
//!
//! A conflict is an unordered pair. [`calendar::find_conflicts`] sorts by
//! start time, and Rust's sort is stable, so two events starting at the same
//! instant come out in whatever order the input happened to be in — which
//! depends on which account was polled first, and on the order Google returned
//! the page in. An id built as `first|second` would therefore flip between
//! polls, and each flip registers as a *new* event: the same clash would nag
//! the owner every two minutes forever. [`conflict_external_id`] sorts the two
//! fully-qualified event ids before joining them, so the pair has one id
//! whichever order it arrives in. `a_conflict_has_the_same_id_whichever_order_the_accounts_are_polled_in`
//! pins it.
//!
//! # Why one dead account no longer silences the other
//!
//! The first version of this module propagated the first error it met. The
//! reasoning was Canvas's: a poll that returns the healthy account's rows when
//! the other's token has lapsed *looks* complete, the daemon records a
//! success, the breaker never trips, and `ea status` stays green while half
//! the owner's mail goes unread.
//!
//! That reasoning assumed a lapsed token is rare. It is not: an OAuth client
//! in Google's **Testing** publishing status has its refresh tokens expired
//! every seven days, so `private` going dead is a weekly certainty, and a
//! weekly certainty that takes `work` down with it — and escalates the
//! scheduler's breaker to an hour's cooldown — is worse than a partial poll.
//! A silent work inbox for a week beats nothing.
//!
//! So a poll is now per-account. Every account that answers contributes its
//! rows; every account that fails contributes **one** synthetic row of
//! [`KIND_CONNECTOR_ERROR`], under the stable id
//! [`account_error_external_id`] — stable so that a grant that stays dead is
//! one event the owner is told about once, not one event every two minutes.
//! The payload is written to be *scored*, not merely recorded: it names the
//! account, says what stopped, says what is invisible while it is down, and
//! carries the exact command that fixes it. That row is now the only signal
//! for a single dead account, because the breaker will not trip for one.
//!
//! The payload is also deliberately **stable across polls**: the daemon keys
//! an event's identity on `(source, external_id)` and resets its triage state
//! whenever the payload changes, so a message carrying a timestamp or a
//! request id would re-notify the owner every two minutes. Nothing that
//! varies between two identical failures goes in it.
//!
//! If **every** account fails, [`poll`] returns `Err`. That is not a partial
//! poll, it is a connector that cannot do its job — a dead network, a deleted
//! token directory, a revoked OAuth client — and the breaker is exactly the
//! right thing to see it.
//!
//! # Why the mail body is truncated
//!
//! These payloads are read back into a tier-1 triage prompt, in batches. One
//! newsletter with a 200 KB body would cost more tokens than the rest of the
//! batch put together and tell triage nothing the first two thousand
//! characters did not. [`MAX_BODY_CHARS`] bounds it, and the truncation is
//! visible in the text rather than silent.

use anyhow::{bail, Context};
use chrono::{DateTime, SecondsFormat, Utc};
use ea_core::store::events::kinds;
use serde::{Deserialize, Serialize};

use crate::auth::{authorize_command, Auth};
use crate::calendar::{self, CalEvent, CalendarClient};
use crate::gmail::{GmailClient, Mail};

/// The `kind` on an upcoming-event row. Triage mutes and keyword rules match
/// on it, so these are stable names rather than anything derived per item.
///
/// The names themselves live in [`ea_core::store::events::kinds`], which owns
/// the table these rows land in: the daemon's briefings select on these
/// strings, and a private copy here would let a rename here empty a briefing
/// section with no error and no failing test.
pub const KIND_EVENT: &str = kinds::CALENDAR_EVENT;
/// The `kind` on a conflict row.
pub const KIND_CONFLICT: &str = kinds::CALENDAR_CONFLICT;
/// The `kind` on an unread-mail row.
pub const KIND_MAIL: &str = kinds::MAIL;
/// The `kind` on the synthetic row reporting an account this poll could not
/// read. See the module docs.
pub const KIND_CONNECTOR_ERROR: &str = kinds::CONNECTOR_ERROR;

/// What a failing account was being asked for, as it appears in an error
/// row's payload.
const SIGNAL_CALENDAR: &str = "calendar";
const SIGNAL_MAIL: &str = "mail";

/// How far ahead a poll looks on the calendar.
///
/// A week is the horizon on which a student can still act on a clash — move a
/// meeting, tell someone they will be late. Looking further would surface
/// conflicts nobody can do anything about yet, and a conflict that is still
/// there next week will be reported next week, because the id is stable and
/// dedup keeps it from being re-announced in between.
pub const LOOKAHEAD: chrono::Duration = chrono::Duration::days(7);

/// The Gmail search a poll runs.
///
/// Unread mail only — read mail is, by definition, mail the owner has already
/// dealt with — but `is:unread` *alone* is not that search. It includes the
/// Promotions and Social tabs, which in a typical mailbox hold far more
/// unread messages than the inbox does; with [`MAX_UNREAD`] capping a poll at
/// 25 messages per account, a week of newsletters would fill every poll and
/// starve out the mail that matters. `newer_than:7d` bounds it the same way
/// [`LOOKAHEAD`] bounds the calendar: mail older than a week that is still
/// unread is not news.
pub const UNREAD_QUERY: &str = "is:unread -category:promotions -category:social newer_than:7d";

/// How many unread messages one poll fetches per account. Each one costs a
/// `messages.get` (see the `gmail` module docs on the list-then-get cost), so
/// this is the number that bounds a poll's request count: 1 + 25 per account.
/// An inbox with more than 25 unread messages has a bigger problem than this
/// connector can solve, and the oldest of them are not news.
///
/// The `get`s run [`crate::gmail::GET_CONCURRENCY`] at a time, so 25 requests
/// cost four round trips rather than 25 — which is what lets a two-account
/// poll fit inside the daemon's `watch_poll` deadline on a link that is not
/// fast.
pub const MAX_UNREAD: usize = 25;

/// How much of a mail body reaches a payload. See the module docs.
pub const MAX_BODY_CHARS: usize = 2000;

/// One change, in the shape the daemon's event store records. `source` is
/// deliberately absent: the daemon supplies it from the connector name it
/// already knows, so a connector cannot write events attributed to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub external_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// `gcal:<account>:<id>` — the account is part of the id because the same
/// event id can legitimately exist in two accounts (an invitation accepted in
/// both), and they are two rows, not one.
pub fn event_external_id(account: &str, id: &str) -> String {
    format!("gcal:{account}:{id}")
}

/// `gmail:<account>:<id>`, for the same reason.
pub fn mail_external_id(account: &str, id: &str) -> String {
    format!("gmail:{account}:{id}")
}

/// `gerror:<account>` — the id of the synthetic row reporting that this
/// account could not be read.
///
/// One per account, independent of *what* failed. A grant that dies takes
/// both signals down together, and an id that varied with the failing signal
/// would report calendar-and-mail as a second event after reporting
/// calendar-only. The daemon dedups on it, so a persistently dead account is
/// one row that stays in the event log rather than 720 rows a day.
pub fn account_error_external_id(account: &str) -> String {
    format!("gerror:{account}")
}

/// The stable id of an unordered pair of clashing events. See the module docs
/// for why the sort is load-bearing rather than cosmetic.
pub fn conflict_external_id(a: &CalEvent, b: &CalEvent) -> String {
    let (lo, hi) = ordered(a, b);
    format!(
        "gconflict:{}|{}",
        event_external_id(&lo.account, &lo.id),
        event_external_id(&hi.account, &hi.id)
    )
}

/// The two events of a conflict in a canonical order — by fully-qualified
/// event id, the same order [`conflict_external_id`] joins them in, so the
/// payload does not churn either.
fn ordered<'a>(a: &'a CalEvent, b: &'a CalEvent) -> (&'a CalEvent, &'a CalEvent) {
    if event_external_id(&a.account, &a.id) <= event_external_id(&b.account, &b.id) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Poll every account for all three signals.
///
/// `now` is a parameter rather than `Utc::now()` so the window is
/// deterministic in tests. The accounts are walked in the order given; the
/// output order follows, but no consumer may depend on it — the ids are what
/// identify a row.
pub async fn poll(
    auth: &Auth,
    calendar_client: &CalendarClient,
    gmail_client: &GmailClient,
    accounts: &[String],
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<WatchEntry>> {
    // Not an empty poll. A connector that has never been authorised, or whose
    // token directory has been wiped, would otherwise report "nothing to
    // tell you" forever and look perfectly healthy doing it.
    if accounts.is_empty() {
        bail!(
            "no Google account has been authorised, so there is nothing to poll. \
             Authorise one with:\n  {}\n  {}",
            authorize_command("work"),
            authorize_command("private")
        );
    }

    let mut events: Vec<CalEvent> = Vec::new();
    let mut mail_rows: Vec<WatchEntry> = Vec::new();
    let mut failures: Vec<AccountFailure> = Vec::new();

    for account in accounts {
        let mut failure = AccountFailure::new(account);

        match calendar_client
            .list_events(auth, account, now, now + LOOKAHEAD)
            .await
            .with_context(|| format!("polling the calendar of Google account {account:?}"))
        {
            Ok(found) => events.extend(found),
            Err(err) => failure.note(SIGNAL_CALENDAR, &err),
        }

        match gmail_client
            .list_recent(auth, account, UNREAD_QUERY, MAX_UNREAD)
            .await
            .with_context(|| format!("polling the unread mail of Google account {account:?}"))
        {
            Ok(mail) => mail_rows.extend(mail.iter().map(mail_entry)),
            Err(err) => failure.note(SIGNAL_MAIL, &err),
        }

        if !failure.signals.is_empty() {
            failures.push(failure);
        }
    }

    // Not a partial poll: a connector that cannot read a single one of its
    // accounts is down, and the breaker should see it.
    if failures.len() == accounts.len() {
        bail!(
            "every authorised Google account failed this poll. {}",
            failures
                .iter()
                .map(AccountFailure::summary)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let mut entries: Vec<WatchEntry> = events.iter().map(event_entry).collect();

    // Over the merged list, deliberately: a work lecture against a private
    // appointment is the clash worth knowing about. An account that failed
    // contributes nothing here, so a clash involving it is simply not
    // reported this poll — under-reporting a conflict is the harmless
    // direction, and the error row says which account is missing.
    for (a, b) in calendar::find_conflicts(&events) {
        entries.push(conflict_entry(&a, &b));
    }

    entries.extend(mail_rows);
    entries.extend(failures.iter().map(AccountFailure::entry));

    Ok(entries)
}

/// What went wrong for one account during one poll.
struct AccountFailure {
    account: String,
    /// The signals that failed, in the order they were attempted.
    signals: Vec<&'static str>,
    /// One rendered error per failed signal, deduplicated: a dead grant
    /// produces the same sentence for the calendar and for the mail, and
    /// saying it twice makes the payload worse, not more informative.
    errors: Vec<String>,
}

impl AccountFailure {
    fn new(account: &str) -> Self {
        Self {
            account: account.to_string(),
            signals: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// `{err:#}` — the whole `anyhow` chain on one line. Never `{err:?}`:
    /// that is a backtrace, and this text ends up in a triage prompt.
    fn note(&mut self, signal: &'static str, err: &anyhow::Error) {
        self.signals.push(signal);
        let rendered = format!("{err:#}");
        if !self.errors.contains(&rendered) {
            self.errors.push(rendered);
        }
    }

    fn summary(&self) -> String {
        format!(
            "Account {:?} ({}): {}",
            self.account,
            self.signals.join(" and "),
            self.errors.join(" | ")
        )
    }

    /// The synthetic row. Everything in it is derived from the account and
    /// the error text, so two identical failures produce two identical
    /// payloads and the daemon does not treat the second as news.
    fn entry(&self) -> WatchEntry {
        let signals = self.signals.join(" and ");
        WatchEntry {
            external_id: account_error_external_id(&self.account),
            kind: KIND_CONNECTOR_ERROR.to_string(),
            payload: serde_json::json!({
                "account": self.account,
                "failed": self.signals,
                "error": self.errors.join(" | "),
                // Written for a triage prompt, which scores what it can read.
                // "A connector errored" is a 20; "you are not being told
                // about anything sent to this account" is the truth and is a
                // 90. The sentence has to carry the consequence, not the
                // mechanism.
                "summary": format!(
                    "Google account {:?} could not be read this poll ({signals}), so nothing \
                     arriving there is reaching you.",
                    self.account
                ),
                "impact": format!(
                    "While this lasts, no {signals} from the {:?} Google account appears in \
                     triage at all: a meeting moved, an exam result, an invoice, a message \
                     from a supervisor would all pass unseen. The other authorised accounts \
                     are unaffected and were reported in this same poll.",
                    self.account
                ),
                "remedy": format!(
                    "If the error mentions invalid_grant or HTTP 401, the grant is gone and \
                     only re-authorising restores it:\n  {}\nAn OAuth client left in \
                     Google's Testing publishing status expires its refresh tokens every 7 \
                     days, which is the usual cause. Anything else (a 5xx, a connection \
                     failure) is likely transient and the next poll will clear it.",
                    authorize_command(&self.account)
                ),
            }),
        }
    }
}

/// One upcoming event, as a watch row.
pub fn event_entry(event: &CalEvent) -> WatchEntry {
    WatchEntry {
        external_id: event_external_id(&event.account, &event.id),
        kind: KIND_EVENT.to_string(),
        payload: serde_json::json!({
            "account": event.account,
            "title": event.title,
            "start": stamp(event.start),
            "end": stamp(event.end),
            "all_day": event.all_day,
            "location": event.location,
            "attendees": event.attendees,
            "html_link": event.html_link,
        }),
    }
}

/// One clash, as a watch row.
pub fn conflict_entry(a: &CalEvent, b: &CalEvent) -> WatchEntry {
    let (first, second) = ordered(a, b);
    WatchEntry {
        external_id: conflict_external_id(a, b),
        kind: KIND_CONFLICT.to_string(),
        payload: serde_json::json!({
            "overlap_start": stamp(first.start.max(second.start)),
            "overlap_end": stamp(first.end.min(second.end)),
            "cross_account": first.account != second.account,
            "events": [brief(first), brief(second)],
        }),
    }
}

/// One unread message, as a watch row, with its body bounded.
pub fn mail_entry(mail: &Mail) -> WatchEntry {
    WatchEntry {
        external_id: mail_external_id(&mail.account, &mail.id),
        kind: KIND_MAIL.to_string(),
        payload: serde_json::json!({
            "account": mail.account,
            "thread_id": mail.thread_id,
            "from": mail.from,
            "subject": mail.subject,
            "snippet": mail.snippet,
            "body": truncate_body(&mail.body),
            "received_at": stamp(mail.received_at),
            "labels": mail.labels,
        }),
    }
}

/// Just enough of an event to identify it inside a conflict payload. The full
/// event is already its own row.
fn brief(event: &CalEvent) -> serde_json::Value {
    serde_json::json!({
        "external_id": event_external_id(&event.account, &event.id),
        "account": event.account,
        "title": event.title,
        "start": stamp(event.start),
        "end": stamp(event.end),
        "html_link": event.html_link,
    })
}

/// Cut a body to [`MAX_BODY_CHARS`] characters, saying so where it was cut.
///
/// Counted in `char`s, not bytes: slicing a UTF-8 string at byte 2000 can land
/// inside a multi-byte character, and `å` is not an edge case in this owner's
/// mail.
pub fn truncate_body(body: &str) -> String {
    let total = body.chars().count();
    if total <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let head: String = body.chars().take(MAX_BODY_CHARS).collect();
    format!("{head}… [truncated; {total} characters in the original]")
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Offline, like the rest of this crate: one `wiremock` server on loopback
// serves both the Calendar and the Gmail paths, and the two accounts are told
// apart by the bearer token each one's stored access token produces.

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Arc;

    use wiremock::matchers::{header, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::{RefreshBackend, RefreshError, RefreshResponse, TokenStore, Tokens};

    const WORK_TOKEN: &str = "ya29.WORK-ACCESS-do-not-leak";
    const PRIVATE_TOKEN: &str = "ya29.PRIVATE-ACCESS-do-not-leak";

    struct UnusedBackend;

    impl RefreshBackend for UnusedBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a,
            >,
        > {
            Box::pin(async { panic!("a poll must not need to refresh a healthy token") })
        }
    }

    /// An `Auth` holding a healthy, distinct access token for each account, so
    /// a mock can tell the two apart by the `Authorization` header.
    fn auth_for(dir: &Path, accounts: &[(&str, &str)]) -> Auth {
        auth_for_with_backend(dir, accounts, Arc::new(UnusedBackend))
    }

    /// The weekly case: Google has forgotten the grant. A 401 now forces a
    /// refresh, so a test that wants a poll to *fail* has to say what the
    /// refresh finds.
    struct RevokedBackend;

    impl RefreshBackend for RevokedBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a,
            >,
        > {
            Box::pin(async { Err(RefreshError::InvalidGrant) })
        }
    }

    fn auth_for_with_backend(
        dir: &Path,
        accounts: &[(&str, &str)],
        backend: Arc<dyn RefreshBackend>,
    ) -> Auth {
        let store = TokenStore::new(Some(dir.to_path_buf()));
        for (account, access_token) in accounts {
            store
                .write(
                    account,
                    &Tokens {
                        access_token: (*access_token).to_string(),
                        refresh_token: "1//not-used-here".to_string(),
                        expiry: Utc::now() + chrono::Duration::hours(1),
                        scope: crate::auth::SCOPES.join(" "),
                    },
                )
                .unwrap();
        }
        Auth::with_backend(store, backend)
    }

    fn both_accounts(dir: &Path) -> Auth {
        auth_for(dir, &[("work", WORK_TOKEN), ("private", PRIVATE_TOKEN)])
    }

    /// Both accounts authorised, but Google has revoked the grants: the
    /// state a Testing-status OAuth client reaches every seven days.
    fn both_accounts_revoked(dir: &Path) -> Auth {
        auth_for_with_backend(
            dir,
            &[("work", WORK_TOKEN), ("private", PRIVATE_TOKEN)],
            Arc::new(RevokedBackend),
        )
    }

    fn json(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    /// Mount a calendar page for whichever account carries `token`.
    async fn mount_events(server: &MockServer, token: &str, items: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path_matcher("/calendars/primary/events"))
            .and(header("authorization", bearer(token).as_str()))
            .respond_with(json(serde_json::json!({ "items": items })))
            .mount(server)
            .await;
    }

    /// Mount a `messages.list` plus one `messages.get` per message for
    /// whichever account carries `token`.
    async fn mount_mail(server: &MockServer, token: &str, messages: &[serde_json::Value]) {
        let refs: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "id": m["id"], "threadId": m["threadId"] }))
            .collect();
        Mock::given(method("GET"))
            .and(path_matcher("/users/me/messages"))
            .and(header("authorization", bearer(token).as_str()))
            .respond_with(json(serde_json::json!({ "messages": refs })))
            .mount(server)
            .await;
        for message in messages {
            let id = message["id"].as_str().unwrap().to_string();
            Mock::given(method("GET"))
                .and(path_matcher(format!("/users/me/messages/{id}")))
                .and(header("authorization", bearer(token).as_str()))
                .respond_with(json(message.clone()))
                .mount(server)
                .await;
        }
    }

    async fn mount_no_mail(server: &MockServer, token: &str) {
        mount_mail(server, token, &[]).await;
    }

    /// Every endpoint refuses this account's bearer token: what a revoked
    /// grant looks like from the outside. Paired with
    /// [`both_accounts_revoked`], whose refresh also fails, so the forced
    /// 401 refresh finds nothing to recover.
    async fn mount_dead(server: &MockServer, token: &str) {
        Mock::given(method("GET"))
            .and(header("authorization", bearer(token).as_str()))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
                "application/json",
            ))
            .mount(server)
            .await;
    }

    fn timed(id: &str, summary: &str, start: &str, end: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "summary": summary,
            "start": { "dateTime": start },
            "end": { "dateTime": end },
            "htmlLink": format!("https://calendar.google.com/{id}"),
            "updated": "2026-09-20T10:00:00Z",
        })
    }

    fn message(id: &str, subject: &str, body: &str) -> serde_json::Value {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        serde_json::json!({
            "id": id,
            "threadId": format!("t-{id}"),
            "labelIds": ["INBOX", "UNREAD"],
            "snippet": "a snippet",
            "internalDate": "1700000000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "From", "value": "someone@example.com" },
                    { "name": "Subject", "value": subject },
                ],
                "body": { "data": URL_SAFE_NO_PAD.encode(body.as_bytes()) },
            },
        })
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a timestamp")
    }

    fn clients(server: &MockServer) -> (CalendarClient, GmailClient) {
        (
            CalendarClient::new(&server.uri()).unwrap(),
            GmailClient::new(&server.uri()).unwrap(),
        )
    }

    fn accounts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn of_kind<'a>(entries: &'a [WatchEntry], kind: &str) -> Vec<&'a WatchEntry> {
        entries.iter().filter(|e| e.kind == kind).collect()
    }

    const NOW: &str = "2026-09-24T08:00:00Z";

    // -----------------------------------------------------------------------
    // The three signals
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_poll_reports_events_conflicts_and_unread_mail_from_every_account() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts(tmp.path());

        mount_events(
            &server,
            WORK_TOKEN,
            serde_json::json!([timed(
                "w1",
                "Lecture",
                "2026-09-25T10:00:00Z",
                "2026-09-25T12:00:00Z"
            )]),
        )
        .await;
        mount_events(
            &server,
            PRIVATE_TOKEN,
            serde_json::json!([timed(
                "p1",
                "Dentist",
                "2026-09-25T11:00:00Z",
                "2026-09-25T11:30:00Z"
            )]),
        )
        .await;
        mount_mail(&server, WORK_TOKEN, &[message("m1", "Exam", "Body one")]).await;
        mount_mail(
            &server,
            PRIVATE_TOKEN,
            &[message("m2", "Invoice", "Body two")],
        )
        .await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .unwrap();

        let ids: BTreeSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert!(ids.contains("gcal:work:w1"), "{ids:?}");
        assert!(ids.contains("gcal:private:p1"), "{ids:?}");
        assert!(ids.contains("gmail:work:m1"), "{ids:?}");
        assert!(ids.contains("gmail:private:m2"), "{ids:?}");

        assert_eq!(of_kind(&entries, KIND_EVENT).len(), 2);
        assert_eq!(of_kind(&entries, KIND_MAIL).len(), 2);

        let conflicts = of_kind(&entries, KIND_CONFLICT);
        assert_eq!(
            conflicts.len(),
            1,
            "the work lecture and the private appointment overlap: {entries:#?}"
        );
        assert_eq!(
            conflicts[0].payload["cross_account"], true,
            "the cross-account clash is the one worth reporting"
        );
        assert_eq!(
            conflicts[0].payload["overlap_start"],
            "2026-09-25T11:00:00Z"
        );
        assert_eq!(conflicts[0].payload["overlap_end"], "2026-09-25T11:30:00Z");
    }

    /// The owner accepted one invitation in both accounts. Merging the
    /// calendars makes those two copies overlap 100%, and without the
    /// `iCalUID` check every poll would report a permanent cross-account
    /// clash with a stable id that dedup never retires — noise in exactly the
    /// signal this connector exists to produce. Both copies are still their
    /// own event row: they really are two calendar entries.
    #[tokio::test]
    async fn one_invitation_accepted_in_both_accounts_is_not_reported_as_a_clash() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts(tmp.path());

        let mut work_copy = timed(
            "w-copy",
            "Sprint review",
            "2026-09-25T10:00:00Z",
            "2026-09-25T11:00:00Z",
        );
        work_copy["iCalUID"] = serde_json::json!("sprint-review@google.com");
        let mut private_copy = timed(
            "p-copy",
            "Sprint review",
            "2026-09-25T10:00:00Z",
            "2026-09-25T11:00:00Z",
        );
        private_copy["iCalUID"] = serde_json::json!("sprint-review@google.com");

        mount_events(&server, WORK_TOKEN, serde_json::json!([work_copy])).await;
        mount_events(&server, PRIVATE_TOKEN, serde_json::json!([private_copy])).await;
        mount_no_mail(&server, WORK_TOKEN).await;
        mount_no_mail(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .unwrap();

        assert_eq!(
            of_kind(&entries, KIND_CONFLICT),
            Vec::<&WatchEntry>::new(),
            "one meeting in two calendars must not be a conflict: {entries:#?}"
        );
        assert_eq!(
            of_kind(&entries, KIND_EVENT).len(),
            2,
            "both copies are still real calendar entries"
        );
    }

    #[tokio::test]
    async fn an_event_payload_carries_what_triage_needs() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_for(tmp.path(), &[("work", WORK_TOKEN)]);

        mount_events(
            &server,
            WORK_TOKEN,
            serde_json::json!([{
                "id": "w1",
                "summary": "Lecture",
                "start": { "dateTime": "2026-09-25T10:00:00Z" },
                "end": { "dateTime": "2026-09-25T12:00:00Z" },
                "location": "D1",
                "attendees": [{ "email": "prof@kth.se" }],
                "htmlLink": "https://calendar.google.com/w1",
                "updated": "2026-09-20T10:00:00Z",
            }]),
        )
        .await;
        mount_no_mail(&server, WORK_TOKEN).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work"]), at(NOW))
            .await
            .unwrap();

        let event = of_kind(&entries, KIND_EVENT)[0];
        assert_eq!(event.external_id, "gcal:work:w1");
        assert_eq!(event.payload["account"], "work");
        assert_eq!(event.payload["title"], "Lecture");
        assert_eq!(event.payload["start"], "2026-09-25T10:00:00Z");
        assert_eq!(event.payload["end"], "2026-09-25T12:00:00Z");
        assert_eq!(event.payload["all_day"], false);
        assert_eq!(event.payload["location"], "D1");
        assert_eq!(event.payload["attendees"][0], "prof@kth.se");
        assert_eq!(event.payload["html_link"], "https://calendar.google.com/w1");
    }

    // -----------------------------------------------------------------------
    // The conflict id, and why it is sorted
    // -----------------------------------------------------------------------

    /// The test the whole sort exists for. Both events start at the same
    /// instant, so `find_conflicts`'s stable sort leaves them in input order —
    /// which is the order the accounts were polled in. An unsorted id would
    /// flip, and the same clash would re-register as news on every poll.
    #[tokio::test]
    async fn a_conflict_has_the_same_id_whichever_order_the_accounts_are_polled_in() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts(tmp.path());

        // Identical starts: the tie is broken by input order alone.
        mount_events(
            &server,
            WORK_TOKEN,
            serde_json::json!([timed(
                "w1",
                "Lecture",
                "2026-09-25T10:00:00Z",
                "2026-09-25T12:00:00Z"
            )]),
        )
        .await;
        mount_events(
            &server,
            PRIVATE_TOKEN,
            serde_json::json!([timed(
                "p1",
                "Dentist",
                "2026-09-25T10:00:00Z",
                "2026-09-25T10:30:00Z"
            )]),
        )
        .await;
        mount_no_mail(&server, WORK_TOKEN).await;
        mount_no_mail(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);

        let forward = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .unwrap();
        let reversed = poll(&auth, &cal, &mail, &accounts(&["private", "work"]), at(NOW))
            .await
            .unwrap();

        assert_eq!(of_kind(&forward, KIND_CONFLICT).len(), 1, "{forward:#?}");
        assert_eq!(of_kind(&reversed, KIND_CONFLICT).len(), 1, "{reversed:#?}");

        let ids: BTreeSet<&str> = of_kind(&forward, KIND_CONFLICT)
            .into_iter()
            .chain(of_kind(&reversed, KIND_CONFLICT))
            .map(|e| e.external_id.as_str())
            .collect();
        assert_eq!(
            ids.len(),
            1,
            "one clash must be one row across both polls, not two: {ids:?}"
        );

        // And the payload is canonical too, so a re-record is not a change.
        let forward_payload = &of_kind(&forward, KIND_CONFLICT)[0].payload;
        let reversed_payload = &of_kind(&reversed, KIND_CONFLICT)[0].payload;
        assert_eq!(forward_payload, reversed_payload);
    }

    #[test]
    fn conflict_external_id_is_symmetric() {
        let a = CalEvent {
            id: "zzz".into(),
            account: "work".into(),
            ical_uid: None,
            title: "A".into(),
            start: at("2026-09-25T10:00:00Z"),
            end: at("2026-09-25T11:00:00Z"),
            all_day: false,
            location: None,
            attendees: vec![],
            html_link: String::new(),
            updated: String::new(),
        };
        let b = CalEvent {
            id: "aaa".into(),
            account: "private".into(),
            ..a.clone()
        };
        assert_eq!(conflict_external_id(&a, &b), conflict_external_id(&b, &a));
        assert_eq!(
            conflict_external_id(&a, &b),
            "gconflict:gcal:private:aaa|gcal:work:zzz"
        );
    }

    // -----------------------------------------------------------------------
    // Accounts are separate universes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_same_message_id_in_two_accounts_is_two_entries() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts(tmp.path());

        mount_events(&server, WORK_TOKEN, serde_json::json!([])).await;
        mount_events(&server, PRIVATE_TOKEN, serde_json::json!([])).await;
        mount_mail(&server, WORK_TOKEN, &[message("same", "At work", "One")]).await;
        mount_mail(&server, PRIVATE_TOKEN, &[message("same", "At home", "Two")]).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .unwrap();

        let ids: BTreeSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            ["gmail:private:same", "gmail:work:same"]
                .into_iter()
                .collect::<BTreeSet<&str>>(),
            "one message id in two mailboxes is two pieces of news"
        );
    }

    // -----------------------------------------------------------------------
    // The payload budget
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_very_long_mail_body_is_truncated_before_it_reaches_a_payload() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_for(tmp.path(), &[("work", WORK_TOKEN)]);

        let long = "å".repeat(50_000);
        mount_events(&server, WORK_TOKEN, serde_json::json!([])).await;
        mount_mail(&server, WORK_TOKEN, &[message("m1", "Newsletter", &long)]).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work"]), at(NOW))
            .await
            .unwrap();

        let body = entries[0].payload["body"].as_str().unwrap();
        let chars = body.chars().count();
        assert!(
            chars > MAX_BODY_CHARS && chars < MAX_BODY_CHARS + 100,
            "a 50k body must arrive at about {MAX_BODY_CHARS} characters, got {chars}"
        );
        assert!(
            body.contains("truncated"),
            "the cut must be visible: {body}"
        );
        assert!(
            body.starts_with(&"å".repeat(100)),
            "the head of the body must survive intact"
        );
    }

    #[test]
    fn a_short_body_is_left_exactly_as_it_is() {
        assert_eq!(truncate_body("Räksmörgås"), "Räksmörgås");
        let exact = "x".repeat(MAX_BODY_CHARS);
        assert_eq!(truncate_body(&exact), exact);
    }

    // -----------------------------------------------------------------------
    // Failing loudly
    // -----------------------------------------------------------------------

    /// The ruling this module was rewritten under. A Testing-status OAuth
    /// client expires its refresh tokens weekly, so `private` dying is not an
    /// edge case — and taking `work` down with it for a week is worse than
    /// handing over a partial poll that says, loudly, which half is missing.
    #[tokio::test]
    async fn one_dead_account_still_reports_the_other_and_exactly_one_error_row() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts_revoked(tmp.path());

        mount_events(
            &server,
            WORK_TOKEN,
            serde_json::json!([timed(
                "w1",
                "Lecture",
                "2026-09-25T10:00:00Z",
                "2026-09-25T12:00:00Z"
            )]),
        )
        .await;
        mount_mail(&server, WORK_TOKEN, &[message("m1", "Exam", "Body one")]).await;
        mount_dead(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .expect("one dead account must not silence the healthy one");

        let ids: BTreeSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert!(ids.contains("gcal:work:w1"), "{ids:?}");
        assert!(ids.contains("gmail:work:m1"), "{ids:?}");

        let errors = of_kind(&entries, KIND_CONNECTOR_ERROR);
        assert_eq!(
            errors.len(),
            1,
            "one dead account, one error row: {entries:#?}"
        );
        assert_eq!(errors[0].external_id, "gerror:private");
        assert_eq!(errors[0].payload["account"], "private");
        assert_eq!(
            errors[0].payload["failed"],
            serde_json::json!(["calendar", "mail"]),
            "a dead grant takes both signals, and that is one row, not two"
        );

        let rendered = errors[0].payload.to_string();
        assert!(rendered.contains("401"), "{rendered}");
        assert!(
            rendered.contains("ea-google-authorize private"),
            "the payload must carry the command that fixes it: {rendered}"
        );
        assert!(
            !rendered.contains(WORK_TOKEN) && !rendered.contains(PRIVATE_TOKEN),
            "a token reached an event payload, which is a triage prompt: {rendered}"
        );
    }

    /// The error row is the *only* signal now — the breaker will not trip for
    /// one dead account — so it has to arrive once and stay put. The daemon
    /// dedups on `(source, external_id)` and re-triages whenever a payload
    /// changes, so a row whose id or payload churned would nag the owner
    /// every two minutes until they learned to ignore it.
    #[tokio::test]
    async fn a_persistently_dead_account_reports_the_same_row_on_every_poll() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts_revoked(tmp.path());

        mount_events(&server, WORK_TOKEN, serde_json::json!([])).await;
        mount_no_mail(&server, WORK_TOKEN).await;
        mount_dead(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);
        let accounts = accounts(&["work", "private"]);

        let first = poll(&auth, &cal, &mail, &accounts, at(NOW)).await.unwrap();
        let second = poll(&auth, &cal, &mail, &accounts, at("2026-09-24T08:02:00Z"))
            .await
            .unwrap();

        let first_errors = of_kind(&first, KIND_CONNECTOR_ERROR);
        let second_errors = of_kind(&second, KIND_CONNECTOR_ERROR);
        assert_eq!(first_errors.len(), 1);
        assert_eq!(
            second_errors.len(),
            1,
            "a second poll must not add a second error row: {second:#?}"
        );
        assert_eq!(
            first_errors[0], second_errors[0],
            "identical id AND identical payload, or the daemon treats the second as news"
        );
    }

    /// Not a partial poll: a connector that cannot read any of its accounts
    /// is down, and the breaker is exactly the right thing to see it.
    #[tokio::test]
    async fn every_account_failing_is_an_error_so_the_breaker_sees_it() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts_revoked(tmp.path());

        mount_dead(&server, WORK_TOKEN).await;
        mount_dead(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);
        let err = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .expect_err("no account answered; that is a connector failure");
        let rendered = format!("{err:#}");

        assert!(
            rendered.contains("every authorised Google account"),
            "{rendered}"
        );
        assert!(rendered.contains("work"), "{rendered}");
        assert!(rendered.contains("private"), "{rendered}");
        assert!(!rendered.contains(WORK_TOKEN), "{rendered}");
        assert!(!rendered.contains(PRIVATE_TOKEN), "{rendered}");
    }

    /// Half an account is still a reported account: the calendar answered, so
    /// `work` contributes its events *and* an error row saying the mail did
    /// not.
    #[tokio::test]
    async fn one_signal_failing_reports_the_other_signal_and_says_which_is_missing() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = both_accounts(tmp.path());

        mount_events(
            &server,
            WORK_TOKEN,
            serde_json::json!([timed(
                "w1",
                "Lecture",
                "2026-09-25T10:00:00Z",
                "2026-09-25T12:00:00Z"
            )]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path_matcher("/users/me/messages"))
            .and(header("authorization", bearer(WORK_TOKEN).as_str()))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        mount_events(&server, PRIVATE_TOKEN, serde_json::json!([])).await;
        mount_no_mail(&server, PRIVATE_TOKEN).await;

        let (cal, mail) = clients(&server);
        let entries = poll(&auth, &cal, &mail, &accounts(&["work", "private"]), at(NOW))
            .await
            .unwrap();

        assert_eq!(of_kind(&entries, KIND_EVENT).len(), 1);
        let errors = of_kind(&entries, KIND_CONNECTOR_ERROR);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].payload["failed"],
            serde_json::json!(["mail"]),
            "the calendar answered; only the mail is missing: {:#?}",
            errors[0]
        );
        assert!(errors[0].payload["error"].as_str().unwrap().contains("500"));
    }

    /// The single-account case of the all-accounts rule: with one account
    /// authorised, that account failing *is* every account failing, so the
    /// poll errors rather than reporting a calendar with no mail beside it.
    #[tokio::test]
    async fn the_only_accounts_mail_failing_fails_the_whole_poll() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_for(tmp.path(), &[("work", WORK_TOKEN)]);

        mount_events(&server, WORK_TOKEN, serde_json::json!([])).await;
        Mock::given(method("GET"))
            .and(path_matcher("/users/me/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let (cal, mail) = clients(&server);
        let err = poll(&auth, &cal, &mail, &accounts(&["work"]), at(NOW))
            .await
            .expect_err("mail failing must fail the poll, not silently drop the mail signal");
        assert!(format!("{err:#}").contains("500"), "{err:#}");
    }

    /// Silence is the failure mode this connector is most likely to have, and
    /// "no accounts authorised" is the way in.
    #[tokio::test]
    async fn a_poll_with_no_authorised_accounts_is_an_error_not_an_empty_list() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_for(tmp.path(), &[]);

        let (cal, mail) = clients(&server);
        let err = poll(&auth, &cal, &mail, &[], at(NOW))
            .await
            .expect_err("an unauthorised connector must complain, not go quiet");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("ea-google-authorize work"), "{rendered}");
    }

    // -----------------------------------------------------------------------
    // Window
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_calendar_window_starts_now_and_runs_a_week() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_for(tmp.path(), &[("work", WORK_TOKEN)]);

        mount_events(&server, WORK_TOKEN, serde_json::json!([])).await;
        mount_no_mail(&server, WORK_TOKEN).await;

        let (cal, mail) = clients(&server);
        poll(&auth, &cal, &mail, &accounts(&["work"]), at(NOW))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let events_request = requests
            .iter()
            .find(|r| r.url.path() == "/calendars/primary/events")
            .expect("the calendar was polled");
        let query: Vec<(String, String)> = events_request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let get = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert!(
            get("timeMin").starts_with("2026-09-24T08:00:00"),
            "{query:?}"
        );
        assert!(
            get("timeMax").starts_with("2026-10-01T08:00:00"),
            "{query:?}"
        );
    }
}
