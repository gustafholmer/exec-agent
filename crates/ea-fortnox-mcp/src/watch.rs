//! What the daemon sees: `watch_poll`'s two signals.
//!
//! The daemon calls `watch_poll` on a timer (daily — see
//! `connectors/fortnox/connector.toml`), parses the reply as
//! `[{ external_id, kind, payload }]`, and records each row under
//! `(source = "fortnox", external_id)`. That key is the whole contract: a row
//! whose id it has seen before is an update, a row whose id is new is news,
//! and a row whose *payload* changed re-opens the event for triage.
//! Everything here exists to make those ids stable and the payloads a
//! function of the thing being watched rather than of the clock.
//!
//! # Two signals
//!
//! * **Unpaid customer invoices** — `fortnox-invoice:<document number>`, one
//!   per open kundfaktura, carrying an `overdue` flag computed against today.
//! * **Tax deadlines** — `tax-deadline:<kind>:<period>`, from
//!   [`crate::deadlines`], which is pure arithmetic and needs no API at all.
//!
//! # Why customer invoices and not supplier invoices
//!
//! The external id is `fortnox-invoice:<number>`, with nothing in it naming
//! which side of the ledger the row came from. Fortnox numbers kundfakturor
//! and leverantörsfakturor in two independent sequences, so polling both into
//! that id space would eventually have invoice 1042 from one side collide with
//! invoice 1042 from the other — and a collision here is not an error anybody
//! sees. It is one row in the event store flipping between two invoices'
//! payloads on alternate polls, re-opening for triage every time, with the
//! numbers on screen belonging to whichever it happened to be that morning.
//!
//! Receivables are the side this connector watches, then: money owed to the
//! owner's company, which nobody else is going to chase. Payables are visible
//! through the `unpaid_invoices` tool with `kind: "supplier"` whenever a
//! session asks. Widening the watcher to both is a real improvement and a
//! small one — it needs a side in the id (`fortnox-invoice:customer:1042`),
//! which is a breaking change to every id already recorded, so it belongs in
//! its own change rather than smuggled into this one.
//!
//! # Why an error is never an empty array
//!
//! [`FortnoxServer::poll`] propagates every failure. If it returned `[]`
//! instead, the daemon would see exactly what it sees on a quiet week —
//! nothing to report — the circuit breaker would never trip, and `ea status`
//! would show the connector green. That is not hypothetical here: the owner's
//! Fortnox refresh token has already lapsed once, silently. A connector whose
//! grant dies must become *noisier*, not quieter.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::deadlines::{self, Deadline};
use crate::tools::{amount_value, coerce_amount};

/// The `kind` on an unpaid-invoice row. Triage mutes and keywords match on it,
/// so it is a stable name rather than something derived per invoice.
pub const UNPAID_INVOICE_KIND: &str = "unpaid_invoice";

/// The `kind` on a tax-deadline row.
pub const TAX_DEADLINE_KIND: &str = "tax_deadline";

/// One change, in the shape the daemon's event store records:
/// `(source, external_id)` is the idempotency key, `source` being the
/// connector name the daemon already knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchEntry {
    pub external_id: String,
    pub kind: String,
    pub payload: Value,
}

/// `fortnox-invoice:<document number>` — stable across polls, so an invoice
/// that has not changed is recorded once however often it is seen.
pub fn invoice_external_id(document_number: &str) -> String {
    format!("fortnox-invoice:{document_number}")
}

/// `tax-deadline:<kind>:<period>`, e.g. `tax-deadline:moms:2026-09`.
///
/// Both halves are needed: three declarations can fall due on the same day,
/// and the same declaration recurs every period.
pub fn deadline_external_id(deadline: &Deadline) -> String {
    format!(
        "tax-deadline:{}:{}",
        deadline.kind.as_str(),
        deadline.period
    )
}

/// One Fortnox invoice row as a watch entry, or [`None`] when the row carries
/// no usable `DocumentNumber`.
///
/// A row with no number is skipped rather than given a synthetic id: a
/// synthetic id would be unstable across polls, so the same invoice would
/// arrive as a new event every day forever. Skipping it loses one row from one
/// poll; the alternative loses the owner's willingness to read any of them.
///
/// # The payload is a fact about the invoice, plus one about today
///
/// `overdue` is the single field computed against the clock, and it is a
/// boolean on purpose. A `days_overdue` integer would change every day, and
/// because the daemon re-opens an event whose payload changed, every overdue
/// invoice would nag once a day until it was paid. The boolean flips exactly
/// once, on the morning after the due date, which is the one moment worth
/// telling somebody about.
///
/// Amounts go through the same [`coerce_amount`] / [`amount_value`] pair the
/// rest of this crate uses, so Fortnox answering `"1000.00"` on one poll and
/// `1000` on the next produces the same payload rather than a spurious change.
pub fn invoice_entry(row: &Value, today: NaiveDate) -> Option<WatchEntry> {
    let document_number = string_field(row, "DocumentNumber")?;
    let due_date = string_field(row, "DueDate");
    let overdue = due_date
        .as_deref()
        .and_then(|date| date.parse::<NaiveDate>().ok())
        .is_some_and(|date| date < today);

    Some(WatchEntry {
        external_id: invoice_external_id(&document_number),
        kind: UNPAID_INVOICE_KIND.to_string(),
        payload: json!({
            "document_number": document_number,
            "customer_number": row.get("CustomerNumber").and_then(as_text),
            "customer_name": row.get("CustomerName").and_then(as_text),
            "invoice_date": string_field(row, "InvoiceDate"),
            "due_date": due_date,
            "currency": string_field(row, "Currency"),
            "total": row.get("Total").map(|v| amount_value(coerce_amount(v))),
            "balance": row.get("Balance").map(|v| amount_value(coerce_amount(v))),
            "overdue": overdue,
        }),
    })
}

/// Every declaration falling due within [`deadlines::HORIZON_DAYS`] of
/// `today`, as watch entries. Pure: no Fortnox call, so these keep arriving
/// even on a connector whose grant has lapsed.
pub fn deadline_entries(today: NaiveDate) -> Vec<WatchEntry> {
    deadlines::upcoming_deadlines(today, deadlines::HORIZON_DAYS)
        .into_iter()
        .map(|deadline| WatchEntry {
            external_id: deadline_external_id(&deadline),
            kind: TAX_DEADLINE_KIND.to_string(),
            payload: deadline.payload(),
        })
        .collect()
}

/// A field Fortnox may answer as a string or as a number.
fn string_field(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(as_text)
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if text.trim().is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().expect("a date")
    }

    fn invoice(number: &str, due: &str, balance: f64) -> Value {
        json!({
            "DocumentNumber": number,
            "CustomerNumber": "C-1",
            "CustomerName": "Acme AB",
            "InvoiceDate": "2026-08-01",
            "DueDate": due,
            "Currency": "SEK",
            "Total": 12_500.0,
            "Balance": balance,
        })
    }

    #[test]
    fn an_invoice_id_is_fortnox_invoice_colon_the_document_number() {
        let entry = invoice_entry(&invoice("1042", "2026-09-30", 12_500.0), date("2026-09-24"))
            .expect("an entry");
        assert_eq!(entry.external_id, "fortnox-invoice:1042");
        assert_eq!(entry.kind, UNPAID_INVOICE_KIND);
    }

    #[test]
    fn a_past_due_date_is_overdue_and_a_future_one_is_not() {
        let today = date("2026-09-24");
        let past = invoice_entry(&invoice("1", "2026-09-23", 100.0), today).unwrap();
        let exactly_today = invoice_entry(&invoice("2", "2026-09-24", 100.0), today).unwrap();
        let future = invoice_entry(&invoice("3", "2026-10-31", 100.0), today).unwrap();

        assert_eq!(past.payload["overdue"], true);
        assert_eq!(
            exactly_today.payload["overdue"], false,
            "an invoice is not late on the day it is due"
        );
        assert_eq!(future.payload["overdue"], false);
    }

    #[test]
    fn an_invoice_with_no_document_number_is_skipped_rather_than_given_an_unstable_id() {
        let row = json!({ "CustomerName": "Acme AB", "Balance": 10.0 });
        assert!(invoice_entry(&row, date("2026-09-24")).is_none());
    }

    #[test]
    fn a_document_number_may_arrive_as_a_json_number() {
        let row = json!({ "DocumentNumber": 1042, "DueDate": "2026-10-01" });
        let entry = invoice_entry(&row, date("2026-09-24")).unwrap();
        assert_eq!(entry.external_id, "fortnox-invoice:1042");
    }

    /// The idempotency property, at the level this module controls: the same
    /// row and the same day give the same bytes, so the daemon sees an update
    /// with nothing in it and leaves the event closed.
    #[test]
    fn the_same_invoice_on_the_same_day_serialises_identically() {
        let today = date("2026-09-24");
        let row = invoice("1042", "2026-09-30", 12_500.0);
        let once = invoice_entry(&row, today).unwrap();
        let again = invoice_entry(&row, today).unwrap();
        assert_eq!(once, again);
        assert!(
            !once.payload.to_string().contains("days"),
            "a countdown would re-open this invoice for triage every day: {}",
            once.payload
        );
    }

    /// A string amount and the equal numeric amount must not look like a
    /// change. Fortnox is inconsistent about which it sends.
    #[test]
    fn a_balance_of_1000_and_a_balance_of_the_string_1000_00_are_the_same_payload() {
        let today = date("2026-09-24");
        let numeric = json!({ "DocumentNumber": "7", "Balance": 1000 });
        let text = json!({ "DocumentNumber": "7", "Balance": "1000.00" });
        assert_eq!(
            invoice_entry(&numeric, today).unwrap().payload,
            invoice_entry(&text, today).unwrap().payload
        );
    }

    /// And the change that *should* be seen: a part payment moves the balance,
    /// the payload differs, and the daemon re-opens the row for triage.
    #[test]
    fn a_changed_balance_changes_the_payload() {
        let today = date("2026-09-24");
        let before = invoice_entry(&invoice("1042", "2026-09-30", 12_500.0), today).unwrap();
        let after = invoice_entry(&invoice("1042", "2026-09-30", 2_500.0), today).unwrap();
        assert_eq!(
            before.external_id, after.external_id,
            "it is the same invoice, so the daemon must update rather than add"
        );
        assert_ne!(before.payload, after.payload);
        assert_eq!(after.payload["balance"], 2500.0);
    }

    #[test]
    fn deadline_ids_are_tax_deadline_colon_kind_colon_period() {
        let entries = deadline_entries(date("2026-09-24"));
        assert!(!entries.is_empty(), "the 12th of October is inside 45 days");
        for entry in &entries {
            assert_eq!(entry.kind, TAX_DEADLINE_KIND);
            let parts: Vec<&str> = entry.external_id.split(':').collect();
            assert_eq!(parts.len(), 3, "{}", entry.external_id);
            assert_eq!(parts[0], "tax-deadline");
            assert_eq!(
                entry.payload["note"],
                deadlines::REMINDER_NOTE,
                "{}",
                entry.payload
            );
        }
    }

    #[test]
    fn deadline_ids_are_unique_within_one_poll() {
        let entries = deadline_entries(date("2026-09-24"));
        let ids: std::collections::BTreeSet<&str> = entries
            .iter()
            .map(|entry| entry.external_id.as_str())
            .collect();
        assert_eq!(
            ids.len(),
            entries.len(),
            "two rows sharing an id means one of them is silently dropped"
        );
    }
}
