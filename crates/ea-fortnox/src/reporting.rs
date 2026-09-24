//! Reporting: income-statement, VAT, cash, and invoice summaries computed
//! over Fortnox account and invoice rows.
//!
//! Ported from `reporting.ts`, eighty-eight lines with four test cases,
//! translated verbatim below and then extended.
//!
//! # The crux: what happens to an account `class_of` rejects
//!
//! These functions run over the **live Fortnox chart of accounts** for a real
//! company, not over a curated list. Task 1 made [`class_of`] stricter than
//! upstream — exactly four ASCII digits, classes 1–8 only — so a class-9
//! internal/statistical account, or any malformed account number the live
//! chart happens to contain, is an `Err` from `class_of` where upstream's
//! `Number(accountNumber.trim()[0])` would silently produce `NaN` or a
//! wrong-but-quiet digit.
//!
//! If [`summarise_result`] propagated that `Err`, one unrecognised account
//! anywhere in the chart would fail the whole call and blank the owner's
//! entire income statement — worse than upstream, which just drops that one
//! account out of every classed sum (`NaN >= 3` is `false`, so
//! `isResultAccount` and every class-membership check upstream performs
//! quietly answers "no" for it; `reporting.ts:41`'s
//! `` BAS_CLASS_LABELS[c] ?? `Klass ${c}` `` fallback exists for the same
//! reason, in case one such account is ever visible enough to need a label).
//!
//! So none of the functions in this module return `Result`. An account
//! `class_of` rejects is treated exactly as upstream treats a `NaN` class:
//! excluded from every class-based sum it would otherwise contribute to,
//! never causing an error and never blanking anything else in the summary.
//! [`summarise_result`] and its `by_class` breakdown achieve this by using
//! `class_of(..).ok()` and dropping the `None`s; [`summarise_vat`] and
//! [`cash_accounts`] never call `class_of` at all — matching upstream, which
//! tests VAT and cash membership with a regex/prefix on the string, not
//! through `basClass` — so they cannot be affected by this question in the
//! first place.

use rust_decimal::Decimal;
use serde_json::Value;

use crate::domain::bas::{class_label, class_of, is_input_vat, is_output_vat};

/// One row of the Fortnox chart of accounts: a BAS account number and its
/// balance in kronor.
///
/// `balance` is already a parsed [`Decimal`] here — turning a raw Fortnox
/// `Balance` field (which can arrive as a JSON number or a JSON string, see
/// [`sum_invoice_balances`]) into kronor is the caller's job, the same
/// division of labour upstream keeps between fetching and summarising.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub number: String,
    pub balance: Decimal,
}

/// One row of [`ResultSummary::by_class`]: a BAS class, its label, and the
/// summed balance of every result account in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassBalance {
    pub class: u8,
    pub label: &'static str,
    pub balance: Decimal,
}

/// The three shapes `netResult` can take, named the way upstream names them
/// rather than translated to English — this is Swedish bookkeeping vocabulary
/// a report's reader expects.
///
/// `Förlust` cannot be a Rust identifier written as an enum variant name
/// without relying on non-ASCII identifier support, so the variant is named
/// in ASCII and [`Outcome::as_str`] carries the accented spelling; compare
/// against `as_str()`, not `{:?}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Vinst,
    Forlust,
    Noll,
}

impl Outcome {
    /// The label exactly as upstream's `'Vinst' | 'Förlust' | 'Noll'` union
    /// spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Vinst => "Vinst",
            Outcome::Forlust => "Förlust",
            Outcome::Noll => "Noll",
        }
    }
}

/// Income-statement bottom line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    /// `-(sum of class-3 balances)`. Revenue accounts carry credit (negative)
    /// balances in BAS; sign-flipped so revenue reads positive.
    pub revenue: Decimal,
    /// Sum of class 4–7 balances (material, external costs both classes,
    /// personnel). Cost accounts carry debit (positive) balances already.
    pub operating_costs: Decimal,
    /// `-(sum of class-8 balances)`, sign-flipped the same way as revenue.
    pub financial_items: Decimal,
    /// `revenue - operating_costs + financial_items`.
    pub net_result: Decimal,
    pub outcome: Outcome,
    /// One row per class 3–8 that has a nonzero balance, in class order.
    pub by_class: Vec<ClassBalance>,
}

/// Income-statement bottom line. BAS: net profit = -(sum of result-account
/// balances).
///
/// An account [`class_of`] rejects (empty, malformed, or class 9) is silently
/// excluded from every sum here — see the module doc for why. It is never in
/// `by_class` either, since that iterates the fixed classes 3–8, each label
/// supplied by [`class_label`], which is `Some` for all of them.
pub fn summarise_result(accounts: &[AccountRow]) -> ResultSummary {
    // Every account paired with its class, silently dropping the ones
    // `class_of` rejects. This single pass stands in for both upstream's
    // `isResultAccount` pre-filter (classes 3-8 are the only ones any of the
    // sums below select) and its per-class `basClass(...) === c` checks.
    let classified: Vec<(u8, Decimal)> = accounts
        .iter()
        .filter_map(|a| class_of(&a.number).ok().map(|c| (c, a.balance)))
        .collect();

    let sum_classes = |classes: &[u8]| -> Decimal {
        classified
            .iter()
            .filter(|(c, _)| classes.contains(c))
            .fold(Decimal::ZERO, |acc, (_, bal)| acc + bal)
    };

    let revenue = -sum_classes(&[3]);
    let operating_costs = sum_classes(&[4, 5, 6, 7]);
    let financial_items = -sum_classes(&[8]);
    let net_result = revenue - operating_costs + financial_items;

    let by_class: Vec<ClassBalance> = [3u8, 4, 5, 6, 7, 8]
        .into_iter()
        .map(|class| ClassBalance {
            class,
            label: class_label(class).expect("classes 3-8 all have a label"),
            balance: sum_classes(&[class]),
        })
        .filter(|row| !row.balance.is_zero())
        .collect();

    let outcome = if net_result > Decimal::ZERO {
        Outcome::Vinst
    } else if net_result < Decimal::ZERO {
        Outcome::Forlust
    } else {
        Outcome::Noll
    };

    ResultSummary {
        revenue,
        operating_costs,
        financial_items,
        net_result,
        outcome,
        by_class,
    }
}

/// Net moms position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VatSummary {
    pub output_vat: Decimal,
    pub input_vat: Decimal,
    pub net_vat_to_pay: Decimal,
}

/// Net moms position. Output accounts carry credit balances (negative).
///
/// Unlike [`summarise_result`], this never calls [`class_of`] at all —
/// [`is_output_vat`] and [`is_input_vat`] test the account number directly
/// (the 26xx pattern), exactly as `isOutputVatAccount`/`isInputVatAccount` do
/// upstream, so an account `class_of` would reject simply fails both
/// predicates and drops out, with no error path to consider.
pub fn summarise_vat(accounts: &[AccountRow]) -> VatSummary {
    let sum_where = |pred: fn(&str) -> bool| -> Decimal {
        accounts
            .iter()
            .filter(|a| pred(&a.number))
            .fold(Decimal::ZERO, |acc, a| acc + a.balance)
    };

    let output_vat = -sum_where(is_output_vat);
    let input_vat = sum_where(is_input_vat);
    VatSummary {
        output_vat,
        input_vat,
        net_vat_to_pay: output_vat - input_vat,
    }
}

/// One bank/cash account and its balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashAccount {
    pub account: String,
    pub balance: Decimal,
}

/// Bank/cash accounts (BAS 19xx) with balances, in the order they appear in
/// `accounts`.
///
/// A plain prefix check on the string, exactly like upstream's
/// `String(a.Number).startsWith('19')` — no [`class_of`] call, so again no
/// error path.
pub fn cash_accounts(accounts: &[AccountRow]) -> Vec<CashAccount> {
    accounts
        .iter()
        .filter(|a| a.number.starts_with("19"))
        .map(|a| CashAccount {
            account: a.number.clone(),
            balance: a.balance,
        })
        .collect()
}

/// One row of the Fortnox invoice list, the slice [`sum_invoice_balances`]
/// needs.
///
/// `balance` and `total` are raw JSON [`Value`]s rather than [`Decimal`]s
/// because the live Fortnox API is observed to emit `Balance` as either a
/// JSON number or a JSON string for the same field — upstream's
/// `Number(inv.Balance)` tolerates both by coercion, and this type preserves
/// that tolerance instead of forcing the caller to normalise first. `None`
/// means the field was absent or JSON `null` (matching upstream's
/// `inv.Balance != null` check, which is true for both `undefined` and
/// `null`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvoiceRow {
    pub balance: Option<Value>,
    pub total: Option<Value>,
}

/// Totals over a set of invoices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvoiceTotals {
    pub count: usize,
    pub total: Decimal,
}

/// Coerce a JSON `Balance`/`Total` value to kronor, the way
/// `Number(value)` does upstream for the two shapes the live API actually
/// sends.
///
/// A value that is neither a JSON number nor a JSON string parseable as a
/// decimal (upstream: `Number(...)` produces `NaN`) is treated as zero rather
/// than propagated as an error. This is a deliberate departure from
/// upstream's behaviour, not a translation of it: `NaN` upstream poisons the
/// running sum for every invoice after it (`NaN + x` is `NaN`), silently
/// blanking the whole total. Treating the one bad row as zero instead keeps
/// every other invoice's contribution intact, which is the same
/// graceful-degradation choice made for an unclassifiable account number
/// above, applied to the one other place this module meets untrusted input.
fn coerce_amount(value: &Value) -> Decimal {
    match value {
        Value::Number(n) => n.to_string().parse::<Decimal>().unwrap_or(Decimal::ZERO),
        Value::String(s) => s.trim().parse::<Decimal>().unwrap_or(Decimal::ZERO),
        _ => Decimal::ZERO,
    }
}

/// Sum outstanding invoice amounts (prefer `Balance`, fall back to `Total`).
///
/// Matches upstream exactly: `Balance` wins whenever it is present — even
/// when it is `0`, which must **not** fall back to `Total` — and only an
/// absent (or JSON-`null`) `Balance` falls back to `Total`, itself treated as
/// zero when absent.
pub fn sum_invoice_balances(invoices: &[InvoiceRow]) -> InvoiceTotals {
    let total = invoices.iter().fold(Decimal::ZERO, |acc, inv| {
        // `Some(Value::Null)` and `None` both mean "no Balance", matching
        // upstream's `inv.Balance != null`, which is true for `undefined` and
        // `null` alike.
        let amount = match &inv.balance {
            Some(Value::Null) | None => inv
                .total
                .as_ref()
                .map(coerce_amount)
                .unwrap_or(Decimal::ZERO),
            Some(v) => coerce_amount(v),
        };
        acc + amount
    });

    InvoiceTotals {
        count: invoices.len(),
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test literal parses")
    }

    fn row(number: &str, balance: &str) -> AccountRow {
        AccountRow {
            number: number.to_string(),
            balance: d(balance),
        }
    }

    /// The upstream test fixture, `reporting.test.ts`'s `accounts` array,
    /// translated verbatim.
    fn upstream_accounts() -> Vec<AccountRow> {
        vec![
            row("1930", "50000"),  // Företagskonto
            row("1910", "1000"),   // Kassa
            row("3001", "-80000"), // Försäljning — revenue: credit balance
            row("5410", "20000"),  // Förbrukn. — cost: debit balance
            row("2610", "-20000"), // Utg moms 25 — output VAT: credit
            row("2640", "5000"),   // Ing moms — input VAT: debit
        ]
    }

    // ---- the four cases from reporting.test.ts, verbatim -------------------

    #[test]
    fn cash_accounts_selects_19xx_accounts_with_their_balances() {
        assert_eq!(
            cash_accounts(&upstream_accounts()),
            vec![
                CashAccount {
                    account: "1930".to_string(),
                    balance: d("50000"),
                },
                CashAccount {
                    account: "1910".to_string(),
                    balance: d("1000"),
                },
            ]
        );
    }

    #[test]
    fn summarise_result_computes_revenue_costs_and_net_result() {
        let r = summarise_result(&upstream_accounts());
        assert_eq!(r.revenue, d("80000"));
        assert_eq!(r.operating_costs, d("20000"));
        assert_eq!(r.net_result, d("60000"));
        assert_eq!(r.outcome, Outcome::Vinst);
        assert_eq!(r.outcome.as_str(), "Vinst");
    }

    #[test]
    fn summarise_vat_flips_output_sign_and_nets_against_input() {
        let v = summarise_vat(&upstream_accounts());
        assert_eq!(
            v,
            VatSummary {
                output_vat: d("20000"),
                input_vat: d("5000"),
                net_vat_to_pay: d("15000"),
            }
        );
    }

    #[test]
    fn sum_invoice_balances_sums_balance_falling_back_to_total_with_a_count() {
        let invoices = vec![
            InvoiceRow {
                balance: Some(json!(1250)),
                total: None,
            },
            InvoiceRow {
                balance: None,
                total: Some(json!(500)),
            },
            InvoiceRow {
                balance: Some(json!(0)),
                total: Some(json!(999)),
            },
        ];
        assert_eq!(
            sum_invoice_balances(&invoices),
            InvoiceTotals {
                count: 3,
                total: d("1750"),
            }
        );
    }

    // ---- beyond the upstream four -------------------------------------------

    #[test]
    fn an_empty_account_list_yields_zeros_not_a_panic() {
        let r = summarise_result(&[]);
        assert_eq!(r.revenue, Decimal::ZERO);
        assert_eq!(r.operating_costs, Decimal::ZERO);
        assert_eq!(r.financial_items, Decimal::ZERO);
        assert_eq!(r.net_result, Decimal::ZERO);
        assert_eq!(r.outcome, Outcome::Noll);
        assert!(r.by_class.is_empty());

        let v = summarise_vat(&[]);
        assert_eq!(
            v,
            VatSummary {
                output_vat: Decimal::ZERO,
                input_vat: Decimal::ZERO,
                net_vat_to_pay: Decimal::ZERO,
            }
        );

        assert!(cash_accounts(&[]).is_empty());
    }

    #[test]
    fn an_empty_invoice_list_yields_zero_total_and_zero_count() {
        assert_eq!(
            sum_invoice_balances(&[]),
            InvoiceTotals {
                count: 0,
                total: Decimal::ZERO,
            }
        );
    }

    #[test]
    fn a_balance_arriving_as_a_json_string_is_tolerated_like_a_number() {
        let invoices = vec![
            InvoiceRow {
                balance: Some(json!("1250.50")),
                total: None,
            },
            InvoiceRow {
                balance: Some(json!(749.50)),
                total: None,
            },
        ];
        assert_eq!(
            sum_invoice_balances(&invoices),
            InvoiceTotals {
                count: 2,
                total: d("2000.00"),
            }
        );
    }

    #[test]
    fn a_missing_balance_falls_back_to_total_and_a_missing_total_too_is_zero() {
        let invoices = vec![
            InvoiceRow {
                balance: None,
                total: Some(json!(300)),
            },
            InvoiceRow {
                balance: None,
                total: None,
            },
            InvoiceRow {
                balance: Some(Value::Null),
                total: Some(json!(50)),
            },
        ];
        assert_eq!(
            sum_invoice_balances(&invoices),
            InvoiceTotals {
                count: 3,
                total: d("350"),
            }
        );
    }

    #[test]
    fn a_json_null_balance_is_treated_as_absent_not_as_zero_that_wins() {
        // A Balance explicitly present as 0 must win over Total (see the
        // upstream-verbatim test above); a Balance that is JSON null must
        // behave like an absent field and fall back to Total instead.
        let with_zero = InvoiceRow {
            balance: Some(json!(0)),
            total: Some(json!(999)),
        };
        let with_null = InvoiceRow {
            balance: Some(Value::Null),
            total: Some(json!(999)),
        };
        assert_eq!(sum_invoice_balances(&[with_zero]).total, Decimal::ZERO);
        assert_eq!(sum_invoice_balances(&[with_null]).total, d("999"));
    }

    #[test]
    fn an_unclassifiable_account_is_dropped_from_the_result_summary_not_an_error() {
        // A class-9 internal account and a malformed one both sit alongside
        // well-formed accounts. Neither should change the summary computed
        // from the good ones, and neither should make summarise_result panic
        // or need a Result to call.
        let accounts = vec![
            row("3001", "-80000"), // revenue
            row("5410", "20000"),  // cost
            row("9000", "12345"),  // class 9: internal/statistical, out of scope
            row("abcd", "999"),    // malformed
            row("", "1"),          // empty
        ];
        let r = summarise_result(&accounts);
        assert_eq!(r.revenue, d("80000"));
        assert_eq!(r.operating_costs, d("20000"));
        assert_eq!(r.net_result, d("60000"));
        assert_eq!(r.outcome, Outcome::Vinst);
        // Class 9 has no row in by_class at all — class_label only covers 1-8.
        assert!(r.by_class.iter().all(|c| c.class != 9));
    }

    #[test]
    fn an_unclassifiable_account_is_also_absent_from_cash_and_vat_summaries() {
        let accounts = vec![
            row("1930", "50000"),
            row("9000", "1"), // must not be mistaken for a 19xx cash account
            row("2610", "-20000"),
            row("abcd", "1"),
        ];
        assert_eq!(cash_accounts(&accounts).len(), 1);
        let v = summarise_vat(&accounts);
        assert_eq!(v.output_vat, d("20000"));
        assert_eq!(v.input_vat, Decimal::ZERO);
    }

    #[test]
    fn by_class_omits_zero_balance_classes_and_orders_by_class() {
        let accounts = vec![row("3001", "-1000"), row("7010", "1000")];
        let r = summarise_result(&accounts);
        assert_eq!(
            r.by_class,
            vec![
                ClassBalance {
                    class: 3,
                    label: "Intäkter",
                    balance: d("-1000"),
                },
                ClassBalance {
                    class: 7,
                    label: "Personalkostnader",
                    balance: d("1000"),
                },
            ]
        );
    }
}
