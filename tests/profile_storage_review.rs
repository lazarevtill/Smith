//! Independent public-API regressions. Synthetic temporary profiles only.
use rusqlite::Connection;
use smith_profile_storage::{
    check_rollback, close, create, migrate_plaintext_copy, unlock, MigrationRequest, Secret,
    DB_FILE, PLAINTEXT_FILE,
};
use std::{collections::BTreeMap, fs, path::Path};

const PASSPHRASE: &str = "reviewer synthetic secret 2026-10-15";
fn key() -> Secret {
    Secret::new(PASSPHRASE.into()).unwrap()
}
fn raw(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_key(
                db.handle(),
                PASSPHRASE.as_ptr().cast(),
                PASSPHRASE.len() as i32,
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    db
}
fn bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}
fn source(root: &Path) {
    fs::create_dir(root).unwrap();
    let db = Connection::open(root.join(PLAINTEXT_FILE)).unwrap();
    db.execute_batch("PRAGMA user_version=5;
        CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,version INTEGER NOT NULL,title_locked INTEGER NOT NULL,activity INTEGER NOT NULL);
        CREATE TABLE messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);
        CREATE TABLE artifacts(id TEXT PRIMARY KEY,chat_id TEXT REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);
        CREATE TABLE run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL,warning TEXT,finish_reason TEXT,transport_incomplete INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE message_branches(message_id TEXT PRIMARY KEY REFERENCES messages(id),parent_id TEXT REFERENCES messages(id));
        CREATE TABLE conversation_heads(chat_id TEXT PRIMARY KEY REFERENCES chats(id),leaf_id TEXT REFERENCES messages(id));
        CREATE TABLE text_attachments(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id),name TEXT NOT NULL,media_type TEXT NOT NULL,content TEXT NOT NULL);
        CREATE TABLE opaque(id INTEGER PRIMARY KEY AUTOINCREMENT, value BLOB);
        INSERT INTO opaque(id,value) VALUES(17,x'0000ff');
        INSERT INTO opaque(id,value) VALUES(100,x'0102');
        DELETE FROM opaque WHERE id=100;
        INSERT INTO chats(rowid,id,title,created_at,updated_at,version,title_locked,activity) VALUES(-42,'c','synthetic',1,1,0,0,-42);
        INSERT INTO conversation_heads VALUES('c',NULL);").unwrap();
    db.close().unwrap();
}

#[test]
fn wrong_key_with_wal_does_not_touch_live_database_or_sidecars() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.as_path().join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    db.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO settings VALUES('review','encrypted committed WAL');").unwrap();
    assert!(path.with_file_name(format!("{DB_FILE}-wal")).exists());
    let before = bytes(root.as_path());
    assert!(unlock(
        &path,
        &Secret::new("wrong reviewer passphrase".into()).unwrap()
    )
    .is_err());
    assert_eq!(bytes(root.as_path()), before);
    close(db).unwrap();
}

#[test]
fn checkpoint_busy_returns_owned_connection_until_reader_releases() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.as_path().join(DB_FILE);
    let db = create(&path, &key()).unwrap();
    db.execute_batch("INSERT INTO settings VALUES('review','before');")
        .unwrap();
    let reader = raw(&path);
    reader
        .execute_batch("BEGIN; SELECT * FROM settings;")
        .unwrap();
    db.execute_batch("UPDATE settings SET value='after' WHERE key='review';")
        .unwrap();
    let (db, _) = close(db).unwrap_err().into_parts();
    assert_eq!(
        db.query_row("SELECT value FROM settings WHERE key='review'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "after"
    );
    reader.execute_batch("ROLLBACK;").unwrap();
    reader.close().unwrap();
    close(db).unwrap();
}

#[test]
fn migration_preserves_autoincrement_high_water_and_negative_rowid() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source_root = root.as_path().join("source");
    let destination = root.as_path().join("destination");
    source(&source_root);
    let before = bytes(&source_root);
    migrate_plaintext_copy(
        &MigrationRequest {
            source_copy: &source_root,
            destination: &destination,
            offline_copy_attested: true,
        },
        &key(),
    )
    .unwrap();
    assert_eq!(bytes(&source_root), before);
    assert!(
        check_rollback(&destination, &source_root, &key(), true)
            .unwrap()
            .eligible
    );
    let db = unlock(&destination.join(DB_FILE), &key()).unwrap();
    assert_eq!(
        db.query_row("SELECT rowid FROM chats WHERE id='c'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        -42
    );
    assert_eq!(
        db.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='opaque'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        100
    );
    db.execute_batch("INSERT INTO opaque(value) VALUES(x'03');")
        .unwrap();
    assert_eq!(db.last_insert_rowid(), 101);
    close(db).unwrap();
    assert!(
        !check_rollback(&destination, &source_root, &key(), true)
            .unwrap()
            .eligible
    );
}

#[test]
fn unknown_table_write_and_revert_cannot_regain_rollback_eligibility() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source_root = root.as_path().join("source");
    let destination = root.as_path().join("destination");
    source(&source_root);
    migrate_plaintext_copy(
        &MigrationRequest {
            source_copy: &source_root,
            destination: &destination,
            offline_copy_attested: true,
        },
        &key(),
    )
    .unwrap();
    let db = unlock(&destination.join(DB_FILE), &key()).unwrap();
    db.execute_batch("UPDATE opaque SET value=x'4444' WHERE id=17; UPDATE opaque SET value=x'0000ff' WHERE id=17;").unwrap();
    close(db).unwrap();
    let report = check_rollback(&destination, &source_root, &key(), true).unwrap();
    assert!(!report.eligible);
    assert_eq!(report.later_writes, 2);
    assert!(report.source_unchanged);
}

#[test]
fn invalid_branch_graph_is_rejected_without_live_repair() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let path = root.as_path().join(DB_FILE);
    close(create(&path, &key()).unwrap()).unwrap();
    let db = raw(&path);
    db.execute_batch("INSERT INTO chats(id,title,created_at) VALUES('a','a',1),('b','b',1);
        INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES('m1','a','user','a','completed',1),('m2','b','user','b','completed',2);
        INSERT INTO message_branches VALUES('m1',NULL),('m2','m1');
        INSERT INTO conversation_heads VALUES('a','m1'),('b','m2');").unwrap();
    close(db).unwrap();
    let before = bytes(root.as_path());
    assert!(unlock(&path, &key()).is_err());
    assert_eq!(bytes(root.as_path()), before);
}
