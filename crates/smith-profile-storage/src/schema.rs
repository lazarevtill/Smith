use crate::{require, Error, Result, APPLICATION_ID, SCHEMA_VERSION};
use rusqlite::{params, Connection};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
const BASE: &str = "
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS chats(id TEXT PRIMARY KEY,title TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL,error TEXT,created_at INTEGER NOT NULL,run_id TEXT);
CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY,chat_id TEXT NOT NULL REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS run_configs(run_id TEXT PRIMARY KEY,provider TEXT NOT NULL,endpoint TEXT NOT NULL,model TEXT NOT NULL);";
const SIX: &str = "
CREATE TABLE drafts(owner TEXT PRIMARY KEY,version INTEGER NOT NULL CHECK(version>=1),text TEXT NOT NULL,attachments TEXT NOT NULL DEFAULT '[]',media TEXT NOT NULL DEFAULT '[]',updated_at INTEGER NOT NULL);
CREATE TABLE media_objects(id TEXT PRIMARY KEY,request_id TEXT NOT NULL UNIQUE,origin TEXT NOT NULL CHECK(origin IN ('upload','generated')),name TEXT NOT NULL,media_type TEXT NOT NULL,format TEXT NOT NULL,sha256 TEXT NOT NULL CHECK(length(sha256)=64),byte_len INTEGER NOT NULL CHECK(byte_len>=0 AND byte_len<=8388608),manifest_json TEXT NOT NULL,bytes BLOB NOT NULL CHECK(length(bytes)=byte_len),created_at INTEGER NOT NULL);
CREATE TABLE message_media(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,media_id TEXT NOT NULL REFERENCES media_objects(id),position INTEGER NOT NULL CHECK(position>=0 AND position<3),delivery TEXT NOT NULL CHECK(delivery IN ('context','reference_only')),UNIQUE(message_id,position),UNIQUE(message_id,media_id));
CREATE INDEX message_media_object ON message_media(media_id);
CREATE TABLE artifact_media(artifact_id TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,media_id TEXT NOT NULL REFERENCES media_objects(id));
CREATE INDEX artifact_media_object ON artifact_media(media_id);
CREATE TABLE agent_profile(id INTEGER PRIMARY KEY CHECK(id=1),principal_id TEXT NOT NULL UNIQUE);
INSERT INTO agent_profile(id,principal_id) VALUES(1,lower(hex(randomblob(16))));
CREATE TABLE agent_workspaces(id TEXT PRIMARY KEY,principal_id TEXT NOT NULL REFERENCES agent_profile(principal_id),label TEXT NOT NULL,identity TEXT NOT NULL,capabilities TEXT NOT NULL,policy_revision INTEGER NOT NULL CHECK(policy_revision>=1),status TEXT NOT NULL CHECK(status IN ('active','unavailable')),created_at INTEGER NOT NULL,UNIQUE(id,principal_id),UNIQUE(principal_id,identity));
CREATE TABLE agent_tasks(id TEXT PRIMARY KEY,workspace_id TEXT NOT NULL,principal_id TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0),snapshot TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,FOREIGN KEY(workspace_id,principal_id) REFERENCES agent_workspaces(id,principal_id));
CREATE TABLE agent_task_trace(task_id TEXT NOT NULL REFERENCES agent_tasks(id),revision INTEGER NOT NULL CHECK(revision>=0),event TEXT NOT NULL,PRIMARY KEY(task_id,revision));
CREATE INDEX agent_tasks_owner_updated ON agent_tasks(principal_id,updated_at,id);
CREATE TABLE chat_trash(chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,trashed_at INTEGER NOT NULL);
CREATE TABLE action_history_state(id INTEGER PRIMARY KEY CHECK(id=1),revision INTEGER NOT NULL CHECK(revision>=0));
INSERT INTO action_history_state(id,revision) VALUES(1,0);
CREATE TABLE action_history(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,kind TEXT NOT NULL CHECK(kind IN ('rename','branch','trash','restore')),before_json TEXT NOT NULL,after_json TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('applied','undone','invalidated')),created_at INTEGER NOT NULL);
CREATE INDEX action_history_state_sequence ON action_history(state,sequence);
CREATE TABLE action_history_versions(chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,version INTEGER NOT NULL CHECK(version>=0));";

const REQUIRED: &[(&str, &[&str])] = &[
    ("settings", &["key", "value"]),
    (
        "chats",
        &[
            "id",
            "title",
            "created_at",
            "updated_at",
            "version",
            "title_locked",
            "activity",
        ],
    ),
    (
        "messages",
        &[
            "id",
            "chat_id",
            "role",
            "content",
            "status",
            "error",
            "created_at",
            "run_id",
        ],
    ),
    (
        "artifacts",
        &["id", "chat_id", "name", "content", "created_at"],
    ),
    (
        "run_configs",
        &[
            "run_id",
            "provider",
            "endpoint",
            "model",
            "warning",
            "finish_reason",
            "transport_incomplete",
        ],
    ),
    ("message_branches", &["message_id", "parent_id"]),
    ("conversation_heads", &["chat_id", "leaf_id"]),
    (
        "text_attachments",
        &["id", "message_id", "name", "media_type", "content"],
    ),
    (
        "drafts",
        &[
            "owner",
            "version",
            "text",
            "attachments",
            "media",
            "updated_at",
        ],
    ),
    (
        "media_objects",
        &[
            "id",
            "request_id",
            "origin",
            "name",
            "media_type",
            "format",
            "sha256",
            "byte_len",
            "manifest_json",
            "bytes",
            "created_at",
        ],
    ),
    (
        "message_media",
        &["id", "message_id", "media_id", "position", "delivery"],
    ),
    ("artifact_media", &["artifact_id", "media_id"]),
    ("agent_profile", &["id", "principal_id"]),
    (
        "agent_workspaces",
        &[
            "id",
            "principal_id",
            "label",
            "identity",
            "capabilities",
            "policy_revision",
            "status",
            "created_at",
        ],
    ),
    (
        "agent_tasks",
        &[
            "id",
            "workspace_id",
            "principal_id",
            "revision",
            "snapshot",
            "created_at",
            "updated_at",
        ],
    ),
    ("agent_task_trace", &["task_id", "revision", "event"]),
    ("chat_trash", &["chat_id", "trashed_at"]),
    ("action_history_state", &["id", "revision"]),
    (
        "action_history",
        &[
            "sequence",
            "id",
            "kind",
            "before_json",
            "after_json",
            "state",
            "created_at",
        ],
    ),
    ("action_history_versions", &["chat_id", "version"]),
];

pub(crate) fn columns(db: &Connection, table: &str) -> Result<BTreeSet<String>> {
    Ok(db
        .prepare(&format!("PRAGMA table_info({})", quote(table)))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?)
}
fn require_columns(db: &Connection, table: &str, names: &[&str]) -> Result<()> {
    let kind: String = db.query_row(
        "SELECT type FROM sqlite_schema WHERE name=?1",
        [table],
        |r| r.get(0),
    )?;
    require(
        kind == "table",
        "A required application table was replaced by another schema object.",
    )?;
    let actual = columns(db, table)?;
    require(
        names.iter().all(|name| actual.contains(*name)),
        "Profile schema is missing required tables or columns.",
    )
}

/// Shared schema builder for explicitly selected synthetic fixtures. This helper
/// does not establish encryption; applications must use create/unlock instead.
pub fn initialize_schema(db: &Connection) -> Result<()> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    require(
        (0..=SCHEMA_VERSION).contains(&version),
        "Unsupported application schema; no reset was attempted.",
    )?;
    if version == SCHEMA_VERSION {
        return validate_application(db);
    }
    if version == 0 {
        let count: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table'",
            [],
            |r| r.get(0),
        )?;
        require(
            count == 0,
            "Unversioned nonempty database is not a recognized application profile.",
        )?;
    }
    db.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| {
        db.execute_batch(BASE)?;
        if version < 3 {
            db.execute_batch("ALTER TABLE run_configs ADD COLUMN warning TEXT; ALTER TABLE run_configs ADD COLUMN finish_reason TEXT; ALTER TABLE run_configs ADD COLUMN transport_incomplete INTEGER NOT NULL DEFAULT 0;")?;
        }
        if version < 4 {
            require(
                columns(db, "artifacts")?
                    == ["id", "chat_id", "name", "content", "created_at"]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                "Historical artifact schema has unsupported columns; source retained.",
            )?;
            db.execute_batch("ALTER TABLE chats ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN version INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN title_locked INTEGER NOT NULL DEFAULT 0; ALTER TABLE chats ADD COLUMN activity INTEGER NOT NULL DEFAULT 0; UPDATE chats SET updated_at=created_at,activity=rowid; CREATE TABLE artifacts_v4(id TEXT PRIMARY KEY,chat_id TEXT REFERENCES chats(id),name TEXT NOT NULL,content TEXT NOT NULL,created_at INTEGER NOT NULL); INSERT INTO artifacts_v4(rowid,id,chat_id,name,content,created_at) SELECT rowid,id,chat_id,name,content,created_at FROM artifacts; DROP TABLE artifacts; ALTER TABLE artifacts_v4 RENAME TO artifacts;")?;
        }
        if version < 5 {
            db.execute_batch("CREATE TABLE message_branches(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,parent_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED); CREATE INDEX message_branch_parent ON message_branches(parent_id); CREATE TABLE conversation_heads(chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,leaf_id TEXT REFERENCES messages(id) DEFERRABLE INITIALLY DEFERRED); CREATE TABLE text_attachments(id TEXT PRIMARY KEY,message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,name TEXT NOT NULL,media_type TEXT NOT NULL,content TEXT NOT NULL); CREATE INDEX attachment_message ON text_attachments(message_id);")?;
            let chats = db
                .prepare("SELECT id FROM chats ORDER BY rowid")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for chat in chats {
                backfill(db, &chat)?;
            }
        }
        if version < 6 {
            db.execute_batch(SIX)?;
        }
        if version < 7 {
            db.execute_batch(crate::schema_v7::SQL)?;
        }
        if version < 8 {
            db.execute_batch(crate::schema_v8::SQL)?;
        }
        if version < 9 {
            db.execute_batch(crate::schema_v9::SQL)?;
        }
        if version < 10 {
            db.execute_batch(crate::schema_v10::SQL)?;
        }
        if version < 11 {
            db.execute_batch(crate::schema_v11::SQL)?;
        }
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        validate_application(db)?;
        db.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = db.execute_batch("ROLLBACK;");
    }
    result
}

fn backfill(db: &Connection, chat: &str) -> Result<()> {
    let rows = db
        .prepare("SELECT id,role FROM messages WHERE chat_id=?1 ORDER BY rowid")?
        .query_map([chat], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut previous: Option<String> = None;
    let mut user: Option<String> = None;
    for (id, role) in rows {
        let parent = if role == "assistant" {
            user.as_ref().or(previous.as_ref())
        } else {
            previous.as_ref()
        };
        db.execute(
            "INSERT INTO message_branches(message_id,parent_id) VALUES(?1,?2)",
            params![id, parent],
        )?;
        if role == "user" {
            user = Some(id.clone());
        }
        previous = Some(id);
    }
    db.execute(
        "INSERT INTO conversation_heads(chat_id,leaf_id) VALUES(?1,?2)",
        params![chat, previous],
    )?;
    Ok(())
}

pub(crate) fn plain_source(db: &Connection) -> Result<i64> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    require((1..=5).contains(&version), "Only explicit plaintext Next schemas 1 through 5 can be migrated here; original Smith schema 25 uses its separate preservation tool.")?;
    require_columns(db, "chats", &["id", "title", "created_at"])?;
    let reserved:i64=db.query_row("SELECT count(*) FROM sqlite_schema WHERE name GLOB '_smith_*' OR type='trigger' OR (type='table' AND rootpage=0)",[],|r|r.get(0))?;
    require(reserved==0,"Source contains unsupported reserved, virtual or executable schema objects; source retained.")?;
    for (table, names) in &REQUIRED[..8] {
        let existing = columns(db, table)?;
        if !existing.is_empty() {
            let relevant: Vec<&str> = names
                .iter()
                .copied()
                .filter(|name| {
                    !(version < 4
                        && *table == "chats"
                        && ["updated_at", "version", "title_locked", "activity"].contains(name)
                        || version < 3
                            && *table == "run_configs"
                            && ["warning", "finish_reason", "transport_incomplete"].contains(name))
                })
                .collect();
            require_columns(db, table, &relevant)?;
        } else {
            require(
                version < 4
                    || ["message_branches", "conversation_heads", "text_attachments"]
                        .contains(table)
                        && version < 5,
                "Versioned source schema is incomplete.",
            )?;
        }
    }
    Ok(version)
}

fn validate_application(db: &Connection) -> Result<()> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let required = REQUIRED
        .iter()
        .chain(
            if version >= 7 {
                crate::schema_v7::REQUIRED
            } else {
                &[]
            }
            .iter(),
        )
        .chain(
            if version >= 8 {
                crate::schema_v8::REQUIRED
            } else {
                &[]
            }
            .iter(),
        )
        .chain(
            if version >= 9 {
                crate::schema_v9::REQUIRED
            } else {
                &[]
            }
            .iter(),
        )
        .chain(
            if version >= 10 {
                crate::schema_v10::REQUIRED
            } else {
                &[]
            }
            .iter(),
        )
        .chain(
            if version >= 11 {
                crate::schema_v11::REQUIRED
            } else {
                &[]
            }
            .iter(),
        );
    for (table, names) in required {
        require_columns(db, table, names)?;
        for name in *names {
            let integer = [
                "created_at",
                "updated_at",
                "version",
                "title_locked",
                "activity",
                "transport_incomplete",
                "byte_len",
                "position",
                "policy_revision",
                "revision",
                "sequence",
                "trashed_at",
                "row_number",
                "original_seq",
                "deleted",
                "definition_revision",
                "pinned",
                "enabled",
                "format_version",
            ]
            .contains(name)
                || *name == "id" && ["agent_profile", "action_history_state"].contains(table);
            let kind = if integer && *table != "agent_host_operations" {
                "integer"
            } else if ["media_objects", "legacy_artifacts"].contains(table) && *name == "bytes" {
                "blob"
            } else {
                "text"
            };
            let nullable = [
                ("agent_host_operations", "result_json"),
                ("messages", "error"),
                ("messages", "run_id"),
                ("artifacts", "chat_id"),
                ("run_configs", "warning"),
                ("run_configs", "finish_reason"),
                ("message_branches", "parent_id"),
                ("conversation_heads", "leaf_id"),
                ("legacy_records", "record_id"),
                ("legacy_message_details", "reasoning"),
                ("legacy_message_details", "tool_call_id"),
                ("legacy_message_details", "projection_warning"),
                ("legacy_artifacts", "session_id"),
                ("legacy_artifacts", "message_id"),
                ("legacy_artifacts", "bytes"),
                ("legacy_artifacts", "sha256"),
                ("legacy_tasks", "agent_id"),
                ("legacy_tasks", "output"),
                ("legacy_tasks", "error"),
            ]
            .contains(&(*table, *name));
            let column = quote(name);
            let predicate = if nullable {
                format!("{column} IS NOT NULL AND typeof({column})<>'{kind}'")
            } else {
                format!("typeof({column})<>'{kind}'")
            };
            let sql = format!("SELECT count(*) FROM {} WHERE {predicate}", quote(table));
            require(
                db.query_row(&sql, [], |r| r.get::<_, i64>(0))? == 0,
                "Profile contains an invalid application value type; data was retained.",
            )?;
        }
    }
    for sql in [
        "SELECT count(*) FROM message_branches b JOIN messages m ON m.id=b.message_id JOIN messages p ON p.id=b.parent_id WHERE m.chat_id<>p.chat_id OR m.rowid<=p.rowid",
        "SELECT count(*) FROM conversation_heads h JOIN messages m ON m.id=h.leaf_id WHERE h.chat_id<>m.chat_id",
    ] {
        require(db.query_row(sql,[],|r|r.get::<_,i64>(0))?==0,"Saved conversation graph is inconsistent.")?;
    }
    let principal_count: i64 = db.query_row(
        "SELECT count(*) FROM agent_profile WHERE id=1 AND length(principal_id)=32",
        [],
        |r| r.get(0),
    )?;
    require(principal_count == 1, "Profile agent identity is missing.")?;
    Ok(())
}

fn user_tables(db: &Connection) -> Result<Vec<String>> {
    Ok(db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_smith_*' ORDER BY name")?
        .query_map([],|r|r.get(0))?.collect::<std::result::Result<Vec<_>,_>>()?)
}
fn audit_sql(table: &str, operation: &str) -> (String, String) {
    let name = format!(
        "_smith_write_{}_{}",
        crate::files::hex(table.as_bytes()),
        operation.to_ascii_lowercase()
    );
    let sql=format!("CREATE TRIGGER {} AFTER {} ON {} BEGIN UPDATE _smith_storage SET write_generation=write_generation+1 WHERE id=1; END",quote(&name),operation,quote(table));
    (name, sql)
}
fn schema_signature(db: &Connection) -> Result<String> {
    let rows = db
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&rows)?);
    Ok(crate::files::digest(&bytes))
}
pub(crate) fn install_storage(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE; CREATE TABLE _smith_storage(id INTEGER PRIMARY KEY CHECK(id=1),format INTEGER NOT NULL CHECK(format=1),profile_id TEXT NOT NULL,write_generation INTEGER NOT NULL CHECK(write_generation>=0),schema_sha256 TEXT NOT NULL);")?;
    let result = (|| {
        db.execute(
            "INSERT INTO _smith_storage VALUES(1,1,?1,0,'')",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        for table in user_tables(db)? {
            for op in ["INSERT", "UPDATE", "DELETE"] {
                db.execute_batch(&audit_sql(&table, op).1)?;
            }
        }
        db.execute(
            "UPDATE _smith_storage SET schema_sha256=?1 WHERE id=1",
            [schema_signature(db)?],
        )?;
        db.pragma_update(None, "application_id", APPLICATION_ID)?;
        db.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = db.execute_batch("ROLLBACK;");
    }
    result
}
pub(crate) fn identity(db: &Connection) -> Result<(String, i64)> {
    let (id, generation): (String, i64) = db.query_row(
        "SELECT profile_id,write_generation FROM _smith_storage WHERE id=1 AND format=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    require(
        uuid::Uuid::parse_str(&id).is_ok() && generation >= 0,
        "Encrypted profile identity is invalid.",
    )?;
    Ok((id, generation))
}
pub(crate) fn validate(db: &Connection) -> Result<()> {
    require(
        db.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? == APPLICATION_ID,
        "This is not a supported encrypted Next profile.",
    )?;
    require(
        [6, 7, 8, 9, 10, SCHEMA_VERSION].contains(&db.query_row(
            "PRAGMA user_version",
            [],
            |r| r.get::<_, i64>(0),
        )?),
        "Unsupported encrypted application schema; no migration or reset was attempted.",
    )?;
    validate_application(db)?;
    identity(db)?;
    require(
        db.query_row("SELECT count(*) FROM _smith_storage", [], |r| {
            r.get::<_, i64>(0)
        })? == 1,
        "Encrypted profile storage identity is ambiguous.",
    )?;
    let expected_signature: String = db.query_row(
        "SELECT schema_sha256 FROM _smith_storage WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    require(
        schema_signature(db)? == expected_signature,
        "Encrypted profile schema differs from its creation contract.",
    )?;
    let actual: BTreeMap<String, String> = db
        .prepare(
            "SELECT name,sql FROM sqlite_schema WHERE type='trigger' AND name GLOB '_smith_*'",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut expected = BTreeMap::new();
    for table in user_tables(db)? {
        for op in ["INSERT", "UPDATE", "DELETE"] {
            let (name, sql) = audit_sql(&table, op);
            expected.insert(name, sql);
        }
    }
    require(
        actual == expected,
        "Profile write-history protection is missing or changed.",
    )?;
    Ok(())
}

/// Only called after full authentication/schema/audit validation. DDL, new
/// audit triggers, signature and generation advance commit together.
pub(crate) fn upgrade_encrypted(db: &Connection) -> Result<()> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    require(
        matches!(version, 6..=10),
        "Unsupported encrypted schema upgrade.",
    )?;
    validate(db)?;
    db.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| {
        if version < 7 {
            db.execute_batch(crate::schema_v7::SQL)?;
        }
        if version < 8 {
            db.execute_batch(crate::schema_v8::SQL)?;
        }
        if version < 9 {
            db.execute_batch(crate::schema_v9::SQL)?;
        }
        if version < 10 {
            db.execute_batch(crate::schema_v10::SQL)?;
        }
        if version < 11 {
            db.execute_batch(crate::schema_v11::SQL)?;
        }
        for table in user_tables(db)? {
            for op in ["INSERT", "UPDATE", "DELETE"] {
                let (name, sql) = audit_sql(&table, op);
                let exists: i64 = db.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name=?1",
                    [&name],
                    |r| r.get(0),
                )?;
                if exists == 0 {
                    db.execute_batch(&sql)?;
                }
            }
        }
        db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        db.execute("UPDATE _smith_storage SET write_generation=write_generation+1,schema_sha256=?1 WHERE id=1", [schema_signature(db)?])?;
        validate(db)?;
        db.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = db.execute_batch("ROLLBACK;");
    }
    result
}

/// Call only after successful unlock and app config validation. Recovery is a
/// real logical write and intentionally makes migrated rollback ineligible.
pub fn recover_interrupted(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE;")?;
    let result=db.execute_batch("UPDATE run_configs SET transport_incomplete=1,warning=CASE finish_reason WHEN 'length' THEN 'The model reached its limit before Smith restarted. The incomplete answer is saved; do not retry automatically.' ELSE 'Smith restarted after the model confirmed completion. Saved text is preserved; do not retry automatically.' END WHERE finish_reason IN ('stop','length') AND run_id IN (SELECT run_id FROM messages WHERE status='streaming'); UPDATE messages SET status=CASE (SELECT finish_reason FROM run_configs WHERE run_id=messages.run_id) WHEN 'stop' THEN 'completed' WHEN 'length' THEN 'limited' ELSE 'interrupted' END,error=CASE WHEN (SELECT finish_reason FROM run_configs WHERE run_id=messages.run_id) IN ('stop','length') THEN NULL ELSE 'Smith restarted before this answer finished. Partial text is saved; retry when ready.' END WHERE status='streaming'; COMMIT;");
    if result.is_err() {
        let _ = db.execute_batch("ROLLBACK;");
    }
    result.map_err(Error::from)
}

#[cfg(test)]
mod upgrade_tests {
    use super::*;

    #[test]
    fn authenticated_schema9_upgrade_preserves_memory_and_audits_extensions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join(crate::DB_FILE);
        let key = crate::Secret::new("Synthetic schema9 extension upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &key, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        db.execute_batch("PRAGMA user_version=9; INSERT INTO agent_memories(id,principal_id,namespace,content,tags_json,created_at,updated_at,version) SELECT 'memory',principal_id,'facts','Retained curated fact','[]',1,2,3 FROM agent_profile; INSERT INTO memory_metadata VALUES('memory',1,1,2);").unwrap();
        install_storage(&db).unwrap();
        let before = identity(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 1));
        let memory = db.query_row("SELECT content,version,pinned,deleted FROM agent_memories JOIN memory_metadata ON memory_id=id WHERE id='memory'", [], |r| Ok((r.get::<_, String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?))).unwrap();
        assert_eq!(memory, ("Retained curated fact".into(), 3, 1, 1));
        db.execute("INSERT INTO agent_extensions(id,principal_id,kind,revision,body_json,provenance_json,created_at,updated_at) SELECT 'extension',principal_id,'skill',1,'{}','{}',1,1 FROM agent_profile", []).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 2));
        let sha = "a".repeat(64);
        db.execute("INSERT INTO agent_extension_imports(request_id,principal_id,source_sha256,request_sha256,extension_id,result_json,created_at) SELECT 'receipt',principal_id,?1,?1,'extension','{}',1 FROM agent_profile", [&sha]).unwrap();
        db.execute("DELETE FROM agent_extensions WHERE id='extension'", [])
            .unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 4));
        assert_eq!(
            db.query_row("SELECT count(*) FROM agent_extension_imports", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        validate(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0, before.1 + 4));
        crate::close(db).unwrap();
    }

    #[test]
    fn failed_schema10_ddl_leaves_authenticated_schema9_unchanged() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join(crate::DB_FILE);
        let key = crate::Secret::new("Synthetic failed schema10 upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &key, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        db.execute_batch("PRAGMA user_version=9; CREATE TABLE agent_extension_imports(sentinel TEXT); INSERT INTO agent_extension_imports VALUES('preserve conflict');").unwrap();
        install_storage(&db).unwrap();
        let before = identity(&db).unwrap();
        let signature = schema_signature(&db).unwrap();
        assert!(upgrade_encrypted(&db).is_err());
        assert_eq!(identity(&db).unwrap(), before);
        assert_eq!(schema_signature(&db).unwrap(), signature);
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            9
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='agent_extensions'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT sentinel FROM agent_extension_imports", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "preserve conflict"
        );
        validate(&db).unwrap();
        crate::close(db).unwrap();
    }

    #[test]
    fn authenticated_schema8_upgrade_preserves_memory_and_audits_curation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join(crate::DB_FILE);
        let key = crate::Secret::new("Synthetic schema8 memory upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &key, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        db.execute_batch("DROP TABLE memory_metadata; PRAGMA user_version=8; INSERT INTO agent_memories(id,principal_id,namespace,content,tags_json,created_at,updated_at,version) SELECT 'memory',principal_id,'facts','Retained fact','[]',1,1,1 FROM agent_profile;").unwrap();
        install_storage(&db).unwrap();
        let before = identity(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 1));
        assert_eq!(
            db.query_row(
                "SELECT content FROM agent_memories WHERE id='memory'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "Retained fact"
        );
        db.execute("INSERT INTO memory_metadata(memory_id,pinned,deleted,updated_at) VALUES('memory',1,0,2)",[]).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0, before.1 + 2));
        validate(&db).unwrap();
        crate::close(db).unwrap();
    }

    #[test]
    fn authenticated_schema7_upgrade_preserves_rows_and_audits_new_tables() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join(crate::DB_FILE);
        let key = crate::Secret::new("Synthetic schema7 upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &key, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v9::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v8::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        db.execute_batch("PRAGMA user_version=7; INSERT INTO settings VALUES('old','retained');")
            .unwrap();
        install_storage(&db).unwrap();
        let before = identity(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 1));
        assert_eq!(
            db.query_row("SELECT value FROM settings WHERE key='old'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "retained"
        );
        db.execute("INSERT INTO agent_definitions(id,principal_id,revision,body_json,created_at,updated_at) SELECT 'synthetic',principal_id,1,'{}',1,1 FROM agent_profile", []).unwrap();
        assert_eq!(identity(&db).unwrap(), (before.0.clone(), before.1 + 2));
        validate(&db).unwrap();
        crate::close(db).unwrap();
        let db = crate::unlock(&path, &key).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM agent_definitions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        crate::close(db).unwrap();
    }

    #[test]
    fn failed_schema8_ddl_leaves_authenticated_schema7_unchanged() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join(crate::DB_FILE);
        let key = crate::Secret::new("Synthetic failed schema upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &key, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v9::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v8::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        db.execute_batch("PRAGMA user_version=7; CREATE TABLE agent_definitions(sentinel TEXT); INSERT INTO agent_definitions VALUES('preserve conflict');").unwrap();
        install_storage(&db).unwrap();
        let before = identity(&db).unwrap();
        let signature = schema_signature(&db).unwrap();
        assert!(upgrade_encrypted(&db).is_err());
        assert_eq!(identity(&db).unwrap(), before);
        assert_eq!(schema_signature(&db).unwrap(), signature);
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            db.query_row("SELECT sentinel FROM agent_definitions", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "preserve conflict"
        );
        validate(&db).unwrap();
        crate::close(db).unwrap();
    }

    #[test]
    fn authenticated_schema6_upgrade_preserves_identity_and_advances_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(temporary.path())
            .unwrap()
            .join(crate::DB_FILE);
        let secret = crate::Secret::new("Synthetic schema6 upgrade key".into()).unwrap();
        let db = crate::open_cipher(&path, &secret, true).unwrap();
        initialize_schema(&db).unwrap();
        for (table, _) in crate::schema_v11::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v10::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v9::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v8::REQUIRED {
            db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                .unwrap();
        }
        for (table, _) in crate::schema_v7::REQUIRED {
            if *table != "agent_workspaces" {
                db.execute_batch(&format!("DROP TABLE {}", quote(table)))
                    .unwrap();
            }
        }
        db.execute_batch("ALTER TABLE agent_workspaces DROP COLUMN resources;PRAGMA user_version=6;INSERT INTO settings VALUES('old','retained');").unwrap();
        install_storage(&db).unwrap();
        let identity = identity(&db).unwrap();
        crate::close(db).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(crate::unlock(
            &path,
            &crate::Secret::new("Wrong upgrade key".into()).unwrap()
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let db = crate::unlock(&path, &secret).unwrap();
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(super::identity(&db).unwrap(), (identity.0, identity.1 + 1));
        assert_eq!(
            db.query_row("SELECT value FROM settings WHERE key='old'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "retained"
        );
        validate(&db).unwrap();
        crate::close(db).unwrap();
    }
}
