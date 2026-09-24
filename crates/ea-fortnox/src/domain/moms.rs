//! Swedish VAT (moms): splitting a VAT-inclusive amount into net + VAT.
//!
//! Ported from `domain/moms.ts`, which is thirteen lines long and carries
//! three test cases. Those thirteen lines decide how every krona the business
//! handles is divided between a revenue/cost account and a moms account, so
//! the port adds the test the TypeScript does not have: an exhaustive sweep
//! asserting `net + vat == gross` **exactly**, for every supported rate. A
//! one-öre gap makes a voucher unbalanced and Fortnox rejects it outright.

use anyhow::{bail, Result};
use rust_decimal::Decimal;

use super::money::round2;

/// The standard Swedish VAT rates, in percent.
///
/// Verbatim from `moms.ts`'s `VAT_RATES`, order included: 25% is the general
/// rate, 12% covers food and restaurant/hotel services, 6% covers books,
/// newspapers, passenger transport and cultural events.
///
/// 0% is *not* in this list — it is not a VAT rate, it is the absence of one
/// (exports, exempt supplies). [`split_gross`] accepts it all the same, and
/// the boundary tests pin that, but it does not belong in the list of rates
/// a user picks from.
pub const VAT_RATES: [u8; 3] = [25, 12, 6];

/// A gross amount divided into its net and VAT components.
///
/// The invariant that matters, and which [`split_gross`] guarantees for every
/// input it accepts: `net + vat == gross`, exactly, with no tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MomsSplit {
    /// The amount excluding VAT, rounded to öre.
    pub net: Decimal,
    /// The VAT itself, rounded to öre. Always `gross - net`.
    pub vat: Decimal,
    /// The VAT-inclusive amount, rounded to öre. Note this is the *rounded*
    /// input, not the raw input — matching `moms.ts`, which returns
    /// `gross: round2(gross)`.
    pub gross: Decimal,
    /// The rate applied, in percent.
    pub rate: u8,
}

/// Split a VAT-inclusive (gross) amount into net + VAT for the given rate.
///
/// # Which component is rounded, and why
///
/// `net` is computed by rounding; `vat` is then obtained by subtraction.
/// Rounding both independently is the classic way to produce a voucher that
/// is one öre out, and the `net_plus_vat_always_reconciles_to_gross` test
/// exists to catch exactly that.
///
/// This matches `moms.ts` exactly, which is the reason for the choice:
///
/// ```js
/// const net = round2(gross / (1 + rate / 100));
/// const vat = round2(gross - net);
/// ```
///
/// The upstream already derives one component by subtraction, so it already
/// satisfies the reconciliation property — verified under node over every öre
/// from 0.01 to 100.00 at each of 0/6/12/25%: zero failures. There was
/// therefore no defect to fix and no reason to pick the other component.
///
/// It is not a free choice, though — rounding `vat` instead and deriving
/// `net = gross - vat` also reconciles, but produces a *different split* at
/// exact half-öre midpoints, because half-away-from-zero then rounds the
/// other component up. Computed exactly in integer öre:
///
/// | rate | gross values where the two disagree, 0.01–100.00 |
/// |------|--------------------------------------------------|
/// | 0%   | none (`vat` is always 0) |
/// | 6%   | none — `net = 50n/53` öre can never end in exactly `.5` |
/// | 12%  | **357** — every `gross ≡ 0.14 (mod 0.28)` kr |
/// | 25%  | none — `net = 0.8n` öre can never end in exactly `.5` |
///
/// The first is `0.14` kr at 12%, whose exact net is `0.125`: rounding `net`
/// gives `(0.13, 0.01)`, rounding `vat` gives `(0.12, 0.02)`. Both reconcile;
/// neither is more accurate (the error is half an öre either way). Only one
/// of them is what has been filing this company's VAT returns, so that is the
/// one here, pinned by the `midpoint_at_twelve_percent_rounds_net_up` test.
///
/// # Errors
///
/// * an unsupported `rate` — anything but 0% or a member of [`VAT_RATES`];
/// * a negative `gross`. A credit note is the reversal of an invoice, a
///   different operation with different accounts and a different sign
///   convention; accepting one here would silently book a nonsense voucher.
///   Reject it and make the caller be explicit.
pub fn split_gross(gross: Decimal, rate: u8) -> Result<MomsSplit> {
    if rate != 0 && !VAT_RATES.contains(&rate) {
        bail!("unsupported VAT rate {rate}%: supported rates are 0%, 6%, 12% and 25%");
    }
    if gross.is_sign_negative() && !gross.is_zero() {
        bail!(
            "negative gross amount {gross}: split_gross takes a VAT-inclusive amount \
             owed or received, not a reversal — a credit note is a separate operation"
        );
    }

    let divisor = Decimal::ONE + Decimal::from(rate) / Decimal::from(100u8);

    // Round *one* component and derive the other by subtraction. `net` is the
    // rounded one, because that is what `moms.ts` rounds; see the doc comment
    // for the measurement behind that choice.
    //
    // Both lines divide and subtract the *raw* `gross`, not the rounded one,
    // again matching `moms.ts`. It matters only when the caller passes an
    // amount finer than an öre, and then it matters: `splitGross(1250.005,
    // 25)` upstream is `{net: 1000, vat: 250.01}`, where rounding `gross` to
    // 1250.01 first and dividing that would give `{net: 1000.01, vat: 250}`.
    // Both reconcile; upstream's uses the undegraded input to place the net,
    // so it is the one kept.
    let net = round2(gross / divisor);

    // The reported gross: upstream's third field, and the figure the two
    // lines add up to.
    let gross = round2(gross);

    // `vat` is a *plain subtraction* of two whole-öre figures. It is exact,
    // so `net + vat == gross` holds by construction rather than by luck, and
    // there is nothing left for a rounding rule to disagree about.
    //
    // Upstream writes `round2(gross - net)`, and that second `round2` is a
    // trap this port must not copy. It looks like a no-op and is one for any
    // 2dp input, but for an input carrying sub-öre dust the residual can be
    // exactly -0.005 — `splitGross(0.005, 0)`: `net` rounds up to 0.01, so
    // `gross - net` is -0.005. JavaScript's `Math.round` breaks that tie
    // toward +infinity (`Math.round(-0.5) === -0`), so upstream gets `vat: 0`
    // and reconciles. `money::round2` is half away from *zero* — deliberately,
    // see its doc comment — so it would return -0.01 and produce a voucher
    // that is two öre out with a negative VAT line. The reconciliation sweep
    // `reconciles_for_inputs_finer_than_an_ore` caught exactly this.
    //
    // `net <= gross` because `gross / divisor <= gross` for any rate >= 0 and
    // `round2` is monotonic, so `vat` is never negative.
    let vat = gross - net;

    Ok(MomsSplit {
        net,
        vat,
        gross,
        rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Decimal` from a literal like `dec("1250.00")`, so the tests read as
    /// the kronor amounts they are.
    fn dec(s: &str) -> Decimal {
        s.parse().expect("test literal is a valid decimal")
    }

    // ---- the three cases from moms.test.ts, verbatim ---------------------

    /// `moms.test.ts`: "splits a 25% gross amount into net + vat (kronor, 2dp)".
    /// 1250 kr gross @ 25% -> 1000 net + 250 vat.
    #[test]
    fn splits_a_twenty_five_percent_gross_amount_into_net_and_vat() {
        let split = split_gross(dec("1250"), 25).unwrap();
        assert_eq!(
            split,
            MomsSplit {
                net: dec("1000"),
                vat: dec("250"),
                gross: dec("1250"),
                rate: 25,
            }
        );
    }

    /// `moms.test.ts`: "rounds to 2 decimals". 100 kr @ 25% -> net 80, vat 20.
    #[test]
    fn rounds_to_two_decimals() {
        let split = split_gross(dec("100"), 25).unwrap();
        assert_eq!(split.net, dec("80"));
        assert_eq!(split.vat, dec("20"));
    }

    /// `moms.test.ts`: "exposes the standard Swedish rates".
    #[test]
    fn exposes_the_standard_swedish_rates() {
        assert_eq!(VAT_RATES, [25, 12, 6]);
    }

    // ---- the property test the TypeScript does not have ------------------

    /// **The point of this module.** `net + vat` must equal `gross` exactly,
    /// at every supported rate, over every öre from 0.01 to 100.00 and then a
    /// sparse sweep to 1,000,000 kr — 80,000 splits in all.
    ///
    /// A single öre of slack here makes a voucher unbalanced, and Fortnox
    /// rejects an unbalanced voucher outright. There is no tolerance to widen:
    /// the assertion is equality on `Decimal`, which is exact.
    #[test]
    fn net_plus_vat_always_reconciles_to_gross() {
        for rate in [0u8, 6, 12, 25] {
            let fine = (1..=10_000).map(|c| Decimal::new(c, 2));
            // 100.00 kr in öre, i.e. k * 100 kronor, up to 1,000,000 kr.
            let coarse = (1..=10_000).map(|k| Decimal::new(k * 10_000, 2));
            for gross in fine.chain(coarse) {
                let split = split_gross(gross, rate).unwrap();
                assert_eq!(
                    split.net + split.vat,
                    gross,
                    "rate {rate}% on {gross} kr: {} + {} != {gross}",
                    split.net,
                    split.vat
                );
                assert_eq!(
                    split.gross, gross,
                    "rate {rate}% on {gross} kr: gross field"
                );
                assert_eq!(split.rate, rate);
                assert!(
                    split.vat >= Decimal::ZERO,
                    "rate {rate}% on {gross} kr: negative vat {}",
                    split.vat
                );
                assert!(
                    split.net >= Decimal::ZERO,
                    "rate {rate}% on {gross} kr: negative net {}",
                    split.net
                );
                assert!(
                    split.net <= gross,
                    "rate {rate}% on {gross} kr: net {} exceeds gross",
                    split.net
                );
            }
        }
    }

    /// Both components are whole öre — nothing leaves this function carrying
    /// a third decimal for a later `to_api` to quietly truncate.
    #[test]
    fn both_components_are_whole_ore() {
        for rate in [0u8, 6, 12, 25] {
            for c in 1..=2_000 {
                let split = split_gross(Decimal::new(c, 2), rate).unwrap();
                assert_eq!(round2(split.net), split.net);
                assert_eq!(round2(split.vat), split.vat);
            }
        }
    }

    // ---- boundary cases beyond the upstream three ------------------------

    #[test]
    fn zero_gross_splits_into_zeros() {
        for rate in [0u8, 6, 12, 25] {
            let split = split_gross(Decimal::ZERO, rate).unwrap();
            assert_eq!(split.net, Decimal::ZERO, "rate {rate}%");
            assert_eq!(split.vat, Decimal::ZERO, "rate {rate}%");
            assert_eq!(split.gross, Decimal::ZERO, "rate {rate}%");
            assert_eq!(split.rate, rate);
        }
    }

    /// 0% is not a VAT rate but it is a real case (exports, exempt supplies),
    /// and it must not book a moms line at all.
    #[test]
    fn zero_rate_puts_everything_in_net() {
        for gross in ["0.01", "1", "99.99", "1250", "1000000"] {
            let split = split_gross(dec(gross), 0).unwrap();
            assert_eq!(split.net, dec(gross), "{gross} kr @ 0%");
            assert_eq!(split.vat, Decimal::ZERO, "{gross} kr @ 0%");
        }
    }

    /// An unsupported rate is an error, and the error says which rates *are*
    /// supported — the caller is usually a human reading a log line.
    #[test]
    fn an_unsupported_rate_is_an_error_naming_the_supported_rates() {
        let err = split_gross(dec("100"), 17).unwrap_err().to_string();
        assert!(err.contains("17"), "{err}");
        for rate in ["0%", "6%", "12%", "25%"] {
            assert!(err.contains(rate), "error should name {rate}: {err}");
        }

        // Every other plausible wrong rate is rejected too, including the
        // ones a caller might reach for by confusing a rate with a multiplier.
        for rate in [1u8, 5, 7, 11, 13, 20, 24, 26, 50, 100, 125, 255] {
            assert!(
                split_gross(dec("100"), rate).is_err(),
                "rate {rate}% should be rejected"
            );
        }
        // ...and every rate that is supported is accepted.
        for rate in [0u8, 6, 12, 25] {
            assert!(split_gross(dec("100"), rate).is_ok(), "rate {rate}%");
        }
    }

    /// A credit note is a different operation. Booking one through here would
    /// produce a voucher with the signs the wrong way round on both lines.
    #[test]
    fn a_negative_gross_is_an_error() {
        for gross in ["-0.01", "-1", "-1250", "-1000000"] {
            let err = split_gross(dec(gross), 25).unwrap_err().to_string();
            assert!(err.contains("credit note"), "error should say why: {err}");
        }
        // The rate is validated before the sign, but a negative amount at a
        // bad rate is still an error rather than a split.
        assert!(split_gross(dec("-100"), 17).is_err());
        // Negative zero is not a negative amount.
        assert!(split_gross(dec("-0.00"), 25).is_ok());
    }

    // ---- the case that discriminates net-first from vat-first ------------

    /// The one place the choice of rounded component is observable.
    ///
    /// 0.14 kr at 12% has an exact net of 0.125 — a true half-öre midpoint.
    /// Rounding `net` (half away from zero, as `money::round2` does) gives
    /// 0.13 and leaves 0.01 of VAT; rounding `vat` instead would give 0.02 of
    /// VAT and 0.12 of net. Both reconcile, so the property test cannot tell
    /// them apart. This test can, and pins the upstream behaviour.
    ///
    /// Verified against `moms.ts` under node: `splitGross(0.14, 12)` returns
    /// `{ net: 0.13, vat: 0.01, gross: 0.14, rate: 12 }`.
    #[test]
    fn midpoint_at_twelve_percent_rounds_net_up() {
        let split = split_gross(dec("0.14"), 12).unwrap();
        assert_eq!(split.net, dec("0.13"));
        assert_eq!(split.vat, dec("0.01"));

        // The same midpoint recurs every 0.28 kr; 357 of them fall inside the
        // property test's fine sweep. Three more, to show it is a pattern and
        // not one lucky value.
        for (gross, net, vat) in [
            ("0.42", "0.38", "0.04"),
            ("0.70", "0.63", "0.07"),
            ("1.26", "1.13", "0.13"),
        ] {
            let split = split_gross(dec(gross), 12).unwrap();
            assert_eq!(split.net, dec(net), "{gross} kr @ 12%");
            assert_eq!(split.vat, dec(vat), "{gross} kr @ 12%");
        }

        // 25% and 6% have no such midpoints at all: an exact net of
        // `0.8n` öre or `50n/53` öre can never end in exactly `.5`.
    }

    // ---- ordinary arithmetic at each real rate ---------------------------

    #[test]
    fn splits_at_each_standard_rate() {
        // Textbook amounts where the arithmetic is exact at every rate.
        for (gross, rate, net, vat) in [
            ("1250.00", 25u8, "1000.00", "250.00"),
            ("1120.00", 12, "1000.00", "120.00"),
            ("1060.00", 6, "1000.00", "60.00"),
            ("625.00", 25, "500.00", "125.00"),
            ("112.00", 12, "100.00", "12.00"),
            ("10.60", 6, "10.00", "0.60"),
        ] {
            let split = split_gross(dec(gross), rate).unwrap();
            assert_eq!(split.net, dec(net), "{gross} kr @ {rate}%");
            assert_eq!(split.vat, dec(vat), "{gross} kr @ {rate}%");
        }
    }

    /// A gross that does not divide cleanly still reconciles, and the VAT is
    /// the residual rather than an independently-rounded figure.
    #[test]
    fn an_inexact_quotient_puts_the_remainder_in_vat() {
        // 100 kr @ 6%: exact net is 94.339622..., rounds to 94.34.
        let split = split_gross(dec("100"), 6).unwrap();
        assert_eq!(split.net, dec("94.34"));
        assert_eq!(split.vat, dec("5.66"));
        assert_eq!(split.net + split.vat, dec("100"));

        // 99.99 kr @ 12%: exact net is 89.276785714..., rounds to 89.28.
        let split = split_gross(dec("99.99"), 12).unwrap();
        assert_eq!(split.net, dec("89.28"));
        assert_eq!(split.vat, dec("10.71"));
        assert_eq!(split.net + split.vat, dec("99.99"));
    }

    /// The smallest amounts: one öre cannot be split, so it all lands in net
    /// and the VAT line is zero. That is the arithmetic, not a special case.
    #[test]
    fn one_ore_gross_is_all_net() {
        for rate in [6u8, 12, 25] {
            let split = split_gross(dec("0.01"), rate).unwrap();
            assert_eq!(split.net, dec("0.01"), "rate {rate}%");
            assert_eq!(split.vat, Decimal::ZERO, "rate {rate}%");
        }
        // 0.03 @ 25% is the first amount that carries any VAT at all.
        let split = split_gross(dec("0.03"), 25).unwrap();
        assert_eq!(split.net, dec("0.02"));
        assert_eq!(split.vat, dec("0.01"));
    }

    /// Sub-öre dust in the input: the reported `gross` is rounded, but the
    /// division and the subtraction both use the raw amount, exactly as
    /// `moms.ts` does.
    ///
    /// Verified against the compiled upstream module under node:
    /// `splitGross(1250.004, 25)` -> `{net: 1000, vat: 250, gross: 1250}`,
    /// `splitGross(1250.005, 25)` -> `{net: 1000, vat: 250.01, gross: 1250.01}`.
    ///
    /// The second is the discriminating one. Rounding `gross` to 1250.01
    /// before dividing would give `{net: 1000.01, vat: 250}` — it still
    /// reconciles, so only this test rules it out.
    #[test]
    fn a_gross_finer_than_ore_uses_the_raw_amount_to_place_the_net() {
        let split = split_gross(dec("1250.004"), 25).unwrap();
        assert_eq!(split.gross, dec("1250.00"));
        assert_eq!(split.net, dec("1000.00"));
        assert_eq!(split.vat, dec("250.00"));

        // Half an öre rounds away from zero, as money::round2 does, and the
        // extra öre lands on the VAT line because the net was placed from the
        // raw 1250.005 (-> 1000.004 -> 1000.00).
        let split = split_gross(dec("1250.005"), 25).unwrap();
        assert_eq!(split.gross, dec("1250.01"));
        assert_eq!(split.net, dec("1000.00"));
        assert_eq!(split.vat, dec("250.01"));
    }

    /// The reconciliation property again, for inputs finer than an öre: the
    /// two lines must add up to the `gross` the split reports, whatever scale
    /// the caller handed in. `round2` being translation-invariant under a
    /// whole-öre shift is what makes this hold; this test is the evidence.
    #[test]
    fn reconciles_for_inputs_finer_than_an_ore() {
        for rate in [0u8, 6, 12, 25] {
            // Every tenth of a millikrona from 0.0001 to 20.0000 — which
            // sweeps every dust value against every öre boundary, including
            // the exact half-öre midpoints at .005.
            for m in 1..=200_000i64 {
                let gross = Decimal::new(m, 4);
                let split = split_gross(gross, rate).unwrap();
                assert_eq!(
                    split.net + split.vat,
                    split.gross,
                    "rate {rate}% on {gross} kr: {} + {} != {}",
                    split.net,
                    split.vat,
                    split.gross
                );
                assert!(split.net >= Decimal::ZERO && split.vat >= Decimal::ZERO);
            }
        }
    }
}
