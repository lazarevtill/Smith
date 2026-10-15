use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use smith_profile_migrate::{reconcile, run, verify, Keys, Operation, Request};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

const MASTER: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const BLOB: &[u8] = b"SENSITIVE_BLOB_SENTINEL!";
const BLOB_PATH: &str = "11/22/112233445566778899aabbccddeeff00.bin";
const DERIVED_PATH: &str = "aa/bb/aabbccdd5566778899aabbccddeeff00.bin";
fn derive() -> (String, [u8; 32]) {
    let k = Hkdf::<Sha256>::new(None, &[0x11; 32]);
    let mut db = [0u8; 32];
    let mut blob = [0u8; 32];
    k.expand(b"smith-sqlcipher-db-v1", &mut db).unwrap();
    k.expand(b"smith-artifact-blob-v1", &mut blob).unwrap();
    (db.iter().map(|b| format!("{b:02x}")).collect(), blob)
}
fn conn(p: &Path) -> Connection {
    let c = Connection::open(p).unwrap();
    c.pragma_update(None, "key", derive().0).unwrap();
    c
}
fn seal(id: &str, bytes: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new_from_slice(&derive().1).unwrap();
    let nonce = Nonce::from([3u8; 12]);
    let mut result = nonce.to_vec();
    result.extend(
        cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: bytes,
                    aad: uuid::Uuid::parse_str(id).unwrap().as_bytes(),
                },
            )
            .unwrap(),
    );
    result
}
struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    source: PathBuf,
    managed: PathBuf,
    dest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let source = root.join("copy");
        let managed = source.join("artifacts");
        fs::create_dir_all(managed.join("11/22")).unwrap();
        fs::create_dir_all(managed.join("aa/bb")).unwrap();
        let c = conn(&source.join("smith.db"));
        c.execute_batch(include_str!("fixture.sql")).unwrap();
        c.execute(
            "UPDATE artifacts SET size=?1 WHERE store_id='managed'",
            [(BLOB.len() + 28) as i64],
        )
        .unwrap();
        drop(c);
        fs::write(
            managed.join(BLOB_PATH),
            seal("11223344-5566-7788-99aa-bbccddeeff00", BLOB),
        )
        .unwrap();
        fs::write(
            managed.join(DERIVED_PATH),
            seal(
                "aabbccdd-5566-7788-99aa-bbccddeeff00",
                b"Derived external text",
            ),
        )
        .unwrap();
        fs::write(managed.join("orphan.bin"), b"SENSITIVE_ORPHAN_SENTINEL").unwrap();
        fs::write(
            source.join("config.json"),
            b"{\"token\":\"SENSITIVE_CONFIG_SENTINEL\"}",
        )
        .unwrap();
        fs::create_dir(source.join("identity-reference")).unwrap();
        fs::write(
            source.join("identity-reference/sealed.bin"),
            b"SENSITIVE_IDENTITY_REFERENCE",
        )
        .unwrap();
        let dest = root.join("destination");
        Self {
            _temp: temp,
            root,
            source,
            managed,
            dest,
        }
    }
    fn keys(&self) -> Keys {
        Keys::from_master_hex(MASTER).unwrap()
    }
    fn request(&self) -> Request<'_> {
        Request {
            source_copy: &self.source,
            managed_copy: &self.managed,
            destination: &self.dest,
            offline_copy_attested: true,
        }
    }
    fn migrate(&self) -> smith_profile_migrate::Manifest {
        run(&self.request(), &self.keys(), Operation::Migrate).unwrap()
    }
}
fn tree(p: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, p: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(p).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out)
            } else {
                out.insert(p.strip_prefix(root).unwrap().into(), fs::read(p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(p, p, &mut out);
    out
}
fn assert_refused(f: &Fixture) {
    let before = tree(&f.source);
    assert!(run(&f.request(), &f.keys(), Operation::Migrate).is_err());
    assert!(!f.dest.exists());
    assert_eq!(before, tree(&f.source));
    assert!(fs::read_dir(&f.root).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".smith-migration-stage-")));
}

#[test]
fn encrypted_roundtrip_preserves_every_table_branch_payload_status_and_file() {
    let f = Fixture::new();
    let before = tree(&f.source);
    let m = f.migrate();
    assert_eq!(before, tree(&f.source));
    assert_eq!(m, verify(&f.dest, &f.keys()).unwrap());
    assert!(m.cipher_version.starts_with("4."));
    println!("verified SQLCipher {}", m.cipher_version);
    assert_eq!(m.verified_blobs, 2);
    assert_eq!(m.ancillary_files, 3);
    assert_eq!(m.external_stores, 1);
    let c = conn(&f.dest.join("smith.db"));
    let source = conn(&f.source.join("smith.db"));
    let schema = |c: &Connection| {
        c.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY name")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(schema(&source), schema(&c));
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM messages WHERE parent_id='root'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    let fields: (String, String, String) = c
        .query_row(
            "SELECT images,content,status FROM messages WHERE id='root'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(fields.0.contains("SENSITIVE_IMAGE"));
    assert_eq!(fields.1, "SENSITIVE_CHAT_SENTINEL");
    assert_eq!(fields.2, "complete");
    assert_eq!(
        c.query_row("SELECT status FROM messages WHERE id='branch1'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "cancelled"
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'private'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        c.query_row("SELECT enabled FROM schedules", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let vault = conn(&f.dest.join("migration.db"));
    let config: Vec<u8> = vault
        .query_row(
            "SELECT bytes FROM preserved_files WHERE path='config.json'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(config, b"{\"token\":\"SENSITIVE_CONFIG_SENTINEL\"}");
    for (path, bytes) in tree(&f.dest) {
        assert!(
            !bytes.starts_with(b"SQLite format 3\0"),
            "plaintext file {path:?}"
        );
        for sentinel in [
            "SENSITIVE_CHAT_SENTINEL",
            "SENSITIVE_CONFIG_SENTINEL",
            "SENSITIVE_BLOB_SENTINEL",
            "SENSITIVE_IMAGE_SENTINEL",
            "SENSITIVE_ORPHAN_SENTINEL",
            "SENSITIVE_IDENTITY_REFERENCE",
            "SENSITIVE_EXTERNAL_ROOT",
            "SENSITIVE_TOOL_SENTINEL",
            "SENSITIVE_TRACE_SENTINEL",
            MASTER,
        ] {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|w| w == sentinel.as_bytes()),
                "plaintext sentinel in {path:?}"
            );
        }
    }
}
#[test]
fn preflight_inventory_is_encrypted_and_not_an_activated_profile() {
    let f = Fixture::new();
    let before = tree(&f.source);
    let m = run(&f.request(), &f.keys(), Operation::Preflight).unwrap();
    assert_eq!(m.verified_blobs, 2);
    assert!(!f.dest.join("smith.db").exists());
    assert!(!f.dest.join("artifacts").exists());
    assert!(f.dest.join("migration.db").exists());
    assert!(verify(&f.dest, &f.keys()).is_err());
    assert_eq!(before, tree(&f.source));
}
#[test]
fn wrong_database_key_fails_without_reset() {
    let f = Fixture::new();
    let before = tree(&f.source);
    let wrong = Keys::from_master_hex(&"22".repeat(32)).unwrap();
    assert!(run(&f.request(), &wrong, Operation::Migrate).is_err());
    assert_eq!(before, tree(&f.source));
    assert!(!f.dest.exists());
}
#[test]
fn wrong_blob_key_fails_even_with_correct_database_key() {
    let f = Fixture::new();
    let keys = Keys::from_derived(zeroize::Zeroizing::new(derive().0), &"22".repeat(32)).unwrap();
    assert!(run(&f.request(), &keys, Operation::Migrate).is_err());
    assert!(!f.dest.exists());
}
#[test]
fn corrupt_encrypted_pages_fail_without_reset() {
    let f = Fixture::new();
    let p = f.source.join("smith.db");
    let mut b = fs::read(&p).unwrap();
    b[4200] ^= 1;
    fs::write(p, b).unwrap();
    assert_refused(&f);
}
#[test]
fn corrupt_blob_fails_authentication() {
    let f = Fixture::new();
    let p = f.managed.join(BLOB_PATH);
    let mut b = fs::read(&p).unwrap();
    b[18] ^= 1;
    fs::write(p, b).unwrap();
    assert_refused(&f);
}
#[test]
fn missing_blob_refuses_publication() {
    let f = Fixture::new();
    fs::remove_file(f.managed.join(BLOB_PATH)).unwrap();
    assert_refused(&f);
}
#[test]
fn unknown_schema_fails_without_reset() {
    for version in [0, 24, 26, 999] {
        let f = Fixture::new();
        conn(&f.source.join("smith.db"))
            .pragma_update(None, "user_version", version)
            .unwrap();
        assert_refused(&f);
    }
}
#[test]
fn partial_schema25_is_rejected() {
    let f = Fixture::new();
    conn(&f.source.join("smith.db"))
        .execute_batch("DROP TABLE tool_recipes")
        .unwrap();
    assert_refused(&f);
}
#[test]
fn plaintext_database_is_rejected() {
    let f = Fixture::new();
    fs::remove_file(f.source.join("smith.db")).unwrap();
    let c = Connection::open(f.source.join("smith.db")).unwrap();
    c.execute_batch(include_str!("fixture.sql")).unwrap();
    drop(c);
    assert_refused(&f);
}
#[test]
fn broken_parent_graph_is_rejected() {
    let f = Fixture::new();
    conn(&f.source.join("smith.db"))
        .execute(
            "UPDATE messages SET parent_id='branch3' WHERE id='root'",
            [],
        )
        .unwrap();
    assert_refused(&f);
}
#[test]
fn dangling_store_and_artifact_path_escape_are_rejected() {
    for sql in [
        "UPDATE artifacts SET store_id='missing' WHERE store_id='managed'",
        "UPDATE artifacts SET relpath='../outside' WHERE store_id='managed'",
        "UPDATE artifacts SET relpath='C:\\outside' WHERE store_id='managed'",
    ] {
        let f = Fixture::new();
        conn(&f.source.join("smith.db")).execute(sql, []).unwrap();
        assert_refused(&f);
    }
}
#[test]
fn external_roots_are_reference_only() {
    let f = Fixture::new();
    let external = f.root.join("external");
    fs::create_dir(&external).unwrap();
    fs::write(
        external.join("never-read.txt"),
        b"EXTERNAL_FILE_MUST_NOT_BE_COPIED",
    )
    .unwrap();
    let c = conn(&f.source.join("smith.db"));
    c.execute(
        "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
        [external.to_str().unwrap()],
    )
    .unwrap();
    drop(c);
    let before = tree(&external);
    f.migrate();
    assert_eq!(before, tree(&external));
    let v = conn(&f.dest.join("migration.db"));
    assert_eq!(
        v.query_row(
            "SELECT count(*) FROM preserved_files WHERE path LIKE '%never-read%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
#[test]
fn stale_fts_index_is_not_silently_rebuilt() {
    let f = Fixture::new();
    conn(&f.source.join("smith.db"))
        .execute(
            "UPDATE memory_episodes SET summary='changed text without index update'",
            [],
        )
        .unwrap();
    assert_refused(&f);
}
#[test]
fn existing_destination_is_untouched() {
    let f = Fixture::new();
    fs::create_dir(&f.dest).unwrap();
    fs::write(f.dest.join("sentinel"), b"keep").unwrap();
    assert!(run(&f.request(), &f.keys(), Operation::Migrate).is_err());
    assert_eq!(fs::read(f.dest.join("sentinel")).unwrap(), b"keep");
}
#[test]
fn wal_commits_survive_and_source_wal_bytes_are_unchanged() {
    let f = Fixture::new();
    let working = f.root.join("synthetic-writer");
    fs::create_dir(&working).unwrap();
    fs::copy(f.source.join("smith.db"), working.join("smith.db")).unwrap();
    let writer = conn(&working.join("smith.db"));
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO messages(id,session_id,seq,role,content,parent_id,images,status) VALUES('wal','session',5,'assistant','WAL_ONLY_CONTENT','branch3','[]','complete');").unwrap();
    for name in ["smith.db", "smith.db-wal", "smith.db-shm"] {
        fs::copy(working.join(name), f.source.join(name)).unwrap();
    }
    assert!(fs::metadata(f.source.join("smith.db-wal")).unwrap().len() > 0);
    let before = tree(&f.source);
    let m = f.migrate();
    assert_eq!(before, tree(&f.source));
    assert_eq!(m.database.tables["messages"].rows, 6);
    assert_eq!(
        conn(&f.dest.join("smith.db"))
            .query_row("SELECT content FROM messages WHERE id='wal'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        "WAL_ONLY_CONTENT"
    );
}
#[test]
fn later_writes_block_lossless_rollback() {
    let f = Fixture::new();
    f.migrate();
    let exact = reconcile(&f.dest, &f.source, &f.keys()).unwrap();
    assert!(exact.exact);
    assert!(exact.rollback_safe_without_losing_writes);
    let c = conn(&f.source.join("smith.db"));
    c.execute("INSERT INTO messages(id,session_id,seq,role,content,parent_id,images,status) VALUES('later','session',5,'assistant','Keep this later write','branch3','[]','complete')",[]).unwrap();
    drop(c);
    let before = tree(&f.source);
    let r = reconcile(&f.dest, &f.source, &f.keys()).unwrap();
    assert!(!r.exact);
    assert!(!r.rollback_safe_without_losing_writes);
    assert!(r.changed_tables.contains(&"messages".into()));
    assert_eq!(before, tree(&f.source));
}
#[test]
fn verify_detects_artifact_and_database_divergence() {
    for artifact in [true, false] {
        let f = Fixture::new();
        f.migrate();
        if artifact {
            fs::write(f.dest.join("artifacts").join(BLOB_PATH), b"tampered").unwrap();
        } else {
            conn(&f.dest.join("smith.db"))
                .execute("UPDATE messages SET content='changed' WHERE id='root'", [])
                .unwrap();
        }
        assert!(verify(&f.dest, &f.keys()).is_err());
    }
}
#[cfg(unix)]
#[test]
fn symlinks_and_hardlinks_are_refused() {
    use std::os::unix::fs::symlink;
    for hard in [true, false] {
        let f = Fixture::new();
        let outside = f.root.join("outside");
        fs::write(&outside, b"do not touch").unwrap();
        if hard {
            fs::hard_link(&outside, f.source.join("linked")).unwrap();
        } else {
            symlink(&outside, f.source.join("linked")).unwrap();
        }
        assert!(run(&f.request(), &f.keys(), Operation::Migrate).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"do not touch");
        assert!(!f.dest.exists());
    }
}
#[test]
fn cli_stdin_key_never_appears_in_output() {
    let f = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_smith-profile-migrate"))
        .args([
            "migrate",
            "--source-copy",
            f.source.to_str().unwrap(),
            "--managed-copy",
            f.managed.to_str().unwrap(),
            "--destination",
            f.dest.to_str().unwrap(),
            "--offline-copy",
            "--key-source",
            "stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{MASTER}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("\"ok\":true"));
    for text in [&*out, &String::from_utf8_lossy(&output.stderr)] {
        assert!(!text.contains(MASTER));
        assert!(!text.contains("SENSITIVE_"));
    }
}
#[test]
fn explicit_copy_attestation_is_mandatory() {
    let f = Fixture::new();
    let mut r = f.request();
    r.offline_copy_attested = false;
    assert!(run(&r, &f.keys(), Operation::Migrate).is_err());
    assert!(!f.dest.exists());
}
#[test]
fn fixture_keys_follow_original_hkdf_passphrase_contract() {
    let f = Fixture::new();
    let c = conn(&f.source.join("smith.db"));
    assert!(c
        .query_row("PRAGMA cipher_version", [], |r| r.get::<_, String>(0))
        .unwrap()
        .starts_with("4."));
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        25
    );
    let raw = Connection::open(f.source.join("smith.db")).unwrap();
    raw.pragma_update(None, "key", format!("x'{}'", derive().0))
        .unwrap();
    assert!(raw
        .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
        .is_err());
    assert_ne!(Sha256::digest(BLOB).as_slice(), BLOB);
}

#[test]
fn verify_requires_correct_blob_key_and_authenticates_vault() {
    let f = Fixture::new();
    f.migrate();
    let wrong = Keys::from_derived(zeroize::Zeroizing::new(derive().0), &"22".repeat(32)).unwrap();
    assert!(verify(&f.dest, &wrong).is_err());
    let p = f.dest.join("migration.db");
    let mut bytes = fs::read(&p).unwrap();
    bytes[4500] ^= 0x40;
    fs::write(p, bytes).unwrap();
    assert!(verify(&f.dest, &f.keys()).is_err());
}
#[test]
fn encrypted_vault_detects_missing_and_tampered_ancillary_files() {
    for sql in [
        "DELETE FROM preserved_files WHERE path='config.json'",
        "UPDATE preserved_files SET bytes=X'00' WHERE path='config.json'",
    ] {
        let f = Fixture::new();
        f.migrate();
        conn(&f.dest.join("migration.db")).execute(sql, []).unwrap();
        assert!(verify(&f.dest, &f.keys()).is_err());
    }
}
#[test]
fn external_overlap_is_rejected_before_enumerating_registered_files() {
    let f = Fixture::new();
    let external = f.source.join("user-folder");
    fs::create_dir(&external).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/does/not/exist", external.join("must-not-traverse")).unwrap();
    }
    conn(&f.source.join("smith.db"))
        .execute(
            "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
            [external.to_str().unwrap()],
        )
        .unwrap();
    let err = run(&f.request(), &f.keys(), Operation::Migrate).unwrap_err();
    assert!(err.0.contains("external root overlaps"));
    assert!(!f.dest.exists());
}
#[test]
fn destination_inside_external_store_is_refused() {
    let f = Fixture::new();
    let external = f.root.join("user-folder");
    fs::create_dir(&external).unwrap();
    conn(&f.source.join("smith.db"))
        .execute(
            "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
            [external.to_str().unwrap()],
        )
        .unwrap();
    let target = external.join("new");
    let request = Request {
        destination: &target,
        ..f.request()
    };
    assert!(run(&request, &f.keys(), Operation::Migrate).is_err());
    assert_eq!(fs::read_dir(external).unwrap().count(), 0);
}
#[test]
fn hot_journal_and_oversized_files_fail_closed() {
    let f = Fixture::new();
    fs::write(f.source.join("smith.db-journal"), b"not a clean copy").unwrap();
    assert_refused(&f);
    let f = Fixture::new();
    let large = fs::File::create(f.source.join("oversized.bin")).unwrap();
    large.set_len(256 * 1024 * 1024 + 1).unwrap();
    assert!(run(&f.request(), &f.keys(), Operation::Migrate).is_err());
    assert!(!f.dest.exists());
}
#[cfg(unix)]
#[test]
fn readonly_source_and_private_destination_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    for p in tree(&f.source).keys() {
        fs::set_permissions(f.source.join(p), fs::Permissions::from_mode(0o400)).unwrap();
    }
    fs::set_permissions(&f.source, fs::Permissions::from_mode(0o500)).unwrap();
    let before = tree(&f.source);
    f.migrate();
    assert_eq!(before, tree(&f.source));
    assert_eq!(
        fs::metadata(&f.dest).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for p in tree(&f.dest).keys() {
        assert_eq!(
            fs::metadata(f.dest.join(p)).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    fs::set_permissions(&f.source, fs::Permissions::from_mode(0o700)).unwrap();
}
#[cfg(unix)]
#[test]
fn cli_inherited_secret_descriptor_is_supported() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let input = format!("{MASTER}\n");
    assert_eq!(
        unsafe { libc::write(fds[1], input.as_ptr().cast(), input.len()) },
        input.len() as isize
    );
    unsafe {
        libc::close(fds[1]);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_smith-profile-migrate"));
    command.args([
        "preflight",
        "--source-copy",
        f.source.to_str().unwrap(),
        "--managed-copy",
        f.managed.to_str().unwrap(),
        "--destination",
        f.dest.to_str().unwrap(),
        "--offline-copy",
        "--key-fd",
        "3",
    ]);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fds[0], 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    unsafe {
        libc::close(fds[0]);
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(MASTER));
}

#[test]
fn rollback_requires_complete_and_valid_migrated_baseline() {
    for change in [
        "missing-db",
        "corrupt-db",
        "missing-blob",
        "corrupt-blob",
        "preflight",
    ] {
        let f = Fixture::new();
        if change == "preflight" {
            run(&f.request(), &f.keys(), Operation::Preflight).unwrap();
        } else {
            f.migrate();
            match change {
                "missing-db" => fs::remove_file(f.dest.join("smith.db")).unwrap(),
                "corrupt-db" => fs::write(f.dest.join("smith.db"), b"invalid database").unwrap(),
                "missing-blob" => {
                    fs::remove_file(f.dest.join("artifacts").join(BLOB_PATH)).unwrap()
                }
                "corrupt-blob" => {
                    fs::write(f.dest.join("artifacts").join(BLOB_PATH), b"invalid blob").unwrap()
                }
                _ => unreachable!(),
            }
        }
        let before = tree(&f.source);
        assert!(
            reconcile(&f.dest, &f.source, &f.keys()).is_err(),
            "{change}"
        );
        assert_eq!(before, tree(&f.source));
    }
}
#[cfg(unix)]
#[test]
fn terminal_prompt_suppresses_echo_and_restores_it_on_interrupt() {
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let master_file = unsafe { fs::File::from_raw_fd(master) };
    let slave_file = unsafe { fs::File::from_raw_fd(slave) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_smith-profile-migrate"));
    command
        .args([
            "verify",
            "--profile-copy",
            f.source.to_str().unwrap(),
            "--offline-copy",
        ])
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file.try_clone().unwrap());
    // Detach from any developer terminal but do not claim the synthetic PTY as
    // a controlling terminal. This forces the inherited-stdin fallback and
    // avoids macOS controlling-terminal teardown while the parent retains a slave.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    let mut poll = libc::pollfd {
        fd: master,
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 5000) },
        1,
        "terminal prompt timed out"
    );
    let mut prompt = [0u8; 512];
    let read = unsafe { libc::read(master, prompt.as_mut_ptr().cast(), prompt.len()) };
    assert!(read > 0, "prompt read failed");
    let prompt = String::from_utf8_lossy(&prompt[..read as usize]);
    assert!(
        prompt.contains("Existing master secret"),
        "unexpected prompt: {prompt}"
    );
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(unsafe { libc::tcgetattr(slave, settings.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { settings.assume_init() }.c_lflag & libc::ECHO, 0);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("interrupted terminal prompt failed to exit within deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(unsafe { libc::tcgetattr(slave, restored.as_mut_ptr()) }, 0);
    assert_ne!(unsafe { restored.assume_init() }.c_lflag & libc::ECHO, 0);
    drop(master_file);
    drop(slave_file);
}

#[test]
fn external_dot_aliases_and_relative_roots_cannot_bypass_exclusion() {
    for alias in ["parent", "dot", "relative", "windows-parent"] {
        let f = Fixture::new();
        let user = f.source.join("user-folder");
        fs::create_dir(&user).unwrap();
        fs::write(user.join("private.txt"), b"must stay external").unwrap();
        let elsewhere = f.root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let external = match alias {
            "parent" => format!("{}/../copy/user-folder", elsewhere.display()),
            "dot" => format!("{}/./user-folder", f.source.display()),
            "relative" => "copy/user-folder".to_owned(),
            _ => "C:\\users\\elsewhere\\..\\copy\\user-folder".to_owned(),
        };
        conn(&f.source.join("smith.db"))
            .execute(
                "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
                [external],
            )
            .unwrap();
        let error = run(&f.request(), &f.keys(), Operation::Migrate).unwrap_err();
        assert!(error.0.starts_with("external root"));
        assert!(!f.dest.exists());
    }
}

#[test]
fn original_blob_contract_rejects_uuid_text_aad_and_plaintext_size() {
    let f = Fixture::new();
    let cipher = ChaCha20Poly1305::new_from_slice(&derive().1).unwrap();
    let nonce = Nonce::from([4u8; 12]);
    let mut wrong = nonce.to_vec();
    wrong.extend(
        cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: BLOB,
                    aad: b"11223344-5566-7788-99aa-bbccddeeff00",
                },
            )
            .unwrap(),
    );
    fs::write(f.managed.join(BLOB_PATH), wrong).unwrap();
    assert_refused(&f);
    let f = Fixture::new();
    conn(&f.source.join("smith.db"))
        .execute(
            "UPDATE artifacts SET size=?1 WHERE store_id='managed'",
            [BLOB.len() as i64],
        )
        .unwrap();
    assert_refused(&f);
}

#[test]
fn verification_and_reconciliation_check_external_roots_before_inventory() {
    for operation in ["verify", "reconcile"] {
        let f = Fixture::new();
        f.migrate();
        let selected = if operation == "verify" {
            &f.dest
        } else {
            &f.source
        };
        let external = selected.join("external-subfolder");
        fs::create_dir(&external).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                "/unreadable-external-target",
                external.join("must-not-enumerate"),
            )
            .unwrap();
        }
        conn(&selected.join("smith.db"))
            .execute(
                "UPDATE artifact_stores SET root_path=?1 WHERE managed=0",
                [external.to_str().unwrap()],
            )
            .unwrap();
        let error = if operation == "verify" {
            verify(&f.dest, &f.keys()).unwrap_err()
        } else {
            reconcile(&f.dest, &f.source, &f.keys()).unwrap_err()
        };
        assert!(
            error.0.contains("external root overlaps"),
            "{operation}: {error}"
        );
    }
}
