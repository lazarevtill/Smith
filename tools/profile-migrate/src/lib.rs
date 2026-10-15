//! Offline, copy-only encrypted preservation migration. No application runtime.
use chacha20poly1305::{
    aead::{Aead, Payload},
    ChaCha20Poly1305, KeyInit, Nonce,
};
use hkdf::Hkdf;
use rusqlite::{params, types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::{Builder, TempDir};
use zeroize::Zeroizing;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
// Deliberately redact SQL, paths, database values and key material from errors.
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self("filesystem operation failed; source retained")
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self("encrypted database operation failed; check key, integrity and schema")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self("encrypted manifest is invalid")
    }
}
fn require(ok: bool, msg: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error(msg))
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
fn decode_key(s: &str) -> Result<Zeroizing<[u8; 32]>> {
    require(
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "key must contain exactly 64 hexadecimal characters",
    )?;
    let mut out = Zeroizing::new([0; 32]);
    for (i, v) in out.iter_mut().enumerate() {
        *v = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| Error("invalid key"))?;
    }
    Ok(out)
}
/// Intentionally has no Debug/Serialize implementation. Supplied keys never enter a report.
pub struct Keys {
    db: Zeroizing<String>,
    blob: Zeroizing<[u8; 32]>,
}
impl Keys {
    pub fn from_master_hex(s: &str) -> Result<Self> {
        let secret = decode_key(s)?;
        let kdf = Hkdf::<Sha256>::new(None, &*secret);
        let mut db = Zeroizing::new([0u8; 32]);
        let mut blob = Zeroizing::new([0u8; 32]);
        kdf.expand(b"smith-sqlcipher-db-v1", &mut *db)
            .map_err(|_| Error("key derivation failed"))?;
        kdf.expand(b"smith-artifact-blob-v1", &mut *blob)
            .map_err(|_| Error("key derivation failed"))?;
        Ok(Self {
            db: Zeroizing::new(hex(&*db)),
            blob,
        })
    }
    /// Exact original SQLCipher passphrase (including x'hex' if explicitly raw) + blob key.
    pub fn from_derived(db: Zeroizing<String>, blob_hex: &str) -> Result<Self> {
        require(
            !db.is_empty() && db.len() <= 4096 && !db.contains('\0'),
            "invalid database passphrase",
        )?;
        Ok(Self {
            db,
            blob: decode_key(blob_hex)?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRecord {
    pub bytes: u64,
    pub sha256: String,
    pub mode: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableRecord {
    pub rows: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseRecord {
    pub schema_version: i64,
    pub schema_sha256: String,
    pub tables: BTreeMap<String, TableRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub cipher_version: String,
    pub database: DatabaseRecord,
    pub source_files: BTreeMap<String, FileRecord>,
    pub artifact_files: BTreeMap<String, FileRecord>,
    pub verified_blobs: u64,
    pub external_stores: u64,
    pub external_artifacts: u64,
    pub ancillary_files: u64,
    pub offline_copy_attested: bool,
    pub runtime_activation: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Preflight,
    Migrate,
}
pub struct Request<'a> {
    pub source_copy: &'a Path,
    pub managed_copy: &'a Path,
    pub destination: &'a Path,
    pub offline_copy_attested: bool,
}
const LIMIT: u64 = 256 * 1024 * 1024;
const REQUIRED: &[(&str, &[&str])] = &[
    (
        "agents",
        &["id", "name", "system_prompt", "capabilities", "working_dir"],
    ),
    (
        "sessions",
        &["id", "agent_id", "title", "created_at", "updated_at"],
    ),
    (
        "messages",
        &[
            "id",
            "session_id",
            "seq",
            "role",
            "content",
            "reasoning",
            "tool_calls",
            "tool_call_id",
            "created_at",
            "parent_id",
            "images",
            "status",
        ],
    ),
    (
        "memory_episodes",
        &[
            "id",
            "summary",
            "embedding",
            "session_id",
            "valid_to",
            "pinned",
        ],
    ),
    (
        "tasks",
        &[
            "id",
            "status",
            "trigger_kind",
            "input",
            "output",
            "error",
            "transcript",
        ],
    ),
    ("tools", &["name", "parameters", "source", "enabled"]),
    (
        "schedules",
        &["id", "cron", "payload", "enabled", "last_run", "next_run"],
    ),
    ("plugins", &["id", "manifest", "enabled"]),
    ("watch_triggers", &["id", "agent_id"]),
    ("skills", &["id"]),
    ("methods", &["id"]),
    ("tool_recipes", &["id"]),
    ("agent_souls", &["agent_id", "content"]),
    ("interaction_signals", &["id", "session_id"]),
    (
        "agent_soul_candidates",
        &["agent_id", "content", "content_hash"],
    ),
    ("mcp_providers", &["id", "transport", "enabled"]),
    ("lessons", &["id", "trigger", "embedding", "valid_to"]),
    (
        "artifacts",
        &["id", "relpath", "store_id", "size", "source_mtime"],
    ),
    (
        "artifact_stores",
        &["id", "root_path", "managed", "deep_index"],
    ),
    (
        "artifact_index",
        &["artifact_id", "text", "embedding", "updated_at"],
    ),
    ("memory_fts", &["summary"]),
    ("lessons_fts", &["trigger"]),
    ("artifact_index_fts", &["text"]),
];
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn safe_relative(s: &str) -> Result<PathBuf> {
    require(
        !s.is_empty() && !s.contains('\\') && !s.contains(':') && !s.contains('\0'),
        "unsafe relative artifact path",
    )?;
    let p = Path::new(s);
    require(
        p.components().all(|v| matches!(v, Component::Normal(_))),
        "unsafe relative artifact path",
    )?;
    Ok(p.to_path_buf())
}
// Registered roots are data, not filesystem paths to resolve. Reject aliases
// lexically so overlap checks never need to touch an external folder.
fn external_root(path: &str) -> Result<PathBuf> {
    let normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    require(
        (normalized.starts_with('/') || drive) && !normalized.contains('\0'),
        "external root must be absolute",
    )?;
    require(
        !normalized.split('/').any(|p| p == "." || p == ".."),
        "external root must not contain dot or parent aliases",
    )?;
    Ok(PathBuf::from(normalized))
}
fn check_external_roots(c: &Connection, selected: &[&Path]) -> Result<u64> {
    let stores = c
        .prepare("SELECT root_path,managed FROM artifact_stores")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut count = 0;
    for (path, managed) in stores {
        require(
            managed == 0 || managed == 1,
            "invalid artifact store managed flag",
        )?;
        if managed == 0 {
            count += 1;
            let external = external_root(&path)?;
            require(selected.iter().all(|root|!root.starts_with(&external)&&!external.starts_with(root)),
                "external root overlaps a selected copy or destination; refuse external file access")?;
        }
    }
    Ok(count)
}
fn safe_root(p: &Path) -> Result<PathBuf> {
    // Reject symlinks in every component, including ancestors.
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let mut walk = PathBuf::new();
    for c in abs.components() {
        require(
            !matches!(c, Component::ParentDir),
            "parent traversal is not accepted",
        )?;
        walk.push(c);
        require(
            !fs::symlink_metadata(&walk)?.file_type().is_symlink(),
            "symlink roots are not accepted",
        )?;
    }
    require(abs.is_dir(), "selected root is not a directory")?;
    Ok(fs::canonicalize(abs)?)
}
fn mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if meta.permissions().readonly() {
            0o400
        } else {
            0o600
        }
    }
}
fn read_file(p: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let meta = fs::symlink_metadata(p)?;
    require(
        meta.is_file() && !meta.file_type().is_symlink(),
        "only regular copied files are accepted",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        require(
            meta.nlink() == 1,
            "hard-linked files are not offline copies",
        )?;
    }
    require(
        meta.len() <= LIMIT,
        "file exceeds 256 MiB per-file preservation limit; no files imported",
    )?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = options.open(p)?;
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::Read::by_ref(&mut f)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= LIMIT,
        "file exceeds preservation limit",
    )?;
    Ok(bytes)
}
fn record(p: &Path) -> Result<FileRecord> {
    let bytes = read_file(p)?;
    Ok(FileRecord {
        bytes: bytes.len() as u64,
        sha256: digest(&bytes),
        mode: mode(&fs::symlink_metadata(p)?),
    })
}
fn walk(root: &Path, skip: Option<&Path>) -> Result<BTreeMap<String, FileRecord>> {
    fn visit(
        root: &Path,
        p: &Path,
        skip: Option<&Path>,
        out: &mut BTreeMap<String, FileRecord>,
        depth: usize,
        entries: &mut usize,
        bytes: &mut u64,
    ) -> Result<()> {
        require(depth <= 32, "copied directory depth exceeds limit")?;
        for entry in fs::read_dir(p)? {
            *entries += 1;
            require(
                *entries <= 10000,
                "copied directory entry count exceeds limit",
            )?;
            let entry = entry?;
            let path = entry.path();
            if skip == Some(path.as_path()) {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            require(
                !meta.file_type().is_symlink(),
                "symlinks in copies are refused",
            )?;
            if meta.is_dir() {
                visit(root, &path, skip, out, depth + 1, entries, bytes)?
            } else {
                *bytes = bytes
                    .checked_add(meta.len())
                    .ok_or(Error("copied byte count overflow"))?;
                require(*bytes <= LIMIT, "copied directory exceeds 256 MiB limit")?;
                let name = path
                    .strip_prefix(root)
                    .map_err(|_| Error("path escaped copy"))?
                    .to_str()
                    .ok_or(Error("non-UTF8 copied paths are not supported"))?
                    .to_owned();
                safe_relative(&name)?;
                out.insert(name, record(&path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, skip, &mut out, 0, &mut 0, &mut 0)?;
    Ok(out)
}
fn private_temp(prefix: &str, parent: Option<&Path>) -> Result<TempDir> {
    let mut builder = Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(match parent {
        Some(p) => builder.tempdir_in(p)?,
        None => builder.tempdir()?,
    })
}
fn private_dir(p: &Path) -> Result<()> {
    fs::create_dir(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn write_new(p: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(p)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
fn open_cipher(path: &Path, keys: &Keys, create: bool) -> Result<Connection> {
    if create {
        write_new(path, &[])?;
    }
    let flags = if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let c = Connection::open_with_flags(path, flags)?;
    // sqlite3_key avoids a formatted SQL statement containing the supplied secret.
    let rc = unsafe {
        rusqlite::ffi::sqlite3_key(c.handle(), keys.db.as_ptr().cast(), keys.db.len() as i32)
    };
    require(
        rc == rusqlite::ffi::SQLITE_OK,
        "SQLCipher key initialization failed",
    )?;
    let version: String = c
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .map_err(|_| Error("SQLCipher is unavailable; refusing plaintext SQLite"))?;
    require(
        version.starts_with("4."),
        "unsupported SQLCipher version; requires 4.x",
    )?;
    c.execute_batch("PRAGMA cipher_memory_security=ON; PRAGMA temp_store=MEMORY; PRAGMA trusted_schema=OFF; PRAGMA busy_timeout=1000;")?;
    let _: i64 = c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
    Ok(c)
}
fn integrity(c: &Connection) -> Result<()> {
    let mut stmt = c.prepare("PRAGMA cipher_integrity_check")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    require(
        rows.is_empty() || rows.iter().all(|s| s == "ok"),
        "SQLCipher page authentication failed",
    )?;
    let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    require(integrity == "ok", "database integrity check failed")?;
    require(
        c.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "foreign-key reconciliation failed",
    )?;
    Ok(())
}
fn checks(c: &Connection) -> Result<()> {
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    require(
        version == 25,
        "unsupported source schema; only original schema 25 is accepted",
    )?;
    integrity(c)?;
    for (table, required) in REQUIRED {
        let mut stmt = c.prepare(&format!("PRAGMA table_info({})", quote(table)))?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?;
        require(
            required.iter().all(|v| cols.contains(*v)),
            "schema25 required table/column contract is missing",
        )?;
    }
    for sql in [
        "SELECT count(*) FROM messages m LEFT JOIN sessions s ON s.id=m.session_id WHERE s.id IS NULL",
        "SELECT count(*) FROM messages m LEFT JOIN messages p ON p.id=m.parent_id WHERE m.parent_id IS NOT NULL AND (p.id IS NULL OR p.session_id<>m.session_id OR p.seq>=m.seq)",
        "SELECT count(*) FROM (SELECT session_id,seq FROM messages GROUP BY session_id,seq HAVING count(*)>1)",
        "SELECT count(*) FROM artifacts a LEFT JOIN artifact_stores s ON s.id=a.store_id WHERE s.id IS NULL",
    ] { let n:i64=c.query_row(sql,[],|r|r.get(0))?; require(n==0,"message graph, sequence, session or artifact-store reconciliation failed")?; }
    // FTS5 external-content integrity check does not rebuild or discard indexes.
    for table in ["memory_fts", "lessons_fts", "artifact_index_fts"] {
        c.execute_batch("SAVEPOINT migration_fts_check")?;
        let result = c.execute(
            &format!(
                "INSERT INTO {}({}, rank) VALUES('integrity-check', 1)",
                quote(table),
                quote(table)
            ),
            [],
        );
        c.execute_batch("ROLLBACK TO migration_fts_check; RELEASE migration_fts_check")?;
        result.map_err(|_| Error("FTS index does not reconcile with external content"))?;
    }
    Ok(())
}
fn row_digest(row: &rusqlite::Row<'_>, n: usize) -> rusqlite::Result<[u8; 32]> {
    let mut h = Sha256::new();
    for i in 0..n {
        match row.get_ref(i)? {
            ValueRef::Null => h.update([0]),
            ValueRef::Integer(v) => {
                h.update([1]);
                h.update(v.to_le_bytes());
            }
            ValueRef::Real(v) => {
                h.update([2]);
                h.update(v.to_bits().to_le_bytes());
            }
            ValueRef::Text(v) => {
                h.update([3]);
                h.update((v.len() as u64).to_le_bytes());
                h.update(v);
            }
            ValueRef::Blob(v) => {
                h.update([4]);
                h.update((v.len() as u64).to_le_bytes());
                h.update(v);
            }
        }
    }
    Ok(h.finalize().into())
}
fn table_record(c: &Connection, table: &str) -> Result<TableRecord> {
    // Include rowids where present; they are part of FTS external-content identity.
    let sql = if c
        .prepare(&format!("SELECT rowid FROM {} LIMIT 0", quote(table)))
        .is_ok()
    {
        format!("SELECT rowid,* FROM {}", quote(table))
    } else {
        format!("SELECT * FROM {}", quote(table))
    };
    let mut stmt = c.prepare(&sql)?;
    let count = stmt.column_count();
    let mut hashes = stmt
        .query_map([], |r| row_digest(r, count))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    hashes.sort_unstable();
    let mut h = Sha256::new();
    for v in &hashes {
        h.update(v);
    }
    Ok(TableRecord {
        rows: hashes.len() as u64,
        sha256: hex(&h.finalize()),
    })
}
fn database_record(c: &Connection) -> Result<DatabaseRecord> {
    let mut schema =
        c.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")?;
    let hashes = schema
        .query_map([], |r| row_digest(r, 4))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut h = Sha256::new();
    for v in hashes {
        h.update(v);
    }
    let tables = c
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut records = BTreeMap::new();
    for t in tables {
        records.insert(t.clone(), table_record(c, &t)?);
    }
    Ok(DatabaseRecord {
        schema_version: 25,
        schema_sha256: hex(&h.finalize()),
        tables: records,
    })
}
fn checkpoint(c: &Connection) -> Result<()> {
    let busy: i64 = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    require(busy == 0, "copied WAL could not be checkpointed")?;
    let mode: String = c.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    require(mode == "delete", "copied journal mode could not be sealed")?;
    Ok(())
}
fn copy_database(source: &Path, stage: &Path) -> Result<()> {
    let db = read_file(&source.join("smith.db"))?;
    require(
        db.len() >= 512 && !db.starts_with(b"SQLite format 3\0"),
        "source is plaintext, empty or truncated; refusing import",
    )?;
    write_new(&stage.join("smith.db"), &db)?;
    let wal = source.join("smith.db-wal");
    if wal.exists() {
        write_new(&stage.join("smith.db-wal"), &read_file(&wal)?)?;
    }
    // Never copy SHM: SQLCipher reconstructs it from the copied WAL privately.
    require(
        !source.join("smith.db-journal").exists(),
        "hot rollback journal requires recovery of the offline copy before import",
    )?;
    Ok(())
}
fn verify_blob(keys: &Keys, id: &str, sealed: &[u8], size: Option<i64>) -> Result<()> {
    decrypt_blob(keys, id, sealed, size).map(|_| ())
}
fn decrypt_blob(
    keys: &Keys,
    id: &str,
    sealed: &[u8],
    size: Option<i64>,
) -> Result<Zeroizing<Vec<u8>>> {
    require(sealed.len() >= 28, "managed artifact is truncated")?;
    let cipher = ChaCha20Poly1305::new_from_slice(&*keys.blob)
        .map_err(|_| Error("blob key initialization failed"))?;
    // Original artifact_gen persists UUID::as_bytes() (16 bytes) as AAD,
    // and records the sealed byte length, including nonce and authentication tag.
    let artifact_id = uuid::Uuid::parse_str(id).map_err(|_| Error("artifact ID is not a UUID"))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &Nonce::try_from(&sealed[..12]).map_err(|_| Error("invalid blob nonce"))?,
                Payload {
                    msg: &sealed[12..],
                    aad: artifact_id.as_bytes(),
                },
            )
            .map_err(|_| {
                Error("managed artifact authentication failed; check blob key and copied bytes")
            })?,
    );
    if let Some(size) = size {
        require(
            size >= 0 && size as usize == sealed.len(),
            "managed artifact size does not match metadata",
        )?;
    }
    Ok(plaintext)
}

/// Read-only bridge for a separately implemented application importer. Only a
/// complete, verified preservation output is accepted. The keyed database is
/// copied privately, and the old key is never retained by this object.
pub struct VerifiedCopy {
    database: Connection,
    root: PathBuf,
    manifest: Manifest,
    _temporary: TempDir,
}
impl VerifiedCopy {
    pub fn database(&self) -> &Connection {
        &self.database
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    /// Validate a later application publication root against retained external
    /// registrations without stat'ing or opening any registered external path.
    pub fn require_disjoint_destination(&self, destination: &Path) -> Result<()> {
        check_external_roots(&self.database, &[destination]).map(|_| ())
    }

    /// Returns authenticated original managed bytes, or Smith-derived bytes
    /// for an external reference. It never opens a registered external path.
    pub fn artifact_bytes(&self, id: &str, keys: &Keys) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let (rel, size, managed): (String, i64, i64) = self.database.query_row(
            "SELECT a.relpath,a.size,s.managed FROM artifacts a JOIN artifact_stores s ON s.id=a.store_id WHERE a.id=?1",
            [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let relative = if managed == 1 {
            safe_relative(&rel)?
        } else {
            let id = uuid::Uuid::parse_str(id)
                .map_err(|_| Error("artifact ID is not a UUID"))?
                .simple()
                .to_string();
            PathBuf::from(format!("{}/{}/{}.bin", &id[..2], &id[2..4], id))
        };
        let name = relative.to_str().ok_or(Error("invalid managed path"))?;
        let Some(expected) = self.manifest.artifact_files.get(name) else {
            require(managed != 1, "referenced managed artifact is missing")?;
            return Ok(None);
        };
        let bytes = read_file(&self.root.join("artifacts").join(relative))?;
        require(
            bytes.len() as u64 == expected.bytes && digest(&bytes) == expected.sha256,
            "verified artifact changed before import",
        )?;
        Ok(Some(decrypt_blob(
            keys,
            id,
            &bytes,
            (managed == 1).then_some(size),
        )?))
    }
}
pub fn open_verified_copy(root: &Path, keys: &Keys) -> Result<VerifiedCopy> {
    let manifest = verify(root, keys)?;
    let temporary = private_temp("smith-verified-import-", None)?;
    copy_database(root, temporary.path())?;
    let database = open_cipher(&temporary.path().join("smith.db"), keys, false)?;
    checks(&database)?;
    require(
        database_record(&database)? == manifest.database,
        "verified database changed before import",
    )?;
    database.execute_batch("PRAGMA query_only=ON;")?;
    Ok(VerifiedCopy {
        database,
        root: safe_root(root)?,
        manifest,
        _temporary: temporary,
    })
}
fn sync_tree(path: &Path) -> Result<()> {
    for e in fs::read_dir(path)? {
        let p = e?.path();
        if p.is_dir() {
            sync_tree(&p)?;
        } else {
            File::open(p)?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}
fn destination_parent(destination: &Path) -> Result<(PathBuf, PathBuf)> {
    let parent = safe_root(
        destination
            .parent()
            .ok_or(Error("destination needs a parent directory"))?,
    )?;
    let name = destination
        .file_name()
        .ok_or(Error("invalid destination"))?;
    let path = parent.join(name);
    require(
        !path.exists() && fs::symlink_metadata(&path).is_err(),
        "destination already exists; it will never be overwritten",
    )?;
    Ok((parent, path))
}
fn publish(stage: TempDir, destination: &Path) -> Result<()> {
    // No-replace rename is required; ordinary rename can overwrite an empty directory.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(stage.path().as_os_str().as_bytes())
            .map_err(|_| Error("invalid stage path"))?;
        let to = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| Error("invalid destination path"))?;
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        require(
            rc == 0,
            "atomic destination publication failed; destination was not replaced",
        )?;
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(stage.path().as_os_str().as_bytes())
            .map_err(|_| Error("invalid stage path"))?;
        let to = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| Error("invalid destination path"))?;
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        require(
            rc == 0,
            "atomic destination publication failed; destination was not replaced",
        )?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err(Error(
            "atomic publication is currently supported on macOS/Linux only",
        ));
    }
    File::open(destination.parent().ok_or(Error("destination has no parent"))?)
        .and_then(|parent|parent.sync_all())
        .map_err(|_|Error("destination was published, but durability could not be confirmed; retain and verify it before retrying"))?;
    Ok(())
}

/// Import only caller-selected, quiesced copies. Never open a source SQLite connection.
pub fn run(request: &Request<'_>, keys: &Keys, operation: Operation) -> Result<Manifest> {
    require(
        request.offline_copy_attested,
        "explicit offline-copy attestation is required",
    )?;
    let source = safe_root(request.source_copy)?;
    let managed = safe_root(request.managed_copy)?;
    let (parent, destination) = destination_parent(request.destination)?;
    require(
        !destination.starts_with(&source)
            && !destination.starts_with(&managed)
            && !source.starts_with(&destination)
            && !managed.starts_with(&destination),
        "source and destination must not overlap",
    )?;
    require(
        !source.starts_with(&managed)
            && (!managed.starts_with(&source) || managed == source.join("artifacts")),
        "managed copy must be artifacts/ or a separate copied root",
    )?;
    let skip = if managed.starts_with(&source) {
        Some(managed.as_path())
    } else {
        None
    };
    let mut initial_database = BTreeMap::new();
    initial_database.insert("smith.db".to_owned(), record(&source.join("smith.db"))?);
    if source.join("smith.db-wal").exists() {
        initial_database.insert(
            "smith.db-wal".to_owned(),
            record(&source.join("smith.db-wal"))?,
        );
    }
    let scratch = private_temp("smith-migration-inspect-", None)?;
    let work = scratch.path().join("work");
    private_dir(&work)?;
    copy_database(&source, &work)?;
    for name in ["smith.db", "smith.db-wal"] {
        if let Some(expected) = initial_database.get(name) {
            let actual = record(&work.join(name))?;
            require(
                expected.sha256 == actual.sha256 && expected.bytes == actual.bytes,
                "database copy changed during capture",
            )?;
        }
    }
    let c = open_cipher(&work.join("smith.db"), keys, false)?;
    checks(&c)?;
    let before = database_record(&c)?;
    let cipher_version: String = c.query_row("PRAGMA cipher_version", [], |r| r.get(0))?;
    let external_stores = check_external_roots(&c, &[&source, &managed, &destination])?;
    let stage = private_temp(".smith-migration-stage-", Some(&parent))?;
    let source_files = walk(&source, skip)?;
    let artifact_files = walk(&managed, None)?;
    for (name, expected) in initial_database {
        require(
            source_files.get(&name) == Some(&expected),
            "database changed during inventory",
        )?;
    }
    let blobs=c.prepare("SELECT a.id,a.relpath,a.size,s.managed FROM artifacts a JOIN artifact_stores s ON s.id=a.store_id ORDER BY a.id")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let mut verified = BTreeSet::new();
    let mut external_artifacts = 0;
    for (id, rel, size, is_managed) in blobs {
        let candidate = if is_managed == 1 {
            Some((safe_relative(&rel)?, Some(size)))
        } else {
            external_artifacts += 1;
            // External rows may have a Smith-derived sealed text blob in the managed root.
            let uuid = uuid::Uuid::parse_str(&id)
                .map_err(|_| Error("artifact ID is not a UUID"))?
                .simple()
                .to_string();
            let derived = PathBuf::from(format!("{}/{}/{}.bin", &uuid[..2], &uuid[2..4], uuid));
            if managed.join(&derived).exists() {
                Some((derived, None))
            } else {
                None
            }
        };
        if let Some((rel, size)) = candidate {
            let name = rel.to_str().ok_or(Error("invalid managed path"))?;
            require(
                artifact_files.contains_key(name),
                "referenced managed artifact is missing",
            )?;
            let bytes = read_file(&managed.join(&rel))?;
            verify_blob(keys, &id, &bytes, size)?;
            require(
                verified.insert(name.to_owned()),
                "multiple artifact records reference the same blob",
            )?;
            if operation == Operation::Migrate {
                write_new(&stage.path().join("artifacts").join(rel), &bytes)?;
            }
        }
    }
    let ancillary_files = source_files
        .keys()
        .filter(|p| !matches!(p.as_str(), "smith.db" | "smith.db-wal" | "smith.db-shm"))
        .count() as u64
        + (artifact_files.len() - verified.len()) as u64;
    let manifest = Manifest {
        format: 1,
        cipher_version,
        database: before.clone(),
        source_files,
        artifact_files,
        verified_blobs: verified.len() as u64,
        external_stores,
        external_artifacts,
        ancillary_files,
        offline_copy_attested: true,
        runtime_activation:
            "disabled; encrypted preservation profile requires an encryption-capable runtime".into(),
    };
    checkpoint(&c)?;
    require(
        before == database_record(&c)?,
        "database rows or schema changed during checkpoint reconciliation",
    )?;
    drop(c);
    if operation == Operation::Migrate {
        write_new(
            &stage.path().join("smith.db"),
            &read_file(&work.join("smith.db"))?,
        )?;
    }
    drop(scratch);
    let mut vault = open_cipher(&stage.path().join("migration.db"), keys, true)?;
    vault.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    let tx = vault.transaction()?;
    tx.execute_batch("CREATE TABLE manifest(singleton INTEGER PRIMARY KEY CHECK(singleton=1), document TEXT NOT NULL); CREATE TABLE preserved_files(namespace TEXT NOT NULL,path TEXT NOT NULL,mode INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(namespace,path)); CREATE TABLE migration_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),kind TEXT NOT NULL,runtime_enabled INTEGER NOT NULL CHECK(runtime_enabled=0));")?;
    let serialized = Zeroizing::new(serde_json::to_string(&manifest)?);
    tx.execute("INSERT INTO manifest VALUES(1,?1)", [serialized.as_str()])?;
    tx.execute(
        "INSERT INTO migration_state VALUES(1,?1,0)",
        [if operation == Operation::Migrate {
            "migration"
        } else {
            "preflight"
        }],
    )?;
    if operation == Operation::Migrate {
        for (name, record) in &manifest.source_files {
            if matches!(name.as_str(), "smith.db" | "smith.db-wal" | "smith.db-shm") {
                continue;
            }
            let bytes = read_file(&source.join(safe_relative(name)?))?;
            tx.execute(
                "INSERT INTO preserved_files VALUES('profile',?1,?2,?3)",
                params![name, record.mode, bytes.as_slice()],
            )?;
        }
        for (name, record) in &manifest.artifact_files {
            if verified.contains(name) {
                continue;
            }
            let bytes = read_file(&managed.join(safe_relative(name)?))?;
            tx.execute(
                "INSERT INTO preserved_files VALUES('artifacts',?1,?2,?3)",
                params![name, record.mode, bytes.as_slice()],
            )?;
        }
    }
    tx.commit()?;
    drop(vault);
    require(
        fs::metadata(stage.path().join("migration.db"))?.len() <= LIMIT,
        "encrypted ancillary vault exceeds 256 MiB verification limit; no destination published",
    )?;
    require(
        manifest.source_files == walk(&source, skip)?
            && manifest.artifact_files == walk(&managed, None)?,
        "source changed while migrating; stage discarded, repeat with a quiesced copy",
    )?;
    if operation == Operation::Migrate {
        verify_internal(stage.path(), keys)?;
    }
    write_new(&stage.path().join("READ-ME.txt"),b"Encrypted offline preservation profile. Runtime activation is disabled. Do not open with the plaintext Smith Next app. migration.db contains the encrypted manifest and ancillary files. Original key custody remains required.\n")?;
    sync_tree(stage.path())?;
    publish(stage, &destination)?;
    Ok(manifest)
}

fn read_manifest(vault: &Connection) -> Result<Manifest> {
    integrity(vault)?;
    let serialized = Zeroizing::new(vault.query_row(
        "SELECT document FROM manifest WHERE singleton=1",
        [],
        |r| r.get::<_, String>(0),
    )?);
    let m: Manifest = serde_json::from_str(&serialized)?;
    require(m.format == 1, "unknown migration manifest version")?;
    Ok(m)
}
fn verify_internal(root: &Path, keys: &Keys) -> Result<Manifest> {
    let vault = open_cipher(&root.join("migration.db"), keys, false)?;
    let m = read_manifest(&vault)?;
    let kind: String = vault.query_row(
        "SELECT kind FROM migration_state WHERE singleton=1 AND runtime_enabled=0",
        [],
        |r| r.get(0),
    )?;
    require(
        kind == "migration",
        "preflight is an inventory only, not a migrated profile",
    )?;
    let c = open_cipher(&root.join("smith.db"), keys, false)?;
    checks(&c)?;
    require(
        database_record(&c)? == m.database,
        "database differs from preservation manifest",
    )?;
    let mut blob_rows=c.prepare("SELECT a.id,a.relpath,a.size,s.managed FROM artifacts a JOIN artifact_stores s ON s.id=a.store_id")?;
    let blobs = blob_rows
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, rel, size, managed) in blobs {
        let rel = if managed == 1 {
            safe_relative(&rel)?
        } else {
            let uuid = uuid::Uuid::parse_str(&id)
                .map_err(|_| Error("artifact ID is not a UUID"))?
                .simple()
                .to_string();
            PathBuf::from(format!("{}/{}/{}.bin", &uuid[..2], &uuid[2..4], uuid))
        };
        let blob = root.join("artifacts").join(&rel);
        if managed == 1 || blob.exists() {
            verify_blob(
                keys,
                &id,
                &read_file(&blob)?,
                if managed == 1 { Some(size) } else { None },
            )?;
        }
    }
    drop(blob_rows);
    checkpoint(&c)?;
    drop(c);
    let mut vault_count = 0;
    for namespace in ["profile", "artifacts"] {
        let mut stmt = vault.prepare(
            "SELECT path,mode,bytes FROM preserved_files WHERE namespace=?1 ORDER BY path",
        )?;
        let mut rows = stmt.query([namespace])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            safe_relative(&path)?;
            let mode: u32 = row.get(1)?;
            let bytes = Zeroizing::new(row.get::<_, Vec<u8>>(2)?);
            let expected = if namespace == "profile" {
                m.source_files.get(&path)
            } else {
                m.artifact_files.get(&path)
            }
            .ok_or(Error("unexpected preserved file"))?;
            require(
                expected.sha256 == digest(&bytes)
                    && expected.bytes == bytes.len() as u64
                    && expected.mode == mode,
                "preserved ancillary bytes do not match manifest",
            )?;
            vault_count += 1;
        }
    }
    require(
        vault_count == m.ancillary_files,
        "ancillary-file reconciliation failed",
    )?;
    let artifact_root = root.join("artifacts");
    let actual = if artifact_root.exists() {
        walk(&artifact_root, None)?
    } else {
        BTreeMap::new()
    };
    require(
        actual.len() as u64 == m.verified_blobs,
        "managed blob count differs from manifest",
    )?;
    for (name, rec) in actual {
        let expected = m
            .artifact_files
            .get(&name)
            .ok_or(Error("unexpected managed blob"))?;
        require(
            rec.sha256 == expected.sha256 && rec.bytes == expected.bytes,
            "managed blob bytes differ from manifest",
        )?;
    }
    // Every source file must be represented exactly once, rather than count-only reconciliation.
    for name in m
        .source_files
        .keys()
        .filter(|p| !matches!(p.as_str(), "smith.db" | "smith.db-wal" | "smith.db-shm"))
    {
        let count: i64 = vault.query_row(
            "SELECT count(*) FROM preserved_files WHERE namespace='profile' AND path=?1",
            [name],
            |r| r.get(0),
        )?;
        require(count == 1, "missing preserved profile file")?;
    }
    for name in m.artifact_files.keys() {
        let count: i64 = vault.query_row(
            "SELECT count(*) FROM preserved_files WHERE namespace='artifacts' AND path=?1",
            [name],
            |r| r.get(0),
        )?;
        require(
            count + i64::from(artifact_root.join(name).is_file()) == 1,
            "missing or duplicate preserved artifact",
        )?;
    }
    Ok(m)
}
/// Verification itself works on a private copy, leaving the selected profile unchanged.
pub fn verify(profile_copy: &Path, keys: &Keys) -> Result<Manifest> {
    let profile = safe_root(profile_copy)?;
    // Inspect copied metadata before walking any potential registered subfolder.
    let inspect = private_temp("smith-verify-inspect-", None)?;
    copy_database(&profile, inspect.path())?;
    let database = open_cipher(&inspect.path().join("smith.db"), keys, false)?;
    check_external_roots(&database, &[&profile])?;
    drop(database);
    drop(inspect);
    let records = walk(&profile, None)?;
    let temp = private_temp("smith-verify-", None)?;
    for name in records.keys() {
        write_new(
            &temp.path().join(safe_relative(name)?),
            &read_file(&profile.join(name))?,
        )?;
    }
    let result = verify_internal(temp.path(), keys)?;
    require(
        records == walk(&profile, None)?,
        "profile changed during verification",
    )?;
    Ok(result)
}
#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub exact: bool,
    pub changed_tables: Vec<String>,
    pub added_tables: Vec<String>,
    pub removed_tables: Vec<String>,
    pub schema_changed: bool,
    pub changed_files: usize,
    pub rollback_safe_without_losing_writes: bool,
}
/// Compare a later offline original-format copy against an encrypted baseline manifest.
/// Reports changes only; never merges, activates a runtime or overwrites either profile.
pub fn reconcile(baseline_copy: &Path, current_copy: &Path, keys: &Keys) -> Result<Reconciliation> {
    let baseline = safe_root(baseline_copy)?;
    let current = safe_root(current_copy)?;
    // Inventory equality is meaningful for rollback only if the actual encrypted
    // baseline database, artifacts and ancillary vault remain recoverable.
    let m = verify(&baseline, keys)?;
    let baseline_before = walk(&baseline, None)?;
    let temp = private_temp("smith-reconcile-", None)?;
    let dbdir = temp.path().join("current");
    private_dir(&dbdir)?;
    copy_database(&current, &dbdir)?;
    let c = open_cipher(&dbdir.join("smith.db"), keys, false)?;
    checks(&c)?;
    check_external_roots(&c, &[&current])?;
    let current_before = walk(&current, None)?;
    let now = database_record(&c)?;
    let changed_tables = now
        .tables
        .iter()
        .filter(|(name, value)| {
            m.database
                .tables
                .get(*name)
                .is_some_and(|old| old != *value)
        })
        .map(|(n, _)| n.clone())
        .collect::<Vec<_>>();
    let added_tables = now
        .tables
        .keys()
        .filter(|n| !m.database.tables.contains_key(*n))
        .cloned()
        .collect::<Vec<_>>();
    let removed_tables = m
        .database
        .tables
        .keys()
        .filter(|n| !now.tables.contains_key(*n))
        .cloned()
        .collect::<Vec<_>>();
    let schema_changed = now.schema_sha256 != m.database.schema_sha256;
    let mut expected = BTreeMap::new();
    for (n, r) in &m.source_files {
        if !matches!(n.as_str(), "smith.db" | "smith.db-wal" | "smith.db-shm") {
            expected.insert(n.clone(), r.clone());
        }
    }
    for (n, r) in &m.artifact_files {
        expected.insert(format!("artifacts/{n}"), r.clone());
    }
    let actual = current_before
        .iter()
        .filter(|(n, _)| !matches!(n.as_str(), "smith.db" | "smith.db-wal" | "smith.db-shm"))
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect::<BTreeMap<_, _>>();
    let names: BTreeSet<_> = expected.keys().chain(actual.keys()).collect();
    let changed_files = names
        .iter()
        .filter(|n| expected.get(**n) != actual.get(**n))
        .count();
    let exact = changed_tables.is_empty()
        && added_tables.is_empty()
        && removed_tables.is_empty()
        && !schema_changed
        && changed_files == 0;
    require(
        baseline_before == walk(&baseline, None)? && current_before == walk(&current, None)?,
        "a reconciliation input changed during the check",
    )?;
    Ok(Reconciliation {
        exact,
        changed_tables,
        added_tables,
        removed_tables,
        schema_changed,
        changed_files,
        rollback_safe_without_losing_writes: exact,
    })
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn atomic_publish_refuses_a_destination_created_after_preflight() {
        let root = private_temp("smith-publish-test-", None).unwrap();
        let dest = root.path().join("target");
        let stage = private_temp(".stage-", Some(root.path())).unwrap();
        let staged_path = stage.path().to_owned();
        write_new(
            &stage.path().join("payload"),
            b"encrypted fixture placeholder",
        )
        .unwrap();
        private_dir(&dest).unwrap();
        assert!(publish(stage, &dest).is_err());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
        assert!(!staged_path.exists());
    }
}
