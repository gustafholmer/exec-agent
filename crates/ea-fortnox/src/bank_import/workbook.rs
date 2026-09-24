//! The spreadsheet boundary: everything that knows about `.xlsx` lives here.
//!
//! Upstream this is `bank-import/workbook.ts`, 282 lines around `exceljs`, and
//! it is the one module in this phase whose translation is not a rename.
//! `exceljs` is a read/write library with a row-object model: you open a
//! workbook, mutate a sheet, and write the same workbook back. `calamine` reads
//! only, and reads into a [`Range<Data>`] you index by `(row, column)`. The
//! consequences are set out under *Divergences* below.
//!
//! # Sheets
//!
//! | sheet | direction | what it is |
//! |-------|-----------|------------|
//! | `SEB` | read | the raw bank export |
//! | `Kvitton` | read | receipts the user keeps by hand |
//! | `Förslag` | read + write | the review surface |
//!
//! `Förslag` is read as well as written so that a re-run of the proposer merges
//! rather than clobbers: see [`merge_forslag`].
//!
//! # What a Swedish bank export actually contains
//!
//! Three shapes decide whether this module works on the owner's real file, and
//! all three are pinned by tests:
//!
//! * **A decimal comma.** `-1234,56`, not `-1234.56`.
//! * **A non-breaking space as the thousands separator.** Swedish exports write
//!   `1 234,56` with `U+00A0`, not an ASCII space. A naive `trim()` leaves it in
//!   the middle of the string and the parse fails. Amount parsing goes through
//!   [`money::from_kronor`], which strips every [`char::is_whitespace`] —
//!   `U+00A0` included — exactly as the TypeScript's `\s` did.
//! * **`YYYY-MM-DD` dates**, which may arrive as a real Excel date cell *or* as
//!   a text cell, depending on how the bank wrote the file and what Excel did to
//!   it afterwards. Both are accepted; anything else is an error naming the row.
//!
//! # Divergences from the TypeScript
//!
//! **1. Writing rebuilds the file instead of editing it.** Upstream's
//! `writeForslag(wb, path, rows)` removes the `Förslag` worksheet from the
//! already-loaded `exceljs` workbook, adds a fresh one, and saves the *same*
//! workbook object back to disk — so the `SEB` and `Kvitton` sheets, their
//! formatting, and any extra sheet the user added all survive untouched.
//! `calamine` cannot do that: it parses cell values and discards the rest of the
//! package, and `rust_xlsxwriter` writes a new file from nothing. There is no
//! read-modify-write path between them. So [`write_workbook`] takes the parsed
//! data for *all three* sheets and writes a complete new file. Cell values
//! round-trip; anything a user added that is not a cell value in one of the
//! three known sheets does not. This is the one behaviour in the module that
//! could not be reproduced, and it is a real difference in what the user sees.
//!
//! **2. A malformed date or amount is an error, not a silently dropped row.**
//! Upstream's `readSeb` ends with `if (belopp === undefined || !datum) return;`
//! — a row whose amount will not parse vanishes without a word, and a row whose
//! date is `hej` is kept *with the date `hej`*, because `toIsoDate` returns its
//! input when the regex misses. Both are worse than stopping. Here, a row whose
//! date or amount cell is **present but unreadable** is an error naming the
//! absolute row number, which is the only thing that makes it fixable in Excel.
//!
//! **3. A blank cell still skips the row, as upstream.** That is deliberately
//! *not* folded into divergence 2. Real exports carry trailing summary rows — a
//! `Summa` in the text column with no date and no amount — and erroring on those
//! would refuse whole files that upstream imports fine. The rule is: all three
//! key cells blank, or the date or amount cell blank, skips the row; a cell with
//! something unreadable in it errors.
//!
//! **4. A sheet with no cells at all reads as no rows.** Upstream throws
//! `"SEB"-fliken saknar förväntade kolumner` for a completely empty sheet,
//! because the header lookup finds nothing. An empty sheet has no *wrong*
//! headers, it has no headers, and "no transactions" is the honest answer. A
//! sheet that has cells but not the expected header names still errors.
//!
//! # Language
//!
//! Error messages are English, like the rest of this crate — they are read by
//! whoever is debugging. Text that ends up *in the spreadsheet* the user reviews
//! (`Konfidens`, `Motivering`, the header row) stays Swedish and is reproduced
//! from upstream verbatim.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use calamine::{Data, Range, Reader, Xlsx};
use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_xlsxwriter::{Color, Format, Workbook as XlsxWorkbook};

use super::types::{Confidence, Direction, ForslagRow, KvittoRow, SebRow};
use crate::domain::money;

/// The sheet holding the raw bank export.
pub const SHEET_SEB: &str = "SEB";
/// The sheet holding hand-kept receipts.
pub const SHEET_KVITTON: &str = "Kvitton";
/// The review sheet: proposals in, approvals out.
pub const SHEET_FORSLAG: &str = "Förslag";

/// `Förslag` header row, in column order. Reproduced from upstream verbatim.
pub const FORSLAG_HEADERS: [&str; 13] = [
    "Rad_id",
    "Datum",
    "Text",
    "Belopp",
    "Riktning",
    "BAS_konto",
    "Momssats",
    "Moms_kr",
    "Konto_namn",
    "Konfidens",
    "Motivering",
    "Matchad_kvitto",
    "Godkänn",
];

/// `SEB` header row, in column order.
pub const SEB_HEADERS: [&str; 4] = ["Bokföringsdatum", "Text", "Belopp", "Saldo"];

/// `Kvitton` header row, in column order.
pub const KVITTON_HEADERS: [&str; 10] = [
    "Datum",
    "Belopp_inkl_moms",
    "Leverantör",
    "Beskrivning",
    "Momssats",
    "Moms_kr",
    "Kategori",
    "BAS_konto",
    "Kvitto_ref",
    "Riktning",
];

/// Column widths for the three sheets, matching upstream's.
const FORSLAG_WIDTHS: [f64; 13] = [
    14.0, 12.0, 40.0, 12.0, 10.0, 12.0, 10.0, 10.0, 24.0, 10.0, 44.0, 28.0, 10.0,
];
const SEB_WIDTHS: [f64; 4] = [14.0, 40.0, 12.0, 12.0];
const KVITTON_WIDTHS: [f64; 10] = [
    14.0, 16.0, 24.0, 28.0, 10.0, 10.0, 18.0, 12.0, 12.0, 10.0,
];

// ---- reading ---------------------------------------------------------------

/// A parsed workbook: the cell values of every sheet, and nothing else.
///
/// Held as an owned snapshot rather than a live reader so the read functions
/// can take `&self` and so a caller can read the same sheet twice without
/// re-parsing. A bank export is a few hundred rows; the memory is irrelevant.
#[derive(Debug, Clone)]
pub struct Workbook {
    sheets: Vec<(String, Range<Data>)>,
}

impl Workbook {
    /// Open an `.xlsx` file from disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let reader: Xlsx<_> = calamine::open_workbook(path)
            .with_context(|| format!("opening workbook {}", path.display()))?;
        Self::from_reader(reader)
            .with_context(|| format!("reading workbook {}", path.display()))
    }

    /// Open an `.xlsx` workbook already in memory — an HTTP upload, or a file
    /// this process just wrote. Upstream's `openWorkbookBuffer`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let reader: Xlsx<_> =
            Xlsx::new(Cursor::new(bytes.to_vec())).context("parsing workbook bytes")?;
        Self::from_reader(reader).context("reading workbook bytes")
    }

    fn from_reader<RS>(mut reader: Xlsx<RS>) -> Result<Self>
    where
        RS: std::io::Read + std::io::Seek,
    {
        let names = reader.sheet_names();
        let mut sheets = Vec::with_capacity(names.len());
        for name in names {
            let range = reader
                .worksheet_range(&name)
                .with_context(|| format!("reading sheet {name:?}"))?;
            sheets.push((name, range));
        }
        Ok(Self { sheets })
    }

    /// Every sheet name, in workbook order.
    pub fn sheet_names(&self) -> Vec<&str> {
        self.sheets.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn sheet(&self, name: &str) -> Option<&Range<Data>> {
        self.sheets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r)
    }
}

/// A sheet's header row, resolved to column offsets within the used range.
struct Headers {
    /// Header name → index into the row slice `rows()` yields.
    by_name: HashMap<String, usize>,
    /// 1-based absolute row number of the header row, for error messages.
    header_row: u32,
}

impl Headers {
    fn get(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    fn require(&self, sheet: &str, name: &str) -> Result<usize> {
        self.get(name).ok_or_else(|| {
            anyhow!("sheet {sheet:?} has no {name:?} column in its header row (row {})", self.header_row)
        })
    }
}

/// Read the first row of the used range as a header row.
///
/// Upstream reads absolute row 1. This reads the first row `calamine` reports,
/// which is row 1 for any file written normally and is the first row with any
/// content otherwise — a leading blank row in a hand-edited export then works
/// rather than reading a row of blanks as the headers.
fn headers_of(range: &Range<Data>) -> Option<Headers> {
    let (start_row, _) = range.start()?;
    let first = range.rows().next()?;
    let mut by_name = HashMap::new();
    for (i, cell) in first.iter().enumerate() {
        let name = cell_text(cell);
        let name = name.trim();
        if !name.is_empty() {
            by_name.entry(name.to_string()).or_insert(i);
        }
    }
    Some(Headers {
        by_name,
        header_row: start_row + 1,
    })
}

/// A data row: the cells, plus the 1-based absolute row number Excel shows.
struct DataRow<'a> {
    cells: &'a [Data],
    number: u32,
}

impl DataRow<'_> {
    fn cell(&self, col: Option<usize>) -> &Data {
        col.and_then(|c| self.cells.get(c)).unwrap_or(&Data::Empty)
    }
}

/// Every row after the header, with absolute row numbers attached.
fn data_rows(range: &Range<Data>) -> Vec<DataRow<'_>> {
    let Some((start_row, _)) = range.start() else {
        return Vec::new();
    };
    range
        .rows()
        .enumerate()
        .skip(1)
        .map(|(i, cells)| DataRow {
            cells,
            number: start_row + i as u32 + 1,
        })
        .collect()
}

/// Read the `SEB` sheet.
///
/// # Errors
///
/// * the sheet is absent;
/// * the sheet has cells but no `Bokföringsdatum`, `Text` or `Belopp` column;
/// * a row's date or amount cell holds something that is not a date or a
///   number. The message names the absolute row number.
pub fn read_seb(wb: &Workbook) -> Result<Vec<SebRow>> {
    let range = wb
        .sheet(SHEET_SEB)
        .ok_or_else(|| anyhow!("the workbook has no {SHEET_SEB:?} sheet"))?;
    let Some(h) = headers_of(range) else {
        // Divergence 4: no cells at all means no transactions.
        return Ok(Vec::new());
    };
    let c_date = h.require(SHEET_SEB, "Bokföringsdatum")?;
    let c_text = h.require(SHEET_SEB, "Text")?;
    let c_amount = h.require(SHEET_SEB, "Belopp")?;
    let c_saldo = h.get("Saldo");

    let mut rows = Vec::new();
    for row in data_rows(range) {
        let date_cell = row.cell(Some(c_date));
        let text_cell = row.cell(Some(c_text));
        let amount_cell = row.cell(Some(c_amount));

        // Upstream `isEmptyRow`, plus upstream's own `!datum || belopp ===
        // undefined` skip: see divergence 3.
        // A wholly blank row, and also a partially-filled one — a trailing
        // `Summa` footer with text but no date and no amount. Upstream skips
        // both; see divergence 3.
        if is_blank(date_cell) || is_blank(amount_cell) {
            continue;
        }

        let bokforingsdatum = iso_date(date_cell).ok_or_else(|| {
            anyhow!(
                "{SHEET_SEB} row {}: Bokföringsdatum {} is not a YYYY-MM-DD date",
                row.number,
                describe(date_cell)
            )
        })?;
        let belopp = cell_number(amount_cell).ok_or_else(|| {
            anyhow!(
                "{SHEET_SEB} row {}: Belopp {} is not a number",
                row.number,
                describe(amount_cell)
            )
        })?;

        rows.push(SebRow {
            bokforingsdatum,
            text: cell_text(text_cell),
            belopp,
            saldo: c_saldo.and_then(|c| cell_number(row.cell(Some(c)))),
        });
    }
    Ok(rows)
}

/// Read the `Kvitton` sheet.
///
/// # Errors
///
/// As [`read_seb`], for the `Datum` and `Belopp_inkl_moms` columns.
pub fn read_kvitton(wb: &Workbook) -> Result<Vec<KvittoRow>> {
    let range = wb
        .sheet(SHEET_KVITTON)
        .ok_or_else(|| anyhow!("the workbook has no {SHEET_KVITTON:?} sheet"))?;
    let Some(h) = headers_of(range) else {
        return Ok(Vec::new());
    };
    let c_date = h.require(SHEET_KVITTON, "Datum")?;
    let c_amount = h.require(SHEET_KVITTON, "Belopp_inkl_moms")?;

    let mut rows = Vec::new();
    for row in data_rows(range) {
        let date_cell = row.cell(Some(c_date));
        let amount_cell = row.cell(Some(c_amount));
        if is_blank(date_cell) || is_blank(amount_cell) {
            continue;
        }

        let datum = iso_date(date_cell).ok_or_else(|| {
            anyhow!(
                "{SHEET_KVITTON} row {}: Datum {} is not a YYYY-MM-DD date",
                row.number,
                describe(date_cell)
            )
        })?;
        let belopp_inkl_moms = cell_number(amount_cell).ok_or_else(|| {
            anyhow!(
                "{SHEET_KVITTON} row {}: Belopp_inkl_moms {} is not a number",
                row.number,
                describe(amount_cell)
            )
        })?;

        let text = |name: &str| -> Option<String> {
            let s = cell_text(row.cell(h.get(name)));
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let number = |name: &str| cell_number(row.cell(h.get(name)));

        rows.push(KvittoRow {
            datum,
            belopp_inkl_moms,
            leverantor: text("Leverantör"),
            beskrivning: text("Beskrivning"),
            momssats: number("Momssats"),
            moms_kr: number("Moms_kr"),
            kategori: text("Kategori"),
            bas_konto: text("BAS_konto"),
            kvitto_ref: text("Kvitto_ref"),
            riktning: text("Riktning"),
        });
    }
    Ok(rows)
}

/// Read the existing `Förslag` rows, so a re-run can merge rather than clobber.
///
/// An absent sheet, or one without a `Rad_id` column, is no rows — upstream
/// returns `[]` in both cases rather than throwing, because the sheet not
/// existing yet is the normal first run.
pub fn read_forslag(wb: &Workbook) -> Result<Vec<ForslagRow>> {
    let Some(range) = wb.sheet(SHEET_FORSLAG) else {
        return Ok(Vec::new());
    };
    let Some(h) = headers_of(range) else {
        return Ok(Vec::new());
    };
    let Some(c_id) = h.get("Rad_id") else {
        return Ok(Vec::new());
    };

    let mut rows = Vec::new();
    for row in data_rows(range) {
        let rad_id = cell_text(row.cell(Some(c_id)));
        if rad_id.is_empty() {
            continue;
        }
        let text = |name: &str| cell_text(row.cell(h.get(name)));
        let number = |name: &str| cell_number(row.cell(h.get(name)));

        rows.push(ForslagRow {
            rad_id,
            datum: text("Datum"),
            text: text("Text"),
            // Upstream: a `Belopp` that will not parse becomes 0. Kept — the
            // Förslag sheet is regenerated from the SEB sheet on every run, so
            // a broken amount here is transient, and `post` refuses the row on
            // its blank account long before the amount matters.
            belopp: number("Belopp").unwrap_or(Decimal::ZERO),
            riktning: Direction::from_sheet(&text("Riktning")),
            bas_konto: text("BAS_konto"),
            momssats: number("Momssats"),
            moms_kr: number("Moms_kr"),
            konto_namn: text("Konto_namn"),
            konfidens: Confidence::from_sheet(&text("Konfidens")),
            motivering: text("Motivering"),
            matchad_kvitto: text("Matchad_kvitto"),
            godkann: text("Godkänn"),
        });
    }
    Ok(rows)
}

/// Fold the user's edits back over a freshly-computed proposal, keyed by
/// `Rad_id`.
///
/// The four user-owned columns — `BAS_konto`, `Momssats`, `Moms_kr` and
/// `Godkänn` — win when the existing sheet has a value for them. Everything
/// else is refreshed from the new proposal. This is what makes `propose` safe
/// to re-run after the bank export grows: the rows already reviewed keep their
/// review.
///
/// "Has a value" is a non-empty cell: a blank the user cleared falls back to
/// the proposal, which is upstream's `e.basKonto !== ''` exactly.
pub fn merge_forslag(proposed: &[ForslagRow], existing: &[ForslagRow]) -> Vec<ForslagRow> {
    let prev: HashMap<&str, &ForslagRow> = existing
        .iter()
        .map(|r| (r.rad_id.as_str(), r))
        .collect();

    proposed
        .iter()
        .map(|p| match prev.get(p.rad_id.as_str()) {
            None => p.clone(),
            Some(e) => ForslagRow {
                bas_konto: if e.bas_konto.is_empty() {
                    p.bas_konto.clone()
                } else {
                    e.bas_konto.clone()
                },
                momssats: e.momssats.or(p.momssats),
                moms_kr: e.moms_kr.or(p.moms_kr),
                godkann: if e.godkann.is_empty() {
                    p.godkann.clone()
                } else {
                    e.godkann.clone()
                },
                ..p.clone()
            },
        })
        .collect()
}

// ---- writing ---------------------------------------------------------------

/// Write a complete workbook: the two input sheets and the review sheet.
///
/// See divergence 1 in the module docs — this replaces the whole file rather
/// than editing the `Förslag` sheet of an existing one, because there is no
/// read-modify-write path from `calamine` to `rust_xlsxwriter`. Callers that
/// want upstream's behaviour read the file first and pass all three sheets
/// back in.
pub fn write_workbook(
    path: impl AsRef<Path>,
    seb: &[SebRow],
    kvitton: &[KvittoRow],
    forslag: &[ForslagRow],
) -> Result<()> {
    let bytes = build_workbook(seb, kvitton, Some(forslag))?;
    std::fs::write(path.as_ref(), bytes)
        .with_context(|| format!("writing workbook {}", path.as_ref().display()))
}

/// [`write_workbook`], to memory.
pub fn write_workbook_to_buffer(
    seb: &[SebRow],
    kvitton: &[KvittoRow],
    forslag: &[ForslagRow],
) -> Result<Vec<u8>> {
    build_workbook(seb, kvitton, Some(forslag))
}

/// Write a fresh input workbook with only the `SEB` and `Kvitton` sheets.
///
/// Upstream's `writeInputWorkbook`, used by the `sample` command to hand the
/// user a correctly-shaped file to paste their export into.
pub fn write_input_workbook(
    path: impl AsRef<Path>,
    seb: &[SebRow],
    kvitton: &[KvittoRow],
) -> Result<()> {
    let bytes = build_workbook(seb, kvitton, None)?;
    std::fs::write(path.as_ref(), bytes)
        .with_context(|| format!("writing workbook {}", path.as_ref().display()))
}

fn build_workbook(
    seb: &[SebRow],
    kvitton: &[KvittoRow],
    forslag: Option<&[ForslagRow]>,
) -> Result<Vec<u8>> {
    let mut wb = XlsxWorkbook::new();
    // Upstream's bold white-on-dark-green header, frozen below row 1.
    let header = Format::new()
        .set_bold()
        .set_font_color(Color::RGB(0x00FF_FFFF))
        .set_background_color(Color::RGB(0x002F_5233));

    {
        let s = wb.add_worksheet();
        s.set_name(SHEET_SEB)?;
        write_header(s, &SEB_HEADERS, &SEB_WIDTHS, &header)?;
        for (i, r) in seb.iter().enumerate() {
            let row = i as u32 + 1;
            s.write_string(row, 0, &r.bokforingsdatum)?;
            s.write_string(row, 1, &r.text)?;
            write_amount(s, row, 2, Some(r.belopp))?;
            write_amount(s, row, 3, r.saldo)?;
        }
    }

    {
        let s = wb.add_worksheet();
        s.set_name(SHEET_KVITTON)?;
        write_header(s, &KVITTON_HEADERS, &KVITTON_WIDTHS, &header)?;
        for (i, r) in kvitton.iter().enumerate() {
            let row = i as u32 + 1;
            s.write_string(row, 0, &r.datum)?;
            write_amount(s, row, 1, Some(r.belopp_inkl_moms))?;
            s.write_string(row, 2, r.leverantor.as_deref().unwrap_or(""))?;
            s.write_string(row, 3, r.beskrivning.as_deref().unwrap_or(""))?;
            write_amount(s, row, 4, r.momssats)?;
            write_amount(s, row, 5, r.moms_kr)?;
            s.write_string(row, 6, r.kategori.as_deref().unwrap_or(""))?;
            s.write_string(row, 7, r.bas_konto.as_deref().unwrap_or(""))?;
            s.write_string(row, 8, r.kvitto_ref.as_deref().unwrap_or(""))?;
            s.write_string(row, 9, r.riktning.as_deref().unwrap_or(""))?;
        }
    }

    if let Some(forslag) = forslag {
        let s = wb.add_worksheet();
        s.set_name(SHEET_FORSLAG)?;
        write_header(s, &FORSLAG_HEADERS, &FORSLAG_WIDTHS, &header)?;
        for (i, r) in forslag.iter().enumerate() {
            let row = i as u32 + 1;
            s.write_string(row, 0, &r.rad_id)?;
            s.write_string(row, 1, &r.datum)?;
            s.write_string(row, 2, &r.text)?;
            write_amount(s, row, 3, Some(r.belopp))?;
            s.write_string(row, 4, r.riktning.as_str())?;
            s.write_string(row, 5, &r.bas_konto)?;
            write_amount(s, row, 6, r.momssats)?;
            write_amount(s, row, 7, r.moms_kr)?;
            s.write_string(row, 8, &r.konto_namn)?;
            s.write_string(row, 9, r.konfidens.as_str())?;
            s.write_string(row, 10, &r.motivering)?;
            s.write_string(row, 11, &r.matchad_kvitto)?;
            s.write_string(row, 12, &r.godkann)?;
        }
    }

    Ok(wb.save_to_buffer()?)
}

fn write_header(
    sheet: &mut rust_xlsxwriter::Worksheet,
    names: &[&str],
    widths: &[f64],
    format: &Format,
) -> Result<()> {
    for (i, name) in names.iter().enumerate() {
        sheet.write_string_with_format(0, i as u16, *name, format)?;
        sheet.set_column_width(i as u16, widths[i])?;
    }
    sheet.set_freeze_panes(1, 0)?;
    Ok(())
}

/// Write a [`Decimal`] as an Excel number, or leave the cell blank.
///
/// The value crosses to `f64` because that is the only numeric type the `.xlsx`
/// format has — a spreadsheet cell *is* an IEEE-754 double. It is rounded to
/// öre first, for the same reason [`money::to_api`] does: an unrounded
/// `Decimal` would otherwise land in the cell as `33.333333333333336`.
fn write_amount(
    sheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    value: Option<Decimal>,
) -> Result<()> {
    match value {
        None => {
            sheet.write_blank(row, col, &Format::new())?;
        }
        Some(v) => {
            let rounded = money::round2(v);
            let as_f64 = rounded
                .to_f64()
                .ok_or_else(|| anyhow!("amount {rounded} does not fit an Excel cell"))?;
            sheet.write_number(row, col, as_f64)?;
        }
    }
    Ok(())
}

// ---- cell coercion ---------------------------------------------------------

fn is_blank(cell: &Data) -> bool {
    match cell {
        Data::Empty => true,
        Data::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// A cell as text, trimmed.
///
/// Upstream's `cellText`, with the pieces that only exist in `exceljs` dropped:
/// there is no rich-text object and no cached formula result in a `calamine`
/// `Data`, because `calamine` resolves both to the value before handing it
/// over. A date cell renders as its ISO date, as upstream.
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => format_float(*f),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(_) | Data::DateTimeIso(_) => iso_date(cell).unwrap_or_default(),
        Data::DurationIso(s) => s.trim().to_string(),
        Data::Error(e) => e.to_string(),
    }
}

/// `f64` → the shortest string that round-trips, matching JavaScript's
/// `String(n)` for every value a spreadsheet holds: `1000.0` is `"1000"`, not
/// `"1000.0"`.
fn format_float(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// A cell as an amount.
///
/// Upstream is
/// `Number(String(v).replace(/\s/g, '').replace(',', '.'))`, which this routes
/// through [`money::from_kronor`] for text cells — same whitespace rule
/// (`U+00A0` included), same single-comma rule, and an error instead of a
/// silent `NaN`. Numeric cells skip the text round-trip entirely.
fn cell_number(cell: &Data) -> Option<Decimal> {
    match cell {
        Data::Int(i) => Some(Decimal::from(*i)),
        // Through the shortest round-tripping decimal string rather than
        // `Decimal::try_from(f64)`: a cell holding -1234.56 is the double
        // nearest that decimal, and the shortest string that round-trips to
        // that double is "-1234.56" — exactly the value the user typed.
        Data::Float(f) => format!("{f}").parse::<Decimal>().ok(),
        Data::String(s) => money::from_kronor(s).ok(),
        _ => None,
    }
}

/// A cell as a `YYYY-MM-DD` date, or [`None`] if it is not one.
///
/// Three shapes are accepted, which is every shape a Swedish bank export has
/// been seen in:
///
/// * a real Excel date cell (`Data::DateTime`);
/// * an ISO-8601 date-time cell (`Data::DateTimeIso`);
/// * a text cell starting `YYYY-MM-DD`, which is upstream's
///   `s.match(/^(\d{4})-(\d{2})-(\d{2})/)`.
///
/// A bare number is **not** accepted even though Excel stores dates as serial
/// numbers, because a cell with no date format is indistinguishable from an
/// amount and guessing would silently invent a date. Upstream accepted it and
/// produced the serial number as the "date" string; erroring is better.
fn iso_date(cell: &Data) -> Option<String> {
    match cell {
        Data::DateTime(dt) => {
            if !dt.is_datetime() {
                return None;
            }
            Some(dt.as_datetime()?.date().format("%Y-%m-%d").to_string())
        }
        Data::DateTimeIso(s) | Data::String(s) => iso_date_prefix(s.trim()),
        _ => None,
    }
}

/// The leading `YYYY-MM-DD` of a string, if it is one and it is a real date.
fn iso_date_prefix(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let shaped = bytes.len() >= 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !shaped {
        return None;
    }
    let candidate = &s[0..10];
    NaiveDate::parse_from_str(candidate, "%Y-%m-%d").ok()?;
    Some(candidate.to_string())
}

/// A cell rendered for an error message, quoted so an invisible character —
/// the non-breaking space, above all — is at least bracketed by something.
fn describe(cell: &Data) -> String {
    match cell {
        Data::Empty => "(blank)".to_string(),
        other => format!("{:?}", cell_text(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    // ---- the fixture -------------------------------------------------------

    #[test]
    fn parses_the_bank_export_fixture_into_transactions() {
        let wb = Workbook::open(fixture("bank-import-sample.xlsx")).unwrap();
        let rows = read_seb(&wb).unwrap();

        assert_eq!(
            rows,
            vec![
                // Row 2: the upstream test case, value for value.
                SebRow {
                    bokforingsdatum: "2026-06-01".to_string(),
                    text: "SWISH KAFFE".to_string(),
                    belopp: dec("-42"),
                    saldo: Some(dec("1000")),
                },
                // Row 3: a real Excel date cell, and an amount written as text
                // with a NON-BREAKING SPACE thousands separator and a comma.
                SebRow {
                    bokforingsdatum: "2026-06-02".to_string(),
                    text: "SPOTIFY AB STOCKHOLM".to_string(),
                    belopp: dec("-1234.56"),
                    saldo: Some(dec("8765.44")),
                },
                // Row 4: a decimal comma with no thousands separator.
                SebRow {
                    bokforingsdatum: "2026-06-03".to_string(),
                    text: "CIRCLE K 4711".to_string(),
                    belopp: dec("-89"),
                    saldo: Some(dec("8676.44")),
                },
                // Row 5 is entirely blank and is skipped.
                // Row 6: income, and no Saldo at all.
                SebRow {
                    bokforingsdatum: "2026-06-05".to_string(),
                    text: "KUNDINBETALNING 1001".to_string(),
                    belopp: dec("6250"),
                    saldo: None,
                },
                // Row 7 is a `Summa` footer with no date and no amount: skipped.
            ]
        );
    }

    #[test]
    fn parses_the_fixtures_receipts() {
        let wb = Workbook::open(fixture("bank-import-sample.xlsx")).unwrap();
        let rows = read_kvitton(&wb).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].datum, "2026-06-02");
        assert_eq!(rows[0].belopp_inkl_moms, dec("1234.56"));
        assert_eq!(rows[0].leverantor.as_deref(), Some("Spotify AB"));
        assert_eq!(rows[0].momssats, Some(dec("25")));
        assert_eq!(rows[0].moms_kr, None);
        assert_eq!(rows[0].kategori.as_deref(), Some("programvara"));
        assert_eq!(rows[0].bas_konto, None);
        assert_eq!(rows[0].kvitto_ref.as_deref(), Some("KV-001"));

        // The second receipt states Belopp_inkl_moms as Swedish text, "89,00".
        assert_eq!(rows[1].belopp_inkl_moms, dec("89.00"));
        assert_eq!(rows[1].bas_konto.as_deref(), Some("5611"));
        assert_eq!(rows[1].moms_kr, Some(dec("17.80")));
    }

    #[test]
    fn the_fixture_has_no_forslag_sheet_yet_and_that_is_not_an_error() {
        let wb = Workbook::open(fixture("bank-import-sample.xlsx")).unwrap();
        assert_eq!(read_forslag(&wb).unwrap(), vec![]);
    }

    #[test]
    fn a_header_only_sheet_yields_no_rows_rather_than_an_error() {
        let wb = Workbook::open(fixture("bank-import-empty.xlsx")).unwrap();
        assert_eq!(read_seb(&wb).unwrap(), vec![]);
        assert_eq!(read_kvitton(&wb).unwrap(), vec![]);
    }

    // ---- the Swedish specifics, isolated -----------------------------------

    #[test]
    fn a_decimal_comma_parses() {
        assert_eq!(
            cell_number(&Data::String("1234,56".to_string())),
            Some(dec("1234.56"))
        );
        assert_eq!(
            cell_number(&Data::String("-89,00".to_string())),
            Some(dec("-89.00"))
        );
    }

    #[test]
    fn a_non_breaking_space_thousands_separator_parses() {
        // U+00A0, the character a Swedish bank export actually writes. A naive
        // `trim()` leaves it in the middle of the string and the parse fails.
        let nbsp = "1\u{00A0}234,56";
        assert!(nbsp.trim().contains('\u{00A0}'), "the test string must still contain the NBSP after trim()");
        assert_eq!(cell_number(&Data::String(nbsp.to_string())), Some(dec("1234.56")));

        // And the ordinary space and the narrow no-break space U+202F, which
        // Excel substitutes on some locales.
        assert_eq!(
            cell_number(&Data::String("1 234,56".to_string())),
            Some(dec("1234.56"))
        );
        assert_eq!(
            cell_number(&Data::String("1\u{202F}234,56".to_string())),
            Some(dec("1234.56"))
        );
    }

    #[test]
    fn an_iso_date_parses_and_anything_else_does_not() {
        assert_eq!(
            iso_date(&Data::String("2026-06-01".to_string())),
            Some("2026-06-01".to_string())
        );
        // Upstream keeps only the date part of a longer ISO string.
        assert_eq!(
            iso_date(&Data::String("2026-06-01T09:30:00Z".to_string())),
            Some("2026-06-01".to_string())
        );
        // Not a date.
        assert_eq!(iso_date(&Data::String("01/06/2026".to_string())), None);
        assert_eq!(iso_date(&Data::String("den 1 juni".to_string())), None);
        // Shaped like a date but not one: there is no 31st of June.
        assert_eq!(iso_date(&Data::String("2026-06-31".to_string())), None);
        // A bare Excel serial number is refused rather than guessed at.
        assert_eq!(iso_date(&Data::Float(46174.0)), None);
    }

    #[test]
    fn a_bad_date_errors_with_the_row_number() {
        let seb = vec![
            SebRow {
                bokforingsdatum: "2026-06-01".to_string(),
                text: "OK".to_string(),
                belopp: dec("-10"),
                saldo: None,
            },
            SebRow {
                // Written to the sheet as a plain string, so it survives the
                // round trip as text and is read back as a broken date.
                bokforingsdatum: "den 2 juni".to_string(),
                text: "TRASIG".to_string(),
                belopp: dec("-20"),
                saldo: None,
            },
        ];
        let bytes = write_workbook_to_buffer(&seb, &[], &[]).unwrap();
        let wb = Workbook::from_bytes(&bytes).unwrap();

        let err = read_seb(&wb).unwrap_err().to_string();
        // Header is row 1, the good row is 2, the broken one is row 3.
        assert!(err.contains("row 3"), "message should name the row: {err}");
        assert!(err.contains("Bokföringsdatum"), "message should name the column: {err}");
    }

    #[test]
    fn a_bad_amount_errors_with_the_row_number() {
        // Built by hand rather than round-tripped, because `write_workbook`
        // cannot write a non-numeric amount.
        let mut wb = XlsxWorkbook::new();
        let s = wb.add_worksheet();
        s.set_name(SHEET_SEB).unwrap();
        for (i, h) in SEB_HEADERS.iter().enumerate() {
            s.write_string(0, i as u16, *h).unwrap();
        }
        s.write_string(1, 0, "2026-06-01").unwrap();
        s.write_string(1, 1, "TRASIGT BELOPP").unwrap();
        s.write_string(1, 2, "ungefär hundra").unwrap();
        let bytes = wb.save_to_buffer().unwrap();

        let err = read_seb(&Workbook::from_bytes(&bytes).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("row 2"), "message should name the row: {err}");
        assert!(err.contains("Belopp"), "message should name the column: {err}");
    }

    #[test]
    fn a_missing_sheet_is_an_error_but_a_missing_forslag_sheet_is_not() {
        let bytes = write_input_workbook_to_buffer(&[], &[]);
        let wb = Workbook::from_bytes(&bytes).unwrap();
        assert!(read_seb(&wb).is_ok());
        assert_eq!(read_forslag(&wb).unwrap(), vec![]);

        // A workbook with neither input sheet.
        let mut other = XlsxWorkbook::new();
        other.add_worksheet().set_name("Något annat").unwrap();
        let bytes = other.save_to_buffer().unwrap();
        let wb = Workbook::from_bytes(&bytes).unwrap();
        assert!(read_seb(&wb).unwrap_err().to_string().contains("SEB"));
        assert!(read_kvitton(&wb).unwrap_err().to_string().contains("Kvitton"));
    }

    #[test]
    fn a_sheet_with_cells_but_wrong_headers_errors() {
        let mut wb = XlsxWorkbook::new();
        let s = wb.add_worksheet();
        s.set_name(SHEET_SEB).unwrap();
        s.write_string(0, 0, "Datum").unwrap();
        s.write_string(0, 1, "Beskrivning").unwrap();
        let bytes = wb.save_to_buffer().unwrap();

        let err = read_seb(&Workbook::from_bytes(&bytes).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Bokföringsdatum"), "{err}");
    }

    fn write_input_workbook_to_buffer(seb: &[SebRow], kvitton: &[KvittoRow]) -> Vec<u8> {
        build_workbook(seb, kvitton, None).unwrap()
    }

    // ---- round trip --------------------------------------------------------

    #[test]
    fn a_written_workbook_reads_back_identically() {
        let seb = vec![SebRow {
            bokforingsdatum: "2026-06-01".to_string(),
            text: "SWISH KAFFE".to_string(),
            belopp: dec("-42.50"),
            saldo: Some(dec("1000")),
        }];
        let kvitton = vec![KvittoRow {
            datum: "2026-06-01".to_string(),
            belopp_inkl_moms: dec("42.50"),
            leverantor: Some("Kafé Ö".to_string()),
            beskrivning: None,
            momssats: Some(dec("12")),
            moms_kr: None,
            kategori: Some("representation".to_string()),
            bas_konto: None,
            kvitto_ref: Some("KV-1".to_string()),
            riktning: Some("Utgift".to_string()),
        }];
        let forslag = vec![ForslagRow {
            rad_id: "abc123".to_string(),
            datum: "2026-06-01".to_string(),
            text: "SWISH KAFFE".to_string(),
            belopp: dec("-42.50"),
            riktning: Direction::Utgift,
            bas_konto: "6071".to_string(),
            momssats: Some(dec("12")),
            moms_kr: Some(dec("4.55")),
            konto_namn: String::new(),
            konfidens: Confidence::Hog,
            motivering: "Kategori \"representation\" → 6071.".to_string(),
            matchad_kvitto: "Kafé Ö 2026-06-01 42.50 (KV-1)".to_string(),
            godkann: "J".to_string(),
        }];

        let bytes = write_workbook_to_buffer(&seb, &kvitton, &forslag).unwrap();
        let wb = Workbook::from_bytes(&bytes).unwrap();

        assert_eq!(read_seb(&wb).unwrap(), seb);
        assert_eq!(read_kvitton(&wb).unwrap(), kvitton);
        assert_eq!(read_forslag(&wb).unwrap(), forslag);
        assert_eq!(
            wb.sheet_names(),
            vec![SHEET_SEB, SHEET_KVITTON, SHEET_FORSLAG]
        );
    }

    #[test]
    fn a_blank_momssats_round_trips_as_blank_not_as_zero() {
        // The distinction `merge_forslag` turns on, so it has to survive the
        // file. A zero here would make a cleared cell win over the proposal.
        let forslag = vec![ForslagRow {
            rad_id: "n1".to_string(),
            datum: "2026-06-01".to_string(),
            text: "X".to_string(),
            belopp: dec("-10"),
            riktning: Direction::Utgift,
            bas_konto: String::new(),
            momssats: None,
            moms_kr: None,
            konto_namn: String::new(),
            konfidens: Confidence::Okand,
            motivering: String::new(),
            matchad_kvitto: String::new(),
            godkann: String::new(),
        }];
        let bytes = write_workbook_to_buffer(&[], &[], &forslag).unwrap();
        let back = read_forslag(&Workbook::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(back[0].momssats, None);
        assert_eq!(back[0].moms_kr, None);
    }

    // ---- merge -------------------------------------------------------------

    fn forslag(rad_id: &str) -> ForslagRow {
        ForslagRow {
            rad_id: rad_id.to_string(),
            datum: "2026-06-01".to_string(),
            text: "X".to_string(),
            belopp: dec("-100"),
            riktning: Direction::Utgift,
            bas_konto: "5410".to_string(),
            momssats: Some(dec("25")),
            moms_kr: Some(dec("20")),
            konto_namn: String::new(),
            konfidens: Confidence::Lag,
            motivering: "ny".to_string(),
            matchad_kvitto: String::new(),
            godkann: String::new(),
        }
    }

    #[test]
    fn merge_keeps_the_users_four_columns_and_refreshes_the_rest() {
        let proposed = vec![forslag("a")];
        let existing = vec![ForslagRow {
            bas_konto: "6110".to_string(),
            momssats: Some(dec("12")),
            moms_kr: Some(dec("7")),
            godkann: "J".to_string(),
            motivering: "gammal".to_string(),
            konfidens: Confidence::Okand,
            ..forslag("a")
        }];

        let merged = merge_forslag(&proposed, &existing);
        assert_eq!(merged.len(), 1);
        // User-owned: kept.
        assert_eq!(merged[0].bas_konto, "6110");
        assert_eq!(merged[0].momssats, Some(dec("12")));
        assert_eq!(merged[0].moms_kr, Some(dec("7")));
        assert_eq!(merged[0].godkann, "J");
        // Everything else: refreshed from the proposal.
        assert_eq!(merged[0].motivering, "ny");
        assert_eq!(merged[0].konfidens, Confidence::Lag);
    }

    #[test]
    fn merge_falls_back_to_the_proposal_for_cleared_cells() {
        let proposed = vec![forslag("a")];
        let existing = vec![ForslagRow {
            bas_konto: String::new(),
            momssats: None,
            moms_kr: None,
            godkann: String::new(),
            ..forslag("a")
        }];
        let merged = merge_forslag(&proposed, &existing);
        assert_eq!(merged[0].bas_konto, "5410");
        assert_eq!(merged[0].momssats, Some(dec("25")));
        assert_eq!(merged[0].moms_kr, Some(dec("20")));
        assert_eq!(merged[0].godkann, "");
    }

    #[test]
    fn merge_passes_through_a_row_the_sheet_has_never_seen() {
        let merged = merge_forslag(&[forslag("new")], &[forslag("old")]);
        assert_eq!(merged, vec![forslag("new")]);
    }

    #[test]
    fn merge_drops_rows_that_are_no_longer_proposed() {
        // A row the bank export no longer contains disappears, approval and
        // all. Upstream does the same: it maps over `proposed`, never
        // `existing`.
        let merged = merge_forslag(&[], &[forslag("gone")]);
        assert_eq!(merged, vec![]);
    }

    // ---- coercion details --------------------------------------------------

    #[test]
    fn cell_text_renders_numbers_the_way_javascript_does() {
        assert_eq!(cell_text(&Data::Float(1000.0)), "1000");
        assert_eq!(cell_text(&Data::Float(-42.5)), "-42.5");
        assert_eq!(cell_text(&Data::Int(7)), "7");
        assert_eq!(cell_text(&Data::Empty), "");
        assert_eq!(cell_text(&Data::String("  padded  ".to_string())), "padded");
    }

    #[test]
    fn cell_number_refuses_an_ambiguous_two_comma_amount() {
        // `money::from_kronor`'s rule: "1,234.56" is an anglophone thousands
        // separator in a Swedish export — a data problem to surface, not to
        // guess at. Upstream's single `.replace(',', '.')` turned it into
        // "1.234.56" and then NaN, so it was refused there too, just silently.
        assert_eq!(cell_number(&Data::String("1,234.56".to_string())), None);
    }
}
