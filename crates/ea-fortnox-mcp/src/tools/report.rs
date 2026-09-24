//! The report tools: `vat_report`, `period_report`, `result_summary`.
//!
//! A port of `reportTools.ts`. The arithmetic is not here — it is
//! [`ea_fortnox::reporting`], where `summarise_vat` and `summarise_result`
//! live with the tests that establish they are right. These three fetch the
//! chart of accounts for a financial year, hand it over, and shape the answer.
//!
//! Every one of them carries a `note` telling the reader to check the figures
//! before filing. That is not boilerplate: these are computed from BAS sign
//! conventions over a live chart of accounts that may contain accounts this
//! crate's classifier does not recognise, and the consequence of a wrong
//! momsdeklaration is Skatteverket's, not a stack trace's.

use ea_fortnox::domain::bas::{is_balance_account, is_input_vat, is_output_vat, is_result_account};
use ea_fortnox::reporting::{summarise_result, summarise_vat, ClassBalance};
use ea_fortnox::FortnoxClient;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde_json::Value;

use super::read::{accounts_where, DateArgs};
use super::{account_rows, amount_value, financial_year_id, render, to_json, FortnoxServer};

/// The sentence `vat_report` ends with. Upstream's, verbatim.
const VAT_NOTE: &str = "Positive net_vat_to_pay = owe Skatteverket; negative = reclaim. Verify \
                        against the official momsdeklaration boxes before filing.";

/// The sentence `result_summary` ends with. Upstream's, verbatim.
const RESULT_NOTE: &str = "Figures are period-to-date and interpreted from BAS sign conventions. \
                           Verify against your accountant/Fortnox resultaträkning before relying \
                           on them.";

/// The chart of accounts for the financial year covering `date`.
///
/// The year is always sent: [`financial_year_id`] errors when none covers the
/// date, so a VAT return can never be computed from whichever year Fortnox
/// happened to default to.
async fn chart(client: &FortnoxClient, date: Option<&str>) -> Result<(i64, Vec<Value>), String> {
    let year = financial_year_id(client, date).await?;
    let year_param = year.to_string();
    let rows = client
        .get_all(
            "accounts",
            "Accounts",
            &[("financialyear", year_param.as_str())],
        )
        .await
        .map_err(render)?;
    Ok((year, rows))
}

fn class_view(class: &ClassBalance) -> Value {
    serde_json::json!({
        "class": class.class,
        "label": class.label,
        "balance": amount_value(class.balance),
    })
}

#[tool_router(router = report_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "VAT report for the financial year covering the given date, ready for a \
                       momsdeklaration: every output-VAT account (261x/262x/263x — 25, 12 and \
                       6 percent), every input-VAT account (264x), and the netted \
                       output_vat, input_vat and net_vat_to_pay. Positive net_vat_to_pay \
                       means money owed to Skatteverket. Read-only. The figures are computed \
                       from the BAS chart and must be checked against the official \
                       momsdeklaration boxes before filing."
    )]
    pub async fn vat_report(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = chart(self.client()?, date.as_deref()).await?;
        let summary = summarise_vat(&account_rows(&accounts));
        to_json(&serde_json::json!({
            "financial_year": year,
            "output_vat_accounts": accounts_where(&accounts, is_output_vat),
            "input_vat_accounts": accounts_where(&accounts, is_input_vat),
            "output_vat": amount_value(summary.output_vat),
            "input_vat": amount_value(summary.input_vat),
            "net_vat_to_pay": amount_value(summary.net_vat_to_pay),
            "note": VAT_NOTE,
        }))
    }

    #[tool(
        description = "Income statement and balance sheet together for the financial year \
                       covering the given date: BAS classes 3 to 8 under profit_and_loss and \
                       classes 1 and 2 under balance_sheet, each as { account, description, \
                       balance }. Read-only. This is the bundle to hand an accountant."
    )]
    pub async fn period_report(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = chart(self.client()?, date.as_deref()).await?;
        to_json(&serde_json::json!({
            "financial_year": year,
            "profit_and_loss": accounts_where(&accounts, is_result_account),
            "balance_sheet": accounts_where(&accounts, is_balance_account),
        }))
    }

    #[tool(
        description = "The income statement at a glance for the financial year covering the \
                       given date: revenue, operating costs, financial items, the net result \
                       and whether that is a Vinst or a Förlust, with a per-BAS-class \
                       breakdown. Read-only. Interpreted from BAS sign conventions and \
                       period-to-date; check it against Fortnox's own resultaträkning before \
                       relying on it."
    )]
    pub async fn result_summary(
        &self,
        Parameters(DateArgs { date }): Parameters<DateArgs>,
    ) -> Result<String, String> {
        let (year, accounts) = chart(self.client()?, date.as_deref()).await?;
        let summary = summarise_result(&account_rows(&accounts));
        to_json(&serde_json::json!({
            "financial_year": year,
            "revenue": amount_value(summary.revenue),
            "operating_costs": amount_value(summary.operating_costs),
            "financial_items": amount_value(summary.financial_items),
            "net_result": amount_value(summary.net_result),
            "outcome": summary.outcome.as_str(),
            "by_class": summary.by_class.iter().map(class_view).collect::<Vec<_>>(),
            "note": RESULT_NOTE,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path as path_matcher, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer};

    use crate::tools::test_support::{json_body, server_for};

    async fn mock_with(accounts: serde_json::Value) -> MockServer {
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
            .respond_with(json_body(serde_json::json!({ "Accounts": accounts })))
            .mount(&mock)
            .await;
        mock
    }

    /// A VAT report for a date in no financial year must refuse.
    ///
    /// This is the worst instance of the omitted-parameter bug: a
    /// momsdeklaration prepared from the wrong year's accounts, filed for a
    /// real company. Without the fix the call succeeds and reports whichever
    /// year Fortnox defaulted to, with `"financial_year": null` as the only
    /// clue.
    #[tokio::test]
    async fn vat_report_refuses_a_date_no_financial_year_covers() {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/financialyears"))
            .and(query_param("date", "2019-06-30"))
            .respond_with(json_body(serde_json::json!({ "FinancialYears": [] })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/financialyears"))
            .and(query_param_is_missing("date"))
            .respond_with(json_body(serde_json::json!({
                "FinancialYears": [
                    { "Id": 7, "FromDate": "2026-01-01", "ToDate": "2026-12-31" },
                ],
            })))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/accounts"))
            .respond_with(json_body(serde_json::json!({ "Accounts": [] })))
            .expect(0)
            .mount(&mock)
            .await;

        let err = server_for(&mock)
            .vat_report(Parameters(DateArgs {
                date: Some("2019-06-30".to_string()),
            }))
            .await
            .expect_err("no year covers 2019-06-30");

        assert!(err.contains("2019-06-30"), "{err}");
        assert!(err.contains("2026-01-01..2026-12-31 (id 7)"), "{err}");
        assert!(
            err.contains("CURRENT year"),
            "the trap must be named: {err}"
        );
    }

    fn today() -> Parameters<DateArgs> {
        Parameters(DateArgs {
            date: Some("2026-05-31".to_string()),
        })
    }

    /// Upstream's `reportTools.test.ts` VAT case, figure for figure.
    #[tokio::test]
    async fn vat_report_nets_both_sides_and_excludes_2650() {
        let mock = mock_with(serde_json::json!([
            { "Number": "2611", "Description": "Utg moms 25%", "Balance": -25000 },
            { "Number": "2621", "Description": "Utg moms 12%", "Balance": -1200 },
            { "Number": "2631", "Description": "Utg moms 6%", "Balance": -600 },
            { "Number": "2640", "Description": "Ing moms", "Balance": 5000 },
            { "Number": "2645", "Description": "Beräknad ing moms utland", "Balance": 700 },
            { "Number": "2650", "Description": "Redovisningskonto moms", "Balance": -3000 },
        ]))
        .await;

        let text = server_for(&mock).vat_report(today()).await.unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["output_vat"], 26800.0);
        assert_eq!(parsed["input_vat"], 5700.0);
        assert_eq!(parsed["net_vat_to_pay"], 21100.0);

        let listed: Vec<&str> = parsed["output_vat_accounts"]
            .as_array()
            .unwrap()
            .iter()
            .chain(parsed["input_vat_accounts"].as_array().unwrap())
            .map(|row| row["account"].as_str().unwrap())
            .collect();
        assert!(!listed.contains(&"2650"), "{listed:?}");
        assert!(parsed["note"].as_str().unwrap().contains("momsdeklaration"));
    }

    #[tokio::test]
    async fn period_report_groups_result_against_balance_accounts() {
        let mock = mock_with(serde_json::json!([
            { "Number": "3010", "Description": "Försäljning", "Balance": -100000 },
            { "Number": "1930", "Description": "Bank", "Balance": 80000 },
        ]))
        .await;

        let text = server_for(&mock).period_report(today()).await.unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["profit_and_loss"][0]["account"], "3010");
        assert_eq!(parsed["balance_sheet"][0]["account"], "1930");
        assert_eq!(parsed["financial_year"], 3);
    }

    /// Upstream's `result_summary` case: revenue only, so the whole of it is
    /// the result and the outcome is a Vinst.
    #[tokio::test]
    async fn result_summary_reports_the_bottom_line_and_its_breakdown() {
        let mock = mock_with(serde_json::json!([
            { "Number": "3010", "Description": "Försäljning", "Balance": -100000 },
            { "Number": "2611", "Description": "Utg moms 25", "Balance": -25000 },
            { "Number": "2640", "Description": "Ing moms", "Balance": 5000 },
            { "Number": "1930", "Description": "Bank", "Balance": 80000 },
        ]))
        .await;

        let text = server_for(&mock).result_summary(today()).await.unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["net_result"], 100000.0);
        assert_eq!(parsed["outcome"], "Vinst");
        assert_eq!(parsed["revenue"], 100000.0);
        // The balance-sheet accounts stay out of the per-class breakdown.
        let classes: Vec<u64> = parsed["by_class"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["class"].as_u64().unwrap())
            .collect();
        assert_eq!(classes, vec![3]);
        assert!(parsed["note"].as_str().unwrap().contains("Verify"));
    }

    /// A loss is reported as one, in Swedish, because that is the word the
    /// owner's accountant uses.
    #[tokio::test]
    async fn a_loss_is_a_forlust() {
        let mock = mock_with(serde_json::json!([
            { "Number": "3010", "Balance": -10000 },
            { "Number": "5010", "Balance": 25000 },
        ]))
        .await;
        let text = server_for(&mock).result_summary(today()).await.unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["net_result"], -15000.0);
        assert_eq!(parsed["outcome"], "Förlust");
    }

    /// Every report must paginate. A chart of accounts fetched one page at a
    /// time is how a VAT return quietly comes out short.
    #[tokio::test]
    async fn every_report_fetches_the_chart_through_get_all() {
        for report in ["vat_report", "period_report", "result_summary"] {
            let mock = mock_with(serde_json::json!([{ "Number": "3010", "Balance": -1 }])).await;
            let server = server_for(&mock);
            match report {
                "vat_report" => server.vat_report(today()).await.unwrap(),
                "period_report" => server.period_report(today()).await.unwrap(),
                _ => server.result_summary(today()).await.unwrap(),
            };
            let accounts = mock
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.url.path() == "/accounts")
                .unwrap_or_else(|| panic!("{report} must fetch the chart"));
            let query = accounts.url.query().unwrap_or_default().to_string();
            assert!(query.contains("limit=500"), "{report}: {query}");
            assert!(query.contains("page=1"), "{report}: {query}");
        }
    }
}
