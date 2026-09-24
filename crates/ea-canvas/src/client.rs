//! The Canvas LMS REST client.
//!
//! Read-only by construction: every method here is a `GET`. Canvas is the
//! system of record for coursework deadlines, and the agent's job is to know
//! about them, not to touch them — there is deliberately no code in this crate
//! that can submit, comment, or enrol.
//!
//! # The token
//!
//! A Canvas access token is a bearer credential for the *entire* account. It
//! can read every course, every submission and every message the owner can,
//! and (with a different verb than any this file uses) act as them. It is read
//! from `~/.config/exec-agent/canvas/credentials.json`, which must be mode
//! `0600`, and it is handled the way the Telegram bot token is:
//!
//! * [`Credentials`] and [`CanvasClient`] have hand-written `Debug` impls that
//!   print `<redacted>` — the derived ones would put the token in the first
//!   `tracing` line that ever formats them.
//! * No error message in this file quotes the token. They quote paths, HTTP
//!   statuses, content types and (truncated) response bodies, none of which
//!   Canvas fills with the credential.
//! * The token travels in the `Authorization` header, never in a URL, so a
//!   `reqwest` error's `Display` — which does carry the URL — cannot leak it.
//!   The base URL is validated to carry no userinfo for the same reason.
//! * Pagination follows `Link: rel="next"` only while it stays on the
//!   configured origin. The header is server-controlled; without the check, a
//!   compromised or misconfigured Canvas could walk the client onto another
//!   host with the `Authorization` header still attached.
//! * The HTTP client follows no redirects at all. `reqwest`'s default policy
//!   strips the `Authorization` header only when the host or port changes,
//!   never when the scheme does, so a same-host redirect from `https` to
//!   `http` would carry the bearer token onto the wire in clear text. This
//!   API has no reason to redirect, so the policy is simply "don't".

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The file, inside the connector's config directory, holding the credentials.
pub const CREDENTIALS_FILE: &str = "credentials.json";

/// The connector's name — the config directory and the policy section both
/// use it.
pub const CONNECTOR: &str = "canvas";

/// How many `Link: rel="next"` hops to follow before calling it a loop. A
/// hundred courses fit in one page; fifty pages is two orders of magnitude of
/// headroom and still terminates.
const MAX_PAGES: usize = 50;

/// Per-request deadline. The daemon has its own, larger one around the whole
/// tool call; this one bounds a single HTTP round trip so a wedged TLS
/// handshake does not eat the caller's entire budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an unexpected response body to quote back. Enough to recognise
/// a login page or an error envelope, not enough to fill a log.
const BODY_SNIPPET: usize = 300;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// The contents of `credentials.json`, once read and checked.
///
/// No `Debug` derive and no `Display`: see the module docs.
#[derive(Clone)]
pub struct Credentials {
    /// e.g. `https://canvas.kth.se`.
    pub base_url: String,
    token: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct RawCredentials {
    #[serde(rename = "baseUrl")]
    base_url: String,
    token: String,
}

/// The sentence every credential failure ends with. A connector that cannot
/// find its token must say what to create and where, because the person
/// reading it is looking at a daemon that has gone quiet, not at this source
/// file.
fn how_to_create(path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new(".")).display();
    format!(
        "Create it with:\n  \
         mkdir -p {dir}\n  \
         printf '{{\"baseUrl\":\"https://canvas.kth.se\",\"token\":\"<token>\"}}' > {path}\n  \
         chmod 600 {path}\n\
         Get <token> from Canvas -> Account -> Settings -> \"+ New access token\".",
        path = path.display()
    )
}

impl Credentials {
    /// Load from a directory — in production
    /// `~/.config/exec-agent/canvas/` (or `$EA_CONFIG_DIR/canvas/`), which
    /// `main` names once and hands to the server.
    ///
    /// The directory is a parameter rather than a constant so the tests can
    /// point one server at a `wiremock` instance, and so anyone running
    /// against two Canvas instances out of one checkout can too. There is
    /// deliberately no no-argument `load()`: this is read on every call (see
    /// [`crate::tools::CanvasServer`]), and a second entry point that
    /// hard-codes the directory is how a start-up latch grows back.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let path: PathBuf = dir.join(CREDENTIALS_FILE);

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no Canvas credentials at {}.\n{}",
                    path.display(),
                    how_to_create(&path)
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        };

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {mode:04o}; it holds a Canvas access token — a bearer \
                 credential for the whole account — and must be 0600 (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        // The parse error is quoted, the file contents are not: a malformed
        // credentials file is exactly the case where the token might be
        // somewhere unexpected in it.
        let raw: RawCredentials = serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not the expected JSON ({err}). It must be an object with \
                 \"baseUrl\" and \"token\".\n{}",
                path.display(),
                how_to_create(&path)
            )
        })?;

        let base_url = raw.base_url.trim().to_string();
        let token = raw.token.trim().to_string();
        if base_url.is_empty() {
            bail!("{} has an empty \"baseUrl\"", path.display());
        }
        if token.is_empty() {
            bail!("{} has an empty \"token\"", path.display());
        }

        // A bearer credential for a whole account does not travel in clear
        // text. Loopback is exempt so the connector can be pointed at a local
        // fake without weakening the real path.
        let parsed = Url::parse(&base_url)
            .with_context(|| format!("{} has an unparseable \"baseUrl\"", path.display()))?;
        let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if parsed.scheme() != "https" && !loopback {
            bail!(
                "{} has baseUrl {base_url:?}, which is not https. The access token is \
                 sent on every request and must not travel in clear text.",
                path.display()
            );
        }

        Ok(Self { base_url, token })
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// A course the token's owner is enrolled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Course {
    pub id: i64,
    pub name: String,
    pub course_code: String,
}

/// One assignment. `due_at` is `None` for an assignment with no deadline,
/// which Canvas is full of — see [`crate::tools`] for why those are not
/// deadlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub id: i64,
    pub course_id: i64,
    pub name: String,
    pub due_at: Option<DateTime<Utc>>,
    pub html_url: String,
}

#[derive(Deserialize)]
struct RawCourse {
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    course_code: Option<String>,
    /// Canvas returns enrolments whose dates have passed as a stub carrying
    /// this flag, an id, and nothing else. Deserialising it into a `Course`
    /// with an empty name would put a nameless course in front of the model.
    #[serde(default)]
    access_restricted_by_date: bool,
}

#[derive(Deserialize)]
struct RawAssignment {
    id: i64,
    #[serde(default)]
    course_id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    due_at: Option<DateTime<Utc>>,
    #[serde(default)]
    html_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A Canvas REST client bound to one instance and one token.
pub struct CanvasClient {
    http: reqwest::Client,
    /// Always ends in `/`, so [`Url::join`] appends rather than replaces the
    /// last path segment.
    base: Url,
    token: String,
}

impl fmt::Debug for CanvasClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanvasClient")
            .field("base", &self.base.as_str())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl CanvasClient {
    /// Fallible for two reasons that both happen at start-up rather than in
    /// flight: the base URL may not be a URL, and `reqwest`'s builder fails
    /// (rather than panicking, as `Client::new` would) if TLS will not start.
    pub fn new(base_url: &str, token: impl Into<String>) -> anyhow::Result<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let mut base = Url::parse(&format!("{trimmed}/"))
            .with_context(|| format!("Canvas base URL {base_url:?} is not a URL"))?;

        if !matches!(base.scheme(), "http" | "https") {
            bail!(
                "Canvas base URL {base_url:?} must be http or https, not {:?}",
                base.scheme()
            );
        }
        // Userinfo in the base URL would end up in every `reqwest` error's
        // `Display`, which is the one place a credential must never be.
        if !base.username().is_empty() || base.password().is_some() {
            bail!("Canvas base URL must not contain a username or password");
        }
        base.set_query(None);
        base.set_fragment(None);

        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // Redirects are never followed. reqwest's default policy strips
            // `Authorization` when the host or port changes but not when the
            // scheme does (reqwest 0.12.28, src/redirect.rs), so a same-host
            // `https` -> `http` redirect would otherwise carry the account's
            // bearer token onto an unencrypted connection. This API has no
            // legitimate redirect to follow.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTPS client for Canvas")?;

        Ok(Self {
            http,
            base,
            token: token.into(),
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// Courses with an active enrolment.
    pub async fn list_courses(&self) -> anyhow::Result<Vec<Course>> {
        let url = self.endpoint(
            "api/v1/courses",
            &[("enrollment_state", "active"), ("per_page", "100")],
        )?;
        let raw: Vec<RawCourse> = self.get_paginated(url).await?;
        Ok(raw
            .into_iter()
            .filter(|course| !course.access_restricted_by_date)
            .map(|course| Course {
                id: course.id,
                name: course
                    .name
                    .unwrap_or_else(|| format!("course {}", course.id)),
                course_code: course.course_code.unwrap_or_default(),
            })
            .collect())
    }

    /// Every assignment in one course, dated or not.
    pub async fn list_assignments(&self, course_id: i64) -> anyhow::Result<Vec<Assignment>> {
        let url = self.endpoint(
            &format!("api/v1/courses/{course_id}/assignments"),
            &[("per_page", "100")],
        )?;
        let raw: Vec<RawAssignment> = self.get_paginated(url).await?;
        Ok(raw
            .into_iter()
            .map(|a| Assignment {
                id: a.id,
                // Canvas sends `course_id` on an assignment, but the course
                // was named in the request either way, so a missing field is
                // recoverable rather than fatal.
                course_id: a.course_id.unwrap_or(course_id),
                name: a.name.unwrap_or_else(|| format!("assignment {}", a.id)),
                due_at: a.due_at,
                html_url: a.html_url.unwrap_or_default(),
            })
            .collect())
    }

    /// Every future-dated assignment across every active course, earliest
    /// first.
    ///
    /// Composed from `list_courses` + `list_assignments` rather than from
    /// Canvas's own `/users/self/upcoming_events`, which answers a different
    /// question (calendar events and assignments within a fixed window) in a
    /// different shape. One failing course fails the whole call: a partial
    /// deadline list read as complete is worse than no list.
    pub async fn list_upcoming(&self) -> anyhow::Result<Vec<Assignment>> {
        self.list_upcoming_since(Utc::now()).await
    }

    /// [`list_upcoming`](Self::list_upcoming) with the clock supplied, so the
    /// tests are not written against "now".
    pub async fn list_upcoming_since(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<Assignment>> {
        let mut upcoming = Vec::new();
        for course in self.list_courses().await? {
            let assignments = self
                .list_assignments(course.id)
                .await
                .with_context(|| format!("listing assignments for course {}", course.id))?;
            upcoming.extend(
                assignments
                    .into_iter()
                    .filter(|a| a.due_at.is_some_and(|due| due >= now)),
            );
        }
        upcoming.sort_by(|a, b| a.due_at.cmp(&b.due_at).then(a.id.cmp(&b.id)));
        Ok(upcoming)
    }

    fn endpoint(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Url> {
        let mut url = self
            .base
            .join(path)
            .with_context(|| format!("building a Canvas URL for {path}"))?;
        // Guarded: `query_pairs_mut` leaves a bare `?` behind when there is
        // nothing to append.
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// `GET` a collection, following `Link: rel="next"` to the end.
    async fn get_paginated<T: DeserializeOwned>(&self, first: Url) -> anyhow::Result<Vec<T>> {
        let mut items = Vec::new();
        let mut next = Some(first);

        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                return Ok(items);
            };
            let (page, link) = self.get_page::<T>(url).await?;
            items.extend(page);
            next = match link {
                None => return Ok(items),
                Some(candidate) => Some(self.same_origin(candidate)?),
            };
        }

        bail!(
            "canvas: {} kept offering another page after {MAX_PAGES} of them; \
             refusing to follow the Link: rel=\"next\" chain further",
            self.base
        )
    }

    /// One `GET`, returning the decoded page and the raw `next` link.
    async fn get_page<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> anyhow::Result<(Vec<T>, Option<String>)> {
        // `where` is the URL with its query stripped: enough to say which
        // endpoint failed, short enough to read in a log line.
        let mut whence = url.clone();
        whence.set_query(None);

        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|err| err.without_url())
            .with_context(|| format!("canvas: GET {whence} failed"))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let next = next_link(
            response
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|value| value.to_str().ok())
                .unwrap_or(""),
        );

        let body = response
            .text()
            .await
            .map_err(|err| err.without_url())
            .with_context(|| format!("canvas: reading the reply to GET {whence}"))?;

        if !status.is_success() {
            bail!(
                "canvas: GET {whence} returned HTTP {status}{}. {}",
                canonical_reason(status),
                explain(status, &body)
            );
        }

        // A 200 that is not JSON is the failure mode that hurts: Canvas
        // answers a login redirect, a WAF challenge or a maintenance window
        // with an HTML page and a perfectly cheerful status code. Without this
        // check the body either fails to deserialise with an opaque message or,
        // worse, deserialises into something empty.
        if !is_json(&content_type) {
            bail!(
                "canvas: GET {whence} answered HTTP {status} with content-type \
                 {content_type:?}, which is not JSON. Canvas serves HTML for a login \
                 redirect or a maintenance page; check that the token is still valid \
                 and that baseUrl points at the API host. Body began: {}",
                snippet(&body)
            );
        }

        let page: Vec<T> = serde_json::from_str(&body).with_context(|| {
            format!(
                "canvas: GET {whence} answered JSON this build does not understand. \
                 Body began: {}",
                snippet(&body)
            )
        })?;

        Ok((page, next))
    }

    /// Refuse a `next` link that leaves the configured origin. The header is
    /// written by the server, and the client attaches the account's bearer
    /// token to whatever it names.
    fn same_origin(&self, candidate: String) -> anyhow::Result<Url> {
        let url = Url::parse(&candidate)
            .with_context(|| format!("canvas: Link: rel=\"next\" is not a URL: {candidate}"))?;
        let same = url.scheme() == self.base.scheme()
            && url.host_str() == self.base.host_str()
            && url.port_or_known_default() == self.base.port_or_known_default();
        if !same {
            bail!(
                "canvas: refusing to follow a Link: rel=\"next\" to {}://{}, which is not \
                 the configured Canvas host {}://{} — the access token is only ever sent \
                 to the configured host",
                url.scheme(),
                url.host_str().unwrap_or("(no host)"),
                self.base.scheme(),
                self.base.host_str().unwrap_or("(no host)"),
            );
        }
        Ok(url)
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
            "The Canvas access token was rejected: it has expired, been deleted, \
                or belongs to another instance. Create a new one under Canvas -> \
                Account -> Settings and rewrite credentials.json. "
        }
        403 => {
            "Canvas refused this request. The token may lack the scope for it, or \
                the account may be rate limited. "
        }
        404 => {
            "Canvas has no such course or assignment, or the token's owner cannot \
                see it. "
        }
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

/// Pull the `rel="next"` URL out of an RFC 8288 `Link` header.
///
/// Scans `<url>` / parameter pairs rather than splitting on `,`, because a
/// comma is legal inside a URL and splitting first silently loses the link.
fn next_link(header: &str) -> Option<String> {
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let close = after.find('>')?;
        let url = &after[..close];
        let tail = &after[close + 1..];
        let params_end = tail.find('<').unwrap_or(tail.len());
        let params = &tail[..params_end];
        let is_next = params.split(';').any(|param| {
            // The comma that separates one link from the next lands at the end
            // of the last parameter of the preceding one.
            let param = param
                .trim()
                .trim_end_matches(',')
                .trim()
                .to_ascii_lowercase();
            param == "rel=\"next\"" || param == "rel=next" || param == "rel='next'"
        });
        if is_next {
            return Some(url.trim().to_string());
        }
        rest = &tail[params_end..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "canvas-token-DO-NOT-LEAK";

    fn client(server: &MockServer) -> CanvasClient {
        CanvasClient::new(&server.uri(), TOKEN).expect("building the client")
    }

    /// `set_body_raw` rather than `set_body_string` plus a header: wiremock's
    /// `set_body_string` stamps `text/plain` over anything inserted before it.
    fn json_page(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a timestamp")
    }

    // --- the wire ---------------------------------------------------------

    #[tokio::test]
    async fn every_request_carries_the_bearer_token_and_asks_for_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .and(header("accept", "application/json"))
            .and(query_param("enrollment_state", "active"))
            .respond_with(json_page(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .list_courses()
            .await
            .expect("an empty course list is a success");
        // `expect(1)` above is checked on drop: without the header matchers
        // the request would not match and the call would 404 instead.
    }

    #[tokio::test]
    async fn a_course_payload_maps_to_a_course() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(serde_json::json!([{
                "id": 41234,
                "name": "XX1001 Example Course",
                "course_code": "XX1001 HT25",
                "enrollment_term_id": 99,
                "workflow_state": "available",
            }])))
            .mount(&server)
            .await;

        let courses = client(&server).list_courses().await.unwrap();
        assert_eq!(
            courses,
            vec![Course {
                id: 41234,
                name: "XX1001 Example Course".into(),
                course_code: "XX1001 HT25".into(),
            }]
        );
    }

    /// Canvas returns a stub for an enrolment whose dates have passed.
    #[tokio::test]
    async fn an_access_restricted_course_stub_is_dropped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(serde_json::json!([
                { "id": 1, "access_restricted_by_date": true },
                { "id": 2, "name": "XX1003", "course_code": "XX1003" },
            ])))
            .mount(&server)
            .await;

        let courses = client(&server).list_courses().await.unwrap();
        assert_eq!(courses.len(), 1, "{courses:?}");
        assert_eq!(courses[0].id, 2);
    }

    #[tokio::test]
    async fn an_assignment_with_a_null_due_at_maps_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/7/assignments"))
            .respond_with(json_page(serde_json::json!([
                {
                    "id": 500,
                    "course_id": 7,
                    "name": "Lab 1",
                    "due_at": "2026-10-01T21:59:00Z",
                    "html_url": "https://canvas.kth.se/courses/7/assignments/500",
                },
                {
                    "id": 501,
                    "course_id": 7,
                    "name": "Optional reading",
                    "due_at": null,
                    "html_url": "https://canvas.kth.se/courses/7/assignments/501",
                },
            ])))
            .mount(&server)
            .await;

        let assignments = client(&server).list_assignments(7).await.unwrap();
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].due_at, Some(at("2026-10-01T21:59:00Z")));
        assert_eq!(
            assignments[1].due_at, None,
            "a null due_at must map to None, not to a fabricated date"
        );
        assert_eq!(assignments[1].name, "Optional reading");
        assert_eq!(assignments[1].course_id, 7);
    }

    #[tokio::test]
    async fn pagination_follows_link_rel_next_across_two_pages() {
        let server = MockServer::start().await;
        let page_two = format!("{}/api/v1/courses?page=2&per_page=100", server.uri());

        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .and(query_param("page", "2"))
            .respond_with(json_page(
                serde_json::json!([{ "id": 2, "name": "Two", "course_code": "T2" }]),
            ))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(
                json_page(serde_json::json!([{ "id": 1, "name": "One", "course_code": "T1" }]))
                    .insert_header(
                        "link",
                        format!("<{page_two}>; rel=\"next\",<{page_two}>; rel=\"last\"",).as_str(),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let courses = client(&server).list_courses().await.unwrap();
        let ids: Vec<i64> = courses.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 2], "both pages must be collected in order");
    }

    #[tokio::test]
    async fn a_next_link_to_another_host_is_refused_and_the_token_is_not_sent_there() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json_page(serde_json::json!([])))
            .expect(0) // the token must never reach this host
            .mount(&elsewhere)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(
                json_page(serde_json::json!([{ "id": 1, "name": "One", "course_code": "T1" }]))
                    .insert_header(
                        "link",
                        format!("<{}/api/v1/courses?page=2>; rel=\"next\"", elsewhere.uri())
                            .as_str(),
                    ),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
            .await
            .expect_err("a cross-origin next link must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("refusing to follow"), "{text}");
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
    }

    /// reqwest's default redirect policy strips `Authorization` only when the
    /// host or port changes, never the scheme — so a same-host `https` ->
    /// `http` redirect would otherwise carry the token onto the wire in
    /// clear text. The client must follow no redirects at all.
    #[tokio::test]
    async fn a_redirect_is_not_followed_and_the_token_is_not_sent_to_the_target() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json_page(serde_json::json!([])))
            .expect(0) // a followed redirect would land here with the token
            .mount(&elsewhere)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(ResponseTemplate::new(301).insert_header(
                "location",
                format!("{}/api/v1/courses", elsewhere.uri()).as_str(),
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
            .await
            .expect_err("a 301 must not be followed transparently");
        let text = format!("{err:#}");
        assert!(text.contains("301"), "{text}");
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
    }

    #[tokio::test]
    async fn an_endless_next_chain_stops_at_the_page_cap() {
        let server = MockServer::start().await;
        let loop_url = format!("{}/api/v1/courses?page=2", server.uri());
        Mock::given(method("GET"))
            .respond_with(
                json_page(serde_json::json!([]))
                    .insert_header("link", format!("<{loop_url}>; rel=\"next\"").as_str()),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
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

    #[tokio::test]
    async fn a_401_errors_naming_the_status_without_leaking_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"errors":[{"message":"user authorisation required"}]}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
            .await
            .expect_err("a 401 must be an error");
        let text = format!("{err:#}");
        assert!(text.contains("401"), "the status must be named: {text}");
        assert!(text.contains("Unauthorized"), "{text}");
        assert!(
            text.contains("Account -> Settings"),
            "a 401 must say how to fix it: {text}"
        );
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
    }

    #[tokio::test]
    async fn an_html_200_errors_instead_of_deserialising_into_nonsense() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<!doctype html><html><body>Canvas is down for maintenance</body></html>",
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
            .await
            .expect_err("an HTML body must not be accepted as an empty list");
        let text = format!("{err:#}");
        assert!(
            text.contains("text/html"),
            "the content type must be named: {text}"
        );
        assert!(text.contains("not JSON"), "{text}");
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
    }

    #[tokio::test]
    async fn a_json_body_of_the_wrong_shape_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(json_page(serde_json::json!({ "errors": "not a list" })))
            .mount(&server)
            .await;

        let err = client(&server)
            .list_courses()
            .await
            .expect_err("wrong shape");
        let text = format!("{err:#}");
        assert!(text.contains("does not understand"), "{text}");
        assert!(!text.contains(TOKEN), "{text}");
    }

    #[tokio::test]
    async fn a_connection_failure_errors_without_leaking_the_token() {
        // A port nothing is listening on.
        let client = CanvasClient::new("http://127.0.0.1:1", TOKEN).unwrap();
        let err = client.list_courses().await.expect_err("no listener");
        let text = format!("{err:#}");
        assert!(!text.contains(TOKEN), "the error leaked the token: {text}");
    }

    // --- list_upcoming ----------------------------------------------------

    /// One course with a past, a future and an undated assignment; a second
    /// course to prove the walk covers all of them and the result is sorted.
    async fn upcoming_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(serde_json::json!([
                { "id": 7, "name": "XX1001", "course_code": "XX1001" },
                { "id": 8, "name": "XX1003", "course_code": "XX1003" },
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/7/assignments"))
            .respond_with(json_page(serde_json::json!([
                { "id": 1, "course_id": 7, "name": "Past", "due_at": "2026-09-01T21:59:00Z" },
                { "id": 2, "course_id": 7, "name": "Later", "due_at": "2026-11-01T21:59:00Z" },
                { "id": 3, "course_id": 7, "name": "Undated", "due_at": null },
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/8/assignments"))
            .respond_with(json_page(serde_json::json!([
                { "id": 4, "course_id": 8, "name": "Sooner", "due_at": "2026-10-05T21:59:00Z" },
            ])))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn upcoming_is_future_dated_only_and_sorted_earliest_first() {
        let server = upcoming_server().await;
        let upcoming = client(&server)
            .list_upcoming_since(at("2026-09-24T00:00:00Z"))
            .await
            .unwrap();

        let names: Vec<&str> = upcoming.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Sooner", "Later"],
            "past and undated assignments are not upcoming deadlines"
        );
    }

    #[tokio::test]
    async fn one_failing_course_fails_the_whole_upcoming_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(serde_json::json!([
                { "id": 7, "name": "XX1001", "course_code": "XX1001" },
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/7/assignments"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let err = client(&server)
            .list_upcoming()
            .await
            .expect_err("a partial deadline list must not be reported as complete");
        let text = format!("{err:#}");
        assert!(text.contains("course 7"), "{text}");
        assert!(text.contains("500"), "{text}");
    }

    // --- credentials ------------------------------------------------------

    fn write_credentials(dir: &Path, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(CREDENTIALS_FILE);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn credentials_load_from_a_private_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{ "baseUrl": "https://canvas.kth.se", "token": "abc123" }"#,
            0o600,
        );

        let creds = Credentials::load_from(tmp.path()).unwrap();
        assert_eq!(creds.base_url, "https://canvas.kth.se");
        assert_eq!(creds.token(), "abc123");
    }

    #[test]
    fn a_missing_credentials_file_names_the_path_and_how_to_create_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = Credentials::load_from(tmp.path()).expect_err("no file");
        let text = format!("{err:#}");
        assert!(
            text.contains(&tmp.path().join(CREDENTIALS_FILE).display().to_string()),
            "{text}"
        );
        assert!(text.contains("chmod 600"), "{text}");
        assert!(text.contains("New access token"), "{text}");
    }

    #[test]
    fn a_malformed_credentials_file_is_an_error_naming_the_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(tmp.path(), "{ this is not json", 0o600);
        let err = Credentials::load_from(tmp.path()).expect_err("malformed");
        let text = format!("{err:#}");
        assert!(text.contains(CREDENTIALS_FILE), "{text}");
        assert!(text.contains("baseUrl"), "{text}");
    }

    #[test]
    fn a_world_readable_credentials_file_is_refused_and_not_echoed() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{ "baseUrl": "https://canvas.kth.se", "token": "leaky" }"#,
            0o644,
        );
        let err = Credentials::load_from(tmp.path()).expect_err("mode 0644");
        let text = format!("{err:#}");
        assert!(text.contains("0644"), "{text}");
        assert!(text.contains("chmod 600"), "{text}");
        assert!(
            !text.contains("leaky"),
            "the error leaked the token: {text}"
        );
    }

    #[test]
    fn a_plaintext_base_url_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{ "baseUrl": "http://canvas.kth.se", "token": "abc" }"#,
            0o600,
        );
        let err = Credentials::load_from(tmp.path()).expect_err("http");
        assert!(format!("{err:#}").contains("not https"), "{err:#}");
    }

    #[test]
    fn a_loopback_base_url_is_allowed_for_local_testing() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{ "baseUrl": "http://127.0.0.1:8080", "token": "abc" }"#,
            0o600,
        );
        Credentials::load_from(tmp.path()).expect("loopback is exempt");
    }

    #[test]
    fn an_empty_token_is_an_error_not_an_anonymous_client() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_credentials(
            tmp.path(),
            r#"{ "baseUrl": "https://canvas.kth.se", "token": "   " }"#,
            0o600,
        );
        let err = Credentials::load_from(tmp.path()).expect_err("empty token");
        assert!(format!("{err:#}").contains("empty \"token\""), "{err:#}");
    }

    #[test]
    fn neither_debug_impl_prints_the_token() {
        let creds = Credentials {
            base_url: "https://canvas.kth.se".into(),
            token: TOKEN.into(),
        };
        let text = format!("{creds:?}");
        assert!(!text.contains(TOKEN), "{text}");
        assert!(text.contains("<redacted>"), "{text}");

        let client = CanvasClient::new("https://canvas.kth.se", TOKEN).unwrap();
        let text = format!("{client:?}");
        assert!(!text.contains(TOKEN), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn a_base_url_with_userinfo_is_refused() {
        // Userinfo would land in every reqwest error's Display.
        let err = CanvasClient::new("https://someone:secret@canvas.kth.se", TOKEN)
            .expect_err("userinfo must be refused");
        assert!(
            format!("{err:#}").contains("username or password"),
            "{err:#}"
        );
    }

    #[test]
    fn a_base_url_with_a_path_prefix_keeps_the_prefix() {
        let client = CanvasClient::new("https://example.test/canvas", TOKEN).unwrap();
        let url = client.endpoint("api/v1/courses", &[]).unwrap();
        assert_eq!(url.as_str(), "https://example.test/canvas/api/v1/courses");
    }

    // --- Link header parsing ---------------------------------------------

    #[test]
    fn next_link_finds_the_next_relation_among_others() {
        let header = "<https://c.test/api?page=1>; rel=\"current\",\
                      <https://c.test/api?page=2>; rel=\"next\",\
                      <https://c.test/api?page=9>; rel=\"last\"";
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://c.test/api?page=2")
        );
    }

    #[test]
    fn next_link_is_none_on_the_last_page() {
        let header = "<https://c.test/api?page=9>; rel=\"last\"";
        assert_eq!(next_link(header), None);
        assert_eq!(next_link(""), None);
    }

    #[test]
    fn next_link_survives_a_comma_inside_a_url() {
        let header = "<https://c.test/api?ids=1,2,3&page=2>; rel=\"next\"";
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://c.test/api?ids=1,2,3&page=2")
        );
    }

    #[test]
    fn is_json_accepts_the_shapes_canvas_sends() {
        assert!(is_json("application/json"));
        assert!(is_json("application/json; charset=utf-8"));
        assert!(is_json("application/vnd.api+json"));
        assert!(!is_json("text/html; charset=utf-8"));
        assert!(!is_json(""));
    }
}
