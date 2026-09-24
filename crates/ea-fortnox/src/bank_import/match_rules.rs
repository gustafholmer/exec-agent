//! Pairing a bank line with the receipt that belongs to it.
//!
//! Upstream this is `bank-import/match.ts`. The module is named `match_rules`
//! here only because `match` is a Rust keyword.
//!
//! # The rule
//!
//! A receipt is a candidate for a bank line when **both** hold:
//!
//! * the amounts agree to within [`AMOUNT_TOLERANCE`] — the bank's signed
//!   amount compared by absolute value against the receipt's gross;
//! * the dates are within [`DATE_TOLERANCE_DAYS`] of each other, inclusive.
//!
//! The date window is five days because a card purchase settles to the account
//! a day or three after the receipt is printed, and a weekend stretches that.
//!
//! # Why a receipt can be claimed only once
//!
//! Two coffees at 42 kr on the same day are two bank lines and two receipts,
//! and if the same receipt could satisfy both, one of them would be booked with
//! VAT it has no document for. So assignment is **greedy by nearest date**: the
//! bank line whose date sits closest to the receipt claims it, and a line that
//! loses a contested receipt is left with nothing rather than with someone
//! else's paperwork.
//!
//! A line that still has more than one viable candidate after that is
//! [`MatchOutcome::Multiple`] — not a guess. [`code::code_line`](super::code::code_line)
//! turns that into a blank account and `FLERA`, and the user picks.
//!
//! # Divergences from the TypeScript
//!
//! * **Amounts are compared exactly**, in [`Decimal`], rather than in doubles.
//!   Upstream's `Math.abs(Math.abs(seb.belopp) - kv.beloppInklMoms) <= 0.01`
//!   carries representation error into a comparison against a threshold; the
//!   tolerance is two orders of magnitude larger than that error, so no case in
//!   the corpus changes, but the comparison here has nothing to be near-miss
//!   about.
//! * **`unmatched_receipts` does not take the bank lines.** Upstream's
//!   signature does and never reads them.
//! * An unparseable date is **not a match**, which is what upstream's `NaN <= 5`
//!   already evaluated to. Reproduced rather than turned into an error, because
//!   by the time rows reach this module
//!   [`workbook::read_seb`](super::workbook::read_seb) has already refused
//!   anything that is not `YYYY-MM-DD`.

use chrono::NaiveDate;
use rust_decimal::Decimal;

use super::types::{KvittoRow, SebRow};

/// How far apart two amounts may be and still be the same payment: one öre.
pub const AMOUNT_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

/// How many days apart a receipt and its bank line may be. Inclusive.
pub const DATE_TOLERANCE_DAYS: i64 = 5;

/// What the matcher concluded for one bank line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOutcome {
    /// Exactly one receipt, and it is this line's.
    One,
    /// No receipt. Either none was a candidate, or the candidates all went to
    /// nearer bank lines.
    None,
    /// More than one viable receipt. The user chooses.
    Multiple,
}

/// The matcher's answer for one bank line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// The conclusion.
    pub outcome: MatchOutcome,
    /// The receipt assigned to this line, set only for [`MatchOutcome::One`].
    pub kvitto_index: Option<usize>,
    /// Every receipt that satisfied amount and date, whether or not it was
    /// assigned. Kept for [`MatchOutcome::Multiple`], where it is what the user
    /// is choosing between, and for explaining a [`MatchOutcome::None`] that
    /// had candidates.
    pub candidate_indices: Vec<usize>,
}

impl MatchResult {
    fn none() -> Self {
        Self {
            outcome: MatchOutcome::None,
            kvitto_index: None,
            candidate_indices: Vec::new(),
        }
    }
}

/// Whole days between two `YYYY-MM-DD` dates, or [`None`] if either will not
/// parse.
fn days_between(a: &str, b: &str) -> Option<i64> {
    let a = NaiveDate::parse_from_str(a, "%Y-%m-%d").ok()?;
    let b = NaiveDate::parse_from_str(b, "%Y-%m-%d").ok()?;
    Some((a - b).num_days().abs())
}

fn amount_matches(seb: &SebRow, kv: &KvittoRow) -> bool {
    (seb.belopp.abs() - kv.belopp_inkl_moms).abs() <= AMOUNT_TOLERANCE
}

fn date_matches(seb: &SebRow, kv: &KvittoRow) -> bool {
    days_between(&seb.bokforingsdatum, &kv.datum).is_some_and(|d| d <= DATE_TOLERANCE_DAYS)
}

/// Every receipt that could belong to this bank line, by index, in input order.
pub fn candidates_for(seb: &SebRow, kvitton: &[KvittoRow]) -> Vec<usize> {
    kvitton
        .iter()
        .enumerate()
        .filter(|(_, kv)| amount_matches(seb, kv) && date_matches(seb, kv))
        .map(|(i, _)| i)
        .collect()
}

/// Match every bank line against the receipts, consuming each receipt at most
/// once. One [`MatchResult`] per bank line, in input order.
pub fn match_all(seb: &[SebRow], kvitton: &[KvittoRow]) -> Vec<MatchResult> {
    let candidates: Vec<Vec<usize>> = seb
        .iter()
        .map(|s| candidates_for(s, kvitton))
        .collect();

    // Every (bank line, receipt) pair that is possible at all, nearest first.
    // Ties break on the bank line's own order and then the receipt's, so the
    // assignment is deterministic rather than dependent on iteration order.
    let mut pairs: Vec<(i64, usize, usize)> = Vec::new();
    for (si, cands) in candidates.iter().enumerate() {
        for &ki in cands {
            let dist = days_between(&seb[si].bokforingsdatum, &kvitton[ki].datum)
                .expect("a candidate pair has two parseable dates");
            pairs.push((dist, si, ki));
        }
    }
    pairs.sort_unstable();

    // Greedy: walk the pairs nearest-first, taking each one whose bank line is
    // still unassigned and whose receipt is still unclaimed.
    let mut assigned: Vec<Option<usize>> = vec![None; seb.len()];
    let mut consumed = vec![false; kvitton.len()];
    for (_, si, ki) in pairs {
        if consumed[ki] || assigned[si].is_some() {
            continue;
        }
        assigned[si] = Some(ki);
        consumed[ki] = true;
    }

    // Classify. A line that was assigned a receipt is still `Multiple` if it
    // had another candidate nobody else took — the assignment picked one, but
    // the ambiguity the user has to resolve is real.
    (0..seb.len())
        .map(|si| {
            let cands = &candidates[si];
            match assigned[si] {
                Some(owner) => {
                    let viable = cands
                        .iter()
                        .filter(|&&ki| ki == owner || !consumed[ki])
                        .count();
                    if viable > 1 {
                        MatchResult {
                            outcome: MatchOutcome::Multiple,
                            kvitto_index: None,
                            candidate_indices: cands.clone(),
                        }
                    } else {
                        MatchResult {
                            outcome: MatchOutcome::One,
                            kvitto_index: Some(owner),
                            candidate_indices: cands.clone(),
                        }
                    }
                }
                // Had candidates, but every one went to a nearer bank line.
                None if !cands.is_empty() => MatchResult {
                    outcome: MatchOutcome::None,
                    kvitto_index: None,
                    candidate_indices: cands.clone(),
                },
                None => MatchResult::none(),
            }
        })
        .collect()
}

/// Receipts no bank line claimed, by index.
///
/// These are the ones worth showing the user: a receipt with no bank line is
/// either a purchase paid some other way or a bank export that does not reach
/// far enough back.
pub fn unmatched_receipts(kvitton: &[KvittoRow], results: &[MatchResult]) -> Vec<usize> {
    let mut consumed = vec![false; kvitton.len()];
    for r in results {
        if r.outcome == MatchOutcome::One {
            if let Some(ki) = r.kvitto_index {
                consumed[ki] = true;
            }
        }
    }
    (0..kvitton.len()).filter(|&i| !consumed[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn seb(datum: &str, belopp: &str) -> SebRow {
        SebRow {
            bokforingsdatum: datum.to_string(),
            text: "X".to_string(),
            belopp: dec(belopp),
            saldo: None,
        }
    }

    fn kv(datum: &str, belopp: &str) -> KvittoRow {
        KvittoRow {
            datum: datum.to_string(),
            belopp_inkl_moms: dec(belopp),
            ..KvittoRow::default()
        }
    }

    #[test]
    fn the_one_ore_tolerance_is_exactly_one_ore() {
        assert_eq!(AMOUNT_TOLERANCE, dec("0.01"));
    }

    // ---- candidatesFor -----------------------------------------------------

    #[test]
    fn a_candidate_needs_both_the_amount_and_the_date() {
        let s = seb("2025-08-10", "-1250");
        let ks = [
            kv("2025-08-12", "1250"),     // ok
            kv("2025-08-12", "1250.009"), // ok: inside the one-öre tolerance
            kv("2025-08-12", "1250.5"),   // amount too far
            kv("2025-08-20", "1250"),     // date too far
        ];
        assert_eq!(candidates_for(&s, &ks), vec![0, 1]);
    }

    #[test]
    fn the_five_day_window_is_inclusive_at_both_ends() {
        let s = seb("2025-08-10", "-100");
        assert_eq!(candidates_for(&s, &[kv("2025-08-15", "100")]), vec![0]);
        assert_eq!(candidates_for(&s, &[kv("2025-08-05", "100")]), vec![0]);
        // Six days, either side: no.
        assert!(candidates_for(&s, &[kv("2025-08-16", "100")]).is_empty());
        assert!(candidates_for(&s, &[kv("2025-08-04", "100")]).is_empty());
    }

    #[test]
    fn the_one_ore_tolerance_is_inclusive() {
        let s = seb("2025-08-10", "-100");
        assert_eq!(candidates_for(&s, &[kv("2025-08-10", "100.01")]), vec![0]);
        assert_eq!(candidates_for(&s, &[kv("2025-08-10", "99.99")]), vec![0]);
        assert!(candidates_for(&s, &[kv("2025-08-10", "100.011")]).is_empty());
    }

    #[test]
    fn the_amount_is_compared_by_absolute_value() {
        // The bank writes an expense negative; the receipt writes it positive.
        assert_eq!(
            candidates_for(&seb("2025-08-10", "-500"), &[kv("2025-08-10", "500")]),
            vec![0]
        );
        // And an income line matches a receipt the same way.
        assert_eq!(
            candidates_for(&seb("2025-08-10", "500"), &[kv("2025-08-10", "500")]),
            vec![0]
        );
    }

    #[test]
    fn an_unparseable_date_is_not_a_match() {
        // Upstream's `NaN <= 5` was already false. Reproduced, not upgraded to
        // an error: the workbook reader refuses such a date long before here.
        let s = seb("inte ett datum", "-100");
        assert!(candidates_for(&s, &[kv("2025-08-10", "100")]).is_empty());
        let s = seb("2025-08-10", "-100");
        assert!(candidates_for(&s, &[kv("i förrgår", "100")]).is_empty());
    }

    #[test]
    fn a_month_boundary_is_counted_in_days_not_in_date_arithmetic() {
        // 2025-08-31 to 2025-09-02 is two days, not a month.
        assert_eq!(
            candidates_for(&seb("2025-08-31", "-100"), &[kv("2025-09-02", "100")]),
            vec![0]
        );
    }

    // ---- outcomes ----------------------------------------------------------

    #[test]
    fn exactly_one_receipt_is_one() {
        let s = [seb("2025-08-10", "-500")];
        let k = [kv("2025-08-11", "500")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].outcome, MatchOutcome::One);
        assert_eq!(r[0].kvitto_index, Some(0));
    }

    #[test]
    fn no_receipt_is_none() {
        let s = [seb("2025-08-10", "-500")];
        let k = [kv("2025-08-11", "999")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].outcome, MatchOutcome::None);
        assert_eq!(r[0].kvitto_index, None);
        assert!(r[0].candidate_indices.is_empty());
    }

    #[test]
    fn two_receipts_for_one_line_is_multiple_and_assigns_neither() {
        let s = [seb("2025-08-10", "-500")];
        let k = [kv("2025-08-10", "500"), kv("2025-08-11", "500")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].outcome, MatchOutcome::Multiple);
        assert_eq!(r[0].candidate_indices, vec![0, 1]);
        // The user chooses, so nothing is assigned and nothing is consumed.
        assert_eq!(r[0].kvitto_index, None);
        assert_eq!(unmatched_receipts(&k, &r), vec![0, 1]);
    }

    #[test]
    fn no_line_matched_gives_an_empty_result_for_no_lines() {
        assert!(match_all(&[], &[kv("2025-08-10", "500")]).is_empty());
        assert_eq!(match_all(&[seb("2025-08-10", "-1")], &[]).len(), 1);
        assert_eq!(
            match_all(&[seb("2025-08-10", "-1")], &[])[0].outcome,
            MatchOutcome::None
        );
    }

    // ---- receipt reuse -----------------------------------------------------

    #[test]
    fn a_receipt_goes_to_the_nearest_bank_line_and_the_other_gets_nothing() {
        let s = [
            seb("2025-08-09", "-500"), // one day from the receipt
            seb("2025-08-12", "-500"), // two days from the receipt
        ];
        let k = [kv("2025-08-10", "500")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].outcome, MatchOutcome::One);
        assert_eq!(r[0].kvitto_index, Some(0));
        assert_eq!(r[1].outcome, MatchOutcome::None);
        // The losing line still reports what it saw, so the reason it has no
        // receipt is visible rather than indistinguishable from "none existed".
        assert_eq!(r[1].candidate_indices, vec![0]);
        assert!(unmatched_receipts(&k, &r).is_empty());
    }

    #[test]
    fn the_nearest_line_wins_regardless_of_input_order() {
        // The same pair with the bank lines swapped: the *nearer* line must win
        // both times, not the first one listed.
        let near = seb("2025-08-09", "-500");
        let far = seb("2025-08-12", "-500");
        let k = [kv("2025-08-10", "500")];

        let r = match_all(&[near.clone(), far.clone()], &k);
        assert_eq!(r[0].kvitto_index, Some(0));
        assert_eq!(r[1].kvitto_index, None);

        let r = match_all(&[far, near], &k);
        assert_eq!(r[0].kvitto_index, None);
        assert_eq!(r[1].kvitto_index, Some(0));
    }

    #[test]
    fn an_equidistant_tie_goes_to_the_earlier_bank_line_deterministically() {
        let s = [seb("2025-08-09", "-500"), seb("2025-08-11", "-500")];
        let k = [kv("2025-08-10", "500")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].kvitto_index, Some(0));
        assert_eq!(r[1].kvitto_index, None);
    }

    #[test]
    fn two_lines_and_two_receipts_pair_off_rather_than_both_reporting_multiple() {
        // The case the greedy pass exists for. Both receipts are candidates for
        // both lines, but once each line has taken the nearer one, neither has
        // a second viable candidate left and both are a clean `One`.
        let s = [seb("2025-08-10", "-500"), seb("2025-08-14", "-500")];
        let k = [kv("2025-08-10", "500"), kv("2025-08-14", "500")];
        let r = match_all(&s, &k);
        assert_eq!(r[0].outcome, MatchOutcome::One);
        assert_eq!(r[0].kvitto_index, Some(0));
        assert_eq!(r[1].outcome, MatchOutcome::One);
        assert_eq!(r[1].kvitto_index, Some(1));
        assert!(unmatched_receipts(&k, &r).is_empty());
    }

    #[test]
    fn unmatched_receipts_are_reported() {
        let s = [seb("2025-08-10", "-500")];
        let k = [
            kv("2025-08-10", "500"),
            kv("2025-08-10", "700"), // no bank line
        ];
        let r = match_all(&s, &k);
        assert_eq!(unmatched_receipts(&k, &r), vec![1]);
    }

    /// Differential against the **compiled upstream**, over contended inputs.
    ///
    /// `tests/.upstream-match-differential.json` holds 1,800 scenarios run
    /// through `dist/bank-import/match.js` under node, generated with a
    /// deterministic LCG in **two shapes**, because one shape was measurably
    /// not enough:
    ///
    /// * **A** — more receipts than bank lines, over eight dates and two
    ///   amounts. This is what reaches `multiple`, the branch hand-written
    ///   tests reach least.
    /// * **B** — several bank lines competing for one or two receipts, over
    ///   twelve dates. A first draft used only shape A, and a mutation that
    ///   replaced *nearest date wins* with *first line wins* passed all 1,500
    ///   of its cases: measured over the recorded data, only 13 had a contested
    ///   receipt that was nonetheless assigned, and in none of them was the
    ///   first line something other than the nearest. Shape B exists to close
    ///   that. The combined corpus now has 571 contested-and-assigned receipts,
    ///   147 of which discriminate the two rules, and the mutation fails.
    ///
    /// Outcome counts across the 2,706 lines: 1,176 `none`, 1,005 `one`,
    /// 525 `multiple`.
    ///
    /// The outcome, the assigned receipt, the candidate list and the unmatched
    /// set are all compared. Greedy assignment with a post-hoc reclassification
    /// is the kind of algorithm where a translation can agree on every example
    /// anyone thought to write and still differ; this is the check that it does
    /// not.
    #[test]
    fn matches_the_compiled_upstream_over_eighteen_hundred_contended_scenarios() {
        #[derive(serde::Deserialize)]
        struct Case {
            s: Vec<(String, f64)>,
            k: Vec<(String, f64)>,
            r: Vec<(String, Option<usize>, Vec<usize>)>,
            u: Vec<usize>,
        }

        let raw = include_str!("../../tests/.upstream-match-differential.json");
        let cases: Vec<Case> = serde_json::from_str(raw).unwrap();
        assert_eq!(cases.len(), 1800, "the recorded differential changed size");

        let mut outcomes = 0usize;
        for (n, case) in cases.iter().enumerate() {
            let sebs: Vec<SebRow> = case
                .s
                .iter()
                .map(|(d, b)| seb(d, &b.to_string()))
                .collect();
            let kvs: Vec<KvittoRow> = case
                .k
                .iter()
                .map(|(d, b)| kv(d, &b.to_string()))
                .collect();

            let got = match_all(&sebs, &kvs);
            assert_eq!(got.len(), case.r.len(), "case {n}: wrong number of results");

            for (i, (outcome, index, candidates)) in case.r.iter().enumerate() {
                let want = match outcome.as_str() {
                    "one" => MatchOutcome::One,
                    "none" => MatchOutcome::None,
                    "multiple" => MatchOutcome::Multiple,
                    other => panic!("case {n}: unknown upstream outcome {other:?}"),
                };
                assert_eq!(got[i].outcome, want, "case {n} line {i}: outcome");
                assert_eq!(got[i].kvitto_index, *index, "case {n} line {i}: assigned receipt");
                assert_eq!(&got[i].candidate_indices, candidates, "case {n} line {i}: candidates");
                outcomes += 1;
            }

            assert_eq!(unmatched_receipts(&kvs, &got), case.u, "case {n}: unmatched receipts");
        }
        assert_eq!(outcomes, 2706, "the differential should cover 2706 outcomes, covered {outcomes}");
    }

    #[test]
    fn a_receipt_is_never_claimed_twice() {
        // The property the whole module exists for, over a case with more
        // contention than any single assertion above.
        let s: Vec<SebRow> = (10..=14)
            .map(|d| seb(&format!("2025-08-{d}"), "-500"))
            .collect();
        let k: Vec<KvittoRow> = (11..=13)
            .map(|d| kv(&format!("2025-08-{d}"), "500"))
            .collect();

        let r = match_all(&s, &k);
        let mut claimed: Vec<usize> = r.iter().filter_map(|x| x.kvitto_index).collect();
        let before = claimed.len();
        claimed.sort_unstable();
        claimed.dedup();
        assert_eq!(claimed.len(), before, "a receipt was claimed more than once");
        assert_eq!(r.len(), s.len());
    }
}
