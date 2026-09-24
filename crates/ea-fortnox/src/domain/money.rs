//! Money: one rounding rule, one way in from text, one way out to JSON.
//!
//! Upstream this is `domain/money.ts`, three lines long and with no test file
//! of its own. That absence is not a licence to leave it unpinned — every VAT
//! split, every voucher balance check and every amount posted to Fortnox goes
//! through [`round2`], so its behaviour at the midpoint is the single most
//! load-bearing decision in the crate. It is pinned here in both directions.

use anyhow::{bail, Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};

/// Round to öre, half away from zero.
///
/// # Which rounding rule, and how that was established
///
/// The TypeScript is:
///
/// ```js
/// Math.round((n + Number.EPSILON) * 100) / 100
/// ```
///
/// This is **half away from zero**, not banker's rounding. Running the
/// TypeScript over the values where the two rules disagree settles it —
/// `0.005 → 0.01`, `0.025 → 0.03`, `0.045 → 0.05`, `0.125 → 0.13`. Under
/// half-to-even those would be `0.00`, `0.02`, `0.04`, `0.12`. They are not.
///
/// The `Number.EPSILON` term is what makes that true rather than accidental.
/// `1.005` as a double is `1.00499999999999989…`, so `1.005 * 100` is
/// `100.49999999999999` and `Math.round` alone would return `1.00`. The nudge
/// exists solely to drag such values back over the midpoint they mathematically
/// sit on. It is a patch for binary representation error, and [`Decimal`] has
/// no representation error to patch, so there is nothing to port: the whole of
/// the TypeScript's intent is the strategy name below.
///
/// # The one deliberate divergence: negative midpoints
///
/// JavaScript's `Math.round` breaks ties toward **positive infinity**, not away
/// from zero. So the TypeScript is asymmetric: `round2(1.005)` is `1.01` but
/// `round2(-1.005)` is `-1.00`, and `round2(-0.125)` is `-0.12` where
/// `round2(0.125)` is `0.13`. That is a property of the `Math.round` spec, not
/// an accounting decision, and it is a bug: a credit note is the negation of
/// the invoice it reverses, so an asymmetric rounding rule leaves a one-öre
/// residue that stops the pair from cancelling. Swedish practice (Skatteverket's
/// öresavrundning) rounds half away from zero in both directions.
///
/// This crate therefore uses [`RoundingStrategy::MidpointAwayFromZero`], which
/// satisfies `round2(-x) == -round2(x)` for every `x`. `rust_decimal` has no
/// "midpoint toward positive infinity" strategy to reproduce the JavaScript
/// exactly, and reproducing it would not be desirable.
pub fn round2(amount: Decimal) -> Decimal {
    amount.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

/// Parse a kronor amount written the way a Swedish spreadsheet or bank export
/// writes one: `"1 234,56"`, `"1234.56"`, `"-89,00"`.
///
/// This is the typed replacement for the TypeScript's
/// `Number(String(v).replace(/\s/g, '').replace(',', '.'))` in
/// `bank-import/workbook.ts`. Two behaviours are carried over deliberately:
///
/// * **All whitespace is stripped**, including the non-breaking space that
///   Excel and the bank exports use as a thousands separator. Rust's
///   [`char::is_whitespace`] covers `U+00A0`, matching JavaScript's `\s`.
/// * **Only one comma is accepted**, and it means the decimal point. The
///   TypeScript's `.replace(',', '.')` replaces the *first* comma only, so
///   `"1,234.56"` became `"1.234.56"` and then `NaN`. It is still rejected
///   here — an anglophone thousands separator in a Swedish export is a data
///   problem to surface, not to guess at.
///
/// Where this departs from the TypeScript is the failure mode. `Number("abc")`
/// is `NaN`, which propagates silently through every later sum and lands in a
/// voucher as `null`. Here it is an error.
pub fn from_kronor(input: &str) -> Result<Decimal> {
    let stripped: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.is_empty() {
        bail!("empty amount");
    }
    let normalised = match stripped.matches(',').count() {
        0 => stripped.clone(),
        1 => stripped.replace(',', "."),
        _ => bail!("amount {input:?} has more than one comma; the decimal separator is ambiguous"),
    };
    normalised
        .parse::<Decimal>()
        .with_context(|| format!("amount {input:?} is not a number"))
}

/// Convert an amount to the `f64` the Fortnox JSON boundary requires.
///
/// Fortnox takes amounts as JSON numbers, so this conversion is unavoidable;
/// the job is to make it harmless. The amount is rounded to öre first, because
/// an unrounded `Decimal` becomes something like `33.333333333333336` in the
/// request body. That rounding is a last line of defence and not a licence to
/// skip [`round2`] earlier: rounding each line independently at the boundary
/// cannot make a set of lines balance that did not already balance.
///
/// # Where this stops being lossless
///
/// An öre amount is never *exactly* representable in binary — `0.01` has no
/// finite binary expansion — so the question is not exactness but round-trip
/// fidelity: does the shortest decimal that names the resulting `f64` (which
/// is what `serde_json` writes, and what Fortnox parses) have the same digits
/// we started with?
///
/// It does for every amount with at most 15 significant decimal digits, which
/// at two decimals means **|amount| < 10^13 kronor** — ten trillion. Above
/// roughly 2^53 öre (≈ 9.0 × 10^13 kronor) neighbouring öre amounts collapse
/// onto the same `f64` and the loss becomes real rather than theoretical. A
/// Swedish AB posts nine-figure amounts at the very most, four orders of
/// magnitude inside the guarantee, so nothing that reaches this function in
/// practice is at risk.
pub fn to_api(amount: Decimal) -> f64 {
    // `Decimal`'s maximum magnitude is ≈ 7.9e28, comfortably inside `f64`'s
    // range, so `to_f64` does not fail. The fallback keeps a panic out of the
    // serialisation path if that ever changes; `NaN` serialises as `null`,
    // which Fortnox rejects loudly rather than booking a wrong number.
    round2(amount).to_f64().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("test literal parses")
    }

    // --- round2: the midpoint rule, pinned in both directions ---------------
    //
    // `domain/money.ts` has no test file upstream. These cases were derived by
    // running the TypeScript `round2` directly under node and recording what it
    // returned; see the doc comment on `round2` for the reasoning.

    #[test]
    fn rounds_midpoints_away_from_zero_not_to_even() {
        // Every one of these disagrees with banker's rounding, which is the
        // whole point of listing them. Half-to-even would give the value in
        // the comment.
        assert_eq!(round2(d("0.005")), d("0.01")); // banker's: 0.00
        assert_eq!(round2(d("0.015")), d("0.02")); // banker's: 0.02 (agrees)
        assert_eq!(round2(d("0.025")), d("0.03")); // banker's: 0.02
        assert_eq!(round2(d("0.035")), d("0.04")); // banker's: 0.04 (agrees)
        assert_eq!(round2(d("0.045")), d("0.05")); // banker's: 0.04
        assert_eq!(round2(d("0.125")), d("0.13")); // banker's: 0.12
        assert_eq!(round2(d("1.005")), d("1.01")); // banker's: 1.00
        assert_eq!(round2(d("2.675")), d("2.68")); // banker's: 2.68 (agrees)
    }

    #[test]
    fn negative_midpoints_round_away_from_zero_too() {
        // DELIBERATE DIVERGENCE from the TypeScript. `Math.round` breaks ties
        // toward positive infinity, so the upstream values are:
        //     round2(-0.005) === -0      (here: -0.01)
        //     round2(-0.025) === -0.02   (here: -0.03)
        //     round2(-0.125) === -0.12   (here: -0.13)
        //     round2(-1.005) === -1      (here: -1.01)
        // That asymmetry is a `Math.round` artifact, not an accounting rule,
        // and it means a credit note does not cancel the invoice it reverses.
        // See the `round2` doc comment.
        assert_eq!(round2(d("-0.005")), d("-0.01"));
        assert_eq!(round2(d("-0.025")), d("-0.03"));
        assert_eq!(round2(d("-0.125")), d("-0.13"));
        assert_eq!(round2(d("-1.005")), d("-1.01"));
    }

    #[test]
    fn rounding_is_symmetric_about_zero() {
        // The property the divergence above buys: a reversal is the exact
        // negation of what it reverses, so the pair nets to zero öre.
        for s in [
            "0.005", "0.015", "0.025", "0.125", "1.005", "2.675", "1249.375", "0.001", "33.335",
        ] {
            let x = d(s);
            assert_eq!(round2(-x), -round2(x), "round2 asymmetric at {s}");
        }
    }

    #[test]
    fn below_the_midpoint_rounds_down_and_above_rounds_up() {
        assert_eq!(round2(d("1.0049999")), d("1.00"));
        assert_eq!(round2(d("1.0050001")), d("1.01"));
        assert_eq!(round2(d("-1.0049999")), d("-1.00"));
        assert_eq!(round2(d("-1.0050001")), d("-1.01"));
    }

    #[test]
    fn keeps_ore_level_values_intact() {
        for s in [
            "0.01",
            "0.99",
            "1.00",
            "0.10",
            "-0.01",
            "12345.67",
            "-12345.67",
        ] {
            assert_eq!(round2(d(s)), d(s), "round2 disturbed {s}");
        }
        // Zero has no sign to lose.
        assert_eq!(round2(d("0")), Decimal::ZERO);
        assert_eq!(round2(d("-0.001")), Decimal::ZERO);
    }

    #[test]
    fn collapses_values_with_more_than_two_decimals() {
        // 25% VAT on 1234.56 is 308.64 exactly; 12% on 99.99 is not.
        assert_eq!(round2(d("11.9988")), d("12.00"));
        assert_eq!(round2(d("33.333333333333333333333333")), d("33.33"));
        assert_eq!(round2(d("66.666666666666666666666666")), d("66.67"));
        assert_eq!(round2(d("-66.666666666666666666666666")), d("-66.67"));
        assert_eq!(round2(d("0.004999999999999999999999")), Decimal::ZERO);
    }

    #[test]
    fn exact_decimal_arithmetic_beats_the_typescript_float_path() {
        // 0.1 + 0.2 is 0.30000000000000004 as doubles; the TypeScript relies on
        // round2 to clean that up. Here there is nothing to clean up, and the
        // test exists to record that the difference is intentional.
        assert_eq!(d("0.1") + d("0.2"), d("0.3"));
        assert_eq!(round2(d("0.1") + d("0.2")), d("0.30"));
    }

    // --- from_kronor --------------------------------------------------------

    #[test]
    fn parses_plain_and_swedish_formatted_amounts() {
        assert_eq!(from_kronor("1234.56").unwrap(), d("1234.56"));
        assert_eq!(from_kronor("1234,56").unwrap(), d("1234.56"));
        assert_eq!(from_kronor("1 234,56").unwrap(), d("1234.56"));
        assert_eq!(from_kronor("  1 234,56  ").unwrap(), d("1234.56"));
        assert_eq!(from_kronor("-89,00").unwrap(), d("-89.00"));
        assert_eq!(from_kronor("0").unwrap(), Decimal::ZERO);
        assert_eq!(from_kronor("0,01").unwrap(), d("0.01"));
    }

    #[test]
    fn strips_the_non_breaking_space_excel_uses_for_thousands() {
        // The bank exports separate thousands with U+00A0, which JavaScript's
        // `\s` strips and `char::is_whitespace` also strips.
        assert_eq!(from_kronor("1\u{00a0}234,56").unwrap(), d("1234.56"));
        assert_eq!(
            from_kronor("1\u{00a0}234\u{00a0}567,89").unwrap(),
            d("1234567.89")
        );
    }

    #[test]
    fn keeps_more_than_two_decimals_rather_than_rounding_on_the_way_in() {
        // Parsing is not the place to round: the caller decides when an amount
        // has finished being computed.
        assert_eq!(from_kronor("1.005").unwrap(), d("1.005"));
    }

    #[test]
    fn rejects_what_the_typescript_turned_into_nan() {
        for bad in [
            "", "   ", "abc", "1.2.3", "12kr", "--1", "1,234.56", "1,2,3", "1 234 kr",
        ] {
            assert!(
                from_kronor(bad).is_err(),
                "expected {bad:?} to be rejected, got {:?}",
                from_kronor(bad)
            );
        }
    }

    // --- to_api -------------------------------------------------------------

    #[test]
    fn to_api_rounds_to_ore_before_leaving_the_type_system() {
        assert_eq!(to_api(d("33.333333333333333333")), 33.33);
        assert_eq!(to_api(d("1.005")), 1.01);
        assert_eq!(to_api(d("-1.005")), -1.01);
        assert_eq!(to_api(d("1234.56")), 1234.56);
        assert_eq!(to_api(Decimal::ZERO), 0.0);
    }

    #[test]
    fn to_api_round_trips_every_ore_amount_an_ab_actually_posts() {
        // Exhaustive over a stride of öre values from one öre up to a billion
        // kronor. The check is the one that matters at the JSON boundary: the
        // shortest decimal naming the f64 — what serde_json writes — parses
        // back to the Decimal we started from.
        let mut amount = d("0.01");
        let step = d("999999.37"); // a non-round stride, to avoid only testing tidy values
        while amount < d("1000000000") {
            let back = Decimal::from_str(&to_api(amount).to_string()).unwrap();
            assert_eq!(back, amount, "f64 round trip lost {amount}");
            assert_eq!(
                Decimal::from_str(&to_api(-amount).to_string()).unwrap(),
                -amount
            );
            amount += step;
        }
    }

    #[test]
    fn to_api_round_trips_up_to_the_documented_bound_and_fails_past_it() {
        // 15 significant digits: the guarantee holds.
        let last_safe = d("9999999999999.99"); // just under 10^13
        assert_eq!(
            Decimal::from_str(&to_api(last_safe).to_string()).unwrap(),
            last_safe
        );

        // The cliff is exactly 2^53 öre = 90_071_992_547_409.92 kronor: one öre
        // below it, 9_007_199_254_740_991 öre, is the first amount whose öre
        // digit an f64 can no longer name. Measured, not assumed.
        let past_the_cliff = d("90071992547409.91");
        assert_eq!(to_api(past_the_cliff), 90071992547409.9_f64);
        assert_ne!(
            Decimal::from_str(&to_api(past_the_cliff).to_string()).unwrap(),
            past_the_cliff,
            "the documented precision bound moved; update the to_api doc comment"
        );
    }
}
