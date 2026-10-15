use rusqlite::{params, Connection};
use smith_profile_storage::{
    check_rollback, close, create, initialize_schema, migrate_plaintext_copy, probe,
    recover_interrupted, unlock, verify_migration, MigrationRequest, ProfileKind, Secret, DB_FILE,
    PLAINTEXT_FILE,
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const PASSPHRASE: &str = "synthetic-only passphrase 🦀 ' never persist";
const PAYLOAD: &str = "SYNTHETIC_PRIVATE_TEXT_6d3643 unicode 🦀";
fn key() -> Secret {
    Secret::new(PASSPHRASE.into()).unwrap()
}

#[test]
fn provenance_rollback_counts_committed_intent_even_after_it_is_removed() {
    let (_temporary, root) = root();
    let source = root.join("plain");
    close(fixture(&source, 5)).unwrap();
    let original = tree(&source);
    let destination = root.join("migrated");
    migrate(&source, &destination);
    let archive = fs::read(destination.join("migration.db")).unwrap();
    assert!(
        check_rollback(&destination, &source, &key(), true)
            .unwrap()
            .eligible
    );
    let mut db = unlock(&destination.join(DB_FILE), &key()).unwrap();
    {
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO run_model_provenance VALUES('r1',1,'{}')", [])
            .unwrap();
        // A failed accepted-run transaction contributes no persistent mutation.
    }
    close(db).unwrap();
    assert!(
        check_rollback(&destination, &source, &key(), true)
            .unwrap()
            .eligible
    );
    let db = unlock(&destination.join(DB_FILE), &key()).unwrap();
    db.execute("INSERT INTO run_model_provenance VALUES('r1',1,'{}')", [])
        .unwrap();
    db.execute("DELETE FROM run_model_provenance WHERE run_id='r1'", [])
        .unwrap();
    close(db).unwrap();
    let report = check_rollback(&destination, &source, &key(), true).unwrap();
    assert!(!report.eligible);
    assert_eq!(tree(&source), original);
    assert_eq!(fs::read(destination.join("migration.db")).unwrap(), archive);
}
#[test]
fn application_passphrases_cannot_select_sqlcipher_raw_key_or_salt_syntax() {
    for prefix in ["x'", "X'"] {
        for count in [64, 96] {
            for tail in ["'", "!"] {
                assert!(Secret::new(format!("{prefix}{}{tail}", "1".repeat(count))).is_err());
            }
        }
    }
    assert!(Secret::new(String::new()).is_err());
    assert!(Secret::new("contains\0nul".into()).is_err());
    assert!(Secret::new("a".repeat(4097)).is_err());
    assert!(Secret::new("quoted ' ordinary passphrase".into()).is_ok());
}
fn root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap();
    (temp, path)
}
fn tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out)
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(path, path, &mut out);
    out
}
fn raw_cipher(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    let rc = unsafe {
        rusqlite::ffi::sqlite3_key(
            db.handle(),
            PASSPHRASE.as_ptr().cast(),
            PASSPHRASE.len() as i32,
        )
    };
    assert_eq!(rc, rusqlite::ffi::SQLITE_OK);
    db
}
fn fixture(path: &Path, version: i64) -> Connection {
    fs::create_dir(path).unwrap();
    let db = Connection::open(path.join(PLAINTEXT_FILE)).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL); CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT); CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL); CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL);").unwrap();
    if version >= 3 {
        db.execute_batch("ALTER TABLE run_configs ADD COLUMN warning TEXT; ALTER TABLE run_configs ADD COLUMN finish_reason TEXT; ALTER TABLE run_configs ADD COLUMN transport_incomplete INTEGER NOT NULL DEFAULT 0;").unwrap();
    }
    if version >= 4 {
        db.execute_batch("ALTER TABLE chats ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN version INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN title_locked INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN activity INTEGER NOT NULL DEFAULT 0;").unwrap();
    }
    db.execute(
        "INSERT INTO chats(rowid,id,title,created_at) VALUES(27,'c',?1,10)",
        [PAYLOAD],
    )
    .unwrap();
    db.execute(
        "INSERT INTO settings VALUES('config',?1)",
        [r#"{"provider":"mock","endpoint":"http://127.0.0.1:11434","model":"fixture"}"#],
    )
    .unwrap();
    db.execute("INSERT INTO messages(rowid,id,chat_id,role,content,status,error,created_at,run_id) VALUES(101,'u','c','user',?1,'completed',NULL,1,NULL),(205,'a1','c','assistant','old retry','cancelled','Cancelled',2,'r1'),(399,'a2','c','assistant','preserved partial','streaming',NULL,3,'r2')",[PAYLOAD]).unwrap();
    db.execute_batch("INSERT INTO run_configs(run_id,provider,endpoint,model) VALUES('r1','mock','http://127.0.0.1:11434','fixture'),('r2','mock','http://127.0.0.1:11434','fixture'); INSERT INTO artifacts(rowid,id,chat_id,name,content,created_at) VALUES(88,'artifact','c','notes.md','retained artifact 🦀',4); CREATE TABLE opaque(id TEXT PRIMARY KEY,bytes BLOB,number INTEGER,nullable TEXT); INSERT INTO opaque(rowid,id,bytes,number,nullable) VALUES(-7,'opaque',x'000102ff',9223372036854775807,NULL); CREATE TABLE opaque_without_rowid(id TEXT PRIMARY KEY,value BLOB) WITHOUT ROWID; INSERT INTO opaque_without_rowid VALUES('kept',x'ff0001'); CREATE INDEX opaque_number ON opaque(number); CREATE VIEW preserved_view AS SELECT id,bytes FROM opaque;").unwrap();
    if version >= 3 {
        db.execute(
            "UPDATE run_configs SET finish_reason='length' WHERE run_id='r2'",
            [],
        )
        .unwrap();
    }
    if version >= 5 {
        db.execute_batch("CREATE TABLE message_branches(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,parent_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED); CREATE INDEX message_branch_parent ON message_branches(parent_id); CREATE TABLE conversation_heads(chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,leaf_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED); CREATE TABLE text_attachments(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,name TEXT NOT NULL,media_type TEXT NOT NULL,content TEXT NOT NULL); CREATE INDEX attachment_message ON text_attachments(message_id); INSERT INTO message_branches VALUES('u',NULL),('a1','u'),('a2','u'); INSERT INTO conversation_heads VALUES('c','a2'); INSERT INTO text_attachments VALUES('t','u','input.txt','text/plain','attachment 🦀');").unwrap();
    }
    db.pragma_update(None, "user_version", version).unwrap();
    fs::create_dir(path.join("ancillary")).unwrap();
    fs::write(path.join("ancillary/config.txt"), PAYLOAD).unwrap();
    db
}
fn migrate(source: &Path, destination: &Path) {
    migrate_plaintext_copy(
        &MigrationRequest {
            source_copy: source,
            destination,
            offline_copy_attested: true,
        },
        &key(),
    )
    .unwrap();
}
fn assert_encrypted_tree(path: &Path) {
    for bytes in tree(path).values() {
        assert!(!bytes.starts_with(b"SQLite format 3\0"));
        for needle in [
            PAYLOAD.as_bytes(),
            PASSPHRASE.as_bytes(),
            b"retained artifact".as_slice(),
            b"preserved partial".as_slice(),
        ] {
            assert!(!bytes.windows(needle.len()).any(|s| s == needle));
        }
    }
}

#[test]
fn actual_sqlcipher_create_unlock_restart_and_no_plaintext_payloads() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    assert_eq!(probe(&path).unwrap(), ProfileKind::Missing);
    let db = create(&path, &key()).unwrap();
    let version: String = db
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .unwrap();
    assert!(version.starts_with("4."));
    db.execute("INSERT INTO settings VALUES('private',?1)", [PAYLOAD])
        .unwrap();
    db.execute("INSERT INTO media_objects VALUES('m','r','upload','input.txt','text/plain','txt',?1,?2,'{}',?3,1)",params!["0".repeat(64),PAYLOAD.len()as i64,PAYLOAD.as_bytes()]).unwrap();
    close(db).unwrap();
    assert_encrypted_tree(&root);
    assert_eq!(probe(&path).unwrap(), ProfileKind::EncryptedCandidate);
    let db = unlock(&path, &key()).unwrap();
    assert_eq!(
        db.query_row("SELECT value FROM settings WHERE key='private'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        PAYLOAD
    );
    assert_eq!(
        db.query_row("SELECT bytes FROM media_objects", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        PAYLOAD.as_bytes()
    );
    close(db).unwrap();
    let plain = Connection::open(&path).unwrap();
    assert!(plain
        .query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
        .is_err());
}
#[test]
fn wrong_key_future_schema_and_corruption_fail_without_reset() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    close(create(&path, &key()).unwrap()).unwrap();
    let before = tree(&root);
    assert!(unlock(
        &path,
        &Secret::new("wrong synthetic passphrase".into()).unwrap()
    )
    .is_err());
    assert_eq!(tree(&root), before);
    let db = raw_cipher(&path);
    db.pragma_update(None, "user_version", 99).unwrap();
    close(db).unwrap();
    let before = tree(&root);
    assert!(unlock(&path, &key()).is_err());
    assert_eq!(tree(&root), before);
    let mut bytes = fs::read(&path).unwrap();
    bytes[128] ^= 1;
    fs::write(&path, bytes).unwrap();
    let before = tree(&root);
    assert!(unlock(&path, &key()).is_err());
    assert_eq!(tree(&root), before);
}
#[test]
fn exclusive_create_and_plaintext_detection_never_replace_existing() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    close(create(&path, &key()).unwrap()).unwrap();
    let before = tree(&root);
    assert!(create(&path, &key()).is_err());
    assert_eq!(tree(&root), before);
    let plain = root.join("plain.db");
    let db = Connection::open(&plain).unwrap();
    initialize_schema(&db).unwrap();
    drop(db);
    let before = fs::read(&plain).unwrap();
    assert_eq!(
        probe(&plain).unwrap(),
        ProfileKind::PlaintextMigrationRequired
    );
    assert!(unlock(&plain, &key()).is_err());
    assert_eq!(fs::read(&plain).unwrap(), before);
}
#[test]
fn close_failure_retains_connection_and_can_be_retried() {
    let (_temp, root) = root();
    let db = create(&root.join(DB_FILE), &key()).unwrap();
    db.execute_batch("BEGIN IMMEDIATE; INSERT INTO settings VALUES('pending','not committed');")
        .unwrap();
    let failure = close(db).unwrap_err();
    let (db, _) = failure.into_parts();
    assert!(!db.is_autocommit());
    db.execute_batch("ROLLBACK;").unwrap();
    close(db).unwrap();
}

#[test]
fn checkpoint_busy_retains_connection_until_reader_releases() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    db.execute("INSERT INTO settings VALUES('one','first')", [])
        .unwrap();
    let reader = raw_cipher(&path);
    reader.execute_batch("BEGIN;").unwrap();
    let _: String = reader
        .query_row("SELECT value FROM settings WHERE key='one'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute("UPDATE settings SET value='second' WHERE key='one'", [])
        .unwrap();
    let (db, _) = close(db).unwrap_err().into_parts();
    reader.execute_batch("ROLLBACK;").unwrap();
    drop(reader);
    close(db).unwrap();
}

#[test]
fn empty_chats_and_legacy_branch_fallback_survive_unlock() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    db.execute_batch("INSERT INTO chats(id,title,created_at) VALUES('empty','Empty chat',1),('fallback','Older compatible chat',2); INSERT INTO messages VALUES('unlinked','fallback','user','retained fallback','completed',NULL,3,NULL);").unwrap();
    close(db).unwrap();
    close(unlock(&path, &key()).unwrap()).unwrap();
}

#[test]
fn private_output_permissions_and_readonly_source_are_supported() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in tree(&source).keys() {
            fs::set_permissions(source.join(path), fs::Permissions::from_mode(0o400)).unwrap();
        }
    }
    let before = tree(&source);
    let target = root.join("encrypted");
    migrate(&source, &target);
    assert_eq!(tree(&source), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for path in tree(&target).keys() {
            assert_eq!(
                fs::metadata(target.join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn profile_lock_is_allowed_but_unknown_destination_content_refuses_reconciliation() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    let target = root.join("encrypted");
    migrate(&source, &target);
    fs::write(target.join("profile.lock"), b"").unwrap();
    assert!(
        check_rollback(&target, &source, &key(), true)
            .unwrap()
            .eligible
    );
    fs::write(target.join("new-payload"), b"must retain").unwrap();
    let before = tree(&target);
    assert!(check_rollback(&target, &source, &key(), true).is_err());
    assert_eq!(tree(&target), before);
}

#[test]
fn archived_bytes_tampering_and_added_schema_are_detected() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    let target = root.join("encrypted");
    migrate(&source, &target);
    let vault = raw_cipher(&target.join("migration.db"));
    vault
        .execute(
            "UPDATE source_files SET bytes=x'0001' WHERE path='ancillary/config.txt'",
            [],
        )
        .unwrap();
    close(vault).unwrap();
    let before = tree(&target);
    assert!(verify_migration(&target, &key(), true).is_err());
    assert_eq!(tree(&target), before);
    let other = root.join("other.sqlite3");
    let db = create(&other, &key()).unwrap();
    db.execute_batch("CREATE VIEW unexpected AS SELECT value FROM settings;")
        .unwrap();
    close(db).unwrap();
    assert!(unlock(&other, &key()).is_err());
}

#[test]
fn bounded_inventory_refuses_oversized_or_deep_inputs_without_publication() {
    for kind in ["size", "depth"] {
        let (_temp, root) = root();
        let source = root.join("source");
        drop(fixture(&source, 5));
        if kind == "size" {
            fs::File::create(source.join("oversized"))
                .unwrap()
                .set_len(256 * 1024 * 1024 + 1)
                .unwrap();
        } else {
            let mut path = source.clone();
            for _ in 0..34 {
                path = path.join("nested");
                fs::create_dir(&path).unwrap();
            }
        }
        let target = root.join("encrypted");
        assert!(migrate_plaintext_copy(
            &MigrationRequest {
                source_copy: &source,
                destination: &target,
                offline_copy_attested: true
            },
            &key()
        )
        .is_err());
        assert!(!target.exists());
    }
}

#[test]
fn drafts_media_agents_and_trash_all_advance_the_persistent_write_counter() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    let before: i64 = db
        .query_row("SELECT write_generation FROM _smith_storage", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute_batch("INSERT INTO drafts(owner,version,text,updated_at) VALUES('new',1,'draft',1); UPDATE drafts SET version=2,text=''; INSERT INTO chats(id,title,created_at) VALUES('c','chat',1); INSERT INTO chat_trash VALUES('c',1); DELETE FROM chat_trash; INSERT INTO action_history(id,kind,before_json,after_json,state,created_at) VALUES('h','trash','{}','{}','applied',1); UPDATE action_history SET state='undone'; UPDATE action_history_state SET revision=revision+1; INSERT INTO action_history_versions VALUES('c',1); UPDATE agent_profile SET principal_id=principal_id;").unwrap();
    let after: i64 = db
        .query_row("SELECT write_generation FROM _smith_storage", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after - before, 10);
    close(db).unwrap();
    close(unlock(&path, &key()).unwrap()).unwrap();
}
#[test]
fn every_historical_next_version_preserves_typed_rows_rowids_branches_and_files() {
    for version in 1..=5 {
        let (_temp, root) = root();
        let source = root.join("source");
        drop(fixture(&source, version));
        let before = tree(&source);
        let target = root.join("encrypted");
        migrate(&source, &target);
        assert_eq!(tree(&source), before);
        assert_encrypted_tree(&target);
        let report = verify_migration(&target, &key(), true).unwrap();
        assert_eq!(report.source_schema, version);
        assert_eq!(report.target_schema, smith_profile_storage::SCHEMA_VERSION);
        assert!(report.baseline_unchanged);
        assert_eq!(report.later_writes, 0);
        assert!(
            check_rollback(&target, &source, &key(), true)
                .unwrap()
                .eligible
        );
        let db = unlock(&target.join(DB_FILE), &key()).unwrap();
        assert_eq!(
            db.query_row("SELECT rowid FROM chats WHERE id='c'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            27
        );
        assert_eq!(
            db.query_row("SELECT rowid FROM artifacts WHERE id='artifact'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            88
        );
        assert_eq!(
            db.query_row("SELECT rowid FROM messages WHERE id='a2'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            399
        );
        assert_eq!(
            db.query_row("SELECT rowid FROM opaque", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            -7
        );
        assert_eq!(
            db.query_row("SELECT content FROM messages WHERE id='u'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            PAYLOAD
        );
        assert_eq!(
            db.query_row("SELECT status FROM messages WHERE id='a1'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "cancelled"
        );
        assert_eq!(
            db.query_row("SELECT status FROM messages WHERE id='a2'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "streaming"
        );
        assert_eq!(
            db.query_row(
                "SELECT parent_id FROM message_branches WHERE message_id='a2'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "u"
        );
        close(db).unwrap();
        assert!(
            check_rollback(&target, &source, &key(), true)
                .unwrap()
                .eligible
        );
    }
}
#[test]
fn committed_write_then_delete_still_refuses_rollback() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    let target = root.join("encrypted");
    migrate(&source, &target);
    let db = unlock(&target.join(DB_FILE), &key()).unwrap();
    db.execute_batch("INSERT INTO settings VALUES('temporary','newer data'); DELETE FROM settings WHERE key='temporary';").unwrap();
    close(db).unwrap();
    let before = tree(&target);
    let report = check_rollback(&target, &source, &key(), true).unwrap();
    assert!(!report.eligible);
    assert!(!report.destination_unchanged);
    assert!(report.source_unchanged);
    assert_eq!(report.later_writes, 2);
    assert_eq!(tree(&target), before);
}
#[test]
fn rolled_back_transaction_does_not_block_rollback_but_recovery_does() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    let target = root.join("encrypted");
    migrate(&source, &target);
    let db = unlock(&target.join(DB_FILE), &key()).unwrap();
    db.execute_batch("BEGIN; INSERT INTO settings VALUES('temporary','discarded'); ROLLBACK;")
        .unwrap();
    close(db).unwrap();
    assert!(
        check_rollback(&target, &source, &key(), true)
            .unwrap()
            .eligible
    );
    let db = unlock(&target.join(DB_FILE), &key()).unwrap();
    recover_interrupted(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT status FROM messages WHERE id='a2'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "limited"
    );
    close(db).unwrap();
    assert!(
        !check_rollback(&target, &source, &key(), true)
            .unwrap()
            .eligible
    );
}
#[test]
fn source_changes_missing_vault_and_wrong_key_prevent_rollback() {
    let (_temp, root) = root();
    let source = root.join("source");
    drop(fixture(&source, 5));
    let target = root.join("encrypted");
    migrate(&source, &target);
    fs::write(source.join("ancillary/config.txt"), "changed").unwrap();
    let report = check_rollback(&target, &source, &key(), true).unwrap();
    assert!(!report.eligible);
    assert!(!report.source_unchanged);
    assert!(report.destination_unchanged);
    assert!(verify_migration(&target, &Secret::new("bad".into()).unwrap(), true).is_err());
    fs::remove_file(target.join("migration.db")).unwrap();
    assert!(check_rollback(&target, &source, &key(), true).is_err());
}
#[test]
fn copied_wal_commits_are_imported_and_source_bytes_never_change() {
    let (_temp, root) = root();
    let original = root.join("fixture-origin");
    let db = fixture(&original, 5);
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO settings VALUES('wal-only','committed WAL payload');").unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    for name in [
        PLAINTEXT_FILE,
        "smith-next.sqlite3-wal",
        "smith-next.sqlite3-shm",
    ] {
        let p = original.join(name);
        if p.exists() {
            fs::copy(p, source.join(name)).unwrap();
        }
    }
    let before = tree(&source);
    let target = root.join("encrypted");
    migrate(&source, &target);
    assert_eq!(tree(&source), before);
    let encrypted = unlock(&target.join(DB_FILE), &key()).unwrap();
    assert_eq!(
        encrypted
            .query_row("SELECT value FROM settings WHERE key='wal-only'", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "committed WAL payload"
    );
    close(encrypted).unwrap();
    drop(db);
}
#[test]
fn unknown_schema_invalid_links_hot_journal_and_missing_attestation_publish_nothing() {
    for problem in ["version", "graph", "view", "journal", "attestation"] {
        let (_temp, root) = root();
        let source = root.join("source");
        let db = fixture(&source, 5);
        match problem {
            "view"=>db.execute_batch("DROP TABLE settings; CREATE VIEW settings AS SELECT 'config' AS key,'{}' AS value;").unwrap(),
            "version" => db.pragma_update(None, "user_version", 25).unwrap(),
            "graph" => {
                db.execute(
                    "UPDATE message_branches SET parent_id='a2' WHERE message_id='a1'",
                    [],
                )
                .unwrap();
            }
            _ => {}
        }
        drop(db);
        if problem == "journal" {
            fs::write(source.join("smith-next.sqlite3-journal"), b"unrecovered").unwrap();
        }
        let before = tree(&source);
        let target = root.join("encrypted");
        assert!(migrate_plaintext_copy(
            &MigrationRequest {
                source_copy: &source,
                destination: &target,
                offline_copy_attested: problem != "attestation"
            },
            &key()
        )
        .is_err());
        assert!(!target.exists());
        assert_eq!(tree(&source), before);
    }
}
#[test]
fn missing_write_protection_is_refused_without_repair() {
    let (_temp, root) = root();
    let path = root.join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    let name: String = db
        .query_row(
            "SELECT name FROM sqlite_schema WHERE type='trigger' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute_batch(&format!("DROP TRIGGER \"{name}\";"))
        .unwrap();
    close(db).unwrap();
    let before = tree(&root);
    assert!(unlock(&path, &key()).is_err());
    assert_eq!(tree(&root), before);
}
#[cfg(unix)]
#[test]
fn source_symlink_hardlink_and_existing_destinations_are_refused() {
    use std::os::unix::fs::symlink;
    for kind in ["symlink", "hardlink", "existing"] {
        let (_temp, root) = root();
        let source = root.join("source");
        drop(fixture(&source, 5));
        let target = root.join("encrypted");
        match kind {
            "symlink" => symlink(source.join(PLAINTEXT_FILE), source.join("alias")).unwrap(),
            "hardlink" => fs::hard_link(source.join(PLAINTEXT_FILE), source.join("alias")).unwrap(),
            _ => {
                fs::create_dir(&target).unwrap();
                fs::write(target.join("keep"), b"existing").unwrap();
            }
        }
        let before = tree(&source);
        assert!(migrate_plaintext_copy(
            &MigrationRequest {
                source_copy: &source,
                destination: &target,
                offline_copy_attested: true
            },
            &key()
        )
        .is_err());
        assert_eq!(tree(&source), before);
        if kind == "existing" {
            assert_eq!(fs::read(target.join("keep")).unwrap(), b"existing");
        }
    }
}
