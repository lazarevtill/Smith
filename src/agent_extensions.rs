//! Encrypted declarative extensions. Definitions and imported records are data;
//! enabled instructions are captured only when the user creates a new task.
#[path = "agent_plugin_import.rs"]
pub(crate) mod plugin_import;
use crate::{
    agent_api::{self, AgentError},
    agent_store::AgentStore,
    App,
};
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    routing::{get, patch, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smith_agent_core::{
    arguments_sha256, CapturedInstruction, CapturedInstructions, Error, InstructionKind,
    InstructionRef, InstructionSelection,
};
use std::{collections::BTreeSet, sync::Arc};

type Result<T> = smith_agent_core::Result<T>;
type ApiResult<T> = std::result::Result<T, AgentError>;
type Body<T> = std::result::Result<Json<T>, JsonRejection>;
const MAX_RECORDS: i64 = 256;
const MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Skill,
    Method,
    Plugin,
    Recipe,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Method => "method",
            Self::Plugin => "plugin",
            Self::Recipe => "recipe",
        }
    }
    fn instruction(self) -> Option<InstructionKind> {
        match self {
            Self::Skill => Some(InstructionKind::Skill),
            Self::Method => Some(InstructionKind::Method),
            Self::Plugin => Some(InstructionKind::Plugin),
            Self::Recipe => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Definition {
    Skill {
        name: String,
        description: String,
        instructions: String,
        #[serde(default)]
        category: String,
    },
    Method {
        name: String,
        description: String,
        instructions: String,
    },
    Plugin {
        manifest_id: String,
        name: String,
        description: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        authors: Vec<String>,
        #[serde(default)]
        homepage: Option<String>,
        #[serde(default)]
        license: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        instructions_file: Option<String>,
        #[serde(default)]
        tools: Vec<String>,
    },
    Recipe {
        name: String,
        description: String,
        command: String,
        parameters: Value,
    },
}
impl Definition {
    fn kind(&self) -> Kind {
        match self {
            Self::Skill { .. } => Kind::Skill,
            Self::Method { .. } => Kind::Method,
            Self::Plugin { .. } => Kind::Plugin,
            Self::Recipe { .. } => Kind::Recipe,
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Skill { name, .. }
            | Self::Method { name, .. }
            | Self::Plugin { name, .. }
            | Self::Recipe { name, .. } => name,
        }
    }
    fn description(&self) -> &str {
        match self {
            Self::Skill { description, .. }
            | Self::Method { description, .. }
            | Self::Plugin { description, .. }
            | Self::Recipe { description, .. } => description,
        }
    }
    fn instructions(&self) -> Option<&str> {
        match self {
            Self::Skill { instructions, .. } | Self::Method { instructions, .. } => {
                Some(instructions)
            }
            Self::Plugin { instructions, .. } => Some(instructions.as_deref().unwrap_or("")),
            Self::Recipe { .. } => None,
        }
    }
    fn validate(&self) -> Result<()> {
        label(self.name(), 80, "Extension name")?;
        bounded(self.description(), 2048, "Extension description")?;
        if let Some(text) = self.instructions() {
            bounded(text, 8192, "Extension instructions")?;
        }
        match self {
            Self::Skill { category, .. } => bounded(category, 128, "Skill category")?,
            Self::Method { .. } => {}
            Self::Plugin {
                manifest_id,
                version,
                authors,
                homepage,
                license,
                instructions,
                instructions_file,
                tools,
                ..
            } => {
                label(manifest_id, 128, "Plugin manifest ID")?;
                bounded(version, 128, "Plugin version")?;
                if authors.len() > 16 || tools.len() > 64 {
                    return Err(Error::LimitExceeded("Plugin metadata entries".into()));
                }
                for author in authors {
                    label(author, 128, "Plugin author")?;
                }
                for tool in tools {
                    label(tool, 128, "Declared plugin tool")?;
                }
                if let Some(homepage) = homepage {
                    bounded(homepage, 2048, "Plugin homepage")?;
                }
                if let Some(license) = license {
                    bounded(license, 256, "Plugin license metadata")?;
                }
                if let Some(file) = instructions_file {
                    // This is retained metadata, never a host path to read. The
                    // current inline importer requires explicitly supplied text.
                    bounded(file, 512, "Plugin instruction file reference")?;
                    if instructions.is_none() {
                        return Err(Error::Unsupported("Resolve the explicitly selected plugin instruction file before saving; this endpoint reads no files".into()));
                    }
                }
            }
            Self::Recipe {
                command,
                parameters,
                ..
            } => {
                bounded(command, 8192, "Recipe command")?;
                if command.trim().is_empty() || !parameters.is_object() {
                    return Err(Error::InvalidInput(
                        "Recipe command and parameter object are required".into(),
                    ));
                }
                if encode(parameters)?.len() > 8192 || json_depth(parameters) > 8 {
                    return Err(Error::LimitExceeded("Recipe parameter schema".into()));
                }
            }
        }
        if encode(self)?.len() > MAX_BYTES {
            return Err(Error::LimitExceeded("Encoded extension definition".into()));
        }
        Ok(())
    }
}
fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(a) => 1 + a.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}
fn bounded(value: &str, max: usize, field: &str) -> Result<()> {
    if value.len() > max {
        Err(Error::LimitExceeded(field.into()))
    } else {
        Ok(())
    }
}
fn label(value: &str, max: usize, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        Err(Error::InvalidInput(format!(
            "{field} must contain 1–{max} bytes without control characters"
        )))
    } else {
        Ok(())
    }
}
fn uuid(id: &str) -> Result<()> {
    InstructionRef {
        id: id.into(),
        revision: 1,
    }
    .validate()
}
fn revision(rev: u64) -> Result<()> {
    if rev == 0 || rev >= i64::MAX as u64 {
        Err(Error::InvalidInput(
            "Extension revision is out of range".into(),
        ))
    } else {
        Ok(())
    }
}
fn db_error(_: rusqlite::Error) -> Error {
    Error::Persistence("Encrypted extension storage failed".into())
}
fn encode<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|_| Error::Persistence("Extension encoding failed".into()))
}
fn body<T>(v: Body<T>) -> ApiResult<T> {
    v.map(|j| j.0)
        .map_err(|_| Error::InvalidInput("Invalid extension request fields or JSON".into()).into())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Origin {
    Authored,
    SelectedDirectory {
        source: smith_agent_core::PluginSourceEvidence,
    },
    Legacy {
        source_table: String,
        row_number: i64,
        record_id: Option<String>,
        source_sha256: String,
        original_enabled: bool,
    },
}
#[derive(Clone, Serialize)]
struct Record {
    id: String,
    revision: u64,
    definition: Definition,
    enabled: bool,
    deleted: bool,
    provenance: Origin,
    created_at: i64,
    updated_at: i64,
}
impl Record {
    fn summary(&self) -> Value {
        json!({"id":self.id,"revision":self.revision,"kind":self.definition.kind(),"name":self.definition.name(),"description":self.definition.description(),"enabled":self.enabled,"deleted":self.deleted,"instruction_bytes":self.definition.instructions().map(str::len),"execution_status":if self.definition.kind()==Kind::Recipe{"unsupported_wsl2_recipe"}else{"instruction_data"},"provenance":self.provenance,"created_at":self.created_at,"updated_at":self.updated_at})
    }
    fn view(&self) -> Value {
        let mut v = self.summary();
        v["definition"] = json!(self.definition);
        v
    }
    fn captured(&self) -> Result<CapturedInstruction> {
        if !self.enabled || self.deleted {
            return Err(Error::Denied(
                "Selected extension is disabled or in Trash".into(),
            ));
        }
        let kind = self.definition.kind().instruction().ok_or_else(|| {
            Error::Unsupported("Recipes cannot be captured as instruction text".into())
        })?;
        CapturedInstruction::new(
            InstructionRef {
                id: self.id.clone(),
                revision: self.revision,
            },
            kind,
            self.definition.name().into(),
            self.definition.description().into(),
            self.definition.instructions().unwrap_or("").into(),
        )
    }
}
type RawRow = (String, u64, String, String, bool, bool, String, i64, i64);
fn raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}
fn decode(row: RawRow) -> Result<Record> {
    uuid(&row.0)?;
    revision(row.1)?;
    bounded(&row.3, MAX_BYTES, "Stored extension definition")?;
    bounded(&row.6, 8192, "Stored extension provenance")?;
    let definition: Definition = serde_json::from_str(&row.3)
        .map_err(|_| Error::Persistence("Stored extension definition is invalid".into()))?;
    definition.validate()?;
    if definition.kind().name() != row.2 {
        return Err(Error::Persistence("Stored extension kind mismatch".into()));
    }
    let provenance: Origin = serde_json::from_str(&row.6)
        .map_err(|_| Error::Persistence("Stored extension provenance is invalid".into()))?;
    Ok(Record {
        id: row.0,
        revision: row.1,
        definition,
        enabled: row.4,
        deleted: row.5,
        provenance,
        created_at: row.7,
        updated_at: row.8,
    })
}
fn load(db: &Connection, owner: &str, id: &str, include_deleted: bool) -> Result<Record> {
    let row=db.query_row("SELECT id,revision,kind,body_json,enabled,deleted,provenance_json,created_at,updated_at FROM agent_extensions WHERE principal_id=?1 AND id=?2 AND (?3 OR deleted=0)",params![owner,id,include_deleted],raw).optional().map_err(db_error)?;
    decode(row.ok_or(Error::NotFound)?)
}

/// Resolve current enabled definitions exactly once, during task creation.
/// Call `validate_capture` in the task insertion transaction to close races.
pub(crate) fn capture(
    db: &Connection,
    owner: &str,
    selection: &InstructionSelection,
) -> Result<CapturedInstructions> {
    selection.validate()?;
    let items = match selection {
        InstructionSelection::Enabled => {
            let mut statement=db.prepare("SELECT id,revision,kind,body_json,enabled,deleted,provenance_json,created_at,updated_at FROM agent_extensions WHERE principal_id=?1 AND enabled=1 AND deleted=0 AND kind!='recipe' ORDER BY kind,id LIMIT 13").map_err(db_error)?;
            let rows = statement.query_map([owner], raw).map_err(db_error)?;
            rows.map(|r| decode(r.map_err(db_error)?)?.captured())
                .collect::<Result<Vec<_>>>()?
        }
        InstructionSelection::Explicit { references } => references
            .iter()
            .map(|reference| {
                let record = load(db, owner, &reference.id, false)?;
                if record.revision != reference.revision {
                    return Err(Error::Conflict);
                }
                record.captured()
            })
            .collect::<Result<Vec<_>>>()?,
    };
    CapturedInstructions::new(selection.clone(), items)
}
pub(crate) fn validate_capture(
    db: &Connection,
    owner: &str,
    captured: &CapturedInstructions,
) -> Result<()> {
    captured.validate()?;
    if capture(db, owner, &captured.selection)? != *captured {
        return Err(Error::Conflict);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    definition: Definition,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    expected_revision: u64,
    definition: Definition,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enable {
    expected_revision: u64,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Purge {
    expected_revision: u64,
    confirm_id: String,
    permanent: bool,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<String>,
    limit: Option<usize>,
    kind: Option<Kind>,
    enabled: Option<bool>,
    #[serde(default)]
    include_deleted: bool,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetQuery {
    #[serde(default)]
    include_deleted: bool,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(plugin_import::routes())
        .route("/api/agent/extensions", get(list).post(create))
        .route("/api/agent/extensions/import-review", post(import_review))
        .route("/api/agent/extensions/import", post(import))
        .route(
            "/api/agent/extensions/{id}",
            get(get_one).put(update).delete(trash),
        )
        .route("/api/agent/extensions/{id}/enabled", patch(enable))
        .route("/api/agent/extensions/{id}/restore", post(restore))
        .route("/api/agent/extensions/{id}/purge", post(purge))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
}
async fn list(
    State(app): State<Arc<App>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    agent_api::ensure_open(&app)?;
    if let Some(id) = &query.after {
        uuid(id)?;
    }
    let limit = query.limit.unwrap_or(32);
    if !(1..=32).contains(&limit) {
        return Err(Error::InvalidInput("Extension page size must be 1–32".into()).into());
    }
    let store = AgentStore::new(&app)?;
    let mut records=store.with_db(|db|{
        let mut q=db.prepare("SELECT id,revision,kind,body_json,enabled,deleted,provenance_json,created_at,updated_at FROM agent_extensions WHERE principal_id=?1 AND (?2 OR deleted=0) AND (?3 IS NULL OR kind=?3) AND (?4 IS NULL OR enabled=?4) AND (?5 IS NULL OR id>?5) ORDER BY id LIMIT ?6").map_err(db_error)?;
        let rows=q.query_map(params![store.owner().id(),query.include_deleted,query.kind.map(Kind::name),query.enabled,query.after,(limit+1) as i64],raw).map_err(db_error)?;
        rows.map(|r|decode(r.map_err(db_error)?)).collect::<Result<Vec<_>>>()
    })?;
    let next = if records.len() > limit {
        records.truncate(limit);
        records.last().map(|r| r.id.clone())
    } else {
        None
    };
    Ok(Json(
        json!({"extensions":records.iter().map(Record::summary).collect::<Vec<_>>(),"next_after":next,"limits":{"records_including_deleted":256,"page_size":32,"definition_bytes":MAX_BYTES,"captured_bytes":16384,"captured_per_kind":4},"default_capture":"enabled","recipe_execution":"unsupported_wsl2_recipe"}),
    ))
}
async fn get_one(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(query): Query<GetQuery>,
) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    agent_api::ensure_open(&app)?;
    let store = AgentStore::new(&app)?;
    Ok(Json(
        json!({"extension":store.with_db(|db|load(db,store.owner().id(),&id,query.include_deleted))?.view()}),
    ))
}
fn insert(
    db: &Connection,
    owner: &str,
    definition: &Definition,
    enabled: bool,
    provenance: &Origin,
) -> Result<Record> {
    definition.validate()?;
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM agent_extensions WHERE principal_id=?1",
            [owner],
            |r| r.get(0),
        )
        .map_err(db_error)?;
    if count >= MAX_RECORDS {
        return Err(Error::LimitExceeded(
            "Extensions including retained Trash (256)".into(),
        ));
    }
    let id = crate::id();
    db.execute("INSERT INTO agent_extensions(id,principal_id,kind,revision,body_json,enabled,deleted,provenance_json,created_at,updated_at) VALUES(?1,?2,?3,1,?4,?5,0,?6,?7,?7)",params![id,owner,definition.kind().name(),encode(definition)?,enabled,encode(provenance)?,crate::now()]).map_err(db_error)?;
    load(db, owner, &id, false)
}
async fn create(State(app): State<Arc<App>>, input: Body<Create>) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    input.definition.validate()?;
    agent_api::work(app, None, move |app| {
        agent_api::ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        let record = store.with_db(|db| {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let record = insert(
                &tx,
                store.owner().id(),
                &input.definition,
                input.enabled,
                &Origin::Authored,
            )?;
            tx.commit().map_err(db_error)?;
            Ok(record)
        })?;
        Ok(Json(json!({"extension":record.view()})))
    })
    .await
}
async fn update(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Update>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    revision(input.expected_revision)?;
    input.definition.validate()?;
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;let store=AgentStore::new(&app)?;
        let record=store.with_db(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;let previous=load(&tx,store.owner().id(),&id,false)?;
            if previous.revision!=input.expected_revision{return Err(Error::Conflict);}
            if previous.definition.kind()!=input.definition.kind(){return Err(Error::InvalidInput("An extension's kind is immutable".into()));}
            if previous.definition!=input.definition || previous.enabled!=input.enabled{
                tx.execute("UPDATE agent_extensions SET body_json=?1,enabled=?2,revision=revision+1,updated_at=?3 WHERE id=?4 AND principal_id=?5",params![encode(&input.definition)?,input.enabled,crate::now(),id,store.owner().id()]).map_err(db_error)?;
            }
            let record=load(&tx,store.owner().id(),&id,false)?;tx.commit().map_err(db_error)?;Ok(record)
        })?;Ok(Json(json!({"extension":record.view()})))
    }).await
}
async fn enable(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Enable>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    mutate(
        app,
        id,
        input.expected_revision,
        Mutation::Enabled(input.enabled),
    )
    .await
}
async fn trash(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Version>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    mutate(app, id, input.expected_revision, Mutation::Trash).await
}
async fn restore(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Version>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    mutate(app, id, input.expected_revision, Mutation::Restore).await
}
enum Mutation {
    Enabled(bool),
    Trash,
    Restore,
}
async fn mutate(
    app: Arc<App>,
    id: String,
    expected: u64,
    mutation: Mutation,
) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    revision(expected)?;
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;let store=AgentStore::new(&app)?;
        let record=store.with_db(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;let current=load(&tx,store.owner().id(),&id,true)?;
            if current.revision!=expected{return Err(Error::Conflict);}
            let (enabled,deleted)=match mutation {
                Mutation::Enabled(value) if !current.deleted=>(value,false),
                Mutation::Trash if !current.deleted=>(current.enabled,true),
                Mutation::Restore if current.deleted=>(current.enabled,false),
                _=>return Err(Error::Conflict),
            };
            if current.enabled!=enabled || current.deleted!=deleted{
                tx.execute("UPDATE agent_extensions SET enabled=?1,deleted=?2,revision=revision+1,updated_at=?3 WHERE id=?4 AND principal_id=?5",params![enabled,deleted,crate::now(),id,store.owner().id()]).map_err(db_error)?;
            }
            let record=load(&tx,store.owner().id(),&id,true)?;tx.commit().map_err(db_error)?;Ok(record)
        })?;Ok(Json(json!({"extension":record.view(),"task_history_preserved":true})))
    }).await
}
async fn purge(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Purge>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    revision(input.expected_revision)?;
    if !input.permanent || input.confirm_id != id {
        return Err(Error::Denied(
            "Confirm permanent removal of this exact trashed extension".into(),
        )
        .into());
    }
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;let store=AgentStore::new(&app)?;
        store.with_db(|db|{let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;let current=load(&tx,store.owner().id(),&id,true)?;if !current.deleted || current.revision!=input.expected_revision{return Err(Error::Conflict);}
            tx.execute("DELETE FROM agent_extensions WHERE id=?1 AND principal_id=?2",params![id,store.owner().id()]).map_err(db_error)?;tx.commit().map_err(db_error)})?;
        Ok(Json(json!({"id":id,"purged":true,"task_history_preserved":true,"import_receipts_preserved":true})))
    }).await
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacySelection {
    source_table: String,
    row_number: i64,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Import {
    request_id: String,
    source_table: String,
    row_number: i64,
    expected_source_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactRow {
    columns: Vec<String>,
    values: Vec<ExactValue>,
}
#[derive(Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ExactValue {
    Null,
    Integer(String),
    Text(String),
    #[serde(other)]
    Unsupported,
}
struct Imported {
    definition: Definition,
    origin: Origin,
    source_sha256: String,
}
fn imported(db: &Connection, selection: &LegacySelection) -> Result<Imported> {
    if !matches!(
        selection.source_table.as_str(),
        "skills" | "methods" | "tool_recipes"
    ) || selection.row_number < 0
    {
        return Err(Error::InvalidInput(
            "Select an original skill, method or tool recipe row".into(),
        ));
    }
    let (record_id,payload):(Option<String>,String)=db.query_row("SELECT record_id,payload_json FROM legacy_records WHERE table_name=?1 AND row_number=?2",params![selection.source_table,selection.row_number],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?.ok_or(Error::NotFound)?;
    bounded(&payload, 64 * 1024, "Original extension row")?;
    let source_sha256 = arguments_sha256(
        &json!({"source_table":selection.source_table,"row_number":selection.row_number,"payload":payload}),
    );
    let row: ExactRow = serde_json::from_str(&payload).map_err(|_| {
        Error::InvalidInput("Original extension row encoding is unsupported".into())
    })?;
    if row.columns.len() != row.values.len()
        || row.columns.len() > 32
        || row.columns.iter().collect::<BTreeSet<_>>().len() != row.columns.len()
    {
        return Err(Error::InvalidInput(
            "Original extension columns are malformed".into(),
        ));
    }
    let value = |key: &str| -> Result<&ExactValue> {
        let index = row
            .columns
            .iter()
            .position(|c| c == key)
            .ok_or_else(|| Error::InvalidInput(format!("Original extension lacks {key}")))?;
        Ok(&row.values[index])
    };
    let text = |key: &str| -> Result<String> {
        match value(key)? {
            ExactValue::Text(s) => Ok(s.clone()),
            _ => Err(Error::InvalidInput(format!(
                "Original extension {key} is not UTF-8 text"
            ))),
        }
    };
    let enabled = match value("enabled")? {
        ExactValue::Integer(s) if s == "0" => false,
        ExactValue::Integer(s) if s == "1" => true,
        _ => {
            return Err(Error::InvalidInput(
                "Original enabled flag is not a boolean integer".into(),
            ))
        }
    };
    let name = text("name")?;
    let description = text("description")?;
    let definition = match selection.source_table.as_str() {
        "skills" => Definition::Skill {
            name,
            description,
            instructions: text("instructions")?,
            category: text("category")?,
        },
        "methods" => Definition::Method {
            name,
            description,
            instructions: text("instructions")?,
        },
        "tool_recipes" => Definition::Recipe {
            name,
            description,
            command: text("command")?,
            parameters: serde_json::from_str(&text("parameters")?).map_err(|_| {
                Error::InvalidInput("Original recipe schema is invalid JSON".into())
            })?,
        },
        _ => unreachable!(),
    };
    definition.validate()?;
    Ok(Imported {
        definition,
        origin: Origin::Legacy {
            source_table: selection.source_table.clone(),
            row_number: selection.row_number,
            record_id,
            source_sha256: source_sha256.clone(),
            original_enabled: enabled,
        },
        source_sha256,
    })
}
async fn import_review(
    State(app): State<Arc<App>>,
    input: Body<LegacySelection>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    agent_api::ensure_open(&app)?;
    let store = AgentStore::new(&app)?;
    let source = store.with_db(|db| imported(db, &input))?;
    Ok(Json(
        json!({"definition":source.definition,"provenance":source.origin,"source_sha256":source.source_sha256,"enabled_on_import":false,"execution": "none"}),
    ))
}
async fn import(State(app): State<Arc<App>>, input: Body<Import>) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&input.request_id)?;
    let request_sha256 = arguments_sha256(&json!(input));
    agent_api::work(app,None,move|app|{
        agent_api::ensure_open(&app)?;let store=AgentStore::new(&app)?;
        let result=store.with_db(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
            let prior:Option<(String,String)>=tx.query_row("SELECT request_sha256,result_json FROM agent_extension_imports WHERE request_id=?1 AND principal_id=?2",params![input.request_id,store.owner().id()],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
            if let Some((hash,result))=prior{
                if hash!=request_sha256{return Err(Error::Conflict);}
                bounded(&result,64*1024,"Stored extension import receipt")?;
                let mut result:Value=serde_json::from_str(&result).map_err(|_|Error::Persistence("Stored extension import receipt is invalid".into()))?;result["replayed"]=json!(true);return Ok(result);
            }
            let source=imported(&tx,&LegacySelection{source_table:input.source_table,row_number:input.row_number})?;
            if source.source_sha256!=input.expected_source_sha256{return Err(Error::Conflict);}
            let count:i64=tx.query_row("SELECT count(*) FROM agent_extension_imports WHERE principal_id=?1",[store.owner().id()],|r|r.get(0)).map_err(db_error)?;
            if count>=1024{return Err(Error::LimitExceeded("Retained extension import receipts (1024)".into()));}
            let record=insert(&tx,store.owner().id(),&source.definition,false,&source.origin)?;
            let result=json!({"extension":record.view(),"replayed":false,"execution":"none"});
            tx.execute("INSERT INTO agent_extension_imports(request_id,principal_id,source_sha256,request_sha256,extension_id,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![input.request_id,store.owner().id(),source.source_sha256,request_sha256,record.id,encode(&result)?,crate::now()]).map_err(db_error)?;
            tx.commit().map_err(db_error)?;Ok(result)
        })?;Ok(Json(result))
    }).await
}

#[cfg(test)]
#[path = "../tests/support/agent_extensions.rs"]
mod tests;
