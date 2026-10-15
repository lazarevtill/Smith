use super::{package::*, *};
use roxmltree::Node;
use std::collections::{BTreeMap, HashMap, HashSet};

const BOOK_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const SHEET_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml";

/// Read a bounded, formula-free XLSX data workbook. Macros, external links,
/// embedded objects, unsupported worksheet features and other Office formats fail.
pub fn read_workbook(bytes: &[u8], limits: &Limits) -> Result<Workbook> {
    read_xml_workbook(bytes, limits, false)
}
pub(super) fn read_xml_workbook(
    bytes: &[u8],
    limits: &Limits,
    macro_container: bool,
) -> Result<Workbook> {
    limits.validate()?;
    let parts = unzip(bytes, limits)?;
    let content_types = content_types(&parts, limits)?;
    let expected_type = if macro_container {
        "application/vnd.ms-excel.sheet.macroEnabled.main+xml"
    } else {
        BOOK_TYPE
    };
    if content_types.get("xl/workbook.xml").map(String::as_str) != Some(expected_type) {
        return Err(unsupported());
    }
    let root_rels = relationships(part(&parts, "_rels/.rels")?, "", limits)?;
    let mut office = 0;
    for relation in root_rels.values() {
        if relation.kind == format!("{REL}/officeDocument") && relation.target == "xl/workbook.xml" {
            office += 1;
        } else if !(relation.kind == format!("{REL}/extended-properties") && relation.target == "docProps/app.xml"
            || relation.kind == "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" && relation.target == "docProps/core.xml") {
            return Err(unsupported());
        }
        part(&parts, &relation.target)?;
    }
    if office != 1 {
        return Err(invalid());
    }
    let rels = relationships(
        part(&parts, "xl/_rels/workbook.xml.rels")?,
        "xl/workbook.xml",
        limits,
    )?;
    let mut approved: HashSet<String> = [
        "[Content_Types].xml",
        "_rels/.rels",
        "xl/workbook.xml",
        "xl/_rels/workbook.xml.rels",
        "docProps/core.xml",
        "docProps/app.xml",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut shared_path = None;
    let mut styles_path = None;
    for relation in rels.values() {
        let kind = relation
            .kind
            .strip_prefix(&format!("{REL}/"))
            .ok_or_else(unsupported)?;
        match kind {
            "worksheet" => {
                if !relation.target.starts_with("xl/worksheets/")
                    || content_types.get(&relation.target).map(String::as_str) != Some(SHEET_TYPE)
                {
                    return Err(unsupported());
                }
            }
            "sharedStrings" => {
                if shared_path.replace(relation.target.as_str()).is_some() {
                    return Err(invalid());
                }
            }
            "styles" => {
                if styles_path.replace(relation.target.as_str()).is_some() {
                    return Err(invalid());
                }
            }
            "theme" => (),
            _ => return Err(unsupported()),
        }
        part(&parts, &relation.target)?;
        approved.insert(relation.target.clone());
    }
    if parts.keys().any(|name| !approved.contains(name)) {
        return Err(unsupported());
    }
    let strings = shared_path
        .map(|path| shared_strings(part(&parts, path)?, limits))
        .transpose()?
        .unwrap_or_default();
    let styles = styles_path
        .map(|path| number_formats(part(&parts, path)?, limits))
        .transpose()?
        .unwrap_or_default();
    let document = xml(part(&parts, "xl/workbook.xml")?, limits)?;
    require_root(&document, MAIN, "workbook")?;
    let root = document.root_element();
    for node in root.children().filter(Node::is_element) {
        if node.tag_name().namespace() != Some(MAIN) {
            return Err(unsupported());
        }
        match node.tag_name().name() {
            "fileVersion" | "workbookPr" | "bookViews" | "sheets" | "calcPr"
            | "workbookProtection" => (),
            "definedNames" if !node.children().any(|n| n.is_element()) => (),
            _ => return Err(unsupported()),
        }
    }
    let date_1904 = child(root, "workbookPr")?
        .and_then(|n| n.attribute("date1904"))
        .map(boolean)
        .transpose()?
        .unwrap_or(false);
    let mut sheets = Vec::new();
    let mut ids = HashSet::new();
    let mut targets = HashSet::new();
    let mut remaining_cells = limits.max_cells;
    let mut remaining_text = limits.max_workbook_text_bytes;
    for node in child(root, "sheets")?
        .ok_or_else(invalid)?
        .children()
        .filter(Node::is_element)
    {
        if sheets.len() >= limits.max_sheets {
            return Err(exceeded());
        }
        if !node.has_tag_name((MAIN, "sheet")) {
            return Err(unsupported());
        }
        let id = attribute(node, "sheetId")?
            .parse::<u32>()
            .map_err(|_| invalid())?;
        if id == 0 || !ids.insert(id) {
            return Err(invalid());
        }
        let rel = node.attribute((REL, "id")).ok_or_else(invalid)?;
        let relationship = rels.get(rel).ok_or_else(invalid)?;
        if relationship.kind != format!("{REL}/worksheet") || !targets.insert(&relationship.target)
        {
            return Err(invalid());
        }
        let name = decode_excel_string(attribute(node, "name")?, limits)?;
        validate_sheet_name(&name)?;
        let visibility = match node.attribute("state").unwrap_or("visible") {
            "visible" => SheetVisibility::Visible,
            "hidden" => SheetVisibility::Hidden,
            "veryHidden" => SheetVisibility::VeryHidden,
            _ => return Err(invalid()),
        };
        let cells = worksheet(
            part(&parts, &relationship.target)?,
            &strings,
            &styles,
            limits,
            &mut remaining_cells,
            &mut remaining_text,
        )?;
        sheets.push(Sheet {
            name,
            visibility,
            cells,
        });
    }
    // An unused worksheet relationship could otherwise silently discard data.
    if rels
        .values()
        .filter(|r| r.kind == format!("{REL}/worksheet"))
        .count()
        != targets.len()
    {
        return Err(invalid());
    }
    let book = Workbook { date_1904, sheets };
    validate_workbook(&book, limits)?;
    Ok(book)
}

fn content_types(
    parts: &BTreeMap<String, String>,
    limits: &Limits,
) -> Result<BTreeMap<String, String>> {
    let document = xml(part(parts, "[Content_Types].xml")?, limits)?;
    require_root(&document, CT, "Types")?;
    let mut result = BTreeMap::new();
    let mut defaults = HashSet::new();
    for node in document.root_element().children().filter(Node::is_element) {
        match (node.tag_name().namespace(), node.tag_name().name()) {
            (Some(CT), "Default") => {
                let extension = attribute(node, "Extension")?;
                let mime = attribute(node, "ContentType")?;
                if !defaults.insert(extension) {
                    return Err(invalid());
                }
                if !matches!(
                    (extension, mime),
                    ("xml", "application/xml")
                        | (
                            "rels",
                            "application/vnd.openxmlformats-package.relationships+xml"
                        )
                ) {
                    return Err(unsupported());
                }
            }
            (Some(CT), "Override") => {
                let name = attribute(node, "PartName")?
                    .strip_prefix('/')
                    .ok_or_else(invalid)?;
                let mime = attribute(node, "ContentType")?;
                if ![
                    BOOK_TYPE,
                    "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
                    SHEET_TYPE,
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml",
                    "application/vnd.openxmlformats-officedocument.theme+xml",
                    "application/vnd.openxmlformats-package.core-properties+xml",
                    "application/vnd.openxmlformats-officedocument.extended-properties+xml",
                ]
                .contains(&mime)
                {
                    return Err(unsupported());
                }
                part(parts, name)?;
                if result.insert(name.to_owned(), mime.to_owned()).is_some() {
                    return Err(invalid());
                }
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(result)
}

fn shared_strings(text: &str, limits: &Limits) -> Result<Vec<String>> {
    let document = xml(text, limits)?;
    require_root(&document, MAIN, "sst")?;
    let mut strings = Vec::new();
    let mut bytes = 0;
    for node in document.root_element().children().filter(Node::is_element) {
        if strings.len() >= limits.max_shared_strings {
            return Err(exceeded());
        }
        if !node.has_tag_name((MAIN, "si")) {
            return Err(unsupported());
        }
        let string = rich_text(node, limits)?;
        bytes += string.len();
        if bytes > limits.max_workbook_text_bytes {
            return Err(exceeded());
        }
        strings.push(string);
    }
    Ok(strings)
}
fn rich_text(node: Node<'_, '_>, limits: &Limits) -> Result<String> {
    let mut decoded = String::new();
    for child in node.children().filter(Node::is_element) {
        if child.has_tag_name((MAIN, "t")) {
            decoded.push_str(&decode_excel_string(simple_text(child)?, limits)?);
        } else if child.has_tag_name((MAIN, "r")) {
            for part in child.children().filter(Node::is_element) {
                if part.has_tag_name((MAIN, "t")) {
                    decoded.push_str(&decode_excel_string(simple_text(part)?, limits)?);
                } else if !part.has_tag_name((MAIN, "rPr")) {
                    return Err(unsupported());
                }
            }
        } else {
            return Err(unsupported());
        }
        if decoded.len() > limits.max_csv_field_bytes {
            return Err(exceeded());
        }
    }
    validate_text(&decoded, limits)?;
    Ok(decoded)
}
fn simple_text<'a>(node: Node<'a, '_>) -> Result<&'a str> {
    if node.children().any(|n| n.is_element()) {
        return Err(invalid());
    }
    // Adjacent text/CDATA nodes must not silently lose later content.
    if node.children().filter(Node::is_text).count() > 1 {
        return Err(unsupported());
    }
    Ok(node.text().unwrap_or(""))
}
fn number_formats(text: &str, limits: &Limits) -> Result<Vec<Option<NumberFormat>>> {
    let doc = xml(text, limits)?;
    require_root(&doc, MAIN, "styleSheet")?;
    let mut custom = HashMap::new();
    if let Some(node) = child(doc.root_element(), "numFmts")? {
        for format in node.children().filter(Node::is_element) {
            if custom.len() >= 128 {
                return Err(exceeded());
            }
            if !format.has_tag_name((MAIN, "numFmt")) {
                return Err(unsupported());
            }
            let id: u32 = attribute(format, "numFmtId")?
                .parse()
                .map_err(|_| invalid())?;
            let code = decode_excel_string(attribute(format, "formatCode")?, limits)?;
            if !(164..=65535).contains(&id) || code.len() > 256 || custom.insert(id, code).is_some()
            {
                return Err(invalid());
            }
        }
    }
    // The model records effective cell number formats, not named-style inheritance.
    // Refuse non-General base styles instead of silently changing their meaning.
    if let Some(base) = child(doc.root_element(), "cellStyleXfs")? {
        for format in base.children().filter(Node::is_element) {
            if !format.has_tag_name((MAIN, "xf"))
                || format.attribute("numFmtId").unwrap_or("0") != "0"
            {
                return Err(unsupported());
            }
        }
    }
    let mut formats = Vec::new();
    if let Some(node) = child(doc.root_element(), "cellXfs")? {
        for format in node.children().filter(Node::is_element) {
            if formats.len() >= 128 {
                return Err(exceeded());
            }
            if !format.has_tag_name((MAIN, "xf")) {
                return Err(unsupported());
            }
            if format.attribute("xfId").unwrap_or("0") != "0"
                || format
                    .attribute("applyNumberFormat")
                    .map(boolean)
                    .transpose()?
                    == Some(false)
            {
                return Err(unsupported());
            }
            let id: u32 = format
                .attribute("numFmtId")
                .unwrap_or("0")
                .parse()
                .map_err(|_| invalid())?;
            let code = custom.get(&id).cloned();
            if id > 65535 || id >= 164 && code.is_none() {
                return Err(unsupported());
            }
            formats.push(if id == 0 {
                None
            } else {
                Some(NumberFormat { id, code })
            });
        }
    }
    Ok(formats)
}
fn style(index: Option<&str>, formats: &[Option<NumberFormat>]) -> Result<Option<NumberFormat>> {
    let index: usize = index.unwrap_or("0").parse().map_err(|_| invalid())?;
    if formats.is_empty() && index == 0 {
        return Ok(None);
    }
    formats.get(index).cloned().ok_or_else(invalid)
}

fn worksheet(
    text: &str,
    strings: &[String],
    styles: &[Option<NumberFormat>],
    limits: &Limits,
    remaining_cells: &mut usize,
    remaining_text: &mut usize,
) -> Result<Vec<Cell>> {
    let doc = xml(text, limits)?;
    require_root(&doc, MAIN, "worksheet")?;
    let root = doc.root_element();
    for node in root.children().filter(Node::is_element) {
        if node.tag_name().namespace() != Some(MAIN)
            || ![
                "sheetPr",
                "dimension",
                "sheetViews",
                "sheetFormatPr",
                "cols",
                "sheetData",
                "printOptions",
                "pageMargins",
                "pageSetup",
                "sheetProtection",
            ]
            .contains(&node.tag_name().name())
        {
            return Err(unsupported());
        }
    }
    if let Some(dimension) = child(root, "dimension")? {
        let value = attribute(dimension, "ref")?;
        if let Some((first, last)) = value.split_once(':') {
            coordinate(first, limits)?;
            coordinate(last, limits)?;
        } else {
            coordinate(value, limits)?;
        }
    }
    let mut col_styles = BTreeMap::new();
    if let Some(cols) = child(root, "cols")? {
        for col in cols.children().filter(Node::is_element) {
            if !col.has_tag_name((MAIN, "col")) {
                return Err(unsupported());
            }
            let min: u16 = attribute(col, "min")?.parse().map_err(|_| invalid())?;
            let max: u16 = attribute(col, "max")?.parse().map_err(|_| invalid())?;
            validate_coordinate(1, min, limits)?;
            validate_coordinate(1, max, limits)?;
            if min > max {
                return Err(invalid());
            }
            if let Some(index) = col.attribute("style") {
                let format = style(Some(index), styles)?;
                for column in min..=max {
                    if col_styles.insert(column, format.clone()).is_some() {
                        return Err(invalid());
                    }
                }
            }
        }
    }
    let mut cells = Vec::new();
    let mut rows = HashSet::new();
    for row in child(root, "sheetData")?
        .ok_or_else(invalid)?
        .children()
        .filter(Node::is_element)
    {
        if !row.has_tag_name((MAIN, "row")) {
            return Err(unsupported());
        }
        let row_index: u32 = attribute(row, "r")?.parse().map_err(|_| invalid())?;
        validate_coordinate(row_index, 1, limits)?;
        if !rows.insert(row_index) {
            return Err(invalid());
        }
        let row_style = row
            .attribute("s")
            .map(|s| style(Some(s), styles))
            .transpose()?;
        if row_style.is_some()
            && row.attribute("customFormat").map(boolean).transpose()? != Some(true)
        {
            return Err(unsupported());
        }
        for cell in row.children().filter(Node::is_element) {
            if !cell.has_tag_name((MAIN, "c")) {
                return Err(unsupported());
            }
            if *remaining_cells == 0 {
                return Err(exceeded());
            }
            *remaining_cells -= 1;
            let (row_num, column) = coordinate(attribute(cell, "r")?, limits)?;
            if row_num != row_index {
                return Err(invalid());
            }
            for item in cell.children().filter(Node::is_element) {
                if !item.has_tag_name((MAIN, "v")) && !item.has_tag_name((MAIN, "is")) {
                    return Err(unsupported());
                }
            }
            let v = child(cell, "v")?.map(simple_text).transpose()?;
            let inline = child(cell, "is")?;
            if v.is_some() && inline.is_some() {
                return Err(invalid());
            }
            let value = match cell.attribute("t").unwrap_or("n") {
                "inlineStr" if v.is_none() => {
                    CellValue::Text(rich_text(inline.ok_or_else(invalid)?, limits)?)
                }
                "s" if inline.is_none() => {
                    let index: usize = v.ok_or_else(invalid)?.parse().map_err(|_| invalid())?;
                    CellValue::Text(strings.get(index).ok_or_else(invalid)?.clone())
                }
                "str" if inline.is_none() => {
                    CellValue::Text(decode_excel_string(v.unwrap_or(""), limits)?)
                }
                "n" if inline.is_none() => match v.filter(|v| !v.is_empty()) {
                    Some(v) => CellValue::Number(v.to_owned()),
                    None => CellValue::Blank,
                },
                "b" if inline.is_none() => CellValue::Boolean(boolean(v.ok_or_else(invalid)?)?),
                "e" if inline.is_none() => CellValue::Error(v.ok_or_else(invalid)?.to_owned()),
                _ => return Err(unsupported()),
            };
            validate_value(&value, limits)?;
            *remaining_text = remaining_text
                .checked_sub(value.display().len())
                .ok_or_else(exceeded)?;
            let number_format = if cell.attribute("s").is_some() {
                style(cell.attribute("s"), styles)?
            } else if let Some(format) = &row_style {
                format.clone()
            } else if let Some(format) = col_styles.get(&column) {
                format.clone()
            } else {
                style(None, styles)?
            };
            cells.push(Cell {
                row: row_num,
                column,
                value,
                number_format,
            });
        }
    }
    cells.sort_by_key(|c| (c.row, c.column));
    Ok(cells)
}
fn boolean(value: &str) -> Result<bool> {
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(invalid()),
    }
}
