//! Putting untrusted text into a prompt without letting it become prompt.
//!
//! Two session kinds splice text somebody else wrote into what a model reads:
//! [`crate::chat`] renders remembered facts into the chat system prompt, and
//! [`crate::briefings`] renders connector material — mail subjects, calendar
//! titles, Notion page titles — into the briefing prompt. Both hold a tool
//! that changes the world, so both draw the same line between *content* and
//! *instruction*: the untrusted text goes between two markers, the prompt says
//! in words that what is between them is data and never orders, and the
//! markers themselves are taken out of the text ([`fenced`]) so the text
//! cannot close the block early and continue as prompt.
//!
//! One helper, not two: the subtlety below is the kind that gets fixed once
//! and reintroduced by the second copy.

/// `text` with `open` and `close` taken out of it — including any that only
/// appear once every occurrence of the other marker has already been
/// stripped.
///
/// Without this a payload containing the closing marker ends the block and
/// everything after it reads as prompt again — the delimiting would be
/// decoration. A single `str::replace` pass is not enough: it does not
/// re-scan what it produced, so a marker with another copy of itself spliced
/// into its own middle (`</remem</remembered-notes>bered-notes>`) has that
/// inner copy removed and the two remaining halves fall back together into
/// the real marker. Repeating the removal until the text stops changing
/// closes that gap; nesting the marker inside itself again just costs one
/// more pass, and both callers cap the length of what they render (a fact by
/// `MAX_TOPIC_CHARS` / `MAX_BODY_CHARS`, a briefing section by the row limit
/// on the events it is built from), so the number of passes is bounded by
/// that cap rather than by the input.
pub fn fenced(text: &str, open: &str, close: &str) -> String {
    let mut text = text.to_string();
    loop {
        let stripped = text.replace(close, "").replace(open, "");
        if stripped == text {
            return stripped;
        }
        text = stripped;
    }
}

#[cfg(test)]
mod tests {
    use super::fenced;

    const OPEN: &str = "<x>";
    const CLOSE: &str = "</x>";

    #[test]
    fn a_marker_nested_inside_itself_does_not_survive_the_stripping() {
        // One pass would remove the inner `</x>` and leave `</` + `x>`
        // sitting next to each other, which is the marker again.
        assert_eq!(fenced("</</x>x>after", OPEN, CLOSE), "after");
        assert_eq!(fenced("<<x>x>after", OPEN, CLOSE), "after");
    }

    #[test]
    fn text_with_no_marker_in_it_is_returned_unchanged() {
        assert_eq!(fenced("an ordinary line", OPEN, CLOSE), "an ordinary line");
    }
}
