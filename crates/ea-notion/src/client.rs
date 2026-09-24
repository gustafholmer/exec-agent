//! The Notion REST client.
//!
//! # The pinned API version
//!
//! [`NOTION_VERSION`] is `2026-03-11`, sent on every request. Notion versions
//! its API by date header rather than by URL, and *requires* the header: a
//! request without it is refused. The value here is pinned to a constant
//! rather than tracking "latest", because Notion's version boundaries are
//! breaking — `2026-03-11` alone renamed `archived` to `in_trash`, replaced
//! the `after` parameter with a `position` object, and renamed a block type —
//! and a client that floats would break on Notion's release schedule rather
//! than on ours.
//!
//! The version was chosen by reading
//! <https://developers.notion.com/reference/versioning>, which names
//! `2026-03-11` as current, rather than by copying the `2022-06-28` that most
//! examples still show. That matters more than it looks: `2025-09-03` split
//! **databases** from **data sources**, a database may now hold several data
//! sources, and Notion's own upgrade guide states that under `2022-06-28`
//! "several API actions will fail" against a multi-source database — page
//! creation and database queries among them. Pinning the comfortable old
//! version would have produced a client that works on every database the
//! author happens to own today and fails on the first one anybody adds a
//! second source to.
//!
//! The consequence is visible in [`NotionClient::query_database`]: under this
//! version `POST /v1/databases/{id}/query` is gone, and a database is queried
//! by resolving it to its data sources (`GET /v1/databases/{id}`) and querying
//! each (`POST /v1/data_sources/{id}/query`).
//!
//! # Three properties this module exists to get right
//!
//! * **[`page_title`] finds the title wherever it is.** Notion keys a page's
//!   properties by the *user's own* column names and marks the title by its
//!   `type`, never by its key. Looking for `"Name"` yields `(untitled)` for
//!   every page in a database whose title column is called `Uppgift`, and a
//!   digest of untitled pages is worth nothing. Notion also splits rich text
//!   on every formatting change, so a title with one bold word arrives as
//!   several runs that must be joined.
//! * **Pagination is followed, and the cap is loud.** Every list endpoint
//!   answers with `has_more` and `next_cursor`. Stopping at the first page
//!   hides most of a workspace and looks exactly like a quiet week — the
//!   failure that cannot be seen. The walk follows the cursor up to
//!   [`MAX_PAGES`] and then **errors, discarding what it collected**, rather
//!   than returning a short list a caller would report as the whole truth.
//!   Same standard as `ea_google::calendar::CalendarClient::list_events`.
//! * **429 is retried, a bounded number of times.** Notion answers HTTP 429
//!   with a `Retry-After` header in whole seconds. One retry after the
//!   requested wait recovers the ordinary burst; a missing header falls back
//!   to [`RetryPolicy::default_backoff`]; and the attempt count is bounded, so
//!   a workspace that is genuinely over its limit produces an error naming the
//!   rate limit instead of an unbounded retry loop, which is how an
//!   integration gets banned.
//!
//! # HTTP hardening
//!
//! The same standard as `ea-google` and `ea-canvas`.
//!
//! * The token travels in the `Authorization` header, never in a URL or a log
//!   line, and every `reqwest` error goes through `.without_url()` before it
//!   is turned into a message.
//! * No redirects (`redirect::Policy::none()`). A same-host https→http
//!   downgrade would put the bearer token on the wire in clear text, and this
//!   API has no legitimate redirect to follow.
//! * A response's content type is checked before it is deserialised. An API
//!   fronted by a CDN can answer 200 with an HTML error page, and that must
//!   not be read as an empty page of results.
//! * Every id that reaches a URL path is validated as a Notion UUID first.
//!   Ids arrive from tool arguments a language model produces, so `../../` in
//!   an id is a realistic input rather than a thought experiment.
//! * No `unwrap`, `expect` or `panic!` outside `#[cfg(test)]`.

use std::fmt;
use std::time::Duration;

use reqwest::{Method, Url};
use serde_json::{json, Value};

use crate::auth::{validate_workspace, INTEGRATIONS_URL};

/// Notion's REST base. Overridable through [`NotionClient::with_base_url`],
/// which is the seam every test in this crate uses to point at `wiremock`
/// instead of the internet.
pub const NOTION_API_BASE: &str = "https://api.notion.com/v1/";

/// The API version sent on every request, as the `Notion-Version` header.
///
/// Pinned, never floating — see the module docs for why this is `2026-03-11`
/// and not the `2022-06-28` that most sample code still carries.
pub const NOTION_VERSION: &str = "2026-03-11";

/// The workspace label a client carries when the caller does not name one.
/// Only ever used to make an error message say *which* workspace's token was
/// refused; see [`NotionClient::with_workspace`].
pub const DEFAULT_WORKSPACE: &str = "default";

/// How many `next_cursor` hops to follow before declaring a loop.
///
/// At [`PAGE_SIZE`] results a page this is 5000 objects, which is far more
/// than any digest wants and still terminates. Reaching it is an error, not a
/// truncation: see the module docs.
pub const MAX_PAGES: usize = 50;

/// Results per page. Notion's documented maximum is 100, and asking for the
/// maximum is what keeps [`MAX_PAGES`] generous in objects while staying small
/// in requests.
const PAGE_SIZE: u32 = 100;

/// Blocks per `children` array. Notion's documented maximum per request;
/// [`NotionClient::append_blocks`] chunks to it rather than letting a long
/// append fail with a 400.
const MAX_CHILDREN_PER_REQUEST: usize = 100;

/// Per-request deadline. Bounds one HTTP round trip, not the caller's whole
/// budget — the same value and reasoning as the Google and Canvas clients.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an unexpected response body to quote back in an error.
const BODY_SNIPPET: usize = 300;

/// What [`page_title`] returns when there is no title to be found. A page can
/// genuinely have an empty title — a database row someone created and has not
/// named yet — so this is a normal output, not an error.
pub const UNTITLED: &str = "(untitled)";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a Notion call failed.
///
/// The variants exist so a caller can *branch*, which is the whole point of
/// the requirement that a deleted page be distinguishable from a dead network:
/// [`NotionError::NotFound`] means the object is gone and the caller should
/// forget it, while [`NotionError::Transport`] means nothing is known and the
/// caller should retry later. Collapsing both into one string would make a
/// watch loop either drop live pages on a flaky network or keep polling
/// deleted ones forever.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NotionError {
    /// The token was refused. Names the workspace and the setup step, because
    /// the fix is always a human at the integrations page.
    #[error(
        "notion: the integration token for workspace {workspace:?} was refused with HTTP 401. \
         The token is wrong, or the integration was removed from the workspace. Create or \
         re-copy an internal integration at {INTEGRATIONS_URL} and write its secret to the \
         credential file for {workspace:?}, then re-share the pages it should see via each \
         page's \"Connections\" menu. ({operation})"
    )]
    Unauthorized {
        /// The workspace label whose credential file holds the bad token.
        workspace: String,
        /// Which call was refused, with no credential in it.
        operation: String,
    },

    /// HTTP 404: the object does not exist, or has not been shared with the
    /// integration — Notion does not distinguish the two, on purpose, and
    /// neither can this.
    #[error(
        "notion: no {kind} with id {id:?} is visible to workspace {workspace:?} (HTTP 404). \
         It was deleted, or it has never been shared with the integration — Notion answers \
         404 for both. If it should exist, open it in Notion and share it via \
         \"Connections\". ({operation})"
    )]
    NotFound {
        workspace: String,
        /// `"page"`, `"database"`, `"data source"` or `"block"`.
        kind: &'static str,
        id: String,
        operation: String,
    },

    /// HTTP 429 that survived [`RetryPolicy::max_attempts`] tries.
    #[error(
        "notion: workspace {workspace:?} is rate limited — Notion answered HTTP 429 on all \
         {attempts} attempts, after waiting {waited_secs:.1}s in total. Giving up rather than \
         retrying further: an integration that keeps hammering a rate limit gets blocked. \
         Notion's limit is roughly 3 requests per second averaged per integration. \
         ({operation})"
    )]
    RateLimited {
        workspace: String,
        attempts: u32,
        waited_secs: f64,
        operation: String,
    },

    /// The request never got an answer: DNS, TLS, connection refused, or the
    /// [`HTTP_TIMEOUT`] deadline. Nothing is known about the object.
    #[error("notion: {operation} could not reach the Notion API: {detail}")]
    Transport { operation: String, detail: String },

    /// A non-2xx status that is none of the above.
    #[error("notion: {operation} returned HTTP {status}. {detail}")]
    Api {
        operation: String,
        status: u16,
        detail: String,
    },

    /// A 2xx whose body is not the JSON this build understands — including a
    /// 200 that is not JSON at all.
    #[error("notion: {operation} answered something this build cannot read. {detail}")]
    Malformed { operation: String, detail: String },

    /// The cursor walk hit [`MAX_PAGES`].
    ///
    /// An error rather than a truncated list, and the collected results are
    /// dropped on the way out: a caller handed half a workspace has no way to
    /// know it is half, and "nothing much happened" is the one conclusion a
    /// digest must never reach by accident.
    #[error(
        "notion: {operation} kept offering another page after {max_pages} of them (up to \
         {page_size} results each). Refusing to follow next_cursor further, and discarding \
         what was collected rather than returning a partial list a caller would report as \
         the whole workspace."
    )]
    PaginationCap {
        operation: String,
        max_pages: usize,
        page_size: u32,
    },

    /// The caller asked for something impossible: a malformed id, a base URL
    /// that is not a URL, too many blocks in one page creation. Never a
    /// server's fault.
    #[error("notion: {0}")]
    Invalid(String),
}

impl NotionError {
    /// The object is gone or was never shared. A caller can forget it.
    pub fn is_not_found(&self) -> bool {
        matches!(self, NotionError::NotFound { .. })
    }

    /// Nothing is known — the request never completed. A caller should retry
    /// later rather than conclude anything about the object.
    pub fn is_transport(&self) -> bool {
        matches!(self, NotionError::Transport { .. })
    }

    /// The token is bad. A caller should stop and tell a human.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, NotionError::Unauthorized { .. })
    }

    /// Notion is throttling. A caller should back off, not re-drive.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, NotionError::RateLimited { .. })
    }
}

// ---------------------------------------------------------------------------
// Title extraction
// ---------------------------------------------------------------------------

/// The title of a page, from its `properties` object.
///
/// Pure: no I/O, no allocation beyond the returned string, and total — every
/// input produces a string.
///
/// # Why this is not `properties["Name"]`
///
/// Notion keys a page's properties by the column names the *user* typed. A
/// Swedish task database calls its title column `Uppgift`; a support database
/// calls it `Ärende`; plenty of people put an emoji in it. What is invariant
/// is the property's `"type": "title"` — a database has exactly one title
/// property, whatever it is called — and for a page that is not in a database
/// the property is literally keyed `title`. So the lookup is by type.
///
/// # Why the runs are joined
///
/// A Notion title is an array of rich-text runs, and Notion starts a new run
/// at every formatting change. `Book **the** venue` arrives as three runs.
/// Reading only `title[0]` would render it as `Book `, which is worse than
/// useless in a digest because it looks like a complete title.
///
/// `plain_text` is Notion's own flattened rendering of a run and is preferred;
/// `text.content` is the fallback, which matters for hand-built request bodies
/// (Notion requires only `text.content` on the way *in*) and for run types
/// that a future API version might add.
pub fn page_title(properties: &Value) -> String {
    let Some(map) = properties.as_object() else {
        return UNTITLED.to_string();
    };

    // Two passes rather than one, so that an explicit `"type": "title"` always
    // wins over the lenient fallback below even if it is not the first key.
    let typed = map
        .values()
        .find(|prop| prop.get("type").and_then(Value::as_str) == Some("title"));

    // Lenient fallback: a property carrying a `title` array but no `type`.
    // Notion always sends `type`, but request bodies this crate itself builds
    // do not, and neither do the fixtures people write by hand.
    let prop = typed.or_else(|| map.values().find(|prop| prop.get("title").is_some()));

    let Some(prop) = prop else {
        return UNTITLED.to_string();
    };

    let Some(runs) = prop.get("title").and_then(Value::as_array) else {
        return UNTITLED.to_string();
    };

    let mut title = String::new();
    for run in runs {
        if let Some(text) = run.get("plain_text").and_then(Value::as_str) {
            title.push_str(text);
        } else if let Some(text) = run
            .get("text")
            .and_then(|t| t.get("content"))
            .and_then(Value::as_str)
        {
            title.push_str(text);
        }
    }

    let trimmed = title.trim();
    if trimmed.is_empty() {
        UNTITLED.to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------

/// How hard to try again after a 429.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first. `1` disables retrying.
    pub max_attempts: u32,
    /// The wait when a 429 arrives without a usable `Retry-After`. Notion
    /// documents the header as always present, but "documented" and "present
    /// on every error path, including the CDN's" are different claims, and a
    /// missing header must not become a zero-delay retry storm.
    pub default_backoff: Duration,
    /// Ceiling on any single wait. A `Retry-After` of 86400 is not a reason
    /// to block a poll for a day; the caller's next scheduled run is a better
    /// place to pick the work back up.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    /// Three attempts, one second of default backoff, one minute of ceiling.
    ///
    /// Three because Notion's limit is an average rather than a hard burst
    /// cap, so the overwhelming majority of 429s clear on the first retry; the
    /// third attempt is there for the case where two pollers collided. Beyond
    /// that the honest answer is "you are over your limit", which the caller
    /// needs to *see* rather than have papered over by a client that quietly
    /// takes a minute per call.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            default_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// How long to wait after a 429, given the response's `Retry-After` header.
///
/// Pure, and separated from the request loop precisely so it can be tested
/// without anything sleeping. Notion documents `Retry-After` as an integer
/// number of seconds; a fractional value is accepted anyway because parsing
/// one costs nothing and refusing it would fall back to a *longer* default.
/// Anything unparseable, negative, or non-finite falls back to
/// [`RetryPolicy::default_backoff`], and everything is clamped to
/// [`RetryPolicy::max_backoff`].
pub fn retry_delay(retry_after: Option<&str>, policy: &RetryPolicy) -> Duration {
    let parsed = retry_after
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(Duration::from_secs_f64);

    parsed
        .unwrap_or(policy.default_backoff)
        .min(policy.max_backoff)
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Check that `raw` is a Notion object id, and return it trimmed.
///
/// Notion ids are UUIDs, written either dashed
/// (`0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0`) or bare
/// (`0f1e2d3c4b5a49688776a5b4c3d2e1f0`); both forms are accepted by the API
/// and both are returned by it, so both are accepted here and passed through
/// unchanged rather than normalised into one.
///
/// This runs before the id is interpolated into a URL path. Ids reach this
/// crate from tool arguments a language model produces, where
/// `../../users/me` is a realistic input; the check is what stops it becoming
/// a different endpoint.
fn checked_id(raw: &str, kind: &'static str) -> Result<String, NotionError> {
    let id = raw.trim();
    let hex: Vec<char> = id.chars().filter(|c| *c != '-').collect();
    let ok = hex.len() == 32
        && hex.iter().all(char::is_ascii_hexdigit)
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');

    if !ok {
        return Err(NotionError::Invalid(format!(
            "{raw:?} is not a Notion {kind} id. Notion ids are UUIDs, dashed \
             (\"0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0\") or bare \
             (\"0f1e2d3c4b5a49688776a5b4c3d2e1f0\"). Copy it from the page URL, or from the \
             \"id\" field of a search result."
        )));
    }
    Ok(id.to_string())
}

// ---------------------------------------------------------------------------
// Parents
// ---------------------------------------------------------------------------

/// Where a new page goes.
///
/// [`Parent::Database`] is kept separate from [`Parent::DataSource`] because
/// under [`NOTION_VERSION`] they are genuinely different addresses, not two
/// names for one thing: a database may hold several data sources, and a page
/// lives in exactly one of them. A caller that only has a database id can use
/// [`Parent::Database`]; Notion resolves it to the database's first data
/// source, which is the right answer for every single-source database and an
/// arbitrary one otherwise, so a caller that cares should resolve it
/// explicitly with [`NotionClient::data_sources`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parent {
    /// A child page under an ordinary page.
    Page(String),
    /// A row in a specific data source.
    DataSource(String),
    /// A row in a database, letting Notion pick the data source.
    Database(String),
}

impl Parent {
    fn to_json(&self) -> Result<Value, NotionError> {
        Ok(match self {
            Parent::Page(id) => {
                let id = checked_id(id, "page")?;
                json!({ "type": "page_id", "page_id": id })
            }
            Parent::DataSource(id) => {
                let id = checked_id(id, "data source")?;
                json!({ "type": "data_source_id", "data_source_id": id })
            }
            Parent::Database(id) => {
                let id = checked_id(id, "database")?;
                json!({ "type": "database_id", "database_id": id })
            }
        })
    }
}

/// One data source inside a database, as `GET /v1/databases/{id}` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSourceRef {
    pub id: String,
    pub name: String,
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A Notion client bound to one base URL and one integration token.
pub struct NotionClient {
    http: reqwest::Client,
    /// Always ends in `/`, so [`Url::join`] appends rather than replaces the
    /// last path segment.
    base: Url,
    token: String,
    workspace: String,
    retry: RetryPolicy,
}

impl fmt::Debug for NotionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The base URL is Notion's public host (or a test server's loopback
        // address) and the workspace is a label, never a credential. The token
        // is neither printed nor fingerprinted.
        f.debug_struct("NotionClient")
            .field("base", &self.base.as_str())
            .field("workspace", &self.workspace)
            .field("token", &"<redacted>")
            .field("version", &NOTION_VERSION)
            .field("retry", &self.retry)
            .finish()
    }
}

impl NotionClient {
    /// A client against the real Notion API, holding `token`.
    ///
    /// Fallible for the same two reasons as the Google and Canvas clients: the
    /// base URL must parse, and building the `reqwest` client fails if TLS
    /// will not start.
    pub fn new(token: impl Into<String>) -> Result<Self, NotionError> {
        Self::with_base_url(NOTION_API_BASE, token)
    }

    /// A client against `base_url`. The seam every test in this crate uses.
    pub fn with_base_url(base_url: &str, token: impl Into<String>) -> Result<Self, NotionError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(NotionError::Invalid(
                "the Notion integration token is empty; there is nothing to authenticate with"
                    .to_string(),
            ));
        }

        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/")).map_err(|err| {
            NotionError::Invalid(format!("Notion base URL {base_url:?} is not a URL: {err}"))
        })?;

        if !matches!(base.scheme(), "http" | "https") {
            return Err(NotionError::Invalid(format!(
                "Notion base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            )));
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            return Err(NotionError::Invalid(
                "Notion base URL must not contain a username or password".to_string(),
            ));
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // No redirects, ever: a same-host https->http downgrade would
            // carry the bearer token in clear text, and this API has no
            // legitimate redirect to follow.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| {
                NotionError::Invalid(format!(
                    "building the HTTPS client for Notion failed: {}",
                    err.without_url()
                ))
            })?;

        Ok(Self {
            http,
            base,
            token,
            workspace: DEFAULT_WORKSPACE.to_string(),
            retry: RetryPolicy::default(),
        })
    }

    /// Name the workspace this token belongs to, so a 401 can say *which*
    /// credential file to fix. The label is validated by the same rule the
    /// token store uses, so a client and its credential file can never
    /// disagree about what a legal label is.
    pub fn with_workspace(mut self, workspace: &str) -> Result<Self, NotionError> {
        validate_workspace(workspace).map_err(|err| NotionError::Invalid(err.to_string()))?;
        self.workspace = workspace.to_string();
        Ok(self)
    }

    /// Override the 429 policy. Production uses [`RetryPolicy::default`]; the
    /// tests use a policy with millisecond waits so that proving the retry
    /// *behaviour* does not cost seconds of wall clock.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Every page and data source the integration can see whose title matches
    /// `query`, following `next_cursor` to the end.
    ///
    /// `query` of `None` returns everything shared with the integration, which
    /// is what a digest wants. `start_cursor` resumes a walk that was
    /// interrupted; `None` starts at the beginning.
    ///
    /// Returns the raw Notion objects. A page's title comes out of
    /// [`page_title`] applied to its `properties`.
    pub async fn search(
        &self,
        query: Option<&str>,
        start_cursor: Option<&str>,
    ) -> Result<Vec<Value>, NotionError> {
        let url = self.endpoint("search")?;
        let mut cursor = start_cursor.map(str::to_string);
        let mut collected = Vec::new();

        for _ in 0..MAX_PAGES {
            let mut body = json!({ "page_size": PAGE_SIZE });
            if let Some(query) = query {
                body["query"] = json!(query);
            }
            if let Some(cursor) = &cursor {
                body["start_cursor"] = json!(cursor);
            }

            let value = self
                .request(Method::POST, url.clone(), Some(body), "search", None)
                .await?;
            let (results, next) = list_page(&value, "search", cursor.as_deref())?;
            collected.extend(results);

            match next {
                None => return Ok(collected),
                Some(next) => cursor = Some(next),
            }
        }

        Err(self.pagination_cap("search"))
    }

    /// The data sources inside a database.
    ///
    /// Since API version `2025-09-03` a database is a container and the rows
    /// live in one or more *data sources* under it; this is the lookup that
    /// turns a database id (the thing in a Notion URL) into the ids the query
    /// endpoint actually takes.
    pub async fn data_sources(&self, database_id: &str) -> Result<Vec<DataSourceRef>, NotionError> {
        let id = checked_id(database_id, "database")?;
        let url = self.endpoint(&format!("databases/{id}"))?;
        let operation = format!("GET databases/{id}");

        let value = self
            .request(Method::GET, url, None, &operation, Some(("database", &id)))
            .await?;

        let Some(entries) = value.get("data_sources").and_then(Value::as_array) else {
            return Err(NotionError::Malformed {
                operation,
                detail: format!(
                    "the reply has no \"data_sources\" array. Under Notion-Version \
                     {NOTION_VERSION} a database reports the data sources its rows live in, \
                     and without one there is nothing to query."
                ),
            });
        };

        let mut sources = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                return Err(NotionError::Malformed {
                    operation,
                    detail: "a data source in the reply has no \"id\"".to_string(),
                });
            };
            sources.push(DataSourceRef {
                id: id.to_string(),
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok(sources)
    }

    /// Every row in one data source, following `next_cursor` to the end.
    pub async fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> Result<Vec<Value>, NotionError> {
        let id = checked_id(data_source_id, "data source")?;
        let url = self.endpoint(&format!("data_sources/{id}/query"))?;
        let operation = format!("POST data_sources/{id}/query");

        let mut cursor = start_cursor.map(str::to_string);
        let mut collected = Vec::new();

        for _ in 0..MAX_PAGES {
            let mut body = json!({ "page_size": PAGE_SIZE });
            if let Some(cursor) = &cursor {
                body["start_cursor"] = json!(cursor);
            }

            let value = self
                .request(
                    Method::POST,
                    url.clone(),
                    Some(body),
                    &operation,
                    Some(("data source", &id)),
                )
                .await?;
            let (results, next) = list_page(&value, &operation, cursor.as_deref())?;
            collected.extend(results);

            match next {
                None => return Ok(collected),
                Some(next) => cursor = Some(next),
            }
        }

        Err(self.pagination_cap(&operation))
    }

    /// Every row in a database, across all of its data sources.
    ///
    /// Under [`NOTION_VERSION`] `POST /v1/databases/{id}/query` no longer
    /// exists, so this resolves the database to its data sources and queries
    /// each. A single-source database — which is every database created before
    /// September 2025, and most since — costs one extra `GET`.
    ///
    /// `start_cursor` resumes an interrupted walk. It is only meaningful when
    /// the database has exactly one data source, because a cursor belongs to
    /// one data source's result set; asking to resume a multi-source database
    /// is refused rather than silently applied to whichever source happens to
    /// be listed first.
    pub async fn query_database(
        &self,
        database_id: &str,
        start_cursor: Option<&str>,
    ) -> Result<Vec<Value>, NotionError> {
        let id = checked_id(database_id, "database")?;
        let sources = self.data_sources(&id).await?;

        match sources.len() {
            0 => Err(NotionError::Malformed {
                operation: format!("GET databases/{id}"),
                detail: "the database reports no data sources, so it has no rows to query"
                    .to_string(),
            }),
            1 => self.query_data_source(&sources[0].id, start_cursor).await,
            _ if start_cursor.is_some() => Err(NotionError::Invalid(format!(
                "database {id:?} has {} data sources, so a start_cursor is ambiguous — a \
                 cursor belongs to one data source's result set. Resolve the database with \
                 data_sources() and call query_data_source() with the cursor's own source.",
                sources.len()
            ))),
            _ => {
                let mut collected = Vec::new();
                for source in &sources {
                    collected.extend(self.query_data_source(&source.id, None).await?);
                }
                Ok(collected)
            }
        }
    }

    /// One page object.
    ///
    /// A deleted or unshared page is [`NotionError::NotFound`]; an unreachable
    /// network is [`NotionError::Transport`]. Those are different variants
    /// because they want different responses — see the type's docs.
    pub async fn fetch_page(&self, page_id: &str) -> Result<Value, NotionError> {
        let id = checked_id(page_id, "page")?;
        let url = self.endpoint(&format!("pages/{id}"))?;
        let operation = format!("GET pages/{id}");
        self.request(Method::GET, url, None, &operation, Some(("page", &id)))
            .await
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Create a page under `parent`, titled `title`, with `blocks` as its
    /// initial content.
    ///
    /// The title is written under the key `title`, which is the title
    /// property's *id* — always the literal string `"title"`, for every
    /// database, whatever the column is called. Notion accepts a property id
    /// wherever it accepts a property name, so this is the one spelling that
    /// works without first fetching the parent's schema to learn that its
    /// title column is called `Uppgift`.
    pub async fn create_page(
        &self,
        parent: &Parent,
        title: &str,
        blocks: &[Value],
    ) -> Result<Value, NotionError> {
        if blocks.len() > MAX_CHILDREN_PER_REQUEST {
            return Err(NotionError::Invalid(format!(
                "create_page was given {} blocks; Notion accepts at most \
                 {MAX_CHILDREN_PER_REQUEST} children in one request. Create the page with the \
                 first {MAX_CHILDREN_PER_REQUEST} and append_blocks the rest.",
                blocks.len()
            )));
        }

        let body = json!({
            "parent": parent.to_json()?,
            "properties": {
                "title": { "title": [ { "text": { "content": title } } ] }
            },
            "children": blocks,
        });

        let url = self.endpoint("pages")?;
        self.request(Method::POST, url, Some(body), "POST pages", None)
            .await
    }

    /// Append `blocks` to the end of a page (or any block that can have
    /// children), returning the created blocks.
    ///
    /// Chunked to [`MAX_CHILDREN_PER_REQUEST`], because Notion refuses a
    /// longer `children` array with a 400 and a digest can easily exceed it.
    /// No `position` is sent, so Notion's default — the end — applies; under
    /// [`NOTION_VERSION`] the old flat `after` parameter no longer exists and
    /// position is an object, which is why nothing here sends one.
    pub async fn append_blocks(
        &self,
        page_id: &str,
        blocks: &[Value],
    ) -> Result<Vec<Value>, NotionError> {
        let id = checked_id(page_id, "page")?;
        if blocks.is_empty() {
            return Err(NotionError::Invalid(
                "append_blocks was given no blocks; Notion refuses an empty children array"
                    .to_string(),
            ));
        }

        let url = self.endpoint(&format!("blocks/{id}/children"))?;
        let operation = format!("PATCH blocks/{id}/children");

        let mut created = Vec::new();
        for chunk in blocks.chunks(MAX_CHILDREN_PER_REQUEST) {
            let body = json!({ "children": chunk });
            let value = self
                .request(
                    Method::PATCH,
                    url.clone(),
                    Some(body),
                    &operation,
                    Some(("block", &id)),
                )
                .await?;
            if let Some(results) = value.get("results").and_then(Value::as_array) {
                created.extend(results.iter().cloned());
            }
        }
        Ok(created)
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    fn endpoint(&self, path: &str) -> Result<Url, NotionError> {
        self.base.join(path).map_err(|err| {
            NotionError::Invalid(format!(
                "building the Notion URL for {path:?} failed: {err}"
            ))
        })
    }

    fn pagination_cap(&self, operation: &str) -> NotionError {
        NotionError::PaginationCap {
            operation: operation.to_string(),
            max_pages: MAX_PAGES,
            page_size: PAGE_SIZE,
        }
    }

    /// One request, with the 429 retry loop, the content-type gate, and the
    /// status mapping.
    ///
    /// `object` is `(kind, id)` for the endpoints where a 404 means "this
    /// specific object is gone" — it is what turns a 404 into
    /// [`NotionError::NotFound`] rather than a generic API error.
    async fn request(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        operation: &str,
        object: Option<(&'static str, &str)>,
    ) -> Result<Value, NotionError> {
        let mut attempt: u32 = 0;
        let mut waited = Duration::ZERO;

        loop {
            attempt += 1;

            // Rebuilt each attempt rather than cloned: a `RequestBuilder` is
            // only conditionally cloneable, and getting that wrong silently
            // turns a retry into a no-op.
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&self.token)
                .header("Notion-Version", NOTION_VERSION)
                .header(reqwest::header::ACCEPT, "application/json");
            if let Some(body) = &body {
                request = request.json(body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    return Err(NotionError::Transport {
                        operation: operation.to_string(),
                        detail: err.without_url().to_string(),
                    });
                }
            };

            let status = response.status();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                if attempt >= self.retry.max_attempts {
                    return Err(NotionError::RateLimited {
                        workspace: self.workspace.clone(),
                        attempts: attempt,
                        waited_secs: waited.as_secs_f64(),
                        operation: operation.to_string(),
                    });
                }
                let header = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let delay = retry_delay(header.as_deref(), &self.retry);
                tracing::warn!(
                    workspace = %self.workspace,
                    operation = %operation,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    retry_after = header.as_deref().unwrap_or("<absent>"),
                    "notion rate limited; backing off"
                );
                waited += delay;
                tokio::time::sleep(delay).await;
                continue;
            }

            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();

            let text = match response.text().await {
                Ok(text) => text,
                Err(err) => {
                    return Err(NotionError::Transport {
                        operation: operation.to_string(),
                        detail: format!("reading the reply failed: {}", err.without_url()),
                    });
                }
            };

            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(NotionError::Unauthorized {
                    workspace: self.workspace.clone(),
                    operation: operation.to_string(),
                });
            }

            if status == reqwest::StatusCode::NOT_FOUND {
                if let Some((kind, id)) = object {
                    return Err(NotionError::NotFound {
                        workspace: self.workspace.clone(),
                        kind,
                        id: id.to_string(),
                        operation: operation.to_string(),
                    });
                }
            }

            if !status.is_success() {
                return Err(NotionError::Api {
                    operation: operation.to_string(),
                    status: status.as_u16(),
                    detail: explain(status, &text),
                });
            }

            // A 200 that is not JSON is the failure mode that hurts: a CDN
            // error page or a captive portal would otherwise either fail to
            // deserialise with an opaque message or, worse, read as an empty
            // list of results.
            if !is_json(&content_type) {
                return Err(NotionError::Malformed {
                    operation: operation.to_string(),
                    detail: format!(
                        "HTTP {status} with content-type {content_type:?}, which is not JSON. \
                         Body began: {}",
                        snippet(&text)
                    ),
                });
            }

            return serde_json::from_str(&text).map_err(|err| NotionError::Malformed {
                operation: operation.to_string(),
                detail: format!(
                    "the reply is not valid JSON ({err}). Body began: {}",
                    snippet(&text)
                ),
            });
        }
    }
}

/// Pull `results` and the *effective* next cursor out of a Notion list reply.
///
/// Two ways a server can make a cursor walk spin, both turned into an error
/// here rather than a hang:
///
/// * `has_more: true` with a null `next_cursor` — the walk would re-request
///   page one forever, collecting the same results [`MAX_PAGES`] times.
/// * A `next_cursor` equal to the cursor just sent — the same loop, one step
///   longer.
fn list_page(
    value: &Value,
    operation: &str,
    sent_cursor: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), NotionError> {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Err(NotionError::Malformed {
            operation: operation.to_string(),
            detail: "the reply has no \"results\" array".to_string(),
        });
    };
    let results = results.clone();

    let has_more = value
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !has_more {
        return Ok((results, None));
    }

    let Some(next) = value.get("next_cursor").and_then(Value::as_str) else {
        return Err(NotionError::Malformed {
            operation: operation.to_string(),
            detail: "the reply says has_more but carries no next_cursor, so there is no way \
                     to ask for the rest without re-reading the same page forever"
                .to_string(),
        });
    };

    if Some(next) == sent_cursor {
        return Err(NotionError::Malformed {
            operation: operation.to_string(),
            detail: "the reply's next_cursor is the cursor that was just sent, which would \
                     re-read the same page forever"
                .to_string(),
        });
    }

    Ok((results, Some(next.to_string())))
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

/// Turn a status and an error body into the sentence that tells the reader
/// what to do about it.
///
/// Notion's error bodies are JSON carrying a stable `code` and a human
/// `message`; both are pulled out when present, because the raw body is mostly
/// braces. A Notion error body never contains the request's own token.
fn explain(status: reqwest::StatusCode, body: &str) -> String {
    let hint = match status.as_u16() {
        400 => "Notion rejected the request as malformed. ",
        403 => {
            "Notion refused this request. The integration's capabilities may not include \
             this operation (an integration set to read-only cannot create or update pages). "
        }
        404 => {
            "Notion has no such object, or it has not been shared with the integration — \
             Notion answers 404 for both. "
        }
        409 => "A conflicting edit was in flight; this one can be retried. ",
        502..=504 => "Notion is unavailable or gateway-erroring; this can be retried. ",
        _ => "",
    };

    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let detail = match &parsed {
        Some(value) => {
            let code = value.get("code").and_then(Value::as_str);
            let message = value.get("message").and_then(Value::as_str);
            match (code, message) {
                (Some(code), Some(message)) => Some(format!("Notion said {code:?}: {message}")),
                (Some(code), None) => Some(format!("Notion said {code:?}")),
                (None, Some(message)) => Some(format!("Notion said: {message}")),
                (None, None) => None,
            }
        }
        None => None,
    };

    match detail {
        Some(detail) => format!("{hint}{detail}"),
        None => format!("{hint}Body began: {}", snippet(body)),
    }
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

    use std::time::Instant;

    use wiremock::matchers::{body_string_contains, header, method as http_method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A well-formed Notion id. Every test that touches a URL path needs one,
    /// because `checked_id` refuses anything that is not a UUID.
    const PAGE_ID: &str = "0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0";
    const DB_ID: &str = "9b3c2a1d-4e5f-4a6b-8c7d-0e1f2a3b4c5d";
    const DS_ID: &str = "11111111-2222-3333-4444-555555555555";
    const DS_ID_2: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    const TOKEN: &str = "ntn_a_secret_that_must_not_leak";

    fn json_body(value: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(value.to_string(), "application/json")
    }

    /// A client pointed at `server`, with millisecond backoff so that proving
    /// the retry *behaviour* costs milliseconds rather than seconds. The two
    /// tests that care about the actual duration build their own policy.
    fn client(server: &MockServer) -> NotionClient {
        NotionClient::with_base_url(&format!("{}/v1", server.uri()), TOKEN)
            .expect("a client against the mock server")
            .with_workspace("work")
            .expect("\"work\" is a legal label")
            .with_retry_policy(RetryPolicy {
                max_attempts: 3,
                default_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(5),
            })
    }

    // =======================================================================
    // Review Focus #1 — title extraction
    // =======================================================================

    #[test]
    fn finds_the_title_however_the_property_is_named() {
        for key in ["Name", "Title", "Uppgift", "Ärende", "📌 Task"] {
            let props = serde_json::json!({
                key: { "type": "title", "title": [{ "plain_text": "Book the venue" }] },
                "Status": { "type": "select", "select": { "name": "Todo" } }
            });
            assert_eq!(page_title(&props), "Book the venue", "property named {key}");
        }
    }

    #[test]
    fn joins_a_title_split_across_rich_text_runs() {
        let props = serde_json::json!({
            "Name": { "type": "title", "title": [
                { "plain_text": "Book " }, { "plain_text": "the venue" }
            ]}
        });
        assert_eq!(page_title(&props), "Book the venue");
    }

    #[test]
    fn falls_back_for_a_page_with_no_title_property() {
        assert_eq!(page_title(&serde_json::json!({})), "(untitled)");
        let empty = serde_json::json!({ "Name": { "type": "title", "title": [] } });
        assert_eq!(page_title(&empty), "(untitled)");
    }

    /// Notion splits on *every* formatting change, so a real bolded title is
    /// three runs with the formatting flags attached. Reading `title[0]` would
    /// yield "Book " — which looks like a complete title and is not.
    #[test]
    fn a_title_with_a_bold_word_survives_its_three_runs() {
        let props = serde_json::json!({
            "Uppgift": { "type": "title", "title": [
                { "plain_text": "Boka ", "annotations": { "bold": false } },
                { "plain_text": "lokalen", "annotations": { "bold": true } },
                { "plain_text": " senast fredag", "annotations": { "bold": false } }
            ]}
        });
        assert_eq!(page_title(&props), "Boka lokalen senast fredag");
    }

    /// The title property is not necessarily the first key, and a `select`
    /// property whose name sorts earlier must not shadow it.
    #[test]
    fn the_title_is_found_even_when_other_properties_come_first() {
        let props = serde_json::json!({
            "Assignee": { "type": "people", "people": [] },
            "Due": { "type": "date", "date": { "start": "2026-10-01" } },
            "Zebra": { "type": "title", "title": [{ "plain_text": "Last alphabetically" }] }
        });
        assert_eq!(page_title(&props), "Last alphabetically");
    }

    /// A page that is not in a database keys its title property literally
    /// `title` — the same code path, and worth pinning because it is the shape
    /// `create_page` writes.
    #[test]
    fn a_child_pages_literal_title_property_works() {
        let props = serde_json::json!({
            "title": { "id": "title", "type": "title", "title": [{ "plain_text": "Weekly digest" }] }
        });
        assert_eq!(page_title(&props), "Weekly digest");
    }

    /// `plain_text` is Notion's own flattened rendering and is preferred, but
    /// a body built for a *request* carries only `text.content` — and that is
    /// what `create_page` sends, so round-tripping it must work.
    #[test]
    fn a_run_with_only_text_content_is_read() {
        let props = serde_json::json!({
            "Name": { "title": [{ "text": { "content": "Written by create_page" } }] }
        });
        assert_eq!(page_title(&props), "Written by create_page");
    }

    #[test]
    fn a_whitespace_only_title_is_untitled_rather_than_blank() {
        let props = serde_json::json!({
            "Name": { "type": "title", "title": [{ "plain_text": "   " }] }
        });
        assert_eq!(page_title(&props), "(untitled)");
    }

    #[test]
    fn page_title_is_total_on_junk_input() {
        for junk in [
            serde_json::json!(null),
            serde_json::json!(7),
            serde_json::json!("a string"),
            serde_json::json!([]),
            serde_json::json!({ "Name": { "type": "title" } }),
            serde_json::json!({ "Name": { "type": "title", "title": "not an array" } }),
            serde_json::json!({ "Name": { "type": "title", "title": [{}] } }),
        ] {
            assert_eq!(page_title(&junk), "(untitled)", "input {junk}");
        }
    }

    // =======================================================================
    // Ids
    // =======================================================================

    #[test]
    fn a_notion_id_is_accepted_dashed_or_bare() {
        assert_eq!(checked_id(PAGE_ID, "page").unwrap(), PAGE_ID);
        let bare = "0f1e2d3c4b5a49688776a5b4c3d2e1f0";
        assert_eq!(checked_id(bare, "page").unwrap(), bare);
        assert_eq!(
            checked_id(&format!("  {PAGE_ID} "), "page").unwrap(),
            PAGE_ID
        );
    }

    /// Ids arrive from tool arguments a language model produces, so the check
    /// runs before anything is interpolated into a URL path.
    #[test]
    fn an_id_that_could_reach_another_endpoint_is_refused() {
        for bad in [
            "../../users/me",
            "0f1e2d3c-4b5a-4968-8776-a5b4c3d2e1f0/../users",
            "",
            "not-a-uuid",
            "0f1e2d3cb447405bbf9fdd2d76c5be4",   // 31 hex
            "0f1e2d3c4b5a49688776a5b4c3d2e1f0b", // 33 hex
            "0f1e2d3c-b447-405b-bf9f-dd2d76c5be4g",
            "https://api.notion.com/v1/users",
        ] {
            assert!(checked_id(bad, "page").is_err(), "{bad:?} must be refused");
        }
    }

    #[tokio::test]
    async fn a_bad_id_fails_before_any_request_is_made() {
        let server = MockServer::start().await;
        let err = client(&server)
            .fetch_page("../../users/me")
            .await
            .expect_err("a traversal id must be refused");
        assert!(matches!(err, NotionError::Invalid(_)), "{err}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "nothing may reach the network"
        );
    }

    // =======================================================================
    // Review Focus #2 — pagination
    // =======================================================================

    #[tokio::test]
    async fn search_follows_the_cursor_and_returns_both_pages() {
        let server = MockServer::start().await;

        // Registered first, so it wins for the request that carries the
        // cursor; the generic mock below serves the first page.
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .and(body_string_contains("cursor-abc"))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [{ "id": "p2" }],
                "has_more": false,
                "next_cursor": null,
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [{ "id": "p1" }],
                "has_more": true,
                "next_cursor": "cursor-abc",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let results = client(&server)
            .search(None, None)
            .await
            .expect("both pages are collected");

        let ids: Vec<&str> = results
            .iter()
            .filter_map(|p| p.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["p1", "p2"], "both pages, in order");
    }

    /// The cap is loud: an error naming it, and *no* partial list. A caller
    /// handed 50 pages of a larger workspace cannot tell it is incomplete.
    #[tokio::test]
    async fn an_endless_cursor_chain_stops_at_the_cap_and_discards_the_partial_list() {
        let server = MockServer::start().await;

        // A fresh cursor every time, so neither loop guard fires and only the
        // page cap can stop this.
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(move |req: &wiremock::Request| {
                let n = req.body.len();
                json_body(serde_json::json!({
                    "object": "list",
                    "results": [{ "id": "endless" }],
                    "has_more": true,
                    "next_cursor": format!("cursor-{n}-{}", uuid::Uuid::new_v4()),
                }))
            })
            .mount(&server)
            .await;

        let err = client(&server)
            .search(None, None)
            .await
            .expect_err("an endless chain must stop");

        assert!(
            matches!(err, NotionError::PaginationCap { .. }),
            "should be the cap, got {err}"
        );
        let text = format!("{err}");
        assert!(text.contains("50"), "names the cap: {text}");
        assert!(
            text.contains("discarding"),
            "says results were dropped: {text}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            MAX_PAGES,
            "exactly MAX_PAGES requests, then stop"
        );
    }

    /// `has_more: true` with a null `next_cursor` would make the walk
    /// re-request page one until the cap, collecting the same results fifty
    /// times. It is a malformed reply, not a pagination cap.
    #[tokio::test]
    async fn has_more_without_a_cursor_is_an_error_not_a_re_read() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [{ "id": "p1" }],
                "has_more": true,
                "next_cursor": null,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server)
            .search(None, None)
            .await
            .expect_err("malformed");
        assert!(matches!(err, NotionError::Malformed { .. }), "{err}");
        assert!(format!("{err}").contains("next_cursor"), "{err}");
    }

    #[tokio::test]
    async fn a_cursor_that_repeats_itself_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [],
                "has_more": true,
                "next_cursor": "same",
            })))
            .mount(&server)
            .await;

        let err = client(&server)
            .search(None, Some("same"))
            .await
            .expect_err("a self-referential cursor must not spin");
        assert!(matches!(err, NotionError::Malformed { .. }), "{err}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "caught on the first reply, not after fifty"
        );
    }

    #[tokio::test]
    async fn a_start_cursor_is_sent_and_a_missing_query_is_omitted() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list", "results": [], "has_more": false, "next_cursor": null,
            })))
            .mount(&server)
            .await;

        client(&server)
            .search(None, Some("resume-here"))
            .await
            .expect("an empty page is fine");

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).expect("a JSON body");
        assert_eq!(body["start_cursor"], "resume-here");
        assert_eq!(body["page_size"], 100);
        assert!(
            body.get("query").is_none(),
            "no query key when None: {body}"
        );
    }

    // =======================================================================
    // Review Focus #3 — rate limits
    // =======================================================================

    #[test]
    fn retry_delay_honours_retry_after_and_clamps_it() {
        let policy = RetryPolicy {
            max_attempts: 3,
            default_backoff: Duration::from_secs(7),
            max_backoff: Duration::from_secs(60),
        };

        // Notion documents whole seconds.
        assert_eq!(retry_delay(Some("1"), &policy), Duration::from_secs(1));
        assert_eq!(retry_delay(Some(" 30 "), &policy), Duration::from_secs(30));
        assert_eq!(retry_delay(Some("0"), &policy), Duration::ZERO);
        // A fractional value costs nothing to accept and refusing it would
        // fall back to a *longer* wait.
        assert_eq!(
            retry_delay(Some("1.5"), &policy),
            Duration::from_millis(1500)
        );
        // No header, or a header this code cannot use: the default backoff.
        assert_eq!(retry_delay(None, &policy), Duration::from_secs(7));
        for junk in [
            "",
            "soon",
            "-5",
            "NaN",
            "inf",
            "Wed, 21 Oct 2026 07:28:00 GMT",
        ] {
            assert_eq!(
                retry_delay(Some(junk), &policy),
                Duration::from_secs(7),
                "Retry-After: {junk:?}"
            );
        }
        // A hostile or absurd header does not block a poll for a day.
        assert_eq!(retry_delay(Some("86400"), &policy), Duration::from_secs(60));
    }

    /// The end-to-end version of the above: a real `Retry-After: 1` is waited
    /// out. This is the one test in the crate that deliberately costs a
    /// second of wall clock, because the point is that the header's value
    /// reaches the sleep.
    #[tokio::test]
    async fn a_429_with_retry_after_1_waits_a_second_and_then_succeeds() {
        let server = MockServer::start().await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "1")
                    .set_body_raw(
                        r#"{"object":"error","status":429,"code":"rate_limited"}"#,
                        "application/json",
                    ),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [{ "id": "after-the-wait" }],
                "has_more": false,
                "next_cursor": null,
            })))
            .expect(1)
            .mount(&server)
            .await;

        // The real default policy: nothing here shortens the wait.
        let client = NotionClient::with_base_url(&format!("{}/v1", server.uri()), TOKEN)
            .unwrap()
            .with_workspace("work")
            .unwrap();

        let started = Instant::now();
        let results = client.search(None, None).await.expect("the retry succeeds");
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 1, "{results:#?}");
        assert!(
            elapsed >= Duration::from_secs(1),
            "must actually wait the second Notion asked for, waited {elapsed:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the original request and exactly one retry"
        );
    }

    /// A 429 with no `Retry-After` must not become a zero-delay retry storm.
    #[tokio::test]
    async fn a_429_without_retry_after_uses_the_default_backoff() {
        let server = MockServer::start().await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(429).set_body_raw(
                r#"{"object":"error","status":429,"code":"rate_limited"}"#,
                "application/json",
            ))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list", "results": [{ "id": "ok" }], "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = NotionClient::with_base_url(&format!("{}/v1", server.uri()), TOKEN)
            .unwrap()
            .with_workspace("work")
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_attempts: 3,
                default_backoff: Duration::from_millis(250),
                max_backoff: Duration::from_secs(60),
            });

        let started = Instant::now();
        let results = client.search(None, None).await.expect("the retry succeeds");
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 1);
        assert!(
            elapsed >= Duration::from_millis(250),
            "the default backoff must be waited, waited {elapsed:?}"
        );
    }

    /// Unbounded retry against a rate limit is how an integration gets
    /// blocked. Three attempts, then an error that says so.
    #[tokio::test]
    async fn repeated_429s_give_up_after_the_bounded_number_of_attempts() {
        let server = MockServer::start().await;

        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "1")
                    .set_body_raw(
                        r#"{"object":"error","status":429,"code":"rate_limited"}"#,
                        "application/json",
                    ),
            )
            .expect(3)
            .mount(&server)
            .await;

        let err = client(&server)
            .search(None, None)
            .await
            .expect_err("a persistent rate limit is an error, not a hang");

        assert!(err.is_rate_limited(), "{err}");
        match &err {
            NotionError::RateLimited {
                workspace,
                attempts,
                ..
            } => {
                assert_eq!(workspace, "work");
                assert_eq!(*attempts, 3, "bounded at the policy's max_attempts");
            }
            other => panic!("expected RateLimited, got {other}"),
        }
        let text = format!("{err}");
        assert!(
            text.contains("rate limited"),
            "names the rate limit: {text}"
        );
        assert!(text.contains("429"), "names the status: {text}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "exactly three attempts, then stop"
        );
    }

    #[tokio::test]
    async fn a_single_attempt_policy_does_not_retry_at_all() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&server)
            .await;

        let err = NotionClient::with_base_url(&format!("{}/v1", server.uri()), TOKEN)
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            })
            .search(None, None)
            .await
            .expect_err("max_attempts of 1 means no retry");
        assert!(err.is_rate_limited(), "{err}");
    }

    // =======================================================================
    // 401, 404, and transport
    // =======================================================================

    #[tokio::test]
    async fn a_401_names_the_workspace_and_the_setup_step() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/pages/{PAGE_ID}")))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"object":"error","status":401,"code":"unauthorized","message":"API token is invalid."}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .fetch_page(PAGE_ID)
            .await
            .expect_err("a bad token is an error");

        assert!(err.is_unauthorized(), "{err}");
        let text = format!("{err}");
        assert!(text.contains("\"work\""), "names the workspace: {text}");
        assert!(
            text.contains(INTEGRATIONS_URL),
            "names the setup page: {text}"
        );
        assert!(
            text.contains("Connections"),
            "names the sharing step: {text}"
        );
        assert!(!text.contains(TOKEN), "must never quote the token: {text}");
    }

    /// "The page was deleted" and "the network is down" want different
    /// responses, so they must be different variants — not two strings a
    /// caller would have to grep.
    #[tokio::test]
    async fn a_404_and_a_transport_failure_are_distinguishable() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/pages/{PAGE_ID}")))
            .respond_with(ResponseTemplate::new(404).set_body_raw(
                r#"{"object":"error","status":404,"code":"object_not_found","message":"Could not find page."}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let gone = client(&server)
            .fetch_page(PAGE_ID)
            .await
            .expect_err("a deleted page is an error");
        assert!(gone.is_not_found(), "{gone}");
        assert!(!gone.is_transport(), "a 404 is not a transport failure");
        let text = format!("{gone}");
        assert!(text.contains(PAGE_ID), "names the page: {text}");
        assert!(
            text.contains("shared"),
            "explains Notion's 404-for-unshared: {text}"
        );

        // Now the same call against a port with nothing behind it. Bind and
        // release, so the port is known to be free rather than guessed — and
        // not a dropped `MockServer`, whose shutdown is asynchronous and which
        // answers 404 to anything it has no mock for, which is exactly the
        // status this test needs to tell apart from a transport failure.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = closed.local_addr().expect("its address");
        drop(closed);

        let offline = NotionClient::with_base_url(&format!("http://{addr}/v1"), TOKEN)
            .unwrap()
            .with_workspace("work")
            .unwrap()
            .fetch_page(PAGE_ID)
            .await
            .expect_err("a dead server is an error");

        assert!(offline.is_transport(), "{offline}");
        assert!(!offline.is_not_found(), "nothing is known about the page");
        assert!(
            !format!("{offline}").contains(TOKEN),
            "the token must not reach the error"
        );
    }

    #[tokio::test]
    async fn a_404_without_a_named_object_stays_a_generic_api_error() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(404).set_body_raw(
                r#"{"object":"error","code":"object_not_found","message":"nope"}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = client(&server).search(None, None).await.expect_err("404");
        assert!(matches!(err, NotionError::Api { status: 404, .. }), "{err}");
        assert!(format!("{err}").contains("object_not_found"), "{err}");
    }

    // =======================================================================
    // HTTP hardening
    // =======================================================================

    #[tokio::test]
    async fn every_request_carries_the_pinned_notion_version_and_the_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .and(header("notion-version", NOTION_VERSION))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(json_body(serde_json::json!({
                "object": "list", "results": [], "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .search(None, None)
            .await
            .expect("the version and auth headers must both be present");
    }

    #[test]
    fn the_pinned_version_is_the_one_this_crate_was_written_against() {
        // A bare-faced pin. If someone bumps this, every endpoint shape in
        // this module — data sources, `position`, `in_trash` — has to be
        // re-read against the new version's docs first.
        assert_eq!(NOTION_VERSION, "2026-03-11");
    }

    #[tokio::test]
    async fn the_token_never_appears_in_a_url() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(serde_json::json!({
                "object": "list", "results": [], "has_more": false,
            })))
            .mount(&server)
            .await;

        client(&server).search(None, None).await.expect("ok");

        for request in server.received_requests().await.unwrap() {
            assert!(
                !request.url.as_str().contains(TOKEN),
                "the token must never appear in a URL: {}",
                request.url
            );
        }
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_so_the_token_cannot_be_carried_elsewhere() {
        let elsewhere = MockServer::start().await;
        Mock::given(http_method("GET"))
            .respond_with(json_body(serde_json::json!({ "id": "leaked" })))
            .expect(0) // a followed redirect would land here with the token
            .mount(&elsewhere)
            .await;

        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/pages/{PAGE_ID}")))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                format!("{}/v1/pages/{PAGE_ID}", elsewhere.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .fetch_page(PAGE_ID)
            .await
            .expect_err("a redirect must not be followed");
        assert!(matches!(err, NotionError::Api { status: 302, .. }), "{err}");
    }

    /// A 200 carrying HTML — a CDN error page, a captive portal — must not
    /// deserialise as an empty result set.
    #[tokio::test]
    async fn a_200_that_is_not_json_is_refused_rather_than_read_as_empty() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("<html><body>Access denied</body></html>", "text/html"),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .search(None, None)
            .await
            .expect_err("HTML is not an empty workspace");
        assert!(matches!(err, NotionError::Malformed { .. }), "{err}");
        let text = format!("{err}");
        assert!(text.contains("text/html"), "{text}");
    }

    #[test]
    fn debug_on_the_client_never_prints_the_token() {
        let client = NotionClient::new(TOKEN).expect("a client");
        let text = format!("{client:?}");
        assert!(!text.contains(TOKEN), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.contains(NOTION_VERSION), "{text}");
    }

    #[test]
    fn a_base_url_carrying_credentials_or_a_bad_scheme_is_refused() {
        assert!(NotionClient::with_base_url("ftp://api.notion.com/v1", TOKEN).is_err());
        assert!(NotionClient::with_base_url("https://user:pw@api.notion.com/v1", TOKEN).is_err());
        assert!(NotionClient::with_base_url("not a url", TOKEN).is_err());
        assert!(
            NotionClient::new("   ").is_err(),
            "an empty token is refused"
        );
    }

    #[test]
    fn a_workspace_label_is_validated_by_the_same_rule_as_the_token_store() {
        assert!(NotionClient::new(TOKEN)
            .unwrap()
            .with_workspace("../x")
            .is_err());
        assert!(NotionClient::new(TOKEN)
            .unwrap()
            .with_workspace("Work")
            .is_err());
        assert_eq!(
            NotionClient::new(TOKEN)
                .unwrap()
                .with_workspace("work")
                .unwrap()
                .workspace(),
            "work"
        );
    }

    // =======================================================================
    // Databases and data sources
    // =======================================================================

    #[tokio::test]
    async fn query_database_resolves_the_database_to_its_data_source_and_queries_that() {
        let server = MockServer::start().await;

        Mock::given(http_method("GET"))
            .and(path(format!("/v1/databases/{DB_ID}")))
            .respond_with(json_body(serde_json::json!({
                "object": "database",
                "id": DB_ID,
                "data_sources": [{ "id": DS_ID, "name": "Tasks" }],
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(http_method("POST"))
            .and(path(format!("/v1/data_sources/{DS_ID}/query")))
            .respond_with(json_body(serde_json::json!({
                "object": "list",
                "results": [{ "id": "row-1" }],
                "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let rows = client(&server)
            .query_database(DB_ID, None)
            .await
            .expect("the database resolves and queries");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "row-1");
    }

    /// Since 2025-09-03 a database can hold several data sources. Querying
    /// only the first would hide whole tables.
    #[tokio::test]
    async fn a_multi_source_database_is_queried_across_every_source() {
        let server = MockServer::start().await;

        Mock::given(http_method("GET"))
            .and(path(format!("/v1/databases/{DB_ID}")))
            .respond_with(json_body(serde_json::json!({
                "data_sources": [
                    { "id": DS_ID, "name": "Tasks" },
                    { "id": DS_ID_2, "name": "Archive" },
                ],
            })))
            .mount(&server)
            .await;
        Mock::given(http_method("POST"))
            .and(path(format!("/v1/data_sources/{DS_ID}/query")))
            .respond_with(json_body(serde_json::json!({
                "results": [{ "id": "live" }], "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(http_method("POST"))
            .and(path(format!("/v1/data_sources/{DS_ID_2}/query")))
            .respond_with(json_body(serde_json::json!({
                "results": [{ "id": "archived" }], "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let rows = client(&server).query_database(DB_ID, None).await.unwrap();
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["live", "archived"]);
    }

    /// A cursor belongs to one data source's result set, so resuming a
    /// multi-source database is refused rather than silently applied to
    /// whichever source happens to be listed first.
    #[tokio::test]
    async fn resuming_a_multi_source_database_is_refused_rather_than_guessed() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/databases/{DB_ID}")))
            .respond_with(json_body(serde_json::json!({
                "data_sources": [{ "id": DS_ID, "name": "A" }, { "id": DS_ID_2, "name": "B" }],
            })))
            .mount(&server)
            .await;

        let err = client(&server)
            .query_database(DB_ID, Some("cursor-x"))
            .await
            .expect_err("an ambiguous cursor is refused");
        assert!(matches!(err, NotionError::Invalid(_)), "{err}");
        assert!(format!("{err}").contains("data_sources()"), "{err}");
    }

    #[tokio::test]
    async fn a_database_reply_without_data_sources_is_malformed_not_empty() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/databases/{DB_ID}")))
            .respond_with(json_body(
                serde_json::json!({ "object": "database", "id": DB_ID }),
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .query_database(DB_ID, None)
            .await
            .expect_err("malformed");
        assert!(matches!(err, NotionError::Malformed { .. }), "{err}");
        assert!(format!("{err}").contains("data_sources"), "{err}");
    }

    #[tokio::test]
    async fn a_deleted_database_is_not_found_rather_than_a_generic_error() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/databases/{DB_ID}")))
            .respond_with(ResponseTemplate::new(404).set_body_raw(
                r#"{"object":"error","code":"object_not_found"}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .query_database(DB_ID, None)
            .await
            .expect_err("404");
        assert!(err.is_not_found(), "{err}");
        match err {
            NotionError::NotFound { kind, .. } => assert_eq!(kind, "database"),
            other => panic!("{other}"),
        }
    }

    #[tokio::test]
    async fn query_data_source_paginates_too() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path(format!("/v1/data_sources/{DS_ID}/query")))
            .and(body_string_contains("next-rows"))
            .respond_with(json_body(serde_json::json!({
                "results": [{ "id": "r2" }], "has_more": false,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(http_method("POST"))
            .and(path(format!("/v1/data_sources/{DS_ID}/query")))
            .respond_with(json_body(serde_json::json!({
                "results": [{ "id": "r1" }], "has_more": true, "next_cursor": "next-rows",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let rows = client(&server)
            .query_data_source(DS_ID, None)
            .await
            .unwrap();
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["r1", "r2"]);
    }

    // =======================================================================
    // Writes
    // =======================================================================

    /// The title goes under the key `title` — the title property's *id*, which
    /// is that literal string for every database whatever the column is
    /// called. Writing `"Name"` would 400 against a database whose title
    /// column is `Uppgift`, which is the write-side twin of the bug
    /// `page_title` exists to avoid.
    #[tokio::test]
    async fn create_page_sends_the_parent_the_title_and_the_blocks() {
        let server = MockServer::start().await;
        Mock::given(http_method("POST"))
            .and(path("/v1/pages"))
            .respond_with(json_body(
                serde_json::json!({ "object": "page", "id": PAGE_ID }),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let block = serde_json::json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [{ "type": "text", "text": { "content": "Hello" } }] }
        });

        let page = client(&server)
            .create_page(
                &Parent::DataSource(DS_ID.to_string()),
                "Weekly digest",
                std::slice::from_ref(&block),
            )
            .await
            .expect("the page is created");
        assert_eq!(page["id"], PAGE_ID);

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["parent"]["type"], "data_source_id");
        assert_eq!(body["parent"]["data_source_id"], DS_ID);
        assert_eq!(
            body["properties"]["title"]["title"][0]["text"]["content"],
            "Weekly digest"
        );
        assert_eq!(body["children"][0], block);

        // The round trip: what was written is what `page_title` reads back.
        assert_eq!(page_title(&body["properties"]), "Weekly digest");
    }

    #[test]
    fn each_parent_kind_serialises_to_notions_own_spelling() {
        assert_eq!(
            Parent::Page(PAGE_ID.to_string()).to_json().unwrap(),
            serde_json::json!({ "type": "page_id", "page_id": PAGE_ID })
        );
        assert_eq!(
            Parent::Database(DB_ID.to_string()).to_json().unwrap(),
            serde_json::json!({ "type": "database_id", "database_id": DB_ID })
        );
        assert_eq!(
            Parent::DataSource(DS_ID.to_string()).to_json().unwrap(),
            serde_json::json!({ "type": "data_source_id", "data_source_id": DS_ID })
        );
        assert!(Parent::Page("../x".to_string()).to_json().is_err());
    }

    #[tokio::test]
    async fn create_page_refuses_more_children_than_notion_accepts() {
        let server = MockServer::start().await;
        let blocks: Vec<Value> = (0..101).map(|i| serde_json::json!({ "n": i })).collect();

        let err = client(&server)
            .create_page(&Parent::Page(PAGE_ID.to_string()), "Too much", &blocks)
            .await
            .expect_err("101 children is a 400 waiting to happen");
        assert!(matches!(err, NotionError::Invalid(_)), "{err}");
        assert!(
            format!("{err}").contains("append_blocks"),
            "says what to do: {err}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// Notion caps `children` at 100 per request, and a digest can exceed it.
    /// Chunking is what stops a long append turning into a 400.
    #[tokio::test]
    async fn append_blocks_chunks_at_notions_hundred_block_limit() {
        let server = MockServer::start().await;
        Mock::given(http_method("PATCH"))
            .and(path(format!("/v1/blocks/{PAGE_ID}/children")))
            .respond_with(json_body(serde_json::json!({
                "object": "list", "results": [{ "id": "b" }],
            })))
            .expect(3)
            .mount(&server)
            .await;

        let blocks: Vec<Value> = (0..250).map(|i| serde_json::json!({ "n": i })).collect();
        let created = client(&server)
            .append_blocks(PAGE_ID, &blocks)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "250 blocks is 100 + 100 + 50");
        let sizes: Vec<usize> = requests
            .iter()
            .map(|r| {
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                body["children"].as_array().map(Vec::len).unwrap_or(0)
            })
            .collect();
        assert_eq!(sizes, vec![100, 100, 50]);
        assert_eq!(created.len(), 3, "the created blocks from every chunk");
    }

    /// No `position` is sent: the default is the end, and under this version
    /// the old flat `after` parameter no longer exists.
    #[tokio::test]
    async fn append_blocks_sends_no_position_so_notions_default_end_applies() {
        let server = MockServer::start().await;
        Mock::given(http_method("PATCH"))
            .and(path(format!("/v1/blocks/{PAGE_ID}/children")))
            .respond_with(json_body(serde_json::json!({ "results": [] })))
            .mount(&server)
            .await;

        client(&server)
            .append_blocks(PAGE_ID, &[serde_json::json!({ "type": "divider" })])
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("position").is_none(), "{body}");
        assert!(
            body.get("after").is_none(),
            "the pre-2026-03-11 spelling: {body}"
        );
    }

    #[tokio::test]
    async fn append_blocks_refuses_an_empty_array_before_asking_notion() {
        let server = MockServer::start().await;
        let err = client(&server)
            .append_blocks(PAGE_ID, &[])
            .await
            .expect_err("Notion refuses an empty children array");
        assert!(matches!(err, NotionError::Invalid(_)), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_page_returns_the_page_and_its_title_reads_back() {
        let server = MockServer::start().await;
        Mock::given(http_method("GET"))
            .and(path(format!("/v1/pages/{PAGE_ID}")))
            .respond_with(json_body(serde_json::json!({
                "object": "page",
                "id": PAGE_ID,
                "properties": {
                    "Ärende": { "type": "title", "title": [
                        { "plain_text": "Trasig " }, { "plain_text": "skrivare" }
                    ]}
                }
            })))
            .mount(&server)
            .await;

        let page = client(&server).fetch_page(PAGE_ID).await.unwrap();
        assert_eq!(page_title(&page["properties"]), "Trasig skrivare");
    }
}
