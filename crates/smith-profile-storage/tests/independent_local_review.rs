//! Independent automatic-workspace publication checks; synthetic paths only.
use smith_profile_storage as storage;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::{
    fs,
    sync::{Arc, Barrier},
};

#[test]
fn competing_creators_publish_one_complete_pair_without_replacing_key() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                storage::prepare_local_workspace(&root)
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(results.iter().any(Result::is_ok));
    let local = root.join(storage::LOCAL_DIRECTORY);
    let before = fs::read(local.join(storage::LOCAL_KEY_FILE)).unwrap();
    let key = storage::local_workspace_secret(&local).unwrap();
    let db = storage::unlock(&local.join(storage::DB_FILE), &key).unwrap();
    db.execute("INSERT INTO settings VALUES('review','preserved')", [])
        .unwrap();
    storage::close(db).unwrap();
    assert_eq!(storage::prepare_local_workspace(&root).unwrap(), local);
    assert_eq!(
        fs::read(local.join(storage::LOCAL_KEY_FILE)).unwrap(),
        before
    );
    let db = storage::unlock(&local.join(storage::DB_FILE), &key).unwrap();
    assert_eq!(
        db.query_row("SELECT value FROM settings WHERE key='review'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "preserved"
    );
    storage::close(db).unwrap();
}

#[test]
fn every_partial_existing_slot_is_retained_without_repair() {
    for variant in ["empty", "key-only", "database-only"] {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let local = root.join(storage::LOCAL_DIRECTORY);
        fs::create_dir(&local).unwrap();
        fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).unwrap();
        let key = local.join(storage::LOCAL_KEY_FILE);
        let db = local.join(storage::DB_FILE);
        if variant == "key-only" {
            fs::write(&key, "1".repeat(64)).unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        }
        if variant == "database-only" {
            fs::write(&db, b"retained incomplete database evidence").unwrap();
        }
        let key_before = fs::read(&key).ok();
        let db_before = fs::read(&db).ok();
        assert!(
            storage::prepare_local_workspace(&root).is_err(),
            "{variant}"
        );
        assert_eq!(fs::read(&key).ok(), key_before);
        assert_eq!(fs::read(&db).ok(), db_before);
    }
}

#[test]
fn automatic_key_aliases_and_hardlinks_never_expand_authority() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let local = storage::prepare_local_workspace(&root).unwrap();
    let key = local.join(storage::LOCAL_KEY_FILE);
    let key_before = fs::read(&key).unwrap();
    let db_before = fs::read(local.join(storage::DB_FILE)).unwrap();
    let extra = root.join("synthetic-second-key-link");
    fs::hard_link(&key, &extra).unwrap();
    assert!(storage::prepare_local_workspace(&root).is_err());
    fs::remove_file(&extra).unwrap();
    fs::rename(&key, &extra).unwrap();
    symlink(&extra, &key).unwrap();
    assert!(storage::prepare_local_workspace(&root).is_err());
    assert_eq!(fs::read(&extra).unwrap(), key_before);
    assert_eq!(fs::read(local.join(storage::DB_FILE)).unwrap(), db_before);
    assert!(fs::symlink_metadata(&key).unwrap().file_type().is_symlink());
}
