//! The mail model, the **transport seam**, and body extraction.
//!
//! # The seam
//!
//! [`MailTransport`] is the only thing in this crate that knows how mail is
//! actually fetched. `watch.rs` and `tools.rs` hold an `Arc<dyn
//! MailTransport>` and never mention Microsoft Graph, HTTP, or OAuth;
//! `graph.rs` is the one implementation.
//!
//! That is not abstraction for its own sake. The discovery spike settled that
//! KTH's mailboxes are in Exchange Online (MX is `mailfilter-ng-N.sunet.se`,
//! SPF includes `spf.protection.outlook.com`, and there is no DNS at all for
//! `imap.kth.se`, `mail.kth.se` or `autodiscover.kth.se`) — so Graph is the
//! only reachable transport. What it did **not** settle, because only the
//! mailbox's owner can, is whether KTH's tenant permits a user to consent to
//! a third-party application requesting delegated `Mail.Read`. Universities
//! commonly disable user consent, and a student cannot register an
//! application inside KTH's own tenant to route around it.
//!
//! So the whole Graph path is built against an assumption that may turn out
//! to be false, and the seam is what bounds the cost of that. If consent is
//! refused and the fallback turns out to be IMAP against some host this spike
//! could not see, what has to be **replaced** is `auth.rs` (an OAuth token
//! store becomes a username/password or app-password credential file) and
//! `graph.rs` (one `impl MailTransport` becomes another, over `async-imap`
//! and `mail-parser`). What survives untouched is everything below this
//! paragraph — the [`Mail`] model, [`extract_body`], the HTML stripper — plus
//! `watch.rs`, `tools.rs`, `connector.toml`, `policy.toml`, and every test of
//! any of them. `swapping_the_transport_needs_no_change_above_the_seam` is a
//! compiling proof of that: it drives the whole poll through a transport that
//! has never heard of Microsoft.
//!
//! # Why the body extraction matters more than it looks
//!
//! Graph returns a message body as `{ contentType, content }`, where
//! `contentType` is `"html"` or `"text"`. University mail is overwhelmingly
//! HTML — a schedule change, a Canvas notification, a payroll notice — and a
//! client that reads only the plain-text case hands triage an **empty body**.
//! An empty body is scored as noise and discarded, so the mail that mattered
//! most is exactly the mail that disappears. That failure was already found
//! once in this project, in the Gmail client (Phase 2, Review Focus #4), and
//! [`extract_body`] is that fix carried over: prefer text, fall back to
//! stripped HTML, fall back again to Graph's own `bodyPreview`.
//!
//! The stripper started as a copy of `ea_google::gmail`'s rather than a shared
//! dependency, on the reasoning that sixty stateless lines are cheaper to
//! duplicate than to lift into a crate whose tests were already merged, and
//! that a shared copy would stop the two connectors evolving it
//! independently. It is now [`ea_core::html::strip_html`], and the reasoning
//! was wrong in a way worth recording: the copies did not evolve
//! independently, they diverged *asymmetrically*. Four fixes made here for
//! Exchange-generated mail — dropping `<style>`/`<script>` contents,
//! decoding numeric entities, a one-pass decoder that cannot double-decode
//! `&amp;lt;`, and `&nbsp;` — never reached the Gmail copy, which went on
//! feeding the same triage prompt through the same 2000-character cap with the
//! defects this copy had already fixed. See that module's docs for the list,
//! and for the contract the stripper holds to.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use ea_core::html::strip_html;
use serde::{Deserialize, Serialize};

/// One message, normalised, tagged with the account it came from.
///
/// Deliberately the same shape as `ea_google::gmail::Mail` minus Gmail's
/// label list, which has no Graph equivalent worth inventing: the daemon's
/// triage reads these payloads, and two mail connectors that describe a
/// message differently would need two prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mail {
    pub id: String,
    pub conversation_id: String,
    pub account: String,
    pub from: String,
    pub subject: String,
    pub preview: String,
    pub body: String,
    pub received_at: DateTime<Utc>,
    pub is_read: bool,
    pub web_link: String,
}

/// A boxed future, because the trait below is used as `dyn` and
/// async-fn-in-trait is not `dyn`-compatible. Same reasoning as
/// [`crate::auth::RefreshBackend`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything this connector needs from a mail backend — and the only place
/// the backend is named. See the module docs.
///
/// Credentials are the transport's business, not the caller's:
/// [`MailTransport::accounts`] is on this trait rather than reached through
/// an `Auth` handle precisely so that a non-OAuth transport can answer it
/// from a credentials file without every caller changing.
pub trait MailTransport: Send + Sync {
    /// The accounts that have usable credentials, sorted, read fresh rather
    /// than latched at start-up — so authorising an account while the daemon
    /// runs needs no restart.
    fn accounts(&self) -> anyhow::Result<Vec<String>>;

    /// The `max` most recent unread messages in one account's inbox, newest
    /// first.
    fn list_unread<'a>(
        &'a self,
        account: &'a str,
        max: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Mail>>>;

    /// One message by the id [`MailTransport::list_unread`] returned.
    fn get<'a>(&'a self, account: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<Mail>>;

    /// The command that (re-)authorises an account, for error messages. On
    /// the Graph transport this is `ea-kth-authorize <account>`; an IMAP
    /// transport would name whatever its own credential file needs.
    fn authorize_hint(&self, account: &str) -> String;
}

// ---------------------------------------------------------------------------
// Body extraction
// ---------------------------------------------------------------------------

/// Graph's `message.body`: `{ "contentType": "html" | "text", "content": … }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemBody {
    #[serde(default, rename = "contentType")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Pull readable text out of a Graph message body, falling back to the
/// preview.
///
/// The fallback chain, in order, and why each rung exists:
///
/// 1. `contentType: "text"` — use the content as it stands.
/// 2. `contentType: "html"` (the common case for university mail) — strip the
///    markup. **This is the rung that must not be missing**; see the module
///    docs.
/// 3. an unrecognised `contentType` with content — strip it anyway. Stripping
///    text that happens to contain no tags is a no-op, whereas discarding
///    HTML that arrived mislabelled loses the whole message.
/// 4. nothing usable — Graph's `bodyPreview`, a plain-text first ~255
///    characters that is present on every message. A short body beats an
///    empty one: an empty body reads as noise to triage and is thrown away.
pub fn extract_body(body: Option<&ItemBody>, preview: &str) -> String {
    let extracted = body
        .and_then(|body| {
            let content = body.content.as_deref().unwrap_or("").trim();
            if content.is_empty() {
                return None;
            }
            let is_plain = body
                .content_type
                .as_deref()
                .is_some_and(|ct| ct.eq_ignore_ascii_case("text"));
            Some(if is_plain {
                content.to_string()
            } else {
                strip_html(content)
            })
        })
        .unwrap_or_default();

    if extracted.trim().is_empty() {
        return preview.trim().to_string();
    }
    extracted
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn html_body(content: &str) -> ItemBody {
        ItemBody {
            content_type: Some("html".to_string()),
            content: Some(content.to_string()),
        }
    }

    /// **The test this module exists for.** A Graph message whose only body is
    /// HTML must produce readable text, not an empty string: triage scores an
    /// empty body as noise and discards it, and university mail is mostly
    /// HTML.
    #[test]
    fn an_html_only_body_produces_readable_text_rather_than_nothing() {
        let body = html_body(
            "<html><head><style>p{color:red}</style></head><body>\
             <p>Kursen <b>XX1002</b> flyttas.</p>\
             <div>Ny sal: D3.<br>Ta med legitimation.</div>\
             <a href=\"https://kth.se\">Mer&nbsp;info &amp; anm&#228;lan</a>\
             </body></html>",
        );

        let text = extract_body(Some(&body), "");

        assert!(
            !text.is_empty(),
            "an HTML-only body must not extract to \"\""
        );
        assert!(text.contains("Kursen XX1002 flyttas."), "{text:?}");
        assert!(text.contains("Ny sal: D3."), "{text:?}");
        assert!(
            text.contains("Ta med legitimation."),
            "<br> must become a line break: {text:?}"
        );
        assert!(text.contains("Mer info & anmälan"), "{text:?}");
        assert!(!text.contains('<'), "no markup may survive: {text:?}");
        assert!(
            !text.contains("color:red"),
            "a <style> block's CSS must be dropped with the tag, not left in the body \
             to crowd the real message out of a truncated triage prompt: {text:?}"
        );
    }

    #[test]
    fn a_plain_text_body_is_left_alone() {
        let body = ItemBody {
            content_type: Some("text".to_string()),
            content: Some("A < B and 5 > 3".to_string()),
        };
        assert_eq!(extract_body(Some(&body), "preview"), "A < B and 5 > 3");
    }

    /// An unknown or missing `contentType` is stripped rather than trusted.
    /// Stripping tagless text changes nothing; trusting mislabelled HTML would
    /// put markup into a triage prompt.
    #[test]
    fn an_unlabelled_body_is_stripped_rather_than_trusted() {
        let body = ItemBody {
            content_type: None,
            content: Some("<p>Hello</p>".to_string()),
        };
        assert_eq!(extract_body(Some(&body), ""), "Hello");
    }

    /// The last rung: a short preview beats an empty body, because an empty
    /// body is discarded as noise.
    #[test]
    fn an_empty_body_falls_back_to_the_preview() {
        assert_eq!(
            extract_body(None, "  Tentamen flyttad  "),
            "Tentamen flyttad"
        );

        let blank = html_body("   ");
        assert_eq!(
            extract_body(Some(&blank), "Tentamen flyttad"),
            "Tentamen flyttad"
        );

        // Markup that strips to nothing at all also falls through.
        let only_markup = html_body("<div></div><br>");
        assert_eq!(
            extract_body(Some(&only_markup), "Tentamen flyttad"),
            "Tentamen flyttad"
        );
    }

    #[test]
    fn a_body_with_neither_content_nor_preview_is_empty_not_a_panic() {
        assert_eq!(extract_body(None, ""), "");
    }
}
