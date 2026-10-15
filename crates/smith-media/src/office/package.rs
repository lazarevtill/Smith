use super::{exceeded, invalid, unsupported};
use crate::{Limits, Result};
use roxmltree::{Document, Node, ParsingOptions};
use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read},
};
use zip::{
    read::{ArchiveOffset, Config},
    CompressionMethod, ZipArchive,
};

pub(super) const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
pub(super) const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

pub(super) fn unzip(bytes: &[u8], limits: &Limits) -> Result<BTreeMap<String, String>> {
    unzip_inner(bytes, limits, false)
}
pub(super) fn unzip_ods(bytes: &[u8], limits: &Limits) -> Result<BTreeMap<String, String>> {
    unzip_inner(bytes, limits, true)
}
fn unzip_inner(bytes: &[u8], limits: &Limits, ods: bool) -> Result<BTreeMap<String, String>> {
    if bytes.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    if !bytes.starts_with(b"PK\x03\x04") {
        return Err(invalid());
    }
    // zip may retry earlier EOCD candidates when the last directory is invalid.
    // Bound EVERY candidate before it can allocate an index, and disallow ZIP64
    // locators (including misleading candidates inside comments/compressed data).
    for (at, marker) in bytes.windows(4).enumerate() {
        if marker == b"PK\x06\x07" || marker == b"PK\x06\x06" {
            return Err(unsupported());
        }
        if marker == b"PK\x05\x06" {
            if let Some(candidate) = bytes.get(at..at + 22) {
                if usize::from(le16(candidate, 10)) > limits.max_zip_entries {
                    return Err(exceeded());
                }
            }
        }
    }
    // Check the complete classic central directory before ZIP-library allocation.
    let end = bytes
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .ok_or_else(invalid)?;
    let eocd = bytes.get(end..end + 22).ok_or_else(invalid)?;
    let u16_at = |i| u16::from_le_bytes([eocd[i], eocd[i + 1]]);
    let u32_at = |i| u32::from_le_bytes(eocd[i..i + 4].try_into().unwrap());
    let count = usize::from(u16_at(10));
    if count > limits.max_zip_entries {
        return Err(exceeded());
    }
    if count == 0
        || u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(8) != u16_at(10)
        || u64::from(u32_at(12)) + u64::from(u32_at(16)) != end as u64
        || end + 22 + usize::from(u16_at(20)) != bytes.len()
    {
        return Err(invalid());
    }
    let central = u32_at(16) as usize;
    preflight_directory(bytes, central, end, count)?;
    let mut archive = ZipArchive::with_config(
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
        Cursor::new(bytes),
    )
    .map_err(|_| invalid())?;
    if archive.len() != count || archive.central_directory_start() != central as u64 {
        return Err(invalid());
    }
    let mut result = BTreeMap::new();
    let mut names = HashSet::new();
    let mut total = 0u64;
    for index in 0..count {
        let mut file = archive.by_index(index).map_err(|_| invalid())?;
        let name = std::str::from_utf8(file.name_raw())
            .map_err(|_| invalid())?
            .to_owned();
        if !safe_part(&name)
            || !names.insert(name.to_ascii_lowercase())
            || file.is_dir()
            || file
                .unix_mode()
                .is_some_and(|m| !matches!(m & 0o170000, 0 | 0o100000))
        {
            return Err(invalid());
        }
        if file.encrypted()
            || !matches!(
                file.compression(),
                CompressionMethod::Stored | CompressionMethod::Deflated
            )
        {
            return Err(unsupported());
        }
        if !(name.ends_with(".xml") || name.ends_with(".rels") || ods && name == "mimetype") {
            return Err(unsupported());
        }
        total = total.checked_add(file.size()).ok_or_else(exceeded)?;
        if file.size() > limits.max_zip_part_bytes as u64
            || total > limits.max_zip_total_bytes as u64
            || file.size() > file.compressed_size().max(1).saturating_mul(200)
        {
            return Err(exceeded());
        }
        let expected_size = file.size();
        let mut part = Vec::new();
        file.by_ref()
            .take(limits.max_zip_part_bytes as u64 + 1)
            .read_to_end(&mut part)
            .map_err(|_| invalid())?;
        if part.len() > limits.max_zip_part_bytes {
            return Err(exceeded());
        }
        if part.len() as u64 != expected_size {
            return Err(invalid());
        }
        let text = String::from_utf8(part).map_err(|_| invalid())?;
        // Even ignored document metadata/theme parts must be safe, bounded XML.
        if name != "mimetype" {
            xml(&text, limits)?;
        }
        result.insert(name, text);
    }
    Ok(result)
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(
        bytes[at..at + 2]
            .try_into()
            .expect("validated fixed header"),
    )
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(
        bytes[at..at + 4]
            .try_into()
            .expect("validated fixed header"),
    )
}
fn extra_fields(mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        let header = bytes.get(..4).ok_or_else(invalid)?;
        if le16(header, 0) == 1 {
            return Err(unsupported());
        }
        let length = usize::from(le16(header, 2));
        bytes = bytes.get(4 + length..).ok_or_else(invalid)?;
    }
    Ok(())
}
fn preflight_directory(bytes: &[u8], central: usize, end: usize, count: usize) -> Result<()> {
    let directory = bytes.get(central..end).ok_or_else(invalid)?;
    let mut pos = 0;
    let mut names = HashSet::new();
    let mut ranges = Vec::new();
    for _ in 0..count {
        let header = directory.get(pos..pos + 46).ok_or_else(invalid)?;
        if !header.starts_with(b"PK\x01\x02") || le16(header, 34) != 0 {
            return Err(invalid());
        }
        let flags = le16(header, 8);
        let compression = le16(header, 10);
        if flags & !0x080e != 0 || !matches!(compression, 0 | 8) {
            return Err(unsupported());
        }
        let packed = le32(header, 20) as usize;
        let unpacked = le32(header, 24);
        let local_at = le32(header, 42) as usize;
        if packed == u32::MAX as usize || unpacked == u32::MAX || local_at == u32::MAX as usize {
            return Err(unsupported());
        }
        let name_len = usize::from(le16(header, 28));
        let extra_len = usize::from(le16(header, 30));
        let comment_len = usize::from(le16(header, 32));
        let name_at = pos + 46;
        let name = directory
            .get(name_at..name_at + name_len)
            .ok_or_else(invalid)?;
        let name_text = std::str::from_utf8(name).map_err(|_| invalid())?;
        if !safe_part(name_text) || !names.insert(name_text.to_ascii_lowercase()) {
            return Err(invalid());
        }
        let extra_at = name_at + name_len;
        extra_fields(
            directory
                .get(extra_at..extra_at + extra_len)
                .ok_or_else(invalid)?,
        )?;
        pos = extra_at + extra_len + comment_len;
        if pos > directory.len() {
            return Err(invalid());
        }
        let local = bytes.get(local_at..local_at + 30).ok_or_else(invalid)?;
        if !local.starts_with(b"PK\x03\x04")
            || le16(local, 6) != flags
            || le16(local, 8) != compression
        {
            return Err(invalid());
        }
        let local_name_len = usize::from(le16(local, 26));
        let local_extra_len = usize::from(le16(local, 28));
        let local_name_at = local_at + 30;
        if bytes.get(local_name_at..local_name_at + local_name_len) != Some(name) {
            return Err(invalid());
        }
        let local_extra_at = local_name_at + local_name_len;
        let data_at = local_extra_at + local_extra_len;
        extra_fields(bytes.get(local_extra_at..data_at).ok_or_else(invalid)?)?;
        let data_end = data_at.checked_add(packed).ok_or_else(invalid)?;
        if data_end > central {
            return Err(invalid());
        }
        if flags & 8 == 0
            && (le32(local, 14) != le32(header, 16)
                || le32(local, 18) as usize != packed
                || le32(local, 22) != unpacked)
        {
            return Err(invalid());
        }
        ranges.push((local_at, data_end));
    }
    if pos != directory.len() {
        return Err(invalid());
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(invalid());
    }
    Ok(())
}

fn safe_part(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.is_ascii()
        && !name.bytes().any(|b| b.is_ascii_control())
        && !name.contains(['\\', ':', '%', '?', '#'])
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub(super) fn xml<'a>(text: &'a str, limits: &Limits) -> Result<Document<'a>> {
    if text.len() > limits.max_zip_part_bytes {
        return Err(exceeded());
    }
    // Reject even empty DTDs (roxmltree otherwise accepts an empty declaration).
    if text.contains("<!DOCTYPE") || text.contains("<!ENTITY") {
        return Err(unsupported());
    }
    let options = ParsingOptions {
        allow_dtd: false,
        nodes_limit: limits.max_xml_nodes,
        entity_resolver: None,
    };
    let doc = Document::parse_with_options(text, options).map_err(|e| {
        if matches!(e, roxmltree::Error::NodesLimitReached) {
            exceeded()
        } else {
            invalid()
        }
    })?;
    for node in doc.descendants() {
        if node.is_pi() {
            return Err(unsupported());
        }
        if node.ancestors().take(limits.max_xml_depth + 2).count() > limits.max_xml_depth + 1 {
            return Err(exceeded());
        }
    }
    Ok(doc)
}
pub(super) fn require_root(doc: &Document<'_>, ns: &str, name: &str) -> Result<()> {
    if !doc.root_element().has_tag_name((ns, name)) {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn attribute<'a>(node: Node<'a, '_>, name: &str) -> Result<&'a str> {
    node.attribute(name).ok_or_else(invalid)
}
pub(super) fn child<'a, 'input>(
    node: Node<'a, 'input>,
    name: &str,
) -> Result<Option<Node<'a, 'input>>> {
    let mut found = node.children().filter(|n| n.has_tag_name((MAIN, name)));
    let first = found.next();
    if found.next().is_some() {
        return Err(invalid());
    }
    Ok(first)
}
pub(super) fn part<'a>(parts: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    parts.get(name).map(String::as_str).ok_or_else(invalid)
}

#[derive(Debug)]
pub(super) struct Relationship {
    pub kind: String,
    pub target: String,
}
pub(super) fn relationships(
    text: &str,
    owner: &str,
    limits: &Limits,
) -> Result<BTreeMap<String, Relationship>> {
    let doc = xml(text, limits)?;
    require_root(&doc, PKG_REL, "Relationships")?;
    let mut result = BTreeMap::new();
    for node in doc.root_element().children().filter(Node::is_element) {
        if !node.has_tag_name((PKG_REL, "Relationship")) {
            return Err(unsupported());
        }
        if node
            .attribute("TargetMode")
            .is_some_and(|mode| mode != "Internal")
        {
            return Err(unsupported());
        }
        let id = attribute(node, "Id")?;
        if id.is_empty() || id.len() > 128 {
            return Err(invalid());
        }
        let target = resolve(owner, attribute(node, "Target")?)?;
        let kind = attribute(node, "Type")?.to_owned();
        if result
            .insert(id.to_owned(), Relationship { kind, target })
            .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(result)
}
fn resolve(owner: &str, target: &str) -> Result<String> {
    if target.is_empty()
        || target.len() > 255
        || !target.is_ascii()
        || target.contains(['\\', ':', '%', '?', '#'])
    {
        return Err(unsupported());
    }
    let mut segments: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        owner
            .rsplit_once('/')
            .map_or(Vec::new(), |(parent, _)| parent.split('/').collect())
    };
    for segment in target.trim_start_matches('/').split('/') {
        match segment {
            "" => return Err(invalid()),
            "." => (),
            ".." => {
                if segments.pop().is_none() {
                    return Err(invalid());
                }
            }
            value => segments.push(value),
        }
    }
    let path = segments.join("/");
    if !safe_part(&path) {
        return Err(invalid());
    }
    Ok(path)
}
