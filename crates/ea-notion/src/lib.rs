//! `ea-notion` — the Notion REST client and its per-workspace token store.
//!
//! The shape is `ea-google`'s, deliberately: one credential file per named
//! identity under `~/.config/exec-agent/<connector>/`, mode `0600`, written
//! atomically; one HTTP client that follows no redirects, checks a content
//! type before it deserialises, and never puts a credential in a URL, a log
//! line, an error, or a `Debug` impl.
//!
//! Three properties here are load-bearing, and each is held down by a test
//! rather than by intent:
//!
//! * **A page's title is found by property *type*, not by property name.**
//!   Notion keys properties by whatever the user called their columns, so
//!   `properties["Name"]` is wrong for every database whose title column is
//!   called `Uppgift`, `Ärende`, or `📌 Task`. It also splits rich text at
//!   every formatting change, so a title containing one bold word arrives as
//!   several runs and has to be joined. See [`client::page_title`].
//! * **Pagination is followed, and the cap is loud.** A list endpoint that
//!   answers `has_more: true` is followed to the end. Hitting
//!   [`client::MAX_PAGES`] is an error that discards the partial results,
//!   because a silently truncated workspace is indistinguishable from a quiet
//!   one — and "nothing happened this week" is exactly the conclusion a digest
//!   must never reach by accident.
//! * **429 is retried a bounded number of times.** Notion answers HTTP 429
//!   with `Retry-After` in whole seconds. The client waits and retries, falls
//!   back to a default backoff when the header is missing, and after
//!   [`client::RetryPolicy::max_attempts`] gives up with an error naming the
//!   rate limit. Unbounded retry against a rate limit is how an integration
//!   gets blocked.
//!
//! The connector directory this crate serves is `connectors/notion` and its
//! name is `notion` — the daemon requires a connector's name to equal its
//! directory's basename, and [`auth::CONNECTOR`] is the single spelling both
//! the credential path and `connectors/notion/connector.toml` are checked
//! against (`tools::tests::the_connector_manifest_is_named_for_its_directory`).
//!
//! On top of that sit the MCP server and the poll:
//!
//! * [`tools`] is the surface the daemon and a session see — `search`,
//!   `get_page`, `query_database`, `create_page`, `append_to_page`, and
//!   `watch_poll`. Every tool but `watch_poll` takes a required `workspace`,
//!   because one integration token can only ever see one workspace and there
//!   is no default one.
//! * [`watch`] is what the daemon's timer calls: one row per database item
//!   with a date inside the horizon, across every configured workspace, with
//!   an error from any one of them failing the whole poll rather than looking
//!   like a quiet week.
#![forbid(unsafe_code)]

pub mod auth;
pub mod client;
pub mod tools;
pub mod watch;

pub use auth::{Credentials, TokenStore};
pub use client::{
    page_title, DataSourceRef, NotionClient, NotionError, Parent, RetryPolicy, NOTION_VERSION,
};
pub use tools::NotionServer;
pub use watch::WatchEntry;
