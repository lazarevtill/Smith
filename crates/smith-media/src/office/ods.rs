//! Passive ODF data tables. Repeat counts are checked before expansion; no
//! formulas, objects, external links or implicit conversion of numeric strings.
use super::*;
use roxmltree::Node;
const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const T: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const X: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const M: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";

pub(super) fn read(bytes: &[u8], limits: &Limits) -> Result<Workbook> {
    limits.validate()?;
    let parts = package::unzip_ods(bytes, limits)?;
    if parts.get("mimetype").map(String::as_str) != Some(Format::Ods.mime()) {
        return Err(unsupported());
    }
    if parts.keys().any(|k| {
        ![
            "mimetype",
            "content.xml",
            "styles.xml",
            "meta.xml",
            "settings.xml",
            "META-INF/manifest.xml",
        ]
        .contains(&k.as_str())
    }) {
        return Err(unsupported());
    }
    for (name, text) in &parts {
        if name == "mimetype" {
            continue;
        }
        let doc = package::xml(text, limits)?;
        for n in doc.descendants().filter(Node::is_element) {
            if matches!(
                n.tag_name().name(),
                "scripts"
                    | "script"
                    | "object"
                    | "object-ole"
                    | "frame"
                    | "image"
                    | "a"
                    | "database-range"
                    | "dde-link"
                    | "encryption-data"
                    | "number-style"
                    | "date-style"
                    | "time-style"
                    | "currency-style"
                    | "percentage-style"
            ) || n.attributes().any(|a| {
                matches!(
                    a.name(),
                    "href"
                        | "formula"
                        | "display"
                        | "number-matrix-columns-spanned"
                        | "number-matrix-rows-spanned"
                )
            }) {
                return Err(unsupported());
            }
        }
    }
    let manifest = package::xml(
        parts.get("META-INF/manifest.xml").ok_or_else(invalid)?,
        limits,
    )?;
    if !manifest.root_element().has_tag_name((M, "manifest")) {
        return Err(invalid());
    }
    for n in manifest.root_element().children().filter(Node::is_element) {
        if !n.has_tag_name((M, "file-entry")) || n.children().any(|c| c.is_element()) {
            return Err(unsupported());
        }
        let path = n.attribute((M, "full-path")).ok_or_else(invalid)?;
        if path == "/" {
            if n.attribute((M, "media-type")) != Some(Format::Ods.mime()) {
                return Err(unsupported());
            }
        } else if !parts.contains_key(path) {
            return Err(unsupported());
        }
    }
    let doc = package::xml(parts.get("content.xml").ok_or_else(invalid)?, limits)?;
    if !doc.root_element().has_tag_name((O, "document-content")) {
        return Err(unsupported());
    }
    let mut spreadsheets = doc
        .descendants()
        .filter(|n| n.has_tag_name((O, "spreadsheet")));
    let spreadsheet = spreadsheets.next().ok_or_else(invalid)?;
    if spreadsheets.next().is_some() {
        return Err(invalid());
    }
    let mut sheets = Vec::new();
    let mut used_cells = 0usize;
    let mut used_text = 0usize;
    for table in spreadsheet.children().filter(Node::is_element) {
        if !table.has_tag_name((T, "table")) {
            return Err(unsupported());
        }
        if sheets.len() == limits.max_sheets {
            return Err(exceeded());
        }
        let name = table.attribute((T, "name")).ok_or_else(invalid)?.to_owned();
        let mut cells = Vec::new();
        let mut row_index = 0usize;
        for row in table.children().filter(Node::is_element) {
            if row.has_tag_name((T, "table-column")) {
                count(row, "number-columns-repeated", limits.max_csv_columns)?;
                continue;
            }
            if !row.has_tag_name((T, "table-row")) {
                return Err(unsupported());
            }
            let repeat_rows = count(row, "number-rows-repeated", limits.max_csv_rows)?;
            if row_index + repeat_rows > limits.max_csv_rows {
                return Err(exceeded());
            }
            let mut column = 0usize;
            for cell in row.children().filter(Node::is_element) {
                if !cell.has_tag_name((T, "table-cell"))
                    || cell.attribute((T, "number-columns-spanned")).is_some()
                    || cell.attribute((T, "number-rows-spanned")).is_some()
                {
                    return Err(unsupported());
                }
                let repeat_columns =
                    count(cell, "number-columns-repeated", limits.max_csv_columns)?;
                if column + repeat_columns > limits.max_csv_columns {
                    return Err(exceeded());
                }
                let expanded = repeat_rows
                    .checked_mul(repeat_columns)
                    .ok_or_else(exceeded)?;
                used_cells = used_cells.checked_add(expanded).ok_or_else(exceeded)?;
                if used_cells > limits.max_cells {
                    return Err(exceeded());
                }
                let value = value(cell, limits)?;
                used_text = used_text
                    .checked_add(
                        value
                            .display()
                            .len()
                            .checked_mul(expanded)
                            .ok_or_else(exceeded)?,
                    )
                    .ok_or_else(exceeded)?;
                if used_text > limits.max_workbook_text_bytes {
                    return Err(exceeded());
                }
                for r in row_index..row_index + repeat_rows {
                    for c in column..column + repeat_columns {
                        cells.push(Cell {
                            row: (r + 1) as u32,
                            column: (c + 1) as u16,
                            value: value.clone(),
                            number_format: None,
                        });
                    }
                }
                column += repeat_columns;
            }
            row_index += repeat_rows;
        }
        sheets.push(Sheet {
            name,
            visibility: SheetVisibility::Visible,
            cells,
        });
    }
    let book = Workbook {
        date_1904: false,
        sheets,
    };
    validate_workbook(&book, limits)?;
    Ok(book)
}
fn count(node: Node<'_, '_>, name: &str, max: usize) -> Result<usize> {
    let n = node
        .attribute((T, name))
        .unwrap_or("1")
        .parse::<usize>()
        .map_err(|_| invalid())?;
    if n == 0 {
        return Err(invalid());
    }
    if n > max {
        return Err(exceeded());
    }
    Ok(n)
}
fn value(node: Node<'_, '_>, limits: &Limits) -> Result<CellValue> {
    let mut text = String::new();
    for (i, p) in node.children().filter(Node::is_element).enumerate() {
        if !p.has_tag_name((X, "p")) {
            return Err(unsupported());
        }
        if i > 0 {
            text.push('\n');
        }
        text_content(p, &mut text, limits)?;
    }
    let value = match node.attribute((O, "value-type")) {
        None if text.is_empty() => CellValue::Blank,
        Some("string") => CellValue::Text(
            node.attribute((O, "string-value"))
                .map_or(text.clone(), str::to_owned),
        ),
        Some("float") => {
            CellValue::Number(node.attribute((O, "value")).ok_or_else(invalid)?.to_owned())
        }
        Some("boolean") => CellValue::Boolean(match node.attribute((O, "boolean-value")) {
            Some("true") => true,
            Some("false") => false,
            _ => return Err(invalid()),
        }),
        // Dates/durations/currency are not flattened into unlabeled text.
        _ => return Err(unsupported()),
    };
    if let Some(raw) = node.attribute((O, "string-value")) {
        if !text.is_empty() && text != raw {
            return Err(unsupported());
        }
    }
    validate_value(&value, limits)?;
    Ok(value)
}
fn text_content(node: Node<'_, '_>, out: &mut String, limits: &Limits) -> Result<()> {
    for child in node.children() {
        if child.is_text() {
            out.push_str(child.text().unwrap_or_default());
        } else if child.is_element() {
            match (child.tag_name().namespace(), child.tag_name().name()) {
                (Some(X), "span") => text_content(child, out, limits)?,
                (Some(X), "s") => {
                    let n = child
                        .attribute((X, "c"))
                        .unwrap_or("1")
                        .parse::<usize>()
                        .map_err(|_| invalid())?;
                    if n == 0
                        || n > limits.max_csv_field_bytes
                        || out.len() + n > limits.max_csv_field_bytes
                    {
                        return Err(exceeded());
                    }
                    out.extend(std::iter::repeat_n(' ', n));
                }
                (Some(X), "tab") => out.push('\t'),
                (Some(X), "line-break") => out.push('\n'),
                _ => return Err(unsupported()),
            }
        }
        if out.len() > limits.max_csv_field_bytes {
            return Err(exceeded());
        }
    }
    Ok(())
}
