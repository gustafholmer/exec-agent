//! `watch_poll`: the one tool the daemon calls on a timer rather than a model
//! calling it.
//!
//! The shape of a poll, the ids, and the reasoning behind both live in
//! [`crate::watch`]. This module is the MCP surface over them, plus the one
//! rule that matters at this layer: **every failure propagates**. See
//! [`FortnoxServer::poll`].

use chrono::Utc;
use rmcp::{tool, tool_router};

use crate::watch::{deadline_entries, invoice_entry, WatchEntry};

use super::{render, to_json, FortnoxServer};

/// The unpaid filter, the same one `unpaid_invoices` uses.
const UNPAID: (&str, &str) = ("filter", "unpaid");

#[tool_router(router = watch_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "Poll Fortnox for things the owner should know about: unpaid customer \
                       invoices (with an overdue flag) and upcoming Swedish tax deadlines \
                       (AGI, preliminärskatt, moms). Returns a JSON array of \
                       { external_id, kind, payload }. Called by the daemon on a timer; \
                       errors are reported rather than swallowed, so a connector whose \
                       Fortnox grant has lapsed is visible instead of quiet. The deadline \
                       rows are computed from the calendar, not read from Skatteverket, and \
                       each says so."
    )]
    pub async fn watch_poll(&self) -> Result<String, String> {
        let entries = self.poll().await?;
        to_json(&entries)
    }
}

impl FortnoxServer {
    /// One poll, as of today (UTC).
    ///
    /// Splitting the clock out into [`FortnoxServer::poll_at`] is what lets
    /// the tests below pin the `overdue` flag against fixed dates instead of
    /// building fixtures relative to whenever the suite happens to run.
    pub async fn poll(&self) -> Result<Vec<WatchEntry>, String> {
        self.poll_at(Utc::now().date_naive()).await
    }

    /// One poll, as of `today`.
    ///
    /// # Errors propagate; they never become `[]`
    ///
    /// A failed Fortnox call is an `Err` all the way out to `is_error: true`
    /// on the wire, which trips the daemon's circuit breaker. Returning an
    /// empty array instead would be indistinguishable from a quiet week: the
    /// breaker would never trip, `ea status` would stay green, and a lapsed
    /// refresh token would take this connector off the air for months without
    /// anybody being told. That has already happened once to this owner.
    ///
    /// The tax deadlines are therefore *not* emitted on a failed poll, even
    /// though they are pure and would have cost nothing to compute. A partial
    /// answer is a successful answer as far as the daemon can tell, and a
    /// poll that keeps reporting deadlines while silently reporting no
    /// invoices is precisely the failure mode this rule exists to prevent.
    pub async fn poll_at(&self, today: chrono::NaiveDate) -> Result<Vec<WatchEntry>, String> {
        let rows = self
            .client()?
            .get_all("invoices", "Invoices", &[UNPAID])
            .await
            .map_err(render)?;

        let mut entries: Vec<WatchEntry> = rows
            .iter()
            .filter_map(|row| invoice_entry(row, today))
            .collect();
        entries.extend(deadline_entries(today));
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use chrono::NaiveDate;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::test_support::{json_body, server_for, unconfigured};
    use crate::watch::{TAX_DEADLINE_KIND, UNPAID_INVOICE_KIND};

    fn date(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    async fn fortnox_with(invoices: serde_json::Value) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/invoices"))
            .respond_with(json_body(serde_json::json!({
                "MetaInformation": { "@TotalPages": 1, "@CurrentPage": 1 },
                "Invoices": invoices,
            })))
            .mount(&mock)
            .await;
        mock
    }

    fn two_invoices() -> serde_json::Value {
        serde_json::json!([
            {
                "DocumentNumber": "1042",
                "CustomerNumber": "C-1",
                "CustomerName": "Acme AB",
                "InvoiceDate": "2026-07-01",
                "DueDate": "2026-07-31",
                "Currency": "SEK",
                "Total": 12500.0,
                "Balance": 12500.0,
            },
            {
                "DocumentNumber": "1043",
                "CustomerNumber": "C-2",
                "CustomerName": "Beta HB",
                "InvoiceDate": "2026-09-01",
                "DueDate": "2026-10-31",
                "Currency": "SEK",
                "Total": 4000.0,
                "Balance": 4000.0,
            },
        ])
    }

    fn invoices_in(entries: &[WatchEntry]) -> Vec<&WatchEntry> {
        entries
            .iter()
            .filter(|entry| entry.kind == UNPAID_INVOICE_KIND)
            .collect()
    }

    #[tokio::test]
    async fn one_entry_per_unpaid_invoice() {
        let mock = fortnox_with(two_invoices()).await;
        let entries = server_for(&mock)
            .poll_at(date("2026-09-24"))
            .await
            .expect("a poll");

        let invoices = invoices_in(&entries);
        assert_eq!(invoices.len(), 2, "{entries:#?}");
        let ids: BTreeSet<&str> = invoices
            .iter()
            .map(|entry| entry.external_id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["fortnox-invoice:1042", "fortnox-invoice:1043"]
                .into_iter()
                .collect()
        );
    }

    #[tokio::test]
    async fn an_overdue_invoice_is_flagged_and_a_future_one_is_not() {
        let mock = fortnox_with(two_invoices()).await;
        let entries = server_for(&mock)
            .poll_at(date("2026-09-24"))
            .await
            .expect("a poll");

        let overdue: Vec<(&str, &serde_json::Value)> = invoices_in(&entries)
            .iter()
            .map(|entry| (entry.external_id.as_str(), &entry.payload["overdue"]))
            .collect();
        assert_eq!(
            overdue,
            vec![
                ("fortnox-invoice:1042", &serde_json::Value::Bool(true)),
                ("fortnox-invoice:1043", &serde_json::Value::Bool(false)),
            ],
            "due 2026-07-31 is past, due 2026-10-31 is not"
        );
    }

    #[tokio::test]
    async fn the_tax_deadlines_appear_alongside_the_invoices() {
        let mock = fortnox_with(two_invoices()).await;
        let entries = server_for(&mock)
            .poll_at(date("2026-09-24"))
            .await
            .expect("a poll");

        let deadlines: Vec<&WatchEntry> = entries
            .iter()
            .filter(|entry| entry.kind == TAX_DEADLINE_KIND)
            .collect();
        assert!(
            !deadlines.is_empty(),
            "the 12th of October is well inside the horizon: {entries:#?}"
        );
        for entry in deadlines {
            assert!(
                entry.external_id.starts_with("tax-deadline:"),
                "{}",
                entry.external_id
            );
            assert_eq!(
                entry.payload["note"],
                crate::deadlines::REMINDER_NOTE,
                "{}",
                entry.payload
            );
        }
    }

    /// Idempotent at the level the daemon cares about: two polls of an
    /// unchanged Fortnox produce the same ids and the same payloads, so the
    /// event store records an update with nothing in it and triage is not
    /// disturbed.
    #[tokio::test]
    async fn polling_twice_produces_the_same_ids_and_the_same_payloads() {
        let mock = fortnox_with(two_invoices()).await;
        let server = server_for(&mock);
        let today = date("2026-09-24");

        let first = server.poll_at(today).await.expect("a poll");
        let second = server.poll_at(today).await.expect("a second poll");
        assert_eq!(first, second);

        let ids: BTreeSet<&str> = first
            .iter()
            .map(|entry| entry.external_id.as_str())
            .collect();
        assert_eq!(
            ids.len(),
            first.len(),
            "two rows sharing an id means one is silently dropped"
        );
    }

    /// The other half: something that really did change must look different,
    /// or `EventStore` will not re-open the row for triage.
    #[tokio::test]
    async fn an_invoice_whose_balance_changed_comes_back_with_a_changed_payload() {
        let today = date("2026-09-24");

        let full = fortnox_with(two_invoices()).await;
        let before = server_for(&full).poll_at(today).await.expect("a poll");

        let mut part_paid = two_invoices();
        part_paid[0]["Balance"] = serde_json::json!(2500.0);
        let partial = fortnox_with(part_paid).await;
        let after = server_for(&partial).poll_at(today).await.expect("a poll");

        let pick = |entries: &[WatchEntry]| {
            entries
                .iter()
                .find(|entry| entry.external_id == "fortnox-invoice:1042")
                .expect("invoice 1042")
                .clone()
        };
        let before = pick(&before);
        let after = pick(&after);

        assert_eq!(
            before.external_id, after.external_id,
            "the same invoice keeps its id, so this is an update and not a new row"
        );
        assert_ne!(
            before.payload, after.payload,
            "a part payment must be visible, or the row stays closed in triage"
        );
        assert_eq!(before.payload["balance"], 12500.0);
        assert_eq!(after.payload["balance"], 2500.0);
    }

    /// The rule the whole module exists for. An error must be an error.
    #[tokio::test]
    async fn a_client_error_propagates_rather_than_returning_an_empty_array() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/invoices"))
            .respond_with(ResponseTemplate::new(403).set_body_raw(
                r#"{"ErrorInformation":{"message":"not authorised for invoices"}}"#,
                "application/json",
            ))
            .mount(&mock)
            .await;

        let error = server_for(&mock)
            .poll_at(date("2026-09-24"))
            .await
            .expect_err("a 403 must not be reported as nothing to tell you");
        assert!(error.contains("fortnox:"), "{error}");

        // And through the tool, which is what reaches the wire.
        let error = server_for(&mock)
            .watch_poll()
            .await
            .expect_err("watch_poll must report the failure, not return []");
        assert_ne!(error.trim(), "[]");
    }

    /// The lapsed-grant case specifically: no credentials at all is still an
    /// error, not silence. The deadlines are pure and could have been emitted;
    /// they are deliberately not, because a poll that half works reads as a
    /// poll that works.
    #[tokio::test]
    async fn an_unconfigured_connector_reports_the_missing_credentials() {
        let error = unconfigured()
            .poll_at(date("2026-09-24"))
            .await
            .expect_err("no credentials must not look like a quiet week");
        assert!(error.contains("no usable credentials"), "{error}");
        assert!(error.contains("app.json"), "{error}");
    }

    /// `watch_poll` serialises to exactly the array the daemon parses.
    #[tokio::test]
    async fn watch_poll_answers_a_json_array_of_external_id_kind_payload() {
        let mock = fortnox_with(two_invoices()).await;
        let text = server_for(&mock).watch_poll().await.expect("watch_poll");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text).expect("a JSON array");
        assert!(!parsed.is_empty());
        for row in parsed {
            assert!(row.get("external_id").and_then(|v| v.as_str()).is_some());
            assert!(row.get("kind").and_then(|v| v.as_str()).is_some());
            assert!(row.get("payload").is_some());
        }
    }
}
