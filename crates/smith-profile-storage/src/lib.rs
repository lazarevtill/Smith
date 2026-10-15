//! SQLCipher application storage and explicitly selected offline-copy migration.
//! No profile discovery, keychain or secret environment variables. The explicit
//! automatic-local mode stores its own generated key in a private owned file.
mod files;
mod legacy;
mod local;
mod migration;
mod schema;
mod schema_v10;
mod schema_v11;
mod schema_v7;
mod schema_v8;
mod schema_v9;

pub use legacy::{
    check_legacy_rollback, import_legacy_copy, preflight_legacy_copy, verify_legacy_import,
    LegacyImportReport, LegacyImportRequest, LegacyKeys,
};
pub use local::{local_workspace_secret, prepare_local_workspace, LOCAL_DIRECTORY, LOCAL_KEY_FILE};
pub use migration::{
    check_rollback, migrate_plaintext_copy, verify_migration, MigrationReport, MigrationRequest,
    RollbackReport,
};
use rusqlite::{Connection, OpenFlags};
pub use schema::{initialize_schema, recover_interrupted};
use serde::Serialize;
use std::path::Path;
use zeroize::Zeroizing;

pub const DB_FILE: &str = "profile.sqlite3";
pub const PLAINTEXT_FILE: &str = "smith-next.sqlite3";
pub const SCHEMA_VERSION: i64 = 11;
const APPLICATION_ID: i64 = 0x534e5052;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self("Profile filesystem operation failed; existing data was retained.")
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self("Profile authentication, integrity or schema validation failed; existing data was retained.")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self("Encrypted migration manifest is invalid.")
    }
}
fn require(ok: bool, text: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error(text))
    }
}

/// Owned, zeroizing secret. Intentionally neither Debug, Clone nor serializable.
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: String) -> Result<Self> {
        let value = Zeroizing::new(value);
        require(
            !value.is_empty() && value.len() <= 4096 && !value.contains('\0'),
            "Enter a nonempty passphrase of at most 4096 bytes.",
        )?;
        require(
            !value
                .as_bytes()
                .get(..2)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"x'")),
            "Raw SQLCipher key syntax is not an application passphrase.",
        )?;
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Missing,
    PlaintextMigrationRequired,
    EncryptedCandidate,
    Invalid,
}

/// Header inspection only. A candidate is authenticated only by `unlock`.
pub fn probe(path: &Path) -> Result<ProfileKind> {
    if !path.try_exists()? {
        for suffix in ["-wal", "-shm", "-journal"] {
            if files::sidecar(path, suffix).try_exists()? {
                return Ok(ProfileKind::Invalid);
            }
        }
        return Ok(ProfileKind::Missing);
    }
    let bytes = files::read_header(path)?;
    Ok(if bytes.starts_with(b"SQLite format 3\0") {
        ProfileKind::PlaintextMigrationRequired
    } else if bytes.len() >= 512 {
        ProfileKind::EncryptedCandidate
    } else {
        ProfileKind::Invalid
    })
}

fn open_cipher(path: &Path, secret: &Secret, new: bool) -> Result<Connection> {
    if new {
        files::write_new(path, &[])?;
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let rc = unsafe {
        rusqlite::ffi::sqlite3_key(db.handle(), secret.0.as_ptr().cast(), secret.0.len() as i32)
    };
    require(
        rc == rusqlite::ffi::SQLITE_OK,
        "SQLCipher key initialization failed.",
    )?;
    let version: String = db
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .map_err(|_| Error("SQLCipher is unavailable; refusing plaintext storage."))?;
    require(
        version.starts_with("4."),
        "Unsupported SQLCipher implementation; version 4 is required.",
    )?;
    db.execute_batch("PRAGMA cipher_memory_security=ON; PRAGMA temp_store=MEMORY; PRAGMA trusted_schema=OFF; PRAGMA busy_timeout=1000; PRAGMA foreign_keys=ON;")?;
    let _: i64 = db.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    Ok(db)
}

fn integrity(db: &Connection, cipher: bool) -> Result<()> {
    if cipher {
        let results = db
            .prepare("PRAGMA cipher_integrity_check")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        require(
            results.is_empty() || results.iter().all(|v| v == "ok"),
            "Encrypted page authentication failed.",
        )?;
    }
    let results = db
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    require(results == ["ok"], "Database integrity check failed.")?;
    require(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "Profile contains invalid foreign-key relationships.",
    )?;
    Ok(())
}

/// Create a new encrypted file exclusively. Never upgrades/replaces an existing file.
pub fn create(path: &Path, secret: &Secret) -> Result<Connection> {
    let (parent, destination) = files::destination(path)?;
    require(
        probe(&destination)? == ProfileKind::Missing,
        "Profile already exists; creation cannot overwrite it.",
    )?;
    let stage = files::private_temp("smith-profile-create-", Some(&parent))?;
    let staged = stage.path().join(DB_FILE);
    let db = open_cipher(&staged, secret, true)?;
    initialize_schema(&db)?;
    schema::install_storage(&db)?;
    schema::validate(&db)?;
    integrity(&db, true)?;
    seal(&db)?;
    db.close()
        .map_err(|_| Error("Could not close staged profile."))?;
    files::sync_tree(stage.path())?;
    files::rename_exclusive(&staged, &destination)?;
    files::sync_parent(&destination)?;
    unlock(&destination, secret)
}

/// Existing authenticated profiles only. Known schemas 6–10 upgrade transactionally
/// after a private-copy rehearsal; wrong-key/unknown/corrupt inputs never reset.
pub fn unlock(path: &Path, secret: &Secret) -> Result<Connection> {
    require(
        probe(path)? == ProfileKind::EncryptedCandidate,
        "An encrypted profile is required; plaintext profiles require explicit copy migration.",
    )?;
    // Authenticate a private encrypted clone before touching live WAL state.
    let parent = files::safe_root(
        path.parent()
            .ok_or(Error("Profile needs a parent directory."))?,
    )?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(Error("Invalid profile filename."))?;
    let before = files::database_inventory(&parent, name)?;
    let check = files::private_temp("smith-profile-unlock-", None)?;
    files::copy_encrypted_database(&parent, name, check.path(), DB_FILE)?;
    let copied = open_cipher(&check.path().join(DB_FILE), secret, false)?;
    schema::validate(&copied)?;
    integrity(&copied, true)?;
    schema::upgrade_encrypted(&copied)?;
    schema::validate(&copied)?;
    integrity(&copied, true)?;
    drop(copied);
    require(
        files::database_inventory(&parent, name)? == before,
        "Profile changed during unlock; stop other writers and retry.",
    )?;
    let db = open_cipher(path, secret, false)?;
    schema::validate(&db)?;
    schema::upgrade_encrypted(&db)?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    Ok(db)
}

pub struct CloseFailure {
    connection: Box<Connection>,
    pub error: Error,
}
impl CloseFailure {
    pub fn into_parts(self) -> (Connection, Error) {
        (*self.connection, self.error)
    }
}
impl std::fmt::Debug for CloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CloseFailure").field(&self.error).finish()
    }
}
impl std::fmt::Display for CloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for CloseFailure {}

/// Caller must have drained requests/tasks. On failure ownership is returned.
pub fn close(db: Connection) -> std::result::Result<(), CloseFailure> {
    let result = (|| {
        require(
            db.is_autocommit(),
            "Cannot lock a profile while a transaction is active.",
        )?;
        let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        require(
            busy == 0,
            "Profile checkpoint is busy; retain the connection and retry locking.",
        )
    })();
    if let Err(error) = result {
        return Err(CloseFailure {
            connection: Box::new(db),
            error,
        });
    }
    db.close().map_err(|(connection, _)| CloseFailure {
        connection: Box::new(connection),
        error: Error("Profile connection could not close; it remains owned and unavailable."),
    })
}

fn seal(db: &Connection) -> Result<()> {
    let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    require(busy == 0, "Private snapshot checkpoint is busy.")?;
    let mode: String = db.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    require(mode == "delete", "Private snapshot could not be sealed.")
}
