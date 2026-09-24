//! Choosing an account and a VAT figure for one bank line.
//!
//! Upstream this is `bank-import/code.ts`, the module carrying the most
//! business rules in the phase. It is pure: a bank line and at most one receipt
//! go in, an account and a VAT amount come out, and nothing is read or written.
//!
//! # The priority order
//!
//! Four rules, tried in order, first hit wins:
//!
//! | # | rule | confidence |
//! |---|------|------------|
//! | 1 | the receipt's own `BAS_konto`, the user's hand coding | `HÖG` |
//! | 2 | the receipt's `Kategori`, through [`CATEGORY_TO_BAS`] | `HÖG` |
//! | 3 | a keyword in the bank text, through [`KEYWORD_TO_BAS`] | `LÅG` |
//! | 4 | nothing matched | `OKÄND`, blank account |
//!
//! A bank line that matched *more than one* receipt short-circuits all four to
//! `FLERA` with a blank account: which receipt it was is the user's call, and
//! guessing would book the wrong VAT.
//!
//! Rules 1 and 2 are `HÖG` because the user chose them. Rule 3 is `LÅG` because
//! `GOOGLE` in a bank text is a software subscription most of the time and an
//! advertising invoice the rest, and no table can tell which. Every `LÅG` row
//! is reviewed before it is posted; that is what the `Förslag` sheet is for.
//!
//! # VAT
//!
//! An exact `Moms_kr` on the receipt beats a computed split — the receipt is
//! the primary document and its öre are the ones Skatteverket would see. A
//! literal `0` alongside a positive rate is treated as *unset* rather than as
//! zero VAT, so a blank cell that arrived as a zero never silently zeroes out
//! the VAT on a VAT-bearing line. See [`resolve_vat`].
//!
//! # Divergences from the TypeScript
//!
//! * **An unsupported VAT rate is an error.** Upstream's `splitGross` divides
//!   by `1 + rate/100` for any rate at all, so a receipt with `Momssats` 20
//!   quietly produced a Danish-rate split. [`moms::split_gross`] accepts only
//!   0, 6, 12 and 25 — Sweden's actual rates — so this returns an error instead
//!   of booking a number nobody can defend. A fractional rate (`12.5`) is
//!   rejected for the same reason.
//! * **[`money::round2`] is symmetric** about zero where the TypeScript's was
//!   not; see [`crate::domain::money`]. Reachable here only through a negative
//!   `Moms_kr`, which is a data error either way.
//! * **`LON` is listed last rather than next to `LÖN`.** Upstream's ordering
//!   makes its own `HALLON` entry unreachable; see the comment at that entry in
//!   [`KEYWORD_TO_BAS`].
//!
//! # Known limitation, deliberately left alone
//!
//! **`LON` is a three-character substring key.** Moving it fixed `HALLON`,
//! which was the one collision with another entry in the table, but it did not
//! change what a three-letter key is: any bank text containing the letters
//! `LON` and no earlier keyword still codes to 7010 (lön). `SALONG`,
//! `LONDON`, `BALLONG`, `MELONI` all do. That is inherited behaviour in a
//! table the owner maintains, and the fix — word-boundary matching, or a
//! longer key — would diverge from upstream for texts nobody here has seen.
//! It is reported rather than fixed: the owner should either rename the entry
//! or accept the miscodings, and either way a `LÅG`-confidence row is meant to
//! be reviewed in the `Förslag` sheet before it is approved.

use anyhow::{bail, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use super::types::{Confidence, Direction, KvittoRow, SebRow};
use crate::domain::moms;
use crate::domain::money::round2;

/// Ingående moms — the account a company's own VAT on purchases is debited to.
pub const INPUT_VAT_ACCOUNT: &str = "2640";

/// The company's ordinary business account, used when the caller names none.
pub const DEFAULT_BANK_ACCOUNT: &str = "1930";

/// Utgående moms by rate: the account VAT charged on sales is credited to.
///
/// Only the three rates Sweden levies. A sale at 0% has no output VAT line at
/// all, which is why there is no entry for it.
pub const OUTPUT_VAT_ACCOUNTS: [(u8, &str); 3] = [(25, "2611"), (12, "2621"), (6, "2631")];

/// The output-VAT account for a rate, if the rate has one.
pub fn output_vat_account(rate: u8) -> Option<&'static str> {
    OUTPUT_VAT_ACCOUNTS
        .iter()
        .find(|(r, _)| *r == rate)
        .map(|(_, a)| *a)
}

/// `Kategori` → BAS account, keys lowercased.
///
/// This maps the user's own bucket to an account. Spelling variants are
/// deliberate and carried over from upstream: a Swedish keyboard is not always
/// to hand, so `forsakring` maps as well as `försäkring` and `kontorsmaterial`
/// as well as `kontorsmateriel`. A slightly-off label still codes rather than
/// falling through to `OKÄND`, and since the user chose the category either
/// way the confidence stays `HÖG`.
pub const CATEGORY_TO_BAS: [(&str, &str); 44] = [
    // office and consumables
    ("kontorsmateriel", "6110"),
    ("kontorsmaterial", "6110"),
    ("förbrukningsinventarier", "5410"),
    ("forbrukningsinventarier", "5410"),
    ("verktyg", "5410"),
    ("inventarier", "1220"),
    // services bought in
    ("konsultarvode", "6550"),
    ("konsult", "6550"),
    ("städning", "6550"),
    ("stadning", "6550"),
    // sales and income
    ("försäljning", "3041"),
    ("forsaljning", "3041"),
    // software and comms
    ("programvara", "5420"),
    ("licens", "5420"),
    ("licenser", "5420"),
    ("saas", "5420"),
    ("telefoni", "6212"),
    ("telefon", "6212"),
    ("mobil", "6212"),
    ("internet", "6230"),
    ("bredband", "6230"),
    // travel, freight, representation
    ("resor", "5800"),
    ("resa", "5800"),
    ("biljett", "5800"),
    ("drivmedel", "5611"),
    ("bensin", "5611"),
    ("diesel", "5611"),
    ("representation", "6071"),
    ("frakt", "5710"),
    ("porto", "6250"),
    // premises and overheads
    ("hyra", "5010"),
    ("lokalhyra", "5010"),
    ("lokal", "5010"),
    ("el", "5020"),
    ("försäkring", "6310"),
    ("forsakring", "6310"),
    // marketing and training
    ("marknadsföring", "5910"),
    ("marknadsforing", "5910"),
    ("annonsering", "5910"),
    ("reklam", "5910"),
    ("utbildning", "7610"),
    ("kurs", "7610"),
    // banking
    ("bankavgift", "6570"),
    ("bankkostnad", "6570"),
];

/// Look a category label up. The caller trims and lowercases first.
pub fn category_to_bas(key: &str) -> Option<&'static str> {
    CATEGORY_TO_BAS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, a)| *a)
}

/// Bank-text keyword → BAS account, matched as an uppercased substring.
///
/// **The order is load-bearing and is not alphabetical.** The first hit wins,
/// so a specific multi-word key must precede a generic one that it contains:
/// `SEB AVGIFT` is a bank charge on the company's own account and must be found
/// before the bare `AVGIFT`, which catches everything else fee-shaped. The last
/// two entries exist in that order for exactly this reason, and reordering this
/// table changes what gets booked.
///
/// Everything matched here is `LÅG` confidence and is reviewed before posting.
pub const KEYWORD_TO_BAS: [(&str, &str); 41] = [
    // financial, tax, salary
    ("RÄNTA", "8410"),
    ("RANTA", "8410"),
    ("SKATTEVERKET", "2650"),
    ("LÖN", "7010"),
    // software and SaaS
    ("SPOTIFY", "5420"),
    ("ADOBE", "5420"),
    ("MICROSOFT", "5420"),
    ("GITHUB", "5420"),
    ("GOOGLE", "5420"),
    ("DROPBOX", "5420"),
    ("NOTION", "5420"),
    ("SLACK", "5420"),
    ("FIGMA", "5420"),
    ("OPENAI", "5420"),
    ("AMAZON WEB", "5420"),
    ("AWS", "5420"),
    // telecom
    ("TELIA", "6212"),
    ("TELENOR", "6212"),
    ("COMVIQ", "6212"),
    ("HALLON", "6212"),
    ("TELE2", "6212"),
    // fuel
    ("CIRCLE K", "5611"),
    ("OKQ8", "5611"),
    ("PREEM", "5611"),
    ("INGO", "5611"),
    ("SHELL", "5611"),
    // travel
    ("TAXI", "5800"),
    ("UBER", "5800"),
    ("FLYG", "5800"),
    // freight and post
    ("POSTNORD", "5710"),
    ("SCHENKER", "5710"),
    ("BRING", "5710"),
    ("DHL", "5710"),
    // insurance
    ("FÖRSÄKRING", "6310"),
    ("FORSAKRING", "6310"),
    ("LÄNSFÖRSÄKR", "6310"),
    ("TRYGG-HANSA", "6310"),
    ("FOLKSAM", "6310"),
    // DIVERGENCE: upstream lists the unaccented `LON` immediately after `LÖN`,
    // in the salary block at the top. It is three letters long and it is a
    // substring of `HALLON`, the Swedish mobile operator listed under telecom
    // below it — so upstream books every Hallon bill to 7010, lön, and its
    // `HALLON` entry can never fire. Measured against the compiled upstream:
    // `codeLine` on "HALLON MOBIL 0701234567" answers 7010 with the motivering
    // `Nyckelord "LON" i SEB-texten`. A full sweep of upstream's table found
    // this to be the only such pair. `LON` therefore lives down here with the
    // other generic keys, and `keyword_table_is_ordered_specific_before_generic`
    // fails if any future entry recreates the problem.
    //
    // KNOWN LIMITATION, NOT FIXED: this is still a three-character substring
    // key, so `SALONG`, `LONDON` and anything else containing the letters
    // still code to 7010. Only the collision with another *table entry* was
    // fixed; the general over-matching is upstream's and is left to the table's
    // owner. See the module docs.
    ("LON", "7010"),
    // bank fees: the generic key must stay last
    ("SEB AVGIFT", "6570"),
    ("AVGIFT", "6570"),
];

/// What [`code_line`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coding {
    /// The BAS account to book against. **Blank** when nothing matched.
    pub bas_konto: String,
    /// VAT rate in percent: 25, 12, 6 or 0.
    pub momssats: Decimal,
    /// VAT in kronor.
    pub moms_kr: Decimal,
    /// The bank account the money moved through.
    pub bank_account: String,
    /// How much to trust `bas_konto`.
    pub confidence: Confidence,
    /// Why, in Swedish, for the human reviewing the `Förslag` sheet.
    pub motivering: String,
}

/// Everything [`code_line`] needs about one bank line.
#[derive(Debug, Clone, Copy)]
pub struct CodeInput<'a> {
    /// The bank line.
    pub seb: &'a SebRow,
    /// The single receipt it matched, if exactly one did.
    pub kvitto: Option<&'a KvittoRow>,
    /// True when it matched more than one receipt.
    pub multiple: bool,
    /// The bank account, defaulting to [`DEFAULT_BANK_ACCOUNT`].
    pub bank_account: Option<&'a str>,
}

impl<'a> CodeInput<'a> {
    /// A bank line with no receipt and the default bank account.
    pub fn new(seb: &'a SebRow) -> Self {
        Self {
            seb,
            kvitto: None,
            multiple: false,
            bank_account: None,
        }
    }
}

/// VAT in kronor for a gross amount, a rate and an optional exact figure.
///
/// An explicit `moms_kr` wins — but only when it is positive, *or* when the
/// rate is zero. That asymmetry is the point: a literal `0` alongside a
/// positive rate is far more likely to be a blank cell that arrived as a zero
/// than a genuine claim that a 25%-rated purchase carried no VAT, and treating
/// it as genuine would silently drop the input VAT the company is owed. With
/// the rate at zero, `0` is the only honest answer anyway, so honouring it
/// costs nothing.
///
/// # Errors
///
/// An unsupported rate. See the module docs.
pub fn resolve_vat(gross: Decimal, rate: Decimal, moms_kr: Option<Decimal>) -> Result<Decimal> {
    if let Some(explicit) = moms_kr {
        if explicit > Decimal::ZERO || rate.is_zero() {
            return Ok(round2(explicit));
        }
    }
    if rate > Decimal::ZERO {
        return Ok(moms::split_gross(gross, rate_as_percent(rate)?)?.vat);
    }
    Ok(Decimal::ZERO)
}

/// A `Momssats` cell as a whole-percent rate.
///
/// # Errors
///
/// A negative rate, a fractional one, or one too large to be a percentage.
/// [`moms::split_gross`] then rejects anything that is not 0, 6, 12 or 25.
fn rate_as_percent(rate: Decimal) -> Result<u8> {
    let normalised = rate.normalize();
    if normalised.scale() != 0 {
        bail!("VAT rate {rate} is not a whole percent");
    }
    normalised
        .to_u8()
        .ok_or_else(|| anyhow::anyhow!("VAT rate {rate} is not a percentage between 0 and 255"))
}

/// Choose an account and a VAT figure for one bank line. Pure.
///
/// # Errors
///
/// Only from VAT resolution: an unsupported or fractional `Momssats` on the
/// matched receipt. Account selection itself cannot fail — failing to find an
/// account is [`Confidence::Okand`], not an error, because the user filling the
/// blank in is the designed path and not an exceptional one.
pub fn code_line(input: CodeInput<'_>) -> Result<Coding> {
    let bank_account = input
        .bank_account
        .unwrap_or(DEFAULT_BANK_ACCOUNT)
        .to_string();
    let gross = round2(input.seb.belopp.abs());

    if input.multiple {
        return Ok(Coding {
            bas_konto: String::new(),
            momssats: Decimal::ZERO,
            moms_kr: Decimal::ZERO,
            bank_account,
            confidence: Confidence::Flera,
            motivering: "Flera kvitton matchar belopp+datum — välj rätt manuellt.".to_string(),
        });
    }

    let kvitto = input.kvitto;
    let rate = kvitto.and_then(|k| k.momssats).unwrap_or(Decimal::ZERO);

    // 1. The user's own coding on the receipt, taken verbatim.
    if let Some(account) = kvitto
        .and_then(|k| k.bas_konto.as_deref())
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let who = kvitto
            .and_then(|k| k.leverantor.as_deref().or(k.beskrivning.as_deref()))
            .unwrap_or("kvitto");
        return Ok(Coding {
            bas_konto: account.to_string(),
            momssats: rate,
            moms_kr: resolve_vat(gross, rate, kvitto.and_then(|k| k.moms_kr))?,
            bank_account,
            confidence: Confidence::Hog,
            motivering: format!("BAS_konto från kvitto ({who})."),
        });
    }

    // 2. The receipt's category, through the built-in table.
    if let Some(kategori) = kvitto.and_then(|k| k.kategori.as_deref()) {
        if let Some(account) = category_to_bas(&kategori.trim().to_lowercase()) {
            return Ok(Coding {
                bas_konto: account.to_string(),
                momssats: rate,
                moms_kr: resolve_vat(gross, rate, kvitto.and_then(|k| k.moms_kr))?,
                bank_account,
                confidence: Confidence::Hog,
                motivering: format!("Kategori \"{kategori}\" → {account}."),
            });
        }
    }

    // 3. A keyword in the bank's own text.
    let upper = input.seb.text.to_uppercase();
    for (keyword, account) in KEYWORD_TO_BAS {
        if upper.contains(keyword) {
            // A keyword hit almost always means there is no receipt, so there
            // is usually no rate either. `kvitto` can still be present — it is
            // a receipt that had neither an account nor a known category — and
            // its rate is honoured when it is.
            return Ok(Coding {
                bas_konto: account.to_string(),
                momssats: rate,
                moms_kr: resolve_vat(gross, rate, kvitto.and_then(|k| k.moms_kr))?,
                bank_account,
                confidence: Confidence::Lag,
                motivering: format!("Nyckelord \"{keyword}\" i SEB-texten → {account}."),
            });
        }
    }

    // 4. Nothing matched. A blank account, and a reason saying so.
    let direction = Direction::of_amount(input.seb.belopp);
    Ok(Coding {
        bas_konto: String::new(),
        momssats: Decimal::ZERO,
        moms_kr: Decimal::ZERO,
        bank_account,
        confidence: Confidence::Okand,
        motivering: format!("Ingen matchning ({direction}). Fyll i BAS_konto innan bokföring."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn seb(belopp: &str, text: &str) -> SebRow {
        SebRow {
            bokforingsdatum: "2025-08-10".to_string(),
            text: text.to_string(),
            belopp: dec(belopp),
            saldo: None,
        }
    }

    fn kvitto(belopp: &str) -> KvittoRow {
        KvittoRow {
            datum: "2025-08-10".to_string(),
            belopp_inkl_moms: dec(belopp),
            ..KvittoRow::default()
        }
    }

    fn code(seb: &SebRow, kv: Option<&KvittoRow>) -> Coding {
        code_line(CodeInput {
            seb,
            kvitto: kv,
            multiple: false,
            bank_account: None,
        })
        .unwrap()
    }

    // ---- the four priorities ----------------------------------------------

    #[test]
    fn priority_1_uses_the_receipts_own_account_verbatim() {
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("25")),
            // A category that maps to a *different* account, so the test shows
            // priority 1 winning rather than the two agreeing by luck.
            kategori: Some("kontorsmateriel".to_string()),
            ..kvitto("1250")
        };
        let c = code(&seb("-1250", "X"), Some(&kv));
        assert_eq!(c.bas_konto, "5410");
        assert_eq!(c.confidence, Confidence::Hog);
        assert_ne!(category_to_bas("kontorsmateriel"), Some("5410"));
    }

    #[test]
    fn priority_2_maps_the_category_through_the_table() {
        let kv = KvittoRow {
            momssats: Some(dec("25")),
            kategori: Some("kontorsmateriel".to_string()),
            ..kvitto("1250")
        };
        let c = code(&seb("-1250", "X"), Some(&kv));
        assert_eq!(c.bas_konto, "6110");
        assert_eq!(c.confidence, Confidence::Hog);
    }

    #[test]
    fn priority_3_pattern_matches_the_bank_text() {
        let c = code(&seb("-200", "SEB AVGIFT månadskostnad"), None);
        assert_eq!(c.bas_konto, "6570");
        assert_eq!(c.confidence, Confidence::Lag);

        let c = code(&seb("-50", "RÄNTA lån"), None);
        assert_eq!(c.bas_konto, "8410");
        assert_eq!(c.confidence, Confidence::Lag);
    }

    #[test]
    fn priority_4_falls_back_to_a_blank_account() {
        let c = code(&seb("-99", "någonting okänt"), None);
        assert_eq!(c.bas_konto, "");
        assert_eq!(c.confidence, Confidence::Okand);
        assert!(c.motivering.contains("Utgift"), "{}", c.motivering);
        assert!(c.motivering.contains("BAS_konto"), "{}", c.motivering);
    }

    #[test]
    fn several_receipts_short_circuit_to_flera_with_no_account() {
        let c = code_line(CodeInput {
            multiple: true,
            ..CodeInput::new(&seb("-100", "X"))
        })
        .unwrap();
        assert_eq!(c.confidence, Confidence::Flera);
        assert_eq!(c.bas_konto, "");
        assert_eq!(c.moms_kr, Decimal::ZERO);
    }

    #[test]
    fn flera_wins_even_when_a_receipt_would_have_coded_it() {
        // `multiple` is checked before anything else upstream, and the receipt
        // passed alongside it must not leak into the answer.
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("25")),
            ..kvitto("100")
        };
        let c = code_line(CodeInput {
            kvitto: Some(&kv),
            multiple: true,
            ..CodeInput::new(&seb("-100", "SPOTIFY"))
        })
        .unwrap();
        assert_eq!(c.confidence, Confidence::Flera);
        assert_eq!(c.bas_konto, "");
        assert_eq!(c.momssats, Decimal::ZERO);
    }

    // ---- the category table ------------------------------------------------

    #[test]
    fn the_category_table_covers_the_common_buckets() {
        for (kategori, account) in [
            ("hyra", "5010"),
            ("lokal", "5010"),
            ("försäkring", "6310"),
            ("forsakring", "6310"),
            ("drivmedel", "5611"),
            ("programvara", "5420"),
            ("licens", "5420"),
            ("telefoni", "6212"),
            ("marknadsföring", "5910"),
            ("utbildning", "7610"),
            // A spelling variant of `kontorsmateriel`.
            ("kontorsmaterial", "6110"),
            ("frakt", "5710"),
        ] {
            let kv = KvittoRow {
                momssats: Some(dec("25")),
                kategori: Some(kategori.to_string()),
                ..kvitto("1000")
            };
            let c = code(&seb("-1000", "X"), Some(&kv));
            assert_eq!(c.bas_konto, account, "category {kategori:?}");
            assert_eq!(c.confidence, Confidence::Hog, "category {kategori:?}");
        }
    }

    #[test]
    fn the_category_label_is_case_insensitive_and_trimmed() {
        for label in ["Programvara", "PROGRAMVARA", "  programvara  "] {
            let kv = KvittoRow {
                kategori: Some(label.to_string()),
                ..kvitto("1000")
            };
            assert_eq!(
                code(&seb("-1000", "X"), Some(&kv)).bas_konto,
                "5420",
                "{label:?}"
            );
        }
    }

    #[test]
    fn a_swedish_uppercase_category_lowercases_correctly() {
        // `Ö` → `ö` is the case the lookup would miss under ASCII-only folding,
        // and `försäkring` is a real key.
        let kv = KvittoRow {
            kategori: Some("FÖRSÄKRING".to_string()),
            ..kvitto("1000")
        };
        assert_eq!(code(&seb("-1000", "X"), Some(&kv)).bas_konto, "6310");
    }

    #[test]
    fn an_unknown_category_falls_through_to_the_keyword_table() {
        let kv = KvittoRow {
            kategori: Some("rymdfarkoster".to_string()),
            ..kvitto("200")
        };
        let c = code(&seb("-200", "SPOTIFY AB"), Some(&kv));
        assert_eq!(c.bas_konto, "5420");
        assert_eq!(c.confidence, Confidence::Lag);
    }

    // ---- the keyword table -------------------------------------------------

    #[test]
    fn the_keyword_table_covers_the_common_vendors() {
        for (text, account) in [
            ("SPOTIFY AB STOCKHOLM", "5420"),
            ("CIRCLE K 4711", "5611"),
            ("TELIA SVERIGE AB", "6212"),
            ("POSTNORD FRAKT", "5710"),
            ("LÄNSFÖRSÄKRINGAR", "6310"),
            ("UBER *TRIP", "5800"),
        ] {
            let c = code(&seb("-200", text), None);
            assert_eq!(c.bas_konto, account, "text {text:?}");
            assert_eq!(c.confidence, Confidence::Lag, "text {text:?}");
        }
    }

    #[test]
    fn the_keyword_match_is_case_insensitive() {
        assert_eq!(code(&seb("-200", "spotify ab"), None).bas_konto, "5420");
        // And on a Swedish letter: `ä` must uppercase to `Ä` for `RÄNTA`.
        assert_eq!(code(&seb("-50", "ränta på lån"), None).bas_konto, "8410");
    }

    #[test]
    fn specific_keywords_stay_ahead_of_the_generic_avgift() {
        assert_eq!(code(&seb("-50", "SEB AVGIFT"), None).bas_konto, "6570");
        assert_eq!(code(&seb("-50", "KORTAVGIFT"), None).bas_konto, "6570");
        // Both land on 6570, so the order is not observable through the account
        // alone. The motivering names the keyword that fired, and it must be
        // the specific one for the specific text.
        assert!(
            code(&seb("-50", "SEB AVGIFT"), None)
                .motivering
                .contains("SEB AVGIFT"),
            "the specific keyword must win"
        );
        assert!(
            code(&seb("-50", "KORTAVGIFT"), None)
                .motivering
                .contains("\"AVGIFT\""),
            "the generic keyword catches the rest"
        );
    }

    /// Differential against the **compiled upstream**.
    ///
    /// `tests/.upstream-code-differential.json` was produced by running
    /// `dist/bank-import/code.js` under node over every keyword and every
    /// category, each in three spellings (bare, embedded in surrounding text /
    /// uppercased, and padded), 255 cases. Re-running it is a one-liner in the
    /// task report. Anything that disagrees here is either a bug in this
    /// translation or a divergence that has to be named.
    #[test]
    fn matches_the_compiled_upstream_except_where_a_divergence_is_named() {
        #[derive(serde::Deserialize)]
        struct Case {
            kind: String,
            text: String,
            #[serde(rename = "basKonto")]
            bas_konto: String,
            konfidens: String,
        }

        let raw = include_str!("../../tests/.upstream-code-differential.json");
        let cases: Vec<Case> = serde_json::from_str(raw).unwrap();
        assert_eq!(cases.len(), 255, "the recorded differential changed size");

        let mut diffs = Vec::new();
        for case in &cases {
            let got = match case.kind.as_str() {
                "kw" => code(&seb("-1250", &case.text), None),
                "cat" => {
                    let kv = KvittoRow {
                        momssats: Some(dec("25")),
                        kategori: Some(case.text.clone()),
                        ..kvitto("1250")
                    };
                    code(&seb("-1250", "X"), Some(&kv))
                }
                other => panic!("unknown case kind {other:?}"),
            };
            if got.bas_konto != case.bas_konto || got.confidence.as_str() != case.konfidens {
                diffs.push(format!(
                    "{} {:?}: upstream {}/{} vs rust {}/{}",
                    case.kind,
                    case.text,
                    case.bas_konto,
                    case.konfidens,
                    got.bas_konto,
                    got.confidence
                ));
            }
        }

        // The ONLY expected disagreements are the three HALLON spellings, where
        // upstream's `LON` shadows its own `HALLON` entry. See
        // `a_hallon_mobile_bill_codes_to_telecom_not_to_salary`.
        assert_eq!(
            diffs,
            vec![
                "kw \"HALLON\": upstream 7010/LÅG vs rust 6212/LÅG".to_string(),
                "kw \"FOO HALLON BAR\": upstream 7010/LÅG vs rust 6212/LÅG".to_string(),
                "kw \"hallon\": upstream 7010/LÅG vs rust 6212/LÅG".to_string(),
            ],
            "unexpected divergence from the compiled upstream"
        );
    }

    #[test]
    fn a_hallon_mobile_bill_codes_to_telecom_not_to_salary() {
        // DIVERGENCE, and a real upstream bug. `LON` is a substring of
        // `HALLON`, and upstream lists `LON` first, so `codeLine` on
        // "HALLON MOBIL 0701234567" answers 7010 (lön) with the motivering
        // `Nyckelord "LON" i SEB-texten` — measured against the compiled
        // dist/bank-import/code.js under node. Upstream's own HALLON entry is
        // dead code.
        let c = code(&seb("-249", "HALLON MOBIL 0701234567"), None);
        assert_eq!(c.bas_konto, "6212");
        assert!(c.motivering.contains("HALLON"), "{}", c.motivering);

        // Moving `LON` did not cost it its own matches.
        let c = code(&seb("-30000", "LONEUTBETALNING JUNI"), None);
        assert_eq!(c.bas_konto, "7010");
        assert_eq!(
            code(&seb("-30000", "LÖNEUTBETALNING"), None).bas_konto,
            "7010"
        );
    }

    #[test]
    fn lon_still_over_matches_any_text_containing_those_three_letters() {
        // NOT A BUG BEING FIXED — a known limitation, pinned so it is visible
        // rather than surprising. Moving `LON` fixed the collision with the
        // table's own `HALLON` entry; it did not stop a three-letter
        // substring key from matching ordinary Swedish and English words.
        // Changing that would diverge from upstream for texts nobody here has
        // seen, so it is reported to the table's owner instead. See the
        // module docs.
        for text in ["SALONG SAX", "LONDON HEATHROW", "BALLONGFÄRD"] {
            let c = code(&seb("-500", text), None);
            assert_eq!(c.bas_konto, "7010", "{text}");
            assert_eq!(c.confidence, Confidence::Lag, "{text}");
        }
    }

    #[test]
    fn the_keyword_table_is_ordered_specific_before_generic() {
        // A structural guard on the table itself, so a future alphabetical
        // tidy-up of KEYWORD_TO_BAS fails here rather than silently changing
        // what gets booked.
        for (i, (outer, _)) in KEYWORD_TO_BAS.iter().enumerate() {
            for (inner, _) in KEYWORD_TO_BAS.iter().skip(i + 1) {
                assert!(
                    !inner.contains(outer),
                    "{inner:?} contains {outer:?} but is listed AFTER it, so \
                     {outer:?} fires first and {inner:?} is unreachable"
                );
            }
        }
    }

    // ---- VAT ----------------------------------------------------------------

    #[test]
    fn an_exact_moms_kr_on_the_receipt_wins_over_a_split() {
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("25")),
            moms_kr: Some(dec("249.5")),
            ..kvitto("1250")
        };
        assert_eq!(code(&seb("-1250", "X"), Some(&kv)).moms_kr, dec("249.5"));
    }

    #[test]
    fn the_gross_is_split_when_the_receipt_states_no_moms_kr() {
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("25")),
            ..kvitto("1250")
        };
        // 1250 - 1250/1.25
        assert_eq!(code(&seb("-1250", "X"), Some(&kv)).moms_kr, dec("250"));
    }

    #[test]
    fn a_zero_rate_gives_zero_vat() {
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("0")),
            ..kvitto("1000")
        };
        assert_eq!(code(&seb("-1000", "X"), Some(&kv)).moms_kr, Decimal::ZERO);
    }

    #[test]
    fn a_stale_zero_moms_kr_does_not_suppress_vat_on_a_rated_line() {
        // The rule the module docs argue for: `0` next to 25% is a blank cell,
        // not a claim of no VAT. Splitting is the safe reading.
        assert_eq!(
            resolve_vat(dec("1250"), dec("25"), Some(Decimal::ZERO)).unwrap(),
            dec("250")
        );
        // But at a zero rate, an explicit `0` is honoured.
        assert_eq!(
            resolve_vat(dec("1000"), Decimal::ZERO, Some(Decimal::ZERO)).unwrap(),
            Decimal::ZERO
        );
    }

    #[test]
    fn the_vat_is_computed_on_the_absolute_gross() {
        // The bank states an expense as a negative; VAT is not negative.
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("25")),
            ..kvitto("1250")
        };
        let out = code(&seb("-1250", "X"), Some(&kv));
        let in_ = code(&seb("1250", "X"), Some(&kv));
        assert_eq!(out.moms_kr, dec("250"));
        assert_eq!(in_.moms_kr, dec("250"));
    }

    #[test]
    fn an_unsupported_vat_rate_is_an_error_rather_than_a_silent_split() {
        // DIVERGENCE: upstream's splitGross divides by 1 + rate/100 for any
        // rate, so 20% produced a plausible-looking Danish-rate split. This
        // refuses it.
        let kv = KvittoRow {
            bas_konto: Some("5410".to_string()),
            momssats: Some(dec("20")),
            ..kvitto("1250")
        };
        let err = code_line(CodeInput {
            kvitto: Some(&kv),
            ..CodeInput::new(&seb("-1250", "X"))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("20"), "{err}");
    }

    #[test]
    fn a_fractional_vat_rate_is_an_error() {
        let err = resolve_vat(dec("1250"), dec("12.5"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("whole percent"), "{err}");
    }

    #[test]
    fn a_rate_written_with_trailing_zeros_is_still_a_whole_percent() {
        // A spreadsheet cell holding 25 often arrives as 25.00.
        assert_eq!(
            resolve_vat(dec("1250"), dec("25.00"), None).unwrap(),
            dec("250")
        );
    }

    // ---- accounts and defaults ---------------------------------------------

    #[test]
    fn the_bank_account_defaults_to_1930_and_is_overridable() {
        assert_eq!(code(&seb("-100", "RÄNTA"), None).bank_account, "1930");
        let c = code_line(CodeInput {
            bank_account: Some("1932"),
            ..CodeInput::new(&seb("-100", "RÄNTA"))
        })
        .unwrap();
        assert_eq!(c.bank_account, "1932");
    }

    #[test]
    fn the_output_vat_accounts_cover_the_three_swedish_rates_and_nothing_else() {
        assert_eq!(output_vat_account(25), Some("2611"));
        assert_eq!(output_vat_account(12), Some("2621"));
        assert_eq!(output_vat_account(6), Some("2631"));
        // 0% has no output-VAT line at all, which is why there is no entry.
        assert_eq!(output_vat_account(0), None);
        assert_eq!(output_vat_account(20), None);
    }

    #[test]
    fn every_table_entry_names_a_four_digit_bas_account() {
        for (key, account) in CATEGORY_TO_BAS.iter().chain(KEYWORD_TO_BAS.iter()) {
            assert!(
                account.len() == 4 && account.bytes().all(|b| b.is_ascii_digit()),
                "{key:?} maps to {account:?}, which is not a BAS account number"
            );
            assert!(
                crate::domain::bas::class_of(account).is_ok(),
                "{key:?} maps to {account:?}, which has no BAS class"
            );
        }
    }

    #[test]
    fn the_category_keys_are_already_lowercase() {
        // The lookup lowercases its argument but not the table, so a key with
        // a capital in it would be permanently unreachable.
        for (key, _) in CATEGORY_TO_BAS {
            assert_eq!(key, key.to_lowercase(), "category key {key:?}");
        }
    }

    #[test]
    fn the_keyword_keys_are_already_uppercase() {
        for (key, _) in KEYWORD_TO_BAS {
            assert_eq!(key, key.to_uppercase(), "keyword {key:?}");
        }
    }
}
