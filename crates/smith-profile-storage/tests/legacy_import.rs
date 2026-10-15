#[path = "support/legacy_fixture.rs"]
mod fixture;
use smith_profile_storage::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
const MASTER: &str = "1111111111111111111111111111111111111111111111111111111111111111";
struct F {
    _temp: tempfile::TempDir,
    source: PathBuf,
    managed: PathBuf,
    destination: PathBuf,
}
impl F {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(t.path()).unwrap();
        let source = root.join("copy");
        fixture::create(&source, &[0x11; 32]);
        Self {
            _temp: t,
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
    fn keys(&self) -> LegacyKeys {
        LegacyKeys::from_master_hex(MASTER.into()).unwrap()
    }
    fn secret(&self) -> Secret {
        Secret::new("New synthetic app passphrase only".into()).unwrap()
    }
    fn import(&self) -> LegacyImportReport {
        import_legacy_copy(&self.request(), &self.keys(), &self.secret()).unwrap()
    }
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, p: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(p).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out)
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
fn original25_import_is_usable_exactly_archived_and_dormant() {
    let f = F::new();
    let before = tree(&f.source);
    let preflight = preflight_legacy_copy(&f.request(), &f.keys()).unwrap();
    assert_eq!(preflight.source_schema, 25);
    assert!(!f.destination.exists());
    let report = f.import();
    assert_eq!(report.mapped_messages, 6);
    assert_eq!(report.mapped_tasks, 1);
    assert_eq!(report.verified_blobs, 2);
    assert!(report.projection_warnings >= 2);
    assert_eq!(tree(&f.source), before);
    let db = unlock(&f.destination.join(DB_FILE), &f.secret()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT leaf_id FROM conversation_heads WHERE chat_id='session'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "latest"
    );
    assert_eq!(
        db.query_row(
            "SELECT parent_id FROM message_branches WHERE message_id='tool'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "branch1"
    );
    assert_eq!(
        db.query_row("SELECT status FROM messages WHERE id='tool'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "imported_reference"
    );
    assert_eq!(
        db.query_row(
            "SELECT original_status FROM legacy_message_details WHERE message_id='tool'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "complete"
    );
    assert_eq!(
        db.query_row("SELECT content FROM messages WHERE id='latest'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "Original selected answer retained."
    );
    assert_eq!(
        db.query_row(
            "SELECT bytes FROM legacy_artifacts WHERE id=?1",
            [fixture::ARTIFACT_ID],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        fixture::BLOB
    );
    let unknown: String = db
        .query_row(
            "SELECT payload_json FROM legacy_records WHERE table_name='original_unknown'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        unknown.contains("9223372036854775807")
            && unknown.contains("00ff0102")
            && unknown.contains("-51")
    );
    for table in [
        "settings",
        "agent_workspaces",
        "agent_tasks",
        "agent_memories",
    ] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        db.query_row("SELECT status FROM legacy_tasks", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "running"
    );
    close(db).unwrap();
    assert!(
        verify_legacy_import(&f.destination, &f.secret(), true)
            .unwrap()
            .baseline_unchanged
    );
    assert!(
        check_legacy_rollback(&f.destination, &f.source, &f.managed, &f.secret(), true)
            .unwrap()
            .eligible
    );
    for bytes in tree(&f.destination).values() {
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        for needle in [
            b"SENSITIVE_CHAT_SENTINEL".as_slice(),
            fixture::BLOB,
            b"New synthetic app passphrase only",
            MASTER.as_bytes(),
        ] {
            assert!(!bytes.windows(needle.len()).any(|w| w == needle));
        }
    }
    assert_eq!(tree(&f.source), before);
}
#[test]
fn original_import_refuses_later_writes_even_when_reverted() {
    let f = F::new();
    f.import();
    let db = unlock(&f.destination.join(DB_FILE), &f.secret()).unwrap();
    db.execute_batch("BEGIN;INSERT INTO settings VALUES('rolled','back');ROLLBACK;")
        .unwrap();
    close(db).unwrap();
    assert!(
        check_legacy_rollback(&f.destination, &f.source, &f.managed, &f.secret(), true)
            .unwrap()
            .eligible
    );
    let db = unlock(&f.destination.join(DB_FILE), &f.secret()).unwrap();
    db.execute_batch(
        "INSERT INTO settings VALUES('later','write');DELETE FROM settings WHERE key='later';",
    )
    .unwrap();
    close(db).unwrap();
    let check =
        check_legacy_rollback(&f.destination, &f.source, &f.managed, &f.secret(), true).unwrap();
    assert!(!check.eligible);
    assert_eq!(check.later_writes, 2);
    assert!(f.destination.join(DB_FILE).is_file());
}
#[test]
fn wrong_keys_corrupt_blobs_future_schema_and_existing_destinations_do_not_reset() {
    for case in 0..5 {
        let f = F::new();
        let old = tree(&f.source);
        let keys = match case {
            0 => LegacyKeys::from_master_hex("22".repeat(32)).unwrap(),
            1 => LegacyKeys::from_derived(fixture::derive(&[0x11; 32]).0, "00".repeat(32)).unwrap(),
            2 => {
                fs::write(f.managed.join(fixture::BLOB_PATH), b"corrupt sealed blob").unwrap();
                f.keys()
            }
            3 => {
                let db = fixture::connection(&f.source.join("smith.db"), &[0x11; 32]);
                db.pragma_update(None, "user_version", 26).unwrap();
                drop(db);
                f.keys()
            }
            _ => {
                fs::create_dir(&f.destination).unwrap();
                fs::write(f.destination.join("retained"), b"retained").unwrap();
                f.keys()
            }
        };
        let before = tree(&f.source);
        assert!(import_legacy_copy(&f.request(), &keys, &f.secret()).is_err());
        assert_eq!(tree(&f.source), before);
        if case == 4 {
            assert_eq!(
                fs::read(f.destination.join("retained")).unwrap(),
                b"retained"
            )
        } else {
            assert!(!f.destination.exists())
        }
        if case < 2 {
            assert_eq!(before, old)
        }
    }
}
#[test]
fn original_archive_verification_and_rollback_fail_closed() {
    let f = F::new();
    f.import();
    let before = tree(&f.destination);
    assert!(verify_legacy_import(
        &f.destination,
        &Secret::new("wrong passphrase".into()).unwrap(),
        true
    )
    .is_err());
    assert_eq!(tree(&f.destination), before);
    fs::write(f.source.join("new-evidence"), b"later").unwrap();
    assert!(
        !check_legacy_rollback(&f.destination, &f.source, &f.managed, &f.secret(), true)
            .unwrap()
            .eligible
    );
    fs::remove_file(f.source.join("new-evidence")).unwrap();
    fs::write(f.destination.join("unrecognized"), b"retain").unwrap();
    assert!(verify_legacy_import(&f.destination, &f.secret(), true).is_err());
    fs::remove_file(f.destination.join("unrecognized")).unwrap();
    fs::remove_file(f.destination.join("import.db")).unwrap();
    assert!(
        check_legacy_rollback(&f.destination, &f.source, &f.managed, &f.secret(), true).is_err()
    );
    assert!(f.destination.join(DB_FILE).exists());
}
#[test]
fn explicit_derived_credentials_match_original_contract() {
    let f = F::new();
    let (database, blob) = fixture::derive(&[0x11; 32]);
    let keys =
        LegacyKeys::from_derived(database, blob.iter().map(|b| format!("{b:02x}")).collect())
            .unwrap();
    let report = import_legacy_copy(&f.request(), &keys, &f.secret()).unwrap();
    assert_eq!(report.verified_blobs, 2);
}

#[test]
fn original_fractional_milliseconds_are_preserved_and_invalid_dates_refused() {
    let f = F::new();
    let db = fixture::connection(&f.source.join("smith.db"), &[0x11; 32]);
    db.execute_batch("UPDATE sessions SET created_at='2026-01-01T00:00:00.123Z',updated_at='2026-01-01T00:00:01.789Z';UPDATE messages SET created_at='2026-01-01T00:00:01.789Z' WHERE id='latest';").unwrap();
    drop(db);
    f.import();
    let db = unlock(&f.destination.join(DB_FILE), &f.secret()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT created_at%1000 FROM chats WHERE id='session'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        123
    );
    assert_eq!(
        db.query_row(
            "SELECT created_at%1000 FROM messages WHERE id='latest'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        789
    );
    close(db).unwrap();
    let f = F::new();
    let db = fixture::connection(&f.source.join("smith.db"), &[0x11; 32]);
    db.execute("UPDATE sessions SET created_at='not a date'", [])
        .unwrap();
    drop(db);
    assert!(import_legacy_copy(&f.request(), &f.keys(), &f.secret()).is_err());
    assert!(!f.destination.exists());
}

#[test]
fn final_import_destination_cannot_overlap_registered_external_store() {
    let f = F::new();
    let db = fixture::connection(&f.source.join("smith.db"), &[0x11; 32]);
    db.execute(
        "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
        [f.destination.to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let before = tree(&f.source);
    assert!(preflight_legacy_copy(&f.request(), &f.keys()).is_err());
    assert!(import_legacy_copy(&f.request(), &f.keys(), &f.secret()).is_err());
    assert_eq!(tree(&f.source), before);
    assert!(!f.destination.exists());
}
