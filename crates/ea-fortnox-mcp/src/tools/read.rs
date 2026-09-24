//! The read tools: `financial_overview`, `profit_and_loss`, `balance_sheet`,
//! `vat_summary`, `unpaid_invoices`, `account_ledger`, `query_fortnox`.
//!
//! A straight port of `readTools.ts`. Two things about them are worth stating.
//!
//! **Every list goes through `get_all`.** Upstream learned this the hard way
//! and its own comment says so: a plain `get` on `accounts` returns page one,
//! and a balance sheet built from page one is wrong without looking wrong.
//!
//! **`query_fortnox` is an escape hatch that cannot write.** It issues a GET
//! and nothing else. The path check is upstream's — no `..`, no absolute
//! URL — kept because the alternative is a model talking itself into
//! `../oauth`.

use ea_fortnox::domain::bas::{is_balance_account, is_input_vat, is_output_vat, is_result_account};
use ea_fortnox::reporting::cash_accounts;
use ea_fortnox::FortnoxClient;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{
    account_number, account_rows, account_view, amount_value, financial_year_id, render, to_json,
    FortnoxServer,
};

/// Which side of the ledger `unpaid_invoices` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceKind {
    /// Kundfakturor — money owed to us.
    Customer,
    /// Leverantörsfakturor — money we owe.
    Supplier,
}

/// The one argument most read tools take.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DateArgs {
    /// Any date inside the target financial year, YYYY-MM-DD. Defaults to
    /// today.
    pub date: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UnpaidInvoicesArgs {
    /// Which side: "customer" receivables or "supplier" payables.
    pub kind: InvoiceKind,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AccountLedgerArgs {
    /// Any date inside the target financial year, YYYY-MM-DD. Defaults to
    /// today.
    pub date: Option<String>,
    /// Voucher series letter, e.g. "A". Omit for every series.
    pub series: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct QueryFortnoxArgs {
    /// Resource path under https://api.fortnox.se/3/, e.g. "customers".
    pub path: String,
    /// Optional query parameters.
    pub query: Option<Map<String, Value>>,
}

/// The unpaid filter both invoice endpoints take.
const UNPAID: (&str, &str) = ("filter", "unpaid");

/// Every account in the financial year covering `date`, paginated.
async fn accounts_for(
    client: &FortnoxClient,
    date: Option<&str>,
) -> Result<(Option<i64>, Vec<Value>), String> {
    let year = financial_year_id(client, date).await?;
    let year_param = year.map(|y| y.to_string());
    let query: Vec<(&str, &str)> = match &year_param {
        Some(value) => vec![("financialyear", value.as_str())],
        None => Vec::new(),
    };
    let rows = client
        .get_all("accounts", "Accounts", &query)
        .await
        .map_err(render)?;
    Ok((year, rows))
}

/// Accounts matching one of the BAS predicates, projected to
/// `{ account, description, balance }`.
pub(crate) fn accounts_where(rows: &[Value], predicate: fn(&str) -> bool) -> Vec<Value> {
    rows.iter()
        .filter(|row| account_number(row).is_some_and(|number| predicate(&number)))
        .map(account_view)
        .collect()
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "High-level snapshot of the company's finances: the balance of every \
                       cash account (19xx), every unpaid customer invoice and every unpaid \
                       supplier invoice. Read-only. Start here when asked how things stand."
    )]
    pub async fn financial_overview(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
        let (year, accounts) = accounts_for(client, date.as_deref()).await?;
        let receivable = client
            .get_all("invoices", "Invoices", &[UNPAID])
            .await
            .map_err(render)?;
        let payable = client
            .get_all("supplierinvoices", "SupplierInvoices", &[UNPAID])
            .await
            .map_err(render)?;

        let cash: Vec<Value> = cash_accounts(&account_rows(&accounts))
            .into_iter()
            .map(|account| {
                serde_json::json!({
                    "account": account.account,
                    "balance": amount_value(account.balance),
                })
            })
            .collect();

        to_json(&serde_json::json!({
            "financial_year": year,
            "cash_accounts": cash,
            "unpaid_customer_invoices": receivable,
            "unpaid_supplier_invoices": payable,
        }))
    }

    #[tool(
        description = "Income-statement accounts (resultaträkning — BAS classes 3 to 8) with \
                       their balances for the financial year covering the given date, as a \
                       JSON array of { account, description, balance }. Read-only. For the \
                       bottom line rather than the accounts, use result_summary."
    )]
    pub async fn profit_and_loss(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = accounts_for(self.client()?, date.as_deref()).await?;
        to_json(&serde_json::json!({
            "financial_year": year,
            "accounts": accounts_where(&accounts, is_result_account),
        }))
    }

    #[tool(
        description = "Balance-sheet accounts (balansräkning — BAS classes 1 and 2) with \
                       their balances for the financial year covering the given date, as a \
                       JSON array of { account, description, balance }. Read-only."
    )]
    pub async fn balance_sheet(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = accounts_for(self.client()?, date.as_deref()).await?;
        to_json(&serde_json::json!({
            "financial_year": year,
            "accounts": accounts_where(&accounts, is_balance_account),
        }))
    }

    #[tool(
        description = "Balances of the Swedish VAT accounts — output moms 261x/262x/263x (25, \
                       12 and 6 percent) and input moms 264x — so you can see what is owed to \
                       or reclaimable from Skatteverket. The settlement account 2650 is \
                       excluded, being neither side. Read-only. For the netted figures, use \
                       vat_report."
    )]
    pub async fn vat_summary(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = accounts_for(self.client()?, date.as_deref()).await?;
        let vat = accounts_where(&accounts, |number| {
            is_output_vat(number) || is_input_vat(number)
        });
        to_json(&serde_json::json!({
            "financial_year": year,
            "vat_accounts": vat,
        }))
    }

    #[tool(
        description = "Outstanding invoices: customer receivables (kundfakturor) when kind is \
                       \"customer\", supplier payables (leverantörsfakturor) when it is \
                       \"supplier\". Returns Fortnox's own rows, every page of them. \
                       Read-only."
    )]
    pub async fn unpaid_invoices(
        &self,
        Parameters(UnpaidInvoicesArgs { kind }): Parameters<UnpaidInvoicesArgs>,
    ) -> Result<String, String> {
        let (path, list_key) = match kind {
            InvoiceKind::Customer => ("invoices", "Invoices"),
            InvoiceKind::Supplier => ("supplierinvoices", "SupplierInvoices"),
        };
        let rows = self
            .client()?
            .get_all(path, list_key, &[UNPAID])
            .await
            .map_err(render)?;
        to_json(&rows)
    }

    #[tool(
        description = "Vouchers booked in the financial year covering the given date, \
                       optionally narrowed to one voucher series. Read-only. Fortnox's list \
                       view omits the individual rows: use query_fortnox on a single voucher \
                       (\"vouchers/A/42\") for line detail."
    )]
    pub async fn account_ledger(
        &self,
        Parameters(AccountLedgerArgs { date, series }): Parameters<AccountLedgerArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
        let year = financial_year_id(client, date.as_deref()).await?;
        let year_param = year.map(|y| y.to_string());
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(value) = &year_param {
            query.push(("financialyear", value.as_str()));
        }
        if let Some(series) = &series {
            query.push(("sieseries", series.as_str()));
        }
        let rows = client
            .get_all("vouchers", "Vouchers", &query)
            .await
            .map_err(render)?;
        to_json(&rows)
    }

    #[tool(
        description = "Escape hatch: GET any Fortnox v3 resource by path, e.g. \"customers\", \
                       \"articles\" or \"vouchers/A/42\", with optional query parameters. \
                       Read-only by construction — this issues a GET and has no way to write. \
                       Prefer the named tools; use this for something they do not cover. \
                       Note that a list endpoint answers one page here, unlike the named \
                       tools, which follow every page."
    )]
    pub async fn query_fortnox(
        &self,
        Parameters(QueryFortnoxArgs { path, query }): Parameters<QueryFortnoxArgs>,
    ) -> Result<String, String> {
        // Upstream's check, kept: a traversal or an absolute URL is either a
        // mistake or an attempt to reach something that is not this API.
        if path.contains("..") || is_absolute_url(&path) {
            return Err(format!("fortnox: refusing suspicious path: {path}"));
        }
        let pairs = query_pairs(query.as_ref())?;
        let borrowed: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let response = self
            .client()?
            .get(path.trim_start_matches('/'), &borrowed)
            .await
            .map_err(render)?;
        to_json(&response)
    }
}

/// Upstream's `/^https?:/i`, widened to any scheme: `file:` and `ftp:` are no
/// more this API than `http:` is, and `Url::join` would follow them.
fn is_absolute_url(path: &str) -> bool {
    match path.find(':') {
        Some(0) | None => false,
        Some(colon) => path[..colon]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'),
    }
}

/// Upstream accepts `z.record(z.union([z.string(), z.number()]))`. Same here,
/// plus booleans, which serialise unambiguously; a nested object or array has
/// no query-string spelling and is refused rather than stringified into
/// something Fortnox will not understand.
fn query_pairs(query: Option<&Map<String, Value>>) -> Result<Vec<(String, String)>, String> {
    let Some(query) = query else {
        return Ok(Vec::new());
    };
    query
        .iter()
        .map(|(key, value)| match value {
            Value::String(s) => Ok((key.clone(), s.clone())),
            Value::Number(n) => Ok((key.clone(), n.to_string())),
            Value::Bool(b) => Ok((key.clone(), b.to_string())),
            other => Err(format!(
                "fortnox: query parameter {key:?} must be a string, number or boolean, not {other}"
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path as path_matcher, query_param};
    use wiremock::{Mock, MockServer};

    use crate::tools::test_support::{json_body, server_for, unconfigured};

    /// The chart upstream's own `readTools.test.ts` uses, including the row
    /// with a leading space that exercises the trim in `class_of`.
    fn chart() -> serde_json::Value {
        serde_json::json!({
            "Accounts": [
                { "Number": "1930", "Description": "Bank", "Balance": 100 },
                { "Number": "2440", "Description": "Leverantörsskulder", "Balance": -50 },
                { "Number": "3010", "Description": "Försäljning", "Balance": -200 },
                { "Number": "5410", "Description": "Förbrukningsinventarier", "Balance": 80 },
                { "Number": "8400", "Description": "Räntekostnader", "Balance": 10 },
                { "Number": " 1510", "Description": "Kundfordringar", "Balance": 30 },
            ],
            "MetaInformation": { "@TotalPages": 1 },
        })
    }

    async fn mock_with_chart() -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/financialyears"))
            .respond_with(json_body(
                serde_json::json!({ "FinancialYears": [{ "Id": 3 }] }),
            ))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/accounts"))
            .respond_with(json_body(chart()))
            .mount(&mock)
            .await;
        mock
    }

    fn accounts_of(text: &str) -> Vec<String> {
        let parsed: Value = serde_json::from_str(text).expect("JSON");
        parsed["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|row| row["account"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[tokio::test]
    async fn profit_and_loss_returns_only_bas_classes_three_to_eight() {
        let mock = mock_with_chart().await;
        let text = server_for(&mock)
            .profit_and_loss(Parameters(DateArgs {
                date: Some("2026-05-31".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(accounts_of(&text), vec!["3010", "5410", "8400"]);
    }

    /// The leading-space row is a balance account: `class_of` trims, and the
    /// projection reports the number exactly as Fortnox spelled it.
    #[tokio::test]
    async fn balance_sheet_returns_classes_one_and_two_and_trims_before_classifying() {
        let mock = mock_with_chart().await;
        let text = server_for(&mock)
            .balance_sheet(Parameters(DateArgs {
                date: Some("2026-05-31".to_string()),
            }))
            .await
            .unwrap();
        assert_eq!(accounts_of(&text), vec!["1930", "2440", " 1510"]);
    }

    /// The financial year is resolved first, and the account list is fetched
    /// with it — through the paginating `get_all`, which is what the `limit`
    /// and `page` parameters on the wire prove.
    #[tokio::test]
    async fn accounts_are_fetched_for_the_resolved_year_through_get_all() {
        let mock = mock_with_chart().await;
        server_for(&mock)
            .balance_sheet(Parameters(DateArgs {
                date: Some("2026-05-31".to_string()),
            }))
            .await
            .unwrap();

        let requests = mock.received_requests().await.unwrap();
        let years = requests
            .iter()
            .find(|r| r.url.path() == "/financialyears")
            .expect("the year is resolved first");
        assert_eq!(
            years
                .url
                .query_pairs()
                .find(|(k, _)| k == "date")
                .unwrap()
                .1,
            "2026-05-31"
        );

        let accounts = requests
            .iter()
            .find(|r| r.url.path() == "/accounts")
            .expect("the chart is fetched");
        let query: Vec<(String, String)> = accounts
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(
            query.contains(&("financialyear".into(), "3".into())),
            "{query:?}"
        );
        assert!(query.contains(&("limit".into(), "500".into())), "{query:?}");
        assert!(query.contains(&("page".into(), "1".into())), "{query:?}");
    }

    /// Upstream's `vat_summary` case: every output rate, the whole 264x input
    /// range, and 2650 excluded.
    #[tokio::test]
    async fn vat_summary_lists_both_sides_and_excludes_the_settlement_account() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/financialyears"))
            .respond_with(json_body(
                serde_json::json!({ "FinancialYears": [{ "Id": 3 }] }),
            ))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/accounts"))
            .respond_with(json_body(serde_json::json!({
                "Accounts": [
                    { "Number": "2611", "Balance": -25000 },
                    { "Number": "2621", "Balance": -1200 },
                    { "Number": "2631", "Balance": -600 },
                    { "Number": "2640", "Balance": 5000 },
                    { "Number": "2645", "Balance": 700 },
                    { "Number": "2650", "Balance": -3000 },
                    { "Number": "1930", "Balance": 80000 },
                ],
            })))
            .mount(&mock)
            .await;

        let text = server_for(&mock)
            .vat_summary(Parameters(DateArgs {
                date: Some("2026-05-31".to_string()),
            }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        let numbers: Vec<&str> = parsed["vat_accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["account"].as_str().unwrap())
            .collect();
        assert_eq!(numbers, vec!["2611", "2621", "2631", "2640", "2645"]);
    }

    #[tokio::test]
    async fn unpaid_invoices_picks_the_endpoint_from_the_kind_and_filters_unpaid() {
        for (kind, expected) in [
            (InvoiceKind::Customer, "/invoices"),
            (InvoiceKind::Supplier, "/supplierinvoices"),
        ] {
            let mock = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path_matcher(expected))
                .and(query_param("filter", "unpaid"))
                .respond_with(json_body(serde_json::json!({
                    "Invoices": [{ "DocumentNumber": 1 }],
                    "SupplierInvoices": [{ "GivenNumber": 2 }],
                })))
                .mount(&mock)
                .await;

            let text = server_for(&mock)
                .unpaid_invoices(Parameters(UnpaidInvoicesArgs { kind }))
                .await
                .unwrap();
            assert_ne!(text, "[]", "{kind:?}");
            let requests = mock.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.path(), expected);
        }
    }

    #[tokio::test]
    async fn financial_overview_reports_cash_and_both_invoice_sides() {
        let mock = mock_with_chart().await;
        Mock::given(method("GET"))
            .and(path_matcher("/invoices"))
            .respond_with(json_body(
                serde_json::json!({ "Invoices": [{ "DocumentNumber": 1 }] }),
            ))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/supplierinvoices"))
            .respond_with(json_body(serde_json::json!({ "SupplierInvoices": [] })))
            .mount(&mock)
            .await;

        let text = server_for(&mock)
            .financial_overview(Parameters(DateArgs { date: None }))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["financial_year"], 3);
        assert_eq!(parsed["cash_accounts"][0]["account"], "1930");
        assert_eq!(parsed["unpaid_customer_invoices"][0]["DocumentNumber"], 1);
        assert_eq!(
            parsed["unpaid_supplier_invoices"].as_array().unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn account_ledger_passes_the_series_through_when_given_and_omits_it_otherwise() {
        let mock = mock_with_chart().await;
        Mock::given(method("GET"))
            .and(path_matcher("/vouchers"))
            .respond_with(json_body(serde_json::json!({ "Vouchers": [] })))
            .mount(&mock)
            .await;
        let server = server_for(&mock);

        server
            .account_ledger(Parameters(AccountLedgerArgs {
                date: None,
                series: Some("A".to_string()),
            }))
            .await
            .unwrap();
        server
            .account_ledger(Parameters(AccountLedgerArgs {
                date: None,
                series: None,
            }))
            .await
            .unwrap();

        let vouchers: Vec<_> = mock
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/vouchers")
            .collect();
        assert_eq!(vouchers.len(), 2);
        assert!(vouchers[0].url.query().unwrap().contains("sieseries=A"));
        assert!(!vouchers[1].url.query().unwrap().contains("sieseries"));
    }

    #[tokio::test]
    async fn query_fortnox_gets_an_arbitrary_resource_with_its_query() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/customers"))
            .and(query_param("financialyear", "3"))
            .respond_with(json_body(serde_json::json!({ "Customers": [] })))
            .mount(&mock)
            .await;

        let text = server_for(&mock)
            .query_fortnox(Parameters(QueryFortnoxArgs {
                path: "/customers".to_string(),
                query: Some(
                    serde_json::json!({ "financialyear": 3 })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            }))
            .await
            .unwrap();
        assert!(text.contains("Customers"), "{text}");
    }

    #[tokio::test]
    async fn query_fortnox_refuses_traversal_and_absolute_urls_without_asking_fortnox() {
        let mock = MockServer::start().await;
        let server = server_for(&mock);
        for path in [
            "../../oauth",
            "https://evil.example/steal",
            "HTTP://evil.example",
            "file:///etc/passwd",
        ] {
            let err = server
                .query_fortnox(Parameters(QueryFortnoxArgs {
                    path: path.to_string(),
                    query: None,
                }))
                .await
                .unwrap_err();
            assert!(err.contains("refusing suspicious path"), "{path}: {err}");
        }
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn a_relative_path_with_a_colon_in_it_is_not_an_absolute_url() {
        assert!(!is_absolute_url("vouchers/A/42"));
        assert!(!is_absolute_url("customers?name=a:b"));
        assert!(is_absolute_url("https://x"));
        assert!(is_absolute_url("file:/x"));
    }

    #[tokio::test]
    async fn a_structured_query_value_is_refused() {
        let mock = MockServer::start().await;
        let err = server_for(&mock)
            .query_fortnox(Parameters(QueryFortnoxArgs {
                path: "customers".to_string(),
                query: Some(
                    serde_json::json!({ "filter": { "nested": true } })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            }))
            .await
            .unwrap_err();
        assert!(err.contains("must be a string, number or boolean"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_read_without_credentials_names_the_missing_file() {
        let err = unconfigured()
            .profit_and_loss(Parameters(DateArgs { date: None }))
            .await
            .unwrap_err();
        assert!(err.contains("no usable credentials"), "{err}");
        assert!(err.contains("app.json"), "{err}");
    }
}
