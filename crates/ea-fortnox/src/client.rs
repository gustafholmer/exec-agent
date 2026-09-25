//! The Fortnox REST client.
//!
//! One bearer-authenticated HTTP client over `https://api.fortnox.se/3/`,
//! taking its token from a [`TokenManager`] and returning `serde_json::Value`
//! — the same shape the TypeScript this is translated from returns, because
//! Fortnox's envelopes differ per endpoint and the typed layers sit above
//! this one.
//!
//! # The 401 budget
//!
//! Exactly **one** forced refresh and **one** retry. A second 401 is an
//! error, and no third request is made.
//!
//! A token can be dead long before the expiry it was issued with — the owner
//! can revoke the app in Fortnox's UI, and the refresh token lapses after 45
//! days of disuse — so a 401 has to be able to provoke a refresh rather than
//! merely being reported. But a grant that has actually been revoked answers
//! 401 to the refreshed token too, and a client that keeps refreshing and
//! retrying then issues an unbounded stream of requests at a company API that
//! rate-limits. One retry, then the error.
//!
//! # 429 and 5xx
//!
//! Both surface as [`FortnoxError::Api`] carrying the status, immediately.
//! The upstream TypeScript sleeps and retries a 429 up to three times inside
//! this method. This client does not sleep: a client that sleeps inside a tool
//! call hides the rate limit from the thing whose job it is to react to it,
//! and it burns the daemon's own call deadline while it waits. When Fortnox
//! sends a `Retry-After`, its value is carried into the [`FortnoxError::Api`]
//! body so a caller that *does* back off has the number Fortnox asked for.
//!
//! **What actually happens to a 429 today, and the gap it leaves.** Nothing
//! retries. `crates/ea-daemon/src/executor.rs` (`Executor::run`, called from
//! `execute_approved`; the `Err(err)` arm around `mark_failed`) marks the
//! action **failed** on any error from the connector — no retry, no backoff,
//! no requeue — and closes the run row with `"error"`. So a 429 on
//! `record_expense` or `attach_receipt` is terminal: the expense is not
//! booked, the action sits in a failed state, and a person has to approve it
//! again. The earlier claim in this module that "the caller owns backoff"
//! described an intended design, not the code; no caller owns it yet.
//!
//! Whoever builds that retry policy — it belongs in the scheduler or the
//! executor, not here — wants: the status from [`FortnoxError::status`], the
//! `Retry-After` now in the error body, and a rule that a 429 or a 5xx is
//! retryable while a 4xx is not. Deliberately not built in this task: a
//! connector-level retry would be invisible to the daemon's budgets and
//! deadlines, which is the mistake the upstream TypeScript makes.
//!
//! # Credentials
//!
//! Same standard as [`crate::auth`], `ea-google`'s calendar client and
//! `ea-canvas`'s: no redirects followed, a content-type gate before
//! deserialising, `.without_url()` on every `reqwest` error, a hand-written
//! `Debug`, no `unwrap`/`expect`/`panic!` outside tests, and the access token
//! only ever in an `Authorization` header — never in a URL, an error or a log.
//!
//! Two related rules, because this client also carries customer accounting
//! data and receipt bytes:
//!
//! - A **successful** response body is never quoted into an error. A 2xx that
//!   is not JSON, or is JSON this build cannot read, is reported by its byte
//!   length and content type only. It is not a credential, but it is very
//!   likely a company's ledger, and it would land in a log line. Error-status
//!   bodies are still quoted (truncated) by [`parse_fortnox_error`]: a Fortnox
//!   error response carries a diagnostic, not the books.
//! - A [`FormPart`]'s **value** never reaches an error message. Only its
//!   `name` does. The bytes are a receipt scan or an invoice PDF.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;

use crate::auth::TokenManager;
use crate::errors::{parse_fortnox_error, FortnoxError};

/// Fortnox's REST base. The trailing slash matters: [`Url::join`] replaces the
/// last path segment without it.
pub const FORTNOX_API_BASE: &str = "https://api.fortnox.se/3/";

/// Per-request deadline. The daemon has its own, larger one around the whole
/// tool call; this bounds a single HTTP round trip.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Rows per page for [`FortnoxClient::get_all`]. Fortnox's documented maximum;
/// its default is 100, which is what makes a plain `get` on a list endpoint
/// quietly return a partial answer.
const PAGE_SIZE: &str = "500";

/// How many pages to follow before calling it a loop.
const MAX_PAGES: usize = 500;

/// A query string, as pairs. Numbers are the caller's to stringify.
pub type Query<'a> = &'a [(&'a str, &'a str)];

/// One part of a `multipart/form-data` post — a description, not a body.
///
/// Fortnox's `inbox` endpoint (the only upload path this client has) takes a
/// file part. `reqwest::multipart::Form` is consumed by the send and is
/// neither `Clone` nor reusable, so the retry in [`FortnoxClient::request`]
/// cannot re-send one. A `FormPart` is plain borrowed data and the form is
/// **rebuilt** from it for each attempt.
///
/// `bytes` is a customer's receipt or invoice. It never reaches an error
/// message, a log or a `Debug`; only [`FormPart::name`] does.
#[derive(Clone, Copy)]
pub struct FormPart<'a> {
    /// The form field name, e.g. `file`. Safe to name in an error.
    pub name: &'a str,
    /// The part's bytes. Never rendered anywhere.
    pub bytes: &'a [u8],
    /// The `filename` in the part's `Content-Disposition`, when it is a file.
    pub filename: Option<&'a str>,
    /// The part's own content type. `None` leaves it to the server.
    pub mime: Option<&'a str>,
}

impl fmt::Debug for FormPart<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The bytes and the filename are the customer's document; the length
        // is the most that may be said about them.
        f.debug_struct("FormPart")
            .field("name", &self.name)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("filename", &self.filename.map(|_| "<redacted>"))
            .field("mime", &self.mime)
            .finish()
    }
}

/// What to put in a request's body, described rather than built.
///
/// The 401 retry sends the request a second time, so the body has to be
/// producible twice. A `serde_json::Value` could simply be borrowed twice; a
/// multipart form cannot. Describing the body and building it inside
/// [`FortnoxClient::send`] makes both cases uniform, and makes the retry's
/// body provably a fresh one rather than a reused handle.
///
/// An enum rather than a `dyn Fn() -> Body` closure: the description is
/// inspectable, it costs no lifetime gymnastics across an `async fn`, and it
/// cannot return something different on the second call the way a closure
/// over mutable state could.
#[derive(Clone, Copy)]
enum BodySpec<'a> {
    /// No body at all — a `GET`.
    None,
    /// A JSON body, with `Content-Type: application/json`.
    Json(&'a serde_json::Value),
    /// A `multipart/form-data` body, rebuilt per attempt. `reqwest` sets the
    /// content type and its boundary.
    Form(&'a [FormPart<'a>]),
}

/// The Fortnox REST client.
pub struct FortnoxClient {
    http: reqwest::Client,
    /// Always ends in `/`.
    base: Url,
    tokens: Arc<TokenManager>,
}

impl fmt::Debug for FortnoxClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The base URL is Fortnox's public host (or a test server on
        // loopback); the token manager is deliberately not printed.
        f.debug_struct("FortnoxClient")
            .field("base", &self.base.as_str())
            .finish()
    }
}

impl FortnoxClient {
    /// Against the real Fortnox API.
    pub fn new(tokens: Arc<TokenManager>) -> Result<Self, FortnoxError> {
        Self::with_base_url(tokens, FORTNOX_API_BASE)
    }

    /// Against an explicit base URL. The seam the tests point at `wiremock`.
    pub fn with_base_url(tokens: Arc<TokenManager>, base_url: &str) -> Result<Self, FortnoxError> {
        Self::build(tokens, base_url, HTTP_TIMEOUT)
    }

    fn build(
        tokens: Arc<TokenManager>,
        base_url: &str,
        timeout: Duration,
    ) -> Result<Self, FortnoxError> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/")).map_err(|err| {
            FortnoxError::Transport(format!(
                "the Fortnox base URL {base_url:?} is not a URL ({err})"
            ))
        })?;

        if !matches!(base.scheme(), "http" | "https") {
            return Err(FortnoxError::Transport(format!(
                "the Fortnox base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            )));
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            return Err(FortnoxError::Transport(
                "the Fortnox base URL must not contain a username or password".to_string(),
            ));
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            // Not optional; see `ea_core::http` for the 403 that proved it.
            .user_agent(ea_core::http::USER_AGENT)
            .timeout(timeout)
            // No redirects, ever: `reqwest` strips `Authorization` when the
            // host or port changes but not when the *scheme* does, so a
            // same-host https→http redirect would put the bearer token on the
            // wire in clear text. This API has no legitimate redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| {
                FortnoxError::Transport(format!(
                    "building the HTTPS client for Fortnox failed: {}",
                    err.without_url()
                ))
            })?;

        Ok(Self { http, base, tokens })
    }

    /// The base URL requests are resolved against.
    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// `GET {base}{path}?{query}`, decoded as JSON.
    pub async fn get(
        &self,
        path: &str,
        query: Query<'_>,
    ) -> Result<serde_json::Value, FortnoxError> {
        self.request(reqwest::Method::GET, path, query, BodySpec::None)
            .await
    }

    /// `POST {base}{path}?{query}` with a JSON body, decoded as JSON.
    pub async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        query: Query<'_>,
    ) -> Result<serde_json::Value, FortnoxError> {
        self.request(reqwest::Method::POST, path, query, BodySpec::Json(body))
            .await
    }

    /// `POST {base}{path}?{query}` with a `multipart/form-data` body.
    ///
    /// Fortnox's `inbox` endpoint takes an uploaded file this way and answers
    /// with the created `File`; that is the first half of attaching a receipt
    /// to a voucher. The form is built from `parts` **once per attempt**, so
    /// the 401 retry sends a fresh, complete body rather than an already
    /// consumed one — see [`BodySpec`].
    pub async fn post_form(
        &self,
        path: &str,
        parts: &[FormPart<'_>],
        query: Query<'_>,
    ) -> Result<serde_json::Value, FortnoxError> {
        self.request(reqwest::Method::POST, path, query, BodySpec::Form(parts))
            .await
    }

    /// Every row of a paginated list endpoint, across all pages.
    ///
    /// Fortnox caps list responses (100 by default, 500 at most), so a plain
    /// [`FortnoxClient::get`] on `accounts` or `vouchers` silently returns
    /// page one — and a VAT return computed from page one is wrong without
    /// ever looking wrong. This follows `MetaInformation.@TotalPages` at the
    /// maximum page size; a response without `MetaInformation` is one page.
    pub async fn get_all(
        &self,
        path: &str,
        list_key: &str,
        query: Query<'_>,
    ) -> Result<Vec<serde_json::Value>, FortnoxError> {
        let mut rows = Vec::new();
        let mut total_pages: usize = 1;

        for page in 1..=MAX_PAGES {
            if page > total_pages {
                break;
            }
            let page_number = page.to_string();
            let mut paged: Vec<(&str, &str)> = query.to_vec();
            paged.push(("limit", PAGE_SIZE));
            paged.push(("page", page_number.as_str()));

            let response = self
                .request(reqwest::Method::GET, path, &paged, BodySpec::None)
                .await?;

            if let Some(serde_json::Value::Array(items)) = response.get(list_key) {
                rows.extend(items.iter().cloned());
            }

            // A response with no `MetaInformation`, or one whose
            // `@TotalPages` is not a positive number, is a single page.
            // Believing a garbage value here is how a client ends up walking
            // four billion pages.
            total_pages = response
                .get("MetaInformation")
                .and_then(|meta| meta.get("@TotalPages"))
                .and_then(total_pages_of)
                .filter(|reported| *reported >= 1)
                .unwrap_or(1);

            if page >= total_pages {
                return Ok(rows);
            }
        }

        Err(FortnoxError::Api {
            status: 200,
            body: format!(
                "GET {path} kept reporting more pages after {MAX_PAGES} of them; refusing \
                 to follow MetaInformation.@TotalPages further"
            ),
        })
    }

    /// One request, with the 401 budget: at most one forced refresh and one
    /// retry. See the module docs for why the bound is not negotiable.
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: Query<'_>,
        body: BodySpec<'_>,
    ) -> Result<serde_json::Value, FortnoxError> {
        let url = self.url(path, query)?;

        // `whence` is the URL with its query stripped: enough to say which
        // endpoint failed, short enough for a log line, and — since the token
        // never travels in a query string — never a credential.
        let mut whence = url.clone();
        whence.set_query(None);

        let token = self.tokens.access_token(false).await?;
        let response = self.send(&method, url.clone(), &token, body).await?;

        // Exactly one retry. Fortnox can retire an access token long before
        // its stated expiry, so a 401 has to be able to provoke a refresh —
        // but a second 401 means the *fresh* token was refused too, and
        // retrying again would only hammer a revoked grant.
        //
        // An `if`, never a `while`: rewriting this as a loop makes the
        // terminal-401 test hang rather than fail, which is the production
        // failure mode in miniature.
        let retried = response.status() == reqwest::StatusCode::UNAUTHORIZED;
        let response = if retried {
            let renewed = self.tokens.access_token(true).await.map_err(|err| {
                let detail = err.to_string();
                FortnoxError::Auth(format!(
                    "{method} {whence} was refused with HTTP 401 and the forced token \
                     refresh failed too: {detail}"
                ))
            })?;
            // `body` is a description, so this builds a *new* body. That is
            // the whole reason it is a description: a multipart form is
            // consumed by the send above and could not be sent again.
            self.send(&method, url, &renewed, body).await?
        } else {
            response
        };

        let result = self.read(response, &method, &whence).await;

        // A 401 can only reach `read` after the retry above, so this is the
        // second one: the refreshed token was refused too. That is not a
        // transient failure, it is a grant that no longer exists, and the only
        // fix is a person in a browser — so the message has to say so and name
        // the command. (The status stays 401 and the variant stays `Api`: this
        // is still Fortnox answering.)
        if retried {
            return result.map_err(|err| match err {
                FortnoxError::Api { status: 401, body } => FortnoxError::Api {
                    status: 401,
                    body: format!(
                        "{body} — the token was refreshed once and refused again, so the \
                         Fortnox grant is gone (revoked in Fortnox's UI, or the refresh \
                         token lapsed). Re-authorise with `{}`.",
                        crate::auth::AUTHORIZE_COMMAND
                    ),
                },
                other => other,
            });
        }
        result
    }

    fn url(&self, path: &str, query: Query<'_>) -> Result<Url, FortnoxError> {
        let mut url = self
            .base
            .join(path.trim_start_matches('/'))
            .map_err(|err| {
                FortnoxError::Transport(format!(
                    "the Fortnox path {path:?} is not a valid URL fragment ({err})"
                ))
            })?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// One bearer-authenticated request, with the URL kept out of the error.
    ///
    /// The body is built here, from its description, on every call — so the
    /// 401 retry gets a fresh one.
    async fn send(
        &self,
        method: &reqwest::Method,
        url: Url,
        token: &str,
        body: BodySpec<'_>,
    ) -> Result<reqwest::Response, FortnoxError> {
        let request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
        let request = match body {
            BodySpec::None => request,
            BodySpec::Json(value) => request.json(value),
            // No explicit `Content-Type`: `reqwest` sets
            // `multipart/form-data` together with the boundary it generated,
            // and setting it by hand would strip the boundary.
            BodySpec::Form(parts) => request.multipart(build_form(parts)?),
        };
        request.send().await.map_err(|err| {
            FortnoxError::Transport(format!("{method} to Fortnox failed: {}", err.without_url()))
        })
    }

    async fn read(
        &self,
        response: reqwest::Response,
        method: &reqwest::Method,
        whence: &Url,
    ) -> Result<serde_json::Value, FortnoxError> {
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Read before the body is consumed. Kept for the error path: without
        // it, a caller that wants to honour Fortnox's own backoff has nothing
        // to honour.
        let retry_after = retry_after_of(response.headers());

        let body = response.text().await.map_err(|err| {
            FortnoxError::Transport(format!(
                "reading the reply to {method} {whence} failed: {}",
                err.without_url()
            ))
        })?;

        if !status.is_success() {
            return Err(with_retry_after(
                parse_fortnox_error(status.as_u16(), &body),
                retry_after,
            ));
        }

        // A 204, or a 200 with nothing in it. Fortnox answers some writes
        // this way and there is nothing to decode.
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }

        // A 200 that is not JSON is the failure mode that hurts: a login or
        // consent page arrives with a cheerful 200 and, without this check,
        // either fails to deserialise with an opaque message or reads as an
        // empty list of rows.
        //
        // The body itself is NOT quoted. A 2xx from this API is a company's
        // accounting data — vouchers, suppliers, account balances — and this
        // message goes to a log. The content type and the length are enough to
        // tell an HTML login page from a truncated JSON reply, and neither can
        // carry a row of someone's ledger.
        if !is_json(&content_type) {
            return Err(FortnoxError::Api {
                status: status.as_u16(),
                body: format!(
                    "{method} {whence} answered with content-type {content_type:?}, which is \
                     not JSON ({} bytes, not quoted: a success body is customer data)",
                    body.len()
                ),
            });
        }

        serde_json::from_str(&body).map_err(|err| FortnoxError::Api {
            status: status.as_u16(),
            body: format!(
                "{method} {whence} answered JSON this build cannot read: {} at line {}, \
                 column {} of {} bytes of {content_type:?} (the body is not quoted: a \
                 success body is customer data)",
                classify(&err),
                err.line(),
                err.column(),
                body.len()
            ),
        })
    }
}

/// Build the multipart body for one attempt.
///
/// The only way this fails is a `mime` the caller made up. The error names the
/// part and the mime — never the filename and never the bytes.
fn build_form(parts: &[FormPart<'_>]) -> Result<reqwest::multipart::Form, FortnoxError> {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        // `Part::bytes` wants owned data and the body is consumed by the send,
        // so each attempt copies. Receipts are kilobytes; correctness under
        // retry is worth the copy.
        let mut built = reqwest::multipart::Part::bytes(part.bytes.to_vec());
        if let Some(filename) = part.filename {
            built = built.file_name(filename.to_string());
        }
        if let Some(mime) = part.mime {
            built = built.mime_str(mime).map_err(|err| {
                FortnoxError::Transport(format!(
                    "the multipart part {:?} was given the content type {mime:?}, which is not \
                     a media type ({})",
                    part.name,
                    err.without_url()
                ))
            })?;
        }
        form = form.part(part.name.to_string(), built);
    }
    Ok(form)
}

/// `serde_json`'s *classification* of a parse failure — "syntax", "data",
/// "eof", "io" — without its message, which can quote the input.
fn classify(err: &serde_json::Error) -> &'static str {
    match err.classify() {
        serde_json::error::Category::Io => "an I/O error",
        serde_json::error::Category::Syntax => "a syntax error",
        serde_json::error::Category::Data => "a value of the wrong shape",
        serde_json::error::Category::Eof => "an unexpected end of input",
    }
}

/// `Retry-After`, as Fortnox sent it, if it is short and printable.
///
/// Both legal forms are kept verbatim (a count of seconds, or an HTTP date):
/// parsing here would silently drop the one this client did not anticipate.
/// The value is server-supplied, so it is length- and character-bounded before
/// it is allowed into a message.
fn retry_after_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let printable = |c: char| c.is_ascii_graphic() || c == ' ';
    if raw.is_empty() || raw.len() > 64 || !raw.chars().all(printable) {
        return None;
    }
    Some(raw.to_string())
}

/// Carry a `Retry-After` into an [`FortnoxError::Api`] body.
///
/// Nothing retries a 429 today (see the module docs), so this is the only
/// place the number survives at all; a caller that later grows a backoff
/// policy needs it, and re-reading it from a consumed response is impossible.
fn with_retry_after(err: FortnoxError, retry_after: Option<String>) -> FortnoxError {
    match (err, retry_after) {
        (FortnoxError::Api { status, body }, Some(retry_after)) => FortnoxError::Api {
            status,
            body: format!("{body} (Retry-After: {retry_after})"),
        },
        (err, _) => err,
    }
}

/// `@TotalPages` arrives as a JSON number from Fortnox and as a string from
/// some of its older endpoints. Both are read; anything else is ignored.
fn total_pages_of(value: &serde_json::Value) -> Option<usize> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().and_then(|n| usize::try_from(n).ok()),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::Ordering;

    use chrono::{DateTime, Utc};
    use wiremock::matchers::{header, method, path as path_matcher, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::test_support::{CountingRefresh, MemStore};
    use crate::auth::StoredTokens;

    /// A token that will not expire during the test run, so nothing refreshes
    /// unless a 401 forces it.
    const LIVE_TOKEN: &str = "liveToken";
    /// What `CountingRefresh` hands back.
    const RENEWED_TOKEN: &str = "newAccess";

    fn far_future() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    fn live_tokens() -> StoredTokens {
        StoredTokens {
            access_token: LIVE_TOKEN.to_string(),
            refresh_token: "storedRefresh".to_string(),
            expires_at: far_future(),
            scope: "bookkeeping".to_string(),
        }
    }

    /// A manager whose stored token is live, plus the refresh backend so the
    /// test can count forced refreshes.
    fn manager() -> (Arc<TokenManager>, Arc<CountingRefresh>, Arc<MemStore>) {
        let store = MemStore::new(Some(live_tokens()));
        let refresh = CountingRefresh::new();
        (
            Arc::new(TokenManager::new(store.clone(), refresh.clone())),
            refresh,
            store,
        )
    }

    fn client_for(server: &MockServer) -> (FortnoxClient, Arc<CountingRefresh>, Arc<MemStore>) {
        let (tokens, refresh, store) = manager();
        (
            FortnoxClient::with_base_url(tokens, &server.uri()).expect("client"),
            refresh,
            store,
        )
    }

    fn json(body: serde_json::Value, status: u16) -> ResponseTemplate {
        ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/json")
    }

    // -----------------------------------------------------------------------
    // get — translated from `fortnox/client.test.ts`
    // -----------------------------------------------------------------------

    /// `reqwest` sends no `User-Agent` at all by default, which is what got
    /// the Canvas connector a 403 from the live API. Fortnox tolerates an
    /// anonymous client today; it need not keep doing so.
    #[tokio::test]
    async fn every_request_identifies_the_client_by_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/vouchers"))
            .and(header("user-agent", ea_core::http::USER_AGENT))
            .respond_with(json(serde_json::json!({ "Vouchers": [] }), 200))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        client
            .get("vouchers", &[])
            .await
            .expect("the User-Agent header must be present");
    }

    #[tokio::test]
    async fn get_adds_bearer_auth_the_base_url_and_the_query_and_returns_parsed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/vouchers"))
            .and(query_param("financialyear", "3"))
            .respond_with(json(
                serde_json::json!({ "Voucher": { "VoucherNumber": 5 } }),
                200,
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let res = client
            .get("vouchers", &[("financialyear", "3")])
            .await
            .expect("a 200 decodes");

        assert_eq!(
            res,
            serde_json::json!({ "Voucher": { "VoucherNumber": 5 } })
        );

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {LIVE_TOKEN}").as_str())
        );
        assert_eq!(
            requests[0]
                .headers
                .get("accept")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(requests[0].url.path(), "/vouchers");
        assert_eq!(requests[0].url.query(), Some("financialyear=3"));
    }

    #[tokio::test]
    async fn a_fortnox_error_body_is_mapped_to_its_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json(
                serde_json::json!({ "ErrorInformation": { "message": "Nope", "code": 2000123 } }),
                400,
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("vouchers", &[]).await.expect_err("a 400");

        assert_eq!(err.status(), Some(400));
        assert!(err.to_string().contains("Nope"), "{err}");
        assert!(err.to_string().contains("2000123"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Review Focus #5: the 401 budget
    // -----------------------------------------------------------------------

    /// One forced refresh, one retry, and the retry carries the NEW token.
    #[tokio::test]
    async fn a_401_forces_exactly_one_refresh_and_one_retry_then_succeeds() {
        let server = MockServer::start().await;
        // Matching on the header is what proves the retry used the new token.
        Mock::given(method("GET"))
            .and(header(
                "authorization",
                format!("Bearer {LIVE_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(401).set_body_raw("", "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(header(
                "authorization",
                format!("Bearer {RENEWED_TOKEN}").as_str(),
            ))
            .respond_with(json(serde_json::json!({ "ok": true }), 200))
            .mount(&server)
            .await;

        let (client, refresh, store) = client_for(&server);
        let res = client
            .get("accounts", &[])
            .await
            .expect("the retry after the forced refresh succeeds");

        assert_eq!(res, serde_json::json!({ "ok": true }));
        assert_eq!(refresh.calls(), 1, "exactly one forced refresh");
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            2,
            "the original request and exactly one retry"
        );
        // The rotation still had to be persisted on this path.
        assert_eq!(
            store.current().expect("stored").refresh_token,
            "rotatedRefresh"
        );
    }

    /// **Review Focus #5.** A second 401 is terminal: no second refresh, no
    /// third request. The assertion that matters is the request count — an
    /// unbounded retry against a revoked grant hammers Fortnox and gets the
    /// integration rate-limited.
    #[tokio::test]
    async fn a_second_401_is_terminal_and_makes_no_third_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"ErrorInformation":{"message":"unauthorized","code":2000663}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let (client, refresh, _store) = client_for(&server);
        let err = client
            .get("accounts", &[])
            .await
            .expect_err("a token refused twice is an error");

        assert_eq!(err.status(), Some(401));
        assert_eq!(refresh.calls(), 1, "exactly one forced refresh, not a loop");
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            2,
            "exactly two requests: the original and one retry"
        );
        // A fresh token refused too is a dead grant, and only a person can fix
        // that. The error has to name the command they must run, or the owner
        // sees "HTTP 401" and has nothing to do with it.
        let rendered = err.to_string();
        assert!(rendered.contains("unauthorized"), "{rendered}");
        assert!(
            rendered.contains(crate::auth::AUTHORIZE_COMMAND),
            "the terminal 401 must name the authorize command: {rendered}"
        );
    }

    /// The hint belongs to the *second* 401 only. A 403, or a 401 the retry
    /// recovered from, must not tell the owner to re-authorise.
    #[tokio::test]
    async fn a_403_is_not_dressed_up_as_a_dead_grant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_raw("forbidden", "text/plain"))
            .mount(&server)
            .await;

        let (client, refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 403");

        assert_eq!(err.status(), Some(403));
        assert_eq!(refresh.calls(), 0, "a 403 is not an expired token");
        assert!(
            !err.to_string().contains(crate::auth::AUTHORIZE_COMMAND),
            "{err}"
        );
    }

    /// The 401 path must not mask a broken grant as an API error: when the
    /// forced refresh itself fails, the message has to send the reader to the
    /// authorize command.
    #[tokio::test]
    async fn a_401_whose_forced_refresh_fails_reports_the_grant_not_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw("", "application/json"))
            .mount(&server)
            .await;

        let store = MemStore::new(Some(live_tokens()));
        let refresh = CountingRefresh::failing();
        let tokens = Arc::new(TokenManager::new(store, refresh.clone()));
        let client = FortnoxClient::with_base_url(tokens, &server.uri()).expect("client");

        let err = client.get("accounts", &[]).await.expect_err("dead grant");

        assert!(matches!(err, FortnoxError::Auth(_)), "{err:?}");
        assert!(
            err.to_string().contains(crate::auth::AUTHORIZE_COMMAND),
            "{err}"
        );
        assert_eq!(refresh.calls(), 1);
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            1,
            "no retry when there is no new token to retry with"
        );
    }

    // -----------------------------------------------------------------------
    // Statuses and transport
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_429_is_an_api_error_carrying_the_status_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_raw("slow down", "text/plain"))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 429");

        assert_eq!(err.status(), Some(429));
        assert!(err.to_string().contains("slow down"), "{err}");
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            1,
            "a rate limit is reported, not retried into"
        );
    }

    /// Nothing retries a 429 in this build, so the error is the only place
    /// Fortnox's own backoff can survive. Dropping the header means no future
    /// retry policy can honour it — it would have to guess.
    #[tokio::test]
    async fn a_429_carries_fortnoxs_retry_after_into_the_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "42")
                    .set_body_raw("slow down", "text/plain"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 429");

        assert_eq!(err.status(), Some(429));
        assert!(
            err.to_string().contains("Retry-After: 42"),
            "the Retry-After Fortnox sent must reach the caller: {err}"
        );
    }

    /// The header is server-supplied. A megabyte of it must not become a log
    /// line, and a value we cannot read is dropped rather than half-quoted.
    #[tokio::test]
    async fn an_absurd_retry_after_is_dropped_rather_than_carried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "9".repeat(5_000).as_str())
                    .set_body_raw("slow down", "text/plain"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 429");

        assert_eq!(err.status(), Some(429));
        assert!(!err.to_string().contains("Retry-After"), "{err}");
        assert!(err.to_string().len() < 500, "{}", err.to_string().len());
    }

    /// A `Retry-After` on a 503 is just as real as one on a 429; carrying it
    /// only for 429 would lose the one Fortnox sends during maintenance.
    #[tokio::test]
    async fn a_retry_after_on_a_503_is_carried_too() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("retry-after", "Wed, 24 Sep 2026 12:00:00 GMT")
                    .set_body_raw("maintenance", "text/plain"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 503");

        assert_eq!(err.status(), Some(503));
        assert!(
            err.to_string()
                .contains("Retry-After: Wed, 24 Sep 2026 12:00:00 GMT"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_500_is_an_api_error_carrying_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(500).set_body_raw("Internal Server Error", "text/plain"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 500");

        assert_eq!(err.status(), Some(500));
        assert!(err.to_string().contains("500"), "{err}");
    }

    /// A dropped connection is not Fortnox saying no, and must not be
    /// reported as one: the two get different responses from the daemon.
    #[tokio::test]
    async fn a_network_failure_is_transport_not_api() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let (tokens, _refresh, _store) = manager();
        let client = FortnoxClient::with_base_url(tokens, &format!("http://127.0.0.1:{port}/3/"))
            .expect("client");
        let err = client
            .get("accounts", &[])
            .await
            .expect_err("nothing listening");

        assert!(matches!(err, FortnoxError::Transport(_)), "{err:?}");
        assert_eq!(err.status(), None);
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains(LIVE_TOKEN), "{rendered}");
    }

    #[tokio::test]
    async fn a_hung_request_times_out_as_a_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                json(serde_json::json!({ "ok": true }), 200).set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let (tokens, _refresh, _store) = manager();
        let client =
            FortnoxClient::build(tokens, &server.uri(), Duration::from_millis(30)).expect("client");
        let err = client.get("accounts", &[]).await.expect_err("timed out");

        assert!(matches!(err, FortnoxError::Transport(_)), "{err:?}");
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains(LIVE_TOKEN), "{rendered}");
    }

    /// A 200 that is not JSON is the failure that hurts: a login or consent
    /// page deserialises into nothing and reads as "no rows".
    /// A 200 that is not JSON must be an error — and must say so without
    /// quoting the body. A 2xx from this API is a company's accounting data;
    /// this message goes to a log. The content type and the length are what
    /// distinguish a login page from a truncated reply, and neither can carry
    /// a row of someone's ledger.
    #[tokio::test]
    async fn an_html_200_errors_without_quoting_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html>Supplier AB owes 41 250 SEK</html>", "text/html"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("not JSON");

        assert_eq!(err.status(), Some(200));
        let rendered = err.to_string();
        let debugged = format!("{err:?}");
        assert!(rendered.contains("text/html"), "{rendered}");
        assert!(rendered.contains("40 bytes"), "{rendered}");
        for leak in ["Supplier AB", "41 250", "<html>"] {
            assert!(
                !rendered.contains(leak) && !debugged.contains(leak),
                "the success body must not be quoted, found {leak:?} in {rendered} / {debugged}"
            );
        }
    }

    /// The same rule for a 200 that *claims* to be JSON and is not: report the
    /// classification, the position and the length, never the bytes.
    #[tokio::test]
    async fn an_unreadable_json_200_errors_without_quoting_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"Supplier":"Supplier AB","Debt":41250"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("bad JSON");

        let rendered = err.to_string();
        let debugged = format!("{err:?}");
        assert!(rendered.contains("38 bytes"), "{rendered}");
        for leak in ["Supplier AB", "41250"] {
            assert!(
                !rendered.contains(leak) && !debugged.contains(leak),
                "found {leak:?} in {rendered} / {debugged}"
            );
        }
    }

    /// `reqwest`'s default policy strips `Authorization` across origins but
    /// not across a same-host https→http downgrade.
    #[tokio::test]
    async fn a_redirect_is_not_followed_and_the_token_is_not_sent_onward() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", elsewhere.uri().as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(json(serde_json::json!({ "leaked": true }), 200))
            .mount(&elsewhere)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("a 302");

        assert_eq!(err.status(), Some(302));
        assert_eq!(
            elsewhere.received_requests().await.expect("requests").len(),
            0,
            "the bearer token must not have been sent to the redirect target"
        );
    }

    #[tokio::test]
    async fn an_empty_success_body_is_null_rather_than_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let res = client.get("accounts", &[]).await.expect("204 is fine");
        assert_eq!(res, serde_json::Value::Null);
    }

    // -----------------------------------------------------------------------
    // post — translated from `fortnox/client.test.ts`
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn post_sends_a_json_body_with_a_content_type_and_returns_the_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/vouchers"))
            .respond_with(json(
                serde_json::json!({ "Voucher": { "VoucherNumber": 9 } }),
                200,
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let sent = serde_json::json!({ "Voucher": { "VoucherSeries": "A" } });
        let res = client.post("vouchers", &sent, &[]).await.expect("post");

        assert_eq!(
            res,
            serde_json::json!({ "Voucher": { "VoucherNumber": 9 } })
        );

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests[0].method.as_str(), "POST");
        assert_eq!(
            requests[0]
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].body).expect("json body"),
            sent
        );
    }

    // -----------------------------------------------------------------------
    // post_form — the Fortnox `inbox` upload (upstream `client.postForm`,
    // reached from `files.ts::uploadInboxFile`).
    // -----------------------------------------------------------------------

    /// The receipt bytes a test uploads. Recognisable in a request body and,
    /// if it ever appeared in one, in an error message.
    const RECEIPT: &[u8] = b"%PDF-1.4 receipt bytes";

    fn receipt_parts() -> [FormPart<'static>; 1] {
        [FormPart {
            name: "file",
            bytes: RECEIPT,
            filename: Some("kvitto.pdf"),
            mime: Some("application/pdf"),
        }]
    }

    /// The boundary out of a request's `content-type`, which is what tells one
    /// built form from another.
    fn boundary_of(request: &wiremock::Request) -> String {
        request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split("boundary=").nth(1))
            .expect("a multipart content-type carries a boundary")
            .to_string()
    }

    #[tokio::test]
    async fn post_form_sends_a_multipart_body_with_the_file_and_returns_the_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/inbox"))
            .and(query_param("path", "Inbox_v"))
            .respond_with(json(serde_json::json!({ "File": { "Id": "f-1" } }), 200))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let res = client
            .post_form("inbox", &receipt_parts(), &[("path", "Inbox_v")])
            .await
            .expect("the upload");

        assert_eq!(res, serde_json::json!({ "File": { "Id": "f-1" } }));

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let content_type = requests[0]
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("multipart/form-data; boundary="),
            "{content_type}"
        );
        let body = requests[0].body.clone();
        assert!(
            body.windows(RECEIPT.len()).any(|w| w == RECEIPT),
            "the file bytes must reach the wire"
        );
        let rendered = String::from_utf8_lossy(&body);
        assert!(rendered.contains(r#"name="file""#), "{rendered}");
        assert!(rendered.contains("kvitto.pdf"), "{rendered}");
        assert!(rendered.contains("application/pdf"), "{rendered}");
    }

    /// **The 401 budget, on the multipart path.** A `reqwest` form is consumed
    /// by the send and is neither `Clone` nor reusable, so the retry can only
    /// work if the body is *rebuilt*. The evidence that it was: the second
    /// request carries the whole file again, under a different boundary, with
    /// the new token — and there are exactly two requests and one refresh.
    #[tokio::test]
    async fn a_401_on_a_form_post_retries_once_with_a_freshly_built_form_and_the_new_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header(
                "authorization",
                format!("Bearer {LIVE_TOKEN}").as_str(),
            ))
            .respond_with(ResponseTemplate::new(401).set_body_raw("", "application/json"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header(
                "authorization",
                format!("Bearer {RENEWED_TOKEN}").as_str(),
            ))
            .respond_with(json(serde_json::json!({ "File": { "Id": "f-2" } }), 200))
            .mount(&server)
            .await;

        let (client, refresh, store) = client_for(&server);
        let res = client
            .post_form("inbox", &receipt_parts(), &[("path", "Inbox_v")])
            .await
            .expect("the retry after the forced refresh uploads the file");

        assert_eq!(res, serde_json::json!({ "File": { "Id": "f-2" } }));
        assert_eq!(refresh.calls(), 1, "exactly one forced refresh");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(
            requests.len(),
            2,
            "the original upload and exactly one retry"
        );

        // Both bodies are complete. A reused (already consumed) form would
        // have sent an empty or truncated body the second time.
        for (nth, request) in requests.iter().enumerate() {
            assert!(
                request.body.windows(RECEIPT.len()).any(|w| w == RECEIPT),
                "request {nth} did not carry the file bytes — the form was not rebuilt"
            );
        }
        // A fresh `Form` generates a fresh boundary, so two different
        // boundaries mean two separately built bodies rather than one handle
        // sent twice.
        assert_ne!(
            boundary_of(&requests[0]),
            boundary_of(&requests[1]),
            "the retry must build a new form, not resend the first one"
        );
        // And the rotation still had to be persisted on this path.
        assert_eq!(
            store.current().expect("stored").refresh_token,
            "rotatedRefresh"
        );
    }

    /// A second 401 on the upload path is terminal too — the bound is in
    /// `request`, so it holds for every body shape, and this pins it.
    #[tokio::test]
    async fn a_second_401_on_a_form_post_is_terminal_and_makes_no_third_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_raw("", "application/json"))
            .mount(&server)
            .await;

        let (client, refresh, _store) = client_for(&server);
        let err = client
            .post_form("inbox", &receipt_parts(), &[])
            .await
            .expect_err("a token refused twice is an error");

        assert_eq!(err.status(), Some(401));
        assert_eq!(refresh.calls(), 1, "exactly one forced refresh, not a loop");
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            2,
            "exactly two requests: the original and one retry"
        );
        assert!(
            err.to_string().contains(crate::auth::AUTHORIZE_COMMAND),
            "{err}"
        );
    }

    /// A form part's value is a customer's receipt. Only its field name may
    /// appear in an error, and nothing but a length in a `Debug`.
    #[tokio::test]
    async fn a_form_parts_bytes_and_filename_stay_out_of_errors_and_debug() {
        let server = MockServer::start().await;
        let (client, _refresh, _store) = client_for(&server);

        let part = FormPart {
            name: "file",
            bytes: RECEIPT,
            filename: Some("kvitto-4711.pdf"),
            mime: Some("not a media type"),
        };
        let err = client
            .post_form("inbox", &[part], &[])
            .await
            .expect_err("an invented media type is refused");

        let rendered = err.to_string();
        let debugged = format!("{err:?}");
        assert!(
            rendered.contains("file"),
            "the field name may be named: {rendered}"
        );
        for leak in ["kvitto-4711", "%PDF", "receipt bytes"] {
            assert!(
                !rendered.contains(leak) && !debugged.contains(leak),
                "found {leak:?} in {rendered} / {debugged}"
            );
        }
        // Nothing was sent: the body could not be built.
        assert_eq!(server.received_requests().await.expect("requests").len(), 0);

        let part_debug = format!("{part:?}");
        assert!(part_debug.contains("22 bytes"), "{part_debug}");
        for leak in ["kvitto-4711", "%PDF", "receipt bytes"] {
            assert!(!part_debug.contains(leak), "found {leak:?} in {part_debug}");
        }
    }

    // -----------------------------------------------------------------------
    // get_all — translated from `fortnox/client.test.ts`
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_all_follows_meta_information_pagination_and_concatenates_every_page() {
        let server = MockServer::start().await;
        let page = |numbers: Vec<i64>, current: i64, total: i64| {
            json(
                serde_json::json!({
                    "Accounts": numbers.iter().map(|n| serde_json::json!({ "Number": n })).collect::<Vec<_>>(),
                    "MetaInformation": { "@TotalPages": total, "@CurrentPage": current, "@TotalResources": 5 },
                }),
                200,
            )
        };
        for (numbers, page_no) in [(vec![1, 2], "1"), (vec![3, 4], "2"), (vec![5], "3")] {
            let current: i64 = page_no.parse().expect("page number");
            Mock::given(method("GET"))
                .and(query_param("page", page_no))
                .respond_with(page(numbers, current, 3))
                .mount(&server)
                .await;
        }

        let (client, _refresh, _store) = client_for(&server);
        let rows = client
            .get_all("accounts", "Accounts", &[("financialyear", "3")])
            .await
            .expect("three pages");

        assert_eq!(
            rows,
            vec![
                serde_json::json!({ "Number": 1 }),
                serde_json::json!({ "Number": 2 }),
                serde_json::json!({ "Number": 3 }),
                serde_json::json!({ "Number": 4 }),
                serde_json::json!({ "Number": 5 }),
            ]
        );

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 3);
        let first = requests[0].url.query().unwrap_or_default().to_string();
        assert!(first.contains("limit=500"), "{first}");
        assert!(first.contains("financialyear=3"), "{first}");
        assert!(first.contains("page=1"), "{first}");
        assert!(
            requests[2]
                .url
                .query()
                .unwrap_or_default()
                .contains("page=3"),
            "{:?}",
            requests[2].url.query()
        );
    }

    #[tokio::test]
    async fn get_all_stops_after_one_page_when_meta_information_says_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json(
                serde_json::json!({
                    "Accounts": [{ "Number": 1930 }],
                    "MetaInformation": { "@TotalPages": 1, "@CurrentPage": 1, "@TotalResources": 1 },
                }),
                200,
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let rows = client
            .get_all("accounts", "Accounts", &[])
            .await
            .expect("one page");

        assert_eq!(rows, vec![serde_json::json!({ "Number": 1930 })]);
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn get_all_treats_a_response_without_meta_information_as_one_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json(
                serde_json::json!({ "Invoices": [{ "DocumentNumber": 7 }] }),
                200,
            ))
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let rows = client
            .get_all("invoices", "Invoices", &[("filter", "unpaid")])
            .await
            .expect("one page");

        assert_eq!(rows, vec![serde_json::json!({ "DocumentNumber": 7 })]);
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    // -----------------------------------------------------------------------
    // Hygiene
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_client_debug_prints_no_credential() {
        let (tokens, _refresh, _store) = manager();
        let client = FortnoxClient::new(tokens).expect("client");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains(LIVE_TOKEN), "{rendered}");
        assert!(rendered.contains("api.fortnox.se"), "{rendered}");
    }

    #[tokio::test]
    async fn a_base_url_with_userinfo_is_refused() {
        let (tokens, _refresh, _store) = manager();
        let err = FortnoxClient::with_base_url(tokens, "https://user:pass@api.fortnox.se/3/")
            .expect_err("userinfo in a base URL ends up in every reqwest error");
        assert!(err.to_string().contains("username"), "{err}");
    }

    #[tokio::test]
    async fn a_successful_get_does_not_touch_the_refresh_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json(serde_json::json!({ "ok": true }), 200))
            .mount(&server)
            .await;

        let (client, refresh, store) = client_for(&server);
        client.get("accounts", &[]).await.expect("200");

        assert_eq!(refresh.calls(), 0);
        assert_eq!(store.saves.load(Ordering::SeqCst), 0);
    }
}
