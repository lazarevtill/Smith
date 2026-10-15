use smith_media::{export_table, CellValue, ExportFormat, Limits, Table};
fn main() {
    let destination = std::env::args()
        .nth(1)
        .expect("fixture destination directory");
    let table = Table {
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
    };
    for (format, name) in [
        (ExportFormat::Pdf, "unicode.pdf"),
        (ExportFormat::Pptx, "unicode.pptx"),
    ] {
        let doc = export_table(&table, format, &Limits::default()).expect("valid synthetic table");
        std::fs::write(std::path::Path::new(&destination).join(name), doc.bytes).unwrap();
    }
}
