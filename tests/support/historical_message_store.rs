use super::*;
use contract::{ExactRow, ExactValue, ResultLink};
use serde_json::json;
#[path = "historical_message_source.rs"]
mod selected_source_tests;

fn db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE legacy_imports(id TEXT PRIMARY KEY,report_json TEXT NOT NULL); CREATE TABLE legacy_records(table_name TEXT NOT NULL,row_number INTEGER NOT NULL,record_id TEXT,payload_json TEXT NOT NULL,PRIMARY KEY(table_name,row_number)); CREATE INDEX identities ON legacy_records(table_name,record_id); CREATE TABLE audit(generation INTEGER NOT NULL); INSERT INTO audit VALUES(0); CREATE TRIGGER original_changes AFTER UPDATE ON legacy_records BEGIN UPDATE audit SET generation=generation+1; END;").unwrap();
    db.execute(
        "INSERT INTO legacy_imports VALUES('synthetic-import',?1)",
        [json!({"import_id":"synthetic-import","source_schema":25}).to_string()],
    )
    .unwrap();
    db
}
// Keep the synthetic archive columns explicit at each insertion.
#[allow(clippy::too_many_arguments)]
fn put(
    db: &Connection,
    row: i64,
    id: &str,
    role: &str,
    seq: i64,
    parent: Option<&str>,
    calls: &str,
    images: &str,
) -> String {
    let value = ExactRow {
        columns: [
            "rowid",
            "id",
            "session_id",
            "role",
            "seq",
            "parent_id",
            "tool_calls",
            "tool_call_id",
            "images",
            "status",
            "future",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        values: vec![
            ExactValue::Integer((-row - 19).to_string()),
            ExactValue::Text(id.into()),
            ExactValue::Text("session".into()),
            ExactValue::Text(role.into()),
            ExactValue::Integer(seq.to_string()),
            parent
                .map(|s| ExactValue::Text(s.into()))
                .unwrap_or(ExactValue::Null),
            ExactValue::Text(calls.into()),
            ExactValue::Text("call".into()),
            ExactValue::Text(images.into()),
            ExactValue::Text("cancelled".into()),
            ExactValue::TextBytes("fffe".into()),
        ],
    };
    let raw = serde_json::to_string(&value).unwrap();
    db.execute(
        "INSERT INTO legacy_records VALUES('messages',?1,?2,?3)",
        params![row, id, raw],
    )
    .unwrap();
    raw
}
const CALL: &str = r#"[{"id":"call","name":"historical_only","arguments":"{literal"}]"#;

#[test]
fn historical_store_read_snapshot_preserves_original_rows_and_does_not_write() {
    let db = db();
    put(&db, 2, "parent", "assistant", 2, None, CALL, "[]");
    let raw = put(&db, 7, "result", "tool", 3, Some("parent"), "[]", "[]");
    let before = db.total_changes();
    let snapshot = capture(&db, 7, None).unwrap();
    assert!(db.is_autocommit());
    assert_eq!(db.total_changes(), before);
    assert_eq!(
        db.query_row("SELECT generation FROM audit", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let cards = snapshot.cards().unwrap();
    assert!(matches!(cards.result_link,ResultLink::Unique{call,..} if call.message.row_number==2));
    assert_eq!(
        cards.original_status,
        Some(ExactValue::Text("cancelled".into()))
    );
    assert_eq!(
        snapshot.original().values[0],
        ExactValue::Integer("-26".into())
    );
    assert_eq!(
        snapshot.original().values.last(),
        Some(&ExactValue::TextBytes("fffe".into()))
    );
    let after: String = db
        .query_row(
            "SELECT payload_json FROM legacy_records WHERE row_number=7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, raw);
    assert_eq!(
        snapshot.source().payload_sha256,
        projection::digest(raw.as_bytes())
    );
}

#[test]
fn historical_store_duplicate_or_missing_ancestors_are_never_guessed() {
    let db = db();
    put(&db, 1, "parent", "assistant", 1, None, CALL, "[]");
    put(&db, 2, "parent", "assistant", 2, None, CALL, "[]");
    put(&db, 3, "result", "tool", 3, Some("parent"), "[]", "[]");
    assert_eq!(
        capture(&db, 3, None).unwrap().cards().unwrap().result_link,
        ResultLink::Unresolved {
            reason: Reason::DuplicateIdentity
        }
    );
    put(&db, 4, "missing", "tool", 4, Some("absent"), "[]", "[]");
    assert_eq!(
        capture(&db, 4, None).unwrap().cards().unwrap().result_link,
        ResultLink::Unresolved {
            reason: Reason::MissingAncestor
        }
    );
}

#[test]
fn historical_store_only_traverses_the_selected_ancestry_not_siblings() {
    let db = db();
    put(&db, 1, "root", "user", 1, None, "[]", "[]");
    put(&db, 2, "sibling", "assistant", 2, Some("root"), CALL, "[]");
    put(&db, 3, "result", "tool", 3, Some("root"), "[]", "[]");
    assert_eq!(
        capture(&db, 3, None).unwrap().cards().unwrap().result_link,
        ResultLink::Unmatched {
            call_id: "call".into()
        }
    );
}

#[test]
fn historical_store_sql_length_gate_and_cumulative_budget_refuse_oversized_rows() {
    let db = db();
    db.execute(
        "INSERT INTO legacy_records VALUES('messages',1,'oversized',CAST(zeroblob(?1) AS TEXT))",
        [contract::MAX_ARCHIVE_ROW as i64 + 1],
    )
    .unwrap();
    let before = db.total_changes();
    assert!(capture(&db, 1, None).is_err());
    assert_eq!(db.total_changes(), before);
    assert!(db.is_autocommit());
    put(
        &db,
        2,
        "parent",
        "assistant",
        2,
        None,
        CALL,
        &" ".repeat(contract::MAX_ANCESTOR_BYTES),
    );
    put(&db, 3, "result", "tool", 3, Some("parent"), "[]", "[]");
    assert_eq!(
        capture(&db, 3, None).unwrap().cards().unwrap().result_link,
        ResultLink::Unresolved {
            reason: Reason::AncestryLimit
        }
    );
}

#[test]
fn historical_store_each_byte_lookup_rechecks_full_source_and_import_identity() {
    use base64::Engine;
    let db = db();
    let bytes = include_bytes!("../../crates/smith-media/tests/fixtures/pixels.png");
    let images = json!([format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )])
    .to_string();
    let raw = put(&db, 1, "image", "user", 1, None, "[]", &images);
    let initial = capture(&db, 1, None).unwrap();
    let source = initial.source().clone();
    let fresh = capture(&db, 1, Some(&source)).unwrap();
    assert_eq!(fresh.embedded_image(&source, 0).unwrap().bytes, bytes);
    // Even a whitespace-only change to exact archived JSON invalidates its receipt.
    db.execute(
        "UPDATE legacy_records SET payload_json=?1 WHERE row_number=1",
        [format!("{raw} ")],
    )
    .unwrap();
    assert!(capture(&db, 1, Some(&source)).is_err());
    let changed = capture(&db, 1, None).unwrap();
    assert!(changed.embedded_image(&source, 0).is_err());
    db.execute(
        "UPDATE legacy_imports SET id='replacement',report_json=?1",
        [json!({"import_id":"replacement","source_schema":25}).to_string()],
    )
    .unwrap();
    assert!(capture(&db, 1, Some(changed.source())).is_err());
}

#[test]
fn historical_store_rejects_malformed_final_import_without_reset_or_joining_write_tx() {
    let db = db();
    put(&db, 1, "message", "user", 1, None, "[]", "[]");
    let tx = db.unchecked_transaction().unwrap();
    assert!(capture(&tx, 1, None).is_err());
    tx.rollback().unwrap();
    db.execute("UPDATE legacy_imports SET report_json='{}'", [])
        .unwrap();
    let before = db.total_changes();
    assert!(capture(&db, 1, None).is_err());
    assert_eq!(db.total_changes(), before);
    assert!(db.is_autocommit());
    db.execute(
        "UPDATE legacy_imports SET report_json=?1",
        [json!({"import_id":"synthetic-import","source_schema":25}).to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO legacy_imports VALUES('second',?1)",
        [json!({"import_id":"second","source_schema":25}).to_string()],
    )
    .unwrap();
    assert!(capture(&db, 1, None).is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM legacy_records", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
