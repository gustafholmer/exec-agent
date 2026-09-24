//! `ea-kth` — the owner's KTH mailbox, read over Microsoft Graph.
//!
//! # What the spike found, and what it could not
//!
//! KTH's mail is **Exchange Online behind SUNET filtering**, and that is
//! measured rather than inferred:
//!
//! * `dig MX kth.se` answers `mailfilter-ng-{1,2,3,4}.sunet.se` — SUNET's
//!   filter is the inbound edge, not Microsoft.
//! * KTH's SPF record is
//!   `v=spf1 include:_spf.kth.se include:spf.protection.outlook.com ~all` —
//!   KTH *sends* through Exchange Online.
//! * `login.microsoftonline.com/kth.se/.well-known/openid-configuration`
//!   resolves, to tenant `3db27ecc-1791-4dda-9b51-798adfa4a3ca`.
//! * There is **no DNS at all** for `imap.kth.se`, `mail.kth.se` or
//!   `autodiscover.kth.se`. Only `webmail.kth.se` (130.237.28.91) exists.
//!
//! So the brief's IMAP branch has no host to connect to, and Microsoft Graph
//! is the only path. What the spike could **not** settle, because only the
//! mailbox's owner can, is whether KTH's tenant permits a user to consent to a
//! third-party application requesting delegated `Mail.Read`. It is
//! user-consentable by default; universities routinely disable that; and a
//! student cannot register an application inside KTH's own tenant to route
//! around it. Everything Microsoft-facing in this crate is therefore built
//! against an assumption, and the README says so to the owner in as many
//! words.
//!
//! # The seam that bounds the risk
//!
//! [`mail::MailTransport`] is the only place that knows how mail is fetched.
//! `watch` and `tools` hold an `Arc<dyn MailTransport>`; [`graph`] is the one
//! implementation. If the assumption above fails and some other transport
//! turns out to be available, what has to be replaced is [`auth`] and
//! [`graph`] — not the model, not the body extraction, not the poll, not the
//! tools, not the policy, and not their tests.
//! `watch::tests::swapping_the_transport_needs_no_change_above_the_seam`
//! drives the whole poll through a transport that has never heard of
//! Microsoft, which is the compiling version of that claim.
//!
//! # Properties pinned by tests rather than by intent
//!
//! * **Nothing here can print a token.** Not a `Debug` impl, not an error, not
//!   a URL. See the `# Token safety` section of [`auth`].
//! * **An account name never reaches the filesystem unvalidated.** Labels
//!   arrive from tool arguments a language model writes, so `../../id_rsa` is
//!   a realistic input. [`auth::TokenStore`] rejects anything outside
//!   `^[a-z0-9][a-z0-9_-]*$` before it constructs a path.
//! * **A rotated refresh token reaches disk.** Microsoft replaces the refresh
//!   token on every refresh; one that lived only in memory would leave a grant
//!   on disk that Microsoft has already retired.
//! * **A 401 is a reason to refresh, not only to fail** — once, and exactly
//!   once.
//! * **HTML-only mail produces a non-empty body.** University mail is mostly
//!   HTML, and a body that extracted to `""` would be scored as noise and
//!   discarded. See [`mail::extract_body`].
//! * **Nothing here writes.** No draft tool, no send tool, and the OAuth
//!   scopes are `offline_access` and a read-only `Mail.Read`.
//!
//! The connector directory is `connectors/kth`, its declared name is `kth`,
//! its policy section is `[kth]` and its command is `ea-kth`. The daemon
//! requires the first three to be equal; [`auth::CONNECTOR`] is the constant
//! that keeps this crate from drifting from them.
#![forbid(unsafe_code)]

pub mod auth;
pub mod graph;
pub mod mail;
pub mod tools;
pub mod watch;
