use super::{
    package::{CT, MAIN, PKG_REL, REL},
    *,
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
};
use zip::{write::SimpleFileOptions, CompressionMethod, DateTime, ZipWriter};

/// Generate a data-only XLSX. Text cells always use inline strings, including
/// leading zeros, numeric-looking identifiers and formula-looking strings.
pub fn write_workbook(book: &Workbook, limits: &Limits) -> Result<Vec<u8>> {
    validate_workbook(book, limits)?;
    let mut formats: Vec<Option<NumberFormat>> = vec![None];
    let mut custom = BTreeMap::new();
    for cell in book.sheets.iter().flat_map(|sheet| &sheet.cells) {
        if cell.number_format.is_some() && !formats.contains(&cell.number_format) {
            formats.push(cell.number_format.clone());
        }
        if let Some(NumberFormat {
            id,
            code: Some(code),
        }) = &cell.number_format
        {
            if custom
                .insert(*id, code.clone())
                .is_some_and(|other| other != *code)
            {
                return Err(invalid());
            }
        }
    }
    if formats.len() > 128 {
        return Err(exceeded());
    }
    let mut parts = BTreeMap::new();
    let mut types = format!("<Types xmlns=\"{CT}\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>");
    let mut workbook = format!(
        "<workbook xmlns=\"{MAIN}\" xmlns:r=\"{REL}\"><workbookPr date1904=\"{}\"/><sheets>",
        u8::from(book.date_1904)
    );
    let mut rels = format!("<Relationships xmlns=\"{PKG_REL}\"><Relationship Id=\"styles\" Type=\"{REL}/styles\" Target=\"styles.xml\"/>");
    for (index, sheet) in book.sheets.iter().enumerate() {
        let id = index + 1;
        let state = match sheet.visibility {
            SheetVisibility::Visible => "visible",
            SheetVisibility::Hidden => "hidden",
            SheetVisibility::VeryHidden => "veryHidden",
        };
        workbook.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{id}\" state=\"{state}\" r:id=\"s{id}\"/>",
            xml_escape(&sheet.name, true)
        ));
        rels.push_str(&format!("<Relationship Id=\"s{id}\" Type=\"{REL}/worksheet\" Target=\"worksheets/sheet{id}.xml\"/>"));
        types.push_str(&format!("<Override PartName=\"/xl/worksheets/sheet{id}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"));
        parts.insert(
            format!("xl/worksheets/sheet{id}.xml"),
            sheet_xml(sheet, &formats, limits)?,
        );
    }
    workbook.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");
    types.push_str("</Types>");
    parts.insert("[Content_Types].xml".into(), types);
    parts.insert("_rels/.rels".into(), format!("<Relationships xmlns=\"{PKG_REL}\"><Relationship Id=\"workbook\" Type=\"{REL}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"));
    parts.insert("xl/workbook.xml".into(), workbook);
    parts.insert("xl/_rels/workbook.xml.rels".into(), rels);
    parts.insert("xl/styles.xml".into(), styles_xml(&formats, &custom));
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o600);
    let mut total = 0;
    if parts.len() > limits.max_zip_entries {
        return Err(exceeded());
    }
    for (name, xml) in parts {
        let document =
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>{xml}");
        total += document.len();
        if document.len() > limits.max_zip_part_bytes || total > limits.max_zip_total_bytes {
            return Err(exceeded());
        }
        zip.start_file(name, options).map_err(|_| invalid())?;
        zip.write_all(document.as_bytes()).map_err(|_| invalid())?;
    }
    let bytes = zip.finish().map_err(|_| invalid())?.into_inner();
    if bytes.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    // Generated files must satisfy the same independent parsing/zip/XML admission
    // policy before a caller can persist them through the managed atomic store.
    read_workbook(&bytes, limits)?;
    Ok(bytes)
}

fn sheet_xml(sheet: &Sheet, formats: &[Option<NumberFormat>], limits: &Limits) -> Result<String> {
    let mut xml = format!("<worksheet xmlns=\"{MAIN}\"><sheetData>");
    let mut cells: Vec<_> = sheet.cells.iter().collect();
    cells.sort_by_key(|c| (c.row, c.column));
    let mut current_row = None;
    for cell in cells {
        if current_row != Some(cell.row) {
            if current_row.is_some() {
                xml.push_str("</row>");
            }
            xml.push_str(&format!("<row r=\"{}\">", cell.row));
            current_row = Some(cell.row);
        }
        let style = formats
            .iter()
            .position(|f| f == &cell.number_format)
            .ok_or_else(invalid)?;
        let reference = reference(cell.row, cell.column);
        let (kind, body) = match &cell.value {
            CellValue::Blank => ("n", String::new()),
            CellValue::Text(value) => (
                "inlineStr",
                format!(
                    "<is><t xml:space=\"preserve\">{}</t></is>",
                    xml_escape(value, true)
                ),
            ),
            CellValue::Number(value) => ("n", format!("<v>{value}</v>")),
            CellValue::Boolean(value) => ("b", format!("<v>{}</v>", u8::from(*value))),
            CellValue::Error(value) => ("e", format!("<v>{}</v>", xml_escape(value, false))),
        };
        xml.push_str(&format!(
            "<c r=\"{reference}\" t=\"{kind}\" s=\"{style}\">{body}</c>"
        ));
        if xml.len() > limits.max_zip_part_bytes {
            return Err(exceeded());
        }
    }
    if current_row.is_some() {
        xml.push_str("</row>");
    }
    xml.push_str("</sheetData></worksheet>");
    Ok(xml)
}
fn styles_xml(formats: &[Option<NumberFormat>], custom: &BTreeMap<u32, String>) -> String {
    let mut xml = format!(
        "<styleSheet xmlns=\"{MAIN}\"><numFmts count=\"{}\">",
        custom.len()
    );
    for (id, code) in custom {
        xml.push_str(&format!(
            "<numFmt numFmtId=\"{id}\" formatCode=\"{}\"/>",
            xml_escape(code, true)
        ));
    }
    xml.push_str("</numFmts><fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>");
    xml.push_str(&format!("<cellXfs count=\"{}\">", formats.len()));
    for format in formats {
        let id = format.as_ref().map_or(0, |f| f.id);
        xml.push_str(&format!("<xf numFmtId=\"{id}\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>"));
    }
    xml.push_str("</cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>");
    xml
}

/// Export CSV, Markdown or XLSX. Unlike the original writer, text is never
/// numerically coerced. CSV rejects formula-like text because CSV has no cell types.
pub fn export_table(
    table: &Table,
    format: ExportFormat,
    limits: &Limits,
) -> Result<GeneratedDocument> {
    let book = table_workbook(table, limits)?;
    let (format, bytes) = match format {
        ExportFormat::Xlsx => (Format::Xlsx, write_workbook(&book, limits)?),
        ExportFormat::Csv => (Format::Csv, csv(table)?.into_bytes()),
        ExportFormat::Markdown => (Format::Markdown, markdown(table).into_bytes()),
        ExportFormat::Pdf => (Format::Pdf, super::pdf::write(table, limits)?),
        ExportFormat::Pptx => (Format::Pptx, super::pptx::write(table, limits)?),
    };
    let extension = match format {
        Format::Csv => "csv",
        Format::Markdown => "md",
        Format::Xlsx => "xlsx",
        Format::Pdf => "pdf",
        Format::Pptx => "pptx",
        _ => unreachable!(),
    };
    crate::inspect(
        crate::AttachmentRequest::new(format!("export.{extension}"), None),
        &bytes,
        limits,
    )?;
    Ok(GeneratedDocument { format, bytes })
}
pub(super) fn table_workbook(table: &Table, limits: &Limits) -> Result<Workbook> {
    limits.validate()?;
    let columns = if !table.headers.is_empty() {
        table.headers.len()
    } else {
        table.rows.first().map_or(0, Vec::len)
    };
    let rows = table.rows.len() + usize::from(!table.headers.is_empty());
    if columns > limits.max_csv_columns
        || rows > limits.max_csv_rows
        || rows.saturating_mul(columns) > limits.max_cells
    {
        return Err(exceeded());
    }
    if table
        .rows
        .iter()
        .any(|row| row.len() != columns || row.is_empty())
    {
        return Err(invalid());
    }
    let mut cells = Vec::new();
    for (column, header) in table.headers.iter().enumerate() {
        cells.push(Cell {
            row: 1,
            column: (column + 1) as u16,
            value: CellValue::Text(header.clone()),
            number_format: None,
        });
    }
    let offset = usize::from(!table.headers.is_empty());
    for (row_index, row) in table.rows.iter().enumerate() {
        for (column, value) in row.iter().enumerate() {
            cells.push(Cell {
                row: (row_index + offset + 1) as u32,
                column: (column + 1) as u16,
                value: value.clone(),
                number_format: None,
            });
        }
    }
    let book = Workbook {
        date_1904: false,
        sheets: vec![Sheet {
            name: "Sheet1".into(),
            visibility: SheetVisibility::Visible,
            cells,
        }],
    };
    validate_workbook(&book, limits)?;
    Ok(book)
}
fn csv(table: &Table) -> Result<String> {
    fn field(text: &str) -> String {
        if text.contains([',', '"', '\r', '\n']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    }
    fn literal(text: &str) -> Result<()> {
        if text.trim_start().starts_with(['=', '+', '-', '@']) {
            return Err(error(
                ErrorKind::UnsupportedFormat,
                "CSV cannot safely preserve formula-like text; use XLSX text cells",
            ));
        }
        Ok(())
    }
    let mut result = String::new();
    if !table.headers.is_empty() {
        for text in &table.headers {
            literal(text)?;
        }
        result.push_str(
            &table
                .headers
                .iter()
                .map(|v| field(v))
                .collect::<Vec<_>>()
                .join(","),
        );
        result.push('\n');
    }
    for row in &table.rows {
        for value in row {
            if let CellValue::Text(text) = value {
                literal(text)?;
            }
        }
        result.push_str(
            &row.iter()
                .map(|v| field(v.display()))
                .collect::<Vec<_>>()
                .join(","),
        );
        result.push('\n');
    }
    Ok(result)
}
fn markdown(table: &Table) -> String {
    fn cell(text: &str) -> String {
        let mut out = String::new();
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '\n' => out.push_str("<br>"),
                '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '!' | '|' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }
    let columns = table
        .headers
        .len()
        .max(table.rows.first().map_or(0, Vec::len))
        .max(1);
    let headers: Vec<_> = (0..columns)
        .map(|i| cell(table.headers.get(i).map_or("", String::as_str)))
        .collect();
    let mut out = format!(
        "| {} |\n| {} |\n",
        headers.join(" | "),
        vec!["---"; columns].join(" | ")
    );
    for row in &table.rows {
        out.push_str(&format!(
            "| {} |\n",
            row.iter()
                .map(|v| cell(v.display()))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    out
}
