//! Deterministic, text-only PDF generation. This is deliberately not a general
//! PDF parser: admission accepts only byte-exact regeneration of this profile.
use super::*;
use std::collections::BTreeMap;

const FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSans-Regular.ttf");
const PREFIX: &[u8] = b"%PDF-1.7\n%SMITH-TABLE-1\n1 0 obj\n<< /Type /SmithTable /Length ";
const MAX_PAGES: usize = 64;

pub(super) fn read_generated(bytes: &[u8], limits: &Limits) -> Result<Table> {
    if bytes.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    let tail = bytes.strip_prefix(PREFIX).ok_or_else(unsupported)?;
    let end = tail.iter().position(|b| *b == b' ').ok_or_else(invalid)?;
    if end > 10 {
        return Err(invalid());
    }
    let len = std::str::from_utf8(&tail[..end])
        .map_err(|_| invalid())?
        .parse::<usize>()
        .map_err(|_| invalid())?;
    if len > limits.max_workbook_text_bytes {
        return Err(exceeded());
    }
    let payload = tail[end..]
        .strip_prefix(b" >>\nstream\n")
        .ok_or_else(invalid)?;
    let table: Table =
        serde_json::from_slice(payload.get(..len).ok_or_else(invalid)?).map_err(|_| invalid())?;
    // Prevent forged context, hidden actions, extra revisions, scripts, links,
    // arbitrary fonts and any discrepancy between metadata and visible output.
    if write(&table, limits)? != bytes {
        return Err(unsupported());
    }
    Ok(table)
}

pub(super) fn write(table: &Table, limits: &Limits) -> Result<Vec<u8>> {
    export::table_workbook(table, limits)?;
    let metadata = serde_json::to_vec(table).map_err(|_| invalid())?;
    if metadata.len() > limits.max_workbook_text_bytes {
        return Err(exceeded());
    }
    let font = ttf_parser::Face::parse(FONT, 0).map_err(|_| invalid())?;
    let units = f64::from(font.units_per_em());
    let mut glyphs = BTreeMap::new();
    let mut lines = Vec::new();
    let rows = (!table.headers.is_empty())
        .then(|| table.headers.join(" | "))
        .into_iter()
        .chain(table.rows.iter().map(|r| {
            r.iter()
                .map(CellValue::display)
                .collect::<Vec<_>>()
                .join(" | ")
        }));
    for row in rows {
        for source_line in row
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
            .split('\n')
        {
            let mut line = String::new();
            let mut width = 0.0;
            for c in source_line.chars() {
                // Latin, Greek, Cyrillic and basic punctuation. Complex shaping,
                // combining marks, bidi and missing glyphs fail visibly.
                if !matches!(c as u32, 0x20..=0x2ff | 0x370..=0x52f | 0x2000..=0x206f | 0x20ac | 0x2116)
                    || matches!(c as u32, 0x483..=0x489 | 0x200b..=0x200f | 0x2028..=0x202f | 0x2060..=0x206f)
                    || forbidden_char(c)
                {
                    return Err(error(
                        ErrorKind::UnsupportedFormat,
                        "PDF text requires unsupported shaping or glyphs",
                    ));
                }
                let id = font.glyph_index(c).ok_or_else(unsupported)?;
                let advance = font.glyph_hor_advance(id).ok_or_else(unsupported)?;
                let w = f64::from(advance) * 10.0 / units;
                if width + w > 515.0 && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    width = 0.0;
                }
                glyphs.insert(
                    c as u16,
                    (id.0, (f64::from(advance) * 1000.0 / units).round() as u32),
                );
                line.push(c);
                width += w;
                if lines.len() >= MAX_PAGES * 62 {
                    return Err(exceeded());
                }
            }
            lines.push(line);
            if lines.len() > MAX_PAGES * 62 {
                return Err(exceeded());
            }
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    let pages = lines.len().div_ceil(62);
    let mut objects: Vec<Vec<u8>> = Vec::new();
    objects.push(stream("/Type /SmithTable", &metadata)); // 1
    objects.push(b"<< /Type /Catalog /Pages 3 0 R /SmithTable 1 0 R >>".to_vec()); // 2
    let kids = (0..pages)
        .map(|i| format!("{} 0 R", 10 + i * 2))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!("<< /Type /Pages /Count {pages} /Kids [{kids}] >>").into_bytes()); // 3
    objects.push(b"<< /Type /Font /Subtype /Type0 /BaseFont /NotoSans-Regular /Encoding /Identity-H /DescendantFonts [5 0 R] /ToUnicode 9 0 R >>".to_vec());
    let widths = glyphs
        .iter()
        .map(|(c, (_, w))| format!("{c} [{w}]"))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(format!("<< /Type /Font /Subtype /CIDFontType2 /BaseFont /NotoSans-Regular /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 6 0 R /CIDToGIDMap 8 0 R /W [{widths}] >>").into_bytes());
    let bounds = font.global_bounding_box();
    let scale = |n: i16| (f64::from(n) * 1000.0 / units).round() as i32;
    objects.push(format!("<< /Type /FontDescriptor /FontName /NotoSans-Regular /Flags 32 /FontBBox [{} {} {} {}] /ItalicAngle 0 /Ascent {} /Descent {} /CapHeight {} /StemV 80 /FontFile2 7 0 R >>", scale(bounds.x_min), scale(bounds.y_min), scale(bounds.x_max), scale(bounds.y_max), scale(font.ascender()), scale(font.descender()), scale(font.capital_height().unwrap_or(font.ascender()))).into_bytes());
    objects.push(stream(&format!("/Length1 {}", FONT.len()), FONT));
    let mut map = vec![0u8; (usize::from(*glyphs.keys().next_back().unwrap_or(&0)) + 1) * 2];
    for (code, (glyph, _)) in &glyphs {
        map[usize::from(*code) * 2..usize::from(*code) * 2 + 2]
            .copy_from_slice(&glyph.to_be_bytes());
    }
    objects.push(stream("", &map));
    let mut cmap = String::from("/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /SmithUnicode def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n");
    let codes: Vec<_> = glyphs.keys().collect();
    for chunk in codes.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for c in chunk {
            cmap.push_str(&format!("<{c:04X}> <{c:04X}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    objects.push(stream("", cmap.as_bytes()));
    for (i, chunk) in lines.chunks(62).enumerate() {
        objects.push(format!("<< /Type /Page /Parent 3 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R >> >> /Contents {} 0 R >>", 11 + i * 2).into_bytes());
        let mut content = String::from("BT /F1 10 Tf 12 TL 40 800 Td\n");
        for line in chunk {
            let hex = line
                .encode_utf16()
                .map(|c| format!("{c:04X}"))
                .collect::<String>();
            content.push_str(&format!("<{hex}> Tj T*\n"));
        }
        content.push_str("ET\n");
        objects.push(stream("", content.as_bytes()));
    }
    let mut out = b"%PDF-1.7\n%SMITH-TABLE-1\n".to_vec();
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
        if out.len() > limits.max_input_bytes {
            return Err(exceeded());
        }
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
    for offset in offsets.iter().skip(1) {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 2 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .as_bytes(),
    );
    if out.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    Ok(out)
}
fn stream(fields: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {fields} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\nendstream");
    out
}
