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
//! this method; here the caller (the daemon's scheduler, which already owns
//! backoff, budgets and a clock) decides. A client that sleeps inside a tool
//! call hides the rate limit from the thing whose job it is to react to it.
//!
//! # Credentials
//!
//! Same standard as [`crate::auth`], `ea-google`'s calendar client and
//! `ea-canvas`'s: no redirects followed, a content-type gate before
//! deserialising, `.without_url()` on every `reqwest` error, a hand-written
//! `Debug`, no `unwrap`/`expect`/`panic!` outside tests, and the access token
//! only ever in an `Authorization` header — never in a URL, an error or a log.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;

use crate::auth::TokenManager;
use crate::errors::{parse_fortnox_error, snippet, FortnoxError};

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
        self.request(reqwest::Method::GET, path, query, None).await
    }

    /// `POST {base}{path}?{query}` with a JSON body, decoded as JSON.
    pub async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        query: Query<'_>,
    ) -> Result<serde_json::Value, FortnoxError> {
        self.request(reqwest::Method::POST, path, query, Some(body))
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
                .request(reqwest::Method::GET, path, &paged, None)
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
        body: Option<&serde_json::Value>,
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
        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let renewed = self.tokens.access_token(true).await.map_err(|err| {
                let detail = err.to_string();
                FortnoxError::Auth(format!(
                    "{method} {whence} was refused with HTTP 401 and the forced token \
                     refresh failed too: {detail}"
                ))
            })?;
            self.send(&method, url, &renewed, body).await?
        } else {
            response
        };

        self.read(response, &method, &whence).await
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
    async fn send(
        &self,
        method: &reqwest::Method,
        url: Url,
        token: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, FortnoxError> {
        let mut request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
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

        let body = response.text().await.map_err(|err| {
            FortnoxError::Transport(format!(
                "reading the reply to {method} {whence} failed: {}",
                err.without_url()
            ))
        })?;

        if !status.is_success() {
            return Err(parse_fortnox_error(status.as_u16(), &body));
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
        if !is_json(&content_type) {
            return Err(FortnoxError::Api {
                status: status.as_u16(),
                body: format!(
                    "{method} {whence} answered with content-type {content_type:?}, which is \
                     not JSON. Body began: {}",
                    snippet(&body)
                ),
            });
        }

        serde_json::from_str(&body).map_err(|err| FortnoxError::Api {
            status: status.as_u16(),
            body: format!(
                "{method} {whence} answered JSON this build cannot read ({err}). Body began: {}",
                snippet(&body)
            ),
        })
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
    #[tokio::test]
    async fn an_html_200_errors_instead_of_deserialising_into_nonsense() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("<html>sign in</html>", "text/html"),
            )
            .mount(&server)
            .await;

        let (client, _refresh, _store) = client_for(&server);
        let err = client.get("accounts", &[]).await.expect_err("not JSON");

        let rendered = err.to_string();
        assert!(rendered.contains("text/html"), "{rendered}");
        assert!(rendered.contains("sign in"), "{rendered}");
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
