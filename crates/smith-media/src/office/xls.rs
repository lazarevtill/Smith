//! Legacy BIFF8 admission. The CFB reader below bounds chains and advertised
//! lengths, then repackages only the passive Workbook stream before Calamine.
//! No untrusted CFB allocation counts or sector pointers reach that dependency.
use super::*;
use calamine::{Data, Reader};
use std::{
    collections::{BTreeMap, HashSet},
    io::Cursor,
};
const END: u32 = 0xffff_fffe;
const FREE: u32 = 0xffff_ffff;
fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2).ok_or_else(invalid)?.try_into().unwrap(),
    ))
}
fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4).ok_or_else(invalid)?.try_into().unwrap(),
    ))
}
fn sector(b: &[u8], size: usize, id: u32) -> Result<&[u8]> {
    let at = (id as usize)
        .checked_add(1)
        .and_then(|n| n.checked_mul(size))
        .ok_or_else(invalid)?;
    b.get(at..at + size).ok_or_else(invalid)
}
fn chain(
    data: &[u8],
    size: usize,
    start: u32,
    fat: &[u32],
    limit: usize,
    header: bool,
) -> Result<Vec<u8>> {
    let mut seen = HashSet::new();
    let mut id = start;
    let mut out = Vec::new();
    while id != END {
        if !seen.insert(id) || out.len().saturating_add(size) > limit {
            return Err(exceeded());
        }
        let part = if header {
            sector(data, size, id)?
        } else {
            let at = (id as usize).checked_mul(size).ok_or_else(invalid)?;
            data.get(at..at + size).ok_or_else(invalid)?
        };
        out.extend_from_slice(part);
        id = *fat.get(id as usize).ok_or_else(invalid)?;
    }
    Ok(out)
}
fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}
fn unicode_string(bytes: &[u8], wide: bool, limits: &Limits) -> Result<String> {
    let text = if wide {
        if bytes.len() % 2 != 0 {
            return Err(invalid());
        }
        String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| invalid())?
    } else {
        bytes.iter().map(|b| char::from(*b)).collect()
    };
    validate_text(&text, limits)?;
    Ok(text)
}
fn workbook_stream(bytes: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    if bytes.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    if !bytes.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") || bytes.len() < 512 {
        return Err(unsupported());
    }
    let size = match (u16_at(bytes, 26)?, u16_at(bytes, 30)?) {
        (3, 9) => 512,
        (4, 12) => 4096,
        _ => return Err(unsupported()),
    };
    if u16_at(bytes, 28)? != 0xfffe
        || u16_at(bytes, 32)? != 6
        || u32_at(bytes, 56)? != 4096
        || bytes.len() % size != 0
    {
        return Err(invalid());
    }
    let sectors = bytes.len() / size - 1;
    let fat_count = u32_at(bytes, 44)? as usize;
    if fat_count == 0
        || fat_count > sectors
        || u32_at(bytes, 40)? as usize > sectors
        || u32_at(bytes, 64)? as usize > sectors
        || u32_at(bytes, 72)? as usize > sectors
    {
        return Err(exceeded());
    }
    let mut difat = words(&bytes[76..512]);
    let mut next = u32_at(bytes, 68)?;
    let mut seen = HashSet::new();
    while next != END && next != FREE {
        if !seen.insert(next) || seen.len() > sectors {
            return Err(invalid());
        }
        let block = words(sector(bytes, size, next)?);
        next = *block.last().ok_or_else(invalid)?;
        difat.extend_from_slice(&block[..block.len() - 1]);
    }
    if seen.len() != u32_at(bytes, 72)? as usize {
        return Err(invalid());
    }
    let fat_ids: Vec<_> = difat.into_iter().filter(|n| *n != FREE).collect();
    if fat_ids.len() != fat_count {
        return Err(invalid());
    }
    let mut fat = Vec::new();
    let mut unique = HashSet::new();
    for id in fat_ids {
        if !unique.insert(id) {
            return Err(invalid());
        }
        fat.extend(words(sector(bytes, size, id)?));
    }
    let directory = chain(
        bytes,
        size,
        u32_at(bytes, 48)?,
        &fat,
        limits.max_input_bytes,
        true,
    )?;
    if directory.len() / 128 > 4096 {
        return Err(exceeded());
    }
    let mut entries = Vec::new();
    for entry in directory.chunks_exact(128) {
        if entry[66] == 0 {
            continue;
        }
        let len = usize::from(u16_at(entry, 64)?);
        if !(2..=64).contains(&len) || len % 2 != 0 || u16_at(entry, len - 2)? != 0 {
            return Err(invalid());
        }
        let name = String::from_utf16(
            &entry[..len - 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| invalid())?;
        let length = u64::from_le_bytes(entry[120..128].try_into().unwrap());
        if length > limits.max_input_bytes as u64 {
            return Err(exceeded());
        }
        entries.push((name, entry[66], u32_at(entry, 116)?, length as usize));
    }
    let root = entries.first().ok_or_else(invalid)?;
    if root.1 != 5 {
        return Err(invalid());
    }
    if entries.iter().skip(1).any(|(name, kind, _, _)| {
        *kind != 2
            || ![
                "Workbook",
                "Book",
                "\u{5}SummaryInformation",
                "\u{5}DocumentSummaryInformation",
            ]
            .contains(&name.as_str())
    }) {
        return Err(unsupported());
    }
    let mut workbooks = entries
        .iter()
        .skip(1)
        .filter(|e| e.0 == "Workbook" || e.0 == "Book");
    let workbook = workbooks.next().ok_or_else(invalid)?;
    if workbooks.next().is_some() {
        return Err(invalid());
    }
    let mut stream = if workbook.3 < 4096 {
        let mut mini = chain(bytes, size, root.2, &fat, limits.max_input_bytes, true)?;
        if root.3 > mini.len() {
            return Err(invalid());
        }
        mini.truncate(root.3);
        let mini_fat = words(&chain(
            bytes,
            size,
            u32_at(bytes, 60)?,
            &fat,
            limits.max_input_bytes,
            true,
        )?);
        chain(
            &mini,
            64,
            workbook.2,
            &mini_fat,
            limits.max_input_bytes,
            false,
        )?
    } else {
        chain(bytes, size, workbook.2, &fat, limits.max_input_bytes, true)?
    };
    if workbook.3 > stream.len() {
        return Err(invalid());
    }
    stream.truncate(workbook.3);
    Ok(stream)
}
#[derive(Default)]
struct Preflight {
    date_1904: bool,
    formats: Vec<u16>,
    format_flags: Vec<(bool, u16, bool)>,
    codes: BTreeMap<u16, String>,
    cells: BTreeMap<(usize, u32, u16), u16>,
}
fn preflight(stream: &[u8], limits: &Limits) -> Result<Preflight> {
    let mut result = Preflight::default();
    let mut at = 0;
    let mut sheets = 0;
    let mut sheet = None;
    let mut cell_count = 0;
    let mut record_count = 0;
    let mut sheet_offsets = BTreeMap::new();
    let mut declared_sheets = 0;
    let mut dense = BTreeMap::<usize, (u32, u16)>::new();
    while at < stream.len() {
        if stream[at..].iter().all(|b| *b == 0) {
            break;
        }
        let record_at = at;
        let kind = u16_at(stream, at)?;
        let len = usize::from(u16_at(stream, at + 2)?);
        at += 4;
        let record = stream.get(at..at + len).ok_or_else(invalid)?;
        at += len;
        record_count += 1;
        if record_count > limits.max_xml_nodes as usize {
            return Err(exceeded());
        }
        match kind {
            0x0809 => {
                if len < 8 || u16_at(record, 0)? != 0x0600 {
                    return Err(unsupported());
                }
                match u16_at(record, 2)? {
                    0x0005 if record_at == 0 => (),
                    0x0010 => {
                        sheet = Some(sheet_offsets.remove(&record_at).ok_or_else(invalid)?);
                        sheets += 1;
                        if sheets > limits.max_sheets {
                            return Err(exceeded());
                        }
                    }
                    _ => return Err(unsupported()),
                }
            }
            0x0085 => {
                if len < 8 || record[5] != 0 || record[4] > 2 {
                    return Err(unsupported());
                }
                if record[7] > 1 {
                    return Err(unsupported());
                }
                let name = unicode_string(
                    record
                        .get(8..8 + usize::from(record[6]) * (usize::from(record[7]) + 1))
                        .ok_or_else(invalid)?,
                    record[7] == 1,
                    limits,
                )?;
                validate_sheet_name(&name)?;
                let pos = u32_at(record, 0)? as usize;
                if pos >= stream.len() || sheet_offsets.insert(pos, declared_sheets).is_some() {
                    return Err(invalid());
                }
                if sheet_offsets.len() > limits.max_sheets {
                    return Err(exceeded());
                }
                declared_sheets += 1;
            }
            0x0200 => {
                if len != 14 {
                    return Err(invalid());
                }
                let rows = u32_at(record, 4)? as usize;
                let cols = usize::from(u16_at(record, 10)?);
                if u32_at(record, 0)? as usize > rows || usize::from(u16_at(record, 8)?) > cols {
                    return Err(invalid());
                }
                if rows > limits.max_csv_rows
                    || cols > limits.max_csv_columns
                    || rows.saturating_mul(cols) > limits.max_cells
                {
                    return Err(exceeded());
                }
            }
            0x0203 | 0x0204 | 0x0205 | 0x027e | 0x00fd | 0x0201 => {
                let r = u32::from(u16_at(record, 0)?) + 1;
                let c = u16_at(record, 2)?.checked_add(1).ok_or_else(exceeded)?;
                validate_coordinate(r, c, limits)?;
                let s = sheet.ok_or_else(invalid)?;
                if result.cells.insert((s, r, c), u16_at(record, 4)?).is_some() {
                    return Err(invalid());
                }
                cell_count += 1;
                if cell_count > limits.max_cells {
                    return Err(exceeded());
                }
                let extent = dense.entry(s).or_default();
                extent.0 = extent.0.max(r);
                extent.1 = extent.1.max(c);
                if (extent.0 as usize).saturating_mul(usize::from(extent.1)) > limits.max_cells {
                    return Err(exceeded());
                }
                let minimum = match kind {
                    0x0203 => 14,
                    0x0204 => 9,
                    0x0205 => 8,
                    0x027e | 0x00fd => 10,
                    _ => 6,
                };
                if len < minimum {
                    return Err(invalid());
                }
                if kind == 0x0204 {
                    let n = usize::from(u16_at(record, 6)?);
                    let flags = record[8];
                    if flags > 1 {
                        return Err(unsupported());
                    }
                    unicode_string(
                        record
                            .get(9..9 + n * (usize::from(flags) + 1))
                            .ok_or_else(invalid)?,
                        flags == 1,
                        limits,
                    )?;
                }
            }
            // Multi-cell and continuation records need a separate preflight;
            // reject instead of allowing implicit unbounded ranges or rich SST.
            0x00fc => {
                if len < 8 {
                    return Err(invalid());
                }
                let declared = u32_at(record, 4)? as usize;
                if declared > limits.max_shared_strings {
                    return Err(exceeded());
                }
                let mut pos = 8;
                let mut strings = 0;
                let mut text = 0;
                while pos < len {
                    let n = usize::from(u16_at(record, pos)?);
                    let flags = *record.get(pos + 2).ok_or_else(invalid)?;
                    if flags > 1 {
                        return Err(unsupported());
                    }
                    pos += 3;
                    let size = n * (usize::from(flags) + 1);
                    if record.get(pos..pos + size).is_none() {
                        return Err(invalid());
                    }
                    unicode_string(&record[pos..pos + size], flags == 1, limits)?;
                    pos += size;
                    strings += 1;
                    text += n * 3;
                    if strings > limits.max_shared_strings
                        || text > limits.max_workbook_text_bytes
                        || n > 32767
                    {
                        return Err(exceeded());
                    }
                }
                if strings != declared {
                    return Err(invalid());
                }
            }
            0x00e0 => {
                if len < 20 || result.formats.len() >= 4096 {
                    return Err(exceeded());
                }
                let flags = u16_at(record, 4)?;
                if flags & 8 != 0 {
                    return Err(unsupported());
                }
                result
                    .format_flags
                    .push((flags & 4 != 0, flags >> 4, record[9] & 4 != 0));
                result.formats.push(u16_at(record, 2)?);
            }
            0x041e => {
                if len < 5 {
                    return Err(invalid());
                }
                let id = u16_at(record, 0)?;
                let n = usize::from(u16_at(record, 2)?);
                if n > 256 {
                    return Err(exceeded());
                }
                let wide = record[4];
                let code = match wide {
                    0 => record
                        .get(5..5 + n)
                        .ok_or_else(invalid)?
                        .iter()
                        .map(|b| char::from(*b))
                        .collect(),
                    1 => String::from_utf16(
                        &record
                            .get(5..5 + n * 2)
                            .ok_or_else(invalid)?
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
                            .collect::<Vec<_>>(),
                    )
                    .map_err(|_| invalid())?,
                    _ => return Err(unsupported()),
                };
                if result.codes.insert(id, code).is_some() {
                    return Err(invalid());
                }
            }
            0x0022 => {
                result.date_1904 = match u16_at(record, 0)? {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid()),
                };
            }
            0x000a => {
                sheet = None;
            }
            // Passive presentation/settings records. They cannot allocate from
            // advertised dimensions or create formulas/external relationships.
            0x0042 if len >= 2 && u16_at(record, 0)? == 1200 => (),
            0x0031 | 0x003d | 0x0040 | 0x0041 | 0x0080 | 0x0081 | 0x0082 | 0x0083 | 0x0084
            | 0x008c | 0x0092 | 0x00a1 | 0x00c1 | 0x00da | 0x00e1 | 0x00e2 | 0x00ff | 0x013d
            | 0x0160 | 0x0161 | 0x01af | 0x01b7 | 0x01bc | 0x01c0 | 0x01c1 | 0x0208 | 0x023e
            | 0x0293 | 0x0055 | 0x005c | 0x005f | 0x000d | 0x000c | 0x000f | 0x0011 | 0x0010
            | 0x0012 | 0x0013 | 0x0014 | 0x0015 | 0x0019 | 0x002a | 0x002b | 0x0026 | 0x0027
            | 0x0028 | 0x0029 => (),
            _ => return Err(unsupported()),
        }
    }
    if sheets == 0 || !sheet_offsets.is_empty() {
        return Err(invalid());
    }
    for xf in result.cells.values() {
        let index = usize::from(*xf);
        let (style, parent, explicit) = *result.format_flags.get(index).ok_or_else(invalid)?;
        if style {
            return Err(unsupported());
        }
        if !explicit && result.formats.get(usize::from(parent)) != result.formats.get(index) {
            return Err(unsupported());
        }
    }
    Ok(result)
}
pub(super) fn read(bytes: &[u8], limits: &Limits) -> Result<Workbook> {
    limits.validate()?;
    let stream = workbook_stream(bytes, limits)?;
    let metadata = preflight(&stream, limits)?;
    let safe = container(&stream)?;
    // Defensive panic containment for malformed legacy strings after allocation
    // preflight; this is not used as a resource boundary.
    let mut reader = std::panic::catch_unwind(|| calamine::Xls::new(Cursor::new(safe)))
        .map_err(|_| invalid())?
        .map_err(|_| invalid())?;
    let mut sheets = Vec::new();
    for (index, info) in reader.sheets_metadata().to_vec().into_iter().enumerate() {
        let range = reader.worksheet_range(&info.name).map_err(|_| invalid())?;
        let mut cells = Vec::new();
        for ((_, row, column), xf) in metadata.cells.iter().filter(|((s, _, _), _)| *s == index) {
            let row = *row;
            let column = *column;
            let value = range
                .get_value((row - 1, u32::from(column) - 1))
                .unwrap_or(&Data::Empty);
            let value = match value {
                Data::String(s) => CellValue::Text(s.clone()),
                Data::Float(f) => CellValue::Number(f.to_string()),
                Data::Int(n) => CellValue::Number(n.to_string()),
                Data::Bool(b) => CellValue::Boolean(*b),
                Data::Error(e) => CellValue::Error(e.to_string()),
                Data::DateTime(d) => CellValue::Number(d.as_f64().to_string()),
                Data::Empty => CellValue::Blank,
                _ => return Err(unsupported()),
            };
            let id = *metadata.formats.get(usize::from(*xf)).ok_or_else(invalid)?;
            let number_format = if id == 0 {
                None
            } else {
                Some(NumberFormat {
                    id: u32::from(id),
                    code: metadata.codes.get(&id).cloned(),
                })
            };
            cells.push(Cell {
                row,
                column,
                value,
                number_format,
            });
        }
        sheets.push(Sheet {
            name: info.name,
            visibility: match info.visible {
                calamine::SheetVisible::Visible => SheetVisibility::Visible,
                calamine::SheetVisible::Hidden => SheetVisibility::Hidden,
                calamine::SheetVisible::VeryHidden => SheetVisibility::VeryHidden,
            },
            cells,
        });
    }
    let book = Workbook {
        date_1904: metadata.date_1904,
        sheets,
    };
    validate_workbook(&book, limits)?;
    Ok(book)
}
/// Canonical CFB3, regular-sector Workbook, fixed bounded headers. Parser sees
/// none of the source directory/miniFAT/DIFAT structures.
fn container(stream: &[u8]) -> Result<Vec<u8>> {
    let count = stream.len().max(4096).div_ceil(512);
    let fats = (count + 1).div_ceil(127);
    if fats > 109 {
        return Err(exceeded());
    }
    let total = count + 1 + fats;
    let mut out = vec![0u8; (total + 1) * 512];
    out[..8].copy_from_slice(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1");
    for (at, v) in [(24, 0x003e_u16), (26, 3), (28, 0xfffe), (30, 9), (32, 6)] {
        out[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    for (at, v) in [
        (44, fats as u32),
        (48, count as u32),
        (56, 4096),
        (60, END),
        (68, END),
    ] {
        out[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    for i in 0..109 {
        let v = if i < fats {
            (count + 1 + i) as u32
        } else {
            FREE
        };
        out[76 + i * 4..80 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    out[512..512 + stream.len()].copy_from_slice(stream);
    for (i, name, kind, start, len) in [
        (0, "Root Entry", 5, END, 0),
        (1, "Workbook", 2, 0, count * 512),
    ] {
        let at = (count + 1) * 512 + i * 128;
        let chars: Vec<_> = name.encode_utf16().chain([0]).collect();
        for (n, c) in chars.iter().enumerate() {
            out[at + n * 2..at + n * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        out[at + 64..at + 66].copy_from_slice(&((chars.len() * 2) as u16).to_le_bytes());
        out[at + 66] = kind;
        out[at + 67] = 1;
        for off in [68, 72, 76] {
            out[at + off..at + off + 4].copy_from_slice(&FREE.to_le_bytes());
        }
        if i == 0 {
            out[at + 76..at + 80].copy_from_slice(&1u32.to_le_bytes());
        }
        out[at + 116..at + 120].copy_from_slice(&start.to_le_bytes());
        out[at + 120..at + 128].copy_from_slice(&(len as u64).to_le_bytes());
    }
    for n in 0..fats * 128 {
        let v = if n < count - 1 {
            (n + 1) as u32
        } else if n <= count {
            END
        } else if n < total {
            0xffff_fffd
        } else {
            FREE
        };
        let at = (count + 2) * 512 + n * 4;
        out[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}
