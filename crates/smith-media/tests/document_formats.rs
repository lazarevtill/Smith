use smith_media::*;
use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};
const XLS: &[u8] = include_bytes!("fixtures/passive.xls");
const ODS: &[u8] = include_bytes!("fixtures/passive.ods");
fn parts(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut z = ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            let mut text = String::new();
            f.read_to_string(&mut text).unwrap();
            (name, text)
        })
        .collect()
}
fn zip(parts: BTreeMap<String, String>) -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    for (n, s) in parts {
        z.start_file(n, SimpleFileOptions::default()).unwrap();
        z.write_all(s.as_bytes()).unwrap();
    }
    z.finish().unwrap().into_inner()
}
fn check(name: &str, bytes: &[u8]) -> Manifest {
    inspect(
        AttachmentRequest::new(name, None),
        bytes,
        &Limits::default(),
    )
    .unwrap()
}
fn table() -> Table {
    Table {
        headers: vec!["Title".into(), "Detail".into()],
        rows: vec![
            vec![
                CellValue::Text("Привет, мир! Ёж".into()),
                CellValue::Text("00123 & <safe> — café".into()),
            ],
            vec![
                CellValue::Text("Exact data".into()),
                CellValue::Number("9007199254740993.00".into()),
            ],
        ],
    }
}

#[test]
fn real_pdf_pptx_unicode_fixtures_are_deterministic_and_keep_context() {
    for (format, name, fixture) in [
        (
            ExportFormat::Pdf,
            "fixture.pdf",
            include_bytes!("fixtures/unicode.pdf").as_slice(),
        ),
        (
            ExportFormat::Pptx,
            "fixture.pptx",
            include_bytes!("fixtures/unicode.pptx").as_slice(),
        ),
    ] {
        let doc = export_table(&table(), format, &Limits::default()).unwrap();
        assert_eq!(doc.bytes, fixture);
        let manifest = check(name, &doc.bytes);
        let Content::Document {
            context, truncated, ..
        } = manifest.content
        else {
            panic!()
        };
        assert!(!truncated);
        assert_eq!(serde_json::from_str::<Table>(&context).unwrap(), table());
        let mut changed = doc.bytes;
        changed.extend_from_slice(b"extra active payload");
        assert!(inspect(
            AttachmentRequest::new(name, None),
            &changed,
            &Limits::default()
        )
        .is_err());
    }
}
#[test]
fn pdf_font_covers_russian_alphabet_and_rejects_unshaped_scripts() {
    let font =
        ttf_parser::Face::parse(include_bytes!("../assets/fonts/NotoSans-Regular.ttf"), 0).unwrap();
    for c in ('А'..='я').chain(['Ё', 'ё']) {
        assert!(font.glyph_index(c).is_some());
    }
    let mut table = table();
    for text in ["مرحبا", "а\u{0483}", "a\u{200d}b", "a\u{200f}b"] {
        table.rows[0][0] = CellValue::Text(text.into());
        assert_eq!(
            export_table(&table, ExportFormat::Pdf, &Limits::default())
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedFormat
        );
    }
}
#[test]
fn generated_document_context_truncation_never_changes_original_bytes() {
    let bytes = include_bytes!("fixtures/unicode.pdf");
    let limits = Limits {
        max_context_chars: 12,
        ..Limits::default()
    };
    let m = inspect(
        AttachmentRequest::new("x.pdf", Some("application/pdf")),
        bytes,
        &limits,
    )
    .unwrap();
    assert_eq!(m.byte_len, bytes.len() as u64);
    let Content::Document {
        context, truncated, ..
    } = m.content
    else {
        panic!()
    };
    assert_eq!(context.chars().count(), 12);
    assert!(truncated);
}
#[test]
fn pdf_page_and_pptx_slide_layout_limits_fail_before_output() {
    let mut t = table();
    t.rows = vec![vec![CellValue::Text("x".repeat(30000)), CellValue::Blank]; 100];
    assert_eq!(
        export_table(&t, ExportFormat::Pdf, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    let mut t = table();
    t.rows = vec![t.rows[0].clone(); 33];
    assert_eq!(
        export_table(&t, ExportFormat::Pptx, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    t.rows.truncate(1);
    t.rows[0][0] = CellValue::Text("x".repeat(81));
    assert_eq!(
        export_table(&t, ExportFormat::Pptx, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
}
#[test]
fn arbitrary_pdf_and_pptx_and_modified_visible_context_are_not_claimed() {
    assert_eq!(
        inspect(
            AttachmentRequest::new("x.pdf", None),
            b"%PDF-1.7\n",
            &Limits::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::UnsupportedFormat
    );
    let mut p = parts(include_bytes!("fixtures/unicode.pptx"));
    p.get_mut("ppt/slides/slide1.xml").unwrap().push(' ');
    assert_eq!(
        inspect(
            AttachmentRequest::new("x.pptx", None),
            &zip(p),
            &Limits::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::UnsupportedFormat
    );
}
#[test]
fn ods_preserves_typed_data_repetition_and_exact_unicode_and_number() {
    let b = read_spreadsheet(Format::Ods, ODS, &Limits::default()).unwrap();
    assert_eq!(b.sheets[0].name, "Данные");
    assert_eq!(b.sheets[0].cells.len(), 6);
    assert_eq!(
        b.sheets[0].cells[0].value,
        CellValue::Text("00123\nПривет   мир".into())
    );
    assert_eq!(
        b.sheets[0].cells[1].value,
        CellValue::Number("9007199254740993.00".into())
    );
    let out = write_workbook(&b, &Limits::default()).unwrap();
    assert_eq!(read_workbook(&out, &Limits::default()).unwrap(), b);
    check("x.ods", ODS);
}
#[test]
fn ods_repetition_formulas_entities_external_objects_and_date_coercion_fail() {
    for (from, to, kind) in [
        (
            "number-rows-repeated=\"2\"",
            "number-rows-repeated=\"4294967295\"",
            ErrorKind::LimitExceeded,
        ),
        (
            "office:value-type=\"float\"",
            "table:formula=\"of:=1+1\" office:value-type=\"float\"",
            ErrorKind::UnsupportedFormat,
        ),
        (
            "office:value-type=\"float\"",
            "office:value-type=\"date\"",
            ErrorKind::UnsupportedFormat,
        ),
        (
            "<text:p>00123</text:p>",
            "<text:a>00123</text:a>",
            ErrorKind::UnsupportedFormat,
        ),
    ] {
        let mut p = parts(ODS);
        let c = p.get_mut("content.xml").unwrap();
        assert!(c.contains(from));
        *c = c.replace(from, to);
        assert_eq!(
            inspect(
                AttachmentRequest::new("x.ods", None),
                &zip(p),
                &Limits::default()
            )
            .unwrap_err()
            .kind,
            kind
        );
    }
}
#[test]
fn macro_free_xlsm_reads_with_exact_type_and_actual_macros_fail() {
    let mut p = parts(include_bytes!("fixtures/cells.xlsx"));
    let types = p.get_mut("[Content_Types].xml").unwrap();
    *types = types
        .replace(
            "spreadsheetml.sheet.main+xml",
            "ms-excel.sheet.macroEnabled.main+xml",
        )
        .replace(
            "application/vnd.openxmlformats-officedocument.ms-excel",
            "application/vnd.ms-excel",
        );
    let bytes = zip(p.clone());
    let b = read_spreadsheet(Format::Xlsm, &bytes, &Limits::default()).unwrap();
    assert!(!b.sheets.is_empty());
    check("x.xlsm", &bytes);
    assert!(read_workbook(&bytes, &Limits::default()).is_err());
    p.insert("xl/vbaProject.bin".into(), "macro bytes".into());
    assert_eq!(
        read_spreadsheet(Format::Xlsm, &zip(p), &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedFormat
    );
}
#[test]
fn xls_independent_biff8_preserves_strings_dates_booleans_and_blanks() {
    let book = read_spreadsheet(Format::Xls, XLS, &Limits::default()).unwrap();
    let cells = &book.sheets[0].cells;
    assert_eq!(cells.len(), 6);
    assert_eq!(cells[0].value, CellValue::Text("00123".into()));
    assert_eq!(cells[1].value, CellValue::Text("Привет".into()));
    assert_eq!(cells[2].value, CellValue::Number("42.125".into()));
    assert_eq!(cells[3].number_format.as_ref().unwrap().id, 14);
    assert_eq!(cells[4].value, CellValue::Boolean(true));
    assert_eq!(cells[5].value, CellValue::Blank);
    assert_eq!(
        read_workbook(
            &write_workbook(&book, &Limits::default()).unwrap(),
            &Limits::default()
        )
        .unwrap(),
        book
    );
    check("x.xls", XLS);
}
#[test]
fn xls_cfb_chains_advertised_allocations_biff_dimensions_and_macros_fail() {
    for (at, value) in [
        (44, 0xffff_fffe),
        (48, 0x0fff_ffff),
        (64, 0x7fff_ffff),
        (72, 0x7fff_ffff),
        (10 * 512, 0),
    ] {
        let mut bytes = XLS.to_vec();
        bytes[at..at + 4].copy_from_slice(&u32::to_le_bytes(value));
        assert!(read_spreadsheet(Format::Xls, &bytes, &Limits::default()).is_err());
    }
    let mut bytes = XLS.to_vec();
    let at = bytes
        .windows(4)
        .position(|b| b == b"\x00\x02\x0e\x00")
        .unwrap();
    bytes[at + 8..at + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        read_spreadsheet(Format::Xls, &bytes, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::LimitExceeded
    );
    let mut bytes = XLS.to_vec();
    let at = bytes
        .windows(4)
        .position(|b| b == b"\x03\x02\x0e\x00")
        .unwrap();
    bytes[at..at + 2].copy_from_slice(&6u16.to_le_bytes());
    assert_eq!(
        read_spreadsheet(Format::Xls, &bytes, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedFormat
    );
}

#[test]
fn xls_preflight_rejects_invalid_unicode_inverted_dimensions_and_ambiguous_formats() {
    let mut bytes = XLS.to_vec();
    let text: Vec<u8> = "Привет".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let at = bytes.windows(text.len()).position(|b| b == text).unwrap();
    bytes[at..at + 2].copy_from_slice(&0xd800u16.to_le_bytes());
    assert!(read_spreadsheet(Format::Xls, &bytes, &Limits::default()).is_err());

    let mut bytes = XLS.to_vec();
    let at = bytes
        .windows(4)
        .position(|b| b == b"\x00\x02\x0e\x00")
        .unwrap();
    bytes[at + 4..at + 8].copy_from_slice(&3u32.to_le_bytes());
    assert!(read_spreadsheet(Format::Xls, &bytes, &Limits::default()).is_err());

    let mut bytes = XLS.to_vec();
    let at = bytes
        .windows(8)
        .position(|b| b == b"\xe0\x00\x14\x00\x00\x00\x0e\x00")
        .unwrap();
    bytes[at + 4 + 9] = 0;
    assert_eq!(
        read_spreadsheet(Format::Xls, &bytes, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::UnsupportedFormat
    );
}
