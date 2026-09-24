//! Posting approved rows to Fortnox, once each.
//!
//! Upstream this is `bank-import/post.ts`.
//!
//! # The three safeguards
//!
//! 1. **Dry run by default.** `commit: false` builds every payload and reports
//!    what it would send, and sends nothing. Nothing about the code path
//!    differs except the final call, so the dry run is a rehearsal of the real
//!    thing rather than a separate approximation of it.
//! 2. **The financial year must already exist.** Fortnox will not take a
//!    voucher dated outside an open year, and creating one is a decision with
//!    consequences this tool has no business making. If the year is missing
//!    the run aborts with [`FiscalYearMissing`] before a single voucher is
//!    built.
//! 3. **Idempotency by marker.** Every voucher this tool has ever written
//!    carries `[imp:<Rad_id>]` in its description. Before posting anything the
//!    run collects those markers from the vouchers already in the year and
//!    skips any row whose marker is there, so `post --commit` twice books each
//!    line once.
//!
//! The markers are fetched through the **paginating** `get_all`, not a plain
//! `get`. A plain `get` returns page one — at most 500 vouchers — and a marker
//! on page two would be invisible, so the row would be posted a second time.
//! That is the failure the pagination exists to prevent, and
//! `collects_markers_through_the_paginating_call` pins it.
//!
//! # Divergences from the TypeScript
//!
//! * **The financial year is overridable.** Upstream hardcodes
//!   `2025-07-01`/`2026-06-30` as module constants, which means that on the
//!   first day of the next financial year `post` aborts saying the year does
//!   not exist — when what has actually happened is that the constants name
//!   last year. The constants stay as the default so the behaviour is unchanged
//!   for the year they describe, but [`PostOptions::fiscal_year`] lets a caller
//!   name a different one. This is an addition, not a translation.
//! * **Errors are [`anyhow::Error`]**, with [`FiscalYearMissing`] recoverable by
//!   [`anyhow::Error::downcast_ref`] — upstream's `instanceof` check, in the
//!   form Rust has.

use std::future::Future;

use anyhow::Result;
use serde_json::Value;

use super::types::ForslagRow;
use super::voucher::{extract_markers, forslag_to_payload, VoucherOptions};
use crate::client::FortnoxClient;

/// First day of the financial year upstream targets.
pub const FY_START: &str = "2025-07-01";
/// Last day of the financial year upstream targets.
pub const FY_END: &str = "2026-06-30";

/// The financial year covering the target span does not exist in Fortnox.
///
/// Creating it is deliberately out of scope: a financial year is a structural
/// decision about the company's books, and a bank-import tool inventing one is
/// how a year gets opened with the wrong dates and every later report is
/// quietly wrong.
#[derive(Debug, thiserror::Error)]
#[error(
    "no financial year covers {from} → {to}. Create it in Fortnox first — \
     opening a financial year is outside this tool's remit."
)]
pub struct FiscalYearMissing {
    /// The first day that had to be covered.
    pub from: String,
    /// The last day that had to be covered.
    pub to: String,
}

/// The slice of the Fortnox client this module needs.
///
/// A trait rather than a concrete [`FortnoxClient`] so the tests can drive
/// every branch — including a failing `post` — without a network, a token, or a
/// mock HTTP server. No test in this crate reaches Fortnox.
pub trait PostClient {
    /// `GET {path}`, decoded as JSON.
    fn get(&self, path: &str) -> impl Future<Output = Result<Value>> + Send;

    /// Every row of a paginated list endpoint, across **all** pages.
    fn get_all(
        &self,
        path: &str,
        list_key: &str,
        query: &[(&str, &str)],
    ) -> impl Future<Output = Result<Vec<Value>>> + Send;

    /// `POST {path}` with a JSON body.
    fn post(&self, path: &str, body: &Value) -> impl Future<Output = Result<Value>> + Send;
}

impl PostClient for FortnoxClient {
    async fn get(&self, path: &str) -> Result<Value> {
        Ok(FortnoxClient::get(self, path, &[]).await?)
    }

    async fn get_all(&self, path: &str, list_key: &str, query: &[(&str, &str)]) -> Result<Vec<Value>> {
        Ok(FortnoxClient::get_all(self, path, list_key, query).await?)
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        Ok(FortnoxClient::post(self, path, body, &[]).await?)
    }
}

/// How to run a posting pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct PostOptions<'a> {
    /// `false` rehearses; `true` actually posts.
    pub commit: bool,
    /// The bank account, defaulting to
    /// [`DEFAULT_BANK_ACCOUNT`](super::code::DEFAULT_BANK_ACCOUNT).
    pub bank_account: Option<&'a str>,
    /// The voucher series, defaulting to
    /// [`DEFAULT_SERIES`](super::voucher::DEFAULT_SERIES).
    pub series: Option<&'a str>,
    /// The financial year that must exist, as `(from, to)`. Defaults to
    /// [`FY_START`]/[`FY_END`]. See the module docs: this is an addition to the
    /// TypeScript, which hardcodes the pair.
    pub fiscal_year: Option<(&'a str, &'a str)>,
}

/// A row that was not posted, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipEntry {
    /// The row's idempotency key.
    pub rad_id: String,
    /// Why it was skipped, in Swedish — this text is shown to the user.
    pub reason: String,
}

/// A row that was posted, or would have been.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedEntry {
    /// The row's idempotency key.
    pub rad_id: String,
    /// The exact body that was sent, or that a `--commit` would send.
    pub payload: Value,
}

/// A row whose posting failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEntry {
    /// The row's idempotency key.
    pub rad_id: String,
    /// The failure, rendered for a human.
    pub error: String,
}

/// What a posting pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostResult {
    /// Rows posted, or that a `--commit` would post.
    pub posted: Vec<PostedEntry>,
    /// Rows deliberately not posted.
    pub skipped: Vec<SkipEntry>,
    /// Rows that could not be posted.
    pub failures: Vec<FailureEntry>,
    /// Whether this pass actually wrote anything.
    pub committed: bool,
}

/// The list of financial years, whichever of the two shapes Fortnox returned.
///
/// Fortnox answers `financialyears` with `FinancialYears` (a list) and a
/// single-year lookup with `FinancialYear` (an object). Upstream accepts both
/// and also tolerates a single object under the plural key; all three are
/// handled here because the cost of being wrong is aborting a run that should
/// have proceeded.
fn financial_years(response: &Value) -> Vec<&Value> {
    let node = response
        .get("FinancialYears")
        .or_else(|| response.get("FinancialYear"));
    match node {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// The year covering `from`..`to`, if Fortnox has one.
///
/// Dates are compared as strings. `YYYY-MM-DD` sorts lexicographically exactly
/// as it sorts chronologically, which is the whole reason the format is written
/// that way, and it is what upstream does.
fn covering_year<'a>(response: &'a Value, from: &str, to: &str) -> Option<&'a Value> {
    financial_years(response).into_iter().find(|fy| {
        let fy_from = date_field(fy, "FromDate");
        let fy_to = date_field(fy, "ToDate");
        fy_from.as_str() <= from && fy_to.as_str() >= to
    })
}

/// A date field, truncated to its first ten characters as upstream's
/// `.slice(0, 10)` does — Fortnox sometimes returns a full timestamp.
fn date_field(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(10)
        .collect()
}

/// Post the approved rows, skipping the ones already booked.
///
/// # Errors
///
/// [`FiscalYearMissing`] when no financial year covers the target span, and
/// whatever the client returns for the two reads. A failure to post an
/// *individual* row is not an error: it lands in
/// [`PostResult::failures`] and the run continues, because stopping at the
/// first bad row would leave the ledger half-written with no summary of where
/// it stopped.
pub async fn run_post<C: PostClient>(
    client: &C,
    rows: &[ForslagRow],
    opts: PostOptions<'_>,
) -> Result<PostResult> {
    let (fy_from, fy_to) = opts.fiscal_year.unwrap_or((FY_START, FY_END));

    // 1. The year must exist. Checked before anything is built, so a missing
    //    year costs one request and writes nothing.
    let years = client.get("financialyears").await?;
    let Some(year) = covering_year(&years, fy_from, fy_to) else {
        return Err(FiscalYearMissing {
            from: fy_from.to_string(),
            to: fy_to.to_string(),
        }
        .into());
    };

    // 2. Every marker already in the year, through the paginating call. See
    //    the module docs for why a plain `get` would be a duplicate-posting
    //    bug rather than a performance question.
    let year_id = year.get("Id").map(render_id);
    let query: Vec<(&str, &str)> = match year_id.as_deref() {
        Some(id) => vec![("financialyear", id)],
        None => Vec::new(),
    };
    let vouchers = client.get_all("vouchers", "Vouchers", &query).await?;
    let booked: Vec<String> = vouchers
        .iter()
        .flat_map(|v| extract_markers(v.get("Description").and_then(Value::as_str)))
        .collect();

    let mut result = PostResult {
        committed: opts.commit,
        ..PostResult::default()
    };

    for row in rows {
        let skip = |reason: &str| SkipEntry {
            rad_id: row.rad_id.clone(),
            reason: reason.to_string(),
        };

        if !row.godkann.trim().eq_ignore_ascii_case("J") {
            result.skipped.push(skip("ej godkänd (Godkänn ≠ J)"));
            continue;
        }
        if row.bas_konto.trim().is_empty() {
            result.skipped.push(skip("BAS_konto saknas"));
            continue;
        }
        if booked.contains(&row.rad_id.to_lowercase()) {
            result.skipped.push(skip("redan bokförd (idempotens)"));
            continue;
        }

        let payload = match forslag_to_payload(
            row,
            VoucherOptions {
                bank_account: opts.bank_account,
                series: opts.series,
            },
        ) {
            Ok(payload) => payload,
            Err(err) => {
                result.failures.push(FailureEntry {
                    rad_id: row.rad_id.clone(),
                    error: format!("{err:#}"),
                });
                continue;
            }
        };

        if !opts.commit {
            result.posted.push(PostedEntry {
                rad_id: row.rad_id.clone(),
                payload,
            });
            continue;
        }

        match client.post("vouchers", &payload).await {
            Ok(_) => result.posted.push(PostedEntry {
                rad_id: row.rad_id.clone(),
                payload,
            }),
            Err(err) => result.failures.push(FailureEntry {
                rad_id: row.rad_id.clone(),
                error: format!("{err:#}"),
            }),
        }
    }

    Ok(result)
}

/// A financial year's `Id` as a query value. Fortnox has returned it as both a
/// JSON number and a JSON string; `Value::to_string` would quote the string
/// one, so the two cases are separated.
fn render_id(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank_import::types::{Confidence, Direction};
    use crate::bank_import::voucher::imp_marker;
    use rust_decimal::Decimal;
    use serde_json::json;
    use std::sync::Mutex;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn row(rad_id: &str) -> ForslagRow {
        ForslagRow {
            rad_id: rad_id.to_string(),
            datum: "2025-08-10".to_string(),
            text: "Köp".to_string(),
            belopp: dec("-1250"),
            riktning: Direction::Utgift,
            bas_konto: "5410".to_string(),
            momssats: Some(dec("25")),
            moms_kr: None,
            konto_namn: String::new(),
            konfidens: Confidence::Hog,
            motivering: String::new(),
            matchad_kvitto: String::new(),
            godkann: "J".to_string(),
        }
    }

    fn fy_ok() -> Value {
        json!({ "FinancialYears": [{ "Id": 7, "FromDate": "2025-07-01", "ToDate": "2026-06-30" }] })
    }

    #[derive(Default)]
    struct Calls {
        get: Vec<String>,
        get_all: Vec<(String, String, Vec<(String, String)>)>,
        post: Vec<(String, Value)>,
    }

    struct Mock {
        financial_years: Value,
        voucher_rows: Vec<Value>,
        /// One entry consumed per `post`; `Err` makes that call fail. An empty
        /// queue succeeds.
        post_outcomes: Mutex<Vec<Result<(), String>>>,
        calls: Mutex<Calls>,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                financial_years: fy_ok(),
                voucher_rows: Vec::new(),
                post_outcomes: Mutex::new(Vec::new()),
                calls: Mutex::new(Calls::default()),
            }
        }
        fn with_years(mut self, years: Value) -> Self {
            self.financial_years = years;
            self
        }
        fn with_vouchers(mut self, rows: Vec<Value>) -> Self {
            self.voucher_rows = rows;
            self
        }
        fn with_post_outcomes(self, outcomes: Vec<Result<(), String>>) -> Self {
            // Reversed so the vector can be popped from the back in order.
            *self.post_outcomes.lock().unwrap() = outcomes.into_iter().rev().collect();
            self
        }
        fn posts(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().post.clone()
        }
        fn get_all_calls(&self) -> Vec<(String, String, Vec<(String, String)>)> {
            self.calls.lock().unwrap().get_all.clone()
        }
    }

    impl PostClient for Mock {
        async fn get(&self, path: &str) -> Result<Value> {
            self.calls.lock().unwrap().get.push(path.to_string());
            Ok(if path == "financialyears" {
                self.financial_years.clone()
            } else {
                json!({})
            })
        }

        async fn get_all(
            &self,
            path: &str,
            list_key: &str,
            query: &[(&str, &str)],
        ) -> Result<Vec<Value>> {
            self.calls.lock().unwrap().get_all.push((
                path.to_string(),
                list_key.to_string(),
                query
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
            Ok(if path == "vouchers" {
                self.voucher_rows.clone()
            } else {
                Vec::new()
            })
        }

        async fn post(&self, path: &str, body: &Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .post
                .push((path.to_string(), body.clone()));
            match self.post_outcomes.lock().unwrap().pop() {
                Some(Err(message)) => Err(anyhow::anyhow!(message)),
                _ => Ok(json!({ "Voucher": { "VoucherNumber": 1, "VoucherSeries": "A" } })),
            }
        }
    }

    // ---- the fiscal-year guard ---------------------------------------------

    #[tokio::test]
    async fn a_missing_fiscal_year_aborts_before_anything_is_posted() {
        let client = Mock::new().with_years(
            json!({ "FinancialYears": [{ "Id": 1, "FromDate": "2024-07-01", "ToDate": "2025-06-30" }] }),
        );
        let err = run_post(&client, &[row("a")], PostOptions { commit: true, ..Default::default() })
            .await
            .unwrap_err();
        assert!(
            err.downcast_ref::<FiscalYearMissing>().is_some(),
            "expected FiscalYearMissing, got {err:#}"
        );
        assert!(client.posts().is_empty(), "nothing may be posted");
        // And the voucher listing is never even fetched.
        assert!(client.get_all_calls().is_empty());
    }

    #[tokio::test]
    async fn a_year_that_merely_overlaps_is_not_enough() {
        // It has to *cover* the span: a year ending mid-span would take some
        // vouchers and reject the rest, half-writing the ledger.
        let client = Mock::new().with_years(
            json!({ "FinancialYears": [{ "Id": 1, "FromDate": "2025-07-01", "ToDate": "2026-03-31" }] }),
        );
        assert!(run_post(&client, &[row("a")], PostOptions::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_wider_year_is_accepted() {
        let client = Mock::new().with_years(
            json!({ "FinancialYears": [{ "Id": 9, "FromDate": "2025-01-01", "ToDate": "2026-12-31" }] }),
        );
        assert!(run_post(&client, &[row("a")], PostOptions::default())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn the_singular_key_and_a_bare_object_are_both_accepted() {
        for years in [
            json!({ "FinancialYear": { "Id": 7, "FromDate": "2025-07-01", "ToDate": "2026-06-30" } }),
            json!({ "FinancialYears": { "Id": 7, "FromDate": "2025-07-01", "ToDate": "2026-06-30" } }),
        ] {
            let client = Mock::new().with_years(years.clone());
            assert!(
                run_post(&client, &[row("a")], PostOptions::default())
                    .await
                    .is_ok(),
                "shape {years}"
            );
        }
    }

    #[tokio::test]
    async fn a_timestamped_date_is_truncated_to_its_day() {
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "Id": 7, "FromDate": "2025-07-01T00:00:00", "ToDate": "2026-06-30T00:00:00" }
        ]}));
        assert!(run_post(&client, &[row("a")], PostOptions::default())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn the_fiscal_year_is_overridable() {
        // ADDITION over the TypeScript, which hardcodes the pair and so starts
        // refusing to run on the first day of the next financial year.
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "Id": 8, "FromDate": "2026-07-01", "ToDate": "2027-06-30" }
        ]}));
        assert!(run_post(&client, &[row("a")], PostOptions::default())
            .await
            .is_err());
        assert!(run_post(
            &client,
            &[row("a")],
            PostOptions {
                fiscal_year: Some(("2026-07-01", "2027-06-30")),
                ..Default::default()
            }
        )
        .await
        .is_ok());
    }

    // ---- dry run -------------------------------------------------------------

    #[tokio::test]
    async fn a_dry_run_posts_nothing_and_lists_what_it_would_post() {
        let client = Mock::new();
        let r = run_post(&client, &[row("a")], PostOptions::default())
            .await
            .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.posted.len(), 1);
        assert!(!r.committed);
        // The rehearsal carries the real body, not a summary of it.
        assert_eq!(r.posted[0].payload["Voucher"]["VoucherSeries"], "A");
    }

    #[tokio::test]
    async fn a_dry_run_and_a_commit_build_the_same_payload() {
        let dry = run_post(&Mock::new(), &[row("a")], PostOptions::default())
            .await
            .unwrap();
        let wet = run_post(
            &Mock::new(),
            &[row("a")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(dry.posted[0].payload, wet.posted[0].payload);
    }

    // ---- commit ---------------------------------------------------------------

    #[tokio::test]
    async fn a_commit_posts_once_per_approved_row() {
        let client = Mock::new();
        let r = run_post(
            &client,
            &[row("a"), row("b")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        let posts = client.posts();
        assert_eq!(posts.len(), 2);
        assert!(posts.iter().all(|(path, body)| path == "vouchers"
            && body.get("Voucher").is_some_and(Value::is_object)));
        assert_eq!(r.posted.len(), 2);
        assert!(r.committed);
    }

    #[tokio::test]
    async fn rows_that_are_not_approved_are_skipped() {
        let client = Mock::new();
        let rows = [
            ForslagRow { godkann: String::new(), ..row("a") },
            ForslagRow { godkann: "N".to_string(), ..row("b") },
        ];
        let r = run_post(&client, &rows, PostOptions { commit: true, ..Default::default() })
            .await
            .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 2);
        assert!(r.skipped[0].reason.contains("Godkänn"), "{}", r.skipped[0].reason);
    }

    #[tokio::test]
    async fn approval_tolerates_whitespace_and_case() {
        let client = Mock::new();
        let rows = [
            ForslagRow { godkann: " j ".to_string(), ..row("a") },
            ForslagRow { godkann: "J".to_string(), ..row("b") },
        ];
        let r = run_post(&client, &rows, PostOptions { commit: true, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(r.posted.len(), 2, "skipped: {:?}", r.skipped);
    }

    #[tokio::test]
    async fn a_row_with_no_account_is_skipped_rather_than_failed() {
        let client = Mock::new();
        let r = run_post(
            &client,
            &[ForslagRow { bas_konto: String::new(), ..row("a") }],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].reason.contains("BAS_konto"), "{}", r.skipped[0].reason);
        assert!(r.failures.is_empty(), "an unfilled row is expected, not a failure");
    }

    #[tokio::test]
    async fn a_row_that_cannot_be_mapped_is_a_failure_and_the_run_continues() {
        // A date the user hand-edited into the sheet: the account is there and
        // the row is approved, so it is not a skip — it is a genuine failure.
        let client = Mock::new();
        let r = run_post(
            &client,
            &[
                ForslagRow { datum: "10/08/2025".to_string(), ..row("a") },
                row("b"),
            ],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].rad_id, "a");
        assert_eq!(r.posted.len(), 1);
        assert_eq!(r.posted[0].rad_id, "b");
        assert_eq!(client.posts().len(), 1, "the bad row must not be sent");
    }

    #[tokio::test]
    async fn a_failed_post_is_summarised_and_the_run_continues() {
        let client = Mock::new().with_post_outcomes(vec![Err("boom".to_string()), Ok(())]);
        let r = run_post(
            &client,
            &[row("a"), row("b")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].rad_id, "a");
        assert!(r.failures[0].error.contains("boom"));
        assert_eq!(r.posted.len(), 1);
        assert_eq!(r.posted[0].rad_id, "b");
    }

    #[tokio::test]
    async fn the_bank_account_and_series_reach_the_payload() {
        let client = Mock::new();
        run_post(
            &client,
            &[row("a")],
            PostOptions {
                commit: true,
                bank_account: Some("1932"),
                series: Some("C"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (_, body) = &client.posts()[0];
        assert_eq!(body["Voucher"]["VoucherSeries"], "C");
        let rows = body["Voucher"]["VoucherRows"]["VoucherRow"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["Account"] == "1932"));
    }

    // ---- idempotency -----------------------------------------------------------

    #[tokio::test]
    async fn collects_markers_through_the_paginating_call() {
        // The pinned detail: a plain `get` sees page one only, and a marker on
        // page two would be missed — re-posting a row already booked.
        let client = Mock::new();
        run_post(&client, &[row("a")], PostOptions { commit: true, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(
            client.get_all_calls(),
            vec![(
                "vouchers".to_string(),
                "Vouchers".to_string(),
                vec![("financialyear".to_string(), "7".to_string())]
            )]
        );
    }

    #[tokio::test]
    async fn a_string_financial_year_id_is_sent_unquoted() {
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "Id": "7", "FromDate": "2025-07-01", "ToDate": "2026-06-30" }
        ]}));
        run_post(&client, &[row("a")], PostOptions::default())
            .await
            .unwrap();
        assert_eq!(client.get_all_calls()[0].2, vec![("financialyear".to_string(), "7".to_string())]);
    }

    #[tokio::test]
    async fn a_year_with_no_id_is_queried_unscoped_rather_than_not_at_all() {
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "FromDate": "2025-07-01", "ToDate": "2026-06-30" }
        ]}));
        run_post(&client, &[row("a")], PostOptions::default())
            .await
            .unwrap();
        assert!(client.get_all_calls()[0].2.is_empty());
    }

    #[tokio::test]
    async fn a_row_already_booked_is_skipped() {
        let client = Mock::new().with_vouchers(vec![
            json!({ "Description": format!("Tidigare {}", imp_marker("aaa111")) }),
        ]);
        let r = run_post(
            &client,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].reason.contains("redan bokförd"), "{}", r.skipped[0].reason);
    }

    #[tokio::test]
    async fn a_row_whose_marker_is_absent_is_booked() {
        let client = Mock::new().with_vouchers(vec![
            json!({ "Description": format!("Annan {}", imp_marker("zzz999")) }),
        ]);
        let r = run_post(
            &client,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(client.posts().len(), 1);
        assert_eq!(r.posted.len(), 1);
    }

    #[tokio::test]
    async fn marker_matching_ignores_case_in_both_directions() {
        // Direction 1: an uppercase marker in the voucher already in Fortnox.
        // `extract_markers` lowercases what it finds.
        let client = Mock::new().with_vouchers(vec![json!({ "Description": "X [imp:AAA111]" })]);
        let r = run_post(
            &client,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty(), "an uppercase marker must still match");
        assert_eq!(r.skipped.len(), 1);

        // Direction 2: an uppercase Rad_id in the sheet, against a lowercase
        // marker. `rad_id` only ever emits lowercase, but the Förslag sheet is
        // a file a human edits, and upstream lowercases the row's own id for
        // exactly this reason. Without it the row is posted a second time.
        let client = Mock::new().with_vouchers(vec![json!({ "Description": "X [imp:aaa111]" })]);
        let r = run_post(
            &client,
            &[row("AAA111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(
            client.posts().is_empty(),
            "an uppercase Rad_id in the sheet must still match a booked marker"
        );
        assert_eq!(r.skipped.len(), 1);
    }

    #[tokio::test]
    async fn a_voucher_with_no_description_does_not_upset_the_marker_scan() {
        let client = Mock::new().with_vouchers(vec![
            json!({ "VoucherNumber": 3 }),
            json!({ "Description": Value::Null }),
            json!({ "Description": format!("Y {}", imp_marker("aaa111")) }),
        ]);
        let r = run_post(
            &client,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(r.skipped.len(), 1);
    }

    #[tokio::test]
    async fn posting_twice_over_is_idempotent_at_the_marker_level() {
        // The property the whole module exists for, exercised end to end: the
        // second pass sees the first pass's marker and posts nothing.
        let first = Mock::new();
        let r1 = run_post(
            &first,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(r1.posted.len(), 1);

        // Feed the description the first pass actually sent back in as an
        // existing voucher, exactly as Fortnox would return it.
        let description = first.posts()[0].1["Voucher"]["Description"].clone();
        let second = Mock::new().with_vouchers(vec![json!({ "Description": description })]);
        let r2 = run_post(
            &second,
            &[row("aaa111")],
            PostOptions { commit: true, ..Default::default() },
        )
        .await
        .unwrap();
        assert!(second.posts().is_empty());
        assert_eq!(r2.skipped.len(), 1);
        assert!(r2.skipped[0].reason.contains("idempotens"));
    }

    #[tokio::test]
    async fn no_rows_is_an_empty_result_rather_than_an_error() {
        let client = Mock::new();
        let r = run_post(&client, &[], PostOptions { commit: true, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(r, PostResult { committed: true, ..PostResult::default() });
        assert!(client.posts().is_empty());
    }
}
