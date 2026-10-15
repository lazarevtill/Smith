//! Generate only synthetic local XLSX test data using the public library API.
use smith_media::{
    write_workbook, Cell, CellValue, Limits, NumberFormat, Sheet, SheetVisibility, Workbook,
};
use std::path::PathBuf;

fn cell(row: u32, column: u16, value: CellValue) -> Cell {
    Cell {
        row,
        column,
        value,
        number_format: None,
    }
}
fn main() {
    let book = Workbook {
        date_1904: false,
        sheets: vec![
            Sheet {
                name: "Data 世界".into(),
                visibility: SheetVisibility::Visible,
                cells: vec![
                    cell(1, 1, CellValue::Text("identifier".into())),
                    cell(1, 2, CellValue::Text("value".into())),
                    cell(2, 1, CellValue::Text("00123".into())),
                    cell(2, 2, CellValue::Number("123.50".into())),
                    cell(3, 1, CellValue::Text("=SUM(A1:A2)".into())),
                    cell(3, 2, CellValue::Boolean(true)),
                    cell(
                        4,
                        1,
                        CellValue::Text("literal _x0041_ / 👋🏽\r\n<&>\"".into()),
                    ),
                    cell(4, 2, CellValue::Blank),
                    Cell {
                        row: 5,
                        column: 2,
                        value: CellValue::Number("45000.25".into()),
                        number_format: Some(NumberFormat {
                            id: 164,
                            code: Some("yyyy-mm-dd hh:mm".into()),
                        }),
                    },
                    cell(6, 2, CellValue::Error("#N/A".into())),
                ],
            },
            Sheet {
                name: "Hidden".into(),
                visibility: SheetVisibility::Hidden,
                cells: vec![cell(3, 3, CellValue::Text("retained".into()))],
            },
        ],
    };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::write(
        dir.join("cells.xlsx"),
        write_workbook(&book, &Limits::default()).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("cells.expected.json"),
        serde_json::to_vec_pretty(&book).unwrap(),
    )
    .unwrap();
}
