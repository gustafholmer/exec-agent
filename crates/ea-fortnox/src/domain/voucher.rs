//! Vouchers: the double-entry body posted to Fortnox.
//!
//! Upstream this is `domain/voucher.ts` — a balance assertion and a payload
//! builder. The payload shape is reproduced exactly, because Fortnox matches
//! field names case-sensitively and a mismatch fails against the live API
//! rather than against anything a test here can see. The validation around it
//! is deliberately stricter than upstream's; each strengthening is named in
//! the doc comment of the function that performs it.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};

use super::money::{round2, to_api};

/// One row of a voucher: an account, an amount in exactly one of the two
/// columns, and an optional free-text note that Fortnox shows on the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoucherLine {
    /// BAS account number, e.g. `"1930"`.
    pub account: String,
    /// Debit amount in kronor. Zero when this is a credit line.
    pub debit: Decimal,
    /// Credit amount in kronor. Zero when this is a debit line.
    pub credit: Decimal,
    /// Row note; becomes `TransactionInformation` when present and non-empty.
    pub info: Option<String>,
}

/// Everything needed to build one voucher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildVoucherInput {
    /// Voucher series, e.g. `"A"`.
    pub series: String,
    /// Transaction date, `YYYY-MM-DD`.
    pub transaction_date: String,
    /// Voucher description.
    pub description: String,
    /// The rows. At least two, and they must balance.
    pub lines: Vec<VoucherLine>,
}

/// Check that a set of lines forms a postable voucher body.
///
/// # What is compared, and at what precision
///
/// **Each line is rounded to öre first, and the öre figures are then summed
/// and compared exactly.** Not the raw `Decimal` sums, and not upstream's
/// sum-then-round.
///
/// The reason is that [`build_payload`] posts each line through
/// [`money::to_api`](super::money::to_api), which rounds that line to öre. The
/// öre figures are therefore the numbers Fortnox actually receives and
/// actually balances, and this check asks exactly the question Fortnox will
/// ask. Comparing anything else means the local answer and the remote one can
/// disagree:
///
/// * **Full `Decimal` precision** rejects vouchers Fortnox would accept. Lines
///   of `10.004` debit against `10.00` credit post as `10.00` / `10.00` and
///   are accepted; an exact comparison of the raw amounts refuses to build
///   them.
/// * **Upstream's sum-then-round** (`round2(Σdebit) != round2(Σcredit)`)
///   accepts vouchers Fortnox rejects. Two `10.004` debit lines against a
///   `20.01` credit line sum to `20.008`, which rounds to `20.01` and looks
///   balanced — but the rows posted are `10.00`, `10.00` and `20.01`, an öre
///   apart. Sub-öre amounts are reachable: `split_gross` divides by
///   `1 + rate/100`, and a caller can pass its unrounded quotient straight
///   into a line.
///
/// For öre-granular lines — every line upstream's own tests use, and every
/// line a correctly-rounded caller produces — per-line rounding is the
/// identity and this is exactly upstream's comparison.
///
/// # Strengthenings over upstream
///
/// Upstream's `assertBalanced` checks the sums and nothing else. This also
/// rejects, before comparing anything:
///
/// * **Fewer than two lines.** One line cannot be double entry.
/// * **A line with both a debit and a credit.** Fortnox accepts such a row and
///   nets it, so the voucher can still balance while the ledger records
///   something nobody meant. Splitting it into two rows is unambiguous.
/// * **A line with neither** (including a line whose only amount is finer than
///   an öre, which posts as `0.00` / `0.00`). It occupies a row and moves no
///   money, which is how a dropped amount shows up.
/// * **A negative debit or credit.** A negative debit is a credit written in
///   the wrong column; Swedish practice reverses a posting by swapping the
///   columns, not by negating. Allowing both encodings makes the
///   one-column-only rule above unenforceable.
///
/// # Errors
///
/// The unbalanced error names both sums and the difference, in kronor to two
/// decimals — the number is what makes the voucher fixable.
pub fn assert_balanced(lines: &[VoucherLine]) -> Result<()> {
    if lines.len() < 2 {
        bail!(
            "a voucher needs at least two lines to be double entry; got {}",
            lines.len()
        );
    }

    let mut debits = Decimal::ZERO;
    let mut credits = Decimal::ZERO;

    for line in lines {
        if line.debit.is_sign_negative() && !line.debit.is_zero()
            || line.credit.is_sign_negative() && !line.credit.is_zero()
        {
            bail!(
                "line for account {} has a negative amount (debit {:.2}, credit {:.2}); \
                 reverse a posting by swapping the columns, not by negating an amount",
                line.account,
                line.debit,
                line.credit
            );
        }

        // Öre, because öre is what `build_payload` posts and what Fortnox
        // balances. See the precision note above.
        let debit = round2(line.debit);
        let credit = round2(line.credit);

        match (debit.is_zero(), credit.is_zero()) {
            (false, false) => bail!(
                "line for account {} has both a debit ({:.2}) and a credit ({:.2}); \
                 a voucher line belongs in one column, so post it as two lines",
                line.account,
                debit,
                credit
            ),
            (true, true) => bail!(
                "line for account {} has neither a debit nor a credit in öre \
                 (debit {}, credit {}); it would post as 0.00 / 0.00",
                line.account,
                line.debit,
                line.credit
            ),
            _ => {}
        }

        debits += debit;
        credits += credit;
    }

    if debits != credits {
        let difference = (debits - credits).abs();
        if debits > credits {
            bail!(
                "voucher does not balance: debits {debits:.2} exceed credits {credits:.2} \
                 by {difference:.2}"
            );
        }
        bail!(
            "voucher does not balance: credits {credits:.2} exceed debits {debits:.2} \
             by {difference:.2}"
        );
    }

    Ok(())
}

/// Build the Fortnox voucher payload.
///
/// The shape is upstream's, field for field:
///
/// ```json
/// { "Voucher": { "VoucherSeries": …, "TransactionDate": …, "Description": …,
///   "VoucherRows": { "VoucherRow": [ { "Account": …, "Debit": …, "Credit": …,
///   "TransactionInformation": … } ] } } }
/// ```
///
/// `TransactionInformation` is omitted when `info` is absent, and also when it
/// is the empty string — the TypeScript spreads `...(l.info ? … : {})`, and
/// `""` is falsy there.
///
/// Amounts cross the boundary through
/// [`money::to_api`](super::money::to_api), so every posted figure is öre.
///
/// # Errors
///
/// The date must be exactly `YYYY-MM-DD` and a real calendar date; anything
/// else is rejected here rather than sent to Fortnox, which answers a bad date
/// with a generic rejection that says nothing about which field was wrong.
/// The lines must satisfy [`assert_balanced`].
pub fn build_payload(input: &BuildVoucherInput) -> Result<Value> {
    validate_transaction_date(&input.transaction_date)?;
    assert_balanced(&input.lines)?;

    let rows: Vec<Value> = input
        .lines
        .iter()
        .map(|line| {
            let mut row = Map::new();
            row.insert("Account".to_string(), json!(line.account));
            row.insert("Debit".to_string(), json!(to_api(line.debit)));
            row.insert("Credit".to_string(), json!(to_api(line.credit)));
            if let Some(info) = line.info.as_deref().filter(|info| !info.is_empty()) {
                row.insert("TransactionInformation".to_string(), json!(info));
            }
            Value::Object(row)
        })
        .collect();

    Ok(json!({
        "Voucher": {
            "VoucherSeries": input.series,
            "TransactionDate": input.transaction_date,
            "Description": input.description,
            "VoucherRows": {
                "VoucherRow": rows,
            },
        },
    }))
}

/// `YYYY-MM-DD`, and a date that exists.
///
/// The literal shape is checked before parsing because `chrono`'s `%m` and
/// `%d` accept one digit as well as two, so `"2026-5-31"` parses cleanly and
/// would then be serialised back out in the caller's own spelling — the wrong
/// spelling for Fortnox.
fn validate_transaction_date(date: &str) -> Result<()> {
    let bytes = date.as_bytes();
    let well_shaped = bytes.len() == 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !well_shaped {
        bail!("transaction date {date:?} is not in YYYY-MM-DD form");
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .with_context(|| format!("transaction date {date:?} is not a real calendar date"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test literal parses")
    }

    fn debit(account: &str, amount: &str) -> VoucherLine {
        VoucherLine {
            account: account.to_string(),
            debit: d(amount),
            credit: Decimal::ZERO,
            info: None,
        }
    }

    fn credit(account: &str, amount: &str) -> VoucherLine {
        VoucherLine {
            account: account.to_string(),
            debit: Decimal::ZERO,
            credit: d(amount),
            info: None,
        }
    }

    fn input(date: &str, lines: Vec<VoucherLine>) -> BuildVoucherInput {
        BuildVoucherInput {
            series: "A".to_string(),
            transaction_date: date.to_string(),
            description: "Köp av dator".to_string(),
            lines,
        }
    }

    fn balanced_lines() -> Vec<VoucherLine> {
        vec![
            debit("5410", "1000"),
            debit("2640", "250"),
            credit("1930", "1250"),
        ]
    }

    // --- the three upstream cases, translated from domain/voucher.test.ts ----

    #[test]
    fn passes_when_debits_equal_credits() {
        assert_balanced(&[
            debit("6540", "1000"),
            debit("2640", "250"),
            credit("1930", "1250"),
        ])
        .expect("balanced voucher is accepted");
    }

    #[test]
    fn errors_when_unbalanced() {
        // Upstream asserts only `/balance/i`. The exact wording is pinned
        // separately in `unbalanced_error_names_the_discrepancy_in_kronor`.
        let err = assert_balanced(&[debit("6540", "1000"), credit("1930", "900")])
            .expect_err("unbalanced voucher is rejected");
        assert!(err.to_string().to_lowercase().contains("balance"), "{err}");
    }

    #[test]
    fn produces_the_nested_fortnox_shape() {
        let payload =
            build_payload(&input("2026-05-31", balanced_lines())).expect("valid voucher builds");

        // This literal is transcribed from the `expect(payload).toEqual({…})`
        // in `voucher.test.ts`, not generated from the code above. The only
        // change is `1000` -> `1000.0` and so on: JavaScript has one number
        // type, `serde_json` distinguishes integer from float, and amounts
        // reach the payload as `f64` (`money::to_api`). `Number(1000)` and
        // `Number(1000.0)` are unequal in `serde_json` but identical as JSON
        // values to Fortnox's parser. Field names, nesting and casing are
        // unchanged.
        assert_eq!(
            payload,
            json!({
                "Voucher": {
                    "VoucherSeries": "A",
                    "TransactionDate": "2026-05-31",
                    "Description": "Köp av dator",
                    "VoucherRows": {
                        "VoucherRow": [
                            { "Account": "5410", "Debit": 1000.0, "Credit": 0.0 },
                            { "Account": "2640", "Debit": 250.0, "Credit": 0.0 },
                            { "Account": "1930", "Debit": 0.0, "Credit": 1250.0 },
                        ],
                    },
                },
            })
        );
    }

    // --- the error message, which is the point of the check -----------------

    #[test]
    fn unbalanced_error_names_the_discrepancy_in_kronor() {
        let err = assert_balanced(&[debit("6540", "100"), credit("1930", "90")])
            .expect_err("unbalanced voucher is rejected");
        assert_eq!(
            err.to_string(),
            "voucher does not balance: debits 100.00 exceed credits 90.00 by 10.00"
        );
    }

    #[test]
    fn the_discrepancy_is_named_the_other_way_round_too() {
        let err = assert_balanced(&[debit("6540", "90.50"), credit("1930", "100")])
            .expect_err("unbalanced voucher is rejected");
        assert_eq!(
            err.to_string(),
            "voucher does not balance: credits 100.00 exceed debits 90.50 by 9.50"
        );
    }

    // --- line-level validation ----------------------------------------------

    #[test]
    fn fewer_than_two_lines_is_an_error() {
        let err = assert_balanced(&[]).expect_err("an empty voucher is rejected");
        assert_eq!(
            err.to_string(),
            "a voucher needs at least two lines to be double entry; got 0"
        );

        // A single line that balances against itself is still not double entry.
        let err = assert_balanced(&[VoucherLine {
            account: "1930".to_string(),
            debit: d("100"),
            credit: d("100"),
            info: None,
        }])
        .expect_err("a one-line voucher is rejected");
        assert_eq!(
            err.to_string(),
            "a voucher needs at least two lines to be double entry; got 1"
        );
    }

    #[test]
    fn a_line_with_both_a_debit_and_a_credit_is_an_error() {
        // These two lines balance in aggregate (1100 debit, 1100 credit), so
        // only the per-line rule catches it.
        let err = assert_balanced(&[
            VoucherLine {
                account: "1930".to_string(),
                debit: d("1000"),
                credit: d("100"),
                info: None,
            },
            VoucherLine {
                account: "6540".to_string(),
                debit: d("100"),
                credit: d("1000"),
                info: None,
            },
        ])
        .expect_err("a two-column line is rejected");
        assert_eq!(
            err.to_string(),
            "line for account 1930 has both a debit (1000.00) and a credit (100.00); \
             a voucher line belongs in one column, so post it as two lines"
        );
    }

    #[test]
    fn a_line_with_neither_a_debit_nor_a_credit_is_an_error() {
        let err = assert_balanced(&[
            debit("6540", "100"),
            credit("1930", "100"),
            debit("2640", "0"),
        ])
        .expect_err("an empty line is rejected");
        assert_eq!(
            err.to_string(),
            "line for account 2640 has neither a debit nor a credit in öre \
             (debit 0, credit 0); it would post as 0.00 / 0.00"
        );
    }

    #[test]
    fn a_line_whose_only_amount_is_finer_than_an_ore_is_an_error() {
        // It would post as 0.00 / 0.00, so it is the empty line above wearing
        // a disguise.
        let err = assert_balanced(&[
            debit("6540", "100"),
            credit("1930", "100"),
            debit("2640", "0.004"),
        ])
        .expect_err("a sub-öre-only line is rejected");
        assert!(
            err.to_string().contains("0.00 / 0.00"),
            "expected the posted figures in the message, got: {err}"
        );
    }

    #[test]
    fn a_negative_amount_is_an_error() {
        // Upstream accepts this: -100 debit against -100 credit sums to
        // -100 == -100 and balances. It posts a nonsense voucher.
        let err = assert_balanced(&[
            VoucherLine {
                account: "6540".to_string(),
                debit: d("-100"),
                credit: Decimal::ZERO,
                info: None,
            },
            credit("1930", "-100"),
        ])
        .expect_err("a negative amount is rejected");
        assert_eq!(
            err.to_string(),
            "line for account 6540 has a negative amount (debit -100.00, credit 0.00); \
             reverse a posting by swapping the columns, not by negating an amount"
        );

        let err = assert_balanced(&[debit("6540", "100"), credit("1930", "-100")])
            .expect_err("a negative credit is rejected");
        assert!(err.to_string().contains("negative amount"), "{err}");
    }

    // --- the balance comparison's precision ---------------------------------
    //
    // These three pin the decision documented on `assert_balanced`: each line
    // is rounded to öre, then the öre figures are summed and compared exactly.

    #[test]
    fn sub_ore_dust_that_posts_as_balanced_is_accepted() {
        // Posts as 10.00 / 10.00, which Fortnox accepts. A comparison at full
        // `Decimal` precision would reject it (10.004 != 10.00).
        assert_balanced(&[debit("5410", "10.004"), credit("1930", "10.00")])
            .expect("a voucher that posts as balanced is accepted");
    }

    #[test]
    fn sub_ore_dust_that_posts_as_unbalanced_is_rejected() {
        // Upstream sums the raw amounts first: 10.004 + 10.004 = 20.008, which
        // `round2` turns into 20.01, matching the credit — so upstream accepts
        // it. The rows posted are 10.00, 10.00 and 20.01, and Fortnox does not.
        let err = assert_balanced(&[
            debit("5410", "10.004"),
            debit("5410", "10.004"),
            credit("1930", "20.01"),
        ])
        .expect_err("a voucher that posts as unbalanced is rejected");
        assert_eq!(
            err.to_string(),
            "voucher does not balance: credits 20.01 exceed debits 20.00 by 0.01"
        );
    }

    #[test]
    fn each_line_is_rounded_before_the_sums_are_compared() {
        // Two lines of 0.005 each round *up*, to 0.01 apiece, so the debit side
        // posts as 0.02. Summing first gives 0.01, which would look balanced
        // against the single credit line — and post an öre out.
        let err = assert_balanced(&[
            debit("5410", "0.005"),
            debit("5410", "0.005"),
            credit("1930", "0.01"),
        ])
        .expect_err("per-line rounding is what counts");
        assert_eq!(
            err.to_string(),
            "voucher does not balance: debits 0.02 exceed credits 0.01 by 0.01"
        );
    }

    #[test]
    fn balanced_lines_at_ore_granularity_are_upstreams_own_comparison() {
        // For 2dp lines, per-line rounding is the identity, so this check and
        // upstream's agree by construction. A spot check that it does.
        assert_balanced(&[
            debit("5410", "33.33"),
            debit("5410", "33.33"),
            debit("5410", "33.34"),
            credit("1930", "100.00"),
        ])
        .expect("öre-granular lines balance");
    }

    // --- build_payload --------------------------------------------------------

    #[test]
    fn transaction_information_is_included_when_the_line_carries_a_note() {
        let mut lines = balanced_lines();
        lines[0].info = Some("Dell XPS 13".to_string());
        let payload = build_payload(&input("2026-05-31", lines)).expect("valid voucher builds");
        assert_eq!(
            payload["Voucher"]["VoucherRows"]["VoucherRow"][0],
            json!({
                "Account": "5410",
                "Debit": 1000.0,
                "Credit": 0.0,
                "TransactionInformation": "Dell XPS 13",
            })
        );
    }

    #[test]
    fn transaction_information_is_omitted_for_an_absent_or_empty_note() {
        let mut lines = balanced_lines();
        lines[0].info = Some(String::new());
        let payload = build_payload(&input("2026-05-31", lines)).expect("valid voucher builds");
        let rows = &payload["Voucher"]["VoucherRows"]["VoucherRow"];
        // `...(l.info ? … : {})` in the TypeScript: "" is falsy, so the key is
        // absent, not present-and-empty.
        assert_eq!(rows[0].get("TransactionInformation"), None);
        assert_eq!(rows[1].get("TransactionInformation"), None);
    }

    #[test]
    fn amounts_reach_the_payload_as_ore() {
        let payload = build_payload(&input(
            "2026-05-31",
            vec![debit("5410", "10.004"), credit("1930", "10.00")],
        ))
        .expect("valid voucher builds");
        assert_eq!(
            payload["Voucher"]["VoucherRows"]["VoucherRow"][0]["Debit"],
            json!(10.0)
        );
    }

    #[test]
    fn a_malformed_transaction_date_is_an_error() {
        for bad in [
            "31/05/2026",
            "2026-5-31",
            "26-05-31",
            "2026-05-31T00:00:00Z",
            "2026-05-31 ",
            "2026/05/31",
            "",
            "yyyy-mm-dd",
        ] {
            let err = match build_payload(&input(bad, balanced_lines())) {
                Ok(payload) => panic!("date {bad:?} was accepted: {payload}"),
                Err(err) => err.to_string(),
            };
            assert!(
                err.contains("is not in YYYY-MM-DD form"),
                "date {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn an_impossible_calendar_date_is_an_error() {
        for bad in ["2026-02-30", "2026-13-01", "2026-00-10", "2026-05-32"] {
            let err = build_payload(&input(bad, balanced_lines()))
                .expect_err("an impossible date is rejected");
            assert!(
                err.to_string().contains("not a real calendar date"),
                "date {bad:?}: {err}"
            );
        }
        // A leap day that does exist is accepted.
        build_payload(&input("2028-02-29", balanced_lines())).expect("2028 is a leap year");
    }

    #[test]
    fn build_payload_rejects_an_unbalanced_voucher() {
        let err = build_payload(&input(
            "2026-05-31",
            vec![debit("5410", "1000"), credit("1930", "900")],
        ))
        .expect_err("an unbalanced voucher never reaches Fortnox");
        assert_eq!(
            err.to_string(),
            "voucher does not balance: debits 1000.00 exceed credits 900.00 by 100.00"
        );
    }

    #[test]
    fn the_serialised_body_has_the_field_names_fortnox_expects() {
        // The assertion above compares `Value`s, which are order-independent.
        // This one pins the exact bytes on the wire, so a stray rename or a
        // case change is visible as text.
        //
        // The key ORDER here is alphabetical, not the TypeScript's insertion
        // order: `serde_json::Map` is a `BTreeMap` unless the `preserve_order`
        // feature is on, and it is not on in this workspace. JSON object keys
        // are unordered by RFC 8259 and Fortnox parses JSON rather than
        // pattern-matching a string, so this is not a wire difference that can
        // matter; the names, the casing and the nesting — which do matter —
        // are upstream's exactly.
        let payload =
            build_payload(&input("2026-05-31", balanced_lines())).expect("valid voucher builds");
        assert_eq!(
            serde_json::to_string(&payload).expect("payload serialises"),
            r#"{"Voucher":{"Description":"Köp av dator","TransactionDate":"2026-05-31","VoucherRows":{"VoucherRow":[{"Account":"5410","Credit":0.0,"Debit":1000.0},{"Account":"2640","Credit":0.0,"Debit":250.0},{"Account":"1930","Credit":1250.0,"Debit":0.0}]},"VoucherSeries":"A"}}"#
        );
    }
}
