//! Saved user-authored personas on the already-keyed profile connection.
//! Definitions are preferences, never principals or executable authority.
use crate::{
    agent_api::{self, AgentError, Permission},
    agent_store::{self, AgentStore, WorkspacePolicy},
    App,
};
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smith_agent_core::{
    arguments_sha256, encode_transition, AgentModelTarget, AgentPersona, Capability, Error,
    GrantManifest, ResourceKind, ResourceScope, Task, TaskEngine, TaskState, ToolRegistry,
    TraceEvent, TraceRecord, TraceStore,
};
use std::{collections::BTreeSet, sync::Arc};

type Result<T> = smith_agent_core::Result<T>;
type ApiResult<T> = std::result::Result<T, AgentError>;
type Body<T> = std::result::Result<Json<T>, JsonRejection>;
const MAX_DEFINITIONS: i64 = 256;
const MAX_CONFIG_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum ModelPreference {
    SelectedAtLaunch,
    Pinned {
        provider: String,
        endpoint: String,
        model: String,
        scope_id: String,
    },
}
impl ModelPreference {
    fn target(&self) -> Option<AgentModelTarget> {
        match self {
            Self::SelectedAtLaunch => None,
            Self::Pinned {
                provider,
                endpoint,
                model,
                scope_id,
            } => Some(AgentModelTarget {
                provider: provider.clone(),
                endpoint: endpoint.clone(),
                model: model.clone(),
                scope_id: scope_id.clone(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefinitionConfig {
    name: String,
    system_prompt: String,
    soul: String,
    workspace_id: String,
    policy_revision: u64,
    capabilities: BTreeSet<Permission>,
    resources: BTreeSet<ResourceScope>,
    model: ModelPreference,
    #[serde(default)]
    extensions: smith_agent_core::InstructionSelection,
}
impl DefinitionConfig {
    fn validate(&self) -> Result<()> {
        self.extensions.validate()?;
        uuid(&self.workspace_id)?;
        revision(self.policy_revision)?;
        if self.name.trim().is_empty()
            || self.name.len() > 80
            || self.name.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput(
                "Agent name must contain 1–80 bytes without control characters".into(),
            ));
        }
        if self.system_prompt.len() > 8192 || self.soul.len() > 4096 {
            return Err(Error::LimitExceeded("Agent persona text".into()));
        }
        if self.resources.len() > 128
            || self
                .resources
                .iter()
                .any(|r| r.id.is_empty() || r.id.len() > 256 || r.id.chars().any(char::is_control))
        {
            return Err(Error::InvalidInput("Agent resource scopes".into()));
        }
        if let Some(target) = self.model.target() {
            target.validate()?;
            if !matches!(
                target.provider.as_str(),
                "openai" | "ollama" | "managed" | "mlx"
            ) || !target.scope_id.starts_with("provider:")
            {
                return Err(Error::InvalidInput(
                    "Choose an explicitly reviewed model target".into(),
                ));
            }
        }
        if encode(self)?.len() > MAX_CONFIG_BYTES {
            return Err(Error::LimitExceeded("Saved agent definition".into()));
        }
        Ok(())
    }
    fn capabilities(&self) -> BTreeSet<Capability> {
        self.capabilities.iter().map(|p| p.capability()).collect()
    }
    fn check_policy(&self, policy: &WorkspacePolicy) -> Result<()> {
        self.validate()?;
        if policy.id != self.workspace_id || policy.revision != self.policy_revision {
            return Err(Error::Conflict);
        }
        if policy.status != "active"
            || !self.capabilities().is_subset(&policy.capabilities)
            || !self.resources.is_subset(&policy.resources)
        {
            return Err(Error::Denied("Saved agent permissions must be a subset of the explicitly approved workspace policy".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct Definition {
    id: String,
    revision: u64,
    definition: DefinitionConfig,
    created_at: i64,
    updated_at: i64,
    deleted: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    definition: DefinitionConfig,
    confirmed: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    expected_revision: u64,
    definition: DefinitionConfig,
    confirmed: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    expected_revision: u64,
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
struct DefinitionQuery {
    #[serde(default)]
    include_deleted: bool,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    include_deleted: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewedTarget {
    revision: String,
    provider: String,
    endpoint: String,
}
impl ReviewedTarget {
    fn expected(&self) -> crate::Expected {
        crate::Expected {
            revision: self.revision.clone(),
            provider: self.provider.clone(),
            endpoint: self.endpoint.clone(),
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelReview {
    expected: ReviewedTarget,
    binding_id: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    request_id: String,
    expected_revision: u64,
    input: String,
    confirmed: bool,
    #[serde(default)]
    model_review: Option<ModelReview>,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/agent/definitions", get(list).post(create))
        .route(
            "/api/agent/definitions/{id}",
            get(get_one).put(update).delete(delete),
        )
        .route("/api/agent/definitions/{id}/launch", post(launch))
        .route("/api/agent/definitions/{id}/restore", post(restore))
        .route("/api/agent/definitions/{id}/purge", post(purge))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
}
fn body<T>(input: Body<T>) -> ApiResult<T> {
    input.map(|j| j.0).map_err(|_| {
        Error::InvalidInput("Invalid saved-agent request fields or JSON".into()).into()
    })
}
fn uuid(id: &str) -> Result<()> {
    if !uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id) {
        return Err(Error::InvalidInput("Expected a UUID identifier".into()));
    }
    Ok(())
}
fn revision(value: u64) -> Result<()> {
    if value == 0 || value >= i64::MAX as u64 {
        return Err(Error::InvalidInput(
            "Definition or policy revision is out of range".into(),
        ));
    }
    Ok(())
}
fn storage(_: rusqlite::Error) -> Error {
    Error::Persistence("Encrypted saved-agent storage failed".into())
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|_| Error::Persistence("Saved-agent encoding failed".into()))
}
fn decode_config(encoded: &str) -> Result<DefinitionConfig> {
    if encoded.len() > MAX_CONFIG_BYTES {
        return Err(Error::LimitExceeded("Stored agent definition".into()));
    }
    let config: DefinitionConfig = serde_json::from_str(encoded)
        .map_err(|_| Error::Persistence("Stored agent definition is invalid".into()))?;
    config.validate()?;
    Ok(config)
}
type Row = (String, u64, String, i64, i64, bool);
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
fn decoded(row: Row) -> Result<Definition> {
    uuid(&row.0)?;
    revision(row.1)?;
    Ok(Definition {
        id: row.0,
        revision: row.1,
        definition: decode_config(&row.2)?,
        created_at: row.3,
        updated_at: row.4,
        deleted: row.5,
    })
}
fn load(db: &Connection, owner: &str, id: &str) -> Result<Definition> {
    load_visible(db, owner, id, false)
}
fn load_visible(
    db: &Connection,
    owner: &str,
    id: &str,
    include_deleted: bool,
) -> Result<Definition> {
    let value = db.query_row("SELECT id,revision,body_json,created_at,updated_at,deleted FROM agent_definitions WHERE id=?1 AND principal_id=?2 AND (?3 OR deleted=0)", params![id,owner,include_deleted], row).optional().map_err(storage)?;
    decoded(value.ok_or(Error::NotFound)?)
}
async fn list(
    State(app): State<Arc<App>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    agent_api::ensure_open(&app)?;
    if let Some(after) = &query.after {
        uuid(after)?;
    }
    let limit = query.limit.unwrap_or(32);
    if !(1..=32).contains(&limit) {
        return Err(Error::InvalidInput("Saved-agent page size must be 1–32".into()).into());
    }
    let store = AgentStore::new(&app)?;
    let mut definitions = store.with_db(|db| {
        let mut statement = db.prepare("SELECT id,revision,body_json,created_at,updated_at,deleted FROM agent_definitions WHERE principal_id=?1 AND (?2 OR deleted=0) AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4").map_err(storage)?;
        let values = statement.query_map(params![store.owner().id(),query.include_deleted,query.after,(limit+1) as i64], row).map_err(storage)?.map(|r| decoded(r.map_err(storage)?)).collect::<Result<Vec<_>>>()?;
        Ok(values)
    })?;
    let next_after = if definitions.len() > limit {
        definitions.truncate(limit);
        definitions.last().map(|row| row.id.clone())
    } else {
        None
    };
    Ok(Json(
        json!({"definitions":definitions,"next_after":next_after,"limits":{"definitions_including_deleted":MAX_DEFINITIONS,"page_size":32,"name_bytes":80,"system_prompt_bytes":8192,"soul_bytes":4096,"task_input_bytes":16384},"model_modes":["selected_at_launch","pinned"],"launch_creates":"pending_task","capabilities_source":"selected_workspace"}),
    ))
}
async fn get_one(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(query): Query<DefinitionQuery>,
) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    agent_api::ensure_open(&app)?;
    let store = AgentStore::new(&app)?;
    let definition =
        store.with_db(|db| load_visible(db, store.owner().id(), &id, query.include_deleted))?;
    Ok(Json(json!({"definition":definition})))
}
async fn create(State(app): State<Arc<App>>, input: Body<Create>) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(
            Error::Denied("Confirm this saved agent and its selected permissions".into()).into(),
        );
    }
    input.definition.validate()?;
    agent_api::work(app, None, move |app| {
        let store = AgentStore::new(&app)?;
        agent_api::ensure_open(&app)?;
        let definition = store.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            input.definition.check_policy(&agent_store::policy(&tx, store.owner().id(), &input.definition.workspace_id)?)?;
            let count: i64 = tx.query_row("SELECT count(*) FROM agent_definitions WHERE principal_id=?1", [store.owner().id()], |r|r.get(0)).map_err(storage)?;
            if count >= MAX_DEFINITIONS { return Err(Error::LimitExceeded("Saved agents including retained deleted definitions".into())); }
            let id = crate::id();
            tx.execute("INSERT INTO agent_definitions(id,principal_id,revision,body_json,deleted,created_at,updated_at) VALUES(?1,?2,1,?3,0,?4,?4)", params![id,store.owner().id(),encode(&input.definition)?,crate::now()]).map_err(storage)?;
            let result = load(&tx,store.owner().id(),&id)?;
            tx.commit().map_err(storage)?;
            Ok(result)
        })?;
        Ok(Json(json!({"definition":definition})))
    }).await
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
    if !input.confirmed {
        return Err(Error::Denied("Confirm the saved agent changes".into()).into());
    }
    agent_api::work(app, None, move |app| {
        agent_api::ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        let definition = store.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let previous = load(&tx,store.owner().id(),&id)?;
            if previous.revision != input.expected_revision { return Err(Error::Conflict); }
            input.definition.check_policy(&agent_store::policy(&tx,store.owner().id(),&input.definition.workspace_id)?)?;
            if previous.definition != input.definition {
                let changed = tx.execute("UPDATE agent_definitions SET revision=revision+1,body_json=?1,updated_at=?2 WHERE id=?3 AND principal_id=?4 AND revision=?5 AND deleted=0", params![encode(&input.definition)?,crate::now(),id,store.owner().id(),input.expected_revision]).map_err(storage)?;
                if changed != 1 { return Err(Error::Conflict); }
            }
            let result = load(&tx,store.owner().id(),&id)?;
            tx.commit().map_err(storage)?;
            Ok(result)
        })?;
        Ok(Json(json!({"definition":definition})))
    }).await
}
async fn delete(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Delete>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    revision(input.expected_revision)?;
    agent_api::work(app, None, move |app| {
        agent_api::ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        store.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let current = load(&tx,store.owner().id(),&id)?;
            if current.revision != input.expected_revision { return Err(Error::Conflict); }
            let n = tx.execute("UPDATE agent_definitions SET deleted=1,revision=revision+1,updated_at=?1 WHERE id=?2 AND principal_id=?3 AND revision=?4 AND deleted=0",params![crate::now(),id,store.owner().id(),input.expected_revision]).map_err(storage)?;
            if n != 1 { return Err(Error::Conflict); }
            tx.commit().map_err(storage)
        })?;
        Ok(Json(json!({"id":id,"revision":input.expected_revision+1,"deleted":true,"task_history_preserved":true})))
    }).await
}

async fn restore(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Delete>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    revision(input.expected_revision)?;
    agent_api::work(app,None,move|app| {
        agent_api::ensure_open(&app)?;
        let store=AgentStore::new(&app)?;
        let definition=store.with_db(|db| {
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let current=load_visible(&tx,store.owner().id(),&id,true)?;
            if !current.deleted || current.revision!=input.expected_revision { return Err(Error::Conflict); }
            let changed=tx.execute("UPDATE agent_definitions SET deleted=0,revision=revision+1,updated_at=?1 WHERE id=?2 AND principal_id=?3 AND revision=?4 AND deleted=1",params![crate::now(),id,store.owner().id(),input.expected_revision]).map_err(storage)?;
            if changed!=1 { return Err(Error::Conflict); }
            let result=load(&tx,store.owner().id(),&id)?;
            tx.commit().map_err(storage)?;
            Ok(result)
        })?;
        Ok(Json(json!({"definition":definition})))
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
            "Confirm permanent removal of this exact deleted agent definition".into(),
        )
        .into());
    }
    agent_api::work(app,None,move|app| {
        agent_api::ensure_open(&app)?;
        let store=AgentStore::new(&app)?;
        store.with_db(|db| {
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let current=load_visible(&tx,store.owner().id(),&id,true)?;
            if !current.deleted || current.revision!=input.expected_revision { return Err(Error::Conflict); }
            let changed=tx.execute("DELETE FROM agent_definitions WHERE id=?1 AND principal_id=?2 AND revision=?3 AND deleted=1",params![id,store.owner().id(),input.expected_revision]).map_err(storage)?;
            if changed!=1 { return Err(Error::Conflict); }
            tx.commit().map_err(storage)
        })?;
        Ok(Json(json!({"id":id,"purged":true,"task_history_preserved":true,"launch_receipts_preserved":true})))
    }).await
}

fn launch_receipt(store: &AgentStore, request_id: &str, hash: &str) -> Result<Option<String>> {
    store.with_db(|db| {
        let prior: Option<(String,String)> = db.query_row("SELECT request_sha256,task_id FROM agent_definition_launches WHERE request_id=?1 AND principal_id=?2",params![request_id,store.owner().id()],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(storage)?;
        match prior {
            Some((prior_hash,task)) if prior_hash == hash => Ok(Some(task)),
            Some(_) => Err(Error::Conflict),
            None => Ok(None),
        }
    })
}
fn launch_response(store: &AgentStore, task_id: &str, replayed: bool) -> ApiResult<Json<Value>> {
    let Json(mut value) = agent_api::task_response(store, task_id, None)?;
    let task = store.load(task_id)?;
    let persona = task.agent_persona.ok_or_else(|| {
        Error::Persistence("Saved-agent launch receipt has no captured persona".into())
    })?;
    value["definition"] =
        json!({"id":persona.definition_id,"revision":persona.definition_revision});
    value["replayed"] = json!(replayed);
    value["model_started"] = json!(false);
    Ok(Json(value))
}
async fn launch(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Launch>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    uuid(&input.request_id)?;
    revision(input.expected_revision)?;
    if !input.confirmed {
        return Err(
            Error::Denied("Confirm creation of a task from this exact saved agent".into()).into(),
        );
    }
    if input.input.trim().is_empty() || input.input.len() > 16384 {
        return Err(Error::InvalidInput("Task input must contain 1–16384 bytes".into()).into());
    }
    let hash = arguments_sha256(&json!({"definition_id":id,"request":input}));
    agent_api::work(app, None, move |app| {
        agent_api::ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        if let Some(task_id) = launch_receipt(&store,&input.request_id,&hash)? {
            return launch_response(&store,&task_id,true);
        }
        let definition = store.with_db(|db|load(db,store.owner().id(),&id))?;
        if definition.revision != input.expected_revision { return Err(Error::Conflict.into()); }
        let config = &definition.definition;
        let policy = store.policy(&config.workspace_id)?;
        config.check_policy(&policy)?;
        let grant = store.grant(&policy)?.delegate_scoped(format!("saved-agent:{id}"),config.capabilities(),config.resources.iter().cloned())?;
        let model_target = if grant.permits(Capability::ModelInference) {
            let review = input.model_review.as_ref().ok_or_else(||Error::Denied("Review the selected model before creating this agent task".into()))?;
            let selection = crate::task_model::selection(&app,&review.expected.expected()).map_err(agent_api::model_error)?;
            if selection.binding_id != review.binding_id { return Err(Error::Conflict.into()); }
            let target = AgentModelTarget { provider:selection.config.provider.clone(),endpoint:selection.config.endpoint.clone(),model:selection.config.model.clone(),scope_id:selection.scope_id.clone() };
            if config.model.target().is_some_and(|pinned|pinned != target) {
                return Err(Error::Denied("Select the saved agent's pinned model target before creating this task".into()).into());
            }
            if !grant.permits_resource(ResourceKind::ProviderBinding,&target.scope_id) {
                return Err(Error::Denied("The selected model target is not in the saved agent's explicit resource grants".into()).into());
            }
            Some(target)
        } else {
            if input.model_review.is_some() { return Err(Error::InvalidInput("This saved agent does not grant model inference".into()).into()); }
            None
        };
        let persona = AgentPersona { definition_id:id,definition_revision:definition.revision,name:config.name.clone(),system_prompt:config.system_prompt.clone(),soul:config.soul.clone(),model_target };
        let extensions=store.with_db(|db|crate::agent_extensions::capture(db,store.owner().id(),&config.extensions))?;
        let launches = LaunchStore { store:store.clone(),definition,request_id:input.request_id.clone(),hash:hash.clone(),grant:grant.manifest(),persona:persona.clone() };
        let created = TaskEngine::new(launches,ToolRegistry::default()).create_with_context(&grant,input.input,Some(persona),extensions);
        match created {
            Ok(task) => launch_response(&store,&task.id,false),
            Err(Error::Conflict) => {
                if let Some(task_id) = launch_receipt(&store,&input.request_id,&hash)? {
                    launch_response(&store,&task_id,true)
                } else { Err(Error::Conflict.into()) }
            }
            Err(e) => Err(e.into()),
        }
    }).await
}

/// Creation-only transaction: definition CAS, workspace authority, initial task,
/// initial trace and exact retry receipt either all commit or all roll back.
struct LaunchStore {
    store: AgentStore,
    definition: Definition,
    request_id: String,
    hash: String,
    grant: GrantManifest,
    persona: AgentPersona,
}
impl TraceStore for LaunchStore {
    fn load(&self, id: &str) -> Result<Task> {
        self.store.load(id)
    }
    fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
        self.store.events(id)
    }
    fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
        if expected.is_some()
            || task.revision != 0
            || task.state != TaskState::Pending
            || task.grant != self.grant
            || task.agent_persona.as_ref() != Some(&self.persona)
            || task.grant.principal != self.store.owner().id()
            || !matches!(event, TraceEvent::Created)
        {
            return Err(Error::Denied("Saved-agent launch identity changed".into()));
        }
        let (snapshot, event) = encode_transition(None, task, event)?;
        self.store.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let current = load(&tx,self.store.owner().id(),&self.definition.id)?;
            if current.revision != self.definition.revision || current.definition != self.definition.definition { return Err(Error::Conflict); }
            let config = &current.definition;
            if task.extensions.selection!=config.extensions { return Err(Error::Conflict); }
            crate::agent_extensions::validate_capture(&tx,self.store.owner().id(),&task.extensions)?;
            let policy = agent_store::policy(&tx,self.store.owner().id(),&config.workspace_id)?;
            config.check_policy(&policy)?;
            if policy.identity != task.grant.workspace || task.grant.capabilities != config.capabilities() || task.grant.resources != config.resources { return Err(Error::Denied("Captured workspace or grant changed".into())); }
            let prior:bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_definition_launches WHERE request_id=?1)",[&self.request_id],|r|r.get(0)).map_err(storage)?;
            if prior { return Err(Error::Conflict); }
            let count:i64 = tx.query_row("SELECT count(*) FROM agent_tasks WHERE principal_id=?1",[self.store.owner().id()],|r|r.get(0)).map_err(storage)?;
            if count >= 256 { return Err(Error::LimitExceeded("Profile tasks".into())); }
            tx.execute("INSERT INTO agent_tasks(id,workspace_id,principal_id,revision,snapshot,created_at,updated_at) VALUES(?1,?2,?3,0,?4,?5,?5)",params![task.id,config.workspace_id,self.store.owner().id(),snapshot,crate::now()]).map_err(storage)?;
            tx.execute("INSERT INTO agent_task_trace(task_id,revision,event) VALUES(?1,0,?2)",params![task.id,event]).map_err(storage)?;
            tx.execute("INSERT INTO agent_definition_launches(request_id,principal_id,definition_id,definition_revision,task_id,request_sha256,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![self.request_id,self.store.owner().id(),current.id,current.revision,task.id,self.hash,crate::now()]).map_err(storage)?;
            tx.commit().map_err(storage)
        })
    }
}

#[cfg(test)]
#[path = "../tests/support/agent_definitions.rs"]
mod tests;
