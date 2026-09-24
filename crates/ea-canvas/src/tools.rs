//! The MCP surface: four read-only tools over [`CanvasClient`].
//!
//! # Why `watch_poll` must fail loudly
//!
//! The daemon calls `watch_poll` on a timer and feeds what comes back into the
//! event store. If a failure returned `[]`, the daemon would see exactly what
//! it sees on a quiet Tuesday — nothing to report — and the circuit breaker
//! would never trip. A connector whose token expired in October would then go
//! silent for the rest of term while `ea status` showed it green. Every error
//! in this file therefore propagates: `Err` to the caller, `is_error: true` on
//! the wire, and a breaker that trips.
//!
//! # Why undated assignments are skipped
//!
//! An assignment with no `due_at` is not a deadline. Canvas courses carry
//! plenty of them — optional reading, ungraded practice, placeholder items the
//! teacher never dated. Emitting them as events would put items with nothing to
//! be late for in front of triage, and triage's whole job is to decide what is
//! urgent.
//!
//! # Why stale deadlines are skipped
//!
//! Canvas routinely reports an enrolment as `enrollment_state=active` long
//! after its term has ended, so `list_courses` cannot be trusted to mean
//! "courses I am taking now". On a connector's first poll, dedup by
//! `external_id` does not help — every assignment from every such course is
//! genuinely new — so without a date floor the very first run can dump years
//! of finished coursework into triage as if it were fresh. That spends this
//! system's whole premise (it earns the right to notify by not being noisy)
//! in one poll. [`STALE_DEADLINE_WINDOW`] bounds how far into the past a
//! `due_at` may be and still be emitted; there is deliberately no forward
//! bound (see its doc comment).

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::client::{Assignment, CanvasClient, Course};

/// The `kind` every watch entry carries. Triage mutes and keywords match on
/// it, so it is a stable name rather than something derived per assignment.
pub const WATCH_KIND: &str = "assignment";

/// A tool named in `policy.toml` that this server deliberately does **not**
/// implement.
///
/// `submit_assignment` is declared `deny` so the door is shut before anyone
/// opens it. The gate matches a policy rule by name, so the rule is in force
/// the moment such a tool appears, rather than after somebody remembers to
/// review the policy. A session must never be able to hand in coursework.
pub const DELIBERATE_PLACEHOLDERS: &[&str] = &["submit_assignment"];

/// How far into the past a `due_at` may be and still be reported by
/// [`CanvasServer::poll`].
///
/// Canvas's `enrollment_state=active` filter does not reliably mean "current
/// term" — a finished course's enrolment routinely stays active — so a fresh
/// poll can otherwise turn up years of past deadlines, each one genuinely new
/// to dedup. Fourteen days is generous for "a deadline I might still care
/// about" (a due date missed over a long weekend, during an outage, or before
/// the daemon was first run) while excluding a stale term's worth of old
/// coursework. The window is inclusive: an assignment due exactly fourteen
/// days ago still counts, so the boundary favors emitting over hiding.
///
/// There is deliberately no forward bound. A future `due_at` in a course the
/// token's owner is actively enrolled in is exactly the deadline this
/// connector exists to surface, however far out it sits (a syllabus posted at
/// the start of term, say); hiding it would defeat the connector's purpose,
/// not protect triage from noise. And because this floor is evaluated fresh
/// against the current time on every poll rather than latched at first sight,
/// nothing needs a matching forward cutoff to "eventually surface": every
/// dated assignment is emitted the first time it is polled for, whether that
/// is today or six months from now.
pub const STALE_DEADLINE_WINDOW: chrono::Duration = chrono::Duration::days(14);

/// One change, in the shape the daemon's event store records:
/// `(source, external_id)` is the idempotency key, `source` being the
/// connector name the daemon already knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub external_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// The stable external id for an assignment. Stable across polls, so a
/// deadline that has not moved is recorded once however often it is seen.
pub fn assignment_external_id(assignment_id: i64) -> String {
    format!("assignment:{assignment_id}")
}

/// Either a working client, or the reason there isn't one.
///
/// A connector with no credentials still starts and still completes the MCP
/// handshake; it just fails every call with a message naming the file to
/// create. Exiting at start-up instead would give the daemon "handshake
/// failed", which says nothing about what to do.
enum Backend {
    Ready(CanvasClient),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct CanvasServer {
    backend: Arc<Backend>,
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl CanvasServer {
    pub fn new(client: CanvasClient) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(client)),
            tool_router: Self::tool_router(),
        }
    }

    /// A server that will answer every call with `reason`.
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Unconfigured(reason.into())),
            tool_router: Self::tool_router(),
        }
    }

    fn client(&self) -> Result<&CanvasClient, String> {
        match &*self.backend {
            Backend::Ready(client) => Ok(client),
            Backend::Unconfigured(reason) => Err(format!(
                "canvas: this connector has no usable credentials, so it cannot read \
                 anything from Canvas. {reason}"
            )),
        }
    }

    /// [`poll_since`](Self::poll_since) against the wall clock.
    pub async fn poll(&self) -> Result<Vec<WatchEntry>, String> {
        self.poll_since(Utc::now()).await
    }

    /// Every dated assignment in every active course, as watch entries, whose
    /// due date is not more than [`STALE_DEADLINE_WINDOW`] in the past as of
    /// `now`.
    ///
    /// Not built on `list_upcoming`: a deadline that has just passed is still a
    /// change worth recording, and triage — not the connector — decides what is
    /// still interesting. The clock is a parameter, not `Utc::now()`, so the
    /// staleness floor is deterministic to test.
    pub async fn poll_since(&self, now: DateTime<Utc>) -> Result<Vec<WatchEntry>, String> {
        let client = self.client()?;
        let courses = client.list_courses().await.map_err(render)?;
        let floor = now - STALE_DEADLINE_WINDOW;

        let mut entries = Vec::new();
        for course in &courses {
            let assignments = client.list_assignments(course.id).await.map_err(render)?;
            for assignment in assignments {
                // The skip that matters: no due date, no deadline, no event.
                let Some(due_at) = assignment.due_at else {
                    continue;
                };
                // The staleness floor: see `STALE_DEADLINE_WINDOW`.
                if due_at < floor {
                    continue;
                }
                entries.push(entry(course, &assignment));
            }
        }
        Ok(entries)
    }
}

fn entry(course: &Course, assignment: &Assignment) -> WatchEntry {
    let due_at = assignment
        .due_at
        .map(|due| due.to_rfc3339_opts(SecondsFormat::Secs, true));
    WatchEntry {
        external_id: assignment_external_id(assignment.id),
        kind: WATCH_KIND.to_string(),
        payload: serde_json::json!({
            "course_id": assignment.course_id,
            "course_name": course.name,
            "course_code": course.course_code,
            "title": assignment.name,
            "due_at": due_at,
            "html_url": assignment.html_url,
        }),
    }
}

/// `anyhow` error -> the string the model reads. `{:#}` keeps the context
/// chain, which is where "listing assignments for course 7" lives.
fn render(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|err| format!("canvas: could not serialise the reply: {err}"))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAssignmentsArgs {
    /// The Canvas course id, as returned by `list_courses`.
    pub course_id: i64,
}

#[tool_router]
impl CanvasServer {
    #[tool(
        description = "List the Canvas courses the user is actively enrolled in, as a \
                       JSON array of { id, name, course_code }. Read-only."
    )]
    pub async fn list_courses(&self) -> Result<String, String> {
        let courses = self.client()?.list_courses().await.map_err(render)?;
        to_json(&courses)
    }

    #[tool(
        description = "List every assignment in one Canvas course, as a JSON array of \
                       { id, course_id, name, due_at, html_url }. `due_at` is null for an \
                       assignment with no deadline. Read-only."
    )]
    pub async fn list_assignments(
        &self,
        Parameters(ListAssignmentsArgs { course_id }): Parameters<ListAssignmentsArgs>,
    ) -> Result<String, String> {
        let assignments = self
            .client()?
            .list_assignments(course_id)
            .await
            .map_err(render)?;
        to_json(&assignments)
    }

    #[tool(
        description = "List every assignment across all active courses whose deadline is \
                       still in the future, earliest first. Read-only."
    )]
    pub async fn list_upcoming(&self) -> Result<String, String> {
        let upcoming = self.client()?.list_upcoming().await.map_err(render)?;
        to_json(&upcoming)
    }

    #[tool(
        description = "Poll Canvas for coursework deadlines. Returns a JSON array of \
                       { external_id, kind, payload }, one entry per assignment that has a \
                       due date; assignments with no due date, or whose due date is more than \
                       14 days in the past, are omitted. Called by the daemon on a timer; \
                       errors are reported rather than swallowed, so a broken connector is \
                       visible."
    )]
    pub async fn watch_poll(&self) -> Result<String, String> {
        let entries = self.poll().await?;
        to_json(&entries)
    }
}

#[tool_handler]
impl ServerHandler for CanvasServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Read-only access to the user's Canvas LMS (KTH): courses, assignments and \
             deadlines. Nothing here can change anything in Canvas — there is no way to \
             submit, comment or enrol, by design.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "canvas-token-DO-NOT-LEAK";

    /// `set_body_raw`: wiremock's `set_body_string` stamps `text/plain` over
    /// any content type inserted before it, and the client rejects non-JSON.
    fn json_page(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
    }

    fn server_for(mock: &MockServer) -> CanvasServer {
        CanvasServer::new(CanvasClient::new(&mock.uri(), TOKEN).unwrap())
    }

    async fn canvas_with(courses: serde_json::Value, assignments: serde_json::Value) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(courses))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/7/assignments"))
            .respond_with(json_page(assignments))
            .mount(&mock)
            .await;
        mock
    }

    fn one_course() -> serde_json::Value {
        serde_json::json!([{ "id": 7, "name": "XX1001", "course_code": "XX1001 HT25" }])
    }

    // --- watch_poll -------------------------------------------------------

    #[tokio::test]
    async fn watch_poll_returns_one_entry_per_dated_assignment() {
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                {
                    "id": 500, "course_id": 7, "name": "Lab 1",
                    "due_at": "2026-10-01T21:59:00Z",
                    "html_url": "https://canvas.kth.se/courses/7/assignments/500",
                },
                {
                    "id": 501, "course_id": 7, "name": "Lab 2",
                    "due_at": "2026-10-15T21:59:00Z",
                    "html_url": "https://canvas.kth.se/courses/7/assignments/501",
                },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll().await.unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].kind, WATCH_KIND);
        assert_eq!(entries[0].payload["title"], "Lab 1");
        assert_eq!(entries[0].payload["course_name"], "XX1001");
        assert_eq!(entries[0].payload["course_code"], "XX1001 HT25");
        assert_eq!(entries[0].payload["due_at"], "2026-10-01T21:59:00Z");
        assert_eq!(
            entries[0].payload["html_url"],
            "https://canvas.kth.se/courses/7/assignments/500"
        );
    }

    /// The skip that makes the difference between a deadline list and a course
    /// dump.
    #[tokio::test]
    async fn watch_poll_skips_assignments_with_no_due_date() {
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 500, "course_id": 7, "name": "Dated", "due_at": "2026-10-01T21:59:00Z" },
                { "id": 501, "course_id": 7, "name": "Undated", "due_at": null },
                { "id": 502, "course_id": 7, "name": "No field at all" },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll().await.unwrap();
        let ids: Vec<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["assignment:500"],
            "an undated assignment is not a deadline and must not become an event"
        );
    }

    // --- the stale-deadline floor ------------------------------------------

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().expect("a timestamp")
    }

    #[tokio::test]
    async fn an_assignment_due_yesterday_is_emitted() {
        let now = at("2026-09-24T00:00:00Z");
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 1, "course_id": 7, "name": "Due yesterday", "due_at": "2026-09-23T00:00:00Z" },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll_since(now).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "a deadline missed yesterday still matters: {entries:?}"
        );
    }

    #[tokio::test]
    async fn an_assignment_due_two_years_ago_is_not_emitted() {
        let now = at("2026-09-24T00:00:00Z");
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 1, "course_id": 7, "name": "Ancient", "due_at": "2024-09-24T00:00:00Z" },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll_since(now).await.unwrap();
        assert_eq!(
            entries.len(),
            0,
            "a finished term's stale-but-active enrolment must not flood the first poll: {entries:?}"
        );
    }

    /// The window is documented as inclusive: an assignment due exactly
    /// `STALE_DEADLINE_WINDOW` in the past still counts, so the boundary
    /// favors emitting over hiding.
    #[tokio::test]
    async fn an_assignment_exactly_at_the_stale_boundary_is_still_emitted() {
        let now = at("2026-09-24T00:00:00Z");
        let boundary = now - STALE_DEADLINE_WINDOW;
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 1, "course_id": 7, "name": "On the line", "due_at": boundary.to_rfc3339() },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll_since(now).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "exactly on the boundary must still be emitted: {entries:?}"
        );
    }

    #[tokio::test]
    async fn a_future_assignment_is_emitted() {
        let now = at("2026-09-24T00:00:00Z");
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 1, "course_id": 7, "name": "Six months out", "due_at": "2027-03-24T00:00:00Z" },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll_since(now).await.unwrap();
        assert_eq!(
            entries.len(),
            1,
            "there is no forward bound: a far-future deadline is still a deadline: {entries:?}"
        );
    }

    #[tokio::test]
    async fn watch_poll_external_ids_are_assignment_colon_id() {
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 91234, "course_id": 7, "name": "Lab", "due_at": "2026-10-01T21:59:00Z" },
            ]),
        )
        .await;

        let entries = server_for(&mock).poll().await.unwrap();
        assert_eq!(entries[0].external_id, "assignment:91234");
        assert_eq!(assignment_external_id(91234), "assignment:91234");
    }

    /// The one that keeps the breaker honest. An empty array here would be
    /// indistinguishable from "no deadlines", and a connector whose token
    /// expired would go quiet instead of complaining.
    #[tokio::test]
    async fn a_client_error_propagates_rather_than_becoming_an_empty_array() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"errors":[{"message":"user authorisation required"}]}"#,
                "application/json",
            ))
            .mount(&mock)
            .await;

        let server = server_for(&mock);

        let err = server
            .poll()
            .await
            .expect_err("a 401 must not look like 'nothing to report'");
        assert!(err.contains("401"), "{err}");
        assert!(!err.contains(TOKEN), "the error leaked the token: {err}");

        // And the same through the tool, which is what the daemon calls.
        let err = server
            .watch_poll()
            .await
            .expect_err("watch_poll must report the failure, not return []");
        assert!(err.contains("401"), "{err}");
        assert_ne!(err, "[]");
    }

    /// A failure part way through the walk is still a failure: a partial
    /// deadline list recorded as complete is worse than none.
    #[tokio::test]
    async fn a_failure_on_the_second_course_fails_the_poll() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses"))
            .respond_with(json_page(serde_json::json!([
                { "id": 7, "name": "XX1001", "course_code": "XX1001" },
                { "id": 8, "name": "XX1003", "course_code": "XX1003" },
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/7/assignments"))
            .respond_with(json_page(serde_json::json!([
                { "id": 1, "course_id": 7, "name": "Lab", "due_at": "2026-10-01T21:59:00Z" },
            ])))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/courses/8/assignments"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&mock)
            .await;

        let err = server_for(&mock)
            .poll()
            .await
            .expect_err("a half-read poll must not be reported as a complete one");
        assert!(err.contains("500"), "{err}");
    }

    #[tokio::test]
    async fn an_unconfigured_server_says_what_to_create_on_every_tool() {
        let server = CanvasServer::unconfigured(
            "no Canvas credentials at /home/x/.config/exec-agent/canvas/credentials.json",
        );
        for err in [
            server.watch_poll().await.expect_err("watch_poll"),
            server.list_courses().await.expect_err("list_courses"),
            server.list_upcoming().await.expect_err("list_upcoming"),
            server
                .list_assignments(Parameters(ListAssignmentsArgs { course_id: 7 }))
                .await
                .expect_err("list_assignments"),
        ] {
            assert!(err.contains("credentials.json"), "{err}");
            assert!(err.contains("no usable credentials"), "{err}");
        }
    }

    // --- the read-only tools ---------------------------------------------

    #[tokio::test]
    async fn list_courses_answers_json() {
        let mock = canvas_with(one_course(), serde_json::json!([])).await;
        let text = server_for(&mock).list_courses().await.unwrap();
        let parsed: Vec<Course> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0].name, "XX1001");
    }

    #[tokio::test]
    async fn list_assignments_answers_json_including_the_undated_ones() {
        let mock = canvas_with(
            one_course(),
            serde_json::json!([
                { "id": 500, "course_id": 7, "name": "Dated", "due_at": "2026-10-01T21:59:00Z" },
                { "id": 501, "course_id": 7, "name": "Undated", "due_at": null },
            ]),
        )
        .await;

        let text = server_for(&mock)
            .list_assignments(Parameters(ListAssignmentsArgs { course_id: 7 }))
            .await
            .unwrap();
        let parsed: Vec<Assignment> = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed.len(),
            2,
            "list_assignments reports the course as it is; only watch_poll filters"
        );
        assert_eq!(parsed[1].due_at, None);
    }

    // --- the tool surface, and its policy --------------------------------

    fn registered_tools() -> BTreeSet<String> {
        CanvasServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn policy_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/canvas/policy.toml")
            .canonicalize()
            .expect("connectors/canvas/policy.toml must exist")
    }

    fn policy_rules() -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(policy_path()).unwrap();
        let parsed: BTreeMap<String, BTreeMap<String, toml::Value>> =
            toml::from_str(&text).expect("policy.toml must parse");
        let section = parsed.get(crate::client::CONNECTOR).unwrap_or_else(|| {
            panic!(
                "policy.toml must have a [{}] section",
                crate::client::CONNECTOR
            )
        });
        section
            .iter()
            .map(|(tool, value)| {
                let mode = match value {
                    toml::Value::String(mode) => mode.clone(),
                    toml::Value::Table(table) => table
                        .get("mode")
                        .and_then(|m| m.as_str())
                        .expect("a table rule must have a mode")
                        .to_string(),
                    other => panic!("unexpected rule shape for {tool}: {other:?}"),
                };
                (tool.clone(), mode)
            })
            .collect()
    }

    #[test]
    fn the_server_registers_exactly_the_four_read_only_tools() {
        let tools = registered_tools();
        let expected: BTreeSet<String> = [
            "list_courses",
            "list_assignments",
            "list_upcoming",
            "watch_poll",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            tools, expected,
            "a new Canvas tool must be a deliberate act, with a policy rule to match"
        );
    }

    /// Direction one: nothing the server offers is unpoliced. A tool with no
    /// rule falls through to the gate's `approve` default, which is not the
    /// same as having been thought about.
    #[test]
    fn every_registered_tool_has_a_policy_rule() {
        let rules = policy_rules();
        for tool in registered_tools() {
            assert!(
                rules.contains_key(&tool),
                "tool {tool:?} has no rule in {}; add one",
                policy_path().display()
            );
        }
    }

    /// Direction two, and the one that catches the subtle failure: a rule left
    /// behind by a rename still parses, still looks deliberate, and applies to
    /// nothing at all — while the tool it used to govern now falls through to
    /// `approve`.
    #[test]
    fn every_policy_rule_names_a_registered_tool_or_a_deliberate_placeholder() {
        let tools = registered_tools();
        for (rule, mode) in policy_rules() {
            if tools.contains(&rule) {
                continue;
            }
            assert!(
                DELIBERATE_PLACEHOLDERS.contains(&rule.as_str()),
                "policy rule {rule:?} names no registered tool. If it is a rename \
                 leftover, delete it — it governs nothing while the renamed tool falls \
                 through to the approve default. If it is a door deliberately held shut, \
                 add it to DELIBERATE_PLACEHOLDERS."
            );
            assert_eq!(
                mode, "deny",
                "placeholder rule {rule:?} exists to forbid a tool that does not exist \
                 yet; anything but deny would pre-authorise it"
            );
        }
    }

    /// Named explicitly rather than left to the loop above: a session must
    /// never be able to hand in the user's coursework, and this is the line
    /// that says so.
    #[test]
    fn submitting_coursework_is_denied_before_any_such_tool_exists() {
        let rules = policy_rules();
        assert_eq!(
            rules.get("submit_assignment").map(String::as_str),
            Some("deny"),
            "canvas.submit_assignment must be denied in policy.toml"
        );
        assert!(
            !registered_tools().contains("submit_assignment"),
            "this connector is read-only; it must not implement submit_assignment"
        );
    }

    #[test]
    fn the_connector_manifest_matches_the_policy_section_and_the_binary_name() {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/canvas/connector.toml")
            .canonicalize()
            .expect("connectors/canvas/connector.toml must exist");
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["name"].as_str(), Some(crate::client::CONNECTOR));
        assert_eq!(manifest["command"].as_str(), Some("ea-canvas"));
        assert!(
            manifest["watch_interval_secs"].as_integer().unwrap() > 0,
            "the daemon polls on this interval"
        );
    }
}
