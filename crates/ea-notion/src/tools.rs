//! The MCP surface: four reads, two writes, and `watch_poll`, over the
//! per-workspace token store.
//!
//! # Every tool names its workspace
//!
//! This connector serves several Notion workspaces behind one binary — one
//! integration token per workspace, because a Notion integration is created
//! *inside* a workspace and can never see another. There is no such thing as
//! "the" workspace, so every tool except [`NotionServer::watch_poll`] takes a
//! required `workspace` argument with no default, pinned by
//! `every_workspace_scoped_tool_requires_a_workspace_with_no_default`.
//!
//! A default would be the worst available failure for the two writes: the
//! model files a note, it lands in the other workspace, and every layer
//! downstream — the reply, the notification, the owner reading it — sees a
//! success. `create_page` is `auto` in the policy precisely because a new page
//! is cheap to undo; that argument only holds if the page lands where the
//! caller meant it to.
//!
//! `watch_poll` is the deliberate exception and cannot be otherwise: the
//! daemon calls it with `{}` on a timer (see `ea_daemon::jobs::run_watch_poll`)
//! and its whole job is to cover *every* configured workspace at once. The
//! exemption list is pinned to that one member by
//! `only_watch_poll_is_exempt_from_the_required_workspace`, so buying a second
//! tool an exemption means deliberately editing a test.
//!
//! # Two writes, and one of them is `auto`
//!
//! `create_page` is the first write in this project that runs without a human
//! tap, and the reasoning is in `connectors/notion/policy.toml` as well as
//! here so that a reader meeting it in either place sees it argued rather than
//! assumed: a new page is additive. It changes nothing that already exists,
//! it is one click to delete, and an assistant that must ask permission before
//! filing a note is an assistant that never files notes — which is the
//! feature.
//!
//! `append_to_page` is `approve`, because it edits a page the owner already
//! maintains. The difference is not the API call's blast radius; it is whose
//! work is at stake.
//!
//! `delete_page` and `archive_page` do not exist and are denied by name. The
//! gate matches rules by name, so the door is shut before anybody builds one.
//!
//! # Why an append never spans two requests
//!
//! [`crate::client::NotionClient::append_blocks`] chunks at Notion's 100-child
//! limit and is **not atomic across chunks**: a failure on chunk 2 leaves
//! chunk 1 on the page, and re-calling with the same input appends chunk 1
//! twice. Rather than rely on nobody ever retrying, this layer refuses more
//! than [`MAX_BLOCKS`] blocks in one call, which is exactly the chunk size —
//! so every append this server performs is one `PATCH`, and one `PATCH`
//! either happened or did not. See
//! `an_append_is_capped_at_one_requests_worth_of_blocks`.
//!
//! # Failing loudly
//!
//! Canvas's rule: every error propagates as a tool error. See the `watch`
//! module docs for why an empty array on failure would leave a revoked token
//! looking healthy forever.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{setup_instructions, TokenStore, CONNECTOR};
use crate::client::{page_title, NotionClient, Parent};
use crate::watch::{self, WatchEntry};

/// Tools named in `policy.toml` that this server deliberately does **not**
/// implement.
///
/// Both are declared `deny` so the rule is already in force the moment such a
/// tool appears, rather than after somebody remembers to review the policy. A
/// session may add to the owner's Notion; it may never remove from it.
pub const DELIBERATE_PLACEHOLDERS: &[&str] = &["delete_page", "archive_page"];

/// The one tool that takes no `workspace`, because the daemon calls it with
/// `{}` and it covers every configured workspace. See the module docs.
pub const WORKSPACE_EXEMPT_TOOLS: &[&str] = &["watch_poll"];

/// The most blocks one `create_page` or `append_to_page` may write.
///
/// Notion's own limit on a `children` array, and deliberately the same number:
/// at this cap `append_blocks` never splits a call into two requests, so a
/// failed append is a failed append rather than a half-written page that a
/// retry would duplicate. See the module docs.
pub const MAX_BLOCKS: usize = 100;

/// The most characters one paragraph block may hold. Notion refuses a longer
/// `text.content`; a long line is split across blocks rather than rejected,
/// because the caller is a language model writing prose, not a client that
/// knows the limit.
pub const MAX_TEXT_CHARS: usize = 2000;

/// Everything a configured server needs. Held behind an `Arc` so the server is
/// `Clone` (rmcp requires it) without copying the store on every call.
struct Ready {
    store: TokenStore,
    /// The Notion API base. A field rather than a constant so the tests can
    /// point the whole server at a `wiremock` server.
    base_url: String,
}

/// Either a usable store, or the reason there isn't one.
///
/// A connector with no credentials still starts and still completes the MCP
/// handshake; it just fails every call with a message naming the file to
/// create. Exiting at start-up instead would reach the daemon as "handshake
/// failed", which tells nobody what to do.
///
/// Note what is deliberately *not* latched here: which workspaces exist. They
/// are read off disk on every call, so adding `work.json` while the daemon
/// runs needs no restart — and a poll with no workspaces fails loudly on its
/// own rather than reporting a quiet week.
enum Backend {
    Ready(Box<Ready>),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct NotionServer {
    backend: Arc<Backend>,
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl NotionServer {
    pub fn new(store: TokenStore, base_url: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(Box::new(Ready {
                store,
                base_url: base_url.into(),
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
                "{CONNECTOR}: this connector has no usable credentials, so it cannot read \
                 anything from Notion. {reason}"
            )),
        }
    }

    /// A client bound to one workspace's token, read fresh off disk.
    ///
    /// The label reaches the filesystem only through `TokenStore`, which
    /// validates it before building a path — so a model that asks for
    /// workspace `../../id_rsa` is refused before a file is opened.
    fn client_for(&self, workspace: &str) -> Result<NotionClient, String> {
        let ready = self.ready()?;
        let creds = ready.store.read(workspace).map_err(render)?;
        NotionClient::with_base_url(&ready.base_url, creds.token)
            .and_then(|client| client.with_workspace(workspace))
            .map_err(|err| err.to_string())
    }

    /// The workspaces that have a credential file, in a stable order. Read
    /// fresh on every poll rather than latched at start-up.
    pub fn workspaces(&self) -> Result<Vec<String>, String> {
        self.ready()?.store.list().map_err(render)
    }

    /// What [`NotionServer::watch_poll`] returns, before serialisation. Public
    /// so the tests can look at the rows rather than at a string.
    pub async fn poll(&self) -> Result<Vec<WatchEntry>, String> {
        self.poll_at(Utc::now()).await
    }

    /// [`poll`](Self::poll) against a fixed clock.
    pub async fn poll_at(&self, now: DateTime<Utc>) -> Result<Vec<WatchEntry>, String> {
        let ready = self.ready()?;
        let workspaces = self.workspaces()?;

        // Not an empty poll. A connector nobody has given a token to would
        // otherwise report "nothing to tell you" forever and look healthy
        // doing it — the exact failure `watch_poll` exists to make visible.
        if workspaces.is_empty() {
            let example = ready.store.path_for("personal").map_err(render)?;
            return Err(format!(
                "{CONNECTOR}: no Notion workspace is configured in {}, so there is nothing \
                 to poll.\n{}",
                ready.store.dir().display(),
                setup_instructions("personal", &example)
            ));
        }

        let mut clients = Vec::with_capacity(workspaces.len());
        for workspace in &workspaces {
            clients.push(self.client_for(workspace)?);
        }

        watch::poll(&clients, now).await.map_err(render)
    }
}

/// `anyhow` error -> the string the model reads. `{:#}` keeps the context
/// chain, which is where "querying data source \"Tasks\" in Notion workspace
/// \"work\"" lives.
fn render(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|err| format!("{CONNECTOR}: could not serialise the reply: {err}"))
}

/// Plain text -> Notion paragraph blocks.
///
/// One block per non-empty line, each split at [`MAX_TEXT_CHARS`] because
/// Notion refuses a longer `text.content`. The split walks characters rather
/// than bytes: slicing a UTF-8 string at byte 2000 panics on a multi-byte
/// boundary, and Swedish prose has one every few words.
pub fn paragraph_blocks(text: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(MAX_TEXT_CHARS) {
            let content: String = chunk.iter().collect();
            blocks.push(json!({
                "object": "block",
                "type": "paragraph",
                "paragraph": {
                    "rich_text": [ { "type": "text", "text": { "content": content } } ],
                },
            }));
        }
    }
    blocks
}

/// The cap that keeps every write to a single request. See the module docs.
fn check_block_count(tool: &str, blocks: &[Value]) -> Result<(), String> {
    if blocks.len() > MAX_BLOCKS {
        return Err(format!(
            "{CONNECTOR}: {tool} was given text that renders as {} paragraph blocks; the \
             limit is {MAX_BLOCKS}, which is Notion's own per-request maximum. A longer \
             write would have to be split across two requests, and a failure on the second \
             would leave the first already written with no way to retry safely. Send less \
             text, or make several calls and check each one.",
            blocks.len()
        ));
    }
    Ok(())
}

/// A search result, flattened to the four fields a model needs to decide what
/// to do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub object: String,
    pub title: String,
    pub url: Option<String>,
}

fn page_hit(value: &Value) -> SearchHit {
    SearchHit {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        object: watch::OBJECT_PAGE.to_string(),
        title: page_title(value.get("properties").unwrap_or(&Value::Null)),
        url: value.get("url").and_then(Value::as_str).map(str::to_string),
    }
}

fn data_source_hit(value: &Value) -> SearchHit {
    SearchHit {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        object: watch::OBJECT_DATA_SOURCE.to_string(),
        title: watch::object_title(value),
        url: value.get("url").and_then(Value::as_str).map(str::to_string),
    }
}

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Which Notion workspace to read, e.g. `work` or `personal`. Required —
    /// there is no default workspace.
    pub workspace: String,
    /// Title text to match. Omit to list everything the integration has been
    /// shared with, which is what a survey of the workspace wants.
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetPageArgs {
    /// Which Notion workspace the page lives in. Required — there is no
    /// default workspace, and a page id from one workspace does not resolve in
    /// another.
    pub workspace: String,
    /// The page id, as a UUID. Copy it from the page URL or from a `search`
    /// result's `id`.
    pub page_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryDatabaseArgs {
    /// Which Notion workspace the database lives in. Required — there is no
    /// default workspace.
    pub workspace: String,
    /// The database id, as a UUID. This is the id in a database's URL. To
    /// query one data source of a multi-source database directly, pass its id
    /// from a `search` result instead.
    pub database_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreatePageArgs {
    /// Which Notion workspace the page is created in. Required — there is no
    /// default workspace, and a note filed in the wrong one is a mistake
    /// nobody downstream can see.
    pub workspace: String,
    /// The page's title.
    pub title: String,
    /// Create the page as a child of this page. Give exactly one of
    /// `parent_page_id` and `parent_database_id`.
    pub parent_page_id: Option<String>,
    /// Create the page as a row in this database. Give exactly one of
    /// `parent_page_id` and `parent_database_id`.
    pub parent_database_id: Option<String>,
    /// The page's body, as plain text. One paragraph per line; blank lines are
    /// dropped. Optional — a titled page with no body is a valid note.
    pub content: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppendToPageArgs {
    /// Which Notion workspace the page lives in. Required — there is no
    /// default workspace.
    pub workspace: String,
    /// The page to append to, as a UUID.
    pub page_id: String,
    /// The text to append. One paragraph per line; blank lines are dropped.
    pub content: String,
}

#[tool_router]
impl NotionServer {
    #[tool(
        description = "Search one Notion workspace for pages and databases whose title \
                       matches `query` (omit it to list everything the integration can see). \
                       Returns { pages: [...], data_sources: [...], unknown: [...] }, each a \
                       list of { id, object, title, url }: Notion's search mixes the two \
                       object types in one list and they are NOT interchangeable — a data \
                       source is a table you query with query_database, a page is a document \
                       you read with get_page. Read-only. `workspace` is required: this \
                       connector serves several Notion workspaces and there is no default \
                       one. An empty result usually means the integration has not been \
                       shared with anything — a Notion token alone sees nothing."
    )]
    pub async fn search(
        &self,
        Parameters(SearchArgs { workspace, query }): Parameters<SearchArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&workspace)?;
        let results = client
            .search(query.as_deref(), None)
            .await
            .map_err(|err| err.to_string())?;

        let split = watch::split_search_results(results);
        let pages: Vec<SearchHit> = split.pages.iter().map(page_hit).collect();
        let data_sources: Vec<SearchHit> = split.data_sources.iter().map(data_source_hit).collect();

        to_json(&json!({
            "workspace": workspace,
            "pages": pages,
            "data_sources": data_sources,
            // Not silently merged into either list: an object type this
            // connector does not know is reported as what it is.
            "unknown": split.other,
        }))
    }

    #[tool(
        description = "Read one Notion page, as { workspace, id, title, url, page }, where \
                       `page` is Notion's raw page object including its properties. \
                       Read-only. `workspace` is required, and must be the workspace the id \
                       came from. A page that has not been shared with the integration \
                       answers 404 just like a page that does not exist."
    )]
    pub async fn get_page(
        &self,
        Parameters(GetPageArgs { workspace, page_id }): Parameters<GetPageArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&workspace)?;
        let page = client
            .fetch_page(&page_id)
            .await
            .map_err(|err| err.to_string())?;

        to_json(&json!({
            "workspace": workspace,
            "id": page.get("id"),
            "title": page_title(page.get("properties").unwrap_or(&Value::Null)),
            "url": page.get("url"),
            "page": page,
        }))
    }

    #[tool(
        description = "List every row of one Notion database, as a JSON array of \
                       { id, title, url, properties }. Under the API version this connector \
                       pins, a database is a container for one or more data sources and the \
                       rows live in those; this resolves the database and returns the rows of \
                       all of them. Read-only. `workspace` is required."
    )]
    pub async fn query_database(
        &self,
        Parameters(QueryDatabaseArgs {
            workspace,
            database_id,
        }): Parameters<QueryDatabaseArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&workspace)?;
        let rows = client
            .query_database(&database_id, None)
            .await
            .map_err(|err| err.to_string())?;

        let rendered: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "id": row.get("id"),
                    "title": page_title(row.get("properties").unwrap_or(&Value::Null)),
                    "url": row.get("url"),
                    "properties": row.get("properties"),
                })
            })
            .collect();
        to_json(&rendered)
    }

    #[tool(
        description = "Create a new Notion page and return { workspace, id, url, title }. \
                       Give exactly one parent: `parent_page_id` for a child page, or \
                       `parent_database_id` for a new row in a database. `content` is plain \
                       text, one paragraph per line. This is the one write in this connector \
                       that does not need a human tap, because a new page is additive and \
                       one click to delete; it changes nothing that already exists. It \
                       cannot edit or replace an existing page — use append_to_page for \
                       that. `workspace` is required: a note filed in the wrong workspace is \
                       a mistake nobody downstream can see."
    )]
    pub async fn create_page(
        &self,
        Parameters(CreatePageArgs {
            workspace,
            title,
            parent_page_id,
            parent_database_id,
            content,
        }): Parameters<CreatePageArgs>,
    ) -> Result<String, String> {
        let parent = match (parent_page_id, parent_database_id) {
            (Some(page), None) => Parent::Page(page),
            (None, Some(database)) => Parent::Database(database),
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{CONNECTOR}: create_page was given both parent_page_id and \
                     parent_database_id. A page has exactly one parent; pass one."
                ));
            }
            (None, None) => {
                return Err(format!(
                    "{CONNECTOR}: create_page needs a parent — either parent_page_id (a \
                     child page) or parent_database_id (a new row). A page created with no \
                     parent would land at the top level of the workspace, where the \
                     integration may not even be able to see it again."
                ));
            }
        };

        let blocks = paragraph_blocks(content.as_deref().unwrap_or_default());
        check_block_count("create_page", &blocks)?;

        let client = self.client_for(&workspace)?;
        let page = client
            .create_page(&parent, &title, &blocks)
            .await
            .map_err(|err| err.to_string())?;

        to_json(&json!({
            "workspace": workspace,
            "id": page.get("id"),
            "url": page.get("url"),
            "title": title,
        }))
    }

    #[tool(
        description = "Append plain-text paragraphs to the end of an existing Notion page \
                       and return how many blocks were added. One paragraph per line. This \
                       edits a page the owner already maintains, so it needs a human tap — \
                       unlike create_page. It can only add to the end: nothing here can \
                       edit, reorder, or remove existing content. `workspace` is required."
    )]
    pub async fn append_to_page(
        &self,
        Parameters(AppendToPageArgs {
            workspace,
            page_id,
            content,
        }): Parameters<AppendToPageArgs>,
    ) -> Result<String, String> {
        let blocks = paragraph_blocks(&content);
        if blocks.is_empty() {
            return Err(format!(
                "{CONNECTOR}: append_to_page was given no text to append (Notion refuses an \
                 empty children array)."
            ));
        }
        check_block_count("append_to_page", &blocks)?;

        let client = self.client_for(&workspace)?;
        let created = client
            .append_blocks(&page_id, &blocks)
            .await
            .map_err(|err| err.to_string())?;

        to_json(&json!({
            "workspace": workspace,
            "page_id": page_id,
            "blocks_appended": created.len(),
        }))
    }

    #[tool(
        description = "Poll Notion across every configured workspace. Returns a JSON array \
                       of { external_id, kind, payload }, one entry per database row whose \
                       date property falls within two weeks either side of now. Rows with no \
                       date property are omitted — an undated item is not a deadline. The \
                       databases polled are exactly those shared with each workspace's \
                       integration. Takes no arguments: it covers every workspace by design. \
                       Errors are reported rather than swallowed, so a revoked token is \
                       visible instead of silent."
    )]
    pub async fn watch_poll(&self) -> Result<String, String> {
        let entries = self.poll().await?;
        to_json(&entries)
    }
}

#[tool_handler]
impl ServerHandler for NotionServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Notion for the owner's several workspaces (typically `work` and `personal`). \
             Every tool takes a required `workspace` argument; there is no default \
             workspace, and an id from one workspace does not resolve in another. Reads are \
             search, get_page and query_database. Writes are create_page, which adds a new \
             page, and append_to_page, which adds paragraphs to the end of an existing one. \
             Nothing here can edit, archive or delete anything that already exists. Each \
             workspace's integration sees only what a human has shared with it through \
             Connections, so an empty search result usually means nothing has been shared \
             rather than that the workspace is empty.",
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

    use wiremock::matchers::{body_partial_json, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::auth::Credentials;

    const TOKEN: &str = "ntn_TOOLS-do-not-leak";
    const SOURCE_ID: &str = "11111111-1111-1111-1111-111111111111";
    const PAGE_ID: &str = "22222222-2222-2222-2222-222222222222";
    const DB_ID: &str = "33333333-3333-3333-3333-333333333333";

    fn json_body(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
    }

    fn store_with(dir: &Path, workspaces: &[&str]) -> TokenStore {
        let store = TokenStore::new(Some(dir.join("notion")));
        for workspace in workspaces {
            store
                .write(
                    workspace,
                    &Credentials {
                        token: TOKEN.to_string(),
                        workspace_name: None,
                    },
                )
                .unwrap();
        }
        store
    }

    fn server_for(mock: &MockServer, dir: &Path, workspaces: &[&str]) -> NotionServer {
        NotionServer::new(store_with(dir, workspaces), format!("{}/v1", mock.uri()))
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // -----------------------------------------------------------------------
    // The tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_separates_pages_from_data_sources() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/search"))
            .respond_with(json_body(json!({
                "object": "list",
                "has_more": false,
                "next_cursor": null,
                "results": [
                    {
                        "object": "page",
                        "id": PAGE_ID,
                        "url": "https://www.notion.so/a-page",
                        "properties": {
                            "Uppgift": {
                                "type": "title",
                                "title": [ { "plain_text": "Skriv rapporten" } ],
                            },
                        },
                    },
                    { "object": "data_source", "id": SOURCE_ID, "name": "Tasks" },
                    { "object": "comet", "id": "who knows" },
                ],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .search(Parameters(SearchArgs {
                workspace: "work".into(),
                query: None,
            }))
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(reply["pages"].as_array().unwrap().len(), 1);
        assert_eq!(reply["pages"][0]["title"], "Skriv rapporten");
        assert_eq!(reply["data_sources"].as_array().unwrap().len(), 1);
        assert_eq!(reply["data_sources"][0]["title"], "Tasks");
        assert_eq!(
            reply["unknown"].as_array().unwrap().len(),
            1,
            "an object type this connector does not know must not be filed as a page"
        );
    }

    #[tokio::test]
    async fn get_page_resolves_the_title_by_property_type() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path_matcher(format!("/v1/pages/{PAGE_ID}")))
            .respond_with(json_body(json!({
                "object": "page",
                "id": PAGE_ID,
                "url": "https://www.notion.so/a-page",
                "properties": {
                    "Ärende": { "type": "title", "title": [ { "plain_text": "Möte" } ] },
                },
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .get_page(Parameters(GetPageArgs {
                workspace: "work".into(),
                page_id: PAGE_ID.into(),
            }))
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(reply["title"], "Möte");
        assert_eq!(reply["workspace"], "work");
        assert_eq!(reply["page"]["id"], PAGE_ID);
    }

    #[tokio::test]
    async fn query_database_resolves_the_database_to_its_data_source() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("GET"))
            .and(path_matcher(format!("/v1/databases/{DB_ID}")))
            .respond_with(json_body(json!({
                "object": "database",
                "id": DB_ID,
                "data_sources": [ { "id": SOURCE_ID, "name": "Tasks" } ],
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_matcher(format!("/v1/data_sources/{SOURCE_ID}/query")))
            .respond_with(json_body(json!({
                "object": "list",
                "has_more": false,
                "next_cursor": null,
                "results": [ {
                    "object": "page",
                    "id": PAGE_ID,
                    "url": "https://www.notion.so/a-row",
                    "properties": {
                        "Name": { "type": "title", "title": [ { "plain_text": "Row" } ] },
                    },
                } ],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .query_database(Parameters(QueryDatabaseArgs {
                workspace: "work".into(),
                database_id: DB_ID.into(),
            }))
            .await
            .unwrap();
        let rows: Vec<Value> = serde_json::from_str(&text).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "Row");
    }

    #[tokio::test]
    async fn create_page_sends_the_title_and_the_body_it_was_given() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/pages"))
            .and(body_partial_json(json!({
                "parent": { "type": "page_id", "page_id": PAGE_ID },
            })))
            .respond_with(json_body(json!({
                "object": "page",
                "id": DB_ID,
                "url": "https://www.notion.so/new-page",
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .create_page(Parameters(CreatePageArgs {
                workspace: "work".into(),
                title: "Weekly notes".into(),
                parent_page_id: Some(PAGE_ID.into()),
                parent_database_id: None,
                content: Some("First line.\n\nSecond line.".into()),
            }))
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["url"], "https://www.notion.so/new-page");
        assert_eq!(reply["title"], "Weekly notes");

        let sent: Value =
            serde_json::from_slice(&mock.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(
            sent["children"].as_array().unwrap().len(),
            2,
            "two non-empty lines is two paragraphs; the blank line is dropped"
        );
        assert_eq!(
            sent["children"][0]["paragraph"]["rich_text"][0]["text"]["content"],
            "First line."
        );
    }

    #[tokio::test]
    async fn create_page_refuses_no_parent_and_refuses_two() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server_for(&mock, tmp.path(), &["work"]);

        let none = server
            .create_page(Parameters(CreatePageArgs {
                workspace: "work".into(),
                title: "Orphan".into(),
                parent_page_id: None,
                parent_database_id: None,
                content: None,
            }))
            .await
            .expect_err("a page with no parent must be refused");
        assert!(none.contains("needs a parent"), "{none}");

        let both = server
            .create_page(Parameters(CreatePageArgs {
                workspace: "work".into(),
                title: "Confused".into(),
                parent_page_id: Some(PAGE_ID.into()),
                parent_database_id: Some(DB_ID.into()),
                content: None,
            }))
            .await
            .expect_err("two parents must be refused");
        assert!(both.contains("exactly one parent"), "{both}");

        assert!(
            mock.received_requests().await.unwrap().is_empty(),
            "neither mistake may reach Notion"
        );
    }

    #[tokio::test]
    async fn append_to_page_sends_one_request_and_reports_what_it_added() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("PATCH"))
            .and(path_matcher(format!("/v1/blocks/{PAGE_ID}/children")))
            .respond_with(json_body(json!({
                "object": "list",
                "results": [ { "object": "block", "id": "b1" } ],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock, tmp.path(), &["work"])
            .append_to_page(Parameters(AppendToPageArgs {
                workspace: "work".into(),
                page_id: PAGE_ID.into(),
                content: "One more thought.".into(),
            }))
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["blocks_appended"], 1);
        assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn append_to_page_refuses_empty_text_before_calling_notion() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let err = server_for(&mock, tmp.path(), &["work"])
            .append_to_page(Parameters(AppendToPageArgs {
                workspace: "work".into(),
                page_id: PAGE_ID.into(),
                content: "\n\n   \n".into(),
            }))
            .await
            .expect_err("Notion refuses an empty children array");
        assert!(err.contains("no text"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    /// The non-atomicity fence. `append_blocks` chunks at 100 and a failure on
    /// chunk 2 leaves chunk 1 written, so a retry duplicates it. This layer
    /// never lets a call reach two chunks: the refusal happens before any
    /// request, so a rejected append has written nothing at all.
    #[tokio::test]
    async fn an_append_is_capped_at_one_requests_worth_of_blocks() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();

        let too_long = (0..=MAX_BLOCKS)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(paragraph_blocks(&too_long).len(), MAX_BLOCKS + 1);

        let err = server_for(&mock, tmp.path(), &["work"])
            .append_to_page(Parameters(AppendToPageArgs {
                workspace: "work".into(),
                page_id: PAGE_ID.into(),
                content: too_long,
            }))
            .await
            .expect_err("a two-chunk append must be refused, not attempted");
        assert!(err.contains(&MAX_BLOCKS.to_string()), "{err}");
        assert!(
            mock.received_requests().await.unwrap().is_empty(),
            "the refusal must happen before anything is written, or the fence is pointless"
        );
    }

    #[test]
    fn a_paragraph_longer_than_notions_limit_is_split_on_a_character_boundary() {
        let line = "å".repeat(MAX_TEXT_CHARS + 5);
        let blocks = paragraph_blocks(&line);
        assert_eq!(blocks.len(), 2);
        let first = blocks[0]["paragraph"]["rich_text"][0]["text"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(first.chars().count(), MAX_TEXT_CHARS);
    }

    // -----------------------------------------------------------------------
    // watch_poll
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn watch_poll_covers_every_configured_workspace() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/search"))
            .respond_with(json_body(json!({
                "object": "list",
                "has_more": false,
                "next_cursor": null,
                "results": [ { "object": "data_source", "id": SOURCE_ID, "name": "Tasks" } ],
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_matcher(format!("/v1/data_sources/{SOURCE_ID}/query")))
            .respond_with(json_body(json!({
                "object": "list",
                "has_more": false,
                "next_cursor": null,
                "results": [ {
                    "object": "page",
                    "id": PAGE_ID,
                    "url": "https://www.notion.so/a-row",
                    "properties": {
                        "Name": { "type": "title", "title": [ { "plain_text": "Row" } ] },
                        "Due": { "type": "date", "date": { "start": "2026-09-26" } },
                    },
                } ],
            })))
            .mount(&mock)
            .await;

        let entries = server_for(&mock, tmp.path(), &["personal", "work"])
            .poll_at(now())
            .await
            .unwrap();

        let ids: Vec<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                format!("notion:personal:{PAGE_ID}"),
                format!("notion:work:{PAGE_ID}")
            ],
            "both configured workspaces must be polled"
        );
    }

    /// The failure the whole design exists to prevent: an empty array here is
    /// indistinguishable from a quiet week, so the breaker never trips.
    #[tokio::test]
    async fn watch_poll_with_no_workspaces_errors_with_the_setup_instructions() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let err = server_for(&mock, tmp.path(), &[])
            .watch_poll()
            .await
            .expect_err("no configured workspace is not a quiet week");

        assert!(err.contains("no Notion workspace is configured"), "{err}");
        assert!(err.contains("my-integrations"), "{err}");
        assert!(
            err.contains("Connections"),
            "the step people miss must be in the message a stuck operator reads: {err}"
        );
    }

    #[tokio::test]
    async fn watch_poll_propagates_a_notion_error_rather_than_returning_an_empty_array() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/search"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                json!({ "code": "unauthorized", "message": "API token is invalid." }).to_string(),
                "application/json",
            ))
            .mount(&mock)
            .await;

        let err = server_for(&mock, tmp.path(), &["work"])
            .watch_poll()
            .await
            .expect_err("a 401 must reach the breaker");
        assert!(err.contains("work"), "{err}");
        assert!(!err.contains(TOKEN), "the token must never be quoted");
    }

    // -----------------------------------------------------------------------
    // Unconfigured credentials
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_workspace_with_no_credential_file_is_refused_with_the_steps_that_fix_it() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let server = server_for(&mock, tmp.path(), &["work"]);

        let err = server
            .get_page(Parameters(GetPageArgs {
                workspace: "personal".into(),
                page_id: PAGE_ID.into(),
            }))
            .await
            .expect_err("there is no personal.json");
        assert!(err.contains("no Notion credentials"), "{err}");
        assert!(err.contains("my-integrations"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    /// A workspace label reaches the filesystem only through `TokenStore`,
    /// which validates it first. Tool arguments come from a language model, so
    /// this is a realistic input rather than a thought experiment.
    #[tokio::test]
    async fn a_traversing_workspace_label_is_refused_before_a_path_is_built() {
        let mock = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let err = server_for(&mock, tmp.path(), &["work"])
            .get_page(Parameters(GetPageArgs {
                workspace: "../../id_rsa".into(),
                page_id: PAGE_ID.into(),
            }))
            .await
            .expect_err("a traversing label must be refused");
        assert!(err.contains("not a usable Notion workspace label"), "{err}");
    }

    #[tokio::test]
    async fn an_unconfigured_server_fails_every_call_with_the_reason() {
        let server = NotionServer::unconfigured("the token directory is unreadable");
        for err in [
            server
                .search(Parameters(SearchArgs {
                    workspace: "work".into(),
                    query: None,
                }))
                .await
                .expect_err("search"),
            server
                .get_page(Parameters(GetPageArgs {
                    workspace: "work".into(),
                    page_id: PAGE_ID.into(),
                }))
                .await
                .expect_err("get_page"),
            server
                .query_database(Parameters(QueryDatabaseArgs {
                    workspace: "work".into(),
                    database_id: DB_ID.into(),
                }))
                .await
                .expect_err("query_database"),
            server
                .create_page(Parameters(CreatePageArgs {
                    workspace: "work".into(),
                    title: "t".into(),
                    parent_page_id: Some(PAGE_ID.into()),
                    parent_database_id: None,
                    content: None,
                }))
                .await
                .expect_err("create_page"),
            server
                .append_to_page(Parameters(AppendToPageArgs {
                    workspace: "work".into(),
                    page_id: PAGE_ID.into(),
                    content: "x".into(),
                }))
                .await
                .expect_err("append_to_page"),
            server.watch_poll().await.expect_err("watch_poll"),
        ] {
            assert!(err.contains("no usable credentials"), "{err}");
            assert!(err.contains("the token directory is unreadable"), "{err}");
        }
    }

    // -----------------------------------------------------------------------
    // The tool surface, its schemas, and its policy
    // -----------------------------------------------------------------------

    fn registered_tools() -> BTreeSet<String> {
        NotionServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn input_schemas() -> BTreeMap<String, Value> {
        NotionServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    Value::Object((*tool.input_schema).clone()),
                )
            })
            .collect()
    }

    fn connector_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/notion")
            .canonicalize()
            .expect("connectors/notion must exist")
    }

    fn policy_path() -> PathBuf {
        connector_dir().join("policy.toml")
    }

    fn policy_sections() -> BTreeMap<String, BTreeMap<String, toml::Value>> {
        let text = std::fs::read_to_string(policy_path()).unwrap();
        toml::from_str(&text).expect("policy.toml must parse")
    }

    fn policy_rules() -> BTreeMap<String, String> {
        let parsed = policy_sections();
        let section = parsed
            .get(CONNECTOR)
            .unwrap_or_else(|| panic!("policy.toml must have a [{CONNECTOR}] section"));
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
            "search",
            "get_page",
            "query_database",
            "create_page",
            "append_to_page",
            "watch_poll",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            registered_tools(),
            expected,
            "a new Notion tool must be a deliberate act, with a policy rule to match"
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

    /// The whole multi-workspace design in one assertion. A tool that defaults
    /// its workspace quietly reads — or writes — the wrong one, and nothing
    /// downstream would notice.
    #[test]
    fn every_workspace_scoped_tool_requires_a_workspace_with_no_default() {
        for (name, schema) in input_schemas() {
            if WORKSPACE_EXEMPT_TOOLS.contains(&name.as_str()) {
                continue;
            }

            let required: Vec<&str> = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("tool {name:?} has no `required` list: {schema:#}"))
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                required.contains(&"workspace"),
                "tool {name:?} does not require a `workspace`: {schema:#}"
            );

            let workspace = &schema["properties"]["workspace"];
            assert_eq!(
                workspace["type"], "string",
                "tool {name:?}'s workspace must be a plain string: {schema:#}"
            );
            assert!(
                workspace.get("default").is_none(),
                "tool {name:?} gives `workspace` a default. There is no default Notion \
                 workspace: a default silently writes to the wrong one. {schema:#}"
            );
        }
    }

    /// The exemption list itself, pinned to its one member.
    ///
    /// Without this, the loop above skips whatever `WORKSPACE_EXEMPT_TOOLS`
    /// happens to contain, so adding `"create_page"` to it would quietly buy
    /// that tool an exemption from the whole multi-workspace rule and nothing
    /// in this crate would fail. Widening the list must mean deliberately
    /// editing this test — and the only reason that has ever been good enough
    /// is the one `watch_poll` has: the daemon calls it with `{}` on a timer,
    /// so a required argument there breaks every poll. A tool a *model* calls
    /// has no such excuse.
    #[test]
    fn only_watch_poll_is_exempt_from_the_required_workspace() {
        assert_eq!(
            WORKSPACE_EXEMPT_TOOLS,
            ["watch_poll"],
            "a second exempt tool means a tool that can write to the wrong workspace with \
             nothing downstream able to tell. See the module docs."
        );
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

    /// The deliberate departure, pinned so it cannot be undone by accident in
    /// either direction: a new page is additive and reversible, appending
    /// edits a page the owner already maintains.
    #[test]
    fn creating_a_page_is_auto_and_appending_to_one_is_not() {
        let rules = policy_rules();
        assert_eq!(
            rules.get("create_page").map(String::as_str),
            Some("auto"),
            "the point of this connector is an assistant that files notes without asking"
        );
        assert_eq!(
            rules.get("append_to_page").map(String::as_str),
            Some("approve"),
            "appending edits a page the owner already maintains; that needs a tap"
        );
    }

    /// The `auto` write is the one rule in this project that will surprise a
    /// reader, so the file must carry the argument, not just the verdict.
    #[test]
    fn the_auto_write_carries_its_reasoning_in_the_policy_file() {
        let sections = policy_sections();
        let rule = sections[CONNECTOR]
            .get("create_page")
            .expect("a create_page rule");
        let note = rule
            .as_table()
            .and_then(|table| table.get("note"))
            .and_then(|note| note.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "create_page must be a table rule with a `note`. It is the first write \
                     in this project that runs without a human tap, and a bare \"auto\" \
                     reads as sloppiness to the next person who opens this file."
                )
            });
        assert!(
            note.contains("reversible"),
            "the note must give the reason, not restate the mode: {note:?}"
        );
    }

    #[test]
    fn removing_a_page_is_denied_before_any_such_tool_exists() {
        let rules = policy_rules();
        for placeholder in DELIBERATE_PLACEHOLDERS {
            assert_eq!(
                rules.get(*placeholder).map(String::as_str),
                Some("deny"),
                "notion.{placeholder} must be denied in policy.toml"
            );
            assert!(
                !registered_tools().contains(*placeholder),
                "this connector adds to Notion but never removes from it; it must not \
                 implement {placeholder}"
            );
        }
    }

    /// `Policy::load_dirs` refuses a connector that declares a foreign
    /// section, because a policy file that could name another connector would
    /// be a privilege-escalation path. A test here catches it at
    /// `cargo test` rather than at daemon start-up.
    #[test]
    fn the_policy_file_declares_only_its_own_section() {
        let sections: Vec<String> = policy_sections().into_keys().collect();
        assert_eq!(sections, [CONNECTOR.to_string()]);
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
        assert_eq!(manifest["name"].as_str(), Some(CONNECTOR));
        assert_eq!(manifest["command"].as_str(), Some("ea-notion"));
        assert!(
            manifest["watch_interval_secs"].as_integer().is_some(),
            "a connector with a watch_poll needs a cadence"
        );
    }
}
