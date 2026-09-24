//! What the daemon sees: one row per dated database item, across every
//! configured workspace.
//!
//! The daemon calls `watch_poll` on a timer, parses the reply as
//! `[{ external_id, kind, payload }]`, and records each row under
//! `(source = "notion", external_id)`. That key is the whole contract: a row
//! whose id it has seen before is an update, a row whose id is new is news.
//! Everything here exists to make those ids stable and the payloads small.
//!
//! # How a poll finds anything at all
//!
//! A Notion integration has no notion of "my workspace". It sees exactly the
//! pages and databases a human has explicitly shared with it through
//! **Connections**, and nothing else — which is why
//! [`crate::auth::setup_instructions`] spends a whole step on that. So a poll
//! cannot be told which databases to read from a config file without that
//! file immediately drifting from the sharing the owner actually did. It
//! discovers them instead: `POST /v1/search` with no query returns everything
//! shared with the integration, and the data sources in that reply are the
//! tables to query.
//!
//! # `search` returns pages *and* data sources, mixed
//!
//! [`NotionClient::search`] sends no `filter`, so its `Vec<Value>` holds both
//! object types interleaved, told apart only by each result's `"object"`
//! field. Treating every element as a page is the obvious bug: `page_title`
//! on a data source returns `(untitled)` and `fetch_page` on one 404s.
//!
//! Nothing in this module indexes into that `Vec` directly. Every caller goes
//! through [`split_search_results`], which sorts each result into
//! [`SearchSplit::pages`], [`SearchSplit::data_sources`], or — the part that
//! makes it safe rather than merely tidy — [`SearchSplit::other`], for an
//! `"object"` value this crate does not know, including a missing one. A
//! third object type appearing in a future API version therefore lands in a
//! bucket nothing queries, rather than being quietly mistaken for whichever
//! branch happened to be the `else`. The poll reads only `data_sources`; the
//! `search` tool reports all three buckets separately so a model can see what
//! it has.
//!
//! # What counts as a due date
//!
//! A Notion row has whatever properties its owner invented, in whatever
//! language. There is no `due_at` field to read, and no property *name* worth
//! hard-coding: the column is called `Due`, `Deadline`, `Förfaller`,
//! `Slutdatum`, or `📅`, depending on who built the table. [`due_date`]
//! therefore searches by property **type** — `"date"` — exactly as
//! [`crate::client::page_title`] searches for the property of type `"title"`,
//! and only then uses names to break a tie:
//!
//! 1. Every property of type `date` carrying a parseable `date.start` is a
//!    candidate. `created_time` and `last_edited_time` are different types and
//!    never candidates, which is what keeps "this row was touched today" out
//!    of the deadline list.
//! 2. A candidate whose name contains one of [`DUE_PROPERTY_HINTS`] wins over
//!    one that does not. That is the `Due` column in a table that also has a
//!    `Started` column.
//! 3. Ties break by earliest date, then by property name — so the choice is
//!    deterministic, and a row with two anonymous date columns reports the
//!    nearer one rather than whichever the JSON happened to list first.
//!
//! A row with no date property at all is **skipped**, for Canvas's reason: an
//! undated item is not a deadline, and feeding it to triage buries the items
//! that are.
//!
//! # The window
//!
//! An item is reported when its date falls in `[now - OVERDUE_GRACE,
//! now + HORIZON]`.
//!
//! The forward half is the horizon proper: two weeks is far enough ahead to
//! act on and near enough to mean something. The backward half exists for the
//! reason Canvas's `STALE_DEADLINE_WINDOW` does — a Notion
//! workspace that has been in use for two years is full of rows dated 2024,
//! every one of which is genuinely new to dedup on the connector's first
//! poll. Without a floor, that first poll dumps two years of finished tasks
//! into triage and spends the system's whole premise at once. With it, a
//! deadline missed over a long weekend still surfaces.
//!
//! # Why the payload carries no timestamp of its own
//!
//! The daemon resets an event's triage state whenever its payload changes, so
//! anything in a payload that varies between two polls of an unchanged row
//! re-notifies the owner every interval. `last_edited_time` is exactly such a
//! field and is deliberately absent. What *is* in there is the due date — so
//! moving a deadline changes the payload, which is the one change that should
//! put the item back in front of triage.
//!
//! # Failing loudly
//!
//! Canvas's rule, not Google's. An error from any workspace fails the whole
//! poll rather than returning the other workspaces' rows. `[]` on failure is
//! indistinguishable from "nothing is due", so the breaker would never trip
//! and a connector whose token was revoked in October would show green until
//! somebody noticed the silence. Google degrades per-account instead because
//! its tokens expire on a seven-day timer by design; a Notion internal
//! integration's secret does not expire, so a Notion 401 means a human
//! revoked something and is worth stopping for.

use anyhow::{bail, Context};
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::{page_title, NotionClient, UNTITLED};

/// The `kind` every row carries. Triage mutes and keyword rules match on it,
/// so it is one stable name rather than something derived per database.
pub const KIND_ITEM: &str = "database_item";

/// Notion's `"object"` discriminator, for the two types this crate knows.
pub const OBJECT_PAGE: &str = "page";
/// See [`OBJECT_PAGE`].
pub const OBJECT_DATA_SOURCE: &str = "data_source";

/// How far ahead a poll looks. See the module docs.
pub const HORIZON: chrono::Duration = chrono::Duration::days(14);

/// How far *behind* a poll looks. See the module docs: without a floor, the
/// first poll of a two-year-old workspace is two years of finished tasks.
pub const OVERDUE_GRACE: chrono::Duration = chrono::Duration::days(14);

/// Substrings (matched case-insensitively against a property's name) that
/// mark a date property as the row's deadline rather than one of its other
/// dates.
///
/// A tie-breaker, never a requirement: a row whose only date property is
/// called `📅` is still reported. Swedish is here because the author's
/// workspaces are, and because the whole point of searching by property type
/// is that the name is not knowable in advance.
pub const DUE_PROPERTY_HINTS: &[&str] = &["due", "deadline", "förfall", "slutdatum", "inlämning"];

/// One change, in the shape the daemon's event store records. `source` is
/// deliberately absent: the daemon supplies it from the connector name it
/// already knows, so a connector cannot write events attributed to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub external_id: String,
    pub kind: String,
    pub payload: Value,
}

/// `notion:<workspace>:<page_id>`.
///
/// The workspace label is part of the id because the two are independent
/// namespaces: a page copied between workspaces, or two integrations pointed
/// at one duplicated template, can legitimately produce the same page id in
/// both — and those are two rows the owner may need to act on separately, not
/// one row that flickers between two payloads.
pub fn item_external_id(workspace: &str, page_id: &str) -> String {
    format!("notion:{workspace}:{page_id}")
}

// ---------------------------------------------------------------------------
// Mixed search results
// ---------------------------------------------------------------------------

/// `POST /v1/search`'s results, sorted by their `"object"` field.
///
/// See the module docs for why the unknown bucket exists rather than an
/// `else` branch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchSplit {
    pub pages: Vec<Value>,
    pub data_sources: Vec<Value>,
    /// Results whose `"object"` is neither [`OBJECT_PAGE`] nor
    /// [`OBJECT_DATA_SOURCE`], or which carry no `"object"` at all.
    pub other: Vec<Value>,
}

/// Sort a search reply into [`SearchSplit`].
pub fn split_search_results(results: Vec<Value>) -> SearchSplit {
    let mut split = SearchSplit::default();
    for result in results {
        match result.get("object").and_then(Value::as_str) {
            Some(OBJECT_PAGE) => split.pages.push(result),
            Some(OBJECT_DATA_SOURCE) => split.data_sources.push(result),
            _ => split.other.push(result),
        }
    }
    split
}

/// A data source's human name, however this API version spells it.
///
/// `GET /v1/databases/{id}` reports `{ id, name }`; a data source appearing in
/// a search reply is a full object, whose display name may instead be the
/// rich-text `title` array a database carries. Both are tried before falling
/// back to [`UNTITLED`], because a nameless table in a digest is worse than a
/// guess.
pub fn object_title(object: &Value) -> String {
    if let Some(name) = object.get("name").and_then(Value::as_str) {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if let Some(runs) = object.get("title").and_then(Value::as_array) {
        let joined: String = runs
            .iter()
            .filter_map(|run| {
                run.get("plain_text")
                    .and_then(Value::as_str)
                    .or_else(|| run.pointer("/text/content").and_then(Value::as_str))
            })
            .collect();
        let joined = joined.trim();
        if !joined.is_empty() {
            return joined.to_string();
        }
    }
    UNTITLED.to_string()
}

// ---------------------------------------------------------------------------
// Due dates
// ---------------------------------------------------------------------------

/// The date property this crate picked for a row, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    /// The property's name, as the owner spelled it. Reported in the payload
    /// so a surprising pick is visible rather than mysterious.
    pub property: String,
    /// `start`, normalised to UTC, for comparison against the window.
    pub start: DateTime<Utc>,
    /// `start` exactly as Notion wrote it — a bare `YYYY-MM-DD` for an
    /// all-day date, an offset timestamp otherwise.
    pub start_raw: String,
    /// `end`, if the property is a range. Carried verbatim; nothing compares
    /// against it.
    pub end_raw: Option<String>,
    /// True when Notion wrote a bare date with no time of day.
    pub all_day: bool,
}

/// Parse a Notion `date.start`/`date.end` string.
///
/// Notion writes either an RFC 3339 timestamp with an offset, or a bare
/// `YYYY-MM-DD` for an all-day date. The bare form is anchored at midnight
/// **UTC**, which is a deliberate approximation: without the owner's time
/// zone, any anchor is off by up to a day at the window's edges, and midnight
/// UTC errs toward reporting an all-day item slightly early rather than
/// slightly late.
pub fn parse_notion_date(raw: &str) -> Option<(DateTime<Utc>, bool)> {
    let raw = raw.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some((parsed.with_timezone(&Utc), false));
    }
    let date = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Some((Utc.from_utc_datetime(&midnight), true))
}

/// The row's deadline, chosen by property *type* and then by name. See the
/// module docs for the full rule.
pub fn due_date(properties: &Value) -> Option<Due> {
    let map = properties.as_object()?;

    let mut candidates: Vec<(u8, DateTime<Utc>, Due)> = Vec::new();
    for (name, property) in map {
        if property.get("type").and_then(Value::as_str) != Some("date") {
            continue;
        }
        let Some(start_raw) = property.pointer("/date/start").and_then(Value::as_str) else {
            continue;
        };
        let Some((start, all_day)) = parse_notion_date(start_raw) else {
            continue;
        };

        let lowered = name.to_lowercase();
        let rank = u8::from(!DUE_PROPERTY_HINTS.iter().any(|hint| lowered.contains(hint)));

        candidates.push((
            rank,
            start,
            Due {
                property: name.clone(),
                start,
                start_raw: start_raw.trim().to_string(),
                end_raw: property
                    .pointer("/date/end")
                    .and_then(Value::as_str)
                    .map(|end| end.trim().to_string()),
                all_day,
            },
        ));
    }

    // Deterministic: hinted name first, then earliest, then property name.
    // Nothing here may depend on JSON key order.
    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.property.cmp(&b.2.property))
    });
    candidates.into_iter().next().map(|(_, _, due)| due)
}

// ---------------------------------------------------------------------------
// The poll
// ---------------------------------------------------------------------------

/// Poll every workspace. `now` is a parameter rather than `Utc::now()` so the
/// window is deterministic in tests.
///
/// One failing workspace fails the whole poll — see the module docs.
pub async fn poll(clients: &[NotionClient], now: DateTime<Utc>) -> anyhow::Result<Vec<WatchEntry>> {
    // Defence in depth: `NotionServer::poll_at` already refuses an empty
    // workspace list with the setup instructions, because it is the layer that
    // knows the token directory's path. Reaching here with none is still not
    // "nothing to report" — it is a connector that cannot do its job — so it
    // must not return `Ok(vec![])`.
    if clients.is_empty() {
        bail!(
            "no Notion workspace was given to poll, so there is nothing to report. \
             This is not an empty workspace; it is a connector with no credentials."
        );
    }

    let mut entries = Vec::new();
    for client in clients {
        entries.extend(poll_workspace(client, now).await?);
    }
    Ok(entries)
}

/// One workspace's dated rows.
pub async fn poll_workspace(
    client: &NotionClient,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<WatchEntry>> {
    let workspace = client.workspace().to_string();

    let results = client.search(None, None).await.with_context(|| {
        format!(
            "searching Notion workspace {workspace:?} for the databases shared with the \
             integration"
        )
    })?;

    let split = split_search_results(results);
    if !split.other.is_empty() {
        tracing::debug!(
            workspace = %workspace,
            count = split.other.len(),
            "search returned objects of an unrecognised type; ignoring them"
        );
    }

    let floor = now - OVERDUE_GRACE;
    let ceiling = now + HORIZON;
    let mut entries = Vec::new();

    for source in &split.data_sources {
        let Some(source_id) = source.get("id").and_then(Value::as_str) else {
            // Loud, not skipped: a data source with no id means the reply is
            // not the shape this crate was written against, and quietly
            // dropping a whole table is how a digest goes wrong silently.
            bail!(
                "a data source in Notion workspace {workspace:?} came back from search with \
                 no \"id\"; the search reply is not the shape this connector expects"
            );
        };
        let source_title = object_title(source);

        let rows = client
            .query_data_source(source_id, None)
            .await
            .with_context(|| {
                format!(
                    "querying data source {source_title:?} ({source_id}) in Notion workspace \
                     {workspace:?}"
                )
            })?;

        for row in rows {
            let Some(page_id) = row.get("id").and_then(Value::as_str) else {
                bail!(
                    "a row of data source {source_title:?} ({source_id}) in Notion workspace \
                     {workspace:?} has no \"id\"; without one there is no stable event id"
                );
            };

            // `archived` is the pre-2026 spelling, `in_trash` the current one.
            // Both are checked: the constant this crate pins may move ahead of
            // a workspace's cached objects, and a trashed row is not news.
            let trashed = matches!(row.get("in_trash").and_then(Value::as_bool), Some(true))
                || matches!(row.get("archived").and_then(Value::as_bool), Some(true));
            if trashed {
                continue;
            }

            let properties = row.get("properties").unwrap_or(&Value::Null);

            // The skip that matters: no date property, no deadline, no event.
            let Some(due) = due_date(properties) else {
                continue;
            };
            if due.start < floor || due.start > ceiling {
                continue;
            }

            entries.push(entry(
                &workspace,
                source_id,
                &source_title,
                page_id,
                &row,
                &due,
            ));
        }
    }

    Ok(entries)
}

fn entry(
    workspace: &str,
    source_id: &str,
    source_title: &str,
    page_id: &str,
    row: &Value,
    due: &Due,
) -> WatchEntry {
    let properties = row.get("properties").unwrap_or(&Value::Null);
    WatchEntry {
        external_id: item_external_id(workspace, page_id),
        kind: KIND_ITEM.to_string(),
        payload: json!({
            "workspace": workspace,
            "title": page_title(properties),
            "due": due.start.to_rfc3339_opts(SecondsFormat::Secs, true),
            "due_raw": due.start_raw,
            "due_end": due.end_raw,
            "due_property": due.property,
            "all_day": due.all_day,
            "data_source": source_title,
            "data_source_id": source_id,
            "url": row.get("url").and_then(Value::as_str),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "ntn_WATCH-do-not-leak";
    const SOURCE_ID: &str = "11111111-1111-1111-1111-111111111111";
    const PAGE_ID: &str = "22222222-2222-2222-2222-222222222222";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-24T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// `set_body_raw`: wiremock's `set_body_string` stamps `text/plain` over
    /// anything set before it, and the client gates on the content type.
    fn json_body(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
    }

    fn client_for(mock: &MockServer, workspace: &str) -> NotionClient {
        NotionClient::with_base_url(&format!("{}/v1", mock.uri()), TOKEN)
            .unwrap()
            .with_workspace(workspace)
            .unwrap()
    }

    fn date_property(start: &str) -> Value {
        json!({ "type": "date", "date": { "start": start, "end": null } })
    }

    fn title_property(text: &str) -> Value {
        json!({
            "type": "title",
            "title": [ { "plain_text": text, "text": { "content": text } } ],
        })
    }

    fn row(id: &str, title: &str, due: Option<&str>) -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert("Name".to_string(), title_property(title));
        if let Some(due) = due {
            properties.insert("Due".to_string(), date_property(due));
        }
        json!({
            "object": "page",
            "id": id,
            "url": format!("https://www.notion.so/{}", id.replace('-', "")),
            "properties": Value::Object(properties),
        })
    }

    fn data_source(id: &str, name: &str) -> Value {
        json!({ "object": "data_source", "id": id, "name": name })
    }

    /// A workspace whose search returns `sources` and whose every data-source
    /// query returns `rows`.
    async fn workspace_with(sources: Vec<Value>, rows: Vec<Value>) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(json_body(json!({
                "object": "list",
                "results": sources,
                "has_more": false,
                "next_cursor": null,
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v1/data_sources/{SOURCE_ID}/query")))
            .respond_with(json_body(json!({
                "object": "list",
                "results": rows,
                "has_more": false,
                "next_cursor": null,
            })))
            .mount(&mock)
            .await;
        mock
    }

    // -----------------------------------------------------------------------
    // One entry per dated item
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn one_entry_per_item_with_a_due_date_inside_the_horizon() {
        let mock = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![
                row(PAGE_ID, "Write the report", Some("2026-09-26")),
                row(
                    "33333333-3333-3333-3333-333333333333",
                    "Book the room",
                    Some("2026-10-01T09:00:00Z"),
                ),
            ],
        )
        .await;

        let entries = poll(&[client_for(&mock, "work")], now()).await.unwrap();

        assert_eq!(entries.len(), 2, "{entries:#?}");
        assert_eq!(
            entries[0].external_id,
            format!("notion:work:{PAGE_ID}"),
            "the id is notion:<workspace>:<page_id>"
        );
        assert_eq!(entries[0].kind, KIND_ITEM);
        assert_eq!(entries[0].payload["title"], "Write the report");
        assert_eq!(entries[0].payload["workspace"], "work");
        assert_eq!(entries[0].payload["data_source"], "Tasks");
        assert_eq!(entries[0].payload["due"], "2026-09-26T00:00:00Z");
        assert_eq!(entries[0].payload["all_day"], true);
        assert_eq!(entries[1].payload["due"], "2026-10-01T09:00:00Z");
        assert_eq!(entries[1].payload["all_day"], false);
    }

    #[tokio::test]
    async fn an_item_with_no_due_date_is_skipped() {
        let mock = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![
                row(PAGE_ID, "Someday, maybe", None),
                row(
                    "33333333-3333-3333-3333-333333333333",
                    "Dated",
                    Some("2026-09-26"),
                ),
            ],
        )
        .await;

        let entries = poll(&[client_for(&mock, "work")], now()).await.unwrap();

        assert_eq!(entries.len(), 1, "{entries:#?}");
        assert_eq!(entries[0].payload["title"], "Dated");
    }

    #[tokio::test]
    async fn dates_outside_the_window_are_skipped_in_both_directions() {
        let mock = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![
                row(PAGE_ID, "Ancient history", Some("2024-01-01")),
                row(
                    "33333333-3333-3333-3333-333333333333",
                    "Next year",
                    Some("2027-01-01"),
                ),
                row(
                    "44444444-4444-4444-4444-444444444444",
                    "Just overdue",
                    Some("2026-09-20"),
                ),
            ],
        )
        .await;

        let entries = poll(&[client_for(&mock, "work")], now()).await.unwrap();

        let titles: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.payload["title"].as_str())
            .collect();
        assert_eq!(
            titles,
            ["Just overdue"],
            "a deadline missed four days ago is still news; 2024 and 2027 are not"
        );
    }

    #[tokio::test]
    async fn a_trashed_row_is_not_news() {
        let mut trashed = row(PAGE_ID, "Cancelled", Some("2026-09-26"));
        trashed["in_trash"] = json!(true);
        let mock = workspace_with(vec![data_source(SOURCE_ID, "Tasks")], vec![trashed]).await;

        let entries = poll(&[client_for(&mock, "work")], now()).await.unwrap();
        assert!(entries.is_empty(), "{entries:#?}");
    }

    // -----------------------------------------------------------------------
    // Two workspaces
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn the_same_page_id_in_two_workspaces_is_two_entries() {
        let a = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Shared template row", Some("2026-09-26"))],
        )
        .await;
        let b = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Shared template row", Some("2026-09-26"))],
        )
        .await;

        let entries = poll(&[client_for(&a, "work"), client_for(&b, "personal")], now())
            .await
            .unwrap();

        let ids: Vec<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                format!("notion:work:{PAGE_ID}"),
                format!("notion:personal:{PAGE_ID}")
            ],
            "one page id in two workspaces is two things to act on, not one"
        );
    }

    // -----------------------------------------------------------------------
    // The payload changes when the deadline moves
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn moving_a_due_date_changes_the_payload_so_triage_reopens_it() {
        let before = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Write the report", Some("2026-09-26"))],
        )
        .await;
        let after = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Write the report", Some("2026-09-30"))],
        )
        .await;

        let first = poll(&[client_for(&before, "work")], now()).await.unwrap();
        let second = poll(&[client_for(&after, "work")], now()).await.unwrap();

        assert_eq!(
            first[0].external_id, second[0].external_id,
            "it is the same item, so dedup must still key it the same"
        );
        assert_ne!(
            first[0].payload, second[0].payload,
            "the EventStore resets triage on a payload change; a moved deadline must be one"
        );
        assert_eq!(second[0].payload["due"], "2026-09-30T00:00:00Z");
    }

    #[tokio::test]
    async fn an_unchanged_row_polled_twice_produces_an_identical_payload() {
        let mock = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Write the report", Some("2026-09-26"))],
        )
        .await;

        let first = poll(&[client_for(&mock, "work")], now()).await.unwrap();
        let later = now() + chrono::Duration::hours(3);
        let second = poll(&[client_for(&mock, "work")], later).await.unwrap();

        assert_eq!(
            first, second,
            "nothing in a payload may vary with the clock, or every poll re-notifies"
        );
    }

    // -----------------------------------------------------------------------
    // Failing loudly
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_error_in_any_workspace_fails_the_whole_poll() {
        let healthy = workspace_with(
            vec![data_source(SOURCE_ID, "Tasks")],
            vec![row(PAGE_ID, "Write the report", Some("2026-09-26"))],
        )
        .await;

        let broken = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                json!({ "code": "unauthorized", "message": "API token is invalid." }).to_string(),
                "application/json",
            ))
            .mount(&broken)
            .await;

        let err = poll(
            &[
                client_for(&healthy, "work"),
                client_for(&broken, "personal"),
            ],
            now(),
        )
        .await
        .expect_err("a revoked token must reach the breaker, not be averaged away");

        let rendered = format!("{err:#}");
        assert!(rendered.contains("personal"), "{rendered}");
        assert!(!rendered.contains(TOKEN), "the token must never be quoted");
    }

    #[tokio::test]
    async fn a_failing_poll_is_an_error_not_an_empty_array() {
        let broken = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(500).set_body_raw("{}", "application/json"))
            .mount(&broken)
            .await;

        let result = poll(&[client_for(&broken, "work")], now()).await;

        // The whole point: `Ok(vec![])` here is indistinguishable from a quiet
        // week, the breaker never trips, and the connector goes silent green.
        assert!(
            result.is_err(),
            "a failed poll returned {:?} instead of erroring",
            result.map(|entries| entries.len())
        );
    }

    #[tokio::test]
    async fn polling_no_workspaces_is_an_error() {
        let err = poll(&[], now())
            .await
            .expect_err("no credentials is not a quiet week");
        assert!(format!("{err:#}").contains("no credentials"));
    }

    // -----------------------------------------------------------------------
    // Mixed search results
    // -----------------------------------------------------------------------

    #[test]
    fn search_results_are_split_by_object_and_unknown_types_are_quarantined() {
        let split = split_search_results(vec![
            json!({ "object": "page", "id": "p" }),
            json!({ "object": "data_source", "id": "d" }),
            json!({ "object": "database", "id": "b" }),
            json!({ "id": "no object field at all" }),
        ]);

        assert_eq!(split.pages.len(), 1);
        assert_eq!(split.data_sources.len(), 1);
        assert_eq!(
            split.other.len(),
            2,
            "an unknown object type must not fall into either known bucket"
        );
    }

    #[tokio::test]
    async fn the_poll_queries_only_the_data_sources_search_returned_not_the_pages() {
        // Search returns a loose page alongside the table. If the poll treated
        // every result as queryable, this mock would receive a query for the
        // page id and answer 404 — so a green run proves the filter works.
        let mock = workspace_with(
            vec![
                json!({ "object": "page", "id": PAGE_ID, "properties": {} }),
                data_source(SOURCE_ID, "Tasks"),
            ],
            vec![row(PAGE_ID, "Write the report", Some("2026-09-26"))],
        )
        .await;

        let entries = poll(&[client_for(&mock, "work")], now()).await.unwrap();
        assert_eq!(entries.len(), 1, "{entries:#?}");

        let queried: Vec<String> = mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|req| req.url.path().to_string())
            .collect();
        assert_eq!(
            queried,
            [
                "/v1/search".to_string(),
                format!("/v1/data_sources/{SOURCE_ID}/query"),
            ],
            "the page in the search reply must not have been queried as a table"
        );
    }

    // -----------------------------------------------------------------------
    // Choosing the date property
    // -----------------------------------------------------------------------

    #[test]
    fn the_due_date_is_found_by_property_type_whatever_the_column_is_called() {
        for name in ["Due", "Deadline", "Förfaller", "Slutdatum", "📅"] {
            let properties = json!({ name: date_property("2026-09-26") });
            let due = due_date(&properties)
                .unwrap_or_else(|| panic!("a date property called {name:?} must be found"));
            assert_eq!(due.property, name);
            assert_eq!(due.start_raw, "2026-09-26");
        }
    }

    #[test]
    fn a_hinted_name_beats_an_earlier_unhinted_date() {
        let properties = json!({
            "Started": date_property("2026-01-01"),
            "Due date": date_property("2026-09-26"),
        });
        let due = due_date(&properties).unwrap();
        assert_eq!(
            due.property, "Due date",
            "a `Started` column must not be mistaken for the deadline"
        );
    }

    #[test]
    fn two_anonymous_date_properties_resolve_to_the_earlier_one_deterministically() {
        let properties = json!({
            "Zeta": date_property("2026-10-05"),
            "Alpha": date_property("2026-09-26"),
        });
        assert_eq!(due_date(&properties).unwrap().property, "Alpha");
    }

    #[test]
    fn a_created_or_edited_timestamp_is_not_a_deadline() {
        let properties = json!({
            "Created": { "type": "created_time", "created_time": "2026-09-24T10:00:00Z" },
            "Edited": {
                "type": "last_edited_time",
                "last_edited_time": "2026-09-24T11:00:00Z",
            },
        });
        assert_eq!(
            due_date(&properties),
            None,
            "`created_time` is a different property type; treating it as a deadline would \
             report every row the owner touched today"
        );
    }

    #[test]
    fn an_empty_or_unparseable_date_is_no_date() {
        for property in [
            json!({ "type": "date", "date": null }),
            json!({ "type": "date", "date": { "start": null } }),
            json!({ "type": "date", "date": { "start": "not a date" } }),
            json!({ "type": "date" }),
        ] {
            assert_eq!(due_date(&json!({ "When": property })), None);
        }
        assert_eq!(due_date(&Value::Null), None);
        assert_eq!(due_date(&json!([])), None);
    }

    #[test]
    fn a_date_range_carries_its_end_verbatim() {
        let properties = json!({
            "Sprint": {
                "type": "date",
                "date": { "start": "2026-09-26", "end": "2026-09-30" },
            },
        });
        let due = due_date(&properties).unwrap();
        assert_eq!(due.start_raw, "2026-09-26");
        assert_eq!(due.end_raw.as_deref(), Some("2026-09-30"));
    }

    #[test]
    fn an_offset_timestamp_is_normalised_to_utc() {
        let (parsed, all_day) = parse_notion_date("2026-09-26T09:00:00+02:00").unwrap();
        assert_eq!(
            parsed.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-09-26T07:00:00Z"
        );
        assert!(!all_day);
    }

    #[test]
    fn a_data_sources_name_is_read_from_name_or_title_or_falls_back() {
        assert_eq!(object_title(&json!({ "name": "Tasks" })), "Tasks");
        assert_eq!(
            object_title(
                &json!({ "title": [ { "plain_text": "Pro" }, { "plain_text": "jects" } ] })
            ),
            "Projects",
            "a title split across formatting runs must be joined"
        );
        assert_eq!(object_title(&json!({ "name": "   " })), UNTITLED);
        assert_eq!(object_title(&json!({})), UNTITLED);
    }
}
