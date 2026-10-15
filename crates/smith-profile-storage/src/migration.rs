use crate::{
    files, integrity, open_cipher, require, schema, seal, Error, Result, Secret, DB_FILE,
    PLAINTEXT_FILE, SCHEMA_VERSION,
};
use files::{FileRecord, LIMIT};
use rusqlite::{params, params_from_iter, types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use zeroize::Zeroizing;

const VAULT: &str = "migration.db";
pub struct MigrationRequest<'a> {
    pub source_copy: &'a Path,
    pub destination: &'a Path,
    pub offline_copy_attested: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationReport {
    pub migration_id: String,
    pub profile_id: String,
    pub source_schema: i64,
    pub target_schema: i64,
    pub tables: u64,
    pub rows: u64,
    pub files: u64,
    pub baseline_unchanged: bool,
    pub later_writes: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct RollbackReport {
    pub eligible: bool,
    pub later_writes: u64,
    pub source_unchanged: bool,
    pub destination_unchanged: bool,
    pub migration_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TableRecord {
    rows: u64,
    sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DatabaseRecord {
    version: i64,
    schema_sha256: String,
    tables: BTreeMap<String, TableRecord>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    report: MigrationReport,
    cipher_version: String,
    source_files: BTreeMap<String, FileRecord>,
    source_database: DatabaseRecord,
    destination_database: DatabaseRecord,
    baseline_generation: i64,
}

fn row_digest(row: &rusqlite::Row<'_>, count: usize) -> rusqlite::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    for column in 0..count {
        match row.get_ref(column)? {
            ValueRef::Null => hash.update([0]),
            ValueRef::Integer(value) => {
                hash.update([1]);
                hash.update(value.to_le_bytes());
            }
            ValueRef::Real(value) => {
                hash.update([2]);
                hash.update(value.to_bits().to_le_bytes());
            }
            ValueRef::Text(value) => {
                hash.update([3]);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value);
            }
            ValueRef::Blob(value) => {
                hash.update([4]);
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value);
            }
        }
    }
    Ok(hash.finalize().into())
}
fn rowid(db: &Connection, table: &str) -> Result<Option<String>> {
    let without: i64 = db.query_row(
        "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
        [table],
        |r| r.get(0),
    )?;
    if without != 0 {
        return Ok(None);
    }
    let names: BTreeSet<String> = schema::columns(db, table)?
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let name = ["_rowid_", "rowid", "oid"]
        .into_iter()
        .find(|s| !names.contains(*s))
        .ok_or(Error(
            "All implicit rowid names are shadowed; exact copy is unsupported.",
        ))?;
    Ok(Some(name.into()))
}
fn record_query(db: &Connection, sql: &str) -> Result<TableRecord> {
    let mut statement = db.prepare(sql)?;
    let count = statement.column_count();
    let mut hashes = statement
        .query_map([], |row| row_digest(row, count))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    hashes.sort_unstable();
    let mut hash = Sha256::new();
    for item in &hashes {
        hash.update(item);
    }
    Ok(TableRecord {
        rows: hashes.len() as u64,
        sha256: files::hex(&hash.finalize()),
    })
}
fn table_record(db: &Connection, table: &str) -> Result<TableRecord> {
    let prefix = rowid(db, table)?
        .map(|s| format!("{},", schema::quote(&s)))
        .unwrap_or_default();
    record_query(
        db,
        &format!("SELECT {prefix}* FROM {}", schema::quote(table)),
    )
}
pub(crate) fn database_record(db: &Connection) -> Result<DatabaseRecord> {
    let schema = record_query(
        db,
        "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name",
    )?;
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut records = BTreeMap::new();
    for table in tables {
        records.insert(table.clone(), table_record(db, &table)?);
    }
    Ok(DatabaseRecord {
        version: db.query_row("PRAGMA user_version", [], |r| r.get(0))?,
        schema_sha256: schema.sha256,
        tables: records,
    })
}
fn copy_rows(source: &Connection, destination: &Connection, table: &str) -> Result<()> {
    let mut names = source
        .prepare(&format!("PRAGMA table_xinfo({})", schema::quote(table)))?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(6)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(_, hidden)| *hidden == 0)
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if let Some(rowid) = rowid(source, table)? {
        names.insert(0, rowid);
    }
    require(!names.is_empty(), "A copied table has no writable columns.")?;
    let names = names
        .iter()
        .map(|s| schema::quote(s))
        .collect::<Vec<_>>()
        .join(",");
    let select = format!("SELECT {names} FROM {}", schema::quote(table));
    let mut statement = source.prepare(&select)?;
    let count = statement.column_count();
    let placeholders = (1..=count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let insert = format!(
        "INSERT INTO {}({names}) VALUES({placeholders})",
        schema::quote(table)
    );
    let mut target = destination.prepare(&insert)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let values = (0..count)
            .map(|i| row.get_ref(i))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        target.execute(params_from_iter(
            values
                .into_iter()
                .map(rusqlite::types::ToSqlOutput::Borrowed),
        ))?;
    }
    Ok(())
}
fn copy_logical(source: &Connection, destination: &Connection) -> Result<()> {
    let objects = source
        .prepare("SELECT type,name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY rowid")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    destination.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
    let result = (|| {
        for (kind, name, sql) in &objects {
            if kind == "table" && name != "sqlite_sequence" {
                destination.execute_batch(sql)?;
            }
        }
        for (kind, name, _) in &objects {
            if kind == "table" && name != "sqlite_sequence" {
                copy_rows(source, destination, name)?;
            }
        }
        if objects
            .iter()
            .any(|(kind, name, _)| kind == "table" && name == "sqlite_sequence")
        {
            destination.execute("DELETE FROM sqlite_sequence", [])?;
            copy_rows(source, destination, "sqlite_sequence")?;
        }
        for (kind, _, sql) in &objects {
            if kind != "table" {
                destination.execute_batch(sql)?;
            }
        }
        let version: i64 = source.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        destination.pragma_update(None, "user_version", version)?;
        destination.execute_batch("COMMIT; PRAGMA foreign_keys=ON;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = destination.execute_batch("ROLLBACK; PRAGMA foreign_keys=ON;");
    }
    result
}
fn verify_preserved_projection(source: &Connection, destination: &Connection) -> Result<()> {
    let tables = source
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for table in tables {
        let mut names = schema::columns(source, &table)?
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(rowid) = rowid(source, &table)? {
            names.insert(0, rowid);
        }
        let sql = format!(
            "SELECT {} FROM {}",
            names
                .iter()
                .map(|s| schema::quote(s))
                .collect::<Vec<_>>()
                .join(","),
            schema::quote(&table)
        );
        require(
            record_query(source, &sql)? == record_query(destination, &sql)?,
            "Schema migration changed an original value or implicit row identity; source retained.",
        )?;
    }
    let objects=source.prepare("SELECT type,name,sql FROM sqlite_schema WHERE type IN ('index','view','trigger') AND sql IS NOT NULL")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
        .collect::<std::result::Result<Vec<_>,_>>()?;
    for (kind, name, sql) in objects {
        let target: String = destination.query_row(
            "SELECT sql FROM sqlite_schema WHERE type=?1 AND name=?2",
            params![kind, name],
            |r| r.get(0),
        )?;
        require(
            target == sql,
            "Schema migration changed an original index or view; source retained.",
        )?;
    }
    Ok(())
}

/// Imports only explicit, independent, quiesced plaintext Next copies. Unknown
/// files and original SQLite/WAL bytes are preserved inside the encrypted vault.
pub fn migrate_plaintext_copy(
    request: &MigrationRequest<'_>,
    secret: &Secret,
) -> Result<MigrationReport> {
    require(
        request.offline_copy_attested,
        "Confirm that the selected source is an independent, quiesced offline copy.",
    )?;
    let source = files::safe_root(request.source_copy)?;
    let (parent, destination) = files::destination(request.destination)?;
    require(
        !destination.starts_with(&source) && !source.starts_with(&destination),
        "Migration source and destination must be disjoint.",
    )?;
    let before = files::inventory(&source)?;
    require(
        before.contains_key(PLAINTEXT_FILE),
        "The copied plaintext Next database is missing.",
    )?;
    let total = before
        .values()
        .try_fold(0u64, |total, item| total.checked_add(item.bytes))
        .ok_or(Error("Source inventory is too large."))?;
    require(
        total <= LIMIT,
        "Source archive exceeds the 256 MiB bounded migration limit.",
    )?;
    let scratch = files::private_temp("smith-next-plaintext-copy-", None)?;
    files::copy_database(
        &source,
        PLAINTEXT_FILE,
        scratch.path(),
        PLAINTEXT_FILE,
        true,
    )?;
    let plain = Connection::open_with_flags(
        scratch.path().join(PLAINTEXT_FILE),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    plain.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY; PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")?;
    integrity(&plain, false)?;
    let source_version = schema::plain_source(&plain)?;
    let source_database = database_record(&plain)?;
    let stage = files::private_temp("smith-next-encrypted-stage-", Some(&parent))?;
    let encrypted = open_cipher(&stage.path().join(DB_FILE), secret, true)?;
    copy_logical(&plain, &encrypted)?;
    require(
        database_record(&encrypted)? == source_database,
        "Encrypted copy does not match original schema, typed rows and rowids.",
    )?;
    schema::initialize_schema(&encrypted)?;
    verify_preserved_projection(&plain, &encrypted)?;
    schema::install_storage(&encrypted)?;
    schema::validate(&encrypted)?;
    integrity(&encrypted, true)?;
    let (profile_id, generation) = schema::identity(&encrypted)?;
    let database = database_record(&encrypted)?;
    let report = MigrationReport {
        migration_id: uuid::Uuid::new_v4().to_string(),
        profile_id,
        source_schema: source_version,
        target_schema: SCHEMA_VERSION,
        tables: database.tables.len() as u64,
        rows: database.tables.values().map(|v| v.rows).sum(),
        files: before.len() as u64,
        baseline_unchanged: true,
        later_writes: 0,
    };
    let cipher_version: String = encrypted.query_row("PRAGMA cipher_version", [], |r| r.get(0))?;
    let manifest = Manifest {
        format: 1,
        report: report.clone(),
        cipher_version,
        source_files: before.clone(),
        source_database,
        destination_database: database,
        baseline_generation: generation,
    };
    let vault = open_cipher(&stage.path().join(VAULT), secret, true)?;
    vault.execute_batch("BEGIN IMMEDIATE; CREATE TABLE manifest(id INTEGER PRIMARY KEY CHECK(id=1),payload TEXT NOT NULL); CREATE TABLE source_files(path TEXT PRIMARY KEY,mode INTEGER NOT NULL,bytes BLOB NOT NULL);")?;
    let json = Zeroizing::new(serde_json::to_string(&manifest)?);
    vault.execute("INSERT INTO manifest VALUES(1,?1)", [json.as_str()])?;
    for (path, record) in &before {
        let bytes = files::read_file(&source.join(files::safe_relative(path)?))?;
        require(
            files::digest(&bytes) == record.sha256 && bytes.len() as u64 == record.bytes,
            "Source changed while archiving; destination was not published.",
        )?;
        vault.execute(
            "INSERT INTO source_files VALUES(?1,?2,?3)",
            params![path, record.mode, bytes.as_slice()],
        )?;
    }
    vault.execute_batch("COMMIT;")?;
    seal(&encrypted)?;
    seal(&vault)?;
    drop(encrypted);
    drop(vault);
    drop(plain);
    require(
        files::record(&stage.path().join(VAULT))?.bytes <= LIMIT,
        "Encrypted archive exceeds the public verification limit.",
    )?;
    let checked = inspect(stage.path(), secret)?;
    require(
        checked.report.baseline_unchanged,
        "Staged migration failed independent reconciliation.",
    )?;
    require(
        files::inventory(&source)? == before,
        "Source changed during migration; destination was not published.",
    )?;
    files::sync_tree(stage.path())?;
    files::rename_exclusive(stage.path(), &destination)?;
    files::sync_parent(&destination)?;
    Ok(report)
}

struct Inspection {
    report: MigrationReport,
    source_files: BTreeMap<String, FileRecord>,
}
fn inspect(root: &Path, secret: &Secret) -> Result<Inspection> {
    let db = open_cipher(&root.join(DB_FILE), secret, false)?;
    schema::validate(&db)?;
    integrity(&db, true)?;
    let vault = open_cipher(&root.join(VAULT), secret, false)?;
    integrity(&vault, true)?;
    let count: i64 = vault.query_row("SELECT count(*) FROM manifest", [], |r| r.get(0))?;
    require(count == 1, "Migration manifest count is invalid.")?;
    let bytes = Zeroizing::new(vault.query_row(
        "SELECT payload FROM manifest WHERE id=1",
        [],
        |r| r.get::<_, String>(0),
    )?);
    let manifest: Manifest = serde_json::from_str(&bytes)?;
    require(
        manifest.format == 1
            && manifest.cipher_version.starts_with("4.")
            && [6, 7, 8, 9, 10, 11].contains(&manifest.report.target_schema),
        "Unsupported encrypted migration manifest.",
    )?;
    let (profile_id, generation) = schema::identity(&db)?;
    require(
        profile_id == manifest.report.profile_id && generation >= manifest.baseline_generation,
        "Migration profile identity or write generation changed unexpectedly.",
    )?;
    let mut count = 0usize;
    let mut stmt = vault.prepare("SELECT path,mode,bytes FROM source_files ORDER BY path")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        files::safe_relative(&path)?;
        let mode: u32 = row.get(1)?;
        let bytes = Zeroizing::new(row.get::<_, Vec<u8>>(2)?);
        let record = manifest
            .source_files
            .get(&path)
            .ok_or(Error("Archive contains an unexpected file."))?;
        require(
            record.bytes == bytes.len() as u64
                && record.sha256 == files::digest(&bytes)
                && record.mode == mode,
            "Archived source bytes do not match the manifest.",
        )?;
        count += 1;
    }
    require(
        count == manifest.source_files.len() && count as u64 == manifest.report.files,
        "Migration archive is incomplete.",
    )?;
    require(
        manifest.source_files.contains_key(PLAINTEXT_FILE),
        "Original database archive is missing.",
    )?;
    let mut report = manifest.report;
    report.later_writes = (generation - manifest.baseline_generation) as u64;
    report.baseline_unchanged =
        report.later_writes == 0 && database_record(&db)? == manifest.destination_database;
    Ok(Inspection {
        report,
        source_files: manifest.source_files,
    })
}
fn snapshot_inspect(profile_copy: &Path, secret: &Secret, attested: bool) -> Result<Inspection> {
    require(
        attested,
        "Confirm the encrypted profile is locked and quiesced or an independent offline copy.",
    )?;
    let root = files::safe_root(profile_copy)?;
    // A host-held advisory lock is expected. All durable application content is
    // inside the two encrypted databases; unknown destination payloads must not
    // be silently excluded from a rollback decision.
    let expected = [
        DB_FILE,
        "profile.sqlite3-wal",
        "profile.sqlite3-shm",
        VAULT,
        "migration.db-wal",
        "migration.db-shm",
        "profile.lock",
    ];
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(Error("Non-UTF-8 migration entry."))?;
        require(expected.contains(&name),"Unexpected files are present in the migrated profile; retain them and reconcile before rollback.")?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        require(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Migration contains a nonregular entry.",
        )?;
    }
    let mut before = files::database_inventory(&root, DB_FILE)?;
    before.extend(files::database_inventory(&root, VAULT)?);
    let scratch = files::private_temp("smith-next-verify-", None)?;
    files::copy_encrypted_database(&root, DB_FILE, scratch.path(), DB_FILE)?;
    files::copy_database(&root, VAULT, scratch.path(), VAULT, false)?;
    let result = inspect(scratch.path(), secret)?;
    let mut after = files::database_inventory(&root, DB_FILE)?;
    after.extend(files::database_inventory(&root, VAULT)?);
    require(
        before == after,
        "Profile changed during verification; stop writers and retry.",
    )?;
    Ok(result)
}
/// Read-only verification of the encrypted archive and current database. Later
/// application writes are reported, not discarded or repaired.
pub fn verify_migration(
    profile_copy: &Path,
    secret: &Secret,
    offline_copy_attested: bool,
) -> Result<MigrationReport> {
    Ok(snapshot_inspect(profile_copy, secret, offline_copy_attested)?.report)
}
/// Eligibility only: host may deactivate a drained profile after this succeeds.
/// No files are deleted, decrypted to an output, overwritten or automatically activated.
pub fn check_rollback(
    current_profile_copy: &Path,
    source_copy: &Path,
    secret: &Secret,
    offline_copy_attested: bool,
) -> Result<RollbackReport> {
    let inspection = snapshot_inspect(current_profile_copy, secret, offline_copy_attested)?;
    let source = files::safe_root(source_copy)?;
    let current = files::safe_root(current_profile_copy)?;
    require(
        !current.starts_with(&source) && !source.starts_with(&current),
        "Rollback source and encrypted profile must be disjoint.",
    )?;
    let first = files::inventory(&source)?;
    let source_unchanged = first == inspection.source_files;
    require(
        files::inventory(&source)? == first,
        "Original source copy changed during rollback verification.",
    )?;
    Ok(RollbackReport {
        eligible: source_unchanged && inspection.report.baseline_unchanged,
        later_writes: inspection.report.later_writes,
        source_unchanged,
        destination_unchanged: inspection.report.baseline_unchanged,
        migration_id: inspection.report.migration_id,
    })
}
