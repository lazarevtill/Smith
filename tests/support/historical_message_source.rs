use super::*;
use contract::MessageSelection;

fn setup() -> (Connection, MessageSelection, String) {
    let db = db();
    db.execute_batch("CREATE TABLE chats(id TEXT PRIMARY KEY,version INTEGER); CREATE TABLE conversation_heads(chat_id TEXT PRIMARY KEY,leaf_id TEXT); CREATE TABLE chat_trash(chat_id TEXT); CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT,role TEXT,status TEXT,run_id TEXT); CREATE TABLE message_branches(message_id TEXT PRIMARY KEY,parent_id TEXT); CREATE TABLE legacy_message_details(message_id TEXT PRIMARY KEY,original_status TEXT,original_seq INTEGER); INSERT INTO chats VALUES('session',4); INSERT INTO conversation_heads VALUES('session','tip'); INSERT INTO messages VALUES('selected','session','assistant','imported_reference',NULL),('tip','session','assistant','completed','new-run'); INSERT INTO message_branches VALUES('selected',NULL),('tip','selected'); INSERT INTO legacy_message_details VALUES('selected','cancelled',37);").unwrap();
    let raw = put(&db, 91, "selected", "assistant", 37, None, CALL, "[]");
    let selection = MessageSelection {
        chat_id: "session".into(),
        chat_version: 4,
        active_leaf: "tip".into(),
        message_id: "selected".into(),
    };
    (db, selection, raw)
}

#[test]
fn selected_source_exact_receipt_distinguishes_archive_row_sequence_and_current_leaf() {
    let (db, selection, raw) = setup();
    let before = db.total_changes();
    let value = resolve(&db, &selection).unwrap();
    assert_eq!(value.selection, selection);
    assert_eq!(value.original_source.source.row_number, 91);
    assert_eq!(
        value.original_source.source.payload_sha256,
        projection::digest(raw.as_bytes())
    );
    assert_eq!(value.original_source.original_message_id, "selected");
    assert_eq!(value.original_source.original_session_id, "session");
    assert_eq!(value.message_status, "imported_reference");
    assert_eq!(db.total_changes(), before);
    assert!(db.is_autocommit());
}

#[test]
fn selected_source_refuses_stale_revision_head_chat_and_trashed_chat() {
    let (db, selected, _) = setup();
    for selection in [
        MessageSelection {
            chat_version: 3,
            ..selected.clone()
        },
        MessageSelection {
            active_leaf: "selected".into(),
            ..selected.clone()
        },
        MessageSelection {
            chat_id: "other".into(),
            ..selected.clone()
        },
    ] {
        assert!(resolve(&db, &selection).is_err());
    }
    db.execute("INSERT INTO chat_trash VALUES('session')", [])
        .unwrap();
    assert!(resolve(&db, &selected).is_err());
}

#[test]
fn selected_source_never_substitutes_a_sibling_or_current_generated_message() {
    let (db, selected, _) = setup();
    db.execute_batch("INSERT INTO messages VALUES('sibling','session','assistant','completed',NULL); INSERT INTO message_branches VALUES('sibling',NULL); INSERT INTO legacy_message_details VALUES('sibling','cancelled',38);").unwrap();
    put(&db, 92, "sibling", "assistant", 38, None, "[]", "[]");
    assert!(resolve(
        &db,
        &MessageSelection {
            message_id: "sibling".into(),
            ..selected.clone()
        }
    )
    .is_err());
    assert!(resolve(
        &db,
        &MessageSelection {
            message_id: "tip".into(),
            ..selected.clone()
        }
    )
    .is_err());
    db.execute(
        "UPDATE messages SET run_id='unexpected-run' WHERE id='selected'",
        [],
    )
    .unwrap();
    assert!(resolve(&db, &selected).is_err());
}

#[test]
fn selected_source_requires_import_membership_and_unique_exact_archive_identity() {
    let (db, selected, _) = setup();
    put(&db, 92, "selected", "assistant", 37, None, CALL, "[]");
    assert!(resolve(&db, &selected).is_err());
    db.execute("DELETE FROM legacy_records WHERE row_number=92", [])
        .unwrap();
    assert!(resolve(&db, &selected).is_ok());
    db.execute("DELETE FROM legacy_message_details", [])
        .unwrap();
    assert!(resolve(&db, &selected).is_err());
}

#[test]
fn selected_source_checks_archived_id_session_role_status_and_sequence() {
    for field in ["id", "session_id", "role", "status", "seq"] {
        let (db, selected, raw) = setup();
        let mut row: ExactRow = serde_json::from_str(&raw).unwrap();
        let index = row.columns.iter().position(|c| c == field).unwrap();
        row.values[index] = if field == "seq" {
            ExactValue::Integer("38".into())
        } else {
            ExactValue::Text("different".into())
        };
        db.execute(
            "UPDATE legacy_records SET payload_json=?1 WHERE row_number=91",
            [serde_json::to_string(&row).unwrap()],
        )
        .unwrap();
        assert!(resolve(&db, &selected).is_err(), "{field}");
    }
}

#[test]
fn selected_source_refuses_incomplete_cross_chat_cyclic_and_non_earlier_paths() {
    for sql in [
        "UPDATE message_branches SET parent_id='missing' WHERE message_id='selected'",
        "UPDATE message_branches SET parent_id='tip' WHERE message_id='selected'",
        "UPDATE messages SET chat_id='different' WHERE id='selected'",
        "DELETE FROM message_branches WHERE message_id='selected'",
        "UPDATE messages SET rowid=99 WHERE id='selected'",
        "UPDATE message_branches SET parent_id=CAST(zeroblob(1025) AS TEXT) WHERE message_id='selected'",
    ] {
        let (db, selected, _) = setup();
        db.execute_batch(sql).unwrap();
        assert!(resolve(&db, &selected).is_err());
    }
}

#[test]
fn selected_source_bounds_archive_and_projected_metadata_before_allocation() {
    for sql in [
        "UPDATE legacy_records SET payload_json=CAST(zeroblob(16777217) AS TEXT)",
        "UPDATE legacy_records SET payload_json=X'0000'",
        "UPDATE messages SET status=CAST(zeroblob(1025) AS TEXT) WHERE id='selected'",
        "UPDATE legacy_message_details SET original_status=CAST(zeroblob(1025) AS TEXT)",
        "UPDATE legacy_message_details SET original_seq='not-an-integer'",
        "UPDATE messages SET status='streaming' WHERE id='selected'",
    ] {
        let (db, selected, _) = setup();
        db.execute_batch(sql).unwrap();
        let before = db.total_changes();
        assert!(resolve(&db, &selected).is_err());
        assert_eq!(before, db.total_changes());
        assert!(db.is_autocommit());
    }
}

#[test]
fn selected_source_requires_valid_selection_and_its_own_read_transaction() {
    let (db, selected, _) = setup();
    for invalid in ["".into(), "x".repeat(1025), "id\nvalue".into()] {
        assert!(resolve(
            &db,
            &MessageSelection {
                message_id: invalid,
                ..selected.clone()
            }
        )
        .is_err());
    }
    assert!(resolve(
        &db,
        &MessageSelection {
            chat_version: -1,
            ..selected.clone()
        }
    )
    .is_err());
    let tx = db.unchecked_transaction().unwrap();
    assert!(resolve(&tx, &selected).is_err());
    tx.rollback().unwrap();
    assert!(resolve(&db, &selected).is_ok());
}

#[test]
fn selected_source_bounds_complete_current_branch_even_after_finding_message() {
    let (db, mut selected, _) = setup();
    let tx = db.unchecked_transaction().unwrap();
    let mut parent = "tip".to_owned();
    for index in 0..contract::MAX_ANCESTORS {
        let id = format!("later-{index}");
        tx.execute(
            "INSERT INTO messages VALUES(?1,'session','assistant','completed',NULL)",
            [&id],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO message_branches VALUES(?1,?2)",
            params![id, parent],
        )
        .unwrap();
        parent = id;
    }
    tx.execute("UPDATE conversation_heads SET leaf_id=?1", [&parent])
        .unwrap();
    tx.commit().unwrap();
    selected.active_leaf = parent.clone();
    selected.message_id = parent.clone();
    db.execute(
        "INSERT INTO legacy_message_details VALUES(?1,'cancelled',38)",
        [&parent],
    )
    .unwrap();
    put(&db, 92, &parent, "assistant", 38, None, "[]", "[]");
    assert!(resolve(&db, &selected).is_err());
    // With a complete bounded branch the same mapped message must succeed.
    db.execute(
        "UPDATE message_branches SET parent_id=NULL WHERE message_id=?1",
        [&parent],
    )
    .unwrap();
    assert!(resolve(&db, &selected).is_ok());
}

#[test]
fn selected_source_hashes_exact_stored_bytes_and_new_snapshot_observes_changes() {
    let (db, selected, raw) = setup();
    let first = resolve(&db, &selected).unwrap();
    let different_bytes = format!(" {raw}\n");
    db.execute(
        "UPDATE legacy_records SET payload_json=?1",
        [&different_bytes],
    )
    .unwrap();
    let second = resolve(&db, &selected).unwrap();
    assert_ne!(
        first.original_source.source.payload_sha256,
        second.original_source.source.payload_sha256
    );
    assert_eq!(
        second.original_source.source.payload_sha256,
        projection::digest(different_bytes.as_bytes())
    );
    db.execute("UPDATE chats SET version=version+1", [])
        .unwrap();
    assert!(resolve(&db, &selected).is_err());
}
