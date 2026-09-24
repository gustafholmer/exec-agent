//! What the daemon sees: `watch_poll`'s rows, across every authorised
//! account.
//!
//! The daemon calls `watch_poll` on a timer, parses the reply as
//! `[{ external_id, kind, payload }]`, and records each row under
//! `(source = "kth", external_id)`. That key is the whole contract: a row
//! whose id it has seen before is an update, a row whose id is new is news.
//! Everything here exists to make those ids stable and the payloads small.
//!
//! # Two signals
//!
//! * **Unread mail** — `kthmail:<account>:<id>`, the [`MAX_UNREAD`] most
//!   recent unread messages in each account's inbox.
//! * **An account that could not be read** — `ktherr:<account>`, one synthetic
//!   row. See below.
//!
//! # A poll never returns an empty list to hide a failure
//!
//! Three rules, and each of them was a bug in some earlier connector in this
//! project before it was a rule here.
//!
//! * **No authorised accounts is an error, not an empty poll.** A connector
//!   whose token directory was wiped would otherwise report "nothing to tell
//!   you" forever and look perfectly healthy doing it.
//! * **An account that fails contributes a row, not silence.** Its id is
//!   stable, so an account that stays dead is one event in the log rather than
//!   one per poll, and its payload is written to be *scored* — it says that
//!   nothing arriving in that mailbox is reaching the owner, which is the fact
//!   that matters, not "a connector errored".
//! * **If every account fails, the poll returns `Err`.** That is not a partial
//!   poll, it is a connector that cannot do its job, and the scheduler's
//!   breaker is exactly the right thing to see it.
//!
//! With a single authorised account — the expected configuration — the second
//! and third rules coincide: any failure is a total failure and the breaker
//! trips. The per-account shape is kept anyway because `ea_google`'s did not
//! have it, and the week it was added was the week a lapsed private-account
//! token had been silencing work mail.
//!
//! # The payload does not churn
//!
//! The daemon resets an event's triage state whenever its payload changes, so
//! a message carrying a timestamp or a request id would re-notify the owner
//! every poll. Nothing that varies between two identical failures goes into an
//! error row.
//!
//! # Why the body is truncated
//!
//! These payloads are read back into a tier-1 triage prompt, in batches. One
//! newsletter with a 200 KB body would cost more tokens than the rest of the
//! batch put together and tell triage nothing the first two thousand
//! characters did not. [`MAX_BODY_CHARS`] bounds it, visibly.

use anyhow::bail;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::mail::{Mail, MailTransport};

/// The `kind` on an unread-mail row. Triage mutes and keyword rules match on
/// it, so these are stable names.
pub const KIND_MAIL: &str = "mail";
/// The `kind` on the synthetic row reporting an account this poll could not
/// read.
pub const KIND_CONNECTOR_ERROR: &str = "connector_error";

/// How many unread messages one poll fetches per account.
///
/// Unlike Gmail's, this costs one request whatever the number (Graph returns
/// bodies in the list response), so the cap is about the *digest* rather than
/// the request budget: a mailbox with more than 25 unread messages has a
/// problem this connector cannot solve, and the oldest of them are not news.
pub const MAX_UNREAD: usize = 25;

/// How much of a mail body reaches a payload. See the module docs.
pub const MAX_BODY_CHARS: usize = 2000;

/// One change, in the shape the daemon's event store records. `source` is
/// deliberately absent: the daemon supplies it from the connector name it
/// already knows, so a connector cannot write events attributed to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub external_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// `kthmail:<account>:<id>` — the account is part of the id because the same
/// message id could legitimately exist in two mailboxes, and those are two
/// rows, not one.
pub fn mail_external_id(account: &str, id: &str) -> String {
    format!("kthmail:{account}:{id}")
}

/// `ktherr:<account>` — the id of the synthetic row reporting that this
/// account could not be read. One per account, independent of what failed, so
/// the daemon dedups it and a persistently dead account is one row rather than
/// hundreds a day.
pub fn account_error_external_id(account: &str) -> String {
    format!("ktherr:{account}")
}

/// Poll every account for unread mail.
///
/// `now` is accepted for symmetry with the other connectors' polls and to keep
/// the signature stable if a time window is ever added; nothing here currently
/// reads it, because "unread" is not a time-bounded question the way "the next
/// seven days" is.
pub async fn poll(
    transport: &dyn MailTransport,
    accounts: &[String],
    _now: DateTime<Utc>,
) -> anyhow::Result<Vec<WatchEntry>> {
    if accounts.is_empty() {
        bail!(
            "no KTH account has been authorised, so there is nothing to poll. \
             Authorise one with:\n  {}",
            transport.authorize_hint("kth")
        );
    }

    let mut rows: Vec<WatchEntry> = Vec::new();
    let mut failures: Vec<AccountFailure> = Vec::new();

    for account in accounts {
        match transport.list_unread(account, MAX_UNREAD).await {
            Ok(mail) => rows.extend(mail.iter().map(mail_entry)),
            Err(err) => failures.push(AccountFailure::new(
                account,
                &err,
                &transport.authorize_hint(account),
            )),
        }
    }

    if failures.len() == accounts.len() {
        bail!(
            "every authorised KTH account failed this poll. {}",
            failures
                .iter()
                .map(AccountFailure::summary)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    rows.extend(failures.iter().map(AccountFailure::entry));
    Ok(rows)
}

/// What went wrong for one account during one poll.
struct AccountFailure {
    account: String,
    /// `{err:#}` — the whole `anyhow` chain on one line. Never `{err:?}`: that
    /// is a backtrace, and this text ends up in a triage prompt.
    error: String,
    remedy: String,
}

impl AccountFailure {
    fn new(account: &str, err: &anyhow::Error, authorize_hint: &str) -> Self {
        Self {
            account: account.to_string(),
            error: format!("{err:#}"),
            remedy: authorize_hint.to_string(),
        }
    }

    fn summary(&self) -> String {
        format!("Account {:?}: {}", self.account, self.error)
    }

    /// The synthetic row. Everything in it derives from the account and the
    /// error text, so two identical failures produce two identical payloads
    /// and the daemon does not treat the second as news.
    fn entry(&self) -> WatchEntry {
        WatchEntry {
            external_id: account_error_external_id(&self.account),
            kind: KIND_CONNECTOR_ERROR.to_string(),
            payload: serde_json::json!({
                "account": self.account,
                "error": self.error,
                // Written for a triage prompt, which scores what it can read.
                // "A connector errored" is a 20; "you are not being told about
                // anything sent to this mailbox" is the truth and is a 90.
                "summary": format!(
                    "The KTH mailbox {:?} could not be read this poll, so nothing \
                     arriving there is reaching you.",
                    self.account
                ),
                "impact": format!(
                    "While this lasts, no mail to the {:?} KTH account appears in triage \
                     at all: a schedule change, an exam result, a message from an \
                     examiner or a supervisor would all pass unseen.",
                    self.account
                ),
                "remedy": format!(
                    "If the error mentions invalid_grant or HTTP 401, the grant is gone \
                     and only re-authorising restores it:\n  {}\nIf it mentions HTTP 403, \
                     read the \"If KTH refuses consent\" section of \
                     connectors/kth/README.md — the tenant may have withdrawn permission \
                     for this application, which no amount of re-authorising will fix. \
                     Anything else (a 5xx, a connection failure, HTTP 429) is likely \
                     transient and the next poll will clear it.",
                    self.remedy
                ),
            }),
        }
    }
}

/// One unread message, as a watch row, with its body bounded.
pub fn mail_entry(mail: &Mail) -> WatchEntry {
    WatchEntry {
        external_id: mail_external_id(&mail.account, &mail.id),
        kind: KIND_MAIL.to_string(),
        payload: serde_json::json!({
            "account": mail.account,
            "conversation_id": mail.conversation_id,
            "from": mail.from,
            "subject": mail.subject,
            "preview": mail.preview,
            "body": truncate_body(&mail.body),
            "received_at": stamp(mail.received_at),
            "web_link": mail.web_link,
        }),
    }
}

/// Cut a body to [`MAX_BODY_CHARS`] characters, saying so where it was cut.
///
/// Counted in `char`s, not bytes: slicing a UTF-8 string at byte 2000 can land
/// inside a multi-byte character, and `å` is not an edge case in this owner's
/// mail.
pub fn truncate_body(body: &str) -> String {
    let total = body.chars().count();
    if total <= MAX_BODY_CHARS {
        return body.to_string();
    }
    let head: String = body.chars().take(MAX_BODY_CHARS).collect();
    format!("{head}… [truncated; {total} characters in the original]")
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::mail::BoxFuture;

    /// What one account answers with: its mail, or the error it fails with.
    type Reply = Result<Vec<Mail>, String>;

    /// A transport that has never heard of Microsoft.
    ///
    /// Its existence is the proof the seam works: `poll` is exercised
    /// end-to-end without HTTP, OAuth, or Graph, which is exactly what an IMAP
    /// replacement would have to satisfy.
    struct FakeTransport {
        accounts: Vec<String>,
        /// Per account, in the order the poll will walk them.
        replies: Mutex<Vec<(String, Reply)>>,
    }

    impl FakeTransport {
        fn new(replies: Vec<(&str, Reply)>) -> Self {
            Self {
                accounts: replies.iter().map(|(a, _)| a.to_string()).collect(),
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(a, r)| (a.to_string(), r))
                        .collect(),
                ),
            }
        }
    }

    impl MailTransport for FakeTransport {
        fn accounts(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.accounts.clone())
        }

        fn list_unread<'a>(
            &'a self,
            account: &'a str,
            _max: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<Mail>>> {
            let reply = self
                .replies
                .lock()
                .map(|replies| {
                    replies
                        .iter()
                        .find(|(a, _)| a == account)
                        .map(|(_, r)| r.clone())
                })
                .unwrap_or(None);
            Box::pin(async move {
                match reply {
                    Some(Ok(mail)) => Ok(mail),
                    Some(Err(message)) => Err(anyhow::anyhow!(message)),
                    None => Err(anyhow::anyhow!("no such account")),
                }
            })
        }

        fn get<'a>(
            &'a self,
            _account: &'a str,
            _id: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<Mail>> {
            Box::pin(async { Err(anyhow::anyhow!("not used in these tests")) })
        }

        fn authorize_hint(&self, account: &str) -> String {
            format!("ea-kth-authorize {account}")
        }
    }

    fn mail(account: &str, id: &str, body: &str) -> Mail {
        Mail {
            id: id.to_string(),
            conversation_id: format!("conv-{id}"),
            account: account.to_string(),
            from: "Kursansvarig <kurs@kth.se>".to_string(),
            subject: "Tentamen".to_string(),
            preview: "preview".to_string(),
            body: body.to_string(),
            received_at: DateTime::parse_from_rfc3339("2026-09-24T07:15:00Z")
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
            is_read: false,
            web_link: "https://outlook.office365.com/owa/?ItemID=x".to_string(),
        }
    }

    /// The seam, demonstrated rather than asserted about: the entire poll runs
    /// against a transport with no Microsoft in it. An IMAP implementation
    /// would need to satisfy exactly this trait and nothing else.
    #[tokio::test]
    async fn swapping_the_transport_needs_no_change_above_the_seam() {
        let transport = FakeTransport::new(vec![(
            "kth",
            Ok(vec![mail("kth", "m1", "Tentamen flyttad till den 3:e.")]),
        )]);

        let rows = poll(&transport, &["kth".to_string()], Utc::now())
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].external_id, "kthmail:kth:m1");
        assert_eq!(rows[0].kind, KIND_MAIL);
        assert_eq!(rows[0].payload["subject"], "Tentamen");
        assert_eq!(rows[0].payload["body"], "Tentamen flyttad till den 3:e.");
        assert_eq!(rows[0].payload["received_at"], "2026-09-24T07:15:00Z");
    }

    /// **A poll must not answer a failure with `[]`.** An empty list is
    /// indistinguishable from "no unread mail", which is how a connector that
    /// has stopped working goes on looking healthy.
    #[tokio::test]
    async fn a_poll_whose_only_account_fails_returns_an_error_and_not_an_empty_list() {
        let transport = FakeTransport::new(vec![("kth", Err("HTTP 401 Unauthorized".to_string()))]);

        let err = poll(&transport, &["kth".to_string()], Utc::now())
            .await
            .expect_err("a total failure must reach the breaker");
        let err = format!("{err:#}");
        assert!(err.contains("every authorised KTH account failed"), "{err}");
        assert!(err.contains("401"), "{err}");
    }

    /// With no accounts at all the poll must be loud too, and must name the
    /// command that fixes it.
    #[tokio::test]
    async fn a_poll_with_no_authorised_accounts_errors_rather_than_reporting_nothing() {
        let transport = FakeTransport::new(vec![]);
        let err = format!("{:#}", poll(&transport, &[], Utc::now()).await.unwrap_err());
        assert!(err.contains("ea-kth-authorize"), "{err}");
        assert!(!err.contains("[]"), "{err}");
    }

    /// One dead account does not silence a healthy one, and the dead one gets
    /// a row a triage prompt can score.
    #[tokio::test]
    async fn one_dead_account_contributes_a_scorable_row_and_does_not_silence_the_other() {
        let transport = FakeTransport::new(vec![
            ("kth", Ok(vec![mail("kth", "m1", "hej")])),
            ("kth-staff", Err("HTTP 401 Unauthorized".to_string())),
        ]);

        let rows = poll(
            &transport,
            &["kth".to_string(), "kth-staff".to_string()],
            Utc::now(),
        )
        .await
        .unwrap();

        assert_eq!(rows.len(), 2);
        let error_row = rows
            .iter()
            .find(|r| r.kind == KIND_CONNECTOR_ERROR)
            .expect("the failing account must be reported");
        assert_eq!(error_row.external_id, "ktherr:kth-staff");
        let summary = error_row.payload["summary"].as_str().unwrap_or_default();
        assert!(
            summary.contains("nothing arriving there is reaching you"),
            "the payload must carry the consequence, not the mechanism: {summary}"
        );
        assert!(error_row.payload["remedy"]
            .as_str()
            .unwrap_or_default()
            .contains("ea-kth-authorize kth-staff"));
    }

    /// The daemon resets triage state when a payload changes. An error row
    /// that varied between two identical failures would re-notify the owner
    /// every poll, forever.
    #[tokio::test]
    async fn an_error_row_is_byte_identical_across_two_identical_failures() {
        let make = || {
            FakeTransport::new(vec![
                ("kth", Ok(vec![])),
                ("kth-staff", Err("HTTP 401 Unauthorized".to_string())),
            ])
        };
        let accounts = ["kth".to_string(), "kth-staff".to_string()];

        let first = poll(&make(), &accounts, Utc::now()).await.unwrap();
        let second = poll(&make(), &accounts, Utc::now() + chrono::Duration::hours(3))
            .await
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_long_body_is_truncated_visibly_and_on_a_character_boundary() {
        let body = "å".repeat(MAX_BODY_CHARS + 50);
        let truncated = truncate_body(&body);
        assert!(truncated.contains("[truncated;"), "{truncated}");
        assert!(truncated.starts_with(&"å".repeat(MAX_BODY_CHARS)));
        assert_eq!(truncate_body("short"), "short");
    }
}
