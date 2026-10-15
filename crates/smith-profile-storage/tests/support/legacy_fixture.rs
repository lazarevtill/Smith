//! Independently authored synthetic original-wire-contract fixture.
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use rusqlite::Connection;
use sha2::Sha256;
use std::{fs, path::Path};
pub const ARTIFACT_ID: &str = "11223344-5566-7788-99aa-bbccddeeff00";
pub const BLOB_PATH: &str = "11/22/112233445566778899aabbccddeeff00.bin";
pub const BLOB: &[u8] = b"Synthetic original managed artifact. Exact retained bytes.\n";
pub fn derive(master: &[u8; 32]) -> (String, [u8; 32]) {
    let kdf = Hkdf::<Sha256>::new(None, master);
    let mut db = [0; 32];
    let mut blob = [0; 32];
    kdf.expand(b"smith-sqlcipher-db-v1", &mut db).unwrap();
    kdf.expand(b"smith-artifact-blob-v1", &mut blob).unwrap();
    (db.iter().map(|b| format!("{b:02x}")).collect(), blob)
}
pub fn connection(path: &Path, master: &[u8; 32]) -> Connection {
    let db = Connection::open(path).unwrap();
    let key = zeroize::Zeroizing::new(derive(master).0);
    let rc =
        unsafe { rusqlite::ffi::sqlite3_key(db.handle(), key.as_ptr().cast(), key.len() as i32) };
    assert_eq!(rc, 0);
    db
}
pub fn create(root: &Path, master: &[u8; 32]) {
    fs::create_dir(root).unwrap();
    let artifacts = root.join("artifacts");
    fs::create_dir_all(artifacts.join("11/22")).unwrap();
    fs::create_dir_all(artifacts.join("aa/bb")).unwrap();
    let db = connection(&root.join("smith.db"), master);
    db.execute_batch(include_str!(
        "../../../../tools/profile-migrate/tests/fixture.sql"
    ))
    .unwrap();
    db.execute_batch("UPDATE sessions SET title='Original synthetic conversation'; UPDATE messages SET images='[]' WHERE id='root'; UPDATE messages SET status='failed' WHERE id='branch2'; UPDATE messages SET status='incomplete' WHERE id='branch3'; INSERT INTO messages VALUES('latest','session',9,'assistant','Original selected answer retained.', 'Original private reasoning retained.','[]',NULL,'2026-01-01T00:00:06Z','root','[]','complete'); UPDATE artifacts SET title='original-proof.txt',session_id='session',message_id='latest',created_at='2026-01-01T00:00:06Z' WHERE store_id='managed'; UPDATE tasks SET created_at='2026-01-01T00:00:00Z',updated_at='2026-01-01T00:00:01Z'; CREATE TABLE original_unknown(id TEXT PRIMARY KEY,exact_number INTEGER,real_value REAL,bytes BLOB,nullable TEXT); INSERT INTO original_unknown(rowid,id,exact_number,real_value,bytes,nullable) VALUES(-51,'opaque',9223372036854775807,-0.125,X'00FF0102',NULL);").unwrap();
    db.execute(
        "UPDATE artifacts SET size=?1 WHERE id=?2",
        rusqlite::params![BLOB.len() + 28, ARTIFACT_ID],
    )
    .unwrap();
    drop(db);
    let (_, key) = derive(master);
    let cipher = ChaCha20Poly1305::new_from_slice(&key).unwrap();
    for (id, path, bytes, nonce) in [
        (ARTIFACT_ID, BLOB_PATH, BLOB, [1u8; 12]),
        (
            "aabbccdd-5566-7788-99aa-bbccddeeff00",
            "aa/bb/aabbccdd5566778899aabbccddeeff00.bin",
            b"Synthetic derived understanding; external original never opened.".as_slice(),
            [2u8; 12],
        ),
    ] {
        let nonce = Nonce::from(nonce);
        let mut sealed = nonce.to_vec();
        sealed.extend(
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
        fs::write(artifacts.join(path), sealed).unwrap();
    }
    fs::write(
        root.join("ancillary-synthetic.txt"),
        b"Original synthetic ancillary evidence; retain exactly.",
    )
    .unwrap();
    fs::write(
        artifacts.join("unmapped-synthetic.bin"),
        b"Unknown synthetic bytes retained.",
    )
    .unwrap();
}
