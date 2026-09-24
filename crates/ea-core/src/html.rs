//! The HTML-to-text stripper both mail connectors use.
//!
//! # Why this is in `ea-core`
//!
//! It started as sixty stateless lines in `ea_google::gmail`, and `ea_kth::mail`
//! copied it rather than depending on it — deliberately, with a comment saying
//! the right moment to share it was when a third connector needed one. That
//! judgement turned out to be wrong in a way worth recording, because the two
//! copies did not stay equal: the KTH copy fixed four things in the Gmail one
//! and the Gmail one kept running with the bugs.
//!
//! * The Gmail copy decoded five entities through a chain of `replace` calls
//!   with **no `&nbsp;`** in it. Outlook-authored mail — which Gmail carries
//!   plenty of — is full of `&nbsp;`, so a literal `&nbsp;` reached the triage
//!   prompt, spending characters against a 2000-character body cap and reading
//!   as noise.
//! * It decoded no numeric entities at all, so Swedish mail arrived as
//!   `f&#246;rfaller`.
//! * It dropped `<style>` and `<script>` *tags* but kept their *contents*, so
//!   an Exchange stylesheet running to hundreds of rules landed at the top of
//!   the body and pushed the actual message out of the truncated prompt.
//! * A chain of `replace` calls decodes its own output: source text containing
//!   a literal `&lt;` (written `&amp;lt;`) became `<` and read as markup. Its
//!   own comment admitted the hazard.
//!
//! Both connectors feed the same triage prompt through the same cap, so those
//! were not cosmetic differences between two tastes — they were one connector
//! running with defects the other had already fixed. This module is the KTH
//! version, lifted whole, with one further fix (see [`is_droppable_control`]).
//!
//! # The contract, deliberately limited
//!
//! This is not an HTML parser and does not try to be. It:
//!
//! * converts `<br>` and closing block-level tags ([`NEWLINE_CLOSERS`]) to
//!   newlines and drops every other tag;
//! * drops [`OPAQUE_ELEMENTS`] (`<style>`, `<script>`) **including their
//!   contents**;
//! * decodes named ([`NAMED_ENTITIES`]) and numeric entities in one
//!   left-to-right pass, so nothing double-decodes;
//! * trims each line, collapses runs of blank lines to one, and drops control
//!   characters.
//!
//! CDATA, comments and malformed tags get no special handling. None of that
//! matters for the job the output does: making text scorable by a triage
//! prompt, not reproducing the document.

/// Closing tags that become a newline.
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
/// Outlook and Exchange generate mail with `<style>` blocks running to hundreds
/// of CSS rules; dropping only the tags leaves every one of those rules in the
/// text. The body reaching triage is truncated at 2000 characters, so a
/// stylesheet at the top would push the actual message out of the prompt
/// entirely — the same "the mail that mattered is the mail that vanishes"
/// failure the HTML fallback exists to prevent, arriving by a different route.
const OPAQUE_ELEMENTS: [&str; 2] = ["style", "script"];

/// The named entities common enough in real mail to bother with.
///
/// `nbsp` is on this list and was not on the Gmail copy's. See the module docs.
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

/// Decode HTML entities in one left-to-right pass.
///
/// Numeric entities are decoded as well as named ones: Swedish mail is full of
/// `å ä ö`, and Exchange emits them as `&#229; &#228; &#246;` often enough that
/// leaving them raw would put literal `f&#246;rfaller` in front of a triage
/// prompt and in front of the owner.
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
                // `&#0;` is a real numeric reference and decodes to a real
                // NUL, which would then travel into an event payload, a JSON
                // prompt and a SQLite TEXT column. The entity is consumed
                // either way — leaving `&#0;` in the text would be no better —
                // but nothing is emitted for it. See `is_droppable_control`.
                if !is_droppable_control(decoded) {
                    out.push(decoded);
                }
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

/// A character that must not reach the output.
///
/// Every control character except the two this stripper's own output is made
/// of: `\n`, which it emits for block-level tags, and `\t`, which is ordinary
/// horizontal whitespace in prose. Everything else — a NUL from `&#0;`, a bare
/// `\r` from a CRLF mail body, an escape sequence someone embedded — is
/// invisible to a reader and costs characters against the triage prompt's cap
/// at best, and at worst travels as a NUL into a JSON payload and a SQLite
/// TEXT column.
fn is_droppable_control(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

/// Trims each line, drops control characters, and collapses two or more
/// consecutive blank lines into one, so stripped markup (which tends to leave
/// a blank line per removed tag) reads as paragraphs rather than a wall of
/// blank lines.
fn collapse_blank_lines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut previous_blank = false;
    for line in input.lines() {
        // Control characters are filtered before the emptiness test, so a line
        // holding nothing but a stray `\r` or a decoded NUL counts as blank
        // rather than as content.
        let cleaned: String = line.chars().filter(|c| !is_droppable_control(*c)).collect();
        let trimmed = cleaned.trim();
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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Carried over from ea-kth
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Carried over from ea-google
    // -----------------------------------------------------------------------

    /// `gmail.rs`'s entity-and-blank-run test, which used to exercise the
    /// Gmail copy through `extract_body`.
    #[test]
    fn common_entities_decode_and_blank_runs_collapse() {
        let body =
            strip_html("<p>Tom &amp; Jerry say &quot;hi&quot;</p>\n\n\n<p>next paragraph</p>");
        assert!(body.contains("Tom & Jerry say \"hi\""), "{body:?}");
        assert!(!body.contains("\n\n\n"), "{body:?}");
    }

    /// `gmail.rs`'s HTML-only fallback fixture, stripped directly. The Gmail
    /// client's own test still drives this through `extract_body`; this one
    /// pins the text the stripper is responsible for.
    #[test]
    fn an_html_only_message_strips_to_readable_text() {
        let text = strip_html(
            "<html><body><p>Your invoice is ready.</p><p>Amount: <b>100 SEK</b></p></body></html>",
        );
        assert_eq!(text, "Your invoice is ready.\nAmount: 100 SEK");
    }

    /// Non-ASCII must survive the stripper untouched — the same Swedish
    /// fixture `gmail.rs` uses for its base64url round-trip, which would catch
    /// a byte-wise slice that a plain ASCII fixture would not.
    #[test]
    fn swedish_text_round_trips_through_the_stripper() {
        let swedish = "Räksmörgås — åäö";
        assert_eq!(strip_html(swedish), swedish);
        assert_eq!(
            strip_html("<p>R&#228;ksm&#246;rg&#229;s &#8212; &#229;&#228;&#246;</p>"),
            swedish
        );
    }

    // -----------------------------------------------------------------------
    // The two cases the Gmail copy failed
    // -----------------------------------------------------------------------

    /// `&nbsp;` was absent from the Gmail copy's five-entity `replace` chain,
    /// so a literal `&nbsp;` reached the triage prompt. Outlook emits them
    /// constantly and Gmail carries plenty of Outlook-authored mail.
    #[test]
    fn a_non_breaking_space_becomes_a_space() {
        assert_eq!(
            strip_html("<p>Mer&nbsp;info &amp; anm&#228;lan</p>"),
            "Mer info & anmälan"
        );
        // Uppercase and mixed-case spellings are real in the wild.
        assert_eq!(strip_html("<p>a&NBSP;b&NbSp;c</p>"), "a b c");
        // A line of nothing but non-breaking spaces is blank, not content.
        assert_eq!(
            strip_html("<p>one</p><p>&nbsp;</p><p>two</p>"),
            "one\n\ntwo"
        );
    }

    /// The Gmail copy decoded no numeric entity but `&#39;`, and that one only
    /// because it was spelled out in the `replace` chain.
    #[test]
    fn numeric_entities_decode_decimal_and_hex() {
        assert_eq!(strip_html("<p>&#82;&#38;&#68;</p>"), "R&D");
        assert_eq!(strip_html("<p>&#x52;&#x26;&#x44;</p>"), "R&D");
        // The one numeric entity the Gmail chain did handle, still handled.
        assert_eq!(strip_html("<p>it&#39;s</p>"), "it's");
    }

    // -----------------------------------------------------------------------
    // The nit fixed on the way past
    // -----------------------------------------------------------------------

    /// `&#0;` is a valid numeric reference to a NUL. Decoding it faithfully
    /// put a NUL into an event payload, a JSON prompt and a SQLite TEXT
    /// column; the surrounding text is kept and the control character is not.
    #[test]
    fn a_control_character_never_reaches_the_output() {
        assert_eq!(strip_html("<p>a&#0;b</p>"), "ab");
        assert_eq!(strip_html("<p>a&#x1B;b&#7;c</p>"), "abc");
        // Literal control characters in the source are dropped too, and a CRLF
        // body does not leave a stray `\r` at the end of every line.
        assert_eq!(strip_html("a\u{0}b"), "ab");
        assert_eq!(strip_html("one\r\ntwo"), "one\ntwo");
        // A line that is nothing but a control character is blank.
        assert_eq!(strip_html("<p>one</p><p>&#0;</p><p>two</p>"), "one\n\ntwo");
        // Tab and newline are not dropped: they are what this emits and what
        // prose contains.
        assert_eq!(strip_html("a\tb<br>c"), "a\tb\nc");
    }
}
