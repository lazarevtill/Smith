//! Reviewer-owned, in-memory mutations of synthetic document fixtures only.
use smith_media::{read_spreadsheet, CellValue, Format, Limits};

#[test]
fn xls_boundsheet_order_must_not_reassign_physical_sheet_coordinates() {
    let original = include_bytes!("fixtures/passive.xls");
    let stream = &original[512..4608];
    let mut at = 0usize;
    let mut bound_at = None;
    let mut worksheet_at = None;
    let mut worksheet_end = None;
    while at + 4 <= stream.len() {
        let kind = u16::from_le_bytes(stream[at..at + 2].try_into().unwrap());
        let len = u16::from_le_bytes(stream[at + 2..at + 4].try_into().unwrap()) as usize;
        if kind == 0x85 {
            bound_at = Some(at);
        }
        if kind == 0x809 && at > 0 {
            worksheet_at = Some(at);
        }
        at += 4 + len;
        if kind == 0xa && worksheet_at.is_some() {
            worksheet_end = Some(at);
            break;
        }
    }
    let (bound_at, worksheet_at, worksheet_end) = (
        bound_at.unwrap(),
        worksheet_at.unwrap(),
        worksheet_end.unwrap(),
    );
    let bound_len = worksheet_at - bound_at - 4;
    let mut first_bound = stream[bound_at..bound_at + bound_len].to_vec();
    let first_sheet = &stream[worksheet_at..worksheet_end];
    let mut second_bound = first_bound.clone();
    let mut second_sheet = first_sheet.to_vec();
    // Keep physical order Data, Next, but advertise tab order Next, Data.
    let first_offset = worksheet_at + bound_len;
    let second_offset = first_offset + first_sheet.len();
    first_bound[4..8].copy_from_slice(&(first_offset as u32).to_le_bytes());
    second_bound[4..8].copy_from_slice(&(second_offset as u32).to_le_bytes());
    second_bound[12..16].copy_from_slice(b"Next");
    let mut at = 0;
    while at + 4 <= second_sheet.len() {
        let kind = u16::from_le_bytes(second_sheet[at..at + 2].try_into().unwrap());
        let len = u16::from_le_bytes(second_sheet[at + 2..at + 4].try_into().unwrap()) as usize;
        if matches!(kind, 0xfd | 0x203 | 0x205 | 0x201) {
            let row = u16::from_le_bytes(second_sheet[at + 4..at + 6].try_into().unwrap()) + 2;
            second_sheet[at + 4..at + 6].copy_from_slice(&row.to_le_bytes());
        }
        if kind == 0x200 {
            second_sheet[at + 4..at + 8].copy_from_slice(&2u32.to_le_bytes());
            second_sheet[at + 8..at + 12].copy_from_slice(&4u32.to_le_bytes());
        }
        at += 4 + len;
    }
    let mut rebuilt = stream[..bound_at].to_vec();
    rebuilt.extend(second_bound);
    rebuilt.extend(first_bound);
    rebuilt.extend_from_slice(&[0x0a, 0, 0, 0]);
    rebuilt.extend_from_slice(first_sheet);
    rebuilt.extend(second_sheet);
    assert!(rebuilt.len() < 4096);
    let mut bytes = original.to_vec();
    bytes[512..4608].fill(0);
    bytes[512..512 + rebuilt.len()].copy_from_slice(&rebuilt);
    let workbook = read_spreadsheet(Format::Xls, &bytes, &Limits::default()).unwrap();
    assert_eq!(workbook.sheets[0].name, "Next");
    assert_eq!(workbook.sheets[1].name, "Data");
    let next = workbook.sheets[0]
        .cells
        .iter()
        .find(|c| c.row == 3 && c.column == 1);
    assert_eq!(
        next.map(|c| &c.value),
        Some(&CellValue::Text("00123".into()))
    );
    let data = workbook.sheets[1]
        .cells
        .iter()
        .find(|c| c.row == 1 && c.column == 1);
    assert_eq!(
        data.map(|c| &c.value),
        Some(&CellValue::Text("00123".into()))
    );
}
