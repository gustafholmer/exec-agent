//! The MCP surface: five read/one-write tools plus `watch_poll`, over the
//! Calendar and Gmail clients.
//!
//! # Every tool names its account
//!
//! This connector serves two Google identities — `work` and `private` — behind
//! one binary, and there is no such thing as "the" account. Every tool except
//! [`GoogleServer::watch_poll`] therefore takes a required `account` argument
//! with no default, pinned by
//! `every_account_scoped_tool_requires_an_account_with_no_default`. A default
//! would be the worst possible failure: the model asks for mail, gets the
//! wrong mailbox, and every layer downstream — triage, the notification, the
//! owner reading it — has no way to tell.
//!
//! `watch_poll` is the deliberate exception, and cannot be otherwise: the
//! daemon calls it with `{}` on a timer (see `ea_daemon::jobs::run_watch_poll`)
//! and its whole job is to cover *every* authorised account at once. It takes
//! no arguments at all, which the same test asserts rather than leaves
//! implied.
//!
//! # Exactly one write, and sending is not it
//!
//! `create_draft` puts a draft in the owner's Drafts folder. Nothing here
//! sends mail, `auth::SCOPES` excludes `gmail.send` so Google would refuse it
//! anyway, and `connectors/google/policy.toml` denies a `send_mail` tool that
//! does not exist — the gate matches rules by name, so the door is shut before
//! anybody opens it.
//!
//! # Failing loudly
//!
//! Identical to Canvas: every error propagates as a tool error. See the
//! `watch` module docs for why an empty array on failure would leave a lapsed
//! token looking healthy forever.

use std::sync::Arc;

use chrono::Utc;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::calendar::{self, CalEvent, CalendarClient};
use crate::gmail::GmailClient;
use crate::watch::{self, WatchEntry};

/// Tools named in `policy.toml` that this server deliberately does **not**
/// implement.
///
/// `send_mail` is declared `deny` so that the rule is already in force the
/// moment such a tool appears, rather than after somebody remembers to review
/// the policy. The same reasoning as Canvas's `submit_assignment`: a session
/// must never be able to send mail as the owner.
pub const DELIBERATE_PLACEHOLDERS: &[&str] = &["send_mail"];

/// The one tool that takes no `account`, because the daemon calls it with
/// `{}` and it covers every authorised account. See the module docs.
pub const ACCOUNT_EXEMPT_TOOLS: &[&str] = &["watch_poll"];

/// Calendar window when a caller does not name one.
pub const DEFAULT_WINDOW_DAYS: u32 = 7;

/// The furthest ahead a caller may ask the calendar to look. A model that
/// asks for ten years would page through a decade of events at 250 per
/// request; there is no question worth that.
pub const MAX_WINDOW_DAYS: u32 = 90;

/// How many messages `list_mail` returns when a caller does not say. The
/// Gmail client caps its own ceiling separately.
pub const DEFAULT_MAIL_MAX: u32 = 25;

/// Everything a configured server needs. Held behind an `Arc` so the server
/// is `Clone` (rmcp requires it) without cloning an `Auth`, which owns the
/// per-account refresh locks and must be shared, not copied.
struct Ready {
    auth: Auth,
    calendar: CalendarClient,
    gmail: GmailClient,
}

/// Either a working set of clients, or the reason there isn't one.
///
/// A connector with no `app.json` still starts and still completes the MCP
/// handshake; it just fails every call with a message naming the file to
/// create. Exiting at start-up instead would reach the daemon as "handshake
/// failed", which tells nobody what to do.
enum Backend {
    /// Boxed only to keep the enum small: `Ready` holds two `reqwest`
    /// clients and the auth lock table, and the unconfigured variant is a
    /// string.
    Ready(Box<Ready>),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct GoogleServer {
    backend: Arc<Backend>,
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl GoogleServer {
    pub fn new(auth: Auth, calendar: CalendarClient, gmail: GmailClient) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(Box::new(Ready {
                auth,
                calendar,
                gmail,
            }))),
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

    fn ready(&self) -> Result<&Ready, String> {
        match &*self.backend {
            Backend::Ready(ready) => Ok(ready),
            Backend::Unconfigured(reason) => Err(format!(
                "google: this connector has no usable credentials, so it cannot read \
                 anything from Google. {reason}"
            )),
        }
    }

    /// Events on one account's primary calendar, from now to `days` ahead.
    async fn events_for(&self, account: &str, days: u32) -> Result<Vec<CalEvent>, String> {
        let ready = self.ready()?;
        let now = Utc::now();
        let window = chrono::Duration::days(i64::from(days.clamp(1, MAX_WINDOW_DAYS)));
        ready
            .calendar
            .list_events(&ready.auth, account, now, now + window)
            .await
            .map_err(render)
    }

    /// The accounts that have a stored grant, in a stable order. Read fresh on
    /// every poll rather than latched at start-up, so authorising a second
    /// account does not need a daemon restart.
    pub fn authorised_accounts(&self) -> Result<Vec<String>, String> {
        self.ready()?.auth.store().list().map_err(render)
    }

    /// What [`GoogleServer::watch_poll`] returns, before serialisation. Public
    /// so the tests can look at the rows rather than at a string.
    pub async fn poll(&self) -> Result<Vec<WatchEntry>, String> {
        let ready = self.ready()?;
        let accounts = self.authorised_accounts()?;
        watch::poll(
            &ready.auth,
            &ready.calendar,
            &ready.gmail,
            &accounts,
            Utc::now(),
        )
        .await
        .map_err(render)
    }
}

/// `anyhow` error -> the string the model reads. `{:#}` keeps the context
/// chain, which is where "polling the calendar of Google account \"private\""
/// lives.
fn render(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|err| format!("google: could not serialise the reply: {err}"))
}

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListEventsArgs {
    /// Which Google account to read: `work` or `private`. Required — there is
    /// no default account.
    pub account: String,
    /// How many days ahead to look. Defaults to 7, capped at 90.
    pub days: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindConflictsArgs {
    /// Which Google account to read: `work` or `private`. Required — there is
    /// no default account.
    pub account: String,
    /// Further accounts to merge into the same scan. Pass the other account
    /// here to find a clash between a work meeting and a private
    /// appointment — the case worth looking for.
    pub other_accounts: Option<Vec<String>>,
    /// How many days ahead to look. Defaults to 7, capped at 90.
    pub days: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMailArgs {
    /// Which Google account to read: `work` or `private`. Required — there is
    /// no default account.
    pub account: String,
    /// Gmail search syntax, e.g. `is:unread` or `from:prof@kth.se`. Defaults
    /// to `is:unread`; pass an empty string for no filter.
    pub query: Option<String>,
    /// How many messages to return. Defaults to 25.
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMailArgs {
    /// Which Google account to read: `work` or `private`. Required — there is
    /// no default account.
    pub account: String,
    /// The Gmail message id, as returned by `list_mail`.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateDraftArgs {
    /// Which Google account the draft is written in: `work` or `private`.
    /// Required — there is no default account, and a draft written in the
    /// wrong identity is a mistake nobody downstream can see.
    pub account: String,
    /// Recipient address. Must not contain a newline.
    pub to: String,
    /// Subject line. Must not contain a newline.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

#[tool_router]
impl GoogleServer {
    #[tool(
        description = "List events on one Google account's primary calendar, from now up to \
                       `days` ahead (default 7, max 90), as a JSON array of \
                       { id, account, title, start, end, all_day, location, attendees, \
                       html_link, updated }. Read-only. `account` is required: this connector \
                       serves several Google identities and there is no default one."
    )]
    pub async fn list_events(
        &self,
        Parameters(ListEventsArgs { account, days }): Parameters<ListEventsArgs>,
    ) -> Result<String, String> {
        let events = self
            .events_for(&account, days.unwrap_or(DEFAULT_WINDOW_DAYS))
            .await?;
        to_json(&events)
    }

    #[tool(
        description = "Find overlapping calendar events. Scans `account`, plus any accounts \
                       named in `other_accounts`, as one merged calendar — pass the other \
                       account to catch a work meeting clashing with a private appointment. \
                       All-day events are excluded and back-to-back meetings are not a clash. \
                       Returns a JSON array of { external_id, kind, payload }, the same rows \
                       watch_poll would report. Read-only."
    )]
    pub async fn find_conflicts(
        &self,
        Parameters(FindConflictsArgs {
            account,
            other_accounts,
            days,
        }): Parameters<FindConflictsArgs>,
    ) -> Result<String, String> {
        let days = days.unwrap_or(DEFAULT_WINDOW_DAYS);

        let mut accounts = vec![account];
        for other in other_accounts.unwrap_or_default() {
            if !accounts.contains(&other) {
                accounts.push(other);
            }
        }

        let mut events = Vec::new();
        for account in &accounts {
            events.extend(self.events_for(account, days).await?);
        }

        let entries: Vec<WatchEntry> = calendar::find_conflicts(&events)
            .iter()
            .map(|(a, b)| watch::conflict_entry(a, b))
            .collect();
        to_json(&entries)
    }

    #[tool(
        description = "List messages in one Google account's mailbox matching a Gmail search \
                       query (default `is:unread`), newest first, as a JSON array of \
                       { id, thread_id, account, from, subject, snippet, body, received_at, \
                       labels }. Read-only. `account` is required: there is no default mailbox."
    )]
    pub async fn list_mail(
        &self,
        Parameters(ListMailArgs {
            account,
            query,
            max,
        }): Parameters<ListMailArgs>,
    ) -> Result<String, String> {
        let ready = self.ready()?;
        let query = query.unwrap_or_else(|| watch::UNREAD_QUERY.to_string());
        let max = max.unwrap_or(DEFAULT_MAIL_MAX) as usize;
        let mail = ready
            .gmail
            .list_recent(&ready.auth, &account, &query, max)
            .await
            .map_err(render)?;
        to_json(&mail)
    }

    #[tool(
        description = "Read one message in full from one Google account's mailbox, by the id \
                       list_mail returned, as { id, thread_id, account, from, subject, snippet, \
                       body, received_at, labels }. Read-only. `account` is required, and must \
                       be the account the id came from."
    )]
    pub async fn get_mail(
        &self,
        Parameters(GetMailArgs { account, id }): Parameters<GetMailArgs>,
    ) -> Result<String, String> {
        let ready = self.ready()?;
        let mail = ready
            .gmail
            .get(&ready.auth, &account, &id)
            .await
            .map_err(render)?;
        to_json(&mail)
    }

    #[tool(
        description = "Create a plain-text draft in one Google account's Drafts folder and \
                       return its id. This is the only tool in this connector that writes \
                       anything. It does NOT send: nothing here can send mail, the OAuth \
                       scopes exclude sending, and the policy denies a send tool by name. \
                       `account` is required — a draft written in the wrong identity is a \
                       mistake nobody downstream can see."
    )]
    pub async fn create_draft(
        &self,
        Parameters(CreateDraftArgs {
            account,
            to,
            subject,
            body,
        }): Parameters<CreateDraftArgs>,
    ) -> Result<String, String> {
        let ready = self.ready()?;
        let id = ready
            .gmail
            .create_draft(&ready.auth, &account, &to, &subject, &body)
            .await
            .map_err(render)?;
        to_json(&serde_json::json!({ "draft_id": id, "account": account }))
    }

    #[tool(
        description = "Poll Google for changes across every authorised account. Returns a JSON \
                       array of { external_id, kind, payload }: one entry per upcoming calendar \
                       event (kind `calendar_event`), one per pair of overlapping events (kind \
                       `calendar_conflict`, computed across accounts), and one per unread \
                       message (kind `mail`). Takes no arguments — it covers every account by \
                       design. Called by the daemon on a timer; errors are reported rather than \
                       swallowed, so a broken connector is visible."
    )]
    pub async fn watch_poll(&self) -> Result<String, String> {
        let entries = self.poll().await?;
        to_json(&entries)
    }
}

#[tool_handler]
impl ServerHandler for GoogleServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Google Calendar and Gmail for the owner's several Google identities (typically \
             `work` and `private`). Every tool takes a required `account` argument; there is \
             no default account. Everything is read-only except `create_draft`, which puts a \
             draft in the Drafts folder. Nothing here can send mail, and nothing here can \
             change a calendar.",
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::{RefreshBackend, RefreshError, RefreshResponse, TokenStore, Tokens};

    const ACCESS_TOKEN: &str = "ya29.TOOLS-ACCESS-do-not-leak";

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
            Box::pin(async { panic!("a tool call must not need to refresh a healthy token") })
        }
    }

    fn auth_for(dir: &Path, accounts: &[&str]) -> Auth {
        let store = TokenStore::new(Some(dir.to_path_buf()));
        for account in accounts {
            store
                .write(
                    account,
                    &Tokens {
                        access_token: ACCESS_TOKEN.to_string(),
                        refresh_token: "1//not-used-here".to_string(),
                        expiry: Utc::now() + chrono::Duration::hours(1),
                        scope: crate::auth::SCOPES.join(" "),
                    },
                )
                .unwrap();
        }
        Auth::with_backend(store, Arc::new(UnusedBackend))
    }

    fn server_for(mock: &MockServer, dir: &Path, accounts: &[&str]) -> GoogleServer {
        GoogleServer::new(
            auth_for(dir, accounts),
            CalendarClient::new(&mock.uri()).unwrap(),
            GmailClient::new(&mock.uri()).unwrap(),
        )
    }

    fn json(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json; charset=utf-8")
    }

    // -----------------------------------------------------------------------
    // The tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_events_reads_the_named_account() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path_matcher("/calendars/primary/events"))
            .respond_with(json(serde_json::json!({
                "items": [{
                    "id": "w1",
                    "summary": "Lecture",
                    "start": { "dateTime": "2026-09-25T10:00:00Z" },
                    "end": { "dateTime": "2026-09-25T12:00:00Z" },
                }],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .list_events(Parameters(ListEventsArgs {
                account: "work".into(),
                days: None,
            }))
            .await
            .unwrap();
        let events: Vec<CalEvent> = serde_json::from_str(&text).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].account, "work");
        assert_eq!(events[0].title, "Lecture");
    }

    #[tokio::test]
    async fn find_conflicts_merges_the_accounts_it_is_given() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        // Both accounts hit the same mock, so both see both events — enough to
        // prove the merge happens and the rows come back in the watch shape.
        Mock::given(method("GET"))
            .and(path_matcher("/calendars/primary/events"))
            .respond_with(json(serde_json::json!({
                "items": [
                    {
                        "id": "a",
                        "summary": "One",
                        "start": { "dateTime": "2026-09-25T10:00:00Z" },
                        "end": { "dateTime": "2026-09-25T12:00:00Z" },
                    },
                    {
                        "id": "b",
                        "summary": "Two",
                        "start": { "dateTime": "2026-09-25T11:00:00Z" },
                        "end": { "dateTime": "2026-09-25T11:30:00Z" },
                    },
                ],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work", "private"])
            .find_conflicts(Parameters(FindConflictsArgs {
                account: "work".into(),
                other_accounts: Some(vec!["private".into()]),
                days: Some(3),
            }))
            .await
            .unwrap();
        let entries: Vec<WatchEntry> = serde_json::from_str(&text).unwrap();
        assert!(!entries.is_empty(), "the overlap must be reported");
        assert!(entries
            .iter()
            .all(|e| e.kind == crate::watch::KIND_CONFLICT));
        let cross = entries
            .iter()
            .find(|e| e.payload["cross_account"] == true)
            .expect("a work/private clash must be found when both accounts are scanned");
        assert!(cross.external_id.starts_with("gconflict:"));
    }

    #[tokio::test]
    async fn create_draft_returns_the_draft_id_and_the_account() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path_matcher("/users/me/drafts"))
            .respond_with(json(serde_json::json!({ "id": "draft-9" })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .create_draft(Parameters(CreateDraftArgs {
                account: "work".into(),
                to: "friend@example.com".into(),
                subject: "Hej".into(),
                body: "Hello".into(),
            }))
            .await
            .unwrap();
        let reply: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["draft_id"], "draft-9");
        assert_eq!(reply["account"], "work");
    }

    #[tokio::test]
    async fn watch_poll_covers_every_authorised_account() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path_matcher("/calendars/primary/events"))
            .respond_with(json(serde_json::json!({ "items": [] })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/users/me/messages"))
            .respond_with(json(serde_json::json!({
                "messages": [{ "id": "m1", "threadId": "t1" }],
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/users/me/messages/m1"))
            .respond_with(json(serde_json::json!({
                "id": "m1",
                "threadId": "t1",
                "labelIds": ["UNREAD"],
                "internalDate": "1700000000000",
            })))
            .mount(&mock)
            .await;

        let entries = server_for(&mock, tmp.path(), &["private", "work"])
            .poll()
            .await
            .unwrap();
        let ids: BTreeSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            ["gmail:private:m1", "gmail:work:m1"]
                .into_iter()
                .collect::<BTreeSet<&str>>(),
            "both authorised accounts must be polled: {entries:#?}"
        );
    }

    /// The daemon calls `watch_poll` with `{}`; if the accounts it should
    /// cover are unknown, that is a failure, not a quiet success.
    #[tokio::test]
    async fn watch_poll_on_a_server_with_no_authorised_accounts_errors() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let err = server_for(&mock, tmp.path(), &[])
            .watch_poll()
            .await
            .expect_err("no accounts must be loud");
        assert!(err.contains("ea-google-authorize"), "{err}");
        assert_ne!(err, "[]");
    }

    #[tokio::test]
    async fn an_unconfigured_server_says_what_to_create_on_every_tool() {
        let server = GoogleServer::unconfigured(
            "no Google OAuth client at /home/x/.config/exec-agent/google/app.json",
        );
        for err in [
            server.watch_poll().await.expect_err("watch_poll"),
            server
                .list_events(Parameters(ListEventsArgs {
                    account: "work".into(),
                    days: None,
                }))
                .await
                .expect_err("list_events"),
            server
                .find_conflicts(Parameters(FindConflictsArgs {
                    account: "work".into(),
                    other_accounts: None,
                    days: None,
                }))
                .await
                .expect_err("find_conflicts"),
            server
                .list_mail(Parameters(ListMailArgs {
                    account: "work".into(),
                    query: None,
                    max: None,
                }))
                .await
                .expect_err("list_mail"),
            server
                .get_mail(Parameters(GetMailArgs {
                    account: "work".into(),
                    id: "m1".into(),
                }))
                .await
                .expect_err("get_mail"),
            server
                .create_draft(Parameters(CreateDraftArgs {
                    account: "work".into(),
                    to: "a@b.c".into(),
                    subject: "s".into(),
                    body: "b".into(),
                }))
                .await
                .expect_err("create_draft"),
        ] {
            assert!(err.contains("app.json"), "{err}");
            assert!(err.contains("no usable credentials"), "{err}");
        }
    }

    // -----------------------------------------------------------------------
    // The tool surface, its schemas, and its policy
    // -----------------------------------------------------------------------

    fn registered_tools() -> BTreeSet<String> {
        GoogleServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn input_schemas() -> BTreeMap<String, serde_json::Value> {
        GoogleServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    serde_json::Value::Object((*tool.input_schema).clone()),
                )
            })
            .collect()
    }

    fn connector_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/google")
            .canonicalize()
            .expect("connectors/google must exist")
    }

    fn policy_path() -> PathBuf {
        connector_dir().join("policy.toml")
    }

    fn policy_rules() -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(policy_path()).unwrap();
        let parsed: BTreeMap<String, BTreeMap<String, toml::Value>> =
            toml::from_str(&text).expect("policy.toml must parse");
        let section = parsed.get(crate::auth::CONNECTOR).unwrap_or_else(|| {
            panic!(
                "policy.toml must have a [{}] section",
                crate::auth::CONNECTOR
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
    fn the_server_registers_exactly_the_six_planned_tools() {
        let expected: BTreeSet<String> = [
            "list_events",
            "find_conflicts",
            "list_mail",
            "get_mail",
            "create_draft",
            "watch_poll",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            registered_tools(),
            expected,
            "a new Google tool must be a deliberate act, with a policy rule to match"
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

    /// The whole multi-account design in one assertion. A tool that defaults
    /// its account quietly reads the wrong mailbox, and nothing downstream
    /// would notice.
    ///
    /// `watch_poll` is exempt, and the exemption is itself checked below: the
    /// daemon calls it with `{}`, so a required argument there would break
    /// every poll.
    #[test]
    fn every_account_scoped_tool_requires_an_account_with_no_default() {
        for (name, schema) in input_schemas() {
            if ACCOUNT_EXEMPT_TOOLS.contains(&name.as_str()) {
                continue;
            }

            let required: Vec<&str> = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("tool {name:?} has no `required` list: {schema:#}"))
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                required.contains(&"account"),
                "tool {name:?} does not require an `account`: {schema:#}"
            );

            let account = &schema["properties"]["account"];
            assert_eq!(
                account["type"], "string",
                "tool {name:?}'s account must be a plain string: {schema:#}"
            );
            assert!(
                account.get("default").is_none(),
                "tool {name:?} gives `account` a default. There is no default Google \
                 account: a default silently reads the wrong mailbox. {schema:#}"
            );
        }
    }

    /// The exemption, pinned rather than assumed: `ea_daemon::jobs` calls
    /// `watch_poll` with `{}`, so it must require nothing at all.
    #[test]
    fn watch_poll_requires_no_arguments_because_the_daemon_calls_it_with_an_empty_object() {
        let schema = &input_schemas()["watch_poll"];
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.len())
            .unwrap_or(0);
        assert_eq!(
            required, 0,
            "the daemon polls with {{}}; a required argument here breaks every poll: {schema:#}"
        );
    }

    /// Named explicitly rather than left to the loop above: a session must
    /// never be able to send mail as the owner, and this is the line that says
    /// so.
    #[test]
    fn sending_mail_is_denied_before_any_such_tool_exists() {
        assert_eq!(
            policy_rules().get("send_mail").map(String::as_str),
            Some("deny"),
            "google.send_mail must be denied in policy.toml"
        );
        assert!(
            !registered_tools().contains("send_mail"),
            "this connector drafts but never sends; it must not implement send_mail"
        );
    }

    #[test]
    fn creating_a_draft_needs_a_human() {
        assert_eq!(
            policy_rules().get("create_draft").map(String::as_str),
            Some("approve"),
            "the one write in this connector must not be auto"
        );
    }

    /// Phase 1's loader refuses a connector whose declared name is not its
    /// directory's basename, and refuses two connectors claiming one name.
    #[test]
    fn the_connector_manifest_is_named_for_its_directory() {
        let dir = connector_dir();
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("connector.toml")).unwrap()).unwrap();

        let basename = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            manifest["name"].as_str(),
            Some(basename),
            "the daemon requires a connector's name to equal its directory's basename"
        );
        assert_eq!(manifest["name"].as_str(), Some(crate::auth::CONNECTOR));
        assert_eq!(manifest["command"].as_str(), Some("ea-google"));
        assert_eq!(
            manifest["watch_interval_secs"].as_integer(),
            Some(120),
            "the Gmail watcher's cadence; see connector.toml"
        );
    }
}
