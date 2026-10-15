//! Independent acceptance checks. All input data and keys are synthetic.
#[path = "support/legacy_fixture.rs"]
mod fixture;

use serde_json::{json, Value};
use smith_profile_storage::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

struct Case {
    _temp: tempfile::TempDir,
    source: PathBuf,
    managed: PathBuf,
    destination: PathBuf,
}
impl Case {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let source = root.join("synthetic-original-copy");
        fixture::create(&source, &[0x11; 32]);
        Self {
            _temp: temp,
            managed: source.join("artifacts"),
            source,
            destination: root.join("imported"),
        }
    }
    fn request(&self) -> LegacyImportRequest<'_> {
        LegacyImportRequest {
            source_copy: &self.source,
            managed_copy: &self.managed,
            destination: &self.destination,
            offline_copy_attested: true,
        }
    }
    fn secret(&self) -> Secret {
        Secret::new("Independent synthetic new passphrase".into()).unwrap()
    }
    fn keys(&self) -> LegacyKeys {
        LegacyKeys::from_master_hex("11".repeat(32)).unwrap()
    }
    fn import(&self) {
        import_legacy_copy(&self.request(), &self.keys(), &self.secret()).unwrap();
    }
}
fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(path).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(p.strip_prefix(root).unwrap().into(), fs::read(p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn offset_milliseconds_and_original_timestamp_spelling_survive_projection() {
    let c = Case::new();
    let source = fixture::connection(&c.source.join("smith.db"), &[0x11; 32]);
    source.execute_batch("UPDATE sessions SET created_at='2026-01-01T03:00:00.123+03:00',updated_at='2026-01-01T00:00:00.789Z'; UPDATE messages SET created_at='1969-12-31T23:59:59.999Z' WHERE id='root'; UPDATE artifacts SET created_at='2026-01-01T00:00:00.456Z' WHERE store_id='managed';").unwrap();
    drop(source);
    let before = bytes(&c.source);
    c.import();
    let db = unlock(&c.destination.join(DB_FILE), &c.secret()).unwrap();
    let times: (i64, i64) = db
        .query_row(
            "SELECT created_at,updated_at FROM chats WHERE id='session'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(times, (1767225600123, 1767225600789));
    assert_eq!(
        db.query_row("SELECT created_at FROM messages WHERE id='root'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        -1
    );
    assert_eq!(
        db.query_row(
            "SELECT created_at FROM artifacts WHERE id=?1",
            [fixture::ARTIFACT_ID],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1767225600456
    );
    assert_eq!(
        db.query_row(
            "SELECT original_created_at FROM legacy_message_details WHERE message_id='root'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "1969-12-31T23:59:59.999Z"
    );
    close(db).unwrap();
    assert_eq!(bytes(&c.source), before);
}

#[test]
fn unknown_sqlite_types_are_exact_and_imported_authority_is_dormant() {
    let c = Case::new();
    let source = fixture::connection(&c.source.join("smith.db"), &[0x11; 32]);
    source.execute_batch("CREATE TABLE reviewer_values(i INTEGER,r REAL,t TEXT,b BLOB,n); INSERT INTO reviewer_values(rowid,i,r,t,b,n) VALUES(-987,-9223372036854775808,0.1,CAST(X'80FF00' AS TEXT),X'000102FF',NULL);").unwrap();
    drop(source);
    c.import();
    let db = unlock(&c.destination.join(DB_FILE), &c.secret()).unwrap();
    let raw: String = db
        .query_row(
            "SELECT payload_json FROM legacy_records WHERE table_name='reviewer_values'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["columns"], json!(["rowid", "i", "r", "t", "b", "n"]));
    assert_eq!(
        value["values"],
        json!([
            {"type":"integer","value":"-987"},
            {"type":"integer","value":"-9223372036854775808"},
            {"type":"real_bits","value":"9a9999999999b93f"},
            {"type":"text_bytes","value":"80ff00"},
            {"type":"blob","value":"000102ff"},
            {"type":"null"}
        ])
    );
    for table in [
        "settings",
        "agent_workspaces",
        "agent_tasks",
        "agent_tool_receipts",
        "agent_memories",
    ] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{table}"
        );
    }
    assert_eq!(
        db.query_row("SELECT status FROM messages WHERE id='tool'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "imported_reference"
    );
    assert_eq!(db.query_row("SELECT count(*) FROM legacy_records WHERE table_name='sqlite_schema' AND record_id='reviewer_values'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    close(db).unwrap();
    assert!(
        verify_legacy_import(&c.destination, &c.secret(), true)
            .unwrap()
            .baseline_unchanged
    );
}

#[test]
fn wrong_key_cannot_publish_and_committed_write_revert_cannot_rollback() {
    let c = Case::new();
    let before = bytes(&c.source);
    let wrong = LegacyKeys::from_master_hex("22".repeat(32)).unwrap();
    assert!(import_legacy_copy(&c.request(), &wrong, &c.secret()).is_err());
    assert!(!c.destination.exists());
    assert_eq!(bytes(&c.source), before);
    c.import();
    let db = unlock(&c.destination.join(DB_FILE), &c.secret()).unwrap();
    db.execute_batch("UPDATE legacy_tasks SET input=input||'temporary'; UPDATE legacy_tasks SET input=substr(input,1,length(input)-9);").unwrap();
    close(db).unwrap();
    let check =
        check_legacy_rollback(&c.destination, &c.source, &c.managed, &c.secret(), true).unwrap();
    assert!(!check.eligible);
    assert!(c.destination.is_dir());
    assert_eq!(bytes(&c.source), before);
}
