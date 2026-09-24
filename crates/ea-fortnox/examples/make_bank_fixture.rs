//! Regenerate the bank-import `.xlsx` test fixtures.
//!
//! ```text
//! cargo run -p ea-fortnox --example make_bank_fixture
//! ```
//!
//! # Why this exists, and what the fixtures are based on
//!
//! **There is no real bank export on this machine, and there must never be one
//! in this repository.** A real SEB export is the owner's financial history:
//! counterparties, amounts, dates, running balance. Committing that to a git
//! repo would be a far worse outcome than a slightly less realistic fixture.
//!
//! So the fixtures are synthesised. Their *shape* is not invented — it is taken
//! from two places that do document it:
//!
//! * `bank-import/workbook.ts`, whose `SEB_HEADERS` and `KVITTON_HEADERS`
//!   constants give the exact column names and order, and whose `cellNumber`
//!   (`String(v).replace(/\s/g, '').replace(',', '.')`) exists precisely because
//!   amounts arrive as text with a space and a comma in them;
//! * `bank-import/workbook.test.ts`, whose single case is
//!   `['2026-06-01', 'SWISH KAFFE', -42, 1000]` — reproduced verbatim as the
//!   first data row, so the one upstream assertion has a direct counterpart.
//!
//! Every counterparty and amount beyond that is made up. `SPOTIFY AB
//! STOCKHOLM` and `CIRCLE K 4711` are copied from `code.test.ts`'s keyword
//! cases, which are themselves synthetic.
//!
//! # What the sample fixture deliberately contains
//!
//! The point of a fixture is the awkward rows, so it has one of each:
//!
//! | row | what it exercises |
//! |-----|-------------------|
//! | 2 | the upstream test case: text date, numeric amount |
//! | 3 | a **real Excel date cell**, and an amount as text with a **non-breaking space** thousands separator and a decimal comma |
//! | 4 | a decimal comma with no thousands separator |
//! | 5 | a wholly blank row in the middle of the data |
//! | 6 | income (positive amount), and no `Saldo` at all |
//! | 7 | a trailing `Summa` footer: text, no date, no amount |
//!
//! This writes the `.xlsx` with `rust_xlsxwriter` **directly**, not through
//! [`ea_fortnox::bank_import::workbook::write_workbook`]. That is on purpose: a
//! fixture produced by the code under test only proves the reader agrees with
//! the writer. Writing the cells by hand here lets the fixture hold shapes the
//! writer would never produce — a text amount, a date cell, a footer row.
//!
//! It remains a synthetic file from a Rust writer, not a file Excel or SEB
//! produced. That is the limit of what can be done offline, and it is the one
//! thing about these fixtures that is weaker than a real export.

use std::path::{Path, PathBuf};

use rust_xlsxwriter::{Color, ExcelDateTime, Format, Workbook, Worksheet, XlsxError};

/// `U+00A0`, the thousands separator a Swedish bank export actually writes.
const NBSP: char = '\u{00A0}';

const SEB_HEADERS: [&str; 4] = ["Bokföringsdatum", "Text", "Belopp", "Saldo"];
const KVITTON_HEADERS: [&str; 10] = [
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

fn main() -> Result<(), XlsxError> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&dir).expect("creating tests/fixtures");

    write_sample(&dir.join("bank-import-sample.xlsx"))?;
    write_empty(&dir.join("bank-import-empty.xlsx"))?;

    println!("wrote fixtures to {}", dir.display());
    Ok(())
}

fn header_format() -> Format {
    Format::new()
        .set_bold()
        .set_font_color(Color::RGB(0x00FF_FFFF))
        .set_background_color(Color::RGB(0x002F_5233))
}

fn write_headers(sheet: &mut Worksheet, names: &[&str]) -> Result<(), XlsxError> {
    let fmt = header_format();
    for (i, name) in names.iter().enumerate() {
        sheet.write_string_with_format(0, i as u16, *name, &fmt)?;
    }
    sheet.set_freeze_panes(1, 0)?;
    Ok(())
}

fn write_sample(path: &Path) -> Result<(), XlsxError> {
    let mut wb = Workbook::new();
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd");

    {
        let s = wb.add_worksheet();
        s.set_name("SEB")?;
        write_headers(s, &SEB_HEADERS)?;

        // Row 2 — upstream's own test case, verbatim.
        s.write_string(1, 0, "2026-06-01")?;
        s.write_string(1, 1, "SWISH KAFFE")?;
        s.write_number(1, 2, -42.0)?;
        s.write_number(1, 3, 1000.0)?;

        // Row 3 — a genuine Excel date cell, and an amount as text with the
        // non-breaking space and the decimal comma. This row is the whole
        // reason the fixture is not just four numbers.
        s.write_datetime_with_format(2, 0, ExcelDateTime::from_ymd(2026, 6, 2)?, &date_fmt)?;
        s.write_string(2, 1, "SPOTIFY AB STOCKHOLM")?;
        s.write_string(2, 2, &format!("-1{NBSP}234,56"))?;
        s.write_number(2, 3, 8765.44)?;

        // Row 4 — a decimal comma with no thousands separator.
        s.write_string(3, 0, "2026-06-03")?;
        s.write_string(3, 1, "CIRCLE K 4711")?;
        s.write_string(3, 2, "-89,00")?;
        s.write_number(3, 3, 8676.44)?;

        // Row 5 — deliberately left entirely blank.

        // Row 6 — income, and no running balance on the line.
        s.write_string(5, 0, "2026-06-05")?;
        s.write_string(5, 1, "KUNDINBETALNING 1001")?;
        s.write_number(5, 2, 6250.0)?;

        // Row 7 — the trailing footer a real export carries. No date, no
        // amount, so the reader must skip it rather than choke on it.
        s.write_string(6, 1, "Summa")?;
        s.write_number(6, 3, 14926.44)?;
    }

    {
        let s = wb.add_worksheet();
        s.set_name("Kvitton")?;
        write_headers(s, &KVITTON_HEADERS)?;

        // A receipt with a category but no pre-coded account and no exact VAT.
        s.write_string(1, 0, "2026-06-02")?;
        s.write_number(1, 1, 1234.56)?;
        s.write_string(1, 2, "Spotify AB")?;
        s.write_string(1, 3, "Musik och ljud")?;
        s.write_number(1, 4, 25.0)?;
        // Moms_kr deliberately blank.
        s.write_string(1, 6, "programvara")?;
        // BAS_konto deliberately blank.
        s.write_string(1, 8, "KV-001")?;
        s.write_string(1, 9, "Utgift")?;

        // A receipt pre-coded by hand, with its gross written Swedish-style as
        // text and an exact VAT amount.
        s.write_string(2, 0, "2026-06-03")?;
        s.write_string(2, 1, "89,00")?;
        s.write_string(2, 2, "Circle K")?;
        s.write_number(2, 4, 25.0)?;
        s.write_number(2, 5, 17.80)?;
        s.write_string(2, 6, "drivmedel")?;
        s.write_string(2, 7, "5611")?;
        s.write_string(2, 8, "KV-002")?;
        s.write_string(2, 9, "Utgift")?;
    }

    // No `Förslag` sheet: this is what the user hands over before the first
    // `propose` run, and reading it must not be an error.

    wb.save(path)?;
    Ok(())
}

/// Both input sheets, headers only. "No transactions" must read as no rows.
fn write_empty(path: &Path) -> Result<(), XlsxError> {
    let mut wb = Workbook::new();

    let s = wb.add_worksheet();
    s.set_name("SEB")?;
    write_headers(s, &SEB_HEADERS)?;

    let s = wb.add_worksheet();
    s.set_name("Kvitton")?;
    write_headers(s, &KVITTON_HEADERS)?;

    wb.save(path)?;
    Ok(())
}
