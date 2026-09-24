//! The Gmail client.
//!
//! Reads recent mail from a single account's inbox and can create — never
//! send — drafts. Read-mostly by construction: every method here is a `GET`
//! except [`create_draft`], and `SCOPES` in [`crate::auth`] deliberately
//! excludes `gmail.send`, so even a bug that tried to send mail would be
//! refused by Google at runtime.
//!
//! # The case that matters most: HTML-only mail
//!
//! Most newsletters, most automated notifications, and much of what a bank or
//! a university sends have no `text/plain` part at all — only
//! `text/html`. [`extract_body`] walks the MIME part tree once looking for
//! `text/plain`; if it finds none, it walks again for `text/html` and strips
//! the markup down to text. An implementation that only reads `text/plain`
//! silently returns an empty body for exactly the mail a triage session is
//! most likely to see, and an empty body reads as noise and gets discarded —
//! see the `an_html_only_message_falls_back_to_stripped_html` test.
//!
//! The stripper is [`ea_core::html::strip_html`], shared with the KTH mail
//! connector. It used to be a private copy here, and the copy in `ea_kth`
//! — taken from this one — fixed four things this one never got:
//! `&nbsp;` was missing from its five-entity `replace` chain (so a literal
//! `&nbsp;` reached the triage prompt, and Gmail carries plenty of
//! Outlook-authored mail), no numeric entity was decoded, `<style>` and
//! `<script>` *contents* survived the tags being dropped, and a chain of
//! `replace` calls double-decoded an already-escaped `&amp;lt;` into markup.
//! Both connectors feed the same prompt through the same 2000-character cap,
//! so one copy running with the other's bugs was a live defect rather than a
//! difference of taste. See that module for the contract, which is
//! deliberately not that of a real HTML parser.
//!
//! # Header handling
//!
//! Gmail is inconsistent about header name casing (`From` vs `from` vs
//! `FROM` depending on the sending server), so [`normalize`] looks headers up
//! case-insensitively. A missing `From` or `Subject` becomes
//! `(unknown sender)` / `(no subject)` — a blank string in either field reads
//! as a bug downstream, not as "this mail had no subject."
//!
//! # The one write in this connector
//!
//! [`create_draft`] posts to `users/me/drafts` and must never touch
//! `users/me/messages/send`. `create_draft_never_touches_the_send_endpoint`
//! pins this by asserting the request path contains `drafts` and not `send`.
//!
//! `to` and `subject` arrive from an LLM's `propose_action` tool call; the
//! human in the loop approves a one-line preview in Telegram, never the raw
//! RFC 822 headers this module writes into the draft. Two consequences:
//!
//! * [`build_rfc822`] **rejects** (never silently strips) a `to` or
//!   `subject` containing `\r`, `\n`, or a NUL byte, before any network
//!   call is made — a `\r\n` in either field would otherwise inject an
//!   attacker-chosen header (a hidden `Bcc:`, say) into a message the
//!   owner may send unread.
//! * a non-ASCII `to` or `subject` (unremarkable for a Swedish company —
//!   `Räksmörgås` is the common case, not an edge case) is RFC
//!   2047-encoded as one or more `=?UTF-8?B?...?=` words, each within the
//!   75-character-per-word limit and folded with `\r\n ` between words,
//!   with `MIME-Version: 1.0` declared accordingly. A pure-ASCII value is
//!   left untouched.
//!
//! # The list-then-get cost, and why `format=metadata` was not used
//!
//! `messages.list` returns only `{id, threadId}` pairs; the body, headers and
//! labels of each message require a separate `messages.get` call. For a
//! 25-message poll that is 1 + 25 = 26 requests. Gmail's `format=metadata`
//! parameter would shrink each `get` (fewer bytes, no attachment payloads),
//! but it also omits the part tree's `body.data` for anything not requested
//! as a metadata header — which would make [`extract_body`] permanently
//! return an empty string, defeating the one requirement (Review Focus #4)
//! this module exists to satisfy. So: `format=full` is used for every `get`,
//! the *count* of requests is bounded instead by capping `max` at
//! [`MAX_MESSAGES`] (Gmail's own documented page-size ceiling), and the cost
//! is documented here rather than hidden. A caller that wants cheaper polling
//! at the price of body text should poll less often or ask for fewer
//! messages, not silently lose bodies.
//!
//! # Why the `get`s run [`GET_CONCURRENCY`] at a time
//!
//! Those 26 requests used to be 26 *round trips*, one after another. The
//! daemon polls this connector every two minutes across two accounts, so the
//! sequential shape spent the whole poll budget on latency: 54 round trips
//! for a two-account poll, which on a slow link is a poll that cannot finish
//! before the next one is due. The `get`s are independent — nothing in one
//! affects another — so they run eight in flight, turning 25 round trips per
//! account into four waves.
//!
//! Eight, not "all of them": a poll must not be able to open 500 sockets at
//! once (`max` is clamped to [`MAX_MESSAGES`], not to 25), and Gmail bills
//! per-user quota per second — `messages.get` costs 5 units against a 250
//! unit/second/user ceiling, so 50 concurrent gets would be at the limit
//! while 8 is comfortably inside it. The wall-clock saving is mostly won by
//! the first few anyway: 25 requests at 8 wide is four waves, at 16 wide it
//! is two, and the second halving buys far less than it risks.
//!
//! The results stay in `messages.list` order — `buffered`, not
//! `buffer_unordered` — because that order is Gmail's recency order and the
//! watch payloads are read in it.
//!
//! # HTTP hardening
//!
//! Identical to [`crate::calendar`]: redirects disabled
//! (`redirect::Policy::none()`), a JSON content-type check before
//! deserialising, `.without_url()` on every `reqwest` error, a hand-written
//! `Debug` printing only the base URL, and a base-URL constructor that
//! rejects userinfo and non-http(s) schemes. `list_recent` does not paginate
//! past its own `max` cap, so there is no `nextPageToken` loop to bound here
//! the way Calendar's `MAX_PAGES` bounds one.

use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use ea_core::html::strip_html;
use futures::stream::{StreamExt, TryStreamExt};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::Auth;

/// Google's Gmail API v1 base. Overridable at the [`GmailClient`] level, same
/// reason as `CalendarClient`: tests point it at a `wiremock` server.
pub const GOOGLE_GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/";

/// Gmail's own documented ceiling on `messages.list`'s `maxResults`. `max` is
/// clamped to this rather than trusted, so a caller cannot accidentally turn
/// one poll into an unbounded number of `messages.get` calls.
const MAX_MESSAGES: usize = 500;

/// How many `messages.get` requests one [`GmailClient::list_recent`] keeps in
/// flight at once. See the module docs for why eight.
pub const GET_CONCURRENCY: usize = 8;

/// Per-request deadline, same value and reasoning as Calendar and `auth.rs`.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an unexpected response body to quote back.
const BODY_SNIPPET: usize = 300;

// ---------------------------------------------------------------------------
// The normalised model
// ---------------------------------------------------------------------------

/// One message, normalised from Gmail's wire shape and tagged with the
/// account it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mail {
    pub id: String,
    pub thread_id: String,
    pub account: String,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
    pub labels: Vec<String>,
}

// ---------------------------------------------------------------------------
// The raw wire shape
// ---------------------------------------------------------------------------

/// A Gmail `messages.get` (`format=full`) response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "threadId")]
    thread_id: Option<String>,
    #[serde(default, rename = "labelIds")]
    label_ids: Option<Vec<String>>,
    #[serde(default)]
    snippet: Option<String>,
    /// Milliseconds since the epoch, as a *string* — Gmail's documented wire
    /// shape, not a number.
    #[serde(default, rename = "internalDate")]
    internal_date: Option<String>,
    #[serde(default)]
    payload: Option<Part>,
}

/// One node of a MIME part tree. A simple message has a single top-level
/// `Part` with a body; a multipart message nests further `Part`s inside
/// `parts`, arbitrarily deep — hence [`extract_body`] recurses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Part {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    /// Present and non-empty for an attachment part. Gmail's own convention
    /// for telling an inline body apart from a file: an attachment can share
    /// a `text/plain` MIME type with an ordinary body (e.g. a `.txt`
    /// attachment), so the MIME type alone cannot distinguish them.
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    headers: Option<Vec<Header>>,
    #[serde(default)]
    body: Option<PartBody>,
    #[serde(default)]
    parts: Option<Vec<Part>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PartBody {
    /// Base64url-encoded content, no padding, per Gmail's API.
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Header {
    name: String,
    value: String,
}

/// One page of `messages.list`. Only the id is used; the rest of each
/// message requires a `messages.get` call (see the module docs on cost).
#[derive(Debug, Default, Deserialize)]
struct MessagesPage {
    #[serde(default)]
    messages: Option<Vec<MessageRef>>,
}

#[derive(Debug, Deserialize)]
struct MessageRef {
    id: String,
}

/// A `drafts.create` response. Only the id is used.
#[derive(Debug, Default, Deserialize)]
struct DraftResponse {
    #[serde(default)]
    id: Option<String>,
}

// ---------------------------------------------------------------------------
// Body extraction
// ---------------------------------------------------------------------------

/// Pull the readable text out of a message's part tree.
///
/// Prefers `text/plain`; falls back to stripped `text/html` when no
/// `text/plain` part exists — see the module docs for why that fallback is
/// the single most important thing this function does. An empty or
/// attachment-only payload returns an empty string, never panics.
pub fn extract_body(payload: &Part) -> String {
    if let Some(part) = find_body_part(payload, "text/plain") {
        return decode_part_data(part);
    }
    if let Some(part) = find_body_part(payload, "text/html") {
        return strip_html(&decode_part_data(part));
    }
    String::new()
}

/// Depth-first search for the first non-attachment part whose MIME type
/// matches and which actually carries inline `body.data` — a container part
/// (e.g. `multipart/alternative`) has neither and exists only to hold
/// `parts`, so the search recurses into it rather than stopping there.
fn find_body_part<'a>(part: &'a Part, mime_type: &str) -> Option<&'a Part> {
    let matches = part
        .mime_type
        .as_deref()
        .map(|mt| mt.eq_ignore_ascii_case(mime_type))
        .unwrap_or(false);
    if matches && !is_attachment(part) && has_inline_data(part) {
        return Some(part);
    }
    for child in part.parts.as_deref().unwrap_or_default() {
        if let Some(found) = find_body_part(child, mime_type) {
            return Some(found);
        }
    }
    None
}

fn is_attachment(part: &Part) -> bool {
    part.filename
        .as_deref()
        .map(|f| !f.is_empty())
        .unwrap_or(false)
}

fn has_inline_data(part: &Part) -> bool {
    part.body
        .as_ref()
        .and_then(|b| b.data.as_deref())
        .map(|d| !d.is_empty())
        .unwrap_or(false)
}

fn decode_part_data(part: &Part) -> String {
    match part.body.as_ref().and_then(|b| b.data.as_deref()) {
        Some(data) => decode_base64url(data),
        None => String::new(),
    }
}

/// Gmail's `body.data` is base64url (`-`/`_` in place of `+`/`/`), and Google
/// omits the trailing `=` padding, though nothing guarantees a well-behaved
/// server never sends it — trimming stray `=` before decoding tolerates
/// both. A decode failure (corrupt data, not a Gmail-shaped payload) returns
/// an empty string rather than propagating an error out of a `pure`
/// function; the caller has no better body to report anyway.
fn decode_base64url(data: &str) -> String {
    let cleaned = data.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(cleaned)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Turn one raw message into a [`Mail`]. Always succeeds — an absent or
/// malformed field becomes a safe default (an empty string, an empty
/// `Vec`, `(unknown sender)`, `(no subject)`) rather than a dropped message,
/// because unlike a cancelled calendar event, there is no "this message does
/// not exist" case for mail that has already been listed by Gmail.
pub fn normalize(raw: &RawMessage, account: &str) -> Mail {
    let headers: &[Header] = raw
        .payload
        .as_ref()
        .and_then(|p| p.headers.as_deref())
        .unwrap_or(&[]);

    let from = header_value(headers, "From")
        .map(str::to_string)
        .unwrap_or_else(|| "(unknown sender)".to_string());
    let subject = header_value(headers, "Subject")
        .map(str::to_string)
        .unwrap_or_else(|| "(no subject)".to_string());

    let body = raw.payload.as_ref().map(extract_body).unwrap_or_default();

    // A message this poorly formed has never been observed from Gmail, but
    // `Mail::received_at` is not optional, so a message with no parseable
    // `internalDate` falls back to the Unix epoch rather than `Utc::now()`:
    // "now" would sort a corrupt message to the *top* of a triage queue
    // ordered by recency, displacing genuinely urgent mail, whereas the
    // epoch sinks it to the bottom where a malformed field belongs.
    let received_at = raw
        .internal_date
        .as_deref()
        .and_then(parse_internal_date)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);

    Mail {
        id: raw.id.clone().unwrap_or_default(),
        thread_id: raw.thread_id.clone().unwrap_or_default(),
        account: account.to_string(),
        from,
        subject,
        snippet: raw.snippet.clone().unwrap_or_default(),
        body,
        received_at,
        labels: raw.label_ids.clone().unwrap_or_default(),
    }
}

/// Case-insensitive header lookup: Gmail is inconsistent about whether a
/// header name arrives as `From`, `from`, or `FROM` depending on the sending
/// server, and a case-sensitive lookup here would silently miss a real
/// header and report `(unknown sender)` for mail that plainly has a sender.
fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// `internalDate` is documented as milliseconds since the epoch, encoded as
/// a JSON *string* (not a number) — deserialising it as an integer would
/// fail on every real Gmail response.
fn parse_internal_date(raw: &str) -> Option<DateTime<Utc>> {
    let millis: i64 = raw.trim().parse().ok()?;
    DateTime::from_timestamp_millis(millis)
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A Gmail client bound to one base URL, shared across accounts. The
/// per-account access token is fetched fresh from [`Auth`] on every call,
/// never cached here.
pub struct GmailClient {
    http: reqwest::Client,
    /// Always ends in `/`, so [`Url::join`] appends rather than replaces the
    /// last path segment.
    base: Url,
}

impl fmt::Debug for GmailClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nothing secret here: the base URL is Google's public API host (or
        // a test server's loopback address), never a credential.
        f.debug_struct("GmailClient")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl GmailClient {
    /// Fallible for the same two reasons as `CalendarClient::new`: the base
    /// URL may not be a URL, and building the `reqwest` client can fail if
    /// TLS will not start.
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/"))
            .with_context(|| format!("Gmail base URL {base_url:?} is not a URL"))?;

        if !matches!(base.scheme(), "http" | "https") {
            bail!(
                "Gmail base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            );
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            bail!("Gmail base URL must not contain a username or password");
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // No redirects, ever: a same-host https->http downgrade would
            // otherwise carry the bearer access token onto the wire in clear
            // text, and this API has no legitimate reason to redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTPS client for Gmail")?;

        Ok(Self { http, base })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// The `max` most recent messages matching `query` (Gmail search syntax,
    /// e.g. `"is:unread"`; pass `""` for no filter), normalised. See the
    /// module docs for the list-then-get cost this makes: one `list` request
    /// plus one `get` per message returned, the `get`s running
    /// [`GET_CONCURRENCY`] at a time and returned in list order.
    pub async fn list_recent(
        &self,
        auth: &Auth,
        account: &str,
        query: &str,
        max: usize,
    ) -> anyhow::Result<Vec<Mail>> {
        let token = auth.access_token(account).await?;
        let capped = max.min(MAX_MESSAGES);

        let ids = self
            .list_message_ids(auth, account, &token, query, capped)
            .await?;

        // `buffered`, so the output keeps `messages.list`'s recency order
        // however the responses interleave, and so a slow message delays only
        // the rows behind it rather than every one of them. The first error
        // short-circuits the rest, exactly as the sequential loop did.
        let token = &token;
        futures::stream::iter(ids)
            .map(|id| async move {
                let raw = self.get_message(auth, account, token, &id).await?;
                anyhow::Ok(normalize(&raw, account))
            })
            .buffered(GET_CONCURRENCY)
            .try_collect()
            .await
    }

    /// One message by id, normalised.
    ///
    /// `id` is validated before it is interpolated into a path. It reaches
    /// this function from a tool argument a language model wrote, and the URL
    /// is built with `Url::join` on a relative string: an id of `../../foo`
    /// would otherwise walk out of `users/me/messages/` and issue a
    /// bearer-authenticated GET against a path nobody asked for. Gmail's own
    /// ids are lower-case hex, so the character class below is generous
    /// rather than restrictive.
    pub async fn get(&self, auth: &Auth, account: &str, id: &str) -> anyhow::Result<Mail> {
        validate_message_id(id)?;
        let token = auth.access_token(account).await?;
        let raw = self.get_message(auth, account, &token, id).await?;
        Ok(normalize(&raw, account))
    }

    /// Create a draft addressed to `to` with `subject` and `body`, and
    /// return its id. Posts to `users/me/drafts`; never touches
    /// `users/me/messages/send` — see the module docs.
    pub async fn create_draft(
        &self,
        auth: &Auth,
        account: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> anyhow::Result<String> {
        // Validate and build the message before touching the network at
        // all — a header-injection attempt in `to`/`subject` must not even
        // trigger a token fetch, let alone the draft POST.
        let raw_message = build_rfc822(to, subject, body)?;
        let token = auth.access_token(account).await?;
        let encoded = URL_SAFE_NO_PAD.encode(raw_message.as_bytes());

        let url = self
            .base
            .join("users/me/drafts")
            .context("building the Gmail drafts URL")?;
        let mut whence = url.clone();
        whence.set_query(None);

        let payload = serde_json::json!({ "message": { "raw": encoded } });

        let response = self
            .send_with_retry(auth, account, &token, &whence, |bearer| {
                self.http
                    .post(url.clone())
                    .bearer_auth(bearer)
                    .header(reqwest::header::ACCEPT, "application/json")
                    .json(&payload)
            })
            .await?;

        let draft: DraftResponse = self.read_json(response, &whence).await?;
        Ok(draft.id.unwrap_or_default())
    }

    async fn list_message_ids(
        &self,
        auth: &Auth,
        account: &str,
        token: &str,
        query: &str,
        max: usize,
    ) -> anyhow::Result<Vec<String>> {
        let mut url = self
            .base
            .join("users/me/messages")
            .context("building the Gmail messages.list URL")?;
        {
            let mut pairs = url.query_pairs_mut();
            if !query.is_empty() {
                pairs.append_pair("q", query);
            }
            pairs.append_pair("maxResults", &max.to_string());
        }

        let page: MessagesPage = self.get_json(auth, account, token, url).await?;
        Ok(page
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    async fn get_message(
        &self,
        auth: &Auth,
        account: &str,
        token: &str,
        id: &str,
    ) -> anyhow::Result<RawMessage> {
        let mut url = self
            .base
            .join(&format!("users/me/messages/{id}"))
            .context("building the Gmail messages.get URL")?;
        url.query_pairs_mut().append_pair("format", "full");

        self.get_json(auth, account, token, url).await
    }

    /// One bearer-authenticated `GET`, decoded, with the 401 retry.
    async fn get_json<T: DeserializeOwned>(
        &self,
        auth: &Auth,
        account: &str,
        token: &str,
        url: Url,
    ) -> anyhow::Result<T> {
        let mut whence = url.clone();
        whence.set_query(None);

        let response = self
            .send_with_retry(auth, account, token, &whence, |bearer| {
                self.http
                    .get(url.clone())
                    .bearer_auth(bearer)
                    .header(reqwest::header::ACCEPT, "application/json")
            })
            .await?;

        self.read_json(response, &whence).await
    }

    /// Send a request, and if Google answers 401, force a token refresh and
    /// send it **once** more.
    ///
    /// The access token can be dead long before it expires — a password
    /// change or a session revoke invalidates every outstanding one — and
    /// [`Auth::access_token`] only refreshes on local expiry, so without this
    /// a revoked session produces 401s for up to an hour: thirty consecutive
    /// failed polls at the daemon's two-minute cadence.
    ///
    /// Exactly one retry. A token Google refuses twice will be refused a
    /// third time, and Phase 1's Fortnox notes record where an unbounded
    /// retry against a dead grant ends: rate limited, for the whole
    /// integration. [`Auth::refresh_after_unauthorized`] bounds the other
    /// half of it — the concurrent `messages.get`s that all see the same 401
    /// share one refresh rather than each buying their own.
    ///
    /// Re-sending is safe for everything this client does: a 401 means Google
    /// rejected the request before acting on it, so the retried `POST` cannot
    /// produce a second draft.
    async fn send_with_retry(
        &self,
        auth: &Auth,
        account: &str,
        token: &str,
        whence: &Url,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        let response = send(build(token), whence).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let fresh = auth
            .refresh_after_unauthorized(account, token)
            .await
            .with_context(|| {
                format!(
                    "gmail: {whence} was refused with HTTP 401 and the forced token \
                     refresh for account {account:?} failed too"
                )
            })?;

        send(build(&fresh), whence).await
    }

    /// Shared response handling for every call this client makes: read the
    /// body, refuse a non-2xx status naming it, refuse a non-JSON
    /// content-type before deserialising (Google, like Canvas, can answer a
    /// 200 with an HTML sign-in or consent page under the wrong conditions),
    /// then deserialise.
    async fn read_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        whence: &Url,
    ) -> anyhow::Result<T> {
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
            .with_context(|| format!("gmail: reading the reply from {whence}"))?;

        if !status.is_success() {
            bail!(
                "gmail: {whence} returned HTTP {status}{}. {}",
                canonical_reason(status),
                explain(status, &body)
            );
        }

        if !is_json(&content_type) {
            bail!(
                "gmail: {whence} answered HTTP {status} with content-type {content_type:?}, \
                 which is not JSON. Check that the account's token is still valid and carries \
                 the required scope. Body began: {}",
                snippet(&body)
            );
        }

        serde_json::from_str(&body).with_context(|| {
            format!(
                "gmail: {whence} answered JSON this build does not understand. Body began: {}",
                snippet(&body)
            )
        })
    }
}

/// Send one built request. `whence` is the URL with its query stripped: the
/// `reqwest` error's own URL is dropped first, because a URL in an error is
/// the one place a credential must never turn up.
async fn send(request: reqwest::RequestBuilder, whence: &Url) -> anyhow::Result<reqwest::Response> {
    request
        .send()
        .await
        .map_err(|err| err.without_url())
        .with_context(|| format!("gmail: the request to {whence} failed"))
}

/// Build a minimal RFC 822 message. Plain text only, UTF-8 body.
///
/// `to` and `subject` are untrusted: they are the arguments an LLM's
/// `propose_action` tool call supplies to [`create_draft`], and the human
/// approving that call sees a one-line preview, not these headers. So this
/// function refuses — rather than silently sanitising — a value containing
/// `\r`, `\n`, or NUL, since a silently altered recipient is its own hazard
/// and the caller needs to know the input was rejected, not guess that it
/// was quietly changed. A non-ASCII value is RFC 2047-encoded so it survives
/// a strict RFC 5322 parser instead of arriving as ambiguous raw UTF-8
/// bytes; see [`encode_header_value`].
fn build_rfc822(to: &str, subject: &str, body: &str) -> anyhow::Result<String> {
    reject_header_injection("to", to)?;
    reject_header_injection("subject", subject)?;

    let to_header = encode_header_value(to);
    let subject_header = encode_header_value(subject);

    Ok(format!(
        "MIME-Version: 1.0\r\nTo: {to_header}\r\nSubject: {subject_header}\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n{body}"
    ))
}

/// Refuse a header value that could inject additional RFC 822 headers or
/// terminate the header block early. `\r` and `\n` are how one header ends
/// and the next begins (or the header block ends and the body begins); a
/// bare NUL is refused too since it has no legitimate place in a header and
/// some parsers treat it as a terminator. Named in the error so the caller
/// (ultimately, whatever surfaced the LLM's malformed argument) knows which
/// field to blame.
fn reject_header_injection(field: &str, value: &str) -> anyhow::Result<()> {
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        bail!(
            "gmail: refusing to draft a message: the {field} field contains a carriage \
             return, newline, or NUL byte, which could inject additional headers into the \
             drafted message"
        );
    }
    Ok(())
}

/// RFC 2047-encode a header value if (and only if) it contains non-ASCII
/// bytes; a pure-ASCII value is returned unchanged so an ordinary subject
/// stays human-readable in the raw message rather than being needlessly
/// base64-wrapped.
///
/// Each `=?UTF-8?B?...?=` "encoded word" is kept within RFC 2047's
/// 75-character-per-word limit: `=?UTF-8?B?` (10 chars) and `?=` (2 chars)
/// are fixed overhead, leaving 63 characters of base64 text, i.e. at most 45
/// raw bytes per word (`ceil(45/3)*4 == 60 <= 63`, with headroom to spare
/// rather than cutting it exactly to the limit). A value that needs more
/// than one word is split on UTF-8 character boundaries — never mid-codepoint
/// — and the words are folded with `\r\n ` (CRLF + a single space) between
/// them, ordinary RFC 5322 header folding, which a compliant parser treats
/// as whitespace between adjacent encoded words when reassembling them.
fn encode_header_value(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }

    const MAX_BYTES_PER_WORD: usize = 45;

    let mut words = Vec::new();
    let mut start = 0;
    while start < value.len() {
        let mut end = (start + MAX_BYTES_PER_WORD).min(value.len());
        while end > start && !value.is_char_boundary(end) {
            end -= 1;
        }
        words.push(format!(
            "=?UTF-8?B?{}?=",
            STANDARD.encode(&value[start..end])
        ));
        start = end;
    }

    words.join("\r\n ")
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

fn explain(status: reqwest::StatusCode, body: &str) -> String {
    let hint = match status.as_u16() {
        401 => {
            "The access token was rejected: it has expired or been revoked. If this \
             persists, re-authorise the account with ea-google-authorize. "
        }
        403 => {
            "Google refused this request. The token may lack the required Gmail scope, or \
             the account may be rate limited. "
        }
        404 => "No such message, or the token's owner cannot see it. ",
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

/// `list_recent(auth, account, query, max)` against the real Gmail API. The
/// testable seam is [`GmailClient::new`], which takes the base URL as a
/// parameter.
pub async fn list_recent(
    auth: &Auth,
    account: &str,
    query: &str,
    max: usize,
) -> anyhow::Result<Vec<Mail>> {
    GmailClient::new(GOOGLE_GMAIL_BASE)?
        .list_recent(auth, account, query, max)
        .await
}

/// A Gmail message id is safe to interpolate into a URL path only if it
/// cannot contain a separator or a dot segment. See [`GmailClient::get`].
fn validate_message_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        bail!("gmail: a message id must not be empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "gmail: message id {id:?} is not a Gmail message id (expected letters, \
             digits, '-' and '_' only)"
        );
    }
    Ok(())
}

/// `get(auth, account, id)` against the real Gmail API.
pub async fn get(auth: &Auth, account: &str, id: &str) -> anyhow::Result<Mail> {
    GmailClient::new(GOOGLE_GMAIL_BASE)?
        .get(auth, account, id)
        .await
}

/// `create_draft(auth, account, to, subject, body)` against the real Gmail
/// API.
pub async fn create_draft(
    auth: &Auth,
    account: &str,
    to: &str,
    subject: &str,
    body: &str,
) -> anyhow::Result<String> {
    GmailClient::new(GOOGLE_GMAIL_BASE)?
        .create_draft(auth, account, to, subject, body)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use wiremock::matchers::{header as header_matcher, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::{RefreshBackend, RefreshError, RefreshResponse, TokenStore, Tokens};

    // -----------------------------------------------------------------------
    // Body extraction
    // -----------------------------------------------------------------------

    fn encode(text: &str) -> String {
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    fn text_part(mime_type: &str, text: &str) -> Part {
        Part {
            mime_type: Some(mime_type.to_string()),
            filename: None,
            headers: None,
            body: Some(PartBody {
                data: Some(encode(text)),
            }),
            parts: None,
        }
    }

    fn container(mime_type: &str, parts: Vec<Part>) -> Part {
        Part {
            mime_type: Some(mime_type.to_string()),
            filename: None,
            headers: None,
            body: None,
            parts: Some(parts),
        }
    }

    #[test]
    fn a_simple_text_plain_body_is_read() {
        let payload = text_part("text/plain", "Hello, this is the body.");
        assert_eq!(extract_body(&payload), "Hello, this is the body.");
    }

    #[test]
    fn text_plain_is_preferred_inside_multipart_alternative() {
        let payload = container(
            "multipart/alternative",
            vec![
                text_part("text/plain", "plain wins"),
                text_part("text/html", "<p>html loses</p>"),
            ],
        );
        assert_eq!(extract_body(&payload), "plain wins");
    }

    /// Review Focus #4: HTML-only mail is the common case, and dropping it
    /// to an empty body would make a triage session discard mail that
    /// mattered.
    #[test]
    fn an_html_only_message_falls_back_to_stripped_html() {
        let payload = text_part(
            "text/html",
            "<html><body><p>Your invoice is ready.</p><p>Amount: <b>100 SEK</b></p></body></html>",
        );

        let body = extract_body(&payload);
        assert!(!body.contains('<'), "tags must be gone: {body:?}");
        assert!(!body.contains('>'), "tags must be gone: {body:?}");
        assert!(body.contains("Your invoice is ready."), "{body:?}");
        assert!(body.contains("100 SEK"), "{body:?}");
    }

    #[test]
    fn nested_multiparts_are_recursed_into() {
        let payload = container(
            "multipart/mixed",
            vec![container(
                "multipart/alternative",
                vec![text_part("text/plain", "deeply nested plain text")],
            )],
        );
        assert_eq!(extract_body(&payload), "deeply nested plain text");
    }

    #[test]
    fn attachment_parts_are_ignored() {
        let mut attachment = text_part("text/plain", "not the body, an attachment");
        attachment.filename = Some("notes.txt".to_string());

        let payload = container(
            "multipart/mixed",
            vec![attachment, text_part("text/plain", "the real body")],
        );

        assert_eq!(extract_body(&payload), "the real body");
    }

    #[test]
    fn an_empty_payload_returns_empty_string_not_panic() {
        let payload = Part::default();
        assert_eq!(extract_body(&payload), "");
    }

    /// A byte-level mistake in base64url decoding (e.g. treating `-`/`_` as
    /// invalid, or mishandling missing padding) would corrupt or drop
    /// non-ASCII text; a plain ASCII fixture would not catch that.
    #[test]
    fn base64url_with_dash_and_underscore_decodes_correctly() {
        let swedish = "Räksmörgås — åäö";
        let payload = text_part("text/plain", swedish);
        assert_eq!(extract_body(&payload), swedish);
    }

    #[test]
    fn html_stripping_decodes_common_entities_and_collapses_blank_runs() {
        let payload = text_part(
            "text/html",
            "<p>Tom &amp; Jerry say &quot;hi&quot;</p>\n\n\n<p>next paragraph</p>",
        );
        let body = extract_body(&payload);
        assert!(body.contains("Tom & Jerry say \"hi\""), "{body:?}");
        assert!(!body.contains("\n\n\n"), "{body:?}");
    }

    // -----------------------------------------------------------------------
    // Normalisation
    // -----------------------------------------------------------------------

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    fn raw_message(headers: Vec<Header>, payload_body: Option<PartBody>) -> RawMessage {
        RawMessage {
            id: Some("msg-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            label_ids: Some(vec!["INBOX".to_string(), "UNREAD".to_string()]),
            snippet: Some("A short preview…".to_string()),
            internal_date: Some("1700000000000".to_string()),
            payload: Some(Part {
                mime_type: Some("text/plain".to_string()),
                filename: None,
                headers: Some(headers),
                body: payload_body,
                parts: None,
            }),
        }
    }

    #[test]
    fn normalize_maps_headers_labels_and_body() {
        let raw = raw_message(
            vec![
                header("From", "advisor@kth.se"),
                header("Subject", "Thesis feedback"),
            ],
            Some(PartBody {
                data: Some(encode("See attached comments.")),
            }),
        );

        let mail = normalize(&raw, "work");

        assert_eq!(mail.id, "msg-1");
        assert_eq!(mail.thread_id, "thread-1");
        assert_eq!(mail.account, "work");
        assert_eq!(mail.from, "advisor@kth.se");
        assert_eq!(mail.subject, "Thesis feedback");
        assert_eq!(mail.snippet, "A short preview…");
        assert_eq!(mail.body, "See attached comments.");
        assert_eq!(mail.labels, vec!["INBOX".to_string(), "UNREAD".to_string()]);
    }

    #[test]
    fn internal_date_milliseconds_string_converts_to_utc_timestamp() {
        let raw = raw_message(vec![], None);
        let mail = normalize(&raw, "work");
        // 1700000000000 ms -> 1700000000 s since epoch.
        assert_eq!(mail.received_at.timestamp(), 1_700_000_000);
    }

    #[test]
    fn an_unparseable_internal_date_sinks_to_the_epoch_not_now() {
        // A corrupt `internalDate` must sink a message to the bottom of a
        // triage queue ordered by recency, not `Utc::now()` it to the top
        // and displace genuinely urgent mail.
        let mut raw = raw_message(vec![], None);
        raw.internal_date = Some("not-a-timestamp".to_string());
        let mail = normalize(&raw, "work");
        assert_eq!(mail.received_at, DateTime::<Utc>::UNIX_EPOCH);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let raw = raw_message(vec![header("from", "lower-case-header@example.com")], None);
        let mail = normalize(&raw, "work");
        assert_eq!(mail.from, "lower-case-header@example.com");
    }

    #[test]
    fn missing_from_becomes_unknown_sender() {
        let raw = raw_message(vec![header("Subject", "no sender here")], None);
        let mail = normalize(&raw, "work");
        assert_eq!(mail.from, "(unknown sender)");
    }

    #[test]
    fn missing_subject_becomes_no_subject() {
        let raw = raw_message(vec![header("From", "someone@example.com")], None);
        let mail = normalize(&raw, "work");
        assert_eq!(mail.subject, "(no subject)");
    }

    // -----------------------------------------------------------------------
    // The HTTP client
    // -----------------------------------------------------------------------

    const ACCESS_TOKEN: &str = "ya29.GMAIL-ACCESS-do-not-leak";

    /// A backend that must never be called: every test below writes a
    /// healthy, unexpired token straight into the store.
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
            Box::pin(async { panic!("gmail calls must not need to refresh a healthy token") })
        }
    }

    /// The access token a forced refresh hands back. Distinct from
    /// [`ACCESS_TOKEN`] so a mock can tell the retry from the first attempt
    /// by its `Authorization` header alone.
    const RENEWED_TOKEN: &str = "ya29.GMAIL-RENEWED-do-not-leak";

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
                    scope: "https://www.googleapis.com/auth/gmail.readonly".to_string(),
                },
            )
            .unwrap();
        Auth::with_backend(store, backend)
    }

    fn json(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    /// The shape of the poll, not just its result. 25 sequential `get`s per
    /// account meant 54 round trips for a two-account poll on a 120-second
    /// budget — 555 ms per request, which a slow link does not have. Eight in
    /// flight turns that into four waves per account.
    ///
    /// Timed, deliberately, because there is nothing else to observe: the
    /// margins are wide (serial would be 6 s; the assertion is 3 s) and the
    /// lower bound is what stops "concurrency" quietly becoming "unbounded",
    /// which is its own way to get an integration rate limited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_message_gets_run_bounded_concurrently_and_stay_in_list_order() {
        const COUNT: usize = 24;
        const DELAY: Duration = Duration::from_millis(250);

        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        let ids: Vec<String> = (0..COUNT).map(|n| format!("msg-{n:02}")).collect();
        let refs: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "threadId": format!("t-{id}") }))
            .collect();

        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .respond_with(json(serde_json::json!({ "messages": refs })))
            .mount(&server)
            .await;

        for id in &ids {
            Mock::given(method("GET"))
                .and(path(format!("/users/me/messages/{id}")))
                .respond_with(
                    json(serde_json::json!({
                        "id": id,
                        "threadId": format!("t-{id}"),
                        "labelIds": ["INBOX"],
                        "snippet": "hi",
                        "internalDate": "1700000000000",
                        "payload": {
                            "mimeType": "text/plain",
                            "headers": [
                                {"name": "From", "value": "a@example.com"},
                                {"name": "Subject", "value": id},
                            ],
                            "body": { "data": encode("body text") },
                        },
                    }))
                    .set_delay(DELAY),
                )
                .mount(&server)
                .await;
        }

        let started = std::time::Instant::now();
        let mail = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "is:unread", COUNT)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(mail.len(), COUNT);
        let got: Vec<&str> = mail.iter().map(|m| m.id.as_str()).collect();
        let want: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(
            got, want,
            "the rows must stay in messages.list order — that order is Gmail's recency order"
        );

        assert!(
            elapsed < DELAY * COUNT as u32 / 2,
            "{COUNT} gets took {elapsed:?}; sequentially they would take {:?}, so these did \
             not run concurrently",
            DELAY * COUNT as u32
        );
        assert!(
            elapsed >= DELAY * 2,
            "{COUNT} gets took {elapsed:?}, which is under two waves at {} in flight: the \
             concurrency is not bounded",
            GET_CONCURRENCY
        );
    }

    #[tokio::test]
    async fn list_recent_fetches_and_normalises_messages() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .respond_with(json(serde_json::json!({
                "messages": [{ "id": "msg-1", "threadId": "t-1" }],
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/users/me/messages/msg-1"))
            .respond_with(json(serde_json::json!({
                "id": "msg-1",
                "threadId": "t-1",
                "labelIds": ["INBOX"],
                "snippet": "hi",
                "internalDate": "1700000000000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [
                        {"name": "From", "value": "a@example.com"},
                        {"name": "Subject", "value": "Hi"},
                    ],
                    "body": { "data": encode("body text") },
                },
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = GmailClient::new(&server.uri()).unwrap();
        let mail = client
            .list_recent(&auth, "work", "is:unread", 10)
            .await
            .unwrap();

        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].account, "work");
        assert_eq!(mail[0].from, "a@example.com");
        assert_eq!(mail[0].body, "body text");
    }

    #[tokio::test]
    async fn list_recent_sends_the_bearer_token_and_never_in_a_query_string() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {ACCESS_TOKEN}").as_str(),
            ))
            .respond_with(json(serde_json::json!({ "messages": [] })))
            .expect(1)
            .mount(&server)
            .await;

        GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "", 10)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].url.query().unwrap_or("").contains(ACCESS_TOKEN),
            "the access token must never appear in the URL"
        );
    }

    /// Gmail's half of the forced refresh. A revoked session invalidates
    /// every outstanding access token at once, and `access_token` alone would
    /// not notice for up to an hour.
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

        Mock::given(method("GET"))
            .and(header_matcher(
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
            .and(path("/users/me/messages"))
            .and(header_matcher(
                "authorization",
                format!("Bearer {RENEWED_TOKEN}").as_str(),
            ))
            .respond_with(json(serde_json::json!({ "messages": [] })))
            .mount(&server)
            .await;

        let mail = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "is:unread", 10)
            .await
            .expect("the retry after the forced refresh must succeed");

        assert!(mail.is_empty());
        assert_eq!(backend.calls(), 1, "exactly one forced refresh");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the original request and exactly one retry"
        );
    }

    /// One retry, not a loop: a token refused twice stays refused, and
    /// hammering the token endpoint is how the whole integration gets rate
    /// limited.
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

        let err = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "", 10)
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

        let err = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "", 10)
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

        let err = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "", 10)
            .await
            .expect_err("an HTML body must not be accepted as empty JSON");

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
            .respond_with(json(serde_json::json!({ "messages": [] })))
            .expect(0) // a followed redirect would land here with the token
            .mount(&elsewhere)
            .await;

        Mock::given(method("GET"))
            .and(path("/users/me/messages"))
            .respond_with(ResponseTemplate::new(301).insert_header(
                "location",
                format!("{}/users/me/messages", elsewhere.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        let err = GmailClient::new(&server.uri())
            .unwrap()
            .list_recent(&auth, "work", "", 10)
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
        let client = GmailClient::new("http://127.0.0.1:1").unwrap();

        let err = client
            .list_recent(&auth, "work", "", 10)
            .await
            .expect_err("no listener");

        assert!(
            !format!("{err:#}").contains(ACCESS_TOKEN),
            "the error leaked the token"
        );
    }

    #[test]
    fn gmail_client_debug_never_prints_a_credential() {
        let client = GmailClient::new("https://gmail.googleapis.com").unwrap();
        let text = format!("{client:?}");
        assert!(!text.contains(ACCESS_TOKEN));
    }

    #[test]
    fn a_base_url_with_userinfo_is_refused() {
        let err = GmailClient::new("https://someone:secret@example.test")
            .expect_err("userinfo must be refused");
        assert!(
            format!("{err:#}").contains("username or password"),
            "{err:#}"
        );
    }

    // -----------------------------------------------------------------------
    // create_draft: the one write, and it must never touch messages/send
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_draft_never_touches_the_send_endpoint() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("POST"))
            .respond_with(json(serde_json::json!({ "id": "draft-1" })))
            .expect(1)
            .mount(&server)
            .await;

        GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(&auth, "work", "friend@example.com", "Hi", "Body text")
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let request_path = requests[0].url.path();
        assert!(request_path.contains("drafts"), "{request_path}");
        assert!(!request_path.contains("send"), "{request_path}");
    }

    #[tokio::test]
    async fn create_draft_base64url_encodes_the_rfc822_message() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        // The literal words must NOT appear in the request body — they must
        // only appear base64url-encoded inside the `raw` field.
        Mock::given(method("POST"))
            .and(path("/users/me/drafts"))
            .respond_with(json(serde_json::json!({ "id": "draft-1" })))
            .expect(1)
            .mount(&server)
            .await;

        GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(
                &auth,
                "work",
                "friend@example.com",
                "Meeting tomorrow",
                "See you at 10.",
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let raw = sent["message"]["raw"].as_str().unwrap();
        assert!(
            !raw.contains("Meeting tomorrow"),
            "the raw field must be base64url-encoded, not plain text"
        );

        let decoded_bytes = URL_SAFE_NO_PAD.decode(raw).unwrap();
        let decoded = String::from_utf8(decoded_bytes).unwrap();
        assert!(decoded.contains("To: friend@example.com"));
        assert!(decoded.contains("Subject: Meeting tomorrow"));
        assert!(decoded.contains("See you at 10."));
    }

    #[tokio::test]
    async fn create_draft_returns_the_draft_id() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("POST"))
            .and(path("/users/me/drafts"))
            .respond_with(json(serde_json::json!({ "id": "draft-42" })))
            .mount(&server)
            .await;

        let id = GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(&auth, "work", "friend@example.com", "Hi", "Body")
            .await
            .unwrap();

        assert_eq!(id, "draft-42");
    }

    #[tokio::test]
    async fn create_draft_sends_the_bearer_token_never_in_the_body_as_plaintext() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("POST"))
            .and(path("/users/me/drafts"))
            .and(wiremock::matchers::header(
                "authorization",
                format!("Bearer {ACCESS_TOKEN}").as_str(),
            ))
            .respond_with(json(serde_json::json!({ "id": "draft-1" })))
            .expect(1)
            .mount(&server)
            .await;

        GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(&auth, "work", "friend@example.com", "Hi", "Body")
            .await
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // Header injection: `to`/`subject` arrive from an LLM's `propose_action`
    // call and must never be able to smuggle extra RFC 822 headers into a
    // draft the owner sends without reading raw headers.
    // -----------------------------------------------------------------------

    #[test]
    fn build_rfc822_rejects_crlf_bcc_injection_in_to() {
        let err = build_rfc822(
            "victim@example.com\r\nBcc: attacker@example.com",
            "Hi",
            "Body",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("to"), "{err:#}");
    }

    #[test]
    fn build_rfc822_rejects_crlf_bcc_injection_in_subject() {
        let err = build_rfc822(
            "victim@example.com",
            "Hi\r\nBcc: attacker@example.com",
            "Body",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("subject"), "{err:#}");
    }

    #[test]
    fn build_rfc822_rejects_a_bare_lf_in_subject() {
        assert!(build_rfc822("victim@example.com", "Hi\nthere", "Body").is_err());
    }

    #[test]
    fn build_rfc822_rejects_a_bare_cr_in_to() {
        assert!(build_rfc822("victim@example.com\rx", "Hi", "Body").is_err());
    }

    #[test]
    fn build_rfc822_still_accepts_an_ordinary_value() {
        let raw = build_rfc822("friend@example.com", "Lunch?", "See you at noon.").unwrap();
        assert!(raw.contains("To: friend@example.com"));
        assert!(raw.contains("Subject: Lunch?"));
        assert!(raw.contains("See you at noon."));
    }

    #[tokio::test]
    async fn create_draft_rejects_crlf_bcc_injection_in_to_and_makes_no_request() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        // Deliberately no mock is mounted: any request at all reaching the
        // server (matched or not — `received_requests` records both) would
        // prove the rejection happened too late.
        let result = GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(
                &auth,
                "work",
                "victim@example.com\r\nBcc: attacker@example.com",
                "Hi",
                "Body",
            )
            .await;

        assert!(result.is_err());
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }

    #[tokio::test]
    async fn create_draft_rejects_crlf_injection_in_subject_and_makes_no_request() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        let result = GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(
                &auth,
                "work",
                "friend@example.com",
                "Hi\r\nBcc: attacker@example.com",
                "Body",
            )
            .await;

        assert!(result.is_err());
        let requests = server.received_requests().await.unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }

    // -----------------------------------------------------------------------
    // RFC 2047: a non-ASCII `to`/`subject` must not become mojibake in the
    // owner's drafts folder. `Räksmörgås` in a subject is the common case
    // for a Swedish company, not an edge case.
    // -----------------------------------------------------------------------

    /// Reverse whatever mix of literal ASCII and folded `=?UTF-8?B?...?=`
    /// encoded words a header value produced by this module contains, back
    /// to the original string. Only as capable as this module's own encoder
    /// needs to be verified against — not a general RFC 2047 decoder.
    fn decode_rfc2047(header: &str) -> String {
        header
            .split("\r\n ")
            .map(|word| {
                let word = word.trim();
                match word
                    .strip_prefix("=?UTF-8?B?")
                    .and_then(|rest| rest.strip_suffix("?="))
                {
                    Some(inner) => {
                        let bytes = STANDARD.decode(inner).unwrap();
                        String::from_utf8(bytes).unwrap()
                    }
                    None => word.to_string(),
                }
            })
            .collect()
    }

    #[test]
    fn encode_header_value_leaves_ascii_unchanged() {
        assert_eq!(encode_header_value("Meeting tomorrow"), "Meeting tomorrow");
    }

    #[test]
    fn encode_header_value_encodes_and_round_trips_swedish_characters() {
        let original = "Räksmörgås — åäö";
        let encoded = encode_header_value(original);
        assert_ne!(encoded, original);
        assert!(encoded.starts_with("=?UTF-8?B?"));
        assert_eq!(decode_rfc2047(&encoded), original);
    }

    #[test]
    fn encode_header_value_folds_a_long_subject_correctly() {
        // 200 repetitions of a 2-byte character comfortably exceeds one
        // encoded word's 45-raw-byte budget, forcing a fold across several.
        let original: String = "ö".repeat(200);
        let encoded = encode_header_value(&original);

        let words: Vec<&str> = encoded.split("\r\n ").collect();
        assert!(
            words.len() > 1,
            "expected folding into multiple words, got one: {encoded}"
        );
        for word in &words {
            assert!(
                word.len() <= 75,
                "encoded word exceeds RFC 2047's 75-character limit ({} chars): {word}",
                word.len()
            );
            assert!(word.starts_with("=?UTF-8?B?") && word.ends_with("?="));
        }

        assert_eq!(decode_rfc2047(&encoded), original);
    }

    // -----------------------------------------------------------------------
    // get: one message by id
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_fetches_and_normalises_one_message() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("GET"))
            .and(path("/users/me/messages/msg-7"))
            .respond_with(json(serde_json::json!({
                "id": "msg-7",
                "threadId": "t-7",
                "labelIds": ["INBOX", "UNREAD"],
                "snippet": "A snippet",
                "internalDate": "1700000000000",
                "payload": {
                    "mimeType": "text/plain",
                    "headers": [
                        { "name": "From", "value": "prof@kth.se" },
                        { "name": "Subject", "value": "Exam" },
                    ],
                    "body": { "data": encode("The whole body.") },
                },
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mail = GmailClient::new(&server.uri())
            .unwrap()
            .get(&auth, "work", "msg-7")
            .await
            .unwrap();

        assert_eq!(mail.id, "msg-7");
        assert_eq!(mail.account, "work");
        assert_eq!(mail.subject, "Exam");
        assert_eq!(mail.body, "The whole body.");
    }

    /// The id arrives from a tool argument a language model wrote, and the URL
    /// is built by joining a relative string onto the base. Without this
    /// check, `../` would walk out of `users/me/messages/` carrying the
    /// bearer token.
    #[tokio::test]
    async fn get_refuses_a_message_id_that_could_walk_the_url_path() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        // No mocks mounted at all: reaching the network is itself the failure.
        let client = GmailClient::new(&server.uri()).unwrap();
        for id in ["../../users/me/settings", "msg/../other", "", "a?b", "a b"] {
            let err = client
                .get(&auth, "work", id)
                .await
                .expect_err("a non-id must be refused before any request");
            assert!(
                err.to_string().contains("message id"),
                "id {id:?} gave {err:#}"
            );
        }
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a rejected id must not reach the network"
        );
    }

    #[test]
    fn build_rfc822_declares_mime_version() {
        let raw = build_rfc822("friend@example.com", "Hi", "Body").unwrap();
        assert!(raw.starts_with("MIME-Version: 1.0\r\n"), "{raw}");
    }

    #[test]
    fn build_rfc822_encodes_a_non_ascii_to_header() {
        let raw = build_rfc822("Räksmörgås <friend@example.com>", "Hi", "Body").unwrap();
        assert!(raw.contains("=?UTF-8?B?"), "{raw}");
        assert!(!raw.contains("Räksmörgås"), "{raw}");
    }

    #[tokio::test]
    async fn create_draft_rfc2047_encodes_a_swedish_subject() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with_healthy_token(tmp.path(), "work");

        Mock::given(method("POST"))
            .and(path("/users/me/drafts"))
            .respond_with(json(serde_json::json!({ "id": "draft-1" })))
            .expect(1)
            .mount(&server)
            .await;

        let subject = "Räksmörgås — åäö";
        GmailClient::new(&server.uri())
            .unwrap()
            .create_draft(
                &auth,
                "work",
                "friend@example.com",
                subject,
                "Smaklig måltid.",
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let raw = sent["message"]["raw"].as_str().unwrap();
        let decoded_bytes = URL_SAFE_NO_PAD.decode(raw).unwrap();
        let decoded = String::from_utf8(decoded_bytes).unwrap();

        assert!(decoded.contains("MIME-Version: 1.0"), "{decoded}");
        assert!(decoded.contains("=?UTF-8?B?"), "{decoded}");

        let after_subject = decoded.split("Subject: ").nth(1).unwrap();
        let header_value = after_subject.split("\r\nContent-Type").next().unwrap();
        assert_eq!(decode_rfc2047(header_value), subject);
    }
}
