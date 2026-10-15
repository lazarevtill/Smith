//! Explicit schema-25 import. Source code, tools, schedulers and URLs are data.
use crate::{
    files, integrity, migration, open_cipher, require, schema, seal, Error, Result, Secret,
    DB_FILE, SCHEMA_VERSION,
};
use rusqlite::{params, types::ValueRef, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smith_profile_migrate as preservation;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use zeroize::Zeroizing;

const VAULT: &str = "import.db";
impl From<preservation::Error> for Error {
    fn from(e: preservation::Error) -> Self {
        Self(e.0)
    }
}

/// Original key material is never persisted or printable. Select its format
/// explicitly: an original identity master key is not the new app passphrase.
pub struct LegacyKeys(preservation::Keys);
impl<'de> Deserialize<'de> for LegacyKeys {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Sensitive(Zeroizing<String>);
        impl<'de> Deserialize<'de> for Sensitive {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                Ok(Self(Zeroizing::new(String::deserialize(d)?)))
            }
        }
        #[derive(Deserialize)]
        #[serde(tag = "format", rename_all = "snake_case", deny_unknown_fields)]
        enum Input {
            Master {
                master_hex: Sensitive,
            },
            Derived {
                database_passphrase: Sensitive,
                artifact_key_hex: Sensitive,
            },
        }
        let result = match Input::deserialize(deserializer)? {
            Input::Master { mut master_hex } => {
                Self::from_master_hex(std::mem::take(&mut *master_hex.0))
            }
            Input::Derived {
                mut database_passphrase,
                mut artifact_key_hex,
            } => Self::from_derived(
                std::mem::take(&mut *database_passphrase.0),
                std::mem::take(&mut *artifact_key_hex.0),
            ),
        };
        result.map_err(serde::de::Error::custom)
    }
}
impl LegacyKeys {
    pub fn from_master_hex(value: String) -> Result<Self> {
        let value = Zeroizing::new(value);
        Ok(Self(preservation::Keys::from_master_hex(&value)?))
    }
    pub fn from_derived(database_passphrase: String, artifact_key_hex: String) -> Result<Self> {
        let blob = Zeroizing::new(artifact_key_hex);
        Ok(Self(preservation::Keys::from_derived(
            Zeroizing::new(database_passphrase),
            &blob,
        )?))
    }
}
pub struct LegacyImportRequest<'a> {
    pub source_copy: &'a Path,
    pub managed_copy: &'a Path,
    pub destination: &'a Path,
    pub offline_copy_attested: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyImportReport {
    pub import_id: String,
    pub profile_id: Option<String>,
    pub source_schema: i64,
    pub target_schema: i64,
    pub source_tables: u64,
    pub source_rows: u64,
    pub source_files: u64,
    pub verified_blobs: u64,
    pub external_stores: u64,
    pub external_artifacts: u64,
    pub mapped_chats: u64,
    pub mapped_messages: u64,
    pub mapped_artifacts: u64,
    pub mapped_tasks: u64,
    pub projection_warnings: u64,
    pub retained_tables: u64,
    pub retained_rows: u64,
    pub retained_schema_objects: u64,
    pub mapped_domains: Vec<String>,
    pub dormant_domains: Vec<String>,
    pub verified: bool,
    pub baseline_unchanged: bool,
    pub later_writes: u64,
    pub rollback_eligible: bool,
}
#[derive(Serialize, Deserialize)]
struct ImportManifest {
    format: u32,
    report: LegacyImportReport,
    original: preservation::Manifest,
    destination: migration::DatabaseRecord,
    baseline_generation: i64,
}
struct Prepared {
    copy: preservation::VerifiedCopy,
    source: PathBuf,
    managed: PathBuf,
    parent: PathBuf,
    destination: PathBuf,
    _scratch: TempDir,
}
fn prepare(request: &LegacyImportRequest<'_>, keys: &LegacyKeys) -> Result<Prepared> {
    require(
        request.offline_copy_attested,
        "Confirm independent, stopped source and managed-artifact copies before importing.",
    )?;
    let source = files::safe_root(request.source_copy)?;
    let managed = files::safe_root(request.managed_copy)?;
    let (parent, destination) = files::destination(request.destination)?;
    require(
        !destination.starts_with(&source)
            && !source.starts_with(&destination)
            && !destination.starts_with(&managed)
            && !managed.starts_with(&destination),
        "Original copies and import destination must be disjoint.",
    )?;
    let scratch = files::private_temp("smith-legacy-preflight-", None)?;
    let preserved = std::fs::canonicalize(scratch.path())?.join("preserved");
    preservation::run(
        &preservation::Request {
            source_copy: &source,
            managed_copy: &managed,
            destination: &preserved,
            offline_copy_attested: true,
        },
        &keys.0,
        preservation::Operation::Migrate,
    )?;
    let copy = preservation::open_verified_copy(&preserved, &keys.0)?;
    copy.require_disjoint_destination(&destination)?;
    let total = copy
        .manifest()
        .source_files
        .values()
        .chain(copy.manifest().artifact_files.values())
        .try_fold(0u64, |a, r| a.checked_add(r.bytes))
        .ok_or(Error("Original archive byte count overflow."))?;
    require(
        total <= files::LIMIT,
        "Combined original copy archive exceeds 256 MiB.",
    )?;
    validate_projection(copy.database())?;
    Ok(Prepared {
        copy,
        source,
        managed,
        parent,
        destination,
        _scratch: scratch,
    })
}
fn count(db: &Connection, table: &str) -> Result<u64> {
    Ok(db.query_row(
        &format!("SELECT count(*) FROM {}", schema::quote(table)),
        [],
        |r| r.get(0),
    )?)
}
fn validate_projection(db: &Connection) -> Result<()> {
    // Fail closed on types that cannot form usable app identities; all other
    // columns/unknown tables remain fully represented in the typed archive.
    for sql in [
        "SELECT count(*) FROM sessions WHERE typeof(id)<>'text' OR length(id)=0 OR length(id)>1024",
        "SELECT count(*) FROM messages WHERE typeof(id)<>'text' OR typeof(session_id)<>'text' OR typeof(seq)<>'integer' OR typeof(role)<>'text' OR typeof(content)<>'text' OR typeof(status)<>'text' OR typeof(tool_calls)<>'text' OR typeof(images)<>'text' OR length(id)=0 OR length(id)>1024",
        "SELECT count(*) FROM tasks WHERE typeof(id)<>'text' OR typeof(status)<>'text' OR typeof(trigger_kind)<>'text' OR typeof(input)<>'text' OR typeof(transcript)<>'text'",
        "SELECT count(*) FROM sessions WHERE unixepoch(created_at,'subsec') IS NULL OR unixepoch(updated_at,'subsec') IS NULL",
        "SELECT count(*) FROM messages WHERE unixepoch(created_at,'subsec') IS NULL",
        "SELECT count(*) FROM artifacts a JOIN artifact_stores s ON s.id=a.store_id WHERE s.managed=1 AND unixepoch(a.created_at,'subsec') IS NULL",
    ] { require(db.query_row(sql,[],|r|r.get::<_,i64>(0))?==0,"Original identity or payload types cannot be projected safely; original copy retained.")?; }
    Ok(())
}
fn report(prepared: &Prepared) -> Result<LegacyImportReport> {
    let m = prepared.copy.manifest();
    let db = prepared.copy.database();
    let projection_warnings=db.query_row("SELECT count(*) FROM messages WHERE role NOT IN ('user','assistant') OR tool_calls<>'[]' OR images<>'[]' OR status NOT IN ('complete','cancelled','failed','incomplete')",[],|r|r.get(0))?;
    Ok(LegacyImportReport {
        import_id: uuid::Uuid::new_v4().to_string(),
        profile_id: None,
        source_schema: 25,
        target_schema: SCHEMA_VERSION,
        source_tables: m.database.tables.len() as u64,
        source_rows: m.database.tables.values().map(|v| v.rows).sum(),
        source_files: (m.source_files.len() + m.artifact_files.len()) as u64,
        verified_blobs: m.verified_blobs,
        external_stores: m.external_stores,
        external_artifacts: m.external_artifacts,
        mapped_chats: count(db, "sessions")?,
        mapped_messages: count(db, "messages")?,
        mapped_artifacts: count(db, "artifacts")?,
        mapped_tasks: count(db, "tasks")?,
        projection_warnings,
        retained_tables: m.database.tables.len() as u64,
        retained_rows: m.database.tables.values().map(|v| v.rows).sum(),
        retained_schema_objects: count(db, "sqlite_schema")?,
        mapped_domains: [
            "conversation_graphs",
            "text_history",
            "authenticated_managed_artifacts",
            "dormant_task_history",
            "original_message_details",
        ]
        .map(String::from)
        .to_vec(),
        dormant_domains: [
            "agents",
            "schedules",
            "watch_triggers",
            "plugins",
            "tools",
            "tool_recipes",
            "mcp_providers",
            "skills",
            "methods",
            "memory_and_learning",
            "external_store_references",
        ]
        .map(String::from)
        .to_vec(),
        verified: true,
        baseline_unchanged: true,
        later_writes: 0,
        rollback_eligible: false,
    })
}
/// Complete preservation preflight and projection validation, without publishing.
pub fn preflight_legacy_copy(
    request: &LegacyImportRequest<'_>,
    keys: &LegacyKeys,
) -> Result<LegacyImportReport> {
    let prepared = prepare(request, keys)?;
    report(&prepared)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum ExactValue {
    Null,
    Integer(String),
    RealBits(String),
    Text(String),
    TextBytes(String),
    Blob(String),
}
#[derive(Serialize, Deserialize)]
struct ExactRow {
    columns: Vec<String>,
    values: Vec<ExactValue>,
}
fn exact_value(value: ValueRef<'_>) -> ExactValue {
    match value {
        ValueRef::Null => ExactValue::Null,
        ValueRef::Integer(v) => ExactValue::Integer(v.to_string()),
        ValueRef::Real(v) => ExactValue::RealBits(files::hex(&v.to_bits().to_le_bytes())),
        ValueRef::Text(v) => match std::str::from_utf8(v) {
            Ok(s) => ExactValue::Text(s.to_owned()),
            Err(_) => ExactValue::TextBytes(files::hex(v)),
        },
        ValueRef::Blob(v) => ExactValue::Blob(files::hex(v)),
    }
}
fn unhex(value: &str) -> Result<Vec<u8>> {
    require(
        value.len() % 2 == 0 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid typed archive bytes.",
    )?;
    (0..value.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&value[i..i + 2], 16)
                .map_err(|_| Error("Invalid typed archive bytes."))
        })
        .collect()
}
fn exact_hash(row: &ExactRow) -> Result<[u8; 32]> {
    require(
        row.columns.len() == row.values.len(),
        "Invalid typed archive column count.",
    )?;
    let mut h = Sha256::new();
    for value in &row.values {
        match value {
            ExactValue::Null => h.update([0]),
            ExactValue::Integer(v) => {
                h.update([1]);
                h.update(
                    v.parse::<i64>()
                        .map_err(|_| Error("Invalid typed integer."))?
                        .to_le_bytes(),
                );
            }
            ExactValue::RealBits(v) => {
                let bits = unhex(v)?;
                require(bits.len() == 8, "Invalid typed real.")?;
                h.update([2]);
                h.update(bits);
            }
            ExactValue::Text(v) => {
                h.update([3]);
                h.update((v.len() as u64).to_le_bytes());
                h.update(v.as_bytes());
            }
            ExactValue::TextBytes(v) | ExactValue::Blob(v) => {
                let bytes = Zeroizing::new(unhex(v)?);
                h.update([if matches!(value, ExactValue::Blob(_)) {
                    4
                } else {
                    3
                }]);
                h.update((bytes.len() as u64).to_le_bytes());
                h.update(&bytes);
            }
        }
    }
    Ok(h.finalize().into())
}
fn archive_rows(
    source: &Connection,
    destination: &Connection,
    manifest: &preservation::Manifest,
) -> Result<()> {
    for (table, expected) in &manifest.database.tables {
        let prefix = if source
            .prepare(&format!(
                "SELECT rowid FROM {} LIMIT 0",
                schema::quote(table)
            ))
            .is_ok()
        {
            "rowid,"
        } else {
            ""
        };
        let mut query =
            source.prepare(&format!("SELECT {prefix}* FROM {}", schema::quote(table)))?;
        let columns = query
            .column_names()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let identity = columns
            .iter()
            .position(|s| s == "id" || s == "name" || s == "artifact_id" || s == "agent_id");
        let mut rows = query.query([])?;
        let mut number = 0i64;
        while let Some(row) = rows.next()? {
            let values = (0..columns.len())
                .map(|i| row.get_ref(i).map(exact_value))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let id = identity.and_then(|i| match &values[i] {
                ExactValue::Text(s) => Some(s.clone()),
                _ => None,
            });
            let payload = Zeroizing::new(serde_json::to_string(&ExactRow {
                columns: columns.clone(),
                values,
            })?);
            destination.execute(
                "INSERT INTO legacy_records VALUES(?1,?2,?3,?4)",
                params![table, number, id, payload.as_str()],
            )?;
            number += 1;
        }
        let mut hashes = Vec::new();
        let mut query = destination.prepare(
            "SELECT payload_json FROM legacy_records WHERE table_name=?1 ORDER BY row_number",
        )?;
        let mut rows = query.query([table])?;
        while let Some(row) = rows.next()? {
            let text = Zeroizing::new(row.get::<_, String>(0)?);
            hashes.push(exact_hash(&serde_json::from_str::<ExactRow>(&text)?)?);
        }
        hashes.sort_unstable();
        let mut h = Sha256::new();
        for hash in &hashes {
            h.update(hash);
        }
        require(
            hashes.len() as u64 == expected.rows && files::hex(&h.finalize()) == expected.sha256,
            "Typed original row reconciliation failed.",
        )?;
    }
    // SQL/schema definitions are readable under the new key too, but remain
    // inert row values. Only our independently defined app schema is executed.
    let mut query = source.prepare(
        "SELECT rowid,type,name,tbl_name,rootpage,sql FROM sqlite_schema ORDER BY rowid",
    )?;
    let columns = query
        .column_names()
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    let mut rows = query.query([])?;
    let mut n = 0i64;
    while let Some(row) = rows.next()? {
        let values = (0..columns.len())
            .map(|i| row.get_ref(i).map(exact_value))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let id: String = row.get(2)?;
        let payload = Zeroizing::new(serde_json::to_string(&ExactRow {
            columns: columns.clone(),
            values,
        })?);
        destination.execute(
            "INSERT INTO legacy_records VALUES('sqlite_schema',?1,?2,?3)",
            params![n, id, payload.as_str()],
        )?;
        let returned=Zeroizing::new(destination.query_row("SELECT payload_json FROM legacy_records WHERE table_name='sqlite_schema' AND row_number=?1",[n],|r|r.get::<_,String>(0))?);
        require(
            *payload == *returned,
            "Original schema record reconciliation failed.",
        )?;
        n += 1;
    }
    Ok(())
}
fn timestamp(db: &Connection, value: Option<&str>) -> Result<i64> {
    let value: Option<i64> = db.query_row(
        "SELECT CAST(round(unixepoch(?1,'subsec')*1000) AS INTEGER)",
        [value],
        |r| r.get(0),
    )?;
    value.ok_or(Error("An original timestamp cannot be represented safely in the application; preservation remains available."))
}
fn project(
    source: &Connection,
    destination: &Connection,
    copy: &preservation::VerifiedCopy,
    keys: &LegacyKeys,
) -> Result<()> {
    let mut sessions =
        source.prepare("SELECT id,title,created_at,updated_at FROM sessions ORDER BY id")?;
    let mut rows = sessions.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let title: Option<String> = row.get(1)?;
        let created: Option<String> = row.get(2)?;
        let updated: Option<String> = row.get(3)?;
        destination.execute("INSERT INTO chats(id,title,created_at,updated_at,version,title_locked,activity) VALUES(?1,?2,?3,?4,0,1,?4)",params![id,title.as_deref().unwrap_or("Imported conversation"),timestamp(source,created.as_deref())?,timestamp(source,updated.as_deref())?])?;
    }
    let mut messages=source.prepare("SELECT id,session_id,seq,role,content,status,created_at,parent_id,reasoning,tool_calls,tool_call_id,images FROM messages ORDER BY session_id,seq")?;
    let mut rows = messages.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let chat: String = row.get(1)?;
        let seq: i64 = row.get(2)?;
        let role: String = row.get(3)?;
        let content: String = row.get(4)?;
        let original: String = row.get(5)?;
        let created: Option<String> = row.get(6)?;
        let parent: Option<String> = row.get(7)?;
        let reasoning: Option<String> = row.get(8)?;
        let calls: String = row.get(9)?;
        let call_id: Option<String> = row.get(10)?;
        let images: String = row.get(11)?;
        let reference =
            !["user", "assistant"].contains(&role.as_str()) || calls != "[]" || images != "[]";
        let status = if reference {
            "imported_reference"
        } else {
            match original.as_str() {
                "complete" => "completed",
                "cancelled" => "cancelled",
                "failed" => "failed",
                "incomplete" => "interrupted",
                _ => "imported_reference",
            }
        };
        let warning = if reference {
            Some("Historical tool/system/image context is retained for inspection and excluded from provider history; nothing is executed or fetched.")
        } else if status == "imported_reference" {
            Some("Unknown original completion status is retained verbatim in imported details and excluded from provider history.")
        } else {
            None
        };
        destination.execute("INSERT INTO messages(id,chat_id,role,content,status,error,created_at,run_id) VALUES(?1,?2,?3,?4,?5,?6,?7,NULL)",params![id,chat,role,content,status,warning,timestamp(source,created.as_deref())?])?;
        destination.execute(
            "INSERT INTO message_branches VALUES(?1,?2)",
            params![id, parent],
        )?;
        destination.execute("INSERT INTO conversation_heads VALUES(?1,?2) ON CONFLICT(chat_id) DO UPDATE SET leaf_id=excluded.leaf_id",params![chat,id])?;
        destination.execute(
            "INSERT INTO legacy_message_details VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                original,
                seq,
                created.as_deref().unwrap_or(""),
                reasoning,
                calls,
                call_id,
                images,
                warning
            ],
        )?;
    }
    let mut artifacts=source.prepare("SELECT a.id,a.session_id,a.message_id,a.store_id,a.title,a.mime,a.relpath,a.created_at,s.managed FROM artifacts a JOIN artifact_stores s ON s.id=a.store_id ORDER BY a.id")?;
    let mut rows = artifacts.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let chat: Option<String> = row.get(1)?;
        let message: Option<String> = row.get(2)?;
        let store: String = row.get(3)?;
        let title: Option<String> = row.get(4)?;
        let mime: Option<String> = row.get(5)?;
        let rel: String = row.get(6)?;
        let created: Option<String> = row.get(7)?;
        let managed: i64 = row.get(8)?;
        let bytes = copy.artifact_bytes(&id, &keys.0)?;
        let digest = bytes.as_ref().map(|b| files::digest(b));
        let kind = if managed == 1 {
            "managed_original"
        } else if bytes.is_some() {
            "derived_reference"
        } else {
            "external_reference"
        };
        destination.execute(
            "INSERT INTO legacy_artifacts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                id,
                chat,
                message,
                store,
                title.as_deref().unwrap_or("Imported artifact"),
                mime.as_deref().unwrap_or("application/octet-stream"),
                rel,
                created.as_deref().unwrap_or(""),
                kind,
                bytes.as_ref().map(|b| b.as_slice()),
                digest
            ],
        )?;
        if managed == 1 {
            let bytes = bytes
                .as_ref()
                .ok_or(Error("Authenticated managed artifact is missing."))?;
            let text = std::str::from_utf8(bytes)
                .ok()
                .filter(|s| s.len() <= 100_000 && !s.contains('\0'));
            let preview=text.unwrap_or("Original managed bytes are retained. Open Imported records to download the authenticated original.");
            let known_chat = chat.as_ref().filter(|id| {
                destination
                    .query_row("SELECT count(*) FROM chats WHERE id=?1", [id], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap_or(0)
                    == 1
            });
            destination.execute(
                "INSERT INTO artifacts(id,chat_id,name,content,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![
                    id,
                    known_chat,
                    title.as_deref().unwrap_or("Imported artifact"),
                    preview,
                    timestamp(source, created.as_deref())?
                ],
            )?;
        }
    }
    let mut tasks=source.prepare("SELECT id,agent_id,status,trigger_kind,input,output,error,transcript,created_at,updated_at FROM tasks ORDER BY id")?;
    let mut rows = tasks.query([])?;
    while let Some(r) = rows.next()? {
        destination.execute(
            "INSERT INTO legacy_tasks VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                r.get::<_, Option<String>>(9)?.unwrap_or_default()
            ],
        )?;
    }
    require(
        count(source, "sessions")? == count(destination, "chats")?
            && count(source, "messages")? == count(destination, "messages")?
            && count(source, "tasks")? == count(destination, "legacy_tasks")?
            && count(source, "artifacts")? == count(destination, "legacy_artifacts")?,
        "Projected row counts do not reconcile.",
    )?;
    Ok(())
}
fn original_inventory(
    source: &Path,
    managed: &Path,
) -> Result<(
    BTreeMap<String, files::FileRecord>,
    BTreeMap<String, files::FileRecord>,
)> {
    let mut profile = files::inventory(source)?;
    if managed == source.join("artifacts") {
        profile.retain(|p, _| !p.starts_with("artifacts/"));
    }
    Ok((profile, files::inventory(managed)?))
}
fn inventory_matches(
    source: &Path,
    managed: &Path,
    manifest: &preservation::Manifest,
) -> Result<bool> {
    let (profile, artifacts) = original_inventory(source, managed)?;
    fn same(
        a: &BTreeMap<String, files::FileRecord>,
        b: &BTreeMap<String, preservation::FileRecord>,
    ) -> bool {
        a.len() == b.len()
            && a.iter().all(|(p, r)| {
                b.get(p)
                    .is_some_and(|b| r.bytes == b.bytes && r.sha256 == b.sha256 && r.mode == b.mode)
            })
    }
    Ok(same(&profile, &manifest.source_files) && same(&artifacts, &manifest.artifact_files))
}
/// Repeats full preflight, constructs an independently verified new encrypted
/// application profile, and publishes exclusively. No original keys are saved.
pub fn import_legacy_copy(
    request: &LegacyImportRequest<'_>,
    keys: &LegacyKeys,
    new_secret: &Secret,
) -> Result<LegacyImportReport> {
    let prepared = prepare(request, keys)?;
    let mut report = report(&prepared)?;
    let stage = files::private_temp("smith-legacy-import-stage-", Some(&prepared.parent))?;
    let db = open_cipher(&stage.path().join(DB_FILE), new_secret, true)?;
    schema::initialize_schema(&db)?;
    schema::install_storage(&db)?;
    report.profile_id = Some(schema::identity(&db)?.0);
    report.rollback_eligible = true;
    db.execute_batch("BEGIN IMMEDIATE;")?;
    archive_rows(prepared.copy.database(), &db, prepared.copy.manifest())?;
    project(prepared.copy.database(), &db, &prepared.copy, keys)?;
    let json = Zeroizing::new(serde_json::to_string(&report)?);
    db.execute(
        "INSERT INTO legacy_imports VALUES(?1,?2,0)",
        params![report.import_id, json.as_str()],
    )?;
    db.execute_batch("COMMIT;")?;
    schema::validate(&db)?;
    integrity(&db, true)?;
    let manifest = ImportManifest {
        format: 1,
        report: report.clone(),
        original: prepared.copy.manifest().clone(),
        destination: migration::database_record(&db)?,
        baseline_generation: schema::identity(&db)?.1,
    };
    let vault = open_cipher(&stage.path().join(VAULT), new_secret, true)?;
    vault.execute_batch("BEGIN IMMEDIATE;CREATE TABLE manifest(id INTEGER PRIMARY KEY CHECK(id=1),payload TEXT NOT NULL);CREATE TABLE original_files(namespace TEXT NOT NULL,path TEXT NOT NULL,mode INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(namespace,path));")?;
    let payload = Zeroizing::new(serde_json::to_string(&manifest)?);
    vault.execute("INSERT INTO manifest VALUES(1,?1)", [payload.as_str()])?;
    for (namespace, root, inventory) in [
        ("profile", &prepared.source, &manifest.original.source_files),
        (
            "artifacts",
            &prepared.managed,
            &manifest.original.artifact_files,
        ),
    ] {
        for (name, expected) in inventory {
            let bytes = files::read_file(&root.join(files::safe_relative(name)?))?;
            require(
                bytes.len() as u64 == expected.bytes && files::digest(&bytes) == expected.sha256,
                "Original copy changed during archival.",
            )?;
            vault.execute(
                "INSERT INTO original_files VALUES(?1,?2,?3,?4)",
                params![namespace, name, expected.mode, bytes.as_slice()],
            )?;
        }
    }
    vault.execute_batch("COMMIT;")?;
    seal(&db)?;
    seal(&vault)?;
    drop(db);
    drop(vault);
    require(
        files::record(&stage.path().join(VAULT))?.bytes <= files::LIMIT,
        "Encrypted import archive exceeds verification limit.",
    )?;
    require(
        inspect(stage.path(), new_secret)?.report.baseline_unchanged,
        "Staged original import failed reconciliation.",
    )?;
    require(
        inventory_matches(&prepared.source, &prepared.managed, &manifest.original)?,
        "Original copies changed during import; no destination was published.",
    )?;
    files::sync_tree(stage.path())?;
    files::rename_exclusive(stage.path(), &prepared.destination)?;
    files::sync_parent(&prepared.destination)?;
    Ok(report)
}
struct Inspection {
    report: LegacyImportReport,
    original: preservation::Manifest,
}
fn inspect(root: &Path, secret: &Secret) -> Result<Inspection> {
    let db = open_cipher(&root.join(DB_FILE), secret, false)?;
    schema::validate(&db)?;
    integrity(&db, true)?;
    let vault = open_cipher(&root.join(VAULT), secret, false)?;
    integrity(&vault, true)?;
    require(
        count(&vault, "manifest")? == 1,
        "Original import manifest is missing or ambiguous.",
    )?;
    let payload = Zeroizing::new(vault.query_row(
        "SELECT payload FROM manifest WHERE id=1",
        [],
        |r| r.get::<_, String>(0),
    )?);
    let manifest: ImportManifest = serde_json::from_str(&payload)?;
    require(
        manifest.format == 1
            && manifest.report.source_schema == 25
            && [7, 8, 9, 10, 11].contains(&manifest.report.target_schema)
            && manifest.original.database.schema_version == 25
            && manifest.original.cipher_version.starts_with("4."),
        "Unsupported original import manifest.",
    )?;
    let (profile, generation) = schema::identity(&db)?;
    require(
        manifest.report.profile_id.as_deref() == Some(&profile)
            && generation >= manifest.baseline_generation,
        "Original import identity or write generation changed unexpectedly.",
    )?;
    let mut n = 0u64;
    let mut stmt = vault
        .prepare("SELECT namespace,path,mode,bytes FROM original_files ORDER BY namespace,path")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let namespace: String = r.get(0)?;
        let name: String = r.get(1)?;
        files::safe_relative(&name)?;
        let mode: u32 = r.get(2)?;
        let bytes = Zeroizing::new(r.get::<_, Vec<u8>>(3)?);
        let inventory = match namespace.as_str() {
            "profile" => &manifest.original.source_files,
            "artifacts" => &manifest.original.artifact_files,
            _ => return Err(Error("Unexpected original archive namespace.")),
        };
        let expected = inventory
            .get(&name)
            .ok_or(Error("Unexpected original archive file."))?;
        require(
            bytes.len() as u64 == expected.bytes
                && files::digest(&bytes) == expected.sha256
                && mode == expected.mode,
            "Original archived bytes do not reconcile.",
        )?;
        n += 1;
    }
    require(
        n == manifest.report.source_files
            && n == (manifest.original.source_files.len() + manifest.original.artifact_files.len())
                as u64
            && manifest.original.source_files.contains_key("smith.db"),
        "Original archive is incomplete.",
    )?;
    let mut report = manifest.report;
    report.later_writes = (generation - manifest.baseline_generation) as u64;
    report.baseline_unchanged =
        report.later_writes == 0 && migration::database_record(&db)? == manifest.destination;
    report.rollback_eligible = report.baseline_unchanged;
    Ok(Inspection {
        report,
        original: manifest.original,
    })
}
fn snapshot(root: &Path, secret: &Secret, attested: bool) -> Result<Inspection> {
    require(
        attested,
        "Confirm that the imported profile is locked and quiesced before verification.",
    )?;
    let root = files::safe_root(root)?;
    for e in std::fs::read_dir(&root)? {
        let e = e?;
        let name = e.file_name();
        let name = name
            .to_str()
            .ok_or(Error("Invalid imported profile entry."))?;
        require(
            [
                DB_FILE,
                "profile.sqlite3-wal",
                "profile.sqlite3-shm",
                VAULT,
                "import.db-wal",
                "import.db-shm",
                "profile.lock",
            ]
            .contains(&name),
            "Unexpected imported profile files require reconciliation before deactivation.",
        )?;
        require(
            std::fs::symlink_metadata(e.path())?.is_file(),
            "Imported profile contains a nonregular entry.",
        )?;
    }
    let before = (
        files::database_inventory(&root, DB_FILE)?,
        files::database_inventory(&root, VAULT)?,
    );
    let scratch = files::private_temp("smith-import-verify-", None)?;
    files::copy_encrypted_database(&root, DB_FILE, scratch.path(), DB_FILE)?;
    files::copy_database(&root, VAULT, scratch.path(), VAULT, false)?;
    let result = inspect(scratch.path(), secret)?;
    require(
        before
            == (
                files::database_inventory(&root, DB_FILE)?,
                files::database_inventory(&root, VAULT)?,
            ),
        "Imported profile changed during verification.",
    )?;
    Ok(result)
}
pub fn verify_legacy_import(
    profile_copy: &Path,
    secret: &Secret,
    offline_copy_attested: bool,
) -> Result<LegacyImportReport> {
    Ok(snapshot(profile_copy, secret, offline_copy_attested)?.report)
}
pub fn check_legacy_rollback(
    profile_copy: &Path,
    source_copy: &Path,
    managed_copy: &Path,
    secret: &Secret,
    offline_copy_attested: bool,
) -> Result<crate::RollbackReport> {
    let checked = snapshot(profile_copy, secret, offline_copy_attested)?;
    let source = files::safe_root(source_copy)?;
    let managed = files::safe_root(managed_copy)?;
    let destination = files::safe_root(profile_copy)?;
    require(
        !destination.starts_with(&source)
            && !source.starts_with(&destination)
            && !destination.starts_with(&managed)
            && !managed.starts_with(&destination),
        "Original copies and imported profile must be disjoint.",
    )?;
    let unchanged = inventory_matches(&source, &managed, &checked.original)?;
    require(
        inventory_matches(&source, &managed, &checked.original)? == unchanged,
        "Original copies changed during rollback verification.",
    )?;
    Ok(crate::RollbackReport {
        eligible: unchanged && checked.report.baseline_unchanged,
        later_writes: checked.report.later_writes,
        source_unchanged: unchanged,
        destination_unchanged: checked.report.baseline_unchanged,
        migration_id: checked.report.import_id,
    })
}

#[cfg(test)]
#[path = "../tests/support/schema_history_review.rs"]
mod schema_history_review;
