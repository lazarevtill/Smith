//! Text-only, self-contained presentation package. Read-back deliberately admits
//! only this canonical profile; general PPTX extraction is not claimed.
use super::{
    package::{CT, PKG_REL, REL},
    *,
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
};
use zip::{write::SimpleFileOptions, CompressionMethod, DateTime, ZipWriter};

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const DATA: &str = "urn:smith:table:1";

pub(super) fn read_generated(bytes: &[u8], limits: &Limits) -> Result<Table> {
    let parts = package::unzip(bytes, limits)?;
    let data = parts.get("customXml/item1.xml").ok_or_else(unsupported)?;
    let doc = package::xml(data, limits)?;
    let root = doc.root_element();
    if !root.has_tag_name((DATA, "table")) || root.children().any(|n| n.is_element()) {
        return Err(unsupported());
    }
    let text = root.text().ok_or_else(invalid)?;
    if text.len() > limits.max_workbook_text_bytes {
        return Err(exceeded());
    }
    let table: Table = serde_json::from_str(text).map_err(|_| invalid())?;
    // Every relationship/part/visible run must be exactly the generated profile.
    if write(&table, limits)? != bytes {
        return Err(unsupported());
    }
    Ok(table)
}
pub(super) fn write(table: &Table, limits: &Limits) -> Result<Vec<u8>> {
    export::table_workbook(table, limits)?;
    if table.rows.len() > 32 {
        return Err(exceeded());
    }
    for row in &table.rows {
        if row.len() > 9
            || row
                .first()
                .is_some_and(|c| c.display().chars().count() > 80)
            || row.iter().skip(1).any(|c| {
                c.display().chars().count() > 100 || c.display().contains(['\n', '\r', '\t'])
            })
            || row
                .first()
                .is_some_and(|c| c.display().contains(['\n', '\r', '\t']))
        {
            return Err(error(
                ErrorKind::LimitExceeded,
                "presentation text exceeds slide layout limits",
            ));
        }
    }
    let metadata = serde_json::to_string(table).map_err(|_| invalid())?;
    if metadata.len() > limits.max_workbook_text_bytes {
        return Err(exceeded());
    }
    let count = table.rows.len().max(1);
    let mut parts = BTreeMap::new();
    parts.insert(
        "customXml/item1.xml".into(),
        format!(
            "<table xmlns=\"{DATA}\">{}</table>",
            xml_escape(&metadata, false)
        ),
    );
    parts.insert(
        "_rels/.rels".into(),
        rels(&[
            ("presentation", "officeDocument", "ppt/presentation.xml"),
            ("data", "customXml", "customXml/item1.xml"),
        ]),
    );
    let mut types = format!("<Types xmlns=\"{CT}\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    for (path, kind) in [
        ("presentation.xml", "presentation.main"),
        ("slideMasters/slideMaster1.xml", "slideMaster"),
        ("slideLayouts/slideLayout1.xml", "slideLayout"),
    ] {
        types.push_str(&format!("<Override PartName=\"/ppt/{path}\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.{kind}+xml\"/>"));
    }
    types.push_str("<Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>");
    let mut ids = String::new();
    let mut relationships = format!("<Relationships xmlns=\"{PKG_REL}\"><Relationship Id=\"master\" Type=\"{REL}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>");
    for i in 1..=count {
        types.push_str(&format!("<Override PartName=\"/ppt/slides/slide{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>"));
        ids.push_str(&format!("<p:sldId id=\"{}\" r:id=\"s{i}\"/>", 255 + i));
        relationships.push_str(&format!(
            "<Relationship Id=\"s{i}\" Type=\"{REL}/slide\" Target=\"slides/slide{i}.xml\"/>"
        ));
        let row = table.rows.get(i - 1);
        let title = row
            .and_then(|r| r.first())
            .map_or("Presentation", CellValue::display);
        let bullets = row.map_or(&[][..], |r| &r[1..]);
        let title_shape = shape(
            2, "Title", 457200, 342900, 11277600, 1143000, title, 3000, false,
        );
        let mut body = String::new();
        for cell in bullets {
            body.push_str(&paragraph(cell.display(), 2000, true));
        }
        if body.is_empty() {
            body.push_str("<a:p/>");
        }
        let body_shape = shape_xml(3, "Body", [685800, 1714500, 10820400, 4572000], &body);
        parts.insert(format!("ppt/slides/slide{i}.xml"), format!("<p:sld xmlns:p=\"{P}\" xmlns:a=\"{A}\" xmlns:r=\"{REL}\"><p:cSld><p:spTree>{}{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>", group(), title_shape, body_shape));
        parts.insert(
            format!("ppt/slides/_rels/slide{i}.xml.rels"),
            rels(&[("layout", "slideLayout", "../slideLayouts/slideLayout1.xml")]),
        );
    }
    types.push_str("</Types>");
    relationships.push_str("</Relationships>");
    parts.insert("[Content_Types].xml".into(), types);
    parts.insert("ppt/presentation.xml".into(), format!("<p:presentation xmlns:p=\"{P}\" xmlns:a=\"{A}\" xmlns:r=\"{REL}\"><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"master\"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"12192000\" cy=\"6858000\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/><p:defaultTextStyle/></p:presentation>"));
    parts.insert("ppt/_rels/presentation.xml.rels".into(), relationships);
    parts.insert("ppt/slideMasters/slideMaster1.xml".into(), format!("<p:sldMaster xmlns:p=\"{P}\" xmlns:a=\"{A}\" xmlns:r=\"{REL}\"><p:cSld><p:spTree>{}</p:spTree></p:cSld><p:clrMap accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" bg1=\"lt1\" bg2=\"lt2\" folHlink=\"folHlink\" hlink=\"hlink\" tx1=\"dk1\" tx2=\"dk2\"/><p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"layout\"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>", group()));
    parts.insert(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels".into(),
        rels(&[
            ("layout", "slideLayout", "../slideLayouts/slideLayout1.xml"),
            ("theme", "theme", "../theme/theme1.xml"),
        ]),
    );
    parts.insert("ppt/slideLayouts/slideLayout1.xml".into(), format!("<p:sldLayout xmlns:p=\"{P}\" xmlns:a=\"{A}\" xmlns:r=\"{REL}\" type=\"blank\" preserve=\"1\"><p:cSld name=\"Blank\"><p:spTree>{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>", group()));
    parts.insert(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
        rels(&[("master", "slideMaster", "../slideMasters/slideMaster1.xml")]),
    );
    parts.insert("ppt/theme/theme1.xml".into(), theme());
    if parts.len() > limits.max_zip_entries {
        return Err(exceeded());
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o600);
    let mut total = 0;
    for (path, text) in parts {
        let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>{text}");
        total += xml.len();
        if total > limits.max_zip_total_bytes {
            return Err(exceeded());
        }
        package::xml(&xml, limits)?;
        zip.start_file(path, options).map_err(|_| invalid())?;
        zip.write_all(xml.as_bytes()).map_err(|_| invalid())?;
    }
    let bytes = zip.finish().map_err(|_| invalid())?.into_inner();
    if bytes.len() > limits.max_input_bytes {
        return Err(exceeded());
    }
    Ok(bytes)
}
fn rels(items: &[(&str, &str, &str)]) -> String {
    let mut out = format!("<Relationships xmlns=\"{PKG_REL}\">");
    for (id, kind, target) in items {
        out.push_str(&format!(
            "<Relationship Id=\"{id}\" Type=\"{REL}/{kind}\" Target=\"{target}\"/>"
        ));
    }
    out.push_str("</Relationships>");
    out
}
fn group() -> &'static str {
    "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>"
}
fn paragraph(text: &str, size: u32, bullet: bool) -> String {
    let ppr = if bullet {
        "<a:pPr marL=\"228600\" indent=\"-228600\"><a:spcAft><a:spcPts val=\"1000\"/></a:spcAft><a:buChar char=\"•\"/></a:pPr>"
    } else {
        "<a:pPr><a:buNone/></a:pPr>"
    };
    format!("<a:p>{ppr}<a:r><a:rPr sz=\"{size}\"><a:solidFill><a:srgbClr val=\"111827\"/></a:solidFill><a:latin typeface=\"Arial\"/></a:rPr><a:t xml:space=\"preserve\">{}</a:t></a:r><a:endParaRPr sz=\"{size}\"/></a:p>", xml_escape(text, false))
}
#[allow(clippy::too_many_arguments)]
fn shape(
    id: u32,
    name: &str,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    text: &str,
    size: u32,
    bullet: bool,
) -> String {
    shape_xml(id, name, [x, y, w, h], &paragraph(text, size, bullet))
}
fn shape_xml(id: u32, name: &str, rect: [u32; 4], paragraphs: &str) -> String {
    let [x, y, w, h] = rect;
    format!("<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap=\"square\"><a:normAutofit/></a:bodyPr><a:lstStyle/>{paragraphs}</p:txBody></p:sp>")
}
fn theme() -> String {
    let mut colors = String::new();
    for (name, color) in [
        ("dk1", "111827"),
        ("lt1", "FFFFFF"),
        ("dk2", "374151"),
        ("lt2", "F3F4F6"),
        ("accent1", "2563EB"),
        ("accent2", "059669"),
        ("accent3", "D97706"),
        ("accent4", "7C3AED"),
        ("accent5", "DB2777"),
        ("accent6", "0891B2"),
        ("hlink", "2563EB"),
        ("folHlink", "7C3AED"),
    ] {
        colors.push_str(&format!(
            "<a:{name}><a:srgbClr val=\"{color}\"/></a:{name}>"
        ));
    }
    let fill = "<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>";
    let line = format!("<a:ln w=\"12700\">{fill}<a:prstDash val=\"solid\"/></a:ln>");
    format!("<a:theme xmlns:a=\"{A}\" name=\"Smith\"><a:themeElements><a:clrScheme name=\"Smith\">{colors}</a:clrScheme><a:fontScheme name=\"Smith\"><a:majorFont><a:latin typeface=\"Arial\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont><a:minorFont><a:latin typeface=\"Arial\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme><a:fmtScheme name=\"Smith\"><a:fillStyleLst>{fill}{fill}{fill}</a:fillStyleLst><a:lnStyleLst>{line}{line}{line}</a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst>{fill}{fill}{fill}</a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>")
}
