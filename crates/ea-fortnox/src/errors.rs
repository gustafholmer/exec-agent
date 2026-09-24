//! The one error type every Fortnox call returns.
//!
//! # Why a flat enum rather than the upstream class hierarchy
//!
//! The TypeScript this crate is translated from grows a small inheritance
//! tree: `FortnoxError` with a `status` and an optional numeric `code`,
//! `FortnoxAuthError extends FortnoxError` pinned to 401, and
//! `RateLimitError extends FortnoxError` pinned to 429 with a `retryAfterMs`.
//! In TypeScript the subclasses buy `instanceof` checks at the call sites.
//!
//! Here the three cases a caller actually branches on are: *the grant is
//! broken and a human has to go to a browser* (`Auth`), *Fortnox answered,
//! and said no* (`Api`, carrying the status so 429 and 404 and 500 are all
//! distinguishable), and *the request never got an answer* (`Transport`). A
//! 429 is an `Api { status: 429, .. }`; nothing needs a separate type to see
//! that. The numeric Fortnox `code` is rendered into the message rather than
//! kept as a field — see [`parse_fortnox_error`].
//!
//! # What may appear in one of these
//!
//! Error *bodies* may be quoted, truncated to [`BODY_SNIPPET`] characters: by
//! definition a Fortnox error response did not carry a token. A *success*
//! body may never be quoted — that is the document that holds the access and
//! refresh tokens — and nothing in this crate puts one in an error. Neither
//! does anything put a token, a refresh token or the client secret into a
//! message, a URL, or a `Debug` impl; `FortnoxError`'s derived `Debug` is
//! safe precisely because no variant is ever constructed from a credential.

/// How much of an unexpected response body to quote back. Enough to recognise
/// a Fortnox error envelope or an HTML login page, not enough to fill a log.
pub const BODY_SNIPPET: usize = 300;

/// Everything that can go wrong talking to Fortnox.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FortnoxError {
    /// The stored grant is missing, unreadable, or was refused. Only a person
    /// re-running the authorize command fixes this, so every message in this
    /// variant names that command.
    #[error("{0}")]
    Auth(String),
    /// Fortnox answered with a non-2xx status (or a 2xx this build cannot
    /// read). `status` is the HTTP status; `body` is the explanation, already
    /// truncated.
    #[error("Fortnox request failed (HTTP {status}): {body}")]
    Api { status: u16, body: String },
    /// The request never completed: DNS, TLS, a timeout, a dropped
    /// connection. Distinct from [`FortnoxError::Api`] because a dropped
    /// connection is not Fortnox saying no, and must not be reported as one.
    #[error("{0}")]
    Transport(String),
}

impl FortnoxError {
    /// The HTTP status, when there was one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Turn a Fortnox error response into a [`FortnoxError::Api`].
///
/// Fortnox wraps its own diagnostics in an `ErrorInformation` object, and is
/// inconsistent about the case of the keys inside it — the live API has been
/// seen answering both `{"ErrorInformation":{"message":..,"code":..}}` and
/// `{"ErrorInformation":{"Message":..,"Code":..}}`. Both are read.
///
/// The numeric `code` is rendered into the message rather than kept as a
/// field: it is a Fortnox support-ticket number, useful to a human reading a
/// log and never branched on in code.
pub fn parse_fortnox_error(status: u16, body: &str) -> FortnoxError {
    if let Some(message) = fortnox_message(body) {
        return FortnoxError::Api {
            status,
            body: message,
        };
    }
    FortnoxError::Api {
        status,
        body: snippet(body),
    }
}

/// The `ErrorInformation` message, rendered with its code, if the body is one.
fn fortnox_message(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let info = field(&parsed, &["ErrorInformation"])?;
    let message = match field(info, &["message"])? {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if message.trim().is_empty() {
        return None;
    }
    // The code is a Fortnox support-ticket number. Rendered without quotes
    // whether it arrives as a JSON number or a JSON string.
    let code = field(info, &["code"]).map(|value| match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    Some(match code {
        Some(code) => format!("Fortnox error {code}: {message}"),
        // The TypeScript renders `Fortnox error : <message>` here, because it
        // interpolates an empty string and then trims only the ends. The
        // stray colon-space is a bug in a message, not a behaviour to port.
        None => format!("Fortnox error: {message}"),
    })
}

/// Look a key up in a JSON object ignoring ASCII case, which is the only case
/// Fortnox's keys differ in.
fn field<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    let object = value.as_object()?;
    for key in keys {
        for (actual, found) in object {
            if actual.eq_ignore_ascii_case(key) {
                return Some(found);
            }
        }
    }
    None
}

/// Truncate a response body for quoting in an error.
pub fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    let total = trimmed.chars().count();
    if total <= BODY_SNIPPET {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(BODY_SNIPPET).collect();
    format!("{head}… ({total} chars total)")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Translated from `fortnox/errors.test.ts`.
    //
    // The upstream assertions on `err.code` become assertions on the rendered
    // message: this crate's error enum has no `code` field (see the module
    // docs), and the TypeScript renders the same number into the message it
    // builds, so the fact under test — "the Fortnox code reaches the reader"
    // — is unchanged.
    // -----------------------------------------------------------------------

    #[test]
    fn error_information_yields_the_message_and_code_case_insensitively() {
        let err = parse_fortnox_error(
            404,
            r#"{"ErrorInformation":{"error":1,"message":"Kan inte hitta kontot.","code":2000423}}"#,
        );
        assert_eq!(err.status(), Some(404));
        let rendered = err.to_string();
        assert!(rendered.contains("Kan inte hitta kontot"), "{rendered}");
        assert!(rendered.contains("2000423"), "{rendered}");
    }

    #[test]
    fn pascal_case_error_information_keys_are_read_too() {
        let err = parse_fortnox_error(
            400,
            r#"{"ErrorInformation":{"Error":1,"Message":"Bad","Code":2000106}}"#,
        );
        assert_eq!(err.status(), Some(400));
        let rendered = err.to_string();
        assert!(rendered.contains("Bad"), "{rendered}");
        assert!(rendered.contains("2000106"), "{rendered}");
    }

    #[test]
    fn a_body_that_is_not_a_fortnox_error_falls_back_to_the_status() {
        let err = parse_fortnox_error(500, "Internal Server Error");
        assert_eq!(err.status(), Some(500));
        let rendered = err.to_string();
        assert!(rendered.contains("500"), "{rendered}");
        assert!(rendered.contains("Internal Server Error"), "{rendered}");
    }

    // ---- Beyond the upstream file -----------------------------------------

    /// The TypeScript quotes the whole body. A Fortnox 500 can be a multi-
    /// kilobyte HTML page, and this error ends up in a log line.
    #[test]
    fn a_very_long_body_is_truncated_before_it_reaches_the_message() {
        let err = parse_fortnox_error(502, &"x".repeat(5_000));
        let rendered = err.to_string();
        assert!(rendered.len() < 1_000, "{} chars", rendered.len());
        assert!(rendered.contains("5000 chars total"), "{rendered}");
    }

    /// `ErrorInformation` with no message is not a Fortnox error envelope we
    /// can improve on — fall back rather than render an empty message.
    #[test]
    fn error_information_without_a_message_falls_back_to_the_raw_body() {
        let err = parse_fortnox_error(400, r#"{"ErrorInformation":{"error":1}}"#);
        let rendered = err.to_string();
        assert!(rendered.contains("ErrorInformation"), "{rendered}");
    }

    #[test]
    fn a_missing_code_still_renders_the_message() {
        let err = parse_fortnox_error(400, r#"{"ErrorInformation":{"message":"Nope"}}"#);
        let rendered = err.to_string();
        assert!(rendered.contains("Nope"), "{rendered}");
    }

    #[test]
    fn only_api_errors_carry_a_status() {
        assert_eq!(FortnoxError::Auth("re-run authorize".into()).status(), None);
        assert_eq!(FortnoxError::Transport("dns".into()).status(), None);
    }
}
