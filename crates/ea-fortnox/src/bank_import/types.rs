//! Shared row shapes for the bank-import pipeline.
//!
//! Upstream this is `bank-import/types.ts`, which keeps itself dependency-free
//! so the pure modules (`match`, `code`, `voucher`) and the spreadsheet-backed
//! `workbook` module agree on what a row is. The same split holds here:
//! nothing in this file knows about `calamine`, `rust_xlsxwriter` or Fortnox.
//!
//! # Where the types differ from the TypeScript
//!
//! The TypeScript leans on JavaScript's two empties. A missing receipt field is
//! `undefined`; a blank spreadsheet cell in the `Förslag` sheet is the empty
//! string, so `momssats` and `momsKr` are typed `number | ''`. Both become
//! [`Option`] here, and the `''` case becomes [`None`] — the merge in
//! [`workbook::merge_forslag`](super::workbook::merge_forslag) turns on exactly
//! that distinction, so it has to survive the translation.
//!
//! Amounts are [`Decimal`], not `f64`, for the reason [`crate::domain::money`]
//! gives. Free text stays [`String`]: it is copied through to a voucher
//! description and never arithmetic.

use std::fmt;

use rust_decimal::Decimal;

/// Which way the money moved, from the sign of the bank amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Money left the account: `Belopp < 0`.
    Utgift,
    /// Money arrived: `Belopp >= 0`.
    Inkomst,
}

impl Direction {
    /// The direction a signed bank amount implies.
    ///
    /// Upstream is `seb.belopp < 0 ? 'Utgift' : 'Inkomst'`, so a zero-kronor
    /// line is `Inkomst`. Faithfully reproduced, oddity included: a 0 kr bank
    /// line is not a real transaction, and inventing a third case here would
    /// only move the question somewhere with less context.
    pub fn of_amount(belopp: Decimal) -> Self {
        if belopp < Decimal::ZERO {
            Direction::Utgift
        } else {
            Direction::Inkomst
        }
    }

    /// The Swedish label, as it is written into the `Riktning` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Utgift => "Utgift",
            Direction::Inkomst => "Inkomst",
        }
    }

    /// Read a `Riktning` cell back.
    ///
    /// Upstream: `t('Riktning') === 'Inkomst' ? 'Inkomst' : 'Utgift'` — anything
    /// that is not exactly `Inkomst`, blank included, is `Utgift`.
    pub fn from_sheet(cell: &str) -> Self {
        if cell == "Inkomst" {
            Direction::Inkomst
        } else {
            Direction::Utgift
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much the coder trusts the account it chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The user's own coding, taken verbatim or via the category table.
    Hog,
    /// A keyword guess from the bank text. Always to be reviewed.
    Lag,
    /// No rule fired; the account is blank and the user must fill it.
    Okand,
    /// More than one receipt matched; the choice is the user's.
    Flera,
}

impl Confidence {
    /// The Swedish label, as it is written into the `Konfidens` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Hog => "HÖG",
            Confidence::Lag => "LÅG",
            Confidence::Okand => "OKÄND",
            Confidence::Flera => "FLERA",
        }
    }

    /// Read a `Konfidens` cell back.
    ///
    /// Upstream casts the cell text straight to the union type and falls back
    /// to `OKÄND` only when it is empty — so a typo like `HOG` would survive as
    /// a bogus `Confidence` in TypeScript. Here anything unrecognised becomes
    /// [`Confidence::Okand`], which is what the empty case already meant: the
    /// column is advisory, and an unreadable value is exactly "unknown".
    pub fn from_sheet(cell: &str) -> Self {
        match cell {
            "HÖG" => Confidence::Hog,
            "LÅG" => Confidence::Lag,
            "FLERA" => Confidence::Flera,
            _ => Confidence::Okand,
        }
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the `SEB` sheet: the raw bank export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SebRow {
    /// Booking date, `YYYY-MM-DD`. Validated on the way in by
    /// [`workbook::read_seb`](super::workbook::read_seb).
    pub bokforingsdatum: String,
    /// The bank's own description of the line.
    pub text: String,
    /// Signed: negative is money out, positive money in.
    pub belopp: Decimal,
    /// Running balance, when the export carries one.
    pub saldo: Option<Decimal>,
}

/// One row of the `Kvitton` sheet: a receipt the user keeps by hand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KvittoRow {
    /// Receipt date, `YYYY-MM-DD`.
    pub datum: String,
    /// Gross amount, VAT included. Always positive.
    pub belopp_inkl_moms: Decimal,
    /// Supplier name.
    pub leverantor: Option<String>,
    /// Free-text description, used when there is no supplier.
    pub beskrivning: Option<String>,
    /// VAT rate in percent: 25, 12, 6 or 0.
    pub momssats: Option<Decimal>,
    /// VAT in kronor, when the receipt states it exactly.
    pub moms_kr: Option<Decimal>,
    /// The user's own bucket, mapped through
    /// [`code::CATEGORY_TO_BAS`](super::code::CATEGORY_TO_BAS).
    pub kategori: Option<String>,
    /// A BAS account the user coded by hand. Wins over everything else.
    pub bas_konto: Option<String>,
    /// The user's reference to the paper or PDF receipt.
    pub kvitto_ref: Option<String>,
    /// Free-text direction hint. Read but not acted on, as upstream.
    pub riktning: Option<String>,
}

/// One row of the `Förslag` review sheet: what the pipeline proposes, plus the
/// columns the user edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForslagRow {
    /// Stable idempotency key; see
    /// [`voucher::rad_id`](super::voucher::rad_id).
    pub rad_id: String,
    /// Transaction date, `YYYY-MM-DD`.
    pub datum: String,
    /// The bank line's text, copied through to the voucher description.
    pub text: String,
    /// Signed amount, as the bank stated it.
    pub belopp: Decimal,
    /// Derived from the sign of `belopp`.
    pub riktning: Direction,
    /// The BAS account to book against. Blank means "unknown, user must fill".
    /// **User-editable**: an edit here survives a re-run of `propose`.
    pub bas_konto: String,
    /// VAT rate in percent. [`None`] is the blank cell. **User-editable.**
    pub momssats: Option<Decimal>,
    /// VAT in kronor. [`None`] is the blank cell. **User-editable.**
    pub moms_kr: Option<Decimal>,
    /// Human-readable account name. Written blank by the proposer; a column for
    /// the user's benefit.
    pub konto_namn: String,
    /// How much to trust `bas_konto`.
    pub konfidens: Confidence,
    /// Why this account was chosen, in Swedish, for the reviewing human.
    pub motivering: String,
    /// A description of the matched receipt, or blank.
    pub matchad_kvitto: String,
    /// `J` approves the row for posting. Blank or `N` skips it.
    /// **User-editable**, and the only column that causes anything to be
    /// posted.
    pub godkann: String,
}
