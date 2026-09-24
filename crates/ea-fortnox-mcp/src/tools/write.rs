//! The three voucher-posting tools: `record_voucher`, `record_expense`,
//! `reconcile_payment`.
//!
//! # No `confirm`, on purpose
//!
//! Upstream's versions of these take `confirm: z.boolean().default(false)` and
//! render a preview unless it is `true`. That parameter is **absent here and
//! must stay absent**. This daemon gates a write before the tool is ever
//! reached: a session calls `propose_action`, `ea-core`'s policy decides, and
//! a queued action waits for a human tap in Telegram. A second flag inside the
//! tool would be a second confirmation in series, and two confirmations means
//! one of them is the one people stop reading — almost certainly the inner
//! one, since by the time the executor calls this tool the person has already
//! decided.
//!
//! So these post when called, every description says so in the same words
//! ([`POSTS_IMMEDIATELY`]), and
//! [`no_write_tool_takes_a_confirm_parameter`](tests::no_write_tool_takes_a_confirm_parameter)
//! reads the generated JSON schemas and fails the build if the flag ever comes
//! back — under any nesting, on any tool.
//!
//! The preview upstream rendered is not lost; it is [`super::preview`].
//!
//! # Validation happens before the network
//!
//! Every one of these builds its payload through
//! [`ea_fortnox::domain::voucher::build_payload`] first, which validates the
//! date and the balance, and calls `post` only if that succeeds. An unbalanced
//! voucher and an unsupported VAT rate therefore cost zero HTTP requests —
//! pinned by tests, because "it errors" and "it errors without having already
//! posted something" are very different properties.

use ea_fortnox::domain::posting::{build_expense_voucher, ExpenseInput};
use ea_fortnox::domain::voucher::{build_payload, BuildVoucherInput, VoucherLine};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

use super::{kronor, render, render_domain, FortnoxServer};

/// The sentence every write tool's description ends with, verbatim.
///
/// Held as a constant so the test can assert the promise is actually made:
/// `#[tool(description = …)]` needs a literal, so each description repeats
/// these words and [`tests::every_write_tool_says_it_posts_immediately`]
/// checks that none of them drifted.
pub const POSTS_IMMEDIATELY: &str = "Posts immediately. Reachable only through propose_action; \
     the approval gate decides whether it runs.";

/// The voucher series used when a caller does not name one. Upstream's
/// `z.string().default('A')`.
pub const DEFAULT_SERIES: &str = "A";

/// The bank account used when a caller does not name one. Upstream's
/// `z.string().default('1930')`.
pub const DEFAULT_BANK_ACCOUNT: &str = "1930";

/// The Fortnox resource every voucher is posted to.
const VOUCHERS: &str = "vouchers";

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// One debit/credit line.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct LineArg {
    /// BAS account number, e.g. "5410".
    pub account: String,
    /// Debit amount in kronor. Zero on a credit line. Never negative — reverse
    /// a posting by swapping the columns.
    pub debit: f64,
    /// Credit amount in kronor. Zero on a debit line.
    pub credit: f64,
    /// Optional note shown on the row in Fortnox.
    pub info: Option<String>,
}

impl LineArg {
    fn to_line(&self) -> Result<VoucherLine, String> {
        Ok(VoucherLine {
            account: self.account.clone(),
            debit: kronor("debit", self.debit)?,
            credit: kronor("credit", self.credit)?,
            info: self.info.clone(),
        })
    }
}

/// `record_voucher` and `preview_voucher` take the same arguments — which is
/// the point: what the preview renders is what the write posts.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RecordVoucherArgs {
    /// Voucher series, e.g. "A".
    pub series: String,
    /// Transaction date, YYYY-MM-DD.
    pub transaction_date: String,
    /// What the voucher is for; shown in the ledger.
    pub description: String,
    /// The rows. At least two, and the debits must equal the credits.
    pub lines: Vec<LineArg>,
}

impl RecordVoucherArgs {
    pub(crate) fn to_input(&self) -> Result<BuildVoucherInput, String> {
        let lines = self
            .lines
            .iter()
            .map(LineArg::to_line)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BuildVoucherInput {
            series: self.series.clone(),
            transaction_date: self.transaction_date.clone(),
            description: self.description.clone(),
            lines,
        })
    }
}

/// `record_expense` and `preview_expense`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExpenseArgs {
    /// VAT-inclusive total paid, in kronor.
    pub gross_amount: f64,
    /// VAT rate in percent: 25, 12, 6, or 0. Anything else is refused before
    /// anything is posted.
    pub vat_rate: u8,
    /// BAS expense account to debit, e.g. "5410".
    pub expense_account: String,
    /// Account credited: "1930" (bank) when it is already paid, "2440"
    /// (supplier debt) when it is not.
    pub payment_account: String,
    /// Transaction date, YYYY-MM-DD.
    pub transaction_date: String,
    /// What the expense was for.
    pub description: String,
    /// Voucher series. Defaults to "A".
    pub series: Option<String>,
}

impl ExpenseArgs {
    /// Upstream's ``Expense ${grossAmount} kr incl. ${vatRate}% VAT``.
    pub(crate) fn label(&self) -> String {
        let gross = self.gross_amount;
        let rate = self.vat_rate;
        format!("Expense {gross} kr incl. {rate}% VAT")
    }

    pub(crate) fn to_expense(&self) -> Result<ExpenseInput, String> {
        Ok(ExpenseInput {
            gross: kronor("grossAmount", self.gross_amount)?,
            vat_rate: self.vat_rate,
            expense_account: self.expense_account.clone(),
            payment_account: self.payment_account.clone(),
            transaction_date: self.transaction_date.clone(),
            description: self.description.clone(),
            series: self
                .series
                .clone()
                .unwrap_or_else(|| DEFAULT_SERIES.to_string()),
        })
    }

    pub(crate) fn to_input(&self) -> Result<BuildVoucherInput, String> {
        build_expense_voucher(&self.to_expense()?).map_err(render_domain)
    }
}

/// Which way the money moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A customer paid us: debit the bank, credit the receivable.
    Incoming,
    /// We paid a supplier: debit the payable, credit the bank.
    Outgoing,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Incoming => "incoming",
            Direction::Outgoing => "outgoing",
        }
    }
}

/// `reconcile_payment` and `preview_reconciliation`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReconcileArgs {
    /// Amount in kronor.
    pub amount: f64,
    /// Bank account. Defaults to "1930".
    pub bank_account: Option<String>,
    /// The other side: a receivable ("1510") for an incoming payment, a
    /// payable ("2440") for an outgoing one.
    pub counter_account: String,
    /// "incoming" = a customer paid us; "outgoing" = we paid a supplier.
    pub direction: Direction,
    /// Transaction date, YYYY-MM-DD.
    pub transaction_date: String,
    /// What the payment was for.
    pub description: String,
    /// Voucher series. Defaults to "A".
    pub series: Option<String>,
}

impl ReconcileArgs {
    /// Upstream's ``Reconcile ${direction} ${amount} kr``.
    pub(crate) fn label(&self) -> String {
        let direction = self.direction.as_str();
        let amount = self.amount;
        format!("Reconcile {direction} {amount} kr")
    }

    /// The two lines, in upstream's order: the debited account first.
    pub(crate) fn to_input(&self) -> Result<BuildVoucherInput, String> {
        let amount = kronor("amount", self.amount)?;
        let bank = self
            .bank_account
            .clone()
            .unwrap_or_else(|| DEFAULT_BANK_ACCOUNT.to_string());
        let (debited, credited) = match self.direction {
            Direction::Incoming => (bank, self.counter_account.clone()),
            Direction::Outgoing => (self.counter_account.clone(), bank),
        };
        Ok(BuildVoucherInput {
            series: self
                .series
                .clone()
                .unwrap_or_else(|| DEFAULT_SERIES.to_string()),
            transaction_date: self.transaction_date.clone(),
            description: self.description.clone(),
            lines: vec![
                VoucherLine {
                    account: debited,
                    debit: amount,
                    credit: Decimal::ZERO,
                    info: Some(self.description.clone()),
                },
                VoucherLine {
                    account: credited,
                    debit: Decimal::ZERO,
                    credit: amount,
                    info: Some(self.description.clone()),
                },
            ],
        })
    }
}

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

#[tool_router(router = write_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "Book a general manual voucher (verifikat) from explicit debit/credit \
                       lines. The lines must balance; an unbalanced set is refused without \
                       anything being sent to Fortnox. Call preview_voucher first to show \
                       the person what would be booked. Posts immediately. Reachable only \
                       through propose_action; the approval gate decides whether it runs."
    )]
    pub async fn record_voucher(
        &self,
        Parameters(args): Parameters<RecordVoucherArgs>,
    ) -> Result<String, String> {
        let payload = build_payload(&args.to_input()?).map_err(render_domain)?;
        let client = self.client()?;
        let response = client.post(VOUCHERS, &payload, &[]).await.map_err(render)?;
        let booked = VoucherRef::of(&response);
        Ok(format!(
            "Booked voucher {}{}.",
            booked.series, booked.number
        ))
    }

    #[tool(
        description = "Book a supplier expense or receipt from a VAT-inclusive gross amount: \
                       debits the net to the expense account and the input VAT to 2640, and \
                       credits the payment account for the gross. The VAT rate must be 25, \
                       12, 6 or 0; any other rate is refused without anything being sent to \
                       Fortnox. Call preview_expense first to show the person what would be \
                       booked, and attach_receipt afterwards with the series, number and \
                       year this returns. Posts immediately. Reachable only through \
                       propose_action; the approval gate decides whether it runs."
    )]
    pub async fn record_expense(
        &self,
        Parameters(args): Parameters<ExpenseArgs>,
    ) -> Result<String, String> {
        let payload = build_payload(&args.to_input()?).map_err(render_domain)?;
        let client = self.client()?;
        let response = client.post(VOUCHERS, &payload, &[]).await.map_err(render)?;
        let booked = VoucherRef::of(&response);
        // Upstream's message, with one change: it names this connector's
        // argument, `financial_year`, where upstream named its own
        // `financialYear`. A model that copies the sentence literally then
        // produces a call that works.
        Ok(format!(
            "Booked expense as voucher {series}{number} (year {year}). To attach the \
             receipt: attach_receipt with series={series}, number={number}, \
             financial_year={year}.",
            series = booked.series,
            number = booked.number,
            year = booked.year,
        ))
    }

    #[tool(
        description = "Book a bank payment against a receivable or payable: a customer \
                       paying an invoice (debit 1930, credit 1510) or a supplier being paid \
                       (debit 2440, credit 1930). Call preview_reconciliation first to show \
                       the person what would be booked. Posts immediately. Reachable only \
                       through propose_action; the approval gate decides whether it runs."
    )]
    pub async fn reconcile_payment(
        &self,
        Parameters(args): Parameters<ReconcileArgs>,
    ) -> Result<String, String> {
        let payload = build_payload(&args.to_input()?).map_err(render_domain)?;
        let client = self.client()?;
        let response = client.post(VOUCHERS, &payload, &[]).await.map_err(render)?;
        let booked = VoucherRef::of(&response);
        Ok(format!(
            "Booked reconciliation as voucher {}{}.",
            booked.series, booked.number
        ))
    }
}

/// What Fortnox says it booked.
///
/// Upstream reads `res?.Voucher ?? {}` and falls back to `''` for the series
/// and number and `'?'` for the year; the same fallbacks are here, because the
/// alternative — failing after a successful post — would tell the caller the
/// voucher did not happen when it did.
struct VoucherRef {
    series: String,
    number: String,
    year: String,
}

impl VoucherRef {
    fn of(response: &Value) -> Self {
        let voucher = response.get("Voucher");
        let field = |name: &str| {
            voucher
                .and_then(|v| v.get(name))
                .map(scalar)
                .unwrap_or_default()
        };
        let year = field("Year");
        Self {
            series: field("VoucherSeries"),
            number: field("VoucherNumber"),
            year: if year.is_empty() {
                "?".to_string()
            } else {
                year
            },
        }
    }
}

/// A JSON scalar as the text a person would read. `null` and anything
/// structured are the empty string, matching upstream's `?? ''`.
fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::test_support::{line, server_for, unconfigured};

    /// The four tools that reach Fortnox with anything but a GET.
    pub(crate) const WRITE_TOOLS: &[&str] = &[
        "record_voucher",
        "record_expense",
        "reconcile_payment",
        "attach_receipt",
    ];

    fn expense_args() -> ExpenseArgs {
        ExpenseArgs {
            gross_amount: 1250.0,
            vat_rate: 25,
            expense_account: "5410".to_string(),
            payment_account: "1930".to_string(),
            transaction_date: "2026-05-31".to_string(),
            description: "Dator".to_string(),
            series: Some("A".to_string()),
        }
    }

    fn balanced_lines() -> Vec<LineArg> {
        vec![
            line("5410", 1000.0, 0.0, None),
            line("2640", 250.0, 0.0, None),
            line("1930", 0.0, 1250.0, None),
        ]
    }

    async fn vouchers_endpoint(body: serde_json::Value) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/vouchers"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json"),
            )
            .mount(&mock)
            .await;
        mock
    }

    // -----------------------------------------------------------------------
    // The thing this whole task is about
    // -----------------------------------------------------------------------

    /// **The pin.** Upstream guards each write with `confirm: true`; this
    /// connector's gate is `propose_action` plus a human tap in Telegram, and
    /// having both would mean one of the two is the one people stop reading.
    ///
    /// Walks every property name in every write tool's generated input schema,
    /// at every depth, and fails if `confirm` appears anywhere. Reintroducing
    /// the parameter — as a top-level argument, as a field of a nested object,
    /// or inside an array's items — breaks this test rather than quietly
    /// adding a second confirmation step.
    #[test]
    fn no_write_tool_takes_a_confirm_parameter() {
        for tool in FortnoxServer::router().list_all() {
            if !WRITE_TOOLS.contains(&tool.name.as_ref()) {
                continue;
            }
            let schema = Value::Object((*tool.input_schema).clone());
            let names = property_names(&schema);
            assert!(
                !names.contains("confirm"),
                "{} has a `confirm` parameter. The approval gate is the confirmation in \
                 this system; a second one inside the tool is the one that stops being \
                 read. Schema: {schema}",
                tool.name
            );
        }
    }

    /// The same, for the previews: a preview that took a `confirm` flag would
    /// be a write in disguise.
    #[test]
    fn no_tool_at_all_takes_a_confirm_parameter() {
        for tool in FortnoxServer::router().list_all() {
            let schema = Value::Object((*tool.input_schema).clone());
            assert!(
                !property_names(&schema).contains("confirm"),
                "{} has a `confirm` parameter: {schema}",
                tool.name
            );
        }
    }

    /// Every property name anywhere in a JSON schema.
    fn property_names(schema: &Value) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        collect(schema, &mut found);
        found
    }

    fn collect(node: &Value, found: &mut BTreeSet<String>) {
        match node {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "properties" {
                        if let Value::Object(properties) = value {
                            found.extend(properties.keys().cloned());
                        }
                    }
                    collect(value, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect(item, found)),
            _ => {}
        }
    }

    /// The promise in the descriptions, checked rather than assumed.
    #[test]
    fn every_write_tool_says_it_posts_immediately() {
        for tool in FortnoxServer::router().list_all() {
            if !WRITE_TOOLS.contains(&tool.name.as_ref()) {
                continue;
            }
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains(POSTS_IMMEDIATELY),
                "{} must carry the standard sentence verbatim. Got: {description}",
                tool.name
            );
        }
    }

    // -----------------------------------------------------------------------
    // Posting
    // -----------------------------------------------------------------------

    /// One call, one voucher. Not zero (the gate already approved it) and not
    /// two (a retry inside the tool would double-book).
    #[tokio::test]
    async fn record_expense_posts_exactly_once() {
        let mock = vouchers_endpoint(
            serde_json::json!({ "Voucher": { "VoucherSeries": "A", "VoucherNumber": 42, "Year": 2 } }),
        )
        .await;
        let server = server_for(&mock);

        let text = server
            .record_expense(Parameters(expense_args()))
            .await
            .expect("a 200 books the voucher");

        let requests = mock.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1, "exactly one POST: {requests:?}");
        assert_eq!(requests[0].url.path(), "/vouchers");

        let body: Value = serde_json::from_slice(&requests[0].body).expect("a JSON body");
        let rows = body["Voucher"]["VoucherRows"]["VoucherRow"]
            .as_array()
            .expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["Account"], "5410");
        assert_eq!(rows[0]["Debit"], 1000.0);
        assert_eq!(rows[1]["Account"], "2640");
        assert_eq!(rows[1]["Debit"], 250.0);
        assert_eq!(rows[2]["Account"], "1930");
        assert_eq!(rows[2]["Credit"], 1250.0);

        // Upstream's follow-up sentence, pointing at this connector's own
        // argument names.
        assert!(text.contains("A42"), "{text}");
        assert!(text.contains("year 2"), "{text}");
        assert!(text.contains("attach_receipt"), "{text}");
        assert!(text.contains("financial_year=2"), "{text}");
    }

    #[tokio::test]
    async fn record_voucher_posts_the_lines_and_reports_the_reference() {
        let mock = vouchers_endpoint(
            serde_json::json!({ "Voucher": { "VoucherSeries": "A", "VoucherNumber": 42 } }),
        )
        .await;
        let text = server_for(&mock)
            .record_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "Dator".to_string(),
                lines: balanced_lines(),
            }))
            .await
            .unwrap();

        assert_eq!(text, "Booked voucher A42.");
        assert_eq!(mock.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reconcile_payment_posts_and_reports_the_reference() {
        let mock = vouchers_endpoint(
            serde_json::json!({ "Voucher": { "VoucherSeries": "B", "VoucherNumber": 7 } }),
        )
        .await;
        let text = server_for(&mock)
            .reconcile_payment(Parameters(ReconcileArgs {
                amount: 5000.0,
                bank_account: None,
                counter_account: "1510".to_string(),
                direction: Direction::Incoming,
                transaction_date: "2026-05-31".to_string(),
                description: "Kund betalar".to_string(),
                series: None,
            }))
            .await
            .unwrap();

        assert_eq!(text, "Booked reconciliation as voucher B7.");
        let requests = mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let rows = body["Voucher"]["VoucherRows"]["VoucherRow"]
            .as_array()
            .unwrap();
        assert_eq!(rows[0]["Account"], "1930", "the bank is debited");
        assert_eq!(rows[1]["Account"], "1510");
    }

    // -----------------------------------------------------------------------
    // Refusals that cost nothing
    // -----------------------------------------------------------------------

    /// The brief's requirement, and the one that matters: refusing *before*
    /// the network, not after.
    #[tokio::test]
    async fn record_voucher_refuses_unbalanced_lines_and_makes_no_http_call() {
        let mock = MockServer::start().await;
        let err = server_for(&mock)
            .record_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "x".to_string(),
                lines: vec![
                    line("5410", 1000.0, 0.0, None),
                    line("1930", 0.0, 900.0, None),
                ],
            }))
            .await
            .expect_err("unbalanced lines must not be posted");

        assert!(err.contains("does not balance"), "{err}");
        assert!(err.contains("100"), "the difference must be named: {err}");
        assert!(
            mock.received_requests().await.unwrap().is_empty(),
            "an unbalanced voucher must cost zero requests"
        );
    }

    #[tokio::test]
    async fn an_unsupported_vat_rate_errors_and_makes_no_http_call() {
        let mock = MockServer::start().await;
        let mut args = expense_args();
        args.vat_rate = 20;
        let err = server_for(&mock)
            .record_expense(Parameters(args))
            .await
            .expect_err("20% is not a Swedish VAT rate");

        assert!(err.contains("unsupported VAT rate 20%"), "{err}");
        assert!(
            mock.received_requests().await.unwrap().is_empty(),
            "a bad rate must cost zero requests"
        );
    }

    #[tokio::test]
    async fn a_malformed_date_errors_and_makes_no_http_call() {
        let mock = MockServer::start().await;
        let mut args = expense_args();
        args.transaction_date = "2026-5-31".to_string();
        let err = server_for(&mock)
            .record_expense(Parameters(args))
            .await
            .expect_err("Fortnox wants YYYY-MM-DD");

        assert!(err.contains("YYYY-MM-DD"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    /// A write on an unconfigured connector must say what is missing — and,
    /// again, must not have posted anything first.
    #[tokio::test]
    async fn a_write_without_credentials_names_the_missing_file() {
        let err = unconfigured()
            .record_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "Dator".to_string(),
                lines: balanced_lines(),
            }))
            .await
            .expect_err("no credentials");
        assert!(err.contains("no usable credentials"), "{err}");
        assert!(err.contains("app.json"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Defaults and labels
    // -----------------------------------------------------------------------

    #[test]
    fn the_series_and_bank_defaults_are_upstreams() {
        let args = ReconcileArgs {
            amount: 5000.0,
            bank_account: None,
            counter_account: "1510".to_string(),
            direction: Direction::Incoming,
            transaction_date: "2026-05-31".to_string(),
            description: "x".to_string(),
            series: None,
        };
        let input = args.to_input().unwrap();
        assert_eq!(input.series, DEFAULT_SERIES);
        assert_eq!(input.lines[0].account, DEFAULT_BANK_ACCOUNT);

        let expense = ExpenseArgs {
            series: None,
            ..expense_args()
        };
        assert_eq!(expense.to_expense().unwrap().series, DEFAULT_SERIES);
    }

    #[test]
    fn the_labels_are_upstreams() {
        assert_eq!(expense_args().label(), "Expense 1250 kr incl. 25% VAT");
        assert_eq!(
            ReconcileArgs {
                amount: 5000.0,
                bank_account: None,
                counter_account: "2440".to_string(),
                direction: Direction::Outgoing,
                transaction_date: "2026-05-31".to_string(),
                description: "x".to_string(),
                series: None,
            }
            .label(),
            "Reconcile outgoing 5000 kr"
        );
    }

    /// Fortnox answering with something unexpected must not turn a *successful
    /// post* into an error — the voucher exists either way.
    #[tokio::test]
    async fn a_response_without_a_voucher_still_reports_success() {
        let mock = vouchers_endpoint(serde_json::json!({})).await;
        let text = server_for(&mock)
            .record_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "Dator".to_string(),
                lines: balanced_lines(),
            }))
            .await
            .expect("the post succeeded");
        assert_eq!(text, "Booked voucher .");
    }
}
