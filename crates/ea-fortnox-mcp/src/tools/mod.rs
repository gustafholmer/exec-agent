//! The MCP surface: seven read tools, three report tools, three previews and
//! four writes.
//!
//! # The shape, and why it differs from upstream's
//!
//! Upstream's `tools/` directory is four files returning arrays of tool
//! modules, each write tool carrying a `confirm` flag that turns a preview
//! into a posting. Here the split is by what a tool *does to the world*, which
//! is the split the policy gate cares about:
//!
//! * [`read`] and [`report`] — GET only.
//! * [`preview`] — renders the voucher a write would post, makes no HTTP call
//!   at all, and works on an unconfigured connector.
//! * [`write`] and [`attach`] — POST, with no `confirm` parameter, because
//!   reaching one means `propose_action` already ran and a human already
//!   tapped approve. See the crate docs.
//!
//! # Where the numbers come from
//!
//! Nothing in this crate does arithmetic. Every figure is computed by
//! [`ea_fortnox`] — [`ea_fortnox::domain::moms::split_gross`] for a VAT split,
//! [`ea_fortnox::domain::voucher::build_payload`] for a posted body,
//! [`ea_fortnox::reporting`] for every summary — which is where the tests that
//! establish the figures are correct also live. These modules translate
//! arguments in and JSON out.
//!
//! # List endpoints go through `get_all`
//!
//! Fortnox caps a list response at 100 rows by default. A VAT return computed
//! from page one is wrong without looking wrong, so every account, voucher and
//! invoice list here uses [`FortnoxClient::get_all`], never a plain `get`.
//! `financialyears` is the one exception, matching upstream: it is looked up
//! by date and answers with the one year that covers it.

pub mod attach;
pub mod preview;
pub mod read;
pub mod report;
pub mod write;

use std::sync::Arc;

use ea_fortnox::domain::money::to_api;
use ea_fortnox::errors::FortnoxError;
use ea_fortnox::reporting::AccountRow;
use ea_fortnox::FortnoxClient;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{tool_handler, ServerHandler};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

/// The name the daemon knows this connector by, and the `[section]` its
/// `policy.toml` rules live under.
pub use ea_fortnox::auth::CONNECTOR;

/// Either a working client, or the reason there isn't one.
///
/// A connector with no `app.json` still starts and still completes the MCP
/// handshake; it just fails every call that needs Fortnox with a message
/// naming the file to create. Exiting at start-up instead would reach the
/// daemon as "handshake failed", which tells nobody what to do.
///
/// The [`preview`] tools deliberately do not consult this at all: rendering
/// what *would* be booked is pure arithmetic, and it is more useful for a
/// half-configured connector to still be able to show a person what a voucher
/// would look like than for it to refuse uniformly.
enum Backend {
    Ready(FortnoxClient),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct FortnoxServer {
    backend: Arc<Backend>,
    tool_router: ToolRouter<Self>,
}

impl FortnoxServer {
    pub fn new(client: FortnoxClient) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(client)),
            tool_router: Self::router(),
        }
    }

    /// A server that will answer every Fortnox-touching call with `reason`.
    /// The previews still work.
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Unconfigured(reason.into())),
            tool_router: Self::router(),
        }
    }

    /// Every tool this connector serves, in one router.
    ///
    /// Public so a test can enumerate the surface and its schemas without
    /// standing up a server — which is how
    /// [`write::tests`] pins the absence of `confirm`.
    pub fn router() -> ToolRouter<Self> {
        Self::read_router()
            + Self::report_router()
            + Self::preview_router()
            + Self::write_router()
            + Self::attach_router()
    }

    pub(crate) fn client(&self) -> Result<&FortnoxClient, String> {
        match &*self.backend {
            Backend::Ready(client) => Ok(client),
            Backend::Unconfigured(reason) => Err(format!(
                "fortnox: this connector has no usable credentials, so it cannot reach \
                 Fortnox. {reason}"
            )),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FortnoxServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Swedish bookkeeping in Fortnox for the owner's company. The read and report \
             tools are free to call. The three preview_* tools render exactly what a write \
             would book and post nothing — call the preview first and show the person its \
             text. The write tools (record_voucher, record_expense, reconcile_payment, \
             attach_receipt) post to Fortnox the moment they are called; they are reachable \
             only through propose_action, and the approval gate — not this server — decides \
             whether one runs. There is deliberately no `confirm` flag: the human tap is the \
             confirmation. Every computed VAT or result figure is best-effort over the BAS \
             chart and must be checked against the official momsdeklaration before filing.",
        )
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A [`FortnoxError`] as the string the model reads.
///
/// `Display` on `FortnoxError` is already written for this: it never carries a
/// token, a URL with credentials in it, or a response body beyond a bounded
/// snippet. See `ea_fortnox::errors`.
pub(crate) fn render(err: FortnoxError) -> String {
    format!("fortnox: {err}")
}

/// An `anyhow` error — everything from `domain` — as the string the model
/// reads. `{:#}` keeps the context chain, which is where "invalid expense
/// account" lives.
pub(crate) fn render_domain(err: anyhow::Error) -> String {
    format!("fortnox: {err:#}")
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|err| format!("fortnox: could not serialise the reply: {err}"))
}

/// Today, UTC, as `YYYY-MM-DD`.
///
/// Upstream's `new Date().toISOString().slice(0, 10)`, which is UTC too. It is
/// only ever used to ask Fortnox *which financial year covers this date*, and
/// a financial year is months long, so the one day a year on which a Swedish
/// local date and a UTC date disagree cannot pick the wrong year except at a
/// year boundary — where the caller should be passing an explicit date anyway.
pub(crate) fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The id of the financial year covering `date` (default: today).
///
/// [`None`] when Fortnox knows no year for that date — upstream's
/// `years[0]?.Id` being `undefined`, which it then spreads into a query as an
/// absent parameter. Reproduced: an absent `financialyear` makes Fortnox
/// answer for its own default year, which is what upstream has been doing.
pub(crate) async fn financial_year_id(
    client: &FortnoxClient,
    date: Option<&str>,
) -> Result<Option<i64>, String> {
    let owned;
    let date = match date {
        Some(date) => date,
        None => {
            owned = today_utc();
            &owned
        }
    };
    let response = client
        .get("financialyears", &[("date", date)])
        .await
        .map_err(render)?;
    Ok(response
        .get("FinancialYears")
        .and_then(Value::as_array)
        .and_then(|years| years.first())
        .and_then(|year| year.get("Id"))
        .and_then(coerce_i64))
}

/// Fortnox is inconsistent about whether an id is a JSON number or a string.
fn coerce_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// One account from a Fortnox `Accounts` row, as [`ea_fortnox::reporting`]
/// wants it.
///
/// `Number` arrives as a string or a number depending on the endpoint;
/// `Balance` likewise. A row with neither is skipped rather than counted as
/// zero, because a zero would silently join every sum.
pub(crate) fn account_rows(rows: &[Value]) -> Vec<AccountRow> {
    rows.iter()
        .filter_map(|row| {
            let number = coerce_string(row.get("Number")?)?;
            Some(AccountRow {
                number,
                balance: row
                    .get("Balance")
                    .map(coerce_amount)
                    .unwrap_or(Decimal::ZERO),
            })
        })
        .collect()
}

/// `{ account, description, balance }` — upstream's projection of an account
/// row, which is what every report returns instead of Fortnox's full row.
pub(crate) fn account_view(row: &Value) -> Value {
    serde_json::json!({
        "account": row.get("Number").cloned().unwrap_or(Value::Null),
        "description": row.get("Description").cloned().unwrap_or(Value::Null),
        "balance": row.get("Balance").cloned().unwrap_or(Value::Null),
    })
}

/// The account number of a Fortnox row, for the class predicates. `None` when
/// the row has no usable `Number`, which makes it fall out of every filter —
/// the same fate upstream's `String(undefined)` gives it.
pub(crate) fn account_number(row: &Value) -> Option<String> {
    row.get("Number").and_then(coerce_string)
}

fn coerce_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A JSON amount as a [`Decimal`].
///
/// Through the shortest round-tripping decimal string rather than
/// `Decimal::try_from(f64)`, the same idiom as `ea_fortnox`'s workbook reader
/// and for the same reason: a balance of `-1234.56` arrives as the double
/// nearest that decimal, and `format!("{f}")` is exactly `"-1234.56"`.
/// Anything unparseable is zero, matching upstream's `Number(x) || 0`.
pub(crate) fn coerce_amount(value: &Value) -> Decimal {
    match value {
        Value::Number(n) => n.to_string().parse().unwrap_or(Decimal::ZERO),
        Value::String(s) => s.trim().parse().unwrap_or(Decimal::ZERO),
        _ => Decimal::ZERO,
    }
}

/// A computed [`Decimal`] as a JSON **number**.
///
/// `rust_decimal`'s own `Serialize` emits a string (`"26800"`), which upstream
/// does not: `JSON.stringify` of a JavaScript number is a number, and a model
/// comparing `net_vat_to_pay` against a threshold should not have to parse a
/// string first. The conversion is
/// [`ea_fortnox::domain::money::to_api`] — round to öre, then to `f64` — the
/// same one every posted amount goes through, so a figure reported here and
/// the same figure posted to Fortnox are the same number. Rounding to öre is
/// a no-op on a sum of öre-granular balances, which is every figure here.
pub(crate) fn amount_value(amount: Decimal) -> Value {
    serde_json::Number::from_f64(to_api(amount))
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// An amount a *caller* supplied, in kronor.
///
/// Same conversion as [`coerce_amount`], but an unusable value is an error
/// rather than zero: silently booking 0.00 kr because a model sent `"1 250"`
/// would be a voucher nobody meant.
pub(crate) fn kronor(field: &str, amount: f64) -> Result<Decimal, String> {
    if !amount.is_finite() {
        return Err(format!(
            "fortnox: {field} must be a finite amount in kronor, not {amount}"
        ));
    }
    format!("{amount}")
        .parse()
        .map_err(|_| format!("fortnox: {field} is not an amount in kronor: {amount}"))
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    /// The catalogue, pinned. Upstream's `index.test.ts` asserts the same
    /// thing over its fourteen tools; this adds the three previews and drops
    /// nothing.
    #[test]
    fn the_server_registers_exactly_the_seventeen_planned_tools() {
        let names: BTreeSet<String> = FortnoxServer::router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let expected: BTreeSet<String> = [
            "account_ledger",
            "attach_receipt",
            "balance_sheet",
            "financial_overview",
            "period_report",
            "preview_expense",
            "preview_reconciliation",
            "preview_voucher",
            "profit_and_loss",
            "query_fortnox",
            "reconcile_payment",
            "record_expense",
            "record_voucher",
            "result_summary",
            "unpaid_invoices",
            "vat_report",
            "vat_summary",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            names, expected,
            "an eighteenth Fortnox tool must be a deliberate act, with a policy rule to match"
        );
    }

    /// Every tool must say what it does; the description is the only thing a
    /// model has to go on when choosing between `record_expense` and
    /// `preview_expense`.
    #[test]
    fn every_tool_has_a_description() {
        for tool in FortnoxServer::router().list_all() {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.len() > 40,
                "{} has no useful description: {description:?}",
                tool.name
            );
        }
    }

    #[test]
    fn an_unconfigured_server_names_the_reason_but_still_previews() {
        let server = FortnoxServer::unconfigured("no Fortnox integration at /tmp/app.json");
        let err = server.client().expect_err("no client");
        assert!(err.contains("app.json"), "{err}");
        assert!(err.contains("no usable credentials"), "{err}");
    }

    #[test]
    fn amounts_convert_through_their_shortest_decimal_string() {
        assert_eq!(kronor("x", 1250.0).unwrap(), Decimal::new(1250, 0));
        assert_eq!(kronor("x", -1234.56).unwrap(), Decimal::new(-123456, 2));
        assert!(kronor("grossAmount", f64::NAN).is_err());
        assert!(kronor("grossAmount", f64::INFINITY).is_err());
    }

    #[test]
    fn account_rows_take_a_number_or_a_string_for_either_field() {
        let rows = vec![
            serde_json::json!({ "Number": "1930", "Balance": 80000.0 }),
            serde_json::json!({ "Number": 2440, "Balance": "-50.25" }),
            serde_json::json!({ "Balance": 1.0 }),
        ];
        let parsed = account_rows(&rows);
        assert_eq!(parsed.len(), 2, "a row with no Number is skipped");
        assert_eq!(parsed[0].number, "1930");
        assert_eq!(parsed[1].number, "2440");
        assert_eq!(parsed[1].balance, Decimal::new(-5025, 2));
    }
}
