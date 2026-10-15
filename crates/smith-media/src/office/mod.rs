//! Data-only XLSX packages and bounded table exports. No formula evaluation.
mod export;
mod ods;
mod package;
mod pdf;
mod pptx;
mod xls;
mod xlsx;

use crate::{error, forbidden_char, Content, ErrorKind, Format, Limits, Result};
pub use export::{export_table, write_workbook};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
pub use xlsx::read_workbook;

/// Read the documented, passive data subset of the specified spreadsheet format.
/// XLSM containers containing macros are rejected; no macro code is stripped.
pub fn read_spreadsheet(format: Format, bytes: &[u8], limits: &Limits) -> Result<Workbook> {
    match format {
        Format::Xlsx => read_workbook(bytes, limits),
        Format::Xlsm => xlsx::read_xml_workbook(bytes, limits, true),
        Format::Ods => ods::read(bytes, limits),
        Format::Xls => xls::read(bytes, limits),
        _ => Err(unsupported()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CellValue {
    Blank,
    Text(String),
    /// Exact finite source lexical number for XML/text. BIFF binary64/RK values
    /// use a round-trip decimal representation of their stored binary value.
    Number(String),
    Boolean(bool),
    Error(String),
}
impl CellValue {
    fn display(&self) -> &str {
        match self {
            Self::Blank => "",
            Self::Text(v) | Self::Number(v) | Self::Error(v) => v,
            Self::Boolean(true) => "TRUE",
            Self::Boolean(false) => "FALSE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberFormat {
    /// Original Excel builtin/custom format identifier. Custom IDs are >= 164.
    pub id: u32,
    pub code: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    /// One-based coordinates. Sparse gaps never cause dense allocation.
    pub row: u32,
    pub column: u16,
    pub value: CellValue,
    pub number_format: Option<NumberFormat>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SheetVisibility {
    #[default]
    Visible,
    Hidden,
    VeryHidden,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sheet {
    pub name: String,
    pub visibility: SheetVisibility,
    pub cells: Vec<Cell>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workbook {
    /// Numeric date cells retain their raw serial plus number-format metadata.
    pub date_1904: bool,
    pub sheets: Vec<Sheet>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<CellValue>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Markdown,
    Xlsx,
    Pdf,
    Pptx,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDocument {
    pub format: Format,
    pub bytes: Vec<u8>,
}

pub(super) fn inspect(format: Format, bytes: &[u8], limits: &Limits) -> Result<Content> {
    let rendered = match format {
        Format::Pdf => serde_json::to_string(&pdf::read_generated(bytes, limits)?),
        Format::Pptx => serde_json::to_string(&pptx::read_generated(bytes, limits)?),
        _ => serde_json::to_string(&read_spreadsheet(format, bytes, limits)?),
    }
    .map_err(|_| invalid())?;
    let total_context_chars = rendered.chars().count();
    Ok(Content::Document {
        context: rendered.chars().take(limits.max_context_chars).collect(),
        truncated: total_context_chars > limits.max_context_chars,
        total_context_chars,
        rows: None,
        columns: None,
    })
}
fn invalid() -> crate::Error {
    error(ErrorKind::InvalidContent, "invalid structured workbook")
}
fn unsupported() -> crate::Error {
    error(
        ErrorKind::UnsupportedFormat,
        "workbook contains unsupported active content or structure",
    )
}
fn exceeded() -> crate::Error {
    error(ErrorKind::LimitExceeded, "workbook exceeds resource budget")
}

fn validate_text(text: &str, limits: &Limits) -> Result<()> {
    if text.len() > limits.max_csv_field_bytes || text.encode_utf16().count() > 32767 {
        return Err(exceeded());
    }
    if text
        .chars()
        .any(|c| forbidden_char(c) || matches!(c, '\u{fffe}' | '\u{ffff}'))
    {
        return Err(invalid());
    }
    Ok(())
}
fn validate_number(text: &str) -> Result<()> {
    if text.is_empty()
        || text.len() > 128
        || text
            .bytes()
            .any(|b| !b.is_ascii_digit() && !b"+-.eE".contains(&b))
        || !text.parse::<f64>().is_ok_and(f64::is_finite)
    {
        return Err(invalid());
    }
    Ok(())
}
fn validate_value(value: &CellValue, limits: &Limits) -> Result<()> {
    validate_text(value.display(), limits)?;
    match value {
        CellValue::Number(v) => validate_number(v)?,
        CellValue::Error(v)
            if ![
                "#NULL!",
                "#DIV/0!",
                "#VALUE!",
                "#REF!",
                "#NAME?",
                "#NUM!",
                "#N/A",
                "#GETTING_DATA",
            ]
            .contains(&v.as_str()) =>
        {
            return Err(invalid())
        }
        _ => (),
    }
    Ok(())
}
fn validate_workbook(book: &Workbook, limits: &Limits) -> Result<()> {
    limits.validate()?;
    if book.sheets.is_empty() {
        return Err(invalid());
    }
    if book.sheets.len() > limits.max_sheets {
        return Err(exceeded());
    }
    if !book
        .sheets
        .iter()
        .any(|s| s.visibility == SheetVisibility::Visible)
    {
        return Err(invalid());
    }
    let mut names = HashSet::new();
    let mut count = 0;
    let mut text_bytes = 0;
    let mut formats = HashSet::new();
    for sheet in &book.sheets {
        validate_sheet_name(&sheet.name)?;
        if !names.insert(sheet.name.to_lowercase()) {
            return Err(invalid());
        }
        let mut coordinates = HashSet::new();
        for cell in &sheet.cells {
            count += 1;
            if count > limits.max_cells {
                return Err(exceeded());
            }
            validate_coordinate(cell.row, cell.column, limits)?;
            if !coordinates.insert((cell.row, cell.column)) {
                return Err(invalid());
            }
            validate_value(&cell.value, limits)?;
            text_bytes += cell.value.display().len();
            if text_bytes > limits.max_workbook_text_bytes {
                return Err(exceeded());
            }
            if let Some(format) = &cell.number_format {
                if format.id == 0
                    || format.id > 65535
                    || (format.id >= 164) != format.code.is_some()
                {
                    return Err(invalid());
                }
                if let Some(code) = &format.code {
                    if code.len() > 256 {
                        return Err(exceeded());
                    }
                    validate_text(code, limits)?;
                }
                formats.insert(format);
                if formats.len() > 128 {
                    return Err(exceeded());
                }
            }
        }
    }
    Ok(())
}
fn validate_sheet_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.encode_utf16().count() > 31
        || name.contains(['[', ']', ':', '*', '?', '/', '\\'])
        || name.starts_with('\'')
        || name.ends_with('\'')
        || name.chars().any(|c| c.is_control() || forbidden_char(c))
    {
        return Err(invalid());
    }
    Ok(())
}
fn validate_coordinate(row: u32, col: u16, limits: &Limits) -> Result<()> {
    if row == 0 || col == 0 {
        return Err(invalid());
    }
    if row as usize > limits.max_csv_rows || col as usize > limits.max_csv_columns {
        return Err(exceeded());
    }
    Ok(())
}
fn coordinate(reference: &str, limits: &Limits) -> Result<(u32, u16)> {
    if reference.len() > 12 {
        return Err(invalid());
    }
    let letters = reference.bytes().take_while(u8::is_ascii_uppercase).count();
    if letters == 0 || letters == reference.len() {
        return Err(invalid());
    }
    let mut col = 0u16;
    for byte in reference.bytes().take(letters) {
        col = col
            .checked_mul(26)
            .and_then(|n| n.checked_add(u16::from(byte - b'A' + 1)))
            .ok_or_else(exceeded)?;
    }
    let tail = &reference[letters..];
    if tail.starts_with('0') || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let row = tail.parse::<u32>().map_err(|_| invalid())?;
    validate_coordinate(row, col, limits)?;
    Ok((row, col))
}
fn reference(row: u32, mut col: u16) -> String {
    let mut letters = Vec::new();
    while col > 0 {
        col -= 1;
        letters.push(b'A' + (col % 26) as u8);
        col /= 26;
    }
    letters.reverse();
    format!(
        "{}{row}",
        String::from_utf8(letters).expect("ASCII column letters")
    )
}

// OOXML's _xHHHH_ layer is distinct from XML entity decoding. Decode one pass,
// including surrogate pairs, so escaped literal underscores are not double-decoded.
fn decode_excel_string(text: &str, limits: &Limits) -> Result<String> {
    let mut units = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let tail = &text[pos..];
        if tail.len() >= 7
            && tail.as_bytes()[0..2] == *b"_x"
            && tail.as_bytes()[6] == b'_'
            && tail.as_bytes()[2..6].iter().all(u8::is_ascii_hexdigit)
        {
            units.push(u16::from_str_radix(&tail[2..6], 16).map_err(|_| invalid())?);
            pos += 7;
        } else {
            let c = tail.chars().next().ok_or_else(invalid)?;
            units.extend(c.encode_utf16(&mut [0; 2]).iter().copied());
            pos += c.len_utf8();
        }
        if units.len() > limits.max_csv_field_bytes {
            return Err(exceeded());
        }
    }
    let decoded = String::from_utf16(&units).map_err(|_| invalid())?;
    validate_text(&decoded, limits)?;
    Ok(decoded)
}
fn xml_escape(text: &str, excel: bool) -> String {
    let mut out = String::new();
    for (i, c) in text.char_indices() {
        if excel
            && c == '_'
            && text[i..].len() >= 7
            && text.as_bytes()[i + 1] == b'x'
            && text.as_bytes()[i + 6] == b'_'
            && text.as_bytes()[i + 2..i + 6]
                .iter()
                .all(u8::is_ascii_hexdigit)
        {
            out.push_str("_x005F_");
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            _ => out.push(c),
        }
    }
    out
}
