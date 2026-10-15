//! Synthetic historical archives: retain the schema-6/7/8/9/10 destination record and
//! source hashes, then exercise the real verifier before and after upgrade.
use super::*;
use serde_json::{json, Value};

#[path = "legacy_fixture.rs"]
mod fixture;

fn secret() -> Secret {
    Secret::new("Synthetic schema-history review key".into()).unwrap()
}

fn read_manifest(root: &Path, vault: &str) -> Value {
    let db = open_cipher(&root.join(vault), &secret(), false).unwrap();
    let payload: String = db
        .query_row("SELECT payload FROM manifest WHERE id=1", [], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

fn write_manifest(root: &Path, vault: &str, manifest: &Value) {
    let db = open_cipher(&root.join(vault), &secret(), false).unwrap();
    db.execute(
        "UPDATE manifest SET payload=?1 WHERE id=1",
        [manifest.to_string()],
    )
    .unwrap();
    crate::close(db).unwrap();
}

/// Fixture construction only. Create the exact historical table set, preserve the
/// profile identity, install its normal audit triggers, and archive its actual
/// logical record. This never labels current-schema bytes as an older baseline.
fn archive_historical(root: &Path, vault: &str, destination_field: &str, version: i64) -> Value {
    assert!(matches!(version, 6..=10));
    let db = open_cipher(&root.join(DB_FILE), &secret(), false).unwrap();
    let (identity, generation) = schema::identity(&db).unwrap();
    let triggers = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND name GLOB '_smith_*'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    for trigger in triggers {
        db.execute_batch(&format!("DROP TRIGGER {}", schema::quote(&trigger)))
            .unwrap();
    }
    for (table, _) in crate::schema_v11::REQUIRED {
        db.execute_batch(&format!("DROP TABLE {}", schema::quote(table)))
            .unwrap();
    }
    if version < 10 {
        for (table, _) in crate::schema_v10::REQUIRED.iter().rev() {
            db.execute_batch(&format!("DROP TABLE {}", schema::quote(table)))
                .unwrap();
        }
    }
    if version < 9 {
        for (table, _) in crate::schema_v9::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", schema::quote(table)))
                .unwrap();
        }
    }
    if version < 8 {
        for (table, _) in crate::schema_v8::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", schema::quote(table)))
                .unwrap();
        }
    }
    if version < 7 {
        for (table, _) in crate::schema_v7::REQUIRED.iter().rev() {
            if *table != "agent_workspaces" {
                db.execute_batch(&format!("DROP TABLE {}", schema::quote(table)))
                    .unwrap();
            }
        }
        db.execute_batch("ALTER TABLE agent_workspaces DROP COLUMN resources")
            .unwrap();
    }
    db.execute_batch("DROP TABLE _smith_storage;").unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
    if version >= 7 {
        db.execute("INSERT INTO agent_memories(id,principal_id,namespace,content,tags_json,created_at,updated_at,version) SELECT '00000000-0000-4000-8000-000000000009',principal_id,'review','Historical memory — preserved exactly','[\"history\"]',1,2,7 FROM agent_profile WHERE id=1",[]).unwrap();
    }
    if version >= 8 {
        // Clearing feedback is a persisted revision tombstone, not a missing row.
        db.execute("INSERT INTO message_feedback(message_id,kind,revision,updated_at) SELECT id,'',4,2 FROM messages WHERE role='assistant' ORDER BY id LIMIT 1",[]).unwrap();
    }
    if version >= 9 {
        // Schema 9 preserves both pin preference and recoverable Trash. Upgrade
        // must not resurrect or unpin this exact historical memory record.
        db.execute("INSERT INTO memory_metadata(memory_id,pinned,deleted,updated_at) VALUES('00000000-0000-4000-8000-000000000009',1,1,2)",[]).unwrap();
    }
    if version >= 10 {
        db.execute("INSERT INTO agent_extensions(id,principal_id,kind,revision,body_json,enabled,deleted,provenance_json,created_at,updated_at) SELECT 'retained-extension',principal_id,'skill',5,'{}',0,1,'{}',1,2 FROM agent_profile", []).unwrap();
    }
    if vault == "import.db" {
        let reports = db
            .prepare("SELECT id,report_json FROM legacy_imports")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        for (id, report) in reports {
            let mut report: Value = serde_json::from_str(&report).unwrap();
            report["target_schema"] = json!(version);
            db.execute(
                "UPDATE legacy_imports SET report_json=?2 WHERE id=?1",
                params![id, report.to_string()],
            )
            .unwrap();
        }
    }
    schema::install_storage(&db).unwrap();
    db.execute(
        "UPDATE _smith_storage SET profile_id=?1,write_generation=?2 WHERE id=1",
        params![identity, generation],
    )
    .unwrap();
    schema::validate(&db).unwrap();
    let record = migration::database_record(&db).unwrap();
    crate::close(db).unwrap();
    let mut manifest = read_manifest(root, vault);
    manifest["report"]["target_schema"] = json!(version);
    manifest[destination_field] = serde_json::to_value(record).unwrap();
    manifest["baseline_generation"] = json!(generation);
    write_manifest(root, vault, &manifest);
    manifest
}

fn assert_upgraded_history(db: &Connection, version: i64) {
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    if version >= 7 {
        let memory: (String, i64, bool, bool) = db.query_row("SELECT m.content,m.version,COALESCE(s.pinned,0),COALESCE(s.deleted,0) FROM agent_memories m LEFT JOIN memory_metadata s ON s.memory_id=m.id WHERE m.id='00000000-0000-4000-8000-000000000009'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            memory,
            (
                "Historical memory — preserved exactly".into(),
                7,
                version >= 9,
                version >= 9
            )
        );
    } else {
        assert_eq!(
            db.query_row("SELECT count(*) FROM agent_memories", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM memory_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        if version >= 9 { 1 } else { 0 }
    );
    if version >= 8 {
        let feedback: (String, i64) = db
            .query_row("SELECT kind,revision FROM message_feedback", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(feedback, (String::new(), 4));
    }
    if version >= 10 {
        let extension:(i64,i64,i64)=db.query_row("SELECT revision,enabled,deleted FROM agent_extensions WHERE id='retained-extension'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(extension, (5, 0, 1));
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM run_model_provenance", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    schema::validate(db).unwrap();
}

fn original_import_history(version: i64) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("original");
    fixture::create(&source, &[0x11; 32]);
    let managed = source.join("artifacts");
    let destination = root.join("imported");
    let request = LegacyImportRequest {
        source_copy: &source,
        managed_copy: &managed,
        destination: &destination,
        offline_copy_attested: true,
    };
    let keys = LegacyKeys::from_master_hex("11".repeat(32)).unwrap();
    let created = import_legacy_copy(&request, &keys, &secret()).unwrap();
    assert_eq!(created.target_schema, SCHEMA_VERSION);
    assert!(
        verify_legacy_import(&destination, &secret(), true)
            .unwrap()
            .baseline_unchanged
    );

    let archived = archive_historical(&destination, "import.db", "destination", version);
    let original_archive = files::record(&destination.join("import.db")).unwrap();
    assert_eq!(
        verify_legacy_import(&destination, &secret(), true)
            .unwrap()
            .target_schema,
        version
    );
    assert!(
        check_legacy_rollback(&destination, &source, &managed, &secret(), true)
            .unwrap()
            .eligible
    );
    let upgraded = crate::unlock(&destination.join(DB_FILE), &secret()).unwrap();
    assert_upgraded_history(&upgraded, version);
    crate::close(upgraded).unwrap();
    let checked = verify_legacy_import(&destination, &secret(), true).unwrap();
    assert_eq!(checked.target_schema, version);
    assert_eq!(checked.later_writes, 1);
    assert!(!checked.baseline_unchanged);
    assert!(
        !check_legacy_rollback(&destination, &source, &managed, &secret(), true)
            .unwrap()
            .eligible
    );
    assert_eq!(
        files::record(&destination.join("import.db")).unwrap(),
        original_archive
    );

    let mut invalid = archived.clone();
    invalid["report"]["target_schema"] = json!(12);
    write_manifest(&destination, "import.db", &invalid);
    assert!(verify_legacy_import(&destination, &secret(), true).is_err());
    let mut changed_hash = archived;
    let record = changed_hash["original"]["source_files"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    record["sha256"] = json!("0".repeat(64));
    write_manifest(&destination, "import.db", &changed_hash);
    assert!(verify_legacy_import(&destination, &secret(), true).is_err());
}

fn next_migration_history(version: i64) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("plain");
    std::fs::create_dir(&source).unwrap();
    let plain = Connection::open(source.join(crate::PLAINTEXT_FILE)).unwrap();
    plain.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL);CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL);INSERT INTO chats VALUES('chat','Synthetic historical chat',1);INSERT INTO messages VALUES('answer','chat','assistant','Preserved exact answer','completed',NULL,1,NULL);PRAGMA user_version=1;").unwrap();
    drop(plain);
    let destination = root.join("migrated");
    let fresh = crate::migrate_plaintext_copy(
        &crate::MigrationRequest {
            source_copy: &source,
            destination: &destination,
            offline_copy_attested: true,
        },
        &secret(),
    )
    .unwrap();
    assert_eq!(fresh.target_schema, SCHEMA_VERSION);
    let archived = archive_historical(
        &destination,
        "migration.db",
        "destination_database",
        version,
    );
    let archive = files::record(&destination.join("migration.db")).unwrap();
    let checked = crate::verify_migration(&destination, &secret(), true).unwrap();
    assert_eq!(checked.target_schema, version);
    assert!(checked.baseline_unchanged);
    assert!(
        crate::check_rollback(&destination, &source, &secret(), true)
            .unwrap()
            .eligible
    );
    let db = crate::unlock(&destination.join(DB_FILE), &secret()).unwrap();
    assert_upgraded_history(&db, version);
    assert_eq!(
        db.query_row("SELECT content FROM messages WHERE id='answer'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "Preserved exact answer"
    );
    crate::close(db).unwrap();
    let checked = crate::verify_migration(&destination, &secret(), true).unwrap();
    assert_eq!(checked.target_schema, version);
    assert_eq!(checked.later_writes, 1);
    assert!(!checked.baseline_unchanged);
    assert!(
        !crate::check_rollback(&destination, &source, &secret(), true)
            .unwrap()
            .eligible
    );
    assert_eq!(
        files::record(&destination.join("migration.db")).unwrap(),
        archive
    );
    let mut future = archived.clone();
    future["report"]["target_schema"] = json!(12);
    write_manifest(&destination, "migration.db", &future);
    assert!(crate::verify_migration(&destination, &secret(), true).is_err());
    let mut changed_hash = archived;
    let record = changed_hash["source_files"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    record["sha256"] = json!("0".repeat(64));
    write_manifest(&destination, "migration.db", &changed_hash);
    assert!(crate::verify_migration(&destination, &secret(), true).is_err());
}

#[test]
fn original_import_archived7_keeps_exact_history_and_hash_checks() {
    original_import_history(7);
}

#[test]
fn original_import_archived8_keeps_exact_history_and_hash_checks() {
    original_import_history(8);
}

#[test]
fn original_import_archived9_keeps_exact_history_and_hash_checks() {
    original_import_history(9);
}

#[test]
fn next_migration_archived6_verifies_without_relabeling_or_losing_audit() {
    next_migration_history(6);
}

#[test]
fn next_migration_archived7_verifies_without_relabeling_or_losing_audit() {
    next_migration_history(7);
}

#[test]
fn next_migration_archived8_verifies_without_relabeling_or_losing_audit() {
    next_migration_history(8);
}

#[test]
fn next_migration_archived9_verifies_without_relabeling_or_losing_audit() {
    next_migration_history(9);
}

#[test]
fn original_import_archived10_keeps_exact_history_and_hash_checks() {
    original_import_history(10);
}

#[test]
fn next_migration_archived10_verifies_without_relabeling_or_losing_audit() {
    next_migration_history(10);
}
