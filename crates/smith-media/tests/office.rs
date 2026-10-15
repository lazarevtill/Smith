use smith_media::{
    export_table, inspect, read_workbook, write_workbook, AttachmentRequest, Cell, CellValue,
    Content, ErrorKind, ExportFormat, Format, Limits, NumberFormat, SheetVisibility, Table,
    Workbook,
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

const CELLS: &[u8] = include_bytes!("fixtures/cells.xlsx");
const SHARED: &[u8] = include_bytes!("fixtures/shared-strings.xlsx");
fn expected() -> Workbook {
    serde_json::from_slice(include_bytes!("fixtures/cells.expected.json")).unwrap()
}
fn parts(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..archive.len())
        .map(|i| {
            let mut file = archive.by_index(i).unwrap();
            let name = file.name().unwrap().into_owned();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            (name, text)
        })
        .collect()
}
fn zip(parts: BTreeMap<String, String>) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, text) in parts {
        zip.start_file(name, options).unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
fn replace(source: &[u8], path: &str, old: &str, new: &str) -> Vec<u8> {
    let mut files = parts(source);
    let text = files.get_mut(path).unwrap();
    assert!(text.contains(old), "fixture marker missing: {old}");
    *text = text.replace(old, new);
    zip(files)
}
fn fails(bytes: &[u8], kind: ErrorKind) {
    assert_eq!(
        read_workbook(bytes, &Limits::default()).unwrap_err().kind,
        kind
    );
}

#[test]
fn typed_workbook_round_trip_retains_text_numbers_coordinates_visibility_and_formats() {
    let expected = expected();
    assert_eq!(read_workbook(CELLS, &Limits::default()).unwrap(), expected);
    let a = write_workbook(&expected, &Limits::default()).unwrap();
    let b = write_workbook(&expected, &Limits::default()).unwrap();
    assert_eq!(a, b, "deterministic ZIP timestamps and entry order");
    assert_eq!(read_workbook(&a, &Limits::default()).unwrap(), expected);
    let manifest = inspect(
        AttachmentRequest::new("report.xlsx", Some(Format::Xlsx.mime())),
        CELLS,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(manifest.format, Format::Xlsx);
    match manifest.content {
        Content::Document {
            context,
            truncated: false,
            ..
        } => assert_eq!(
            serde_json::from_str::<Workbook>(&context).unwrap(),
            expected
        ),
        _ => panic!("expected complete structured context"),
    }
}

#[test]
fn independent_shared_string_fixture_preserves_sparse_cells_rich_text_and_date_system() {
    let book = read_workbook(SHARED, &Limits::default()).unwrap();
    assert!(book.date_1904);
    let cells = &book.sheets[0].cells;
    assert_eq!(cells.len(), 6);
    assert_eq!(cells[0].value, CellValue::Text("00123".into()));
    assert_eq!(cells[1].value, CellValue::Text("rich 世界".into()));
    assert_eq!((cells[2].row, cells[2].column), (4, 2));
    assert_eq!(
        cells[2].number_format,
        Some(NumberFormat {
            id: 170,
            code: Some("yyyy-mm-dd".into())
        })
    );
    assert_eq!(cells[3].value, CellValue::Number("9007199254740993".into()));
    assert_eq!(
        cells[3].number_format, None,
        "explicit general style overrides row style"
    );
    assert_eq!(cells[4].value, CellValue::Text("👋 literal _x0041_".into()));
    assert_eq!(
        cells[5].number_format,
        Some(NumberFormat { id: 14, code: None })
    );
    assert_eq!(
        read_workbook(
            &write_workbook(&book, &Limits::default()).unwrap(),
            &Limits::default()
        )
        .unwrap(),
        book
    );
}

#[test]
fn table_exports_preserve_csv_cells_and_create_real_xlsx() {
    let table = Table {
        headers: vec!["name".into(), "value".into()],
        rows: vec![
            vec![
                CellValue::Text("Ada, \"A\"\nnext".into()),
                CellValue::Number("-12.50".into()),
            ],
            vec![
                CellValue::Text("00123".into()),
                CellValue::Text(String::new()),
            ],
        ],
    };
    let csv = export_table(&table, ExportFormat::Csv, &Limits::default()).unwrap();
    assert_eq!(
        String::from_utf8(csv.bytes.clone()).unwrap(),
        "name,value\n\"Ada, \"\"A\"\"\nnext\",-12.50\n00123,\n"
    );
    let manifest = inspect(
        AttachmentRequest::new("report.csv", None),
        &csv.bytes,
        &Limits::default(),
    )
    .unwrap();
    let Content::Document { context, .. } = manifest.content else {
        panic!()
    };
    let values: Vec<Vec<String>> = serde_json::from_str(&context).unwrap();
    assert_eq!(values[1], ["Ada, \"A\"\nnext", "-12.50"]);
    let xlsx = export_table(&table, ExportFormat::Xlsx, &Limits::default()).unwrap();
    assert!(xlsx.bytes.starts_with(b"PK\x03\x04"));
    let book = read_workbook(&xlsx.bytes, &Limits::default()).unwrap();
    assert_eq!(
        book.sheets[0].cells[4].value,
        CellValue::Text("00123".into())
    );
    assert_eq!(
        book.sheets[0].cells[5].value,
        CellValue::Text(String::new())
    );
}

#[test]
fn formula_looking_text_is_literal_in_xlsx_and_refused_in_untyped_csv() {
    for text in ["=SUM(A1:A2)", " +cmd", "-not a number", "\t@SUM(A1)"] {
        let table = Table {
            headers: vec![],
            rows: vec![vec![CellValue::Text(text.into())]],
        };
        let xlsx = export_table(&table, ExportFormat::Xlsx, &Limits::default()).unwrap();
        assert!(!parts(&xlsx.bytes)["xl/worksheets/sheet1.xml"].contains("<f"));
        assert_eq!(
            read_workbook(&xlsx.bytes, &Limits::default())
                .unwrap()
                .sheets[0]
                .cells[0]
                .value,
            CellValue::Text(text.into())
        );
        assert_eq!(
            export_table(&table, ExportFormat::Csv, &Limits::default())
                .unwrap_err()
                .kind,
            ErrorKind::UnsupportedFormat
        );
    }
}

#[test]
fn markdown_export_escapes_cells_and_keeps_multiline_table_shape() {
    let table = Table {
        headers: vec!["h|i".into()],
        rows: vec![vec![CellValue::Text("<script>[link](url)\nnext".into())]],
    };
    let text = String::from_utf8(
        export_table(&table, ExportFormat::Markdown, &Limits::default())
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(
        text,
        "| h\\|i |\n| --- |\n| &lt;script&gt;\\[link\\]\\(url\\)<br>next |\n"
    );
}

#[test]
fn formulas_macros_external_relationships_and_embedded_parts_are_rejected() {
    let formula = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "<v>123.50</v>",
        "<f>WEBSERVICE(\"https://example.invalid\")</f><v>123.50</v>",
    );
    fails(&formula, ErrorKind::UnsupportedFormat);
    let external = replace(
        CELLS,
        "xl/_rels/workbook.xml.rels",
        "Target=\"worksheets/sheet1.xml\"",
        "Target=\"https://example.invalid/book.xml\" TargetMode=\"External\"",
    );
    fails(&external, ErrorKind::UnsupportedFormat);
    let no_mode = replace(
        CELLS,
        "xl/_rels/workbook.xml.rels",
        "Target=\"worksheets/sheet1.xml\"",
        "Target=\"file:///etc/passwd\"",
    );
    fails(&no_mode, ErrorKind::UnsupportedFormat);
    let macro_type = replace(
        CELLS,
        "[Content_Types].xml",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
        "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
    );
    fails(&macro_type, ErrorKind::UnsupportedFormat);
    for name in [
        "xl/vbaProject.bin",
        "xl/embeddings/object.xml",
        "xl/externalLinks/externalLink1.xml",
    ] {
        let mut entries = parts(CELLS);
        entries.insert(name.into(), "<blocked/>".into());
        fails(&zip(entries), ErrorKind::UnsupportedFormat);
    }
}

#[test]
fn unsupported_cell_types_merges_and_missing_cache_are_not_dropped() {
    let merged = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "</worksheet>",
        "<mergeCells><mergeCell ref=\"A1:B1\"/></mergeCells></worksheet>",
    );
    fails(&merged, ErrorKind::UnsupportedFormat);
    let iso = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "r=\"B2\" t=\"n\"",
        "r=\"B2\" t=\"d\"",
    );
    fails(&iso, ErrorKind::UnsupportedFormat);
    let uncached_formula = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "<v>123.50</v>",
        "<f>1+1</f>",
    );
    fails(&uncached_formula, ErrorKind::UnsupportedFormat);
}

#[test]
fn xml_dtd_entities_processing_instructions_and_wrong_namespaces_fail() {
    for insertion in [
        "<!DOCTYPE worksheet [<!ENTITY xxe SYSTEM \"file:///etc/passwd\">]>",
        "<!DOCTYPE worksheet>",
        "<?xml-stylesheet href=\"https://example.invalid/style.xsl\"?>",
    ] {
        let mut entries = parts(CELLS);
        let content = entries.get_mut("xl/worksheets/sheet1.xml").unwrap();
        let at = content.find("<worksheet").unwrap();
        content.insert_str(at, insertion);
        fails(&zip(entries), ErrorKind::UnsupportedFormat);
    }
    let bad_namespace = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "http://schemas.openxmlformats.org/spreadsheetml/2006/main",
        "urn:fake",
    );
    fails(&bad_namespace, ErrorKind::InvalidContent);
}

#[test]
fn zip_traversal_symlinks_duplicates_crc_and_entry_count_are_rejected() {
    let mut entries = parts(CELLS);
    entries.insert("../outside.xml".into(), "<outside/>".into());
    fails(&zip(entries), ErrorKind::InvalidContent);
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, text) in parts(CELLS) {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer
        .add_symlink("link.xml", "outside.xml", SimpleFileOptions::default())
        .unwrap();
    fails(
        &writer.finish().unwrap().into_inner(),
        ErrorKind::InvalidContent,
    );
    let mut entries = parts(CELLS);
    entries.insert("abc.xml".into(), "<test/>".into());
    entries.insert("def.xml".into(), "<test/>".into());
    let mut duplicate = zip(entries);
    let indices: Vec<_> = duplicate
        .windows(7)
        .enumerate()
        .filter_map(|(i, b)| (b == b"def.xml").then_some(i))
        .collect();
    for index in indices {
        duplicate[index..index + 7].copy_from_slice(b"abc.xml");
    }
    fails(&duplicate, ErrorKind::InvalidContent);
    let mut count = CELLS.to_vec();
    let end = count.windows(4).rposition(|v| v == b"PK\x05\x06").unwrap();
    count[end + 10..end + 12].copy_from_slice(&200u16.to_le_bytes());
    fails(&count, ErrorKind::LimitExceeded);
    let mut corrupted = CELLS.to_vec();
    let first = corrupted
        .windows(4)
        .position(|v| v == b"PK\x03\x04")
        .unwrap();
    let offset = first
        + 30
        + u16::from_le_bytes([corrupted[first + 26], corrupted[first + 27]]) as usize
        + u16::from_le_bytes([corrupted[first + 28], corrupted[first + 29]]) as usize;
    corrupted[offset] ^= 0xff;
    fails(&corrupted, ErrorKind::InvalidContent);
}

#[test]
fn zip_fallback_candidates_zip64_and_overlapping_local_records_fail_before_reading() {
    let mut entries = BTreeMap::new();
    for i in 0..129 {
        entries.insert(format!("part{i}.xml"), "<part/>".into());
    }
    let mut fallback = zip(entries);
    // A forged final, bounded EOCD must not hide the larger earlier directory.
    let at = fallback.len();
    let mut forged = [0u8; 22];
    forged[..4].copy_from_slice(b"PK\x05\x06");
    forged[8..10].copy_from_slice(&1u16.to_le_bytes());
    forged[10..12].copy_from_slice(&1u16.to_le_bytes());
    forged[16..20].copy_from_slice(&(at as u32).to_le_bytes());
    fallback.extend_from_slice(&forged);
    fails(&fallback, ErrorKind::LimitExceeded);

    let mut zip64 = CELLS.to_vec();
    let end = zip64.windows(4).rposition(|v| v == b"PK\x05\x06").unwrap();
    zip64[end + 20..end + 22].copy_from_slice(&4u16.to_le_bytes());
    zip64.extend_from_slice(b"PK\x06\x07");
    fails(&zip64, ErrorKind::UnsupportedFormat);

    let mut overlap = CELLS.to_vec();
    let central: Vec<_> = overlap
        .windows(4)
        .enumerate()
        .filter_map(|(i, v)| (v == b"PK\x01\x02").then_some(i))
        .collect();
    let first_offset: [u8; 4] = overlap[central[0] + 42..central[0] + 46]
        .try_into()
        .unwrap();
    overlap[central[1] + 42..central[1] + 46].copy_from_slice(&first_offset);
    fails(&overlap, ErrorKind::InvalidContent);
}

#[test]
fn workbook_text_limits_and_attribute_whitespace_survive_generation() {
    let mut book = expected();
    book.sheets[0].cells[0].number_format = Some(NumberFormat {
        id: 171,
        code: Some("0\t\n\r\"x\"".into()),
    });
    assert_eq!(
        read_workbook(
            &write_workbook(&book, &Limits::default()).unwrap(),
            &Limits::default()
        )
        .unwrap(),
        book
    );
    book.sheets[0].cells[0].value = CellValue::Text("x".repeat(32768));
    assert_eq!(
        write_workbook(&book, &Limits::default()).unwrap_err().kind,
        ErrorKind::LimitExceeded
    );
}

#[test]
fn excel_string_escapes_respect_run_boundaries_and_attribute_values() {
    let bytes = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "<t xml:space=\"preserve\">00123</t>",
        "<r><t>_x00</t></r><r><t>41_</t></r>",
    );
    let book = read_workbook(&bytes, &Limits::default()).unwrap();
    assert_eq!(
        book.sheets[0].cells[2].value,
        CellValue::Text("_x0041_".into())
    );
    let mut book = expected();
    book.sheets[0].name = "_x0041_".into();
    book.sheets[0].cells[0].number_format = Some(NumberFormat {
        id: 171,
        code: Some("0\"_x0041_\"".into()),
    });
    let bytes = write_workbook(&book, &Limits::default()).unwrap();
    assert!(parts(&bytes)["xl/workbook.xml"].contains("_x005F_x0041_"));
    assert!(parts(&bytes)["xl/styles.xml"].contains("_x005F_x0041_"));
    assert_eq!(read_workbook(&bytes, &Limits::default()).unwrap(), book);
    let escaped = replace(CELLS, "xl/workbook.xml", "Data 世界", "_x005F_x0041_");
    assert_eq!(
        read_workbook(&escaped, &Limits::default()).unwrap().sheets[0].name,
        "_x0041_"
    );
}

#[test]
fn unimplemented_number_format_inheritance_is_rejected_explicitly() {
    for replacement in ["", "customFormat=\"0\""] {
        let row = replace(
            SHARED,
            "xl/worksheets/sheet1.xml",
            "customFormat=\"1\"",
            replacement,
        );
        fails(&row, ErrorKind::UnsupportedFormat);
    }
    let disabled = replace(
        CELLS,
        "xl/styles.xml",
        "applyNumberFormat=\"1\"",
        "applyNumberFormat=\"0\"",
    );
    fails(&disabled, ErrorKind::UnsupportedFormat);
    let inherited = replace(CELLS, "xl/styles.xml", "xfId=\"0\"", "xfId=\"1\"");
    fails(&inherited, ErrorKind::UnsupportedFormat);
    let base = replace(
        CELLS,
        "xl/styles.xml",
        "<cellStyleXfs count=\"1\"><xf numFmtId=\"0\"",
        "<cellStyleXfs count=\"1\"><xf numFmtId=\"14\"",
    );
    fails(&base, ErrorKind::UnsupportedFormat);
}

#[test]
fn zip_xml_sheet_cell_shared_string_and_text_budgets_are_enforced() {
    for limits in [
        Limits {
            max_zip_entries: 2,
            ..Limits::default()
        },
        Limits {
            max_zip_part_bytes: 32,
            ..Limits::default()
        },
        Limits {
            max_zip_total_bytes: 100,
            ..Limits::default()
        },
        Limits {
            max_xml_nodes: 3,
            ..Limits::default()
        },
        Limits {
            max_xml_depth: 2,
            ..Limits::default()
        },
        Limits {
            max_sheets: 1,
            ..Limits::default()
        },
        Limits {
            max_cells: 2,
            ..Limits::default()
        },
        Limits {
            max_csv_rows: 2,
            ..Limits::default()
        },
        Limits {
            max_csv_columns: 1,
            ..Limits::default()
        },
        Limits {
            max_workbook_text_bytes: 3,
            ..Limits::default()
        },
    ] {
        assert_eq!(
            read_workbook(CELLS, &limits).unwrap_err().kind,
            ErrorKind::LimitExceeded
        );
    }
    let limits = Limits {
        max_shared_strings: 2,
        ..Limits::default()
    };
    assert_eq!(
        read_workbook(SHARED, &limits).unwrap_err().kind,
        ErrorKind::LimitExceeded
    );
    let mut bomb = parts(CELLS);
    bomb.insert(
        "xl/styles.xml".into(),
        format!("<styleSheet><!--{}--></styleSheet>", "a".repeat(400_000)),
    );
    fails(&zip(bomb), ErrorKind::LimitExceeded);
}

#[test]
fn malformed_sparse_coordinates_strings_and_numerics_fail_closed() {
    let huge = replace(
        CELLS,
        "xl/worksheets/sheet1.xml",
        "r=\"A1\"",
        "r=\"A1048576\"",
    );
    fails(&huge, ErrorKind::LimitExceeded);
    let duplicate = replace(CELLS, "xl/worksheets/sheet1.xml", "r=\"B1\"", "r=\"A1\"");
    fails(&duplicate, ErrorKind::InvalidContent);
    let bad_index = replace(SHARED, "xl/worksheets/sheet1.xml", "<v>0</v>", "<v>999</v>");
    fails(&bad_index, ErrorKind::InvalidContent);
    for malformed in ["_x0000_", "_xD800_", "_x202E_"] {
        let bad = replace(SHARED, "xl/sharedStrings.xml", "00123", malformed);
        fails(&bad, ErrorKind::InvalidContent);
    }
    for malformed in ["NaN", "inf", "1e999", "<x/>", " 1 "] {
        let bad = replace(
            CELLS,
            "xl/worksheets/sheet1.xml",
            "<v>123.50</v>",
            &format!("<v>{malformed}</v>"),
        );
        fails(&bad, ErrorKind::InvalidContent);
    }
}

#[test]
fn workbook_export_rejects_ambiguous_names_styles_and_out_of_budget_data() {
    let mut book = expected();
    book.sheets[1].name = "data 世界".into();
    assert_eq!(
        write_workbook(&book, &Limits::default()).unwrap_err().kind,
        ErrorKind::InvalidContent
    );
    let mut book = expected();
    book.sheets
        .iter_mut()
        .for_each(|s| s.visibility = SheetVisibility::Hidden);
    assert_eq!(
        write_workbook(&book, &Limits::default()).unwrap_err().kind,
        ErrorKind::InvalidContent
    );
    let mut book = expected();
    book.sheets[0].cells.push(Cell {
        row: 1001,
        column: 1,
        value: CellValue::Text("large sparse row".into()),
        number_format: None,
    });
    assert_eq!(
        write_workbook(&book, &Limits::default()).unwrap_err().kind,
        ErrorKind::LimitExceeded
    );
    let mut book = expected();
    book.sheets[0].cells[0].number_format = Some(NumberFormat {
        id: 164,
        code: Some("0.000".into()),
    });
    assert_eq!(
        write_workbook(&book, &Limits::default()).unwrap_err().kind,
        ErrorKind::InvalidContent
    );
    let table = Table {
        headers: vec!["one".into()],
        rows: vec![vec![]],
    };
    assert_eq!(
        export_table(&table, ExportFormat::Xlsx, &Limits::default())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidContent
    );
}

#[test]
fn unsupported_office_extensions_and_mime_spoofs_are_explicit() {
    for name in [
        "file.xls",
        "file.xlsm",
        "file.ods",
        "file.docx",
        "file.pdf",
        "file.pptx",
    ] {
        assert_eq!(
            inspect(
                AttachmentRequest::new(name, None),
                CELLS,
                &Limits::default()
            )
            .unwrap_err()
            .kind,
            ErrorKind::UnsupportedFormat
        );
    }
    assert_eq!(
        inspect(
            AttachmentRequest::new("file.xlsx", Some("text/plain")),
            CELLS,
            &Limits::default()
        )
        .unwrap_err()
        .kind,
        ErrorKind::MimeMismatch
    );
    fails(b"not a ZIP package", ErrorKind::InvalidContent);
}

#[test]
fn workbook_context_truncation_is_explicit_without_losing_original_data() {
    let limits = Limits {
        max_context_chars: 30,
        ..Limits::default()
    };
    let manifest = inspect(AttachmentRequest::new("report.xlsx", None), CELLS, &limits).unwrap();
    match manifest.content {
        Content::Document {
            context,
            truncated: true,
            total_context_chars,
            ..
        } => {
            assert_eq!(context.chars().count(), 30);
            assert!(total_context_chars > 30);
        }
        _ => panic!(),
    }
    assert_eq!(read_workbook(CELLS, &limits).unwrap(), expected());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn generated_workbooks_and_tables_use_private_atomic_store_and_verified_reads() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = smith_media::MediaStore::open(dir.path(), Limits::default()).unwrap();
    let saved = store
        .generate_workbook(AttachmentRequest::new("book.xlsx", None), &expected())
        .unwrap();
    assert_eq!(
        read_workbook(&store.read_bytes(&saved.id).unwrap(), &Limits::default()).unwrap(),
        expected()
    );
    assert_eq!(store.read(&saved.id).unwrap(), saved);
    let table = Table {
        headers: vec!["name".into()],
        rows: vec![vec![CellValue::Text("Ada".into())]],
    };
    for (filename, format) in [
        ("table.csv", ExportFormat::Csv),
        ("table.md", ExportFormat::Markdown),
        ("table.xlsx", ExportFormat::Xlsx),
    ] {
        let saved = store
            .generate_table(AttachmentRequest::new(filename, None), &table, format)
            .unwrap();
        assert_eq!(store.read(&saved.id).unwrap(), saved);
    }
    assert_eq!(
        store
            .generate_workbook(AttachmentRequest::new("wrong.txt", None), &expected())
            .unwrap_err()
            .kind,
        ErrorKind::MimeMismatch
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join(".staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("attachments"))
            .unwrap()
            .count(),
        4
    );
}
