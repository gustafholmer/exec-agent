//! The previews: what a write *would* book, rendered as text, posting nothing.
//!
//! # Why these tools exist
//!
//! Upstream has no preview tools. It has a `confirm` flag on each write: call
//! without it and the tool renders this same text; call again with
//! `confirm: true` and it posts. That mechanism is removed here (see the crate
//! docs), and removing it without replacing the text would have thrown away
//! the useful half. So the renderer stays and becomes three read-only tools.
//!
//! # The text is upstream's, deliberately
//!
//! [`preview_text`] is a port of `writeTools.ts`'s `previewText`, line by line
//! and space by space. It is not a rewrite and should not become one. The
//! reason is that this string is what the daemon puts in an action's
//! `preview` field, which is what a person reads in Telegram — at a bus stop,
//! on a phone, with ten seconds of attention — before tapping approve on a
//! voucher that will appear in their company's books and in front of their
//! accountant. Upstream's version has been read by a real person approving
//! real vouchers; a nicer-looking rewrite would not have been.
//!
//! ## The one line that changed, and why it had to
//!
//! Upstream's first line is:
//!
//! ```text
//! PREVIEW — nothing posted. Re-run with confirm:true to book.
//! ```
//!
//! The second sentence is an instruction to use a parameter that no longer
//! exists. Leaving it would tell a model to retry with `confirm:true`, which
//! `rmcp` would reject as an unknown argument, and would tell a person that
//! this tool can book — when in this system nothing books without their tap.
//! [`PREVIEW_HEADER`] replaces that sentence and only that sentence; every
//! other character of the rendered text, including the two-space gutters and
//! the parenthesised row notes, is upstream's.
//!
//! ## Numbers
//!
//! Amounts are rendered from the payload's own `Debit`/`Credit`, through
//! `f64`'s [`std::fmt::Display`], which prints `1000` for `1000.0` and
//! `250.5` for `250.5` — the same strings JavaScript's template literal
//! produces for the same values. The two only diverge past `1e21`, where
//! JavaScript switches to exponent notation and Rust does not; no voucher line
//! reaches it.

use ea_fortnox::domain::voucher::build_payload;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde_json::Value;

use super::write::{ExpenseArgs, ReconcileArgs, RecordVoucherArgs};
use super::{render_domain, FortnoxServer};

/// The first line of every preview.
///
/// Upstream: `PREVIEW — nothing posted. Re-run with confirm:true to book.`
/// The first sentence is kept verbatim; the second names a parameter this
/// connector does not have and is replaced by the mechanism that does apply.
/// See the module docs.
pub const PREVIEW_HEADER: &str =
    "PREVIEW — nothing posted. Booking needs propose_action and a human approval.";

/// A voucher payload as the text a person approves.
///
/// Ported from `writeTools.ts`:
///
/// ```js
/// const rows = payload.Voucher.VoucherRows.VoucherRow
///   .map((r) => `  ${r.Account}  debit ${r.Debit}  credit ${r.Credit}${r.TransactionInformation ? `  (${r.TransactionInformation})` : ''}`)
///   .join('\n');
/// return `PREVIEW — …\n${label}\nSeries ${…VoucherSeries}  Date ${…TransactionDate}  "${…Description}"\n${rows}`;
/// ```
///
/// Two-space separators throughout, a leading two-space indent on each row,
/// the description in double quotes, and the row note in parentheses only when
/// it is present and non-empty — `TransactionInformation` is omitted from the
/// payload entirely in that case, so the `?:` and this `if let` agree.
pub fn preview_text(label: &str, payload: &Value) -> String {
    let voucher = &payload["Voucher"];
    let rows: Vec<String> = voucher["VoucherRows"]["VoucherRow"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|row| {
            let note = match row.get("TransactionInformation").and_then(Value::as_str) {
                Some(info) if !info.is_empty() => format!("  ({info})"),
                _ => String::new(),
            };
            format!(
                "  {}  debit {}  credit {}{note}",
                text_of(row.get("Account")),
                amount_of(row.get("Debit")),
                amount_of(row.get("Credit")),
            )
        })
        .collect();

    format!(
        "{PREVIEW_HEADER}\n{label}\nSeries {}  Date {}  \"{}\"\n{}",
        text_of(voucher.get("VoucherSeries")),
        text_of(voucher.get("TransactionDate")),
        text_of(voucher.get("Description")),
        rows.join("\n"),
    )
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// A payload amount as JavaScript would interpolate it: `1000`, not `1000.0`.
fn amount_of(value: Option<&Value>) -> String {
    match value.and_then(Value::as_f64) {
        Some(amount) => format!("{amount}"),
        None => text_of(value),
    }
}

#[tool_router(router = preview_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "Preview a general manual voucher built from explicit debit/credit \
                       lines, exactly as record_voucher would post it. Read-only: renders \
                       the voucher and posts nothing. Unbalanced lines are an error naming \
                       the discrepancy, not a preview. Free to call — show the person its \
                       text, then go through propose_action to book it."
    )]
    pub async fn preview_voucher(
        &self,
        Parameters(args): Parameters<RecordVoucherArgs>,
    ) -> Result<String, String> {
        let input = args.to_input()?;
        let payload = build_payload(&input).map_err(render_domain)?;
        Ok(preview_text("Manual voucher", &payload))
    }

    #[tool(
        description = "Preview booking a supplier expense/receipt from a VAT-inclusive gross \
                       amount — the net to the expense account, the input VAT to 2640, the \
                       gross credited to the payment account — exactly as record_expense \
                       would post it. Read-only: renders the voucher and posts nothing. Free \
                       to call — show the person its text, then go through propose_action to \
                       book it."
    )]
    pub async fn preview_expense(
        &self,
        Parameters(args): Parameters<ExpenseArgs>,
    ) -> Result<String, String> {
        let label = args.label();
        let payload = build_payload(&args.to_input()?).map_err(render_domain)?;
        Ok(preview_text(&label, &payload))
    }

    #[tool(
        description = "Preview booking a bank payment against a receivable or payable — a \
                       customer paying an invoice, or a supplier being paid — exactly as \
                       reconcile_payment would post it. Read-only: renders the voucher and \
                       posts nothing. Free to call — show the person its text, then go \
                       through propose_action to book it."
    )]
    pub async fn preview_reconciliation(
        &self,
        Parameters(args): Parameters<ReconcileArgs>,
    ) -> Result<String, String> {
        let label = args.label();
        let payload = build_payload(&args.to_input()?).map_err(render_domain)?;
        Ok(preview_text(&label, &payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::test_support::{line, unconfigured};
    use crate::tools::write::Direction;

    fn expense_args() -> ExpenseArgs {
        ExpenseArgs {
            gross_amount: 1250.0,
            vat_rate: 25,
            expense_account: "5410".to_string(),
            payment_account: "1930".to_string(),
            transaction_date: "2026-05-31".to_string(),
            description: "Dator".to_string(),
            series: None,
        }
    }

    /// Upstream's `record_expense` preview test, plus the figures the brief
    /// names: a 1,250 kr receipt at 25% is 1000 net and 250 VAT.
    #[tokio::test]
    async fn preview_expense_names_the_three_accounts_and_splits_the_gross() {
        let server = unconfigured();
        let text = server
            .preview_expense(Parameters(expense_args()))
            .await
            .expect("a preview needs no credentials");

        assert!(text.contains("5410"), "{text}");
        assert!(text.contains("2640"), "{text}");
        assert!(text.contains("1930"), "{text}");
        assert!(text.contains("1000"), "{text}");
        assert!(text.contains("250"), "{text}");
        assert!(text.contains("1250"), "{text}");
        assert!(text.starts_with(PREVIEW_HEADER), "{text}");
    }

    /// The exact string, because the exact string is the product. Everything
    /// after the first line is upstream's `previewText` character for
    /// character.
    #[tokio::test]
    async fn the_rendered_preview_is_upstreams_layout() {
        let server = unconfigured();
        let text = server
            .preview_expense(Parameters(expense_args()))
            .await
            .unwrap();
        assert_eq!(
            text,
            format!(
                "{PREVIEW_HEADER}\n\
                 Expense 1250 kr incl. 25% VAT\n\
                 Series A  Date 2026-05-31  \"Dator\"\n  \
                 5410  debit 1000  credit 0  (Dator)\n  \
                 2640  debit 250  credit 0  (Ingående moms)\n  \
                 1930  debit 0  credit 1250  (Dator)"
            )
        );
    }

    /// The header must not invite a model to retry with the parameter this
    /// connector deliberately does not have.
    #[test]
    fn the_header_does_not_mention_confirm() {
        assert!(!PREVIEW_HEADER.to_lowercase().contains("confirm"));
        assert!(PREVIEW_HEADER.starts_with("PREVIEW — nothing posted."));
    }

    #[tokio::test]
    async fn preview_voucher_renders_balanced_lines_with_their_notes() {
        let server = unconfigured();
        let text = server
            .preview_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "Dator".to_string(),
                lines: vec![
                    line("5410", 1000.0, 0.0, Some("Netto")),
                    line("2640", 250.0, 0.0, None),
                    line("1930", 0.0, 1250.0, None),
                ],
            }))
            .await
            .unwrap();

        assert_eq!(
            text,
            format!(
                "{PREVIEW_HEADER}\n\
                 Manual voucher\n\
                 Series A  Date 2026-05-31  \"Dator\"\n  \
                 5410  debit 1000  credit 0  (Netto)\n  \
                 2640  debit 250  credit 0\n  \
                 1930  debit 0  credit 1250"
            )
        );
    }

    /// The brief's requirement: an unbalanced preview is an error that names
    /// the discrepancy, not a preview of a voucher Fortnox would refuse.
    #[tokio::test]
    async fn preview_voucher_on_unbalanced_lines_names_the_discrepancy() {
        let server = unconfigured();
        let err = server
            .preview_voucher(Parameters(RecordVoucherArgs {
                series: "A".to_string(),
                transaction_date: "2026-05-31".to_string(),
                description: "x".to_string(),
                lines: vec![
                    line("5410", 1000.0, 0.0, None),
                    line("1930", 0.0, 900.0, None),
                ],
            }))
            .await
            .expect_err("unbalanced lines cannot be previewed");

        assert!(err.contains("does not balance"), "{err}");
        assert!(err.contains("100"), "the difference must be named: {err}");
        assert!(!err.contains("PREVIEW"), "{err}");
    }

    #[tokio::test]
    async fn preview_reconciliation_puts_the_bank_on_the_right_side() {
        let server = unconfigured();
        let incoming = server
            .preview_reconciliation(Parameters(ReconcileArgs {
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
        assert!(
            incoming.contains("Reconcile incoming 5000 kr"),
            "{incoming}"
        );
        assert!(
            incoming.contains("  1930  debit 5000  credit 0"),
            "{incoming}"
        );
        assert!(
            incoming.contains("  1510  debit 0  credit 5000"),
            "{incoming}"
        );

        let outgoing = server
            .preview_reconciliation(Parameters(ReconcileArgs {
                amount: 5000.0,
                bank_account: None,
                counter_account: "2440".to_string(),
                direction: Direction::Outgoing,
                transaction_date: "2026-05-31".to_string(),
                description: "Vi betalar".to_string(),
                series: None,
            }))
            .await
            .unwrap();
        assert!(
            outgoing.contains("  2440  debit 5000  credit 0"),
            "{outgoing}"
        );
        assert!(
            outgoing.contains("  1930  debit 0  credit 5000"),
            "{outgoing}"
        );
    }

    /// An unsupported rate is refused by the arithmetic, before any payload
    /// exists. The write tool's version of this test also proves no HTTP call
    /// is made.
    #[tokio::test]
    async fn preview_expense_refuses_an_unsupported_vat_rate() {
        let server = unconfigured();
        let mut args = expense_args();
        args.vat_rate = 20;
        let err = server
            .preview_expense(Parameters(args))
            .await
            .expect_err("20% is not a Swedish rate");
        assert!(err.contains("unsupported VAT rate 20%"), "{err}");
    }

    /// Öre, not a float artifact: 1.25 kr at 25% is 1.00 + 0.25.
    #[tokio::test]
    async fn fractional_kronor_render_with_their_decimals() {
        let server = unconfigured();
        let mut args = expense_args();
        args.gross_amount = 1.25;
        let text = server.preview_expense(Parameters(args)).await.unwrap();
        assert!(text.contains("debit 1  credit 0"), "{text}");
        assert!(text.contains("debit 0.25  credit 0"), "{text}");
        assert!(text.contains("credit 1.25"), "{text}");
    }

    /// A nonsense amount is refused before anything is built.
    #[tokio::test]
    async fn a_non_finite_amount_is_refused() {
        let mut args = expense_args();
        args.gross_amount = f64::NAN;
        let err = unconfigured()
            .preview_expense(Parameters(args))
            .await
            .expect_err("NaN kronor");
        assert!(err.contains("finite amount in kronor"), "{err}");
    }
}
