//! The Google Calendar client.
//!
//! Reads events from a single account's primary calendar. Read-only by
//! construction, same as the Canvas connector: every method here is a `GET`.
//!
//! # Two kinds of boundary
//!
//! A Google event's `start`/`end` is either a `dateTime` (an instant, for a
//! timed event) or a bare `date` (a calendar day, for an all-day event —
//! every exam and every deadline that a student marks "all day" rather than
//! giving it a clock time). [`parse_point`] handles both; an implementation
//! that only reads `dateTime` silently drops every all-day event, which is
//! precisely the case [`normalize`]'s tests exist to catch.
//!
//! # Why both accounts feed one list
//!
//! [`find_conflicts`] takes a single slice of [`CalEvent`], not one per
//! account, on purpose: a meeting the `work` account books over a `private`
//! commitment (or the reverse) is invisible to either calendar alone and is
//! the single most valuable thing this connector can surface. See the
//! `a_conflict_spanning_work_and_private_accounts_is_found` test.
//!
//! Merging the accounts creates one false positive that has to be removed
//! again: an invitation the owner accepted in *both* accounts appears twice,
//! overlapping itself completely. [`same_meeting`] uses Google's `iCalUID` —
//! stable across calendars, unlike the per-calendar event `id` — to tell one
//! meeting seen twice from two meetings at once.
//!
//! # HTTP hardening
//!
//! The same standard as [`crate::auth`] and `ea-canvas`'s client: no
//! credential in a log, an error, a panic, or a `Debug` impl, including inside
//! a URL.
//!
//! * The access token travels in the `Authorization` header, never in a URL
//!   or a log line. Every `reqwest` error goes through `.without_url()`.
//! * The HTTP client follows no redirects (`redirect::Policy::none()`) — the
//!   same reasoning as Canvas: `reqwest`'s default policy does not know about
//!   request bodies or same-host scheme downgrades, and this API has no
//!   legitimate redirect to follow.
//! * A response is checked for a JSON content type before it is deserialised.
//!   Google, like Canvas, can answer a 200 with an HTML consent or error page
//!   under the wrong conditions, and that must not be read as an empty page
//!   of events.
//! * Unlike Canvas's `Link: rel="next"` header, Google's `nextPageToken` is a
//!   plain query-string value, not a server-supplied absolute URL, so there is
//!   no second host it could redirect the client to — the same-origin check
//!   Canvas needs does not apply here. The redirect policy and the
//!   content-type check still do, identically, and the page loop is still
//!   capped so a misbehaving server cannot make a single call run forever.

use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::auth::Auth;

/// Google's Calendar API v3 base. Overridable at the [`CalendarClient`] level
/// (that is the whole reason the client is a separate, constructible type
/// rather than a free function that hardcodes the host) so tests can point it
/// at a `wiremock` server instead.
pub const GOOGLE_CALENDAR_BASE: &str = "https://www.googleapis.com/calendar/v3/";

/// How many `nextPageToken` hops to follow before calling it a loop. A busy
/// week does not need more than a couple of pages at 250 events each; fifty
/// pages is two orders of magnitude of headroom and still terminates.
const MAX_PAGES: usize = 50;

/// Per-request deadline, same value and same reasoning as Canvas and
/// `auth::HttpRefreshBackend`: bound one HTTP round trip, not the caller's
/// whole budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an unexpected response body to quote back.
const BODY_SNIPPET: usize = 300;

/// Events per page. Google's documented maximum is 2500; 250 is its default
/// and plenty to make the pagination path exercise in practice rather than
/// only in a test with an artificially tiny page.
const PAGE_SIZE: &str = "250";

// ---------------------------------------------------------------------------
// The normalised model
// ---------------------------------------------------------------------------

/// One calendar event, normalised from Google's wire shape and tagged with
/// the account it came from — the tag is what makes cross-account conflict
/// detection possible once events from `work` and `private` are merged into
/// one `Vec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalEvent {
    pub id: String,
    pub account: String,
    /// Google's cross-calendar identity for the meeting (`iCalUID` on
    /// `events.list`), when it sends one. Two accounts that both accepted the
    /// same invitation hold two events with two different `id`s and *one*
    /// `iCalUID`; [`find_conflicts`] uses that to tell "the same meeting,
    /// twice" from "two meetings at once". `None` when Google omitted it —
    /// see [`same_meeting`] for why a missing id never collapses anything.
    pub ical_uid: Option<String>,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub all_day: bool,
    pub location: Option<String>,
    pub attendees: Vec<String>,
    pub html_link: String,
    pub updated: String,
}

// ---------------------------------------------------------------------------
// The raw wire shape
// ---------------------------------------------------------------------------

/// A Google Calendar `events.list` item, as it arrives on the wire. Every
/// field is optional because Google's own schema documents almost all of them
/// as such, and the whole point of [`normalize`] is to turn "field absent" and
/// "field present but empty" into `None`/a safe default rather than a panic.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawEvent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "iCalUID")]
    i_cal_uid: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    start: Option<RawPoint>,
    #[serde(default)]
    end: Option<RawPoint>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    attendees: Option<Vec<RawAttendee>>,
    #[serde(default, rename = "htmlLink")]
    html_link: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

/// One end of an event: either an instant (`dateTime`) or a bare calendar day
/// (`date`). See the module docs for why both must be handled.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawPoint {
    #[serde(default, rename = "dateTime")]
    date_time: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawAttendee {
    #[serde(default)]
    email: Option<String>,
}

/// One page of `events.list`.
#[derive(Debug, Default, Deserialize)]
struct EventsPage {
    #[serde(default)]
    items: Vec<RawEvent>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Turn one raw event into a [`CalEvent`], or drop it.
///
/// An event is dropped (`None`) rather than erroring — a single malformed or
/// cancelled event must not fail the whole list — when: it is `cancelled`;
/// it has no `id`; or either boundary has neither `dateTime` nor a parseable
/// `date`. A missing `summary` becomes the title `(no title)` rather than
/// being dropped, because an untitled event is still an event a student needs
/// to see on their calendar.
pub fn normalize(raw: &RawEvent, account: &str) -> Option<CalEvent> {
    if raw.status.as_deref() == Some("cancelled") {
        return None;
    }
    let id = raw.id.clone()?;
    let (start, all_day) = parse_point(raw.start.as_ref()?)?;
    let (end, _) = parse_point(raw.end.as_ref()?)?;

    let attendees = raw
        .attendees
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.email.clone())
        .collect();

    Some(CalEvent {
        id,
        account: account.to_string(),
        // An empty `iCalUID` is no id at all, and must not make every event
        // that has one "the same meeting" as every other.
        ical_uid: raw.i_cal_uid.clone().filter(|uid| !uid.trim().is_empty()),
        title: raw
            .summary
            .clone()
            .unwrap_or_else(|| "(no title)".to_string()),
        start,
        end,
        all_day,
        location: raw.location.clone(),
        attendees,
        html_link: raw.html_link.clone().unwrap_or_default(),
        updated: raw.updated.clone().unwrap_or_default(),
    })
}

/// A Google event boundary is either `dateTime` (an instant) or `date` (a
/// bare calendar day, meaning an all-day event). Handling only the first
/// silently drops every all-day event, deadlines included.
fn parse_point(point: &RawPoint) -> Option<(DateTime<Utc>, bool)> {
    if let Some(dt) = &point.date_time {
        return DateTime::parse_from_rfc3339(dt)
            .ok()
            .map(|d| (d.with_timezone(&Utc), false));
    }
    let date = point.date.as_ref()?;
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some((naive.and_hms_opt(0, 0, 0)?.and_utc(), true))
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

/// Whether two events are one meeting seen twice rather than two meetings.
///
/// Google gives every event an `iCalUID` that is stable *across* calendars:
/// an invitation accepted in both the `work` and the `private` account
/// becomes two events with two different `id`s, in two different calendars,
/// carrying one `iCalUID`. Merging both accounts into one conflict scan (see
/// the module docs) makes that pair overlap 100%, so without this check the
/// owner's own dual-accepted meetings would each be reported as a permanent
/// cross-account clash — every poll, forever, with a stable id that dedup
/// will never retire. That is the fastest way to teach someone to ignore the
/// one signal this connector exists to produce.
///
/// Two events with **no** `iCalUID` are *not* the same meeting. Treating a
/// missing id as a match would collapse every id-less event into one and
/// silence real clashes, which is the strictly worse failure: a false
/// conflict is noise, a missing conflict is a meeting walked into.
fn same_meeting(a: &CalEvent, b: &CalEvent) -> bool {
    match (&a.ical_uid, &b.ical_uid) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Every pair of timed events that overlap, across every account represented
/// in `events`.
///
/// All-day events are excluded before the scan: an all-day event overlaps
/// every timed event of that day by construction, and including it would
/// drown the one signal this function exists to produce. Back-to-back
/// meetings (`b.start >= a.end`) are not a conflict.
///
/// Sorts the timed events by start and, for each one, scans forward only
/// until the next one starts at or after the first one ends. That is correct
/// because the list is sorted (every event after that point starts even
/// later, so none of them can overlap either) and keeps the scan linear in
/// practice rather than quadratic.
pub fn find_conflicts(events: &[CalEvent]) -> Vec<(CalEvent, CalEvent)> {
    let mut timed: Vec<&CalEvent> = events.iter().filter(|e| !e.all_day).collect();
    timed.sort_by_key(|e| e.start);

    let mut conflicts = Vec::new();
    for i in 0..timed.len() {
        let a = timed[i];
        for b in &timed[i + 1..] {
            if b.start >= a.end {
                break;
            }
            // One meeting the owner accepted twice is not a clash with
            // itself; see `same_meeting`.
            if same_meeting(a, b) {
                continue;
            }
            conflicts.push((a.clone(), (*b).clone()));
        }
    }
    conflicts
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A Google Calendar client bound to one base URL, shared across accounts.
/// The per-account access token is fetched fresh from [`Auth`] on every call,
/// never cached here.
pub struct CalendarClient {
    http: reqwest::Client,
    /// Always ends in `/`, so [`Url::join`] appends rather than replaces the
    /// last path segment.
    base: Url,
}

impl fmt::Debug for CalendarClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nothing secret here: the base URL is Google's public API host (or a
        // test server's loopback address), never a credential.
        f.debug_struct("CalendarClient")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl CalendarClient {
    /// Fallible for the same two reasons as `CanvasClient::new`: the base URL
    /// may not be a URL, and building the `reqwest` client can fail if TLS
    /// will not start.
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/"))
            .with_context(|| format!("Google Calendar base URL {base_url:?} is not a URL"))?;

        if !matches!(base.scheme(), "http" | "https") {
            bail!(
                "Google Calendar base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            );
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            bail!("Google Calendar base URL must not contain a username or password");
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // No redirects, ever: a same-host https->http downgrade would
            // otherwise carry the bearer access token onto the wire in clear
            // text, and this API has no legitimate reason to redirect a GET.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTPS client for Google Calendar")?;

        Ok(Self { http, base })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// Every event on `account`'s primary calendar with any overlap with
    /// `[from, to)`, normalised, paginating until Google stops returning a
    /// `nextPageToken`.
    pub async fn list_events(
        &self,
        auth: &Auth,
        account: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<CalEvent>> {
        let mut token = auth.access_token(account).await?;

        let mut events = Vec::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let url = self.events_url(from, to, page_token.as_deref())?;
            let (raw_items, next) = self.get_page(auth, account, url, &mut token).await?;
            events.extend(raw_items.iter().filter_map(|raw| normalize(raw, account)));

            match next {
                None => return Ok(events),
                Some(next_token) => page_token = Some(next_token),
            }
        }

        bail!(
            "google calendar: {} kept offering another page after {MAX_PAGES} of them \
             for account {account:?}; refusing to follow nextPageToken further",
            self.base
        )
    }

    fn events_url(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        page_token: Option<&str>,
    ) -> anyhow::Result<Url> {
        let mut url = self
            .base
            .join("calendars/primary/events")
            .context("building the Google Calendar events URL")?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("timeMin", &from.to_rfc3339());
            pairs.append_pair("timeMax", &to.to_rfc3339());
            pairs.append_pair("singleEvents", "true");
            pairs.append_pair("orderBy", "startTime");
            pairs.append_pair("maxResults", PAGE_SIZE);
            if let Some(token) = page_token {
                pairs.append_pair("pageToken", token);
            }
        }
        Ok(url)
    }

    /// One `GET`, returning the decoded page's items and its `nextPageToken`.
    ///
    /// A 401 is retried **once**, against a token forced out of [`Auth`]
    /// rather than waited for. `token` is updated in place so the remaining
    /// pages of the same walk use the new one rather than each rediscovering
    /// the 401 for themselves.
    async fn get_page(
        &self,
        auth: &Auth,
        account: &str,
        url: Url,
        token: &mut String,
    ) -> anyhow::Result<(Vec<RawEvent>, Option<String>)> {
        // `whence` is the URL with its query stripped: enough to say which
        // endpoint failed (and which account's token was used to ask, via the
        // caller's own context), short enough for a log line, and — since the
        // token never travels in the query string — never a credential.
        let mut whence = url.clone();
        whence.set_query(None);

        let response = self.send(url.clone(), token, &whence).await?;

        // Exactly one retry. The access token can be dead long before it
        // expires — a password change or a session revoke kills every
        // outstanding one — and without this the connector reports 401s for
        // the rest of the hour. A *second* 401 means the fresh token is
        // refused too, so retrying again would only hammer Google; the error
        // below is what the reader gets instead.
        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            *token = auth
                .refresh_after_unauthorized(account, token)
                .await
                .with_context(|| {
                    format!(
                        "google calendar: GET {whence} was refused with HTTP 401 and the \
                         forced token refresh for account {account:?} failed too"
                    )
                })?;
            self.send(url, token, &whence).await?
        } else {
            response
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response
            .text()
            .await
            .map_err(|err| err.without_url())
            .with_context(|| format!("google calendar: reading the reply to GET {whence}"))?;

        if !status.is_success() {
            bail!(
                "google calendar: GET {whence} returned HTTP {status}{}. {}",
                canonical_reason(status),
                explain(status, &body)
            );
        }

        // A 200 that is not JSON is the failure mode that hurts: an expired
        // or wrongly scoped token can get a login redirect or a consent page
        // back with a cheerful 200. Without this check that HTML would either
        // fail to deserialise with an opaque message or, worse, silently
        // yield zero events.
        if !is_json(&content_type) {
            bail!(
                "google calendar: GET {whence} answered HTTP {status} with content-type \
                 {content_type:?}, which is not JSON. Check that the account's token is \
                 still valid and carries the calendar scope. Body began: {}",
                snippet(&body)
            );
        }

        let page: EventsPage = serde_json::from_str(&body).with_context(|| {
            format!(
                "google calendar: GET {whence} answered JSON this build does not understand. \
                 Body began: {}",
                snippet(&body)
            )
        })?;

        Ok((page.items, page.next_page_token))
    }

    /// One bearer-authenticated GET, with the URL kept out of the error.
    async fn send(&self, url: Url, token: &str, whence: &Url) -> anyhow::Result<reqwest::Response> {
        self.http
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| err.without_url())
            .with_context(|| format!("google calendar: GET {whence} failed"))
    }
}

/// `list_events(auth, account, from, to)` against the real Google Calendar
/// API. The testable seam is [`CalendarClient::new`], which takes the base
/// URL as a parameter.
pub async fn list_events(
    auth: &Auth,
    account: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> anyhow::Result<Vec<CalEvent>> {
    CalendarClient::new(GOOGLE_CALENDAR_BASE)?
        .list_events(auth, account, from, to)
        .await
}

fn is_json(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

fn canonical_reason(status: reqwest::StatusCode) -> String {
    status
        .canonical_reason()
        .map(|reason| format!(" {reason}"))
        .unwrap_or_default()
}

/// Turn a status into the sentence that tells the reader what to do about it.
fn explain(status: reqwest::StatusCode, body: &str) -> String {
    let hint = match status.as_u16() {
        401 => {
            "The access token was rejected: it has expired or been revoked. If this \
             persists, re-authorise the account with ea-google-authorize. "
        }
        403 => {
            "Google refused this request. The token may lack the calendar scope, or the \
             account may be rate limited. "
        }
        404 => "No such calendar, or the token's owner cannot see it. ",
        _ => "",
    };
    format!("{hint}Body began: {}", snippet(body))
}

fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= BODY_SNIPPET {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(BODY_SNIPPET).collect();
    format!("{head}… ({} chars total)", trimmed.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::{RefreshBackend, RefreshError, RefreshResponse, TokenStore, Tokens};

    // -----------------------------------------------------------------------
    // Normalisation
    // -----------------------------------------------------------------------

    fn point_datetime(s: &str) -> RawPoint {
        RawPoint {
            date_time: Some(s.to_string()),
            date: None,
        }
    }

    fn point_date(s: &str) -> RawPoint {
        RawPoint {
            date_time: None,
            date: Some(s.to_string()),
        }
    }

    fn timed_event() -> RawEvent {
        RawEvent {
            id: Some("evt-1".to_string()),
            i_cal_uid: Some("evt-1@google.com".to_string()),
            status: None,
            summary: Some("Advisor meeting".to_string()),
            start: Some(point_datetime("2026-10-01T14:00:00+02:00")),
            end: Some(point_datetime("2026-10-01T15:00:00+02:00")),
            location: Some("Room 4".to_string()),
            attendees: Some(vec![RawAttendee {
                email: Some("advisor@kth.se".to_string()),
            }]),
            html_link: Some("https://calendar.google.com/event?eid=1".to_string()),
            updated: Some("2026-09-20T10:00:00Z".to_string()),
        }
    }

    #[test]
    fn a_timed_event_maps_and_converts_its_offset_to_utc() {
        let event = normalize(&timed_event(), "work").expect("a plain timed event normalises");

        assert_eq!(event.id, "evt-1");
        assert_eq!(event.account, "work");
        assert_eq!(event.title, "Advisor meeting");
        assert!(!event.all_day);
        // +02:00 -> UTC is two hours earlier.
        assert_eq!(event.start, at("2026-10-01T12:00:00Z"));
        assert_eq!(event.end, at("2026-10-01T13:00:00Z"));
        assert_eq!(event.location.as_deref(), Some("Room 4"));
        assert_eq!(event.attendees, vec!["advisor@kth.se".to_string()]);
        assert_eq!(event.html_link, "https://calendar.google.com/event?eid=1");
        assert_eq!(event.updated, "2026-09-20T10:00:00Z");
    }

    /// Review Focus #3: an all-day event carries `start.date`, not
    /// `start.dateTime`. Handling only `dateTime` would `?`-return `None`
    /// here and silently drop every exam and deadline marked all-day.
    #[test]
    fn an_all_day_event_normalises_with_all_day_true_and_valid_timestamps() {
        let raw = RawEvent {
            id: Some("evt-2".to_string()),
            summary: Some("Exam: XX1001".to_string()),
            start: Some(point_date("2026-10-15")),
            end: Some(point_date("2026-10-16")),
            ..Default::default()
        };

        let event = normalize(&raw, "work").expect("an all-day event must not be dropped");

        assert!(event.all_day);
        assert_eq!(event.start, at("2026-10-15T00:00:00Z"));
        assert_eq!(event.end, at("2026-10-16T00:00:00Z"));
    }

    #[test]
    fn a_missing_summary_becomes_no_title() {
        let raw = RawEvent {
            id: Some("evt-3".to_string()),
            summary: None,
            start: Some(point_datetime("2026-10-01T14:00:00Z")),
            end: Some(point_datetime("2026-10-01T15:00:00Z")),
            ..Default::default()
        };

        let event = normalize(&raw, "work").unwrap();
        assert_eq!(event.title, "(no title)");
    }

    #[test]
    fn a_cancelled_event_is_dropped() {
        let raw = RawEvent {
            id: Some("evt-4".to_string()),
            status: Some("cancelled".to_string()),
            start: Some(point_datetime("2026-10-01T14:00:00Z")),
            end: Some(point_datetime("2026-10-01T15:00:00Z")),
            ..Default::default()
        };

        assert!(normalize(&raw, "work").is_none());
    }

    #[test]
    fn an_event_with_neither_date_nor_datetime_is_dropped_not_panicked() {
        let raw = RawEvent {
            id: Some("evt-5".to_string()),
            start: Some(RawPoint {
                date_time: None,
                date: None,
            }),
            end: Some(point_datetime("2026-10-01T15:00:00Z")),
            ..Default::default()
        };

        assert!(normalize(&raw, "work").is_none());
    }

    // -----------------------------------------------------------------------
    // Conflict detection
    // -----------------------------------------------------------------------

    fn timed(account: &str, id: &str, start: &str, end: &str) -> CalEvent {
        CalEvent {
            id: id.to_string(),
            account: account.to_string(),
            ical_uid: None,
            title: id.to_string(),
            start: at(start),
            end: at(end),
            all_day: false,
            location: None,
            attendees: Vec::new(),
            html_link: String::new(),
            updated: String::new(),
        }
    }

    fn all_day(account: &str, id: &str, start: &str, end: &str) -> CalEvent {
        let mut e = timed(account, id, start, end);
        e.all_day = true;
        e
    }

    fn with_uid(mut event: CalEvent, uid: &str) -> CalEvent {
        event.ical_uid = Some(uid.to_string());
        event
    }

    #[test]
    fn a_straightforward_overlap_is_found() {
        let a = timed("work", "a", "2026-10-01T14:00:00Z", "2026-10-01T15:00:00Z");
        let b = timed("work", "b", "2026-10-01T14:30:00Z", "2026-10-01T15:30:00Z");

        let conflicts = find_conflicts(&[a.clone(), b.clone()]);
        assert_eq!(conflicts, vec![(a, b)]);
    }

    #[test]
    fn back_to_back_meetings_are_not_a_conflict() {
        let a = timed("work", "a", "2026-10-01T14:00:00Z", "2026-10-01T15:00:00Z");
        let b = timed("work", "b", "2026-10-01T15:00:00Z", "2026-10-01T16:00:00Z");

        assert_eq!(find_conflicts(&[a, b]), Vec::new());
    }

    #[test]
    fn all_day_events_are_excluded_even_though_they_overlap_everything() {
        let day = all_day(
            "work",
            "exam",
            "2026-10-01T00:00:00Z",
            "2026-10-02T00:00:00Z",
        );
        let meeting = timed("work", "m", "2026-10-01T14:00:00Z", "2026-10-01T15:00:00Z");

        assert_eq!(
            find_conflicts(&[day, meeting]),
            Vec::new(),
            "an all-day event must not drown the signal by conflicting with everything"
        );
    }

    /// The single most valuable case: a `work` meeting booked over a
    /// `private` commitment is invisible to either calendar alone. This is
    /// the entire reason both accounts are polled into one list.
    #[test]
    fn a_conflict_spanning_work_and_private_accounts_is_found() {
        let work = timed(
            "work",
            "standup",
            "2026-10-01T14:00:00Z",
            "2026-10-01T15:00:00Z",
        );
        let private = timed(
            "private",
            "dentist",
            "2026-10-01T14:30:00Z",
            "2026-10-01T15:30:00Z",
        );

        let conflicts = find_conflicts(&[work.clone(), private.clone()]);
        assert_eq!(conflicts, vec![(work, private)]);
    }

    /// The false positive that would have cost this connector its most
    /// valuable signal: one invitation accepted in *both* accounts is two
    /// events overlapping 100%, and reporting it as a cross-account clash
    /// every two minutes forever is how an owner learns to ignore conflicts.
    #[test]
    fn one_meeting_accepted_in_both_accounts_is_not_a_conflict_with_itself() {
        let work = with_uid(
            timed(
                "work",
                "work-copy",
                "2026-10-01T14:00:00Z",
                "2026-10-01T15:00:00Z",
            ),
            "abc123@google.com",
        );
        let private = with_uid(
            timed(
                "private",
                "private-copy",
                "2026-10-01T14:00:00Z",
                "2026-10-01T15:00:00Z",
            ),
            "abc123@google.com",
        );

        assert_eq!(
            find_conflicts(&[work, private]),
            Vec::new(),
            "the same meeting in two calendars is one meeting, not a clash"
        );
    }

    /// And the check must not have bought that by silencing the real thing.
    #[test]
    fn two_different_meetings_overlapping_across_accounts_are_still_a_conflict() {
        let work = with_uid(
            timed(
                "work",
                "standup",
                "2026-10-01T14:00:00Z",
                "2026-10-01T15:00:00Z",
            ),
            "standup@google.com",
        );
        let private = with_uid(
            timed(
                "private",
                "dentist",
                "2026-10-01T14:30:00Z",
                "2026-10-01T15:30:00Z",
            ),
            "dentist@google.com",
        );

        assert_eq!(
            find_conflicts(&[work.clone(), private.clone()]),
            vec![(work, private)]
        );
    }

    /// A missing id is not a shared id. Collapsing every event Google sent no
    /// `iCalUID` for into one "meeting" would silence real clashes, which is
    /// strictly worse than the noise this check exists to remove.
    #[test]
    fn events_without_an_ical_uid_are_not_collapsed_into_one_meeting() {
        let a = timed("work", "a", "2026-10-01T14:00:00Z", "2026-10-01T15:00:00Z");
        let b = timed(
            "private",
            "b",
            "2026-10-01T14:30:00Z",
            "2026-10-01T15:30:00Z",
        );
        assert_eq!(a.ical_uid, None);

        assert_eq!(find_conflicts(&[a.clone(), b.clone()]), vec![(a, b)]);
    }

    /// An empty string is a missing id, not a shared one — otherwise every
    /// event Google answers with `"iCalUID": ""` matches every other.
    #[test]
    fn an_empty_ical_uid_is_treated_as_absent() {
        let raw = RawEvent {
            id: Some("evt-1".to_string()),
            i_cal_uid: Some("   ".to_string()),
            start: Some(point_datetime("2026-10-01T14:00:00Z")),
            end: Some(point_datetime("2026-10-01T15:00:00Z")),
            ..Default::default()
        };

        assert_eq!(normalize(&raw, "work").unwrap().ical_uid, None);
    }

    #[test]
    fn normalize_carries_the_ical_uid_through() {
        let raw = RawEvent {
            id: Some("evt-1".to_string()),
            i_cal_uid: Some("abc123@google.com".to_string()),
            start: Some(point_datetime("2026-10-01T14:00:00Z")),
            end: Some(point_datetime("2026-10-01T15:00:00Z")),
            ..Default::default()
        };

        assert_eq!(
            normalize(&raw, "work").unwrap().ical_uid.as_deref(),
            Some("abc123@google.com")
        );
    }

    #[test]
    fn a_single_event_yields_no_conflicts() {
        let a = timed("work", "a", "2026-10-01T14:00:00Z", "2026-10-01T15:00:00Z");
        assert_eq!(find_conflicts(&[a]), Vec::new());
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a timestamp")
    }

    // -----------------------------------------------------------------------
    // The HTTP client
    // -----------------------------------------------------------------------

    const ACCESS_TOKEN: &str = "ya29.CALENDAR-ACCESS-do-not-leak";

    /// A backend that must never be called: every test below writes a
    /// healthy, unexpired token straight into the store, so `list_events`
    /// has no reason to refresh.
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
            Box::pin(async { panic!("list_events must not need to refresh a healthy token") })
        }
    }

    /// The access token a forced refresh hands back. Distinct from
    /// [`ACCESS_TOKEN`] so a mock can tell the retry from the first attempt
    /// by its `Authorization` header alone.
    const RENEWED_TOKEN: &str = "ya29.CALENDAR-RENEWED-do-not-leak";

    /// A backend that renews once per call and counts how often it was asked.
    struct RenewingBackend {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RenewingBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RefreshBackend for RenewingBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(RefreshResponse {
                    access_token: RENEWED_TOKEN.to_string(),
                    expires_in: 3599,
                    refresh_token: None,
                    scope: None,
                })
            })
        }
    }

    /// The weekly case: Google has forgotten the grant entirely.
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

    fn auth_with_healthy_token(dir: &std::path::Path, account: &str) -> Auth {
        auth_with_backend(dir, account, Arc::new(UnusedBackend))
    }

    fn auth_with_backend(
        dir: &std::path::Path,
        account: &str,
        backend: Arc<dyn RefreshBackend>,
    ) -> Auth {
        let store = TokenStore::new(Some(dir.to_path_buf()));
        store
            .write(
                account,
                &Tokens {
                    access_token: ACCESS_TOKEN.to_string(),
                    refresh_token: "1//not-used-here".to_string(),
                    expiry: Utc::now() + chrono::Duration::hours(1),
                    scope: "https://www.googleapis.com/auth/calendar.readonly".to_string(),
                },
            )
            .unwrap();
        Auth::with_backend(store, backend)
    }

    fn json_page(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    #[tokio::test]
    async fn list_events_fetches_and_normalises_a_single_page() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(query_param("singleEvents", "true"))
            .respond_with(json_page(serde_json::json!({
                "items": [{
                    "id": "evt-1",
                    "summary": "Advisor meeting",
                    "start": { "dateTime": "2026-10-01T14:00:00+02:00" },
                    "end": { "dateTime": "2026-10-01T15:00:00+02:00" },
                }],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = CalendarClient::new(&server.uri()).unwrap();
        let events = client
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, "work");
        assert_eq!(events[0].title, "Advisor meeting");
    }

    #[tokio::test]
    async fn list_events_sends_the_bearer_token_and_never_in_a_query_string() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {ACCESS_TOKEN}").as_str(),
            ))
            .respond_with(json_page(serde_json::json!({ "items": [] })))
            .expect(1)
            .mount(&server)
            .await;

        CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].url.query().unwrap_or("").contains(ACCESS_TOKEN),
            "the access token must never appear in the URL"
        );
    }

    #[tokio::test]
    async fn list_events_paginates_across_two_pages() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .and(query_param("pageToken", "page-2"))
            .respond_with(json_page(serde_json::json!({
                "items": [{
                    "id": "evt-2",
                    "summary": "Second page",
                    "start": { "dateTime": "2026-10-02T14:00:00Z" },
                    "end": { "dateTime": "2026-10-02T15:00:00Z" },
                }],
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(json_page(serde_json::json!({
                "items": [{
                    "id": "evt-1",
                    "summary": "First page",
                    "start": { "dateTime": "2026-10-01T14:00:00Z" },
                    "end": { "dateTime": "2026-10-01T15:00:00Z" },
                }],
                "nextPageToken": "page-2",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let events = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .unwrap();

        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["evt-1", "evt-2"],
            "both pages must be collected in order"
        );
    }

    #[tokio::test]
    async fn an_endless_page_token_chain_stops_at_the_cap() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(json_page(
                serde_json::json!({ "items": [], "nextPageToken": "loop" }),
            ))
            .mount(&server)
            .await;

        let err = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("an endless chain must stop");

        let text = format!("{err:#}");
        assert!(text.contains("50"), "{text}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            MAX_PAGES,
            "the cap must bound the number of requests"
        );
    }

    /// An access token can be dead long before it expires: a password change
    /// or a session revoke invalidates every outstanding one at once. Without
    /// a forced refresh the connector reports 401s for up to an hour, which
    /// on a two-minute poll is thirty consecutive failed polls.
    #[tokio::test]
    async fn a_401_forces_one_refresh_and_retries_the_request_with_the_new_token() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = RenewingBackend::new();
        let auth = auth_with_backend(
            tmp.path(),
            "work",
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        );

        // The stale token is refused; the renewed one is served. Matching on
        // the header is what proves the retry carried the *new* token.
        Mock::given(method("GET"))
            .and(header(
                "authorization",
                format!("Bearer {ACCESS_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(header(
                "authorization",
                format!("Bearer {RENEWED_TOKEN}").as_str(),
            ))
            .respond_with(json_page(serde_json::json!({
                "items": [{
                    "id": "e1",
                    "summary": "Lecture",
                    "start": { "dateTime": "2026-10-01T10:00:00Z" },
                    "end": { "dateTime": "2026-10-01T12:00:00Z" },
                }],
            })))
            .mount(&server)
            .await;

        let events = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect("the retry after the forced refresh must succeed");

        assert_eq!(events.len(), 1, "{events:#?}");
        assert_eq!(backend.calls(), 1, "exactly one forced refresh");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the original request and exactly one retry"
        );
    }

    /// One retry, not a loop. A token Google refuses twice will be refused a
    /// third time; Phase 1's Fortnox notes record what an unbounded retry
    /// against a revoked grant costs.
    #[tokio::test]
    async fn a_second_401_after_the_forced_refresh_gives_up_rather_than_retrying_again() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = RenewingBackend::new();
        let auth = auth_with_backend(
            tmp.path(),
            "work",
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        );

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("a token refused twice is an error");

        assert!(format!("{err:#}").contains("401"), "{err:#}");
        assert_eq!(backend.calls(), 1, "exactly one forced refresh, not a loop");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one retry only"
        );
    }

    #[tokio::test]
    async fn a_401_errors_naming_the_status_without_leaking_the_token() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        // A 401 now forces a refresh, so this account's grant has to be dead
        // for the call to fail at all — which is the realistic pairing.
        let auth = auth_with_backend(tmp.path(), "work", Arc::new(RevokedBackend));

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("a 401 must be an error");

        let text = format!("{err:#}");
        assert!(text.contains("401"), "{text}");
        assert!(
            !text.contains(ACCESS_TOKEN),
            "the error leaked the token: {text}"
        );
    }

    #[tokio::test]
    async fn an_html_200_errors_instead_of_deserialising_into_nonsense() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<!doctype html><html><body>Sign in to continue</body></html>",
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;

        let err = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("an HTML body must not be accepted as an empty page");

        let text = format!("{err:#}");
        assert!(text.contains("text/html"), "{text}");
        assert!(text.contains("not JSON"), "{text}");
        assert!(
            !text.contains(ACCESS_TOKEN),
            "the error leaked the token: {text}"
        );
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_and_the_token_is_not_sent_to_the_target() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .respond_with(json_page(serde_json::json!({ "items": [] })))
            .expect(0) // a followed redirect would land here with the token
            .mount(&elsewhere)
            .await;

        Mock::given(method("GET"))
            .and(path("/calendars/primary/events"))
            .respond_with(ResponseTemplate::new(301).insert_header(
                "location",
                format!("{}/calendars/primary/events", elsewhere.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        let err = CalendarClient::new(&server.uri())
            .unwrap()
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("a 301 must not be followed transparently");

        let text = format!("{err:#}");
        assert!(text.contains("301"), "{text}");
        assert!(
            !text.contains(ACCESS_TOKEN),
            "the error leaked the token: {text}"
        );
    }

    #[tokio::test]
    async fn a_connection_failure_errors_without_leaking_the_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");
        // A port nothing is listening on.
        let client = CalendarClient::new("http://127.0.0.1:1").unwrap();

        let err = client
            .list_events(
                &auth,
                "work",
                at("2026-09-01T00:00:00Z"),
                at("2026-11-01T00:00:00Z"),
            )
            .await
            .expect_err("no listener");

        assert!(
            !format!("{err:#}").contains(ACCESS_TOKEN),
            "the error leaked the token"
        );
    }

    #[test]
    fn calendar_client_debug_never_prints_a_credential() {
        // CalendarClient holds no credential, but the module's standard is
        // that every Debug impl in it is checked, not assumed.
        let client = CalendarClient::new("https://www.googleapis.com").unwrap();
        let text = format!("{client:?}");
        assert!(!text.contains(ACCESS_TOKEN));
    }

    #[test]
    fn a_base_url_with_userinfo_is_refused() {
        let err = CalendarClient::new("https://someone:secret@example.test")
            .expect_err("userinfo must be refused");
        assert!(
            format!("{err:#}").contains("username or password"),
            "{err:#}"
        );
    }
}
