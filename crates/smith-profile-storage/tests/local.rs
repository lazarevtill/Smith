use smith_profile_storage as storage;
use std::fs;
fn root() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = fs::canonicalize(t.path()).unwrap();
    (t, p)
}
#[test]
fn automatic_local_pair_reopens_and_preserves_manual_files() {
    let (_t, p) = root();
    fs::write(
        p.join("profile.sqlite3"),
        b"untouched manual encrypted evidence",
    )
    .unwrap();
    fs::write(
        p.join("profile-selection.json"),
        b"untouched manual selection",
    )
    .unwrap();
    let local = storage::prepare_local_workspace(&p).unwrap();
    let key = storage::local_workspace_secret(&local).unwrap();
    let db = storage::unlock(&local.join(storage::DB_FILE), &key).unwrap();
    db.execute(
        "INSERT INTO settings VALUES('synthetic','retained local payload')",
        [],
    )
    .unwrap();
    storage::close(db).unwrap();
    let before = fs::read(local.join(storage::LOCAL_KEY_FILE)).unwrap();
    assert_eq!(storage::prepare_local_workspace(&p).unwrap(), local);
    assert_eq!(
        fs::read(local.join(storage::LOCAL_KEY_FILE)).unwrap(),
        before
    );
    let db = storage::unlock(
        &local.join(storage::DB_FILE),
        &storage::local_workspace_secret(&local).unwrap(),
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='synthetic'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "retained local payload"
    );
    storage::close(db).unwrap();
    assert_eq!(
        fs::read(p.join("profile.sqlite3")).unwrap(),
        b"untouched manual encrypted evidence"
    );
    assert_eq!(
        fs::read(p.join("profile-selection.json")).unwrap(),
        b"untouched manual selection"
    );
    assert!(!fs::read(local.join(storage::DB_FILE))
        .unwrap()
        .windows(22)
        .any(|b| b == b"retained local payload"));
}
#[test]
fn lost_corrupt_and_overbroad_local_keys_never_regenerate() {
    for kind in ["missing", "corrupt", "permissions", "partial"] {
        let (_t, p) = root();
        let local = storage::prepare_local_workspace(&p).unwrap();
        let original = fs::read(local.join(storage::DB_FILE)).unwrap();
        let key = local.join(storage::LOCAL_KEY_FILE);
        match kind {
            "missing" => fs::remove_file(&key).unwrap(),
            "corrupt" => fs::write(&key, b"invalid").unwrap(),
            "permissions" => {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => fs::remove_file(local.join(storage::DB_FILE)).unwrap(),
        };
        assert!(storage::prepare_local_workspace(&p).is_err());
        if kind != "partial" {
            assert_eq!(fs::read(local.join(storage::DB_FILE)).unwrap(), original);
        }
        if kind == "missing" {
            assert!(!key.exists());
        }
    }
}
#[test]
fn wrong_valid_local_key_refuses_cipher_open_without_repair() {
    let (_t, p) = root();
    let local = storage::prepare_local_workspace(&p).unwrap();
    let before = fs::read(local.join(storage::DB_FILE)).unwrap();
    fs::write(local.join(storage::LOCAL_KEY_FILE), "0".repeat(64)).unwrap();
    let key = storage::local_workspace_secret(&local).unwrap();
    assert!(storage::unlock(&local.join(storage::DB_FILE), &key).is_err());
    assert_eq!(fs::read(local.join(storage::DB_FILE)).unwrap(), before);
    assert_eq!(
        fs::read_to_string(local.join(storage::LOCAL_KEY_FILE)).unwrap(),
        "0".repeat(64)
    );
}
