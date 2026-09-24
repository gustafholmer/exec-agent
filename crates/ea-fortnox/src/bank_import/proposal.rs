//! The pipeline: bank lines plus receipts become the `Förslag` review sheet.
//!
//! Upstream this is `bank-import/proposal.ts`, and it is the one module in this
//! task that **has no upstream test**. Translating it means writing tests that
//! never existed rather than porting them, so it is held to this project's own
//! standard rather than judged as a translation: the tests below were written
//! from the behaviour the module should have, not recovered from a `.test.ts`.
//! Where they pin something, they pin a decision made here.
//!
//! The module itself is thin, and deliberately so. It composes
//! [`match_rules::match_all`](super::match_rules::match_all) and
//! [`code::code_line`](super::code::code_line), each of which is tested against
//! the compiled upstream, and adds two things of its own: the receipt
//! description that ends up in the `Matchad_kvitto` column, and the decision
//! that a multiply-matched line is passed to the coder as `multiple` rather
//! than with one of its candidates picked arbitrarily.
//!
//! # What the user sees
//!
//! One row per bank line, in export order, with `Godkänn` blank. Nothing is
//! posted until a human writes `J` in that column and runs `post --commit`, so
//! this function's output is a proposal in the literal sense.

use anyhow::{Context, Result};

use super::code::{code_line, CodeInput};
use super::match_rules::{match_all, MatchOutcome};
use super::types::{Direction, ForslagRow, KvittoRow, SebRow};
use super::voucher::rad_id_for_seb;

/// A matched receipt, described for the `Matchad_kvitto` column.
///
/// Who, when, how much, and the user's own reference if they gave one — enough
/// for the reviewer to find the paper receipt without leaving the sheet. The
/// amount is [`Decimal::normalize`]d so `89.00` reads as `89`, matching how the
/// same number is written everywhere else in the sheet.
fn describe_kvitto(kv: &KvittoRow) -> String {
    let who = kv
        .leverantor
        .as_deref()
        .or(kv.beskrivning.as_deref())
        .unwrap_or("kvitto");
    let amount = kv.belopp_inkl_moms.normalize();
    match kv.kvitto_ref.as_deref() {
        Some(reference) if !reference.is_empty() => {
            format!("{who} {} {amount} ({reference})", kv.datum)
        }
        _ => format!("{who} {} {amount}", kv.datum),
    }
}

/// Build the proposed `Förslag` rows. Pure given its inputs.
///
/// One row per bank line, in input order. A line that matched exactly one
/// receipt is coded from it; a line that matched several is handed to the coder
/// as ambiguous and comes back `FLERA` with a blank account, because picking
/// one of them here would book VAT against a document that may not be the
/// right one.
///
/// # Errors
///
/// Only from coding: a receipt with an unsupported or fractional `Momssats`.
/// The error names the bank line it came from, because the user's next move is
/// to go and fix that row.
pub fn build_proposal(
    seb: &[SebRow],
    kvitton: &[KvittoRow],
    bank_account: Option<&str>,
) -> Result<Vec<ForslagRow>> {
    let matches = match_all(seb, kvitton);

    seb.iter()
        .zip(&matches)
        .map(|(line, m)| {
            let kvitto = match (m.outcome, m.kvitto_index) {
                (MatchOutcome::One, Some(i)) => kvitton.get(i),
                _ => None,
            };
            let coding = code_line(CodeInput {
                seb: line,
                kvitto,
                multiple: m.outcome == MatchOutcome::Multiple,
                bank_account,
            })
            .with_context(|| {
                format!(
                    "coding the bank line of {} ({})",
                    line.bokforingsdatum, line.text
                )
            })?;

            Ok(ForslagRow {
                rad_id: rad_id_for_seb(line),
                datum: line.bokforingsdatum.clone(),
                text: line.text.clone(),
                belopp: line.belopp,
                riktning: Direction::of_amount(line.belopp),
                bas_konto: coding.bas_konto,
                momssats: Some(coding.momssats),
                moms_kr: Some(coding.moms_kr),
                // Left for the user; the pipeline has no account-name table
                // and inventing one would be a second source of truth for the
                // BAS chart.
                konto_namn: String::new(),
                konfidens: coding.confidence,
                motivering: coding.motivering,
                matchad_kvitto: kvitto.map(describe_kvitto).unwrap_or_default(),
                // Nothing is ever proposed as approved.
                godkann: String::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank_import::types::Confidence;
    use crate::bank_import::workbook;
    use rust_decimal::Decimal;
    use std::path::PathBuf;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn seb(datum: &str, text: &str, belopp: &str) -> SebRow {
        SebRow {
            bokforingsdatum: datum.to_string(),
            text: text.to_string(),
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

    // ---- shape -------------------------------------------------------------

    #[test]
    fn there_is_one_row_per_bank_line_in_input_order() {
        let lines = [
            seb("2026-06-01", "A", "-10"),
            seb("2026-06-02", "B", "-20"),
            seb("2026-06-03", "C", "30"),
        ];
        let rows = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
    }

    #[test]
    fn no_rows_in_no_rows_out() {
        assert!(build_proposal(&[], &[kv("2026-06-01", "10")], None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn nothing_is_ever_proposed_as_approved() {
        let lines = [seb("2026-06-01", "SPOTIFY", "-100")];
        let rows = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(
            rows[0].godkann, "",
            "a proposal must never pre-approve itself"
        );
        assert_eq!(rows[0].konto_namn, "");
    }

    #[test]
    fn the_direction_follows_the_sign_of_the_amount() {
        let lines = [
            seb("2026-06-01", "UT", "-1"),
            seb("2026-06-01", "IN", "1"),
            seb("2026-06-01", "NOLL", "0"),
        ];
        let rows = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(rows[0].riktning, Direction::Utgift);
        assert_eq!(rows[1].riktning, Direction::Inkomst);
        // Zero is Inkomst, as `Direction::of_amount` documents.
        assert_eq!(rows[2].riktning, Direction::Inkomst);
    }

    #[test]
    fn the_row_id_is_the_bank_lines_own_key() {
        let line = seb("2026-06-01", "SWISH KAFFE", "-42");
        let rows = build_proposal(std::slice::from_ref(&line), &[], None).unwrap();
        assert_eq!(rows[0].rad_id, rad_id_for_seb(&line));
        // And it is the key the TypeScript would have produced.
        assert_eq!(rows[0].rad_id, "5d88deb56e17");
    }

    #[test]
    fn the_same_export_proposes_the_same_keys_twice() {
        // What makes `post` safe to re-run: the ids must not move between runs.
        let lines = [seb("2026-06-01", "A", "-10"), seb("2026-06-02", "B", "-20")];
        let first = build_proposal(&lines, &[], None).unwrap();
        let second = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(first, second);
    }

    // ---- coding through a matched receipt ----------------------------------

    #[test]
    fn a_matched_receipt_codes_the_row_and_is_described() {
        let lines = [seb("2026-06-02", "SPOTIFY AB", "-1234.56")];
        let receipts = [KvittoRow {
            leverantor: Some("Spotify AB".to_string()),
            momssats: Some(dec("25")),
            kategori: Some("programvara".to_string()),
            kvitto_ref: Some("KV-001".to_string()),
            ..kv("2026-06-02", "1234.56")
        }];
        let rows = build_proposal(&lines, &receipts, None).unwrap();

        assert_eq!(rows[0].bas_konto, "5420");
        assert_eq!(rows[0].konfidens, Confidence::Hog);
        assert_eq!(rows[0].momssats, Some(dec("25")));
        assert_eq!(
            rows[0].matchad_kvitto,
            "Spotify AB 2026-06-02 1234.56 (KV-001)"
        );
        assert!(
            rows[0].motivering.contains("programvara"),
            "{}",
            rows[0].motivering
        );
    }

    #[test]
    fn an_unmatched_line_falls_through_to_the_keyword_table() {
        let lines = [seb("2026-06-02", "SPOTIFY AB STOCKHOLM", "-99")];
        let rows = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(rows[0].bas_konto, "5420");
        assert_eq!(rows[0].konfidens, Confidence::Lag);
        assert_eq!(rows[0].matchad_kvitto, "");
    }

    #[test]
    fn a_line_nothing_recognises_is_okand_with_a_blank_account() {
        let lines = [seb("2026-06-02", "ÖVERFÖRING 4711", "-99")];
        let rows = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(rows[0].bas_konto, "");
        assert_eq!(rows[0].konfidens, Confidence::Okand);
        assert_eq!(rows[0].momssats, Some(Decimal::ZERO));
    }

    #[test]
    fn a_multiply_matched_line_is_flera_with_no_receipt_named() {
        let lines = [seb("2026-06-02", "SPOTIFY AB", "-500")];
        let receipts = [
            KvittoRow {
                kategori: Some("programvara".to_string()),
                ..kv("2026-06-02", "500")
            },
            KvittoRow {
                kategori: Some("resor".to_string()),
                ..kv("2026-06-03", "500")
            },
        ];
        let rows = build_proposal(&lines, &receipts, None).unwrap();

        assert_eq!(rows[0].konfidens, Confidence::Flera);
        assert_eq!(rows[0].bas_konto, "", "an ambiguous line must not be coded");
        // Neither candidate is named, because naming one would suggest it was
        // chosen. And the SPOTIFY keyword must not sneak in either.
        assert_eq!(rows[0].matchad_kvitto, "");
    }

    #[test]
    fn a_receipt_claimed_by_a_nearer_line_is_not_described_on_the_farther_one() {
        let lines = [
            seb("2026-06-02", "KAFFE", "-42"), // one day away
            seb("2026-06-05", "KAFFE", "-42"), // two days away
        ];
        let receipts = [KvittoRow {
            leverantor: Some("Kafé Ö".to_string()),
            ..kv("2026-06-03", "42")
        }];
        let rows = build_proposal(&lines, &receipts, None).unwrap();
        assert_eq!(rows[0].matchad_kvitto, "Kafé Ö 2026-06-03 42");
        assert_eq!(rows[1].matchad_kvitto, "");
    }

    #[test]
    fn the_bank_account_is_passed_through_to_the_coder() {
        // It does not appear in the sheet — the Förslag row has no bank column
        // — but it must reach `code_line`, because that is where the default
        // would otherwise be baked in. Observed through the voucher the row
        // later produces.
        use crate::bank_import::voucher::{forslag_to_voucher_input, VoucherOptions};
        let lines = [seb("2026-06-02", "SPOTIFY", "-100")];
        let rows = build_proposal(&lines, &[], Some("1932")).unwrap();
        let input = forslag_to_voucher_input(
            &ForslagRow {
                godkann: "J".to_string(),
                ..rows[0].clone()
            },
            VoucherOptions {
                bank_account: Some("1932"),
                series: None,
            },
        )
        .unwrap();
        assert!(input.lines.iter().any(|l| l.account == "1932"));
    }

    // ---- describe_kvitto ---------------------------------------------------

    #[test]
    fn a_receipt_is_described_by_supplier_then_description_then_a_placeholder() {
        let base = kv("2026-06-01", "42");
        assert_eq!(
            describe_kvitto(&KvittoRow {
                leverantor: Some("Kafé Ö".to_string()),
                beskrivning: Some("Fika".to_string()),
                ..base.clone()
            }),
            "Kafé Ö 2026-06-01 42",
            "the supplier wins over the description"
        );
        assert_eq!(
            describe_kvitto(&KvittoRow {
                beskrivning: Some("Fika".to_string()),
                ..base.clone()
            }),
            "Fika 2026-06-01 42"
        );
        assert_eq!(describe_kvitto(&base), "kvitto 2026-06-01 42");
    }

    #[test]
    fn the_reference_is_appended_only_when_there_is_one() {
        let base = kv("2026-06-01", "42");
        assert_eq!(
            describe_kvitto(&KvittoRow {
                kvitto_ref: Some("KV-9".to_string()),
                ..base.clone()
            }),
            "kvitto 2026-06-01 42 (KV-9)"
        );
        // An empty reference must not leave a bare " ()" in the sheet.
        assert_eq!(
            describe_kvitto(&KvittoRow {
                kvitto_ref: Some(String::new()),
                ..base
            }),
            "kvitto 2026-06-01 42"
        );
    }

    #[test]
    fn the_described_amount_drops_its_trailing_zeros() {
        // The sheet writes 89, not 89.00, everywhere else.
        assert_eq!(
            describe_kvitto(&kv("2026-06-01", "89.00")),
            "kvitto 2026-06-01 89"
        );
        assert_eq!(
            describe_kvitto(&kv("2026-06-01", "89.50")),
            "kvitto 2026-06-01 89.5"
        );
    }

    // ---- errors ------------------------------------------------------------

    #[test]
    fn a_receipt_with_an_impossible_vat_rate_names_the_bank_line_it_broke() {
        let lines = [seb("2026-06-02", "NÅGOT", "-1200")];
        let receipts = [KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("20")),
            ..kv("2026-06-02", "1200")
        }];
        let err = build_proposal(&lines, &receipts, None).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("2026-06-02"), "{chain}");
        assert!(chain.contains("NÅGOT"), "{chain}");
    }

    // ---- end to end --------------------------------------------------------

    #[test]
    fn the_fixture_workbook_proposes_a_reviewable_sheet() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bank-import-sample.xlsx");
        let wb = workbook::Workbook::open(path).unwrap();
        let seb = workbook::read_seb(&wb).unwrap();
        let kvitton = workbook::read_kvitton(&wb).unwrap();
        let rows = build_proposal(&seb, &kvitton, None).unwrap();

        assert_eq!(rows.len(), 4, "one row per bank line the fixture carries");

        // SWISH KAFFE: no receipt, no keyword.
        assert_eq!(rows[0].konfidens, Confidence::Okand);
        assert_eq!(rows[0].bas_konto, "");

        // SPOTIFY: the receipt's category wins over the SPOTIFY keyword, and
        // both happen to say 5420 — so the confidence is what distinguishes
        // which rule fired.
        assert_eq!(rows[1].bas_konto, "5420");
        assert_eq!(rows[1].konfidens, Confidence::Hog);
        assert_eq!(
            rows[1].matchad_kvitto,
            "Spotify AB 2026-06-02 1234.56 (KV-001)"
        );
        assert_eq!(rows[1].moms_kr, Some(dec("246.91"))); // 1234.56 - 1234.56/1.25

        // CIRCLE K: the receipt pre-codes 5611 by hand and states its VAT.
        assert_eq!(rows[2].bas_konto, "5611");
        assert_eq!(rows[2].konfidens, Confidence::Hog);
        assert_eq!(rows[2].moms_kr, Some(dec("17.80")));

        // The income line.
        assert_eq!(rows[3].riktning, Direction::Inkomst);
        assert_eq!(rows[3].belopp, dec("6250"));

        // Nothing is approved, and every row carries a distinct key.
        assert!(rows.iter().all(|r| r.godkann.is_empty()));
        let mut ids: Vec<&str> = rows.iter().map(|r| r.rad_id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len());
    }

    #[test]
    fn a_reviewed_sheet_survives_a_re_run_of_the_proposer() {
        // The loop the whole pipeline exists for: propose, the user edits and
        // approves, the bank export grows, propose again — and the review
        // survives.
        let mut lines = vec![seb("2026-06-01", "SWISH KAFFE", "-42")];
        let first = build_proposal(&lines, &[], None).unwrap();
        assert_eq!(first[0].konfidens, Confidence::Okand);
        assert_eq!(first[0].bas_konto, "");

        // The user fills in the account and approves it.
        let reviewed = vec![ForslagRow {
            bas_konto: "6071".to_string(),
            momssats: Some(dec("12")),
            godkann: "J".to_string(),
            ..first[0].clone()
        }];

        // A week later the export has one more line.
        lines.push(seb("2026-06-08", "SPOTIFY AB", "-119"));
        let second = build_proposal(&lines, &[], None).unwrap();
        let merged = workbook::merge_forslag(&second, &reviewed);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].bas_konto, "6071", "the user's account survived");
        assert_eq!(merged[0].momssats, Some(dec("12")));
        assert_eq!(merged[0].godkann, "J", "the approval survived");
        assert_eq!(merged[1].bas_konto, "5420", "the new line was coded");
        assert_eq!(merged[1].godkann, "", "and is not approved");
    }

    #[test]
    fn the_proposal_round_trips_through_a_workbook() {
        let seb = vec![seb("2026-06-01", "SPOTIFY AB", "-125")];
        let rows = build_proposal(&seb, &[], None).unwrap();
        let bytes = workbook::write_workbook_to_buffer(&seb, &[], &rows).unwrap();
        let back =
            workbook::read_forslag(&workbook::Workbook::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(back, rows);
    }
}
