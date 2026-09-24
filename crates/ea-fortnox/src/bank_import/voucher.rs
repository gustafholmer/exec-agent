//! An approved `Förslag` row becomes a balanced Fortnox voucher.
//!
//! Upstream this is `bank-import/voucher.ts`.
//!
//! # The two shapes
//!
//! **Utgift** (`Belopp < 0`) mirrors
//! [`posting::build_expense_voucher`](crate::domain::posting::build_expense_voucher):
//!
//! ```text
//! debit  <BAS_konto>   net
//! debit  2640          VAT        (only when there is VAT)
//! credit <bank>        gross
//! ```
//!
//! **Inkomst** (`Belopp >= 0`) is its mirror, with output VAT instead of input:
//!
//! ```text
//! debit  <bank>        gross
//! credit <BAS_konto>   net
//! credit <2611|2621|2631>  VAT     (only when there is VAT)
//! ```
//!
//! # Idempotency
//!
//! Every voucher's `Description` ends with `[imp:<Rad_id>]`, and `Rad_id` is a
//! hash of the bank line's date, amount and text. [`post`](super::post) reads
//! the markers off the vouchers already in the financial year and skips any row
//! whose marker is there, so running `post --commit` twice books each line
//! once. That is the only thing standing between a re-run and a duplicated
//! ledger, which is why [`rad_id`] is pinned byte-for-byte against the
//! TypeScript below.
//!
//! # Divergences from the TypeScript
//!
//! * **There is no `VoucherMappingError` type.** Upstream defines one so
//!   callers can tell a mapping failure from a transport failure, and then its
//!   only caller does not: `post.ts` writes
//!   `err instanceof VoucherMappingError ? err.message : (err as Error).message`,
//!   whose two branches are the same expression. Nothing observable was lost by
//!   returning [`anyhow::Error`].
//! * **[`assert_balanced`] is stricter than upstream's**, and rejects a line
//!   with both columns filled, neither filled, or a negative amount. See
//!   [`crate::domain::voucher`]. Reachable here through a zero-kronor bank line,
//!   which has nothing to book.
//! * **An unsupported VAT rate is an error**, as in [`super::code`].

use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use serde_json::Value;
use sha1::{Digest, Sha1};

use super::code::{output_vat_account, resolve_vat, DEFAULT_BANK_ACCOUNT, INPUT_VAT_ACCOUNT};
use super::types::{Direction, ForslagRow, SebRow};
use crate::domain::money::round2;
use crate::domain::voucher::{assert_balanced, build_payload, BuildVoucherInput, VoucherLine};

/// The voucher series bank-imported vouchers land in.
pub const DEFAULT_SERIES: &str = "A";

/// Options for turning a row into a voucher.
#[derive(Debug, Clone, Copy, Default)]
pub struct VoucherOptions<'a> {
    /// The bank account, defaulting to
    /// [`DEFAULT_BANK_ACCOUNT`](super::code::DEFAULT_BANK_ACCOUNT).
    pub bank_account: Option<&'a str>,
    /// The voucher series, defaulting to [`DEFAULT_SERIES`].
    pub series: Option<&'a str>,
}

/// The stable idempotency key for a bank line: date, amount and text, hashed.
///
/// The first 12 hex characters of `SHA-1("<date>|<amount>|<text>")`.
///
/// # Byte-compatibility with the TypeScript, and why it matters
///
/// This key is what stops a second `post --commit` from booking every line
/// twice. Vouchers already in Fortnox carry keys the **TypeScript** computed,
/// so a Rust key that differs by one character would not match them and the
/// whole ledger would be re-posted. The values are therefore pinned against
/// `dist/bank-import/voucher.js` run under node, in
/// [`the_key_is_byte_compatible_with_the_typescript`](self).
///
/// The one place this could have drifted is the amount. Upstream interpolates
/// `${round2(belopp)}`, and JavaScript's `Number` → `String` gives the shortest
/// form: `-1250`, never `-1250.00`. A [`Decimal`] carries its scale, so
/// `-1250.00` would render with its trailing zeros and hash differently from
/// the same amount written `-1250` — and a spreadsheet hands out both spellings
/// for the same number. The amount is therefore
/// [`Decimal::normalize`]d before formatting, which reproduces JavaScript's
/// shortest form for every value with two decimals or fewer. The
/// `scale_does_not_change_the_key` test pins that.
pub fn rad_id(bokforingsdatum: &str, belopp: Decimal, text: &str) -> String {
    let amount = round2(belopp).normalize();
    let basis = format!("{bokforingsdatum}|{amount}|{text}");
    let digest = Sha1::digest(basis.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// [`rad_id`] for a bank line.
pub fn rad_id_for_seb(seb: &SebRow) -> String {
    rad_id(&seb.bokforingsdatum, seb.belopp, &seb.text)
}

/// The marker embedded in a voucher description.
pub fn imp_marker(id: &str) -> String {
    format!("[imp:{id}]")
}

/// Every `Rad_id` a voucher description references, lowercased.
///
/// Upstream's `/\[imp:([0-9a-f]+)\]/gi`, hand-written because the pattern is
/// small and a regex engine is a large dependency for it. Case-insensitive on
/// the hex, as the `i` flag was; the results are lowercased so a marker written
/// in either case compares equal.
pub fn extract_markers(description: Option<&str>) -> Vec<String> {
    const OPEN: &str = "[imp:";
    let Some(description) = description else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut rest = description;
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let hex_len = after.bytes().take_while(u8::is_ascii_hexdigit).count();
        // `+` in the pattern: at least one hex digit, then the closing bracket.
        if hex_len > 0 && after.as_bytes().get(hex_len) == Some(&b']') {
            out.push(after[..hex_len].to_ascii_lowercase());
            rest = &after[hex_len + 1..];
        } else {
            // Not a marker after all; resume scanning just past this `[`.
            rest = &rest[at + 1..];
        }
    }
    out
}

/// Turn an approved row into the input for a balanced voucher.
///
/// # Errors
///
/// * a blank `BAS_konto` — the row is not codeable and must not be posted;
/// * an unsupported or fractional `Momssats`;
/// * an income line whose rate has no output-VAT account;
/// * lines that do not balance, or that
///   [`assert_balanced`] rejects on their shape.
pub fn forslag_to_voucher_input(
    row: &ForslagRow,
    opts: VoucherOptions<'_>,
) -> Result<BuildVoucherInput> {
    let bank_account = opts.bank_account.unwrap_or(DEFAULT_BANK_ACCOUNT);
    let series = opts.series.unwrap_or(DEFAULT_SERIES);

    let account = row.bas_konto.trim();
    if account.is_empty() {
        bail!(
            "row {}: BAS_konto is blank, so there is nothing to book against",
            row.rad_id
        );
    }

    let gross = round2(row.belopp.abs());
    let rate = row.momssats.unwrap_or(Decimal::ZERO);
    let vat = resolve_vat(gross, rate, row.moms_kr)
        .with_context(|| format!("row {}: resolving VAT", row.rad_id))?;
    let net = round2(gross - vat);
    let description = format!("{} {}", row.text, imp_marker(&row.rad_id));
    let info = Some(row.text.clone());

    let lines = match row.riktning {
        Direction::Utgift => {
            let mut lines = vec![VoucherLine {
                account: account.to_string(),
                debit: net,
                credit: Decimal::ZERO,
                info: info.clone(),
            }];
            if vat > Decimal::ZERO {
                lines.push(VoucherLine {
                    account: INPUT_VAT_ACCOUNT.to_string(),
                    debit: vat,
                    credit: Decimal::ZERO,
                    info: Some("Ingående moms".to_string()),
                });
            }
            lines.push(VoucherLine {
                account: bank_account.to_string(),
                debit: Decimal::ZERO,
                credit: gross,
                info,
            });
            lines
        }
        Direction::Inkomst => {
            let mut lines = vec![
                VoucherLine {
                    account: bank_account.to_string(),
                    debit: gross,
                    credit: Decimal::ZERO,
                    info: info.clone(),
                },
                VoucherLine {
                    account: account.to_string(),
                    debit: Decimal::ZERO,
                    credit: net,
                    info,
                },
            ];
            if vat > Decimal::ZERO {
                let rate_percent = rate
                    .normalize()
                    .try_into()
                    .ok()
                    .and_then(output_vat_account)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "row {}: there is no output-VAT account for a rate of {rate}%",
                            row.rad_id
                        )
                    })?;
                lines.push(VoucherLine {
                    account: rate_percent.to_string(),
                    debit: Decimal::ZERO,
                    credit: vat,
                    info: Some("Utgående moms".to_string()),
                });
            }
            lines
        }
    };

    assert_balanced(&lines).with_context(|| format!("row {}", row.rad_id))?;

    Ok(BuildVoucherInput {
        series: series.to_string(),
        transaction_date: row.datum.clone(),
        description,
        lines,
    })
}

/// The Fortnox payload for an approved row.
pub fn forslag_to_payload(row: &ForslagRow, opts: VoucherOptions<'_>) -> Result<Value> {
    build_payload(&forslag_to_voucher_input(row, opts)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank_import::types::Confidence;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn row() -> ForslagRow {
        ForslagRow {
            rad_id: "abc123".to_string(),
            datum: "2025-08-10".to_string(),
            text: "Köp".to_string(),
            belopp: dec("-1250"),
            riktning: Direction::Utgift,
            bas_konto: "5410".to_string(),
            momssats: Some(dec("25")),
            moms_kr: None,
            konto_namn: String::new(),
            konfidens: Confidence::Hog,
            motivering: String::new(),
            matchad_kvitto: String::new(),
            godkann: "J".to_string(),
        }
    }

    fn build(row: &ForslagRow) -> BuildVoucherInput {
        forslag_to_voucher_input(row, VoucherOptions::default()).unwrap()
    }

    fn line<'a>(input: &'a BuildVoucherInput, account: &str) -> &'a VoucherLine {
        input
            .lines
            .iter()
            .find(|l| l.account == account)
            .unwrap_or_else(|| panic!("no line for account {account} in {:?}", input.lines))
    }

    // ---- the idempotency key -----------------------------------------------

    #[test]
    fn the_key_is_byte_compatible_with_the_typescript() {
        // Recorded by running `dist/bank-import/voucher.js` under node. These
        // exact strings are what the vouchers already in Fortnox carry, so a
        // mismatch here would re-post the whole ledger on the next run.
        for (datum, belopp, text, expected) in [
            ("2025-08-10", "-1250", "Köp av dator", "afddb8325417"),
            ("2025-08-10", "-1250", "Köp", "cb33ebe0a737"),
            ("2025-08-11", "-1250", "Köp", "87b33000064d"),
            ("2025-08-10", "-1251", "Köp", "483fc03024f6"),
            ("2025-08-10", "-1250", "Köpa", "d81ca80d0d6c"),
            ("2026-06-01", "-42", "SWISH KAFFE", "5d88deb56e17"),
            (
                "2026-06-02",
                "-1234.56",
                "SPOTIFY AB STOCKHOLM",
                "a478eb19d60b",
            ),
            ("2026-06-05", "6250", "KUNDINBETALNING 1001", "a606f8e7e087"),
            ("2025-08-10", "-1250.50", "Ören", "4b10ed3a035e"),
            ("2025-08-10", "0", "Noll", "818c08d8a25e"),
        ] {
            assert_eq!(
                rad_id(datum, dec(belopp), text),
                expected,
                "radId({datum:?}, {belopp}, {text:?})"
            );
        }
    }

    #[test]
    fn the_key_is_twelve_hex_characters() {
        let id = rad_id("2025-08-10", dec("-1250"), "Köp");
        assert_eq!(id.len(), 12);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn the_key_is_stable_for_the_same_inputs() {
        assert_eq!(
            rad_id("2025-08-10", dec("-1250"), "Köp av dator"),
            rad_id("2025-08-10", dec("-1250"), "Köp av dator")
        );
    }

    #[test]
    fn the_key_changes_when_any_component_changes() {
        let base = rad_id("2025-08-10", dec("-1250"), "Köp");
        assert_ne!(rad_id("2025-08-11", dec("-1250"), "Köp"), base);
        assert_ne!(rad_id("2025-08-10", dec("-1251"), "Köp"), base);
        assert_ne!(rad_id("2025-08-10", dec("-1250"), "Köpa"), base);
    }

    #[test]
    fn scale_does_not_change_the_key() {
        // A spreadsheet hands out -1250, -1250.0 and -1250.00 for the same
        // number. A Decimal keeps its scale, so without `normalize()` these
        // would be three different idempotency keys for one bank line — and
        // none of them would match what the TypeScript wrote.
        let base = rad_id("2025-08-10", dec("-1250"), "Köp");
        assert_eq!(rad_id("2025-08-10", dec("-1250.0"), "Köp"), base);
        assert_eq!(rad_id("2025-08-10", dec("-1250.00"), "Köp"), base);
        // And the trailing zero on a real öre amount is dropped too.
        assert_eq!(
            rad_id("2025-08-10", dec("-1250.50"), "Ören"),
            rad_id("2025-08-10", dec("-1250.5"), "Ören")
        );
    }

    #[test]
    fn the_key_of_a_bank_line_is_the_key_of_its_parts() {
        let seb = SebRow {
            bokforingsdatum: "2026-06-01".to_string(),
            text: "SWISH KAFFE".to_string(),
            belopp: dec("-42"),
            saldo: Some(dec("1000")),
        };
        assert_eq!(rad_id_for_seb(&seb), "5d88deb56e17");
    }

    // ---- markers -----------------------------------------------------------

    #[test]
    fn a_marker_round_trips() {
        let desc = format!("Köp {}", imp_marker("deadbeef01"));
        assert_eq!(extract_markers(Some(&desc)), vec!["deadbeef01"]);
    }

    #[test]
    fn no_marker_is_an_empty_list() {
        assert!(extract_markers(Some("plain text")).is_empty());
        assert!(extract_markers(None).is_empty());
        assert!(extract_markers(Some("")).is_empty());
    }

    #[test]
    fn several_markers_in_one_description_all_come_out() {
        let desc = format!("A {} och B {}", imp_marker("aaa111"), imp_marker("bbb222"));
        assert_eq!(extract_markers(Some(&desc)), vec!["aaa111", "bbb222"]);
    }

    #[test]
    fn markers_are_matched_case_insensitively_and_returned_lowercase() {
        // Upstream's `i` flag, and its `.toLowerCase()` on the capture.
        assert_eq!(extract_markers(Some("X [imp:DEADBEEF]")), vec!["deadbeef"]);
    }

    #[test]
    fn a_malformed_marker_is_not_a_marker() {
        // No closing bracket, no hex, and a non-hex body: none of these match
        // upstream's `\[imp:([0-9a-f]+)\]`.
        assert!(extract_markers(Some("[imp:abc")).is_empty());
        assert!(extract_markers(Some("[imp:]")).is_empty());
        assert!(extract_markers(Some("[imp:zzz]")).is_empty());
        // A broken one must not swallow a good one that follows it.
        assert_eq!(
            extract_markers(Some("[imp:zzz] [imp:abc123]")),
            vec!["abc123"]
        );
    }

    #[test]
    fn a_voucher_description_carries_its_own_marker_back_out() {
        let input = build(&row());
        assert!(input.description.contains(&imp_marker("abc123")));
        assert_eq!(extract_markers(Some(&input.description)), vec!["abc123"]);
    }

    // ---- expense -----------------------------------------------------------

    #[test]
    fn an_expense_books_net_plus_vat_against_the_bank() {
        let input = build(&row());
        assert!(assert_balanced(&input.lines).is_ok());

        assert_eq!(line(&input, "5410").debit, dec("1000")); // net
        assert_eq!(line(&input, "2640").debit, dec("250")); // input VAT
        assert_eq!(line(&input, "1930").credit, dec("1250")); // gross
        assert_eq!(input.series, "A");
        assert_eq!(input.transaction_date, "2025-08-10");
    }

    #[test]
    fn an_expense_at_a_zero_rate_has_no_vat_line() {
        let input = build(&ForslagRow {
            momssats: Some(Decimal::ZERO),
            belopp: dec("-1000"),
            ..row()
        });
        assert!(!input.lines.iter().any(|l| l.account == "2640"));
        assert!(assert_balanced(&input.lines).is_ok());
        assert_eq!(line(&input, "5410").debit, dec("1000"));
        assert_eq!(line(&input, "1930").credit, dec("1000"));
    }

    #[test]
    fn a_blank_momssats_is_treated_as_no_vat() {
        let input = build(&ForslagRow {
            momssats: None,
            belopp: dec("-1000"),
            ..row()
        });
        assert_eq!(input.lines.len(), 2);
        assert!(assert_balanced(&input.lines).is_ok());
    }

    // ---- income ------------------------------------------------------------

    #[test]
    fn income_books_gross_to_the_bank_against_revenue_and_output_vat() {
        let input = build(&ForslagRow {
            riktning: Direction::Inkomst,
            belopp: dec("1250"),
            bas_konto: "3041".to_string(),
            momssats: Some(dec("25")),
            ..row()
        });
        assert!(assert_balanced(&input.lines).is_ok());
        assert_eq!(line(&input, "1930").debit, dec("1250"));
        assert_eq!(line(&input, "3041").credit, dec("1000"));
        assert_eq!(line(&input, "2611").credit, dec("250"));
        assert!(input.description.contains(&imp_marker("abc123")));
    }

    #[test]
    fn income_uses_the_output_vat_account_for_its_rate() {
        for (rate, account, gross, net, vat) in [
            ("25", "2611", "1250", "1000", "250"),
            ("12", "2621", "1120", "1000", "120"),
            ("6", "2631", "1060", "1000", "60"),
        ] {
            let input = build(&ForslagRow {
                riktning: Direction::Inkomst,
                belopp: dec(gross),
                bas_konto: "3041".to_string(),
                momssats: Some(dec(rate)),
                ..row()
            });
            assert_eq!(line(&input, account).credit, dec(vat), "rate {rate}");
            assert_eq!(line(&input, "3041").credit, dec(net), "rate {rate}");
            assert_eq!(line(&input, "1930").debit, dec(gross), "rate {rate}");
        }
    }

    #[test]
    fn income_at_a_zero_rate_is_two_lines() {
        let input = build(&ForslagRow {
            riktning: Direction::Inkomst,
            belopp: dec("1000"),
            bas_konto: "3041".to_string(),
            momssats: Some(Decimal::ZERO),
            ..row()
        });
        assert_eq!(input.lines.len(), 2);
        assert_eq!(line(&input, "1930").debit, dec("1000"));
        assert_eq!(line(&input, "3041").credit, dec("1000"));
    }

    // ---- VAT resolution ----------------------------------------------------

    #[test]
    fn a_stale_zero_moms_kr_splits_the_gross_rather_than_suppressing_vat() {
        let input = build(&ForslagRow {
            riktning: Direction::Inkomst,
            belopp: dec("6250"),
            bas_konto: "3041".to_string(),
            momssats: Some(dec("25")),
            moms_kr: Some(Decimal::ZERO),
            ..row()
        });
        assert_eq!(line(&input, "2611").credit, dec("1250"));
        assert_eq!(line(&input, "3041").credit, dec("5000"));
        assert!(assert_balanced(&input.lines).is_ok());
    }

    #[test]
    fn an_explicit_zero_moms_kr_is_honoured_when_the_rate_is_zero() {
        let input = build(&ForslagRow {
            belopp: dec("-1000"),
            momssats: Some(Decimal::ZERO),
            moms_kr: Some(Decimal::ZERO),
            ..row()
        });
        assert!(!input.lines.iter().any(|l| l.account == "2640"));
    }

    #[test]
    fn an_exact_moms_kr_is_used_verbatim_and_the_net_is_the_remainder() {
        let input = build(&ForslagRow {
            belopp: dec("-1250"),
            momssats: Some(dec("25")),
            moms_kr: Some(dec("249.50")),
            ..row()
        });
        assert_eq!(line(&input, "2640").debit, dec("249.50"));
        assert_eq!(line(&input, "5410").debit, dec("1000.50"));
        assert_eq!(line(&input, "1930").credit, dec("1250"));
        assert!(assert_balanced(&input.lines).is_ok());
    }

    // ---- validation --------------------------------------------------------

    #[test]
    fn a_blank_bas_konto_is_refused() {
        let err = forslag_to_voucher_input(
            &ForslagRow {
                bas_konto: String::new(),
                ..row()
            },
            VoucherOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("BAS_konto"), "{err}");
        assert!(
            err.contains("abc123"),
            "the error should name the row: {err}"
        );
    }

    #[test]
    fn a_whitespace_only_bas_konto_is_refused_too() {
        assert!(forslag_to_voucher_input(
            &ForslagRow {
                bas_konto: "   ".to_string(),
                ..row()
            },
            VoucherOptions::default()
        )
        .is_err());
    }

    #[test]
    fn a_bas_konto_with_stray_whitespace_is_trimmed_rather_than_refused() {
        let input = build(&ForslagRow {
            bas_konto: " 5410 ".to_string(),
            ..row()
        });
        assert_eq!(line(&input, "5410").debit, dec("1000"));
    }

    #[test]
    fn an_income_rate_with_no_output_vat_account_is_refused() {
        // 20% is not a Swedish rate; `resolve_vat` refuses it before the
        // account lookup is even reached, so the message is about the rate.
        let err = forslag_to_voucher_input(
            &ForslagRow {
                riktning: Direction::Inkomst,
                belopp: dec("1200"),
                bas_konto: "3041".to_string(),
                momssats: Some(dec("20")),
                ..row()
            },
            VoucherOptions::default(),
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("20"), "{chain}");
    }

    #[test]
    fn a_zero_kronor_line_is_refused_rather_than_booked_as_two_empty_rows() {
        // DIVERGENCE: upstream builds a voucher of 0.00 / 0.00 lines and its
        // looser assertBalanced passes it, because 0 == 0. `assert_balanced`
        // here rejects a line with neither a debit nor a credit.
        let err = forslag_to_voucher_input(
            &ForslagRow {
                belopp: Decimal::ZERO,
                momssats: Some(Decimal::ZERO),
                ..row()
            },
            VoucherOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("neither"), "{err:#}");
    }

    #[test]
    fn an_explicit_vat_at_a_rate_with_no_output_account_is_refused() {
        // The path that actually reaches the account lookup: an explicit
        // Moms_kr short-circuits `resolve_vat`, so a 20% income line gets as
        // far as asking for an output-VAT account and there is none.
        let err = forslag_to_voucher_input(
            &ForslagRow {
                riktning: Direction::Inkomst,
                belopp: dec("1200"),
                bas_konto: "3041".to_string(),
                momssats: Some(dec("20")),
                moms_kr: Some(dec("200")),
                ..row()
            },
            VoucherOptions::default(),
        )
        .unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("output-VAT account"), "{chain}");
        assert!(
            chain.contains("abc123"),
            "the error should name the row: {chain}"
        );
    }

    // ---- options and payload -----------------------------------------------

    #[test]
    fn the_bank_account_and_series_are_overridable() {
        let input = forslag_to_voucher_input(
            &row(),
            VoucherOptions {
                bank_account: Some("1932"),
                series: Some("B"),
            },
        )
        .unwrap();
        assert_eq!(input.series, "B");
        assert_eq!(line(&input, "1932").credit, dec("1250"));
        assert!(!input.lines.iter().any(|l| l.account == "1930"));
    }

    #[test]
    fn the_payload_has_the_shape_fortnox_expects() {
        let payload = forslag_to_payload(&row(), VoucherOptions::default()).unwrap();
        let voucher = &payload["Voucher"];
        assert_eq!(voucher["VoucherSeries"], "A");
        assert_eq!(voucher["TransactionDate"], "2025-08-10");
        assert_eq!(voucher["Description"], "Köp [imp:abc123]");
        let rows = voucher["VoucherRows"]["VoucherRow"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["Account"], "5410");
        assert_eq!(rows[0]["Debit"], 1000.0);
        assert_eq!(rows[0]["TransactionInformation"], "Köp");
    }

    #[test]
    fn the_payload_refuses_a_row_whose_date_is_not_a_date() {
        // `build_payload` validates the transaction date; a `Förslag` sheet a
        // user has hand-edited can hold anything.
        assert!(forslag_to_payload(
            &ForslagRow {
                datum: "10/08/2025".to_string(),
                ..row()
            },
            VoucherOptions::default()
        )
        .is_err());
    }
}
