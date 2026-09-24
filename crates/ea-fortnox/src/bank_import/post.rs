//! Posting approved rows to Fortnox, once each.
//!
//! Upstream this is `bank-import/post.ts`.
//!
//! # The four safeguards
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
//! 3. **The rows must fall inside that year.** See below: without this,
//!    safeguard 4 is scoped to the wrong year and silently does nothing.
//! 4. **Idempotency by marker.** Every voucher this tool has ever written
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
//! # Why safeguard 3 exists
//!
//! The marker scan is **scoped to one financial year**: `get_all("vouchers",
//! …, &[("financialyear", id)])`. That is upstream's query and it is the right
//! one — scanning every year of a company's history to post one month of bank
//! lines would be gratuitous. But it means the idempotency check only sees
//! vouchers *in the year that was looked up*, and nothing else in the run ties
//! that year to the dates on the rows.
//!
//! Left alone, that is a duplicate-voucher bug rather than a tidiness
//! question. [`FY_START`]/[`FY_END`] name the year that ended 2026-06-30, and
//! a closed year is not deleted from Fortnox — `financialyears` still lists
//! it, so [`covering_year`] still finds it and the run does **not** abort.
//! It then scans last year's vouchers for markers, finds none matching rows
//! dated this year, and posts every one of them. Run it twice and every line
//! is booked twice: the exact failure the pagination work exists to prevent,
//! reached by a different road.
//!
//! So every row's `Datum` is checked against the bounds of the year actually
//! being scanned, and a row outside them aborts the run with
//! [`RowOutsideFinancialYear`] naming the row's date and the year's bounds.
//! The guard is on the *relationship* between the rows and the year, not on
//! the ability to override a constant: [`PostOptions::fiscal_year`] is the
//! escape hatch, and this is what makes forgetting to use it loud.
//!
//! A row whose `Datum` is not a `YYYY-MM-DD` date at all is left to
//! [`forslag_to_payload`], which refuses it as a per-row failure — it has no
//! *when* to compare, and it can never reach Fortnox either way.
//!
//! # Divergences from the TypeScript
//!
//! * **The financial year is overridable, and the rows are checked against
//!   it.** Upstream hardcodes `2025-07-01`/`2026-06-30` as module constants
//!   and never looks at the row dates, which is the duplicate-voucher bug
//!   described above. The constants stay as the default so behaviour is
//!   unchanged for the year they describe;
//!   [`PostOptions::fiscal_year`] names a different one, and safeguard 3
//!   makes the mismatch an error rather than a silent double posting. This is
//!   an addition, not a translation.
//! * **Errors are [`anyhow::Error`]**, with [`FiscalYearMissing`] and
//!   [`RowOutsideFinancialYear`] recoverable by
//!   [`anyhow::Error::downcast_ref`] — upstream's `instanceof` check, in the
//!   form Rust has.

use std::collections::HashSet;
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

/// A row is dated outside the financial year whose vouchers were scanned.
///
/// Posting it anyway would book a voucher into a year whose existing vouchers
/// were never examined for its `[imp:…]` marker, so a second run would book it
/// again. The fix is almost always to name the right year with
/// [`PostOptions::fiscal_year`].
#[derive(Debug, thiserror::Error)]
#[error(
    "row {rad_id} is dated {datum}, outside the financial year {from} → {to} \
     that was checked for already-booked rows. Posting it would scan the wrong \
     year for idempotency markers and could book it twice. Name the right \
     financial year (PostOptions::fiscal_year), or split the rows by year."
)]
pub struct RowOutsideFinancialYear {
    /// The offending row's `Rad_id`.
    pub rad_id: String,
    /// The offending row's `Datum`.
    pub datum: String,
    /// First day of the year that was scanned.
    pub from: String,
    /// Last day of the year that was scanned.
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

    async fn get_all(
        &self,
        path: &str,
        list_key: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<Value>> {
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
    /// [`FY_START`]/[`FY_END`], which name the year that ended 2026-06-30 —
    /// so for current work this has to be set. Leaving it unset for rows
    /// dated outside that year is an error ([`RowOutsideFinancialYear`]), not
    /// a silent second booking; see the module docs. This is an addition to
    /// the TypeScript, which hardcodes the pair.
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

/// `Some(s)` unless `s` is empty — a Fortnox field that was absent or blank.
fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Whether `s` is a `YYYY-MM-DD` date, which is what makes the lexicographic
/// comparison against a financial year's bounds mean what it looks like it
/// means. Anything else is not a date this module will compare.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// Post the approved rows, skipping the ones already booked.
///
/// # UNREACHABLE AS SHIPPED — and the most likely place to break the gate
///
/// **Nothing calls this function outside its own tests.** It is not an MCP
/// tool, not a binary, not reached by the daemon or the CLI; the whole
/// `bank_import` module is translated, tested and dormant, waiting for the
/// task that wires an import flow up. That is deliberate: it is kept so the
/// translation does not have to be redone, and it is left disconnected
/// because nobody has designed the human half of a bank import yet.
///
/// **If you are the one wiring it up, read this first.** `run_post` holds its
/// own [`PostClient`] and POSTs vouchers through it directly. Every other
/// write in this system reaches Fortnox only after `propose_action` has
/// queued the action and a human has approved it — that is what
/// `Policy::decide` governs, and it governs *tools*, not functions. A caller
/// that hands `run_post` a live client bypasses the gate completely: real
/// vouchers land in a real company's books with no proposal, no approval and
/// no queue entry, and the only trace is in Fortnox.
///
/// So the future caller must be a *proposal*: turn the rows into an action
/// that goes through `propose_action`, let the approved action drive the
/// posting, and do not give this function a posting client from inside a
/// session. This project has already shipped one gate bypass and had to fix
/// it. This is where the next one would enter.
///
/// # Errors
///
/// [`FiscalYearMissing`] when no financial year covers the target span,
/// [`RowOutsideFinancialYear`] when a row is dated outside the year whose
/// vouchers would be scanned, and whatever the client returns for the two
/// reads. A failure to post an
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

    // 2. Every row must fall inside the year whose vouchers are about to be
    //    scanned. Checked against the bounds Fortnox reports for the year that
    //    was actually found — which may be wider than the requested span —
    //    falling back to the requested span for a field Fortnox omitted. See
    //    the module docs: without this the marker scan silently covers the
    //    wrong year and every row is posted again on the next run.
    let year_from = non_empty(date_field(year, "FromDate")).unwrap_or_else(|| fy_from.to_string());
    let year_to = non_empty(date_field(year, "ToDate")).unwrap_or_else(|| fy_to.to_string());
    for row in rows {
        let datum = row.datum.trim();
        // A malformed date has no *when* to compare; `forslag_to_payload`
        // refuses it a few lines down as a per-row failure, so it can never
        // be posted regardless.
        if !is_iso_date(datum) {
            continue;
        }
        if datum < year_from.as_str() || datum > year_to.as_str() {
            return Err(RowOutsideFinancialYear {
                rad_id: row.rad_id.clone(),
                datum: datum.to_string(),
                from: year_from,
                to: year_to,
            }
            .into());
        }
    }

    // 3. Every marker already in the year, through the paginating call. See
    //    the module docs for why a plain `get` would be a duplicate-posting
    //    bug rather than a performance question.
    let year_id = year.get("Id").map(render_id);
    let query: Vec<(&str, &str)> = match year_id.as_deref() {
        Some(id) => vec![("financialyear", id)],
        None => Vec::new(),
    };
    let vouchers = client.get_all("vouchers", "Vouchers", &query).await?;
    // A set, not a list: upstream uses a `Set` and a company-year of vouchers
    // against a sheet of rows is a quadratic scan otherwise.
    let booked: HashSet<String> = vouchers
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

    /// One recorded `get_all`: path, list key, and the query it was given.
    type GetAllCall = (String, String, Vec<(String, String)>);

    #[derive(Default)]
    struct Calls {
        get: Vec<String>,
        get_all: Vec<GetAllCall>,
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
        fn get_all_calls(&self) -> Vec<GetAllCall> {
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
        let err = run_post(
            &client,
            &[row("a")],
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
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

    // ---- the row-inside-the-year guard (safeguard 3) ------------------------

    /// The duplicate-voucher path, end to end.
    ///
    /// `FY_START`/`FY_END` name the year that ended 2026-06-30. A closed year
    /// is not deleted from Fortnox, so `covering_year` finds it and the run
    /// does **not** abort — it scopes the marker scan to *that* year's id
    /// while posting rows dated *this* year, whose vouchers it never looked
    /// at. Every row is posted again on every run.
    #[tokio::test]
    async fn rows_dated_after_the_default_year_abort_instead_of_double_posting() {
        // Exactly what Fortnox answers today: last year, still listed,
        // alongside the year the rows actually belong to.
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "Id": 7, "FromDate": "2025-07-01", "ToDate": "2026-06-30" },
            { "Id": 8, "FromDate": "2026-07-01", "ToDate": "2027-06-30" },
        ] }));
        let this_year = ForslagRow {
            datum: "2026-09-01".to_string(),
            ..row("a")
        };

        let err = run_post(
            &client,
            std::slice::from_ref(&this_year),
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        let outside = err
            .downcast_ref::<RowOutsideFinancialYear>()
            .unwrap_or_else(|| panic!("expected RowOutsideFinancialYear, got {err:#}"));
        // The message has to carry both halves of the mismatch, or the reader
        // cannot tell which of the two dates is the one they got wrong.
        assert_eq!(outside.datum, "2026-09-01");
        assert_eq!(outside.from, "2025-07-01");
        assert_eq!(outside.to, "2026-06-30");
        let rendered = format!("{err}");
        for needle in ["2026-09-01", "2025-07-01", "2026-06-30"] {
            assert!(rendered.contains(needle), "{rendered}");
        }

        // Nothing was posted, and the wrong year's vouchers were never even
        // scanned — the guard runs before the listing.
        assert!(client.posts().is_empty(), "nothing may be posted");
        assert!(client.get_all_calls().is_empty());

        // And with the right year named, the same rows go through — against
        // *that* year's id, which is what makes the marker scan meaningful.
        let client = Mock::new().with_years(json!({ "FinancialYears": [
            { "Id": 7, "FromDate": "2025-07-01", "ToDate": "2026-06-30" },
            { "Id": 8, "FromDate": "2026-07-01", "ToDate": "2027-06-30" },
        ] }));
        let r = run_post(
            &client,
            &[this_year],
            PostOptions {
                commit: true,
                fiscal_year: Some(("2026-07-01", "2027-06-30")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(r.posted.len(), 1);
        assert_eq!(
            client.get_all_calls()[0].2,
            vec![("financialyear".to_string(), "8".to_string())]
        );
    }

    #[tokio::test]
    async fn a_row_before_the_year_is_refused_too() {
        let client = Mock::new();
        let err = run_post(
            &client,
            &[
                row("a"),
                ForslagRow {
                    datum: "2025-06-30".to_string(),
                    ..row("b")
                },
            ],
            PostOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.downcast_ref::<RowOutsideFinancialYear>()
                .map(|e| e.rad_id.as_str()),
            Some("b")
        );
    }

    #[tokio::test]
    async fn the_bounds_come_from_the_year_fortnox_returned_not_the_requested_span() {
        // The requested span is a subset of a wider year. A row outside the
        // span but inside the year is fine: that year's vouchers *are* what
        // the marker scan covers.
        let client = Mock::new().with_years(
            json!({ "FinancialYears": [{ "Id": 9, "FromDate": "2025-01-01", "ToDate": "2026-12-31" }] }),
        );
        let r = run_post(
            &client,
            &[ForslagRow {
                datum: "2026-11-30".to_string(),
                ..row("a")
            }],
            PostOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(r.posted.len(), 1);
    }

    #[tokio::test]
    async fn the_boundary_days_of_the_year_are_inside_it() {
        for datum in ["2025-07-01", "2026-06-30"] {
            let client = Mock::new();
            let r = run_post(
                &client,
                &[ForslagRow {
                    datum: datum.to_string(),
                    ..row("a")
                }],
                PostOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{datum}: {e:#}"));
            assert_eq!(r.posted.len(), 1, "{datum}");
        }
    }

    #[tokio::test]
    async fn an_unapproved_row_outside_the_year_still_aborts_the_run() {
        // The sheet, not the row, is what is wrong: a Förslag whose rows
        // straddle a year boundary cannot be posted safely in one pass, and
        // saying so is better than posting half of it.
        let client = Mock::new();
        let err = run_post(
            &client,
            &[ForslagRow {
                datum: "2026-09-01".to_string(),
                godkann: "N".to_string(),
                ..row("a")
            }],
            PostOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<RowOutsideFinancialYear>().is_some());
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
        // Rows in the overridden year, which is the only combination that
        // makes sense: safeguard 3 refuses the rest.
        let in_year = ForslagRow {
            datum: "2026-09-01".to_string(),
            ..row("a")
        };
        assert!(run_post(
            &client,
            &[in_year],
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let posts = client.posts();
        assert_eq!(posts.len(), 2);
        assert!(posts
            .iter()
            .all(|(path, body)| path == "vouchers"
                && body.get("Voucher").is_some_and(Value::is_object)));
        assert_eq!(r.posted.len(), 2);
        assert!(r.committed);
    }

    #[tokio::test]
    async fn rows_that_are_not_approved_are_skipped() {
        let client = Mock::new();
        let rows = [
            ForslagRow {
                godkann: String::new(),
                ..row("a")
            },
            ForslagRow {
                godkann: "N".to_string(),
                ..row("b")
            },
        ];
        let r = run_post(
            &client,
            &rows,
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 2);
        assert!(
            r.skipped[0].reason.contains("Godkänn"),
            "{}",
            r.skipped[0].reason
        );
    }

    #[tokio::test]
    async fn approval_tolerates_whitespace_and_case() {
        let client = Mock::new();
        let rows = [
            ForslagRow {
                godkann: " j ".to_string(),
                ..row("a")
            },
            ForslagRow {
                godkann: "J".to_string(),
                ..row("b")
            },
        ];
        let r = run_post(
            &client,
            &rows,
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(r.posted.len(), 2, "skipped: {:?}", r.skipped);
    }

    #[tokio::test]
    async fn a_row_with_no_account_is_skipped_rather_than_failed() {
        let client = Mock::new();
        let r = run_post(
            &client,
            &[ForslagRow {
                bas_konto: String::new(),
                ..row("a")
            }],
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(
            r.skipped[0].reason.contains("BAS_konto"),
            "{}",
            r.skipped[0].reason
        );
        assert!(
            r.failures.is_empty(),
            "an unfilled row is expected, not a failure"
        );
    }

    #[tokio::test]
    async fn a_row_that_cannot_be_mapped_is_a_failure_and_the_run_continues() {
        // A date the user hand-edited into the sheet: the account is there and
        // the row is approved, so it is not a skip — it is a genuine failure.
        let client = Mock::new();
        let r = run_post(
            &client,
            &[
                ForslagRow {
                    datum: "10/08/2025".to_string(),
                    ..row("a")
                },
                row("b"),
            ],
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
        let rows = body["Voucher"]["VoucherRows"]["VoucherRow"]
            .as_array()
            .unwrap();
        assert!(rows.iter().any(|r| r["Account"] == "1932"));
    }

    // ---- idempotency -----------------------------------------------------------

    #[tokio::test]
    async fn collects_markers_through_the_paginating_call() {
        // The pinned detail: a plain `get` sees page one only, and a marker on
        // page two would be missed — re-posting a row already booked.
        let client = Mock::new();
        run_post(
            &client,
            &[row("a")],
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
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
        assert_eq!(
            client.get_all_calls()[0].2,
            vec![("financialyear".to_string(), "7".to_string())]
        );
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(client.posts().is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(
            r.skipped[0].reason.contains("redan bokförd"),
            "{}",
            r.skipped[0].reason
        );
    }

    #[tokio::test]
    async fn a_row_whose_marker_is_absent_is_booked() {
        let client = Mock::new().with_vouchers(vec![
            json!({ "Description": format!("Annan {}", imp_marker("zzz999")) }),
        ]);
        let r = run_post(
            &client,
            &[row("aaa111")],
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            client.posts().is_empty(),
            "an uppercase marker must still match"
        );
        assert_eq!(r.skipped.len(), 1);

        // Direction 2: an uppercase Rad_id in the sheet, against a lowercase
        // marker. `rad_id` only ever emits lowercase, but the Förslag sheet is
        // a file a human edits, and upstream lowercases the row's own id for
        // exactly this reason. Without it the row is posted a second time.
        let client = Mock::new().with_vouchers(vec![json!({ "Description": "X [imp:aaa111]" })]);
        let r = run_post(
            &client,
            &[row("AAA111")],
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
            PostOptions {
                commit: true,
                ..Default::default()
            },
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
        let r = run_post(
            &client,
            &[],
            PostOptions {
                commit: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            r,
            PostResult {
                committed: true,
                ..PostResult::default()
            }
        );
        assert!(client.posts().is_empty());
    }
}
