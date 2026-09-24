//! The Microsoft Graph implementation of [`MailTransport`].
//!
//! **Everything in this file is unverified against Microsoft.** Every test
//! here runs against `wiremock` on loopback; nothing in this crate has ever
//! spoken to `graph.microsoft.com`. What is asserted below is that this client
//! handles the shapes Microsoft's documentation describes — not that those are
//! the shapes it sends. The README says the same thing to the owner, in the
//! same words, because a connector that looks tested and is not is worse than
//! one that admits it.
//!
//! # One request per account per poll
//!
//! This is the one place the Graph path is plainly better than the Gmail one.
//! Gmail's `messages.list` returns bare `{id, threadId}` pairs, so a 25-message
//! poll costs 1 + 25 requests and the whole of `ea_google::gmail`'s
//! concurrency machinery exists to make that fit inside a poll window. Graph's
//! `$select` returns the **body** in the list response, so a poll is a single
//! `GET`. There is no fan-out here to bound, and no reason to invent one.
//!
//! # The query, and why it is built by hand
//!
//! ```text
//! GET me/mailFolders/inbox/messages
//!     ?$select=id,conversationId,subject,from,receivedDateTime,bodyPreview,isRead,webLink,body
//!     &$top=<max>
//!     &$filter=isRead%20eq%20false
//! ```
//!
//! Three decisions in that, each of which could otherwise be a silent failure.
//!
//! * **The query string is assembled and handed to [`Url::set_query`] rather
//!   than built with `query_pairs_mut`.** The latter form-encodes: `$select`
//!   becomes `%24select` and the spaces in the filter become `+`. Both are
//!   very probably fine, and "very probably" is not a property to rely on
//!   against a server this code has never met. Writing the string means what
//!   goes on the wire is what is written above.
//! * **No `$orderby`.** Graph documents restrictions on combining `$filter`
//!   and `$orderby` on message collections — a request that trips one comes
//!   back as `InefficientFilter`, which would be a poll that fails
//!   permanently for a reason nobody could guess from the code. The messages
//!   are sorted by `receivedDateTime`, newest first, **in this process**
//!   instead. That is deterministic, costs nothing at 25 rows, and does not
//!   depend on an undocumented default ordering.
//! * **`inbox` explicitly, not `me/messages`.** `me/messages` spans every
//!   folder including Deleted Items and Junk, and unread junk is exactly what
//!   a triage digest must not be filled with.
//!
//! There is no pagination. `$top` bounds the result, `@odata.nextLink` is
//! ignored, and that is the intended behaviour: a mailbox with more than
//! [`crate::watch::MAX_UNREAD`] unread messages has a problem this connector
//! cannot solve, and the oldest of them are not news.
//!
//! # HTTP hardening
//!
//! The same list as `ea_google::gmail`, for the same reasons: redirects
//! disabled (`redirect::Policy::none()`, so a same-host https→http downgrade
//! cannot carry the bearer token in clear text), a JSON content-type check
//! before deserialising (Entra can answer a 200 with an HTML sign-in page),
//! `.without_url()` on every `reqwest` error, a hand-written `Debug` printing
//! only the base URL, a base-URL constructor that rejects userinfo and
//! non-http(s) schemes, and no `unwrap`, `expect` or `panic!` anywhere outside
//! the tests.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::auth::{authorize_command, Auth};
use crate::mail::{extract_body, BoxFuture, ItemBody, Mail, MailTransport};

/// Microsoft Graph v1.0. Overridable at the [`GraphTransport`] level so the
/// tests can point it at a `wiremock` server.
pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0/";

/// The properties a poll needs, and no others. `$select` is not an
/// optimisation here so much as a contract: without it Graph returns every
/// property of a message, and `body` would arrive anyway but so would a great
/// deal that only costs bytes.
const SELECT: &str =
    "id,conversationId,subject,from,receivedDateTime,bodyPreview,isRead,webLink,body";

/// Graph's own documented ceiling on `$top` for a message collection. `max` is
/// clamped to it rather than trusted.
const MAX_TOP: usize = 999;

/// Per-request deadline, same value and reasoning as `auth.rs`.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an unexpected response body to quote back.
const BODY_SNIPPET: usize = 300;

// ---------------------------------------------------------------------------
// The wire shape
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct MessagesPage {
    #[serde(default)]
    value: Vec<RawMessage>,
}

/// One Graph `message`, as far as this connector cares.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    from: Option<Recipient>,
    #[serde(default, rename = "receivedDateTime")]
    received_date_time: Option<String>,
    #[serde(default, rename = "bodyPreview")]
    body_preview: Option<String>,
    #[serde(default, rename = "isRead")]
    is_read: Option<bool>,
    #[serde(default, rename = "webLink")]
    web_link: Option<String>,
    #[serde(default)]
    body: Option<ItemBody>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Recipient {
    #[serde(default, rename = "emailAddress")]
    email_address: Option<EmailAddress>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct EmailAddress {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// Turn one raw Graph message into a [`Mail`]. Always succeeds.
///
/// An absent or malformed field becomes a safe default rather than a dropped
/// message: Graph has already told us this message exists, so there is no
/// "this message is not real" case to represent, and a message silently
/// missing from a digest is the failure this connector exists to avoid.
pub fn normalize(raw: &RawMessage, account: &str) -> Mail {
    let from = raw
        .from
        .as_ref()
        .and_then(|r| r.email_address.as_ref())
        .map(format_address)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown sender)".to_string());

    let subject = raw
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)")
        .to_string();

    let preview = raw.body_preview.clone().unwrap_or_default();

    // A message with no parseable `receivedDateTime` falls back to the Unix
    // epoch rather than `Utc::now()`: "now" would sort a corrupt message to
    // the *top* of a recency-ordered triage queue, displacing genuinely
    // urgent mail, whereas the epoch sinks it where a malformed field belongs.
    let received_at = raw
        .received_date_time
        .as_deref()
        .and_then(parse_graph_timestamp)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);

    Mail {
        id: raw.id.clone().unwrap_or_default(),
        conversation_id: raw.conversation_id.clone().unwrap_or_default(),
        account: account.to_string(),
        from,
        subject,
        body: extract_body(raw.body.as_ref(), &preview),
        preview,
        received_at,
        // A message Graph did not label is treated as unread: this connector's
        // whole job is surfacing unread mail, and the safe direction for an
        // ambiguous message is to show it rather than hide it.
        is_read: raw.is_read.unwrap_or(false),
        web_link: raw.web_link.clone().unwrap_or_default(),
    }
}

/// `Name <address>`, or whichever half exists.
fn format_address(address: &EmailAddress) -> String {
    let name = address.name.as_deref().unwrap_or("").trim();
    let addr = address.address.as_deref().unwrap_or("").trim();
    match (name.is_empty(), addr.is_empty()) {
        (false, false) if name == addr => addr.to_string(),
        (false, false) => format!("{name} <{addr}>"),
        (true, false) => addr.to_string(),
        (false, true) => name.to_string(),
        (true, true) => String::new(),
    }
}

/// Graph timestamps are ISO 8601 / RFC 3339, normally `…Z`.
///
/// A value with no offset at all has been observed from OData services; it is
/// read as UTC rather than dropped, because dropping it would sink a perfectly
/// good message to the epoch.
fn parse_graph_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
}

/// A Graph message id is safe to interpolate into a URL path only if it cannot
/// contain a separator or a dot segment.
///
/// The id reaches [`GraphTransport::get`] from a tool argument a language
/// model wrote, and the URL is built with `Url::join` on a relative string: an
/// id of `../../users/someone-else/messages/x` would otherwise walk out of
/// `me/messages/` and issue a bearer-authenticated GET against a resource
/// nobody asked for.
///
/// Graph's REST message ids are base64url text — letters, digits, `-`, `_`,
/// and `=` padding. **Unverified**, like everything else here: if a real id
/// turns out to contain a character outside that set, this refuses it with a
/// message naming the id, which is a loud failure rather than a wrong request.
pub fn validate_message_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        bail!("kth: a message id must not be empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
    {
        bail!(
            "kth: message id {id:?} is not a Microsoft Graph message id (expected \
             base64url text: letters, digits, '-', '_' and '='). It was refused rather \
             than sent, because an id containing '/' or '..' would address a different \
             resource than the one asked for."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The transport
// ---------------------------------------------------------------------------

/// Reads mail over Microsoft Graph, with per-account OAuth handled by [`Auth`].
///
/// Owns its credentials rather than taking them per call: see the seam
/// discussion in [`crate::mail`]. That is what lets a different transport be
/// substituted without touching anything above it.
pub struct GraphTransport {
    http: reqwest::Client,
    /// Always ends in `/`, so [`Url::join`] appends rather than replaces the
    /// last path segment.
    base: Url,
    auth: Arc<Auth>,
}

impl fmt::Debug for GraphTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Nothing secret: the base URL is Microsoft's public API host (or a
        // test server's loopback address), never a credential.
        f.debug_struct("GraphTransport")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl GraphTransport {
    pub fn new(base_url: &str, auth: Arc<Auth>) -> anyhow::Result<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/"))
            .with_context(|| format!("Graph base URL {base_url:?} is not a URL"))?;

        if !matches!(base.scheme(), "http" | "https") {
            bail!(
                "Graph base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            );
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            bail!("Graph base URL must not contain a username or password");
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // No redirects, ever: a same-host https->http downgrade would
            // carry the bearer access token onto the wire in clear text, and
            // this API has no legitimate reason to redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTPS client for Microsoft Graph")?;

        Ok(Self { http, base, auth })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    async fn list_unread_inner(&self, account: &str, max: usize) -> anyhow::Result<Vec<Mail>> {
        let token = self.auth.access_token(account).await?;
        let top = max.clamp(1, MAX_TOP);

        let mut url = self
            .base
            .join("me/mailFolders/inbox/messages")
            .context("building the Graph messages URL")?;
        // Written out rather than form-encoded; see the module docs.
        url.set_query(Some(&format!(
            "$select={SELECT}&$top={top}&$filter=isRead%20eq%20false"
        )));

        let page: MessagesPage = self.get_json(account, &token, url).await?;

        let mut mail: Vec<Mail> = page
            .value
            .iter()
            .map(|raw| normalize(raw, account))
            .collect();
        // Newest first, here rather than with `$orderby`; see the module docs.
        mail.sort_by(|a, b| b.received_at.cmp(&a.received_at));
        Ok(mail)
    }

    async fn get_inner(&self, account: &str, id: &str) -> anyhow::Result<Mail> {
        validate_message_id(id)?;
        let token = self.auth.access_token(account).await?;

        let mut url = self
            .base
            .join(&format!("me/messages/{id}"))
            .context("building the Graph message URL")?;
        url.set_query(Some(&format!("$select={SELECT}")));

        let raw: RawMessage = self.get_json(account, &token, url).await?;
        Ok(normalize(&raw, account))
    }

    /// One bearer-authenticated `GET`, decoded, with the 401 retry.
    async fn get_json<T: DeserializeOwned>(
        &self,
        account: &str,
        token: &str,
        url: Url,
    ) -> anyhow::Result<T> {
        let mut whence = url.clone();
        whence.set_query(None);

        let response = self
            .send_with_retry(account, token, &whence, |bearer| {
                self.http
                    .get(url.clone())
                    .bearer_auth(bearer)
                    .header(reqwest::header::ACCEPT, "application/json")
            })
            .await?;

        self.read_json(response, &whence).await
    }

    /// Send a request, and if Graph answers 401, force a token refresh and
    /// send it **once** more.
    ///
    /// The access token can be dead long before it expires — a password
    /// change, a revoked session, a Conditional Access policy that now demands
    /// a compliant device — and [`Auth::access_token`] only refreshes on local
    /// expiry, so without this a revoked session produces 401s for up to an
    /// hour: a dozen consecutive failed polls.
    ///
    /// Exactly one retry. A token Microsoft refuses twice will be refused a
    /// third time, and Phase 1's Fortnox notes record where an unbounded retry
    /// against a dead grant ends: throttled, for the whole integration.
    /// [`Auth::refresh_after_unauthorized`] bounds the other half of it.
    ///
    /// Re-sending is safe for everything this transport does: every request is
    /// a `GET`.
    async fn send_with_retry(
        &self,
        account: &str,
        token: &str,
        whence: &Url,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        let response = send(build(token), whence).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let fresh = self
            .auth
            .refresh_after_unauthorized(account, token)
            .await
            .with_context(|| {
                format!(
                    "kth: {whence} was refused with HTTP 401 and the forced token refresh \
                     for account {account:?} failed too"
                )
            })?;

        send(build(&fresh), whence).await
    }

    /// Shared response handling: read the body, refuse a non-2xx status naming
    /// it, refuse a non-JSON content-type before deserialising (Microsoft can
    /// answer a 200 with an HTML sign-in or consent page), then deserialise.
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
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let body = response
            .text()
            .await
            .map_err(|err| err.without_url())
            .with_context(|| format!("kth: reading the reply from {whence}"))?;

        if !status.is_success() {
            bail!(
                "kth: {whence} returned HTTP {status}{}. {}",
                canonical_reason(status),
                explain(status, retry_after.as_deref(), &body)
            );
        }

        if !is_json(&content_type) {
            bail!(
                "kth: {whence} answered HTTP {status} with content-type {content_type:?}, \
                 which is not JSON. That is usually a sign-in or consent page, which means \
                 the token is not being accepted. Body began: {}",
                snippet(&body)
            );
        }

        serde_json::from_str(&body).with_context(|| {
            format!(
                "kth: {whence} answered JSON this build does not understand. Body began: {}",
                snippet(&body)
            )
        })
    }
}

impl MailTransport for GraphTransport {
    fn accounts(&self) -> anyhow::Result<Vec<String>> {
        self.auth.store().list()
    }

    fn list_unread<'a>(
        &'a self,
        account: &'a str,
        max: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Mail>>> {
        Box::pin(self.list_unread_inner(account, max))
    }

    fn get<'a>(&'a self, account: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<Mail>> {
        Box::pin(self.get_inner(account, id))
    }

    fn authorize_hint(&self, account: &str) -> String {
        authorize_command(account)
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
        .with_context(|| format!("kth: the request to {whence} failed"))
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

/// The hint attached to a failing status.
///
/// 403 gets the longest one on purpose: it is the status a tenant that refuses
/// this application answers with, and it is the single most likely way this
/// connector fails for real. Sending the reader to "check your network" for an
/// `AADSTS`-class consent refusal would waste an afternoon.
fn explain(status: reqwest::StatusCode, retry_after: Option<&str>, body: &str) -> String {
    let hint = match status.as_u16() {
        401 => "The access token was rejected: it has expired, been revoked, or been \
                refused by a Conditional Access policy. If this persists, re-authorise \
                with ea-kth-authorize. "
            .to_string(),
        403 => "Microsoft Graph refused this request. Either the grant is missing the \
                Mail.Read scope (a grant does not gain scopes retroactively — \
                re-authorise), or KTH's tenant does not permit this application at all. \
                See the \"If KTH refuses consent\" section of connectors/kth/README.md. "
            .to_string(),
        404 => "No such message, or the signed-in account cannot see it. ".to_string(),
        429 => format!(
            "Microsoft Graph is throttling this mailbox.{} Nothing is retried here: the \
             next scheduled poll is the retry, and hammering a throttled endpoint \
             lengthens the penalty. ",
            match retry_after {
                Some(seconds) => format!(" It asked for a {seconds}s pause."),
                None => String::new(),
            }
        ),
        _ => String::new(),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Offline. A `wiremock` server on loopback plays Graph; nothing here can reach
// graph.microsoft.com.

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use crate::auth::{RefreshBackend, RefreshError, TokenResponse, TokenStore, Tokens};

    const ACCESS_TOKEN: &str = "eyJ-GRAPH-ACCESS-do-not-leak";
    const REFRESHED_TOKEN: &str = "eyJ-GRAPH-ACCESS-SECOND";

    struct UnusedBackend;

    impl RefreshBackend for UnusedBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> BoxFuture<'a, Result<TokenResponse, RefreshError>> {
            Box::pin(async { panic!("a healthy token must not be refreshed") })
        }
    }

    /// Counts refreshes and rotates the access token, so a test can assert
    /// both "exactly one refresh" and "the retry used the new token".
    struct CountingBackend(AtomicUsize);

    impl RefreshBackend for CountingBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> BoxFuture<'a, Result<TokenResponse, RefreshError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(TokenResponse {
                    access_token: REFRESHED_TOKEN.to_string(),
                    refresh_token: Some("rotated".to_string()),
                    expires_in: 3600,
                    scope: None,
                })
            })
        }
    }

    fn auth_with(dir: &std::path::Path, backend: Arc<dyn RefreshBackend>) -> Arc<Auth> {
        let store = TokenStore::new(Some(dir.to_path_buf()));
        store
            .write(
                "kth",
                &Tokens {
                    access_token: ACCESS_TOKEN.to_string(),
                    refresh_token: "r".to_string(),
                    expiry: Utc::now() + chrono::Duration::hours(1),
                    scope: crate::auth::SCOPES.join(" "),
                },
            )
            .unwrap();
        Arc::new(Auth::with_backend(
            TokenStore::new(Some(dir.to_path_buf())),
            backend,
        ))
    }

    fn json(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    fn one_message(body_type: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "value": [{
                "id": "AAMkAGI2THVSAAA=",
                "conversationId": "AAQkAGI2conv",
                "subject": "Tentamen flyttad",
                "from": { "emailAddress": { "name": "Kursansvarig", "address": "kurs@kth.se" } },
                "receivedDateTime": "2026-09-24T07:15:00Z",
                "bodyPreview": "Tentamen i XX1002 flyttas",
                "isRead": false,
                "webLink": "https://outlook.office365.com/owa/?ItemID=AAMk",
                "body": { "contentType": body_type, "content": content },
            }],
        })
    }

    // -----------------------------------------------------------------------
    // The one that matters most
    // -----------------------------------------------------------------------

    /// HTML-only mail must arrive with a **non-empty** body. Graph reports
    /// `contentType: "html"` for most university mail, and a body that
    /// extracted to `""` would be scored as noise and discarded — the mail
    /// that mattered most would be the mail that disappeared.
    #[tokio::test]
    async fn an_html_only_message_arrives_with_a_readable_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(json(one_message(
                "html",
                "<html><head><style>.a{color:#fff}</style></head><body>\
                 <p>Tentamen i <b>XX1002</b> flyttas till den 3:e.</p>\
                 <div>Sal D3.<br>Ta med legitimation.</div></body></html>",
            )))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        let mail = transport.list_unread("kth", 25).await.unwrap();
        assert_eq!(mail.len(), 1);
        let message = &mail[0];

        assert!(
            !message.body.trim().is_empty(),
            "an HTML-only message must not arrive with an empty body"
        );
        assert!(
            message
                .body
                .contains("Tentamen i XX1002 flyttas till den 3:e."),
            "{:?}",
            message.body
        );
        assert!(
            message.body.contains("Ta med legitimation."),
            "{:?}",
            message.body
        );
        assert!(!message.body.contains('<'), "{:?}", message.body);
        assert!(!message.body.contains("color:#fff"), "{:?}", message.body);
        assert_eq!(message.from, "Kursansvarig <kurs@kth.se>");
        assert_eq!(message.subject, "Tentamen flyttad");
        assert_eq!(message.account, "kth");
        assert!(!message.is_read);
    }

    // -----------------------------------------------------------------------
    // The request this client actually sends
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_poll_asks_the_inbox_for_unread_mail_with_the_body_selected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(json(serde_json::json!({ "value": [] })))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();
        transport.list_unread("kth", 25).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "a poll is one request, not one per message"
        );
        let query = requests[0].url.query().unwrap_or_default();

        assert!(query.contains("$filter=isRead"), "{query}");
        assert!(query.contains("$top=25"), "{query}");
        assert!(
            query.contains("$select=") && query.contains("body"),
            "the body must be selected, or every message arrives empty: {query}"
        );
        assert!(
            !query.contains("$orderby"),
            "ordering is done in-process; $orderby beside $filter can be refused as an \
             InefficientFilter: {query}"
        );
        assert!(
            requests[0].url.path().contains("mailFolders/inbox"),
            "a poll must not span Junk and Deleted Items: {}",
            requests[0].url.path()
        );
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {ACCESS_TOKEN}").as_str())
        );
    }

    #[tokio::test]
    async fn messages_come_back_newest_first_whatever_order_graph_sent_them_in() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(json(serde_json::json!({
                "value": [
                    { "id": "old", "receivedDateTime": "2026-09-20T08:00:00Z" },
                    { "id": "new", "receivedDateTime": "2026-09-24T08:00:00Z" },
                    { "id": "mid", "receivedDateTime": "2026-09-22T08:00:00Z" },
                ],
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        let ids: Vec<String> = transport
            .list_unread("kth", 25)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    // -----------------------------------------------------------------------
    // The 401 retry
    // -----------------------------------------------------------------------

    /// A 401 on an unexpired token forces **one** refresh and **one** retry —
    /// asserted by counting requests, because "it worked" would also be true
    /// of an implementation that retried forever.
    #[tokio::test]
    async fn a_401_forces_exactly_one_refresh_and_one_retry() {
        let server = MockServer::start().await;
        // First call (the stale bearer) 401s; the retry carries the refreshed
        // one and succeeds. Matching on the header is what proves the retry
        // used the *new* token rather than re-sending the rejected one.
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .and(move |req: &Request| {
                req.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some(format!("Bearer {ACCESS_TOKEN}").as_str())
            })
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":"InvalidAuthenticationToken","message":"Access token has expired."}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .and(move |req: &Request| {
                req.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    == Some(format!("Bearer {REFRESHED_TOKEN}").as_str())
            })
            .respond_with(json(one_message("text", "after the refresh")))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let backend = Arc::new(CountingBackend(AtomicUsize::new(0)));
        let transport =
            GraphTransport::new(&server.uri(), auth_with(tmp.path(), backend.clone())).unwrap();

        let mail = transport.list_unread("kth", 25).await.unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].body, "after the refresh");

        assert_eq!(
            backend.0.load(Ordering::SeqCst),
            1,
            "exactly one refresh: a dead grant answers the same way forever, and an \
             unbounded retry against one is how an integration gets throttled"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one original request and one retry, no more"
        );
    }

    /// A 401 that survives the refresh must not loop: two requests, then an
    /// error.
    #[tokio::test]
    async fn a_401_that_survives_the_refresh_gives_up_rather_than_looping() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"error":{"code":"InvalidAuthenticationToken"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let backend = Arc::new(CountingBackend(AtomicUsize::new(0)));
        let transport =
            GraphTransport::new(&server.uri(), auth_with(tmp.path(), backend.clone())).unwrap();

        let err = transport.list_unread("kth", 25).await.unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
        assert_eq!(backend.0.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Failure reporting
    // -----------------------------------------------------------------------

    /// The likeliest real failure: KTH's tenant refuses the application. The
    /// error has to point at the README section about it, not at the network.
    #[tokio::test]
    async fn a_403_points_at_the_consent_question() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_raw(
                r#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        let err = format!("{:#}", transport.list_unread("kth", 25).await.unwrap_err());
        assert!(err.contains("Mail.Read"), "{err}");
        assert!(err.contains("README"), "{err}");
    }

    #[tokio::test]
    async fn a_429_says_it_is_throttling_and_that_nothing_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "120")
                    .set_body_raw(
                        r#"{"error":{"code":"ApplicationThrottled"}}"#,
                        "application/json",
                    ),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        let err = format!("{:#}", transport.list_unread("kth", 25).await.unwrap_err());
        assert!(err.contains("throttling"), "{err}");
        assert!(err.contains("120s"), "{err}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a throttled endpoint must not be hammered inside one poll"
        );
    }

    /// A 200 carrying an HTML sign-in page must fail loudly rather than
    /// deserialise into an empty list — an empty list is indistinguishable
    /// from "no unread mail", which is how a broken connector looks healthy.
    #[tokio::test]
    async fn a_200_that_is_not_json_is_refused_before_deserialising() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/me/mailFolders/inbox/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body>Sign in</body></html>", "text/html"),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        let err = format!("{:#}", transport.list_unread("kth", 25).await.unwrap_err());
        assert!(err.contains("not JSON"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Ids and URLs
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_message_id_that_could_walk_the_url_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let transport = GraphTransport::new(
            &server.uri(),
            auth_with(tmp.path(), Arc::new(UnusedBackend)),
        )
        .unwrap();

        for bad in [
            "../../users/someone@kth.se/messages/x",
            "..",
            "a/b",
            "a?b",
            "a b",
            "",
        ] {
            assert!(
                transport.get("kth", bad).await.is_err(),
                "get({bad:?}) must be refused"
            );
        }
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a refused id must not reach the network at all"
        );
    }

    #[test]
    fn a_base_url_with_credentials_or_a_strange_scheme_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth = auth_with(tmp.path(), Arc::new(UnusedBackend));
        assert!(GraphTransport::new("https://user:pw@graph.example/v1.0", auth.clone()).is_err());
        assert!(GraphTransport::new("file:///etc/passwd", auth.clone()).is_err());
        assert!(GraphTransport::new("https://graph.example/v1.0", auth).is_ok());
    }

    // -----------------------------------------------------------------------
    // Normalisation
    // -----------------------------------------------------------------------

    #[test]
    fn a_message_missing_every_optional_field_normalises_rather_than_panicking() {
        let mail = normalize(&RawMessage::default(), "kth");
        assert_eq!(mail.from, "(unknown sender)");
        assert_eq!(mail.subject, "(no subject)");
        assert_eq!(mail.body, "");
        assert_eq!(mail.received_at, DateTime::<Utc>::UNIX_EPOCH);
        assert_eq!(mail.account, "kth");
    }

    /// The epoch, not `now`: a corrupt timestamp must sink to the bottom of a
    /// recency-ordered queue, not displace the mail that is genuinely urgent.
    #[test]
    fn an_unparseable_timestamp_sinks_to_the_epoch() {
        let raw = RawMessage {
            received_date_time: Some("not a date".to_string()),
            ..RawMessage::default()
        };
        assert_eq!(
            normalize(&raw, "kth").received_at,
            DateTime::<Utc>::UNIX_EPOCH
        );
    }

    #[test]
    fn a_timestamp_without_an_offset_is_read_as_utc_rather_than_dropped() {
        let raw = RawMessage {
            received_date_time: Some("2026-09-24T07:15:00".to_string()),
            ..RawMessage::default()
        };
        assert_eq!(
            normalize(&raw, "kth").received_at.to_rfc3339(),
            "2026-09-24T07:15:00+00:00"
        );
    }

    #[test]
    fn a_sender_with_only_an_address_or_only_a_name_still_reads() {
        let only_address = Recipient {
            email_address: Some(EmailAddress {
                name: None,
                address: Some("noreply@kth.se".into()),
            }),
        };
        let raw = RawMessage {
            from: Some(only_address),
            ..RawMessage::default()
        };
        assert_eq!(normalize(&raw, "kth").from, "noreply@kth.se");

        // Outlook commonly repeats the address as the display name.
        let duplicated = Recipient {
            email_address: Some(EmailAddress {
                name: Some("noreply@kth.se".into()),
                address: Some("noreply@kth.se".into()),
            }),
        };
        let raw = RawMessage {
            from: Some(duplicated),
            ..RawMessage::default()
        };
        assert_eq!(normalize(&raw, "kth").from, "noreply@kth.se");
    }
}
