//! Expense posting: the balanced voucher for a supplier expense/receipt.
//!
//! Ported from `posting.ts`, which is forty-three lines and carries two test
//! cases. It is the first caller of [`split_gross`], so it is also the first
//! place a caller can feed that function an amount finer than an öre (Task 3's
//! `assert_balanced` doc comment named this as reachable but not yet reached);
//! this module always passes it öre-granular amounts, so that path is not
//! exercised here.

use anyhow::{Context, Result};
use rust_decimal::Decimal;

use super::bas::class_of;
use super::moms::split_gross;
use super::voucher::{assert_balanced, BuildVoucherInput, VoucherLine};

/// Ingående moms — the account the input-VAT line is booked to.
pub const INPUT_VAT_ACCOUNT: &str = "2640";

/// Everything needed to build a supplier expense/receipt voucher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpenseInput {
    /// VAT-inclusive total paid, in kronor.
    pub gross: Decimal,
    /// 25, 12, 6, or 0.
    pub vat_rate: u8,
    /// BAS expense account to debit, e.g. `"5410"`.
    pub expense_account: String,
    /// Credited: bank (`"1930"`) or supplier debt (`"2440"`).
    pub payment_account: String,
    /// `YYYY-MM-DD`.
    pub transaction_date: String,
    pub description: String,
    /// Voucher series, e.g. `"A"`.
    pub series: String,
}

/// Build the balanced voucher input for a supplier expense/receipt.
///
/// # Lines
///
/// Two or three, depending on whether the split carries any VAT:
///
/// * debit the expense account for the net;
/// * debit [`INPUT_VAT_ACCOUNT`] for the VAT — **only when the split's `vat`
///   is nonzero**. Upstream's condition is `split.vat > 0`, checked on the
///   computed split, not on `vatRate` directly; a zero-value `2640` row would
///   clutter the ledger for no reason, and upstream does not emit one. This
///   also covers the case where `vat_rate` is nonzero but the gross is too
///   small to carry any VAT at all (e.g. one öre at 25%: [`split_gross`]
///   places the whole amount in `net`).
/// * credit the payment account for the gross.
///
/// # Errors
///
/// * an invalid `expense_account` — not a well-formed four-digit BAS account.
///   Upstream performs no such check and will happily book to `"abc"`; this is
///   a deliberate strengthening in the style of Task 3's line-level checks,
///   not a behaviour observed in the TypeScript.
/// * an invalid `payment_account`, checked the same way and for the same
///   reason. The two fields play symmetric roles — one is debited, the other
///   credited, but both are BAS account numbers a caller could equally
///   mistype — so validating one and not the other was an asymmetry with no
///   justification, not a deliberate choice. In practice `payment_account` is
///   always `"1930"` (bank) or `"2440"` (supplier debt), both class 1–2 and
///   comfortably inside what [`class_of`] accepts.
/// * an unsupported `vat_rate`, via [`split_gross`].
/// * a negative `gross`, via [`split_gross`].
pub fn build_expense_voucher(input: &ExpenseInput) -> Result<BuildVoucherInput> {
    class_of(&input.expense_account)
        .with_context(|| format!("invalid expense account {:?}", input.expense_account))?;
    class_of(&input.payment_account)
        .with_context(|| format!("invalid payment account {:?}", input.payment_account))?;

    let split = split_gross(input.gross, input.vat_rate)?;

    let mut lines = vec![VoucherLine {
        account: input.expense_account.clone(),
        debit: split.net,
        credit: Decimal::ZERO,
        info: Some(input.description.clone()),
    }];

    if !split.vat.is_zero() {
        lines.push(VoucherLine {
            account: INPUT_VAT_ACCOUNT.to_string(),
            debit: split.vat,
            credit: Decimal::ZERO,
            info: Some("Ingående moms".to_string()),
        });
    }

    lines.push(VoucherLine {
        account: input.payment_account.clone(),
        debit: Decimal::ZERO,
        credit: split.gross,
        info: Some(input.description.clone()),
    });

    assert_balanced(&lines)?;

    Ok(BuildVoucherInput {
        series: input.series.clone(),
        transaction_date: input.transaction_date.clone(),
        description: input.description.clone(),
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().expect("test literal is a valid decimal")
    }

    fn expense(gross: &str, vat_rate: u8) -> ExpenseInput {
        ExpenseInput {
            gross: dec(gross),
            vat_rate,
            expense_account: "5410".to_string(),
            payment_account: "1930".to_string(),
            transaction_date: "2026-06-15".to_string(),
            description: "Kontorsmaterial".to_string(),
            series: "A".to_string(),
        }
    }

    // ---- the two cases from posting.test.ts, verbatim ---------------------

    /// `posting.test.ts`: "splits a 25% gross into net expense + input VAT,
    /// balanced against the payment account".
    #[test]
    fn splits_a_twenty_five_percent_gross_into_net_expense_and_input_vat() {
        let voucher = build_expense_voucher(&expense("1250", 25)).unwrap();
        assert_eq!(voucher.series, "A");
        assert_eq!(voucher.transaction_date, "2026-06-15");
        assert_eq!(
            voucher.lines,
            vec![
                VoucherLine {
                    account: "5410".to_string(),
                    debit: dec("1000"),
                    credit: Decimal::ZERO,
                    info: Some("Kontorsmaterial".to_string()),
                },
                VoucherLine {
                    account: INPUT_VAT_ACCOUNT.to_string(),
                    debit: dec("250"),
                    credit: Decimal::ZERO,
                    info: Some("Ingående moms".to_string()),
                },
                VoucherLine {
                    account: "1930".to_string(),
                    debit: Decimal::ZERO,
                    credit: dec("1250"),
                    info: Some("Kontorsmaterial".to_string()),
                },
            ]
        );
        assert_balanced(&voucher.lines).expect("the built voucher balances");
    }

    /// `posting.test.ts`: "omits the VAT line when rate is 0".
    #[test]
    fn omits_the_vat_line_when_rate_is_zero() {
        let mut input = expense("500", 0);
        input.expense_account = "6310".to_string();
        input.description = "Försäkring".to_string();
        let voucher = build_expense_voucher(&input).unwrap();
        assert_eq!(
            voucher.lines,
            vec![
                VoucherLine {
                    account: "6310".to_string(),
                    debit: dec("500"),
                    credit: Decimal::ZERO,
                    info: Some("Försäkring".to_string()),
                },
                VoucherLine {
                    account: "1930".to_string(),
                    debit: Decimal::ZERO,
                    credit: dec("500"),
                    info: Some("Försäkring".to_string()),
                },
            ]
        );
    }

    // ---- beyond the upstream two --------------------------------------------

    /// Each supported rate produces a balanced three-line voucher, with the
    /// VAT row always present because these gross amounts are large enough to
    /// carry a nonzero VAT component at every one of them.
    #[test]
    fn each_supported_rate_produces_a_balanced_three_line_voucher() {
        for (rate, net, vat) in [
            (25u8, "1000.00", "250.00"),
            (12, "1000.00", "120.00"),
            (6, "1000.00", "60.00"),
        ] {
            let gross = (dec(net) + dec(vat)).to_string();
            let voucher = build_expense_voucher(&expense(&gross, rate)).unwrap();
            assert_eq!(voucher.lines.len(), 3, "rate {rate}%");
            assert_eq!(voucher.lines[0].debit, dec(net), "rate {rate}% net");
            assert_eq!(voucher.lines[1].account, INPUT_VAT_ACCOUNT, "rate {rate}%");
            assert_eq!(voucher.lines[1].debit, dec(vat), "rate {rate}% vat");
            assert_eq!(voucher.lines[2].credit, dec(net) + dec(vat), "rate {rate}%");
            assert_balanced(&voucher.lines).expect("balanced");
        }
    }

    /// An unsupported rate is an error, surfaced from `split_gross`.
    #[test]
    fn an_unsupported_rate_is_an_error() {
        let err = build_expense_voucher(&expense("100", 17)).unwrap_err();
        assert!(err.to_string().contains("17"), "{err}");
    }

    /// An invalid expense account is an error, and names the account.
    #[test]
    fn an_invalid_expense_account_is_an_error() {
        for bad in ["", "abcd", "19", "0000", "9000"] {
            let mut input = expense("1250", 25);
            input.expense_account = bad.to_string();
            let err = build_expense_voucher(&input)
                .expect_err(&format!("account {bad:?} should be rejected"));
            assert!(err.to_string().contains("invalid expense account"), "{err}");
        }
    }

    /// A malformed payment account is an error, and names the account —
    /// symmetric with the expense-account check above.
    #[test]
    fn an_invalid_payment_account_is_an_error() {
        for bad in ["", "abcd", "19", "0000", "9000"] {
            let mut input = expense("1250", 25);
            input.payment_account = bad.to_string();
            let err = build_expense_voucher(&input)
                .expect_err(&format!("payment account {bad:?} should be rejected"));
            assert!(err.to_string().contains("invalid payment account"), "{err}");
        }
    }

    /// A negative gross is an error, surfaced from `split_gross`.
    #[test]
    fn a_negative_gross_is_an_error() {
        let err = build_expense_voucher(&expense("-100", 25)).unwrap_err();
        assert!(err.to_string().contains("credit note"), "{err}");
    }

    /// The 12% midpoints Task 2 identified (`gross ≡ 0.14 mod 0.28` kr) are
    /// where the net-first/vat-first rounding choice is observable. An expense
    /// is the most common way a user reaches one, so this pins that the
    /// resulting voucher still balances there.
    #[test]
    fn the_twelve_percent_midpoint_produces_a_balanced_voucher() {
        for (gross, net, vat) in [
            ("0.14", "0.13", "0.01"),
            ("0.42", "0.38", "0.04"),
            ("0.70", "0.63", "0.07"),
            ("1.26", "1.13", "0.13"),
        ] {
            let voucher = build_expense_voucher(&expense(gross, 12)).unwrap();
            assert_eq!(voucher.lines.len(), 3, "{gross} kr @ 12%");
            assert_eq!(voucher.lines[0].debit, dec(net), "{gross} kr @ 12% net");
            assert_eq!(voucher.lines[1].debit, dec(vat), "{gross} kr @ 12% vat");
            assert_balanced(&voucher.lines).expect("balanced at the midpoint");
        }
    }

    /// A gross too small to carry any VAT (one öre at 25%) omits the VAT line
    /// even though `vat_rate` is nonzero — the upstream condition is on the
    /// computed `split.vat`, not on the rate.
    #[test]
    fn a_gross_too_small_to_carry_vat_omits_the_vat_line_even_at_a_nonzero_rate() {
        let voucher = build_expense_voucher(&expense("0.01", 25)).unwrap();
        assert_eq!(voucher.lines.len(), 2);
        assert_eq!(voucher.lines[0].debit, dec("0.01"));
        assert_eq!(voucher.lines[1].credit, dec("0.01"));
    }
}
