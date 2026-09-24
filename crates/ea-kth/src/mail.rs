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
//! dependency. It is sixty lines with no state, the two connectors would not be
//! free to evolve it independently if it were shared, and lifting it into
//! `ea-core` would touch a crate whose tests are already merged. The right
//! moment to share it is when a third connector needs it; this comment is the
//! marker for that.
//!
//! It has since diverged in two ways, both because Exchange-generated mail is
//! not Gmail-generated mail:
//!
//! * `<style>` and `<script>` **contents** are dropped along with their tags
//!   (see [`OPAQUE_ELEMENTS`]). Outlook emits stylesheets running to hundreds
//!   of rules, and with a 2000-character body cap those would push the real
//!   message out of the triage prompt.
//! * Numeric entities are decoded as well as named ones. Swedish mail is full
//!   of `&#229; &#228; &#246;`, and leaving them raw would put literal
//!   `f&#246;rfaller` in front of the owner. The decoder is a single
//!   left-to-right pass rather than a chain of `replace` calls, so — unlike
//!   the Gmail version — an already-escaped `&amp;lt;` does **not**
//!   double-decode into markup.
//!
//! It is still not a real HTML parser and does not try to be. It converts
//! `<br>` and closing block-level tags to newlines, drops every other tag, and
//! collapses runs of blank lines. CDATA, comments and malformed tags get no
//! special handling. None of that matters for the job the output does: making
//! text scorable by a triage prompt, not reproducing the document.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
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

const NEWLINE_CLOSERS: [&str; 12] = [
    "p",
    "div",
    "tr",
    "li",
    "table",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
];

/// Elements whose *contents* are code, not prose, and must be dropped along
/// with their tags.
///
/// The one deliberate improvement over the Gmail stripper this is otherwise a
/// copy of, and it is not cosmetic here. Outlook and Exchange generate mail
/// with `<style>` blocks running to hundreds of CSS rules; dropping only the
/// tags leaves every one of those rules in the text. The body reaching triage
/// is truncated at 2000 characters, so a stylesheet at the top would push the
/// actual message out of the prompt entirely — the same "the mail that
/// mattered is the mail that vanishes" failure the HTML fallback exists to
/// prevent, arriving by a different route.
const OPAQUE_ELEMENTS: [&str; 2] = ["style", "script"];

/// Strip HTML down to plain-enough text. See the module docs for the exact
/// (deliberately limited) contract.
pub fn strip_html(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut chars = html.chars();
    let mut tag = String::new();

    while let Some(c) = chars.next() {
        if c != '<' {
            text.push(c);
            continue;
        }
        tag.clear();
        let mut closed = false;
        for t in chars.by_ref() {
            if t == '>' {
                closed = true;
                break;
            }
            tag.push(t);
        }
        if !closed {
            // An unterminated `<` at end of input: drop it silently rather
            // than emit a stray angle bracket.
            break;
        }

        let (name, is_closing, self_closing) = parse_tag(&tag);

        if !is_closing && !self_closing && OPAQUE_ELEMENTS.contains(&name.as_str()) {
            skip_to_close(&mut chars, &name);
            continue;
        }

        if name == "br" || (is_closing && NEWLINE_CLOSERS.contains(&name.as_str())) {
            text.push('\n');
        }
    }

    collapse_blank_lines(&decode_entities(&text))
}

/// `(lower-case name, is a closing tag, is self-closing)`.
fn parse_tag(raw_tag: &str) -> (String, bool, bool) {
    let trimmed = raw_tag.trim();
    let is_closing = trimmed.starts_with('/');
    let self_closing = trimmed.ends_with('/');
    let name_part = trimmed.trim_start_matches('/').trim_end_matches('/').trim();
    let name = name_part
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (name, is_closing, self_closing)
}

/// Consume everything up to and including `</name>`, discarding it.
///
/// An element that is never closed consumes the rest of the input, which is
/// the right answer: whatever followed an unterminated `<style>` was inside
/// the stylesheet as far as any browser is concerned.
fn skip_to_close(chars: &mut std::str::Chars<'_>, name: &str) {
    let mut tag = String::new();
    while let Some(c) = chars.next() {
        if c != '<' {
            continue;
        }
        tag.clear();
        for t in chars.by_ref() {
            if t == '>' {
                break;
            }
            tag.push(t);
        }
        let (closed_name, is_closing, _) = parse_tag(&tag);
        if is_closing && closed_name == name {
            return;
        }
    }
}

/// The named entities common enough in real mail to bother with.
const NAMED_ENTITIES: [(&str, char); 6] = [
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", ' '),
];

/// The longest thing that can sit between `&` and `;` before this stops
/// believing it is an entity. `&#x1F600;` is eight; ten leaves room without
/// letting a stray `&` scan half a paragraph.
const MAX_ENTITY_BODY: usize = 10;

/// Decode HTML entities in one left-to-right pass.
///
/// Numeric entities are decoded as well as named ones, which the Gmail
/// stripper does not do. That is not a refinement for its own sake: Swedish
/// mail is full of `å ä ö`, and Exchange emits them as `&#229; &#228; &#246;`
/// often enough that leaving them raw would put literal `f&#246;rfaller` in
/// front of a triage prompt and in front of the owner.
///
/// One pass rather than a chain of `replace` calls, because a chain decodes
/// its own output: source text containing a literal `&lt;` (written
/// `&amp;lt;`) would become `<` and read as markup. Here `&amp;` yields `&`
/// and scanning resumes *after* it, so `lt;` stays text.
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        // Bytes, not chars, and restricted to the ASCII set an entity name or
        // numeric reference can contain — so `body` is always a char boundary
        // and slicing cannot panic on the `&ä` that a Swedish mail will
        // eventually contain.
        let body = after
            .bytes()
            .take(MAX_ENTITY_BODY)
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'#')
            .count();
        let ends_in_semicolon = after.as_bytes().get(body) == Some(&b';');

        match decode_entity(&after[..body]).filter(|_| ends_in_semicolon) {
            Some(decoded) => {
                out.push(decoded);
                rest = &after[body + 1..];
            }
            None => {
                // Not an entity — a bare `&` in prose. Emit it and carry on
                // from the next character, so the `&` cannot be re-examined.
                out.push('&');
                rest = after;
            }
        }
    }

    out.push_str(rest);
    out
}

/// The text between `&` and `;`, decoded, or `None` if it is not something
/// this understands.
fn decode_entity(body: &str) -> Option<char> {
    if let Some(digits) = body.strip_prefix('#') {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    NAMED_ENTITIES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(body))
        .map(|(_, c)| *c)
}

/// Trims each line and collapses two or more consecutive blank lines into one,
/// so stripped markup (which tends to leave a blank line per removed tag)
/// reads as paragraphs rather than a wall of blank lines.
fn collapse_blank_lines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut previous_blank = false;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if previous_blank || out.is_empty() {
                continue;
            }
            previous_blank = true;
            out.push('\n');
        } else {
            previous_blank = false;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(trimmed);
        }
    }
    out.trim_end().to_string()
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

    /// Exchange-generated mail carries large stylesheets. See
    /// [`OPAQUE_ELEMENTS`].
    #[test]
    fn a_stylesheet_does_not_reach_the_body() {
        let text = strip_html(
            "<style type=\"text/css\">\n.x { color: #fff; }\n.y { margin: 0 }\n</style>\
             <script>alert('no')</script>\
             <p>Faktura 12345 f&#246;rfaller imorgon.</p>",
        );
        assert_eq!(text, "Faktura 12345 förfaller imorgon.");
    }

    /// An unclosed `<style>` swallows the rest, which is what a browser does
    /// too — better than emitting a stylesheet as prose.
    #[test]
    fn an_unclosed_opaque_element_consumes_the_remainder() {
        assert_eq!(strip_html("<p>kept</p><style>.a{}"), "kept");
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

    #[test]
    fn an_unterminated_tag_does_not_run_away() {
        assert_eq!(strip_html("before <div never closed"), "before");
    }

    /// Swedish mail arrives with its vowels as numeric references often
    /// enough that not decoding them would put `f&#246;rfaller` in front of
    /// the owner.
    #[test]
    fn numeric_and_named_entities_both_decode_and_a_bare_ampersand_survives() {
        assert_eq!(
            strip_html("<p>Tenta i h&#246;st, sal D&amp;E, klockan 9 &lt; 10</p>"),
            "Tenta i höst, sal D&E, klockan 9 < 10"
        );
        assert_eq!(strip_html("<p>&#xE5;&#xE4;&#xF6;</p>"), "åäö");
        // Not entities: left exactly as written.
        assert_eq!(
            strip_html("<p>R&D, 10 & 20, &notanentity</p>"),
            "R&D, 10 & 20, &notanentity"
        );
        // The double-decode the one-pass scanner exists to avoid: a literal
        // `&lt;` in the source stays text rather than becoming markup.
        assert_eq!(strip_html("<p>&amp;lt;b&amp;gt;</p>"), "&lt;b&gt;");
        // A non-ASCII byte immediately after `&` must not panic the slicer.
        assert_eq!(strip_html("<p>&ätbar</p>"), "&ätbar");
    }

    /// Several blank lines become one — enough to read as a paragraph break,
    /// not enough to be a wall of whitespace in a triage prompt.
    #[test]
    fn runs_of_blank_lines_collapse_to_a_single_one() {
        assert_eq!(
            strip_html("<p>one</p><p></p><p></p><p></p><p>two</p>"),
            "one\n\ntwo"
        );
        assert_eq!(strip_html("<div>one</div><div>two</div>"), "one\ntwo");
    }
}
