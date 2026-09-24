//! The BAS chart of accounts — the Swedish standard chart every Swedish
//! company's bookkeeping is expressed in.
//!
//! A BAS account number is four digits. The leading digit is the *class*, and
//! the class alone answers the two questions this module exists to answer:
//! which report does this account belong to, and is this a VAT account?
//!
//! Ported from `domain/bas.ts`, whose three test cases are translated verbatim
//! below and then extended — the upstream file tests only the VAT predicates.

use anyhow::{bail, Result};

/// The eight BAS classes, in order, indexed by `class - 1`.
///
/// Prefer [`class_label`], which does the off-by-one for you. The array is
/// public because report code wants to iterate the classes in order.
///
/// Classes 5 and 6 share a label. That is not a transcription slip: BAS splits
/// *övriga externa kostnader* across two classes for account-numbering room
/// while treating them as one line in the income statement, and the upstream
/// `BAS_CLASS_LABELS` records that.
pub const CLASS_LABELS: [&str; 8] = [
    "Tillgångar",               // 1 — assets
    "Eget kapital & skulder",   // 2 — equity & liabilities
    "Intäkter",                 // 3 — revenue
    "Material & varor",         // 4
    "Övriga externa kostnader", // 5
    "Övriga externa kostnader", // 6
    "Personalkostnader",        // 7
    "Finansiella poster",       // 8
];

/// The label for a BAS class, or `None` if `class` is not one of 1–8.
pub fn class_label(class: u8) -> Option<&'static str> {
    match class {
        1..=8 => Some(CLASS_LABELS[usize::from(class) - 1]),
        _ => None,
    }
}

/// The BAS class of an account number — its leading digit.
///
/// # Errors
///
/// Upstream this is `Number(accountNumber.trim()[0])`, which returns `NaN` for
/// an empty or non-numeric account and then compares false against everything,
/// so a typo silently drops an account out of every report it belongs in.
/// Here the same input is an error.
///
/// Stricter than upstream in two ways, both deliberate:
///
/// * The whole string must be **exactly four digits**. Upstream looked only at
///   the first character, so `"3abc"` was class 3 and `"30"` was class 3. A BAS
///   account is four digits; anything else is a data error worth surfacing.
/// * The class must be **1–8**. A leading `0` would otherwise come back as the
///   "silent zero" that no label, no report and no predicate can do anything
///   with. Class 9 is reserved for internal/statistical accounts, which this
///   crate does not handle; every four-digit account number written literally
///   in the TypeScript source falls in 1220–8410. That is weaker evidence
///   than it looks: it covers literals grepped out of the TypeScript, not the
///   live Fortnox chart the predicates actually run over at a real company,
///   which can contain class-9 accounts the source never had reason to name.
///   `reporting.ts:41` already carries a `` `Klass ${c}` `` fallback label for
///   an unrecognised class, i.e. upstream itself does not assume the corpus
///   is exhaustive — if a later task needs class 9, this is the function to
///   widen.
pub fn class_of(account: &str) -> Result<u8> {
    let trimmed = account.trim();
    if trimmed.is_empty() {
        bail!("empty BAS account number");
    }
    if trimmed.len() != 4 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{account:?} is not a BAS account number; expected exactly four digits");
    }
    let class = trimmed.as_bytes()[0] - b'0';
    if !(1..=8).contains(&class) {
        bail!("BAS account {trimmed} is in class {class}; this chart covers classes 1-8");
    }
    Ok(class)
}

/// Output VAT (*utgående moms*): 2610–2639.
///
/// Covers all three Swedish rates — `261x` = 25%, `262x` = 12%, `263x` = 6% —
/// including the reverse-charge subaccounts (2615/2625/2635 and friends).
/// Excludes 2650, the *redovisningskonto* where the two sides are settled
/// against each other and which is therefore neither output nor input VAT.
pub fn is_output_vat(account: &str) -> bool {
    // Upstream: /^26[123]\d$/ against the trimmed string.
    let a = account.trim().as_bytes();
    a.len() == 4
        && a[0] == b'2'
        && a[1] == b'6'
        && matches!(a[2], b'1' | b'2' | b'3')
        && a[3].is_ascii_digit()
}

/// Input VAT (*ingående moms*): the whole 264x range.
///
/// 2640 standard, 2641 debiterad, 2645 beräknad på utlandsförvärv, 2647 omvänd
/// skattskyldighet, 2648 vilande, 2649 blandad verksamhet.
pub fn is_input_vat(account: &str) -> bool {
    // Upstream: /^264\d$/ against the trimmed string.
    let a = account.trim().as_bytes();
    a.len() == 4 && a[0] == b'2' && a[1] == b'6' && a[2] == b'4' && a[3].is_ascii_digit()
}

/// Income statement accounts: classes 3–8.
///
/// An account number [`class_of`] rejects is neither a result nor a balance
/// account; both predicates answer `false` rather than guessing.
pub fn is_result_account(account: &str) -> bool {
    matches!(class_of(account), Ok(3..=8))
}

/// Balance sheet accounts: classes 1–2.
pub fn is_balance_account(account: &str) -> bool {
    matches!(class_of(account), Ok(1 | 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- translated verbatim from domain/bas.test.ts ------------------------

    #[test]
    fn classifies_all_three_output_vat_rate_ranges() {
        assert!(is_output_vat("2611")); // utgående moms 25%
        assert!(is_output_vat("2615")); // utg moms omvänd skattskyldighet 25%
        assert!(is_output_vat("2621")); // utgående moms 12%
        assert!(is_output_vat("2631")); // utgående moms 6%
    }

    #[test]
    fn classifies_the_whole_264x_input_vat_range() {
        assert!(is_input_vat("2640")); // ingående moms
        assert!(is_input_vat("2641")); // debiterad ingående moms
        assert!(is_input_vat("2645")); // beräknad ing. moms utlandsförvärv
        assert!(is_input_vat("2647")); // ing. moms omvänd skattskyldighet
        assert!(is_input_vat("2649")); // ing. moms blandad verksamhet
    }

    #[test]
    fn excludes_the_settlement_account_and_non_vat_accounts_from_both_sides() {
        for acct in ["2650", "2710", "1930", "3010"] {
            assert!(!is_output_vat(acct), "{acct} should not be output VAT");
            assert!(!is_input_vat(acct), "{acct} should not be input VAT");
        }
        // Output is not input and vice versa.
        assert!(!is_input_vat("2611"));
        assert!(!is_output_vat("2640"));
    }

    // --- beyond the upstream corpus -----------------------------------------

    #[test]
    fn vat_detection_sweeps_the_whole_2610_to_2650_band() {
        // The exact boundaries, account by account, because being one account
        // out here misroutes a VAT return.
        for n in 2610..=2659 {
            let acct = n.to_string();
            let expect_output = (2610..=2639).contains(&n);
            let expect_input = (2640..=2649).contains(&n);
            assert_eq!(is_output_vat(&acct), expect_output, "output VAT at {acct}");
            assert_eq!(is_input_vat(&acct), expect_input, "input VAT at {acct}");
            // Nothing is both.
            assert!(!(is_output_vat(&acct) && is_input_vat(&acct)));
        }
        // The edges either side of the band.
        assert!(!is_output_vat("2609"));
        assert!(!is_input_vat("2639"));
        assert!(!is_input_vat("2650"));
    }

    #[test]
    fn vat_predicates_tolerate_surrounding_whitespace_but_not_junk() {
        assert!(is_output_vat("  2611 "));
        assert!(is_input_vat("\t2640\n"));
        for bad in ["", "   ", "26110", "261", "26a1", "2 611", "abcd"] {
            assert!(!is_output_vat(bad), "{bad:?} should not be output VAT");
            assert!(!is_input_vat(bad), "{bad:?} should not be input VAT");
        }
    }

    #[test]
    fn every_class_maps_to_its_label() {
        let expected = [
            (1u8, "Tillgångar"),
            (2, "Eget kapital & skulder"),
            (3, "Intäkter"),
            (4, "Material & varor"),
            (5, "Övriga externa kostnader"),
            (6, "Övriga externa kostnader"),
            (7, "Personalkostnader"),
            (8, "Finansiella poster"),
        ];
        for (class, label) in expected {
            assert_eq!(class_label(class), Some(label), "class {class}");
        }
        assert_eq!(class_label(0), None);
        assert_eq!(class_label(9), None);
        assert_eq!(CLASS_LABELS.len(), 8);
    }

    #[test]
    fn class_of_reads_the_leading_digit_of_real_accounts() {
        // One account per class, all drawn from the chart the upstream library
        // actually posts to.
        for (acct, class) in [
            ("1930", 1u8), // företagskonto
            ("2610", 2),   // utgående moms
            ("3010", 3),   // försäljning
            ("4000", 4),   // inköp material
            ("5410", 5),   // förbrukningsinventarier
            ("6212", 6),   // mobiltelefon
            ("7010", 7),   // löner
            ("8410", 8),   // räntekostnader
        ] {
            assert_eq!(class_of(acct).unwrap(), class, "class of {acct}");
        }
        assert_eq!(class_of("  1930  ").unwrap(), 1);
    }

    #[test]
    fn result_accounts_are_classes_three_to_eight_and_balance_accounts_one_to_two() {
        for class in 1..=8u8 {
            let acct = format!("{class}000");
            let is_result = (3..=8).contains(&class);
            assert_eq!(is_result_account(&acct), is_result, "result at {acct}");
            assert_eq!(is_balance_account(&acct), !is_result, "balance at {acct}");
            // The two are a partition of the valid classes: exactly one holds.
            assert!(is_result_account(&acct) ^ is_balance_account(&acct));
        }
    }

    #[test]
    fn a_malformed_account_is_an_error_not_a_panic_or_a_silent_zero() {
        for bad in [
            "",         // empty
            "   ",      // whitespace only
            "abcd",     // non-numeric
            "19",       // too short — upstream read this as class 1
            "193",      // too short
            "19300",    // too long
            "19a0",     // partly numeric
            "-193",     // signed
            "0000",     // class 0: the silent zero
            "9000",     // class 9: internal accounts, out of scope
            "１９３０", // full-width digits: numeric to a human, not ASCII
        ] {
            let got = class_of(bad);
            assert!(got.is_err(), "expected {bad:?} to be rejected, got {got:?}");
            // And the predicates built on it stay false rather than panicking.
            assert!(!is_result_account(bad));
            assert!(!is_balance_account(bad));
        }
    }

    #[test]
    fn every_vat_account_is_a_balance_account_in_class_two() {
        for n in 2610..=2649 {
            let acct = n.to_string();
            if is_output_vat(&acct) || is_input_vat(&acct) {
                assert_eq!(class_of(&acct).unwrap(), 2);
                assert!(is_balance_account(&acct));
                assert!(!is_result_account(&acct));
            }
        }
    }
}
