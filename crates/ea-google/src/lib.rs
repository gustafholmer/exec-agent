//! `ea-google` — Google OAuth and the multi-account token store.
//!
//! This crate is the foundation the Calendar and Gmail connectors sit on, and
//! the pattern Notion will copy. It does one job: hold a Google *refresh
//! token* per account — the owner's `work` and `private` Google identities —
//! and turn it into a usable access token on demand.
//!
//! Three properties are load-bearing, and each is pinned by a test rather than
//! by intent:
//!
//! * **Nothing here can print a token.** Not a `Debug` impl, not an error, not
//!   a panic, and not a URL — a token in a query string leaks through every
//!   log line that ever quotes the URL. See the `# Token safety` section of
//!   [`auth`].
//! * **An account name never reaches the filesystem unvalidated.** Account
//!   labels arrive from configuration *and* from tool arguments a language
//!   model supplies, so `../../id_rsa` is a realistic input, not a thought
//!   experiment. [`auth::TokenStore`] rejects anything outside
//!   `^[a-z0-9][a-z0-9_-]*$` before it constructs a path. Lower-case only:
//!   the token files live on a case-insensitive volume, where `work` and
//!   `Work` would be two labels sharing one file.
//! * **A 401 is a reason to refresh, not only a reason to fail.** A token can
//!   be dead long before it expires — a password change or a session revoke
//!   kills every outstanding one — so the calendar and Gmail clients force a
//!   refresh on `401` and retry the request exactly once. Once: a grant
//!   Google has revoked answers the same way forever, and an unbounded retry
//!   loop against it is how an integration gets rate limited.
//! * **A refresh is persisted and serialised.** A refresh that lives only in
//!   memory means the next daemon restart re-authorises; two tasks refreshing
//!   at once means two grants and a wasted one. [`auth::Auth`] writes through
//!   to disk inside a per-account mutex, and re-reads inside that mutex so the
//!   second waiter picks up the first one's work instead of repeating it.
//!
//! The connector directory this crate serves is `connectors/google` and its
//! name is `google` — the daemon requires a connector's name to equal its
//! directory's basename. Nothing in this task builds that manifest; the
//! constant [`auth::CONNECTOR`] is here so the two cannot drift when Task 4
//! does.
#![forbid(unsafe_code)]

pub mod auth;
pub mod calendar;
pub mod gmail;
pub mod tools;
pub mod watch;
