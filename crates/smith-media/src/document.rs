use crate::{error, forbidden_char, Content, ErrorKind, Format, Limits, Result};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::{collections::HashSet, fmt};

pub(crate) fn inspect(format: Format, bytes: &[u8], limits: &Limits) -> Result<Content> {
    if bytes.len() > limits.max_text_bytes {
        return Err(error(
            ErrorKind::LimitExceeded,
            "document exceeds byte limit",
        ));
    }
    // Do not turn recognizable containers into apparent text by changing their suffix.
    if [
        b"%PDF-".as_slice(),
        b"PK\x03\x04",
        b"GIF87a",
        b"GIF89a",
        b"RIFF",
        b"\x89PNG",
        b"\xff\xd8\xff",
        b"\xd0\xcf\x11\xe0",
        b"\x7fELF",
    ]
    .iter()
    .any(|sig| bytes.starts_with(sig))
    {
        return Err(error(
            ErrorKind::InvalidContent,
            "binary signature in document input",
        ));
    }
    let utf8 = std::str::from_utf8(bytes)
        .map_err(|_| error(ErrorKind::InvalidContent, "document is not valid UTF-8"))?;
    let text = utf8.strip_prefix('\u{feff}').unwrap_or(utf8);
    if text.chars().any(forbidden_char) {
        return Err(error(
            ErrorKind::InvalidContent,
            "document contains forbidden control characters",
        ));
    }
    let (rendered, rows, columns) = match format {
        Format::Text | Format::Markdown => (text.to_owned(), None, None),
        Format::Json => {
            let mut budget = JsonBudget { limits, nodes: 0 };
            let mut de = serde_json::Deserializer::from_str(text);
            JsonGuard {
                budget: &mut budget,
                depth: 0,
            }
            .deserialize(&mut de)
            .and_then(|_| de.end())
            .map_err(|e| {
                // These two messages are emitted exclusively by our visitor below.
                let kind = if e.to_string().starts_with("JSON resource limit") {
                    ErrorKind::LimitExceeded
                } else {
                    ErrorKind::InvalidContent
                };
                error(
                    kind,
                    "JSON failed syntax, duplicate-key, control or resource validation",
                )
            })?;
            // Preserve original number lexemes, whitespace, and object keys. Deserializing
            // to Value could round large numbers or reinterpret reserved number-map keys.
            (text.to_owned(), None, None)
        }
        Format::Csv | Format::Tsv => {
            let cells = delimited(
                text,
                if format == Format::Csv { b',' } else { b'\t' },
                limits,
            )?;
            let count = cells.len();
            let columns = cells.first().map_or(0, Vec::len);
            let rendered = serde_json::to_string(&cells)
                .map_err(|_| error(ErrorKind::InvalidContent, "table rendering failed"))?;
            (rendered, Some(count), Some(columns))
        }
        _ => unreachable!("raster formats use the image parser"),
    };
    let total_context_chars = rendered.chars().count();
    let truncated = total_context_chars > limits.max_context_chars;
    Ok(Content::Document {
        context: rendered.chars().take(limits.max_context_chars).collect(),
        truncated,
        total_context_chars,
        rows,
        columns,
    })
}

/// Strict CSV/TSV: quote only at a field start, escaped quote is doubled, CRLF or LF.
/// Quoted newlines retain their original spelling. Every record has the same width.
fn delimited(text: &str, delimiter: u8, limits: &Limits) -> Result<Vec<Vec<String>>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let input = text.as_bytes();
    let mut i = 0;
    let mut rows = Vec::new();
    loop {
        if rows.len() >= limits.max_csv_rows {
            return Err(error(ErrorKind::LimitExceeded, "table exceeds row limit"));
        }
        let mut row = Vec::new();
        loop {
            if row.len() >= limits.max_csv_columns {
                return Err(error(
                    ErrorKind::LimitExceeded,
                    "table exceeds column limit",
                ));
            }
            let mut cell = Vec::new();
            if input.get(i) == Some(&b'"') {
                i += 1;
                loop {
                    match input.get(i) {
                        None => {
                            return Err(error(
                                ErrorKind::InvalidContent,
                                "unterminated quoted field",
                            ))
                        }
                        Some(b'"') if input.get(i + 1) == Some(&b'"') => {
                            cell.push(b'"');
                            i += 2;
                        }
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(byte) => {
                            cell.push(*byte);
                            i += 1;
                        }
                    }
                    if cell.len() > limits.max_csv_field_bytes {
                        return Err(error(
                            ErrorKind::LimitExceeded,
                            "table field exceeds byte limit",
                        ));
                    }
                }
                if let Some(&byte) = input.get(i) {
                    if byte != delimiter && byte != b'\r' && byte != b'\n' {
                        return Err(error(
                            ErrorKind::InvalidContent,
                            "data after closing field quote",
                        ));
                    }
                }
            } else {
                while let Some(&byte) = input.get(i) {
                    if byte == delimiter || matches!(byte, b'\r' | b'\n') {
                        break;
                    }
                    if byte == b'"' {
                        return Err(error(
                            ErrorKind::InvalidContent,
                            "quote inside unquoted field",
                        ));
                    }
                    cell.push(byte);
                    i += 1;
                    if cell.len() > limits.max_csv_field_bytes {
                        return Err(error(
                            ErrorKind::LimitExceeded,
                            "table field exceeds byte limit",
                        ));
                    }
                }
            }
            row.push(String::from_utf8(cell).expect("ASCII delimiter boundaries preserve UTF-8"));
            if input.get(i) == Some(&delimiter) {
                i += 1;
                continue;
            }
            break;
        }
        if rows
            .first()
            .is_some_and(|first: &Vec<String>| first.len() != row.len())
        {
            return Err(error(
                ErrorKind::InvalidContent,
                "table records have inconsistent column counts",
            ));
        }
        rows.push(row);
        if i == input.len() {
            break;
        }
        if input[i] == b'\r' {
            i += 1;
            if input.get(i) != Some(&b'\n') {
                return Err(error(
                    ErrorKind::InvalidContent,
                    "bare carriage return outside quoted field",
                ));
            }
        }
        i += 1;
        if i == input.len() {
            break;
        }
    }
    Ok(rows)
}

struct JsonBudget<'a> {
    limits: &'a Limits,
    nodes: usize,
}
struct JsonGuard<'a, 'b> {
    budget: &'a mut JsonBudget<'b>,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonGuard<'_, '_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        self.budget.nodes += 1;
        if self.depth > self.budget.limits.max_json_depth
            || self.budget.nodes > self.budget.limits.max_json_nodes
        {
            return Err(de::Error::custom("JSON resource limit"));
        }
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonGuard<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<(), E> {
        if value.chars().any(forbidden_char) {
            Err(de::Error::custom("forbidden JSON control"))
        } else {
            Ok(())
        }
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq
            .next_element_seed(JsonGuard {
                budget: self.budget,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.chars().any(forbidden_char) || !seen.insert(key) {
                return Err(de::Error::custom("duplicate JSON key or forbidden control"));
            }
            map.next_value_seed(JsonGuard {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}
