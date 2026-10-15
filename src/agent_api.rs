//! Explicit tasks and bounded selected-model steps with independent host authority.
#[path = "agent_domain.rs"]
pub(crate) mod domain;
#[path = "agent_effects.rs"]
mod effects;
#[path = "agent_platform.rs"]
mod platform;
use crate::{
    agent_store::{AgentStore, WorkspacePolicy},
    App,
};
use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smith_agent_core::{
    ApprovalDecision, Availability, Capability, Error, HostToolAdapter, RecoveryDecision,
    ResourceScope, Task, TaskEngine, TaskState, ToolCall, ToolOutcome, ToolRegistry, TraceStore,
    Workspace,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
};
use tokio::sync::{oneshot, Notify};

type AResult<T> = Result<T, AgentError>;
type Body<T> = Result<Json<T>, JsonRejection>;

#[derive(Debug)]
pub struct AgentError(Error);
impl From<Error> for AgentError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for AgentError {
    fn into_response(self) -> Response {
        let (status, code, message) = public_error(&self.0);
        (status, Json(json!({"error":message,"code":code}))).into_response()
    }
}
fn public_error(error: &Error) -> (StatusCode, &'static str, String) {
    match error {
            Error::Conflict => (StatusCode::CONFLICT,"conflict","The task or file changed. Refresh and review the current state before trying again.".into()),
            Error::NotFound => (StatusCode::NOT_FOUND,"not_found","Task or workspace not found in this profile.".into()),
            Error::Denied(detail) => (StatusCode::FORBIDDEN,"denied",detail.clone()),
            Error::Unsupported(detail) => (StatusCode::UNPROCESSABLE_ENTITY,"unsupported",detail.clone()),
            Error::InvalidInput(detail) => (StatusCode::BAD_REQUEST,"invalid_input",detail.clone()),
            Error::InvalidTransition(detail) => (StatusCode::CONFLICT,"invalid_transition",detail.clone()),
            Error::LimitExceeded(detail) => (StatusCode::PAYLOAD_TOO_LARGE,"limit_exceeded",detail.clone()),
            Error::UnsafePath(_) => (StatusCode::BAD_REQUEST,"unsafe_path","The file path or temporary workspace is unsafe. Only direct-child files in the provisioned workspace are supported.".into()),
            Error::Io(_) => (StatusCode::CONFLICT,"workspace_unavailable","The temporary workspace could not be opened. Existing task history is retained; you can inspect it or cancel the task.".into()),
            Error::UncertainSideEffect(_) => (StatusCode::CONFLICT,"uncertain_side_effect","The operation may have had an effect. Inspect the affected resource and explicitly resolve the interrupted task.".into()),
            Error::Persistence(_) => (StatusCode::INTERNAL_SERVER_ERROR,"storage","Encrypted task storage failed. Reload the task before retrying; an operation may already have completed.".into()),
        }
}
fn body<T>(input: Body<T>) -> AResult<T> {
    input
        .map(|j| j.0)
        .map_err(|_| Error::InvalidInput("Invalid request fields or JSON.".into()).into())
}
fn uuid(value: &str) -> AResult<()> {
    uuid::Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| Error::InvalidInput("Expected a UUID identifier.".into()).into())
}

#[derive(Default)]
pub struct AgentState {
    work: Arc<WorkState>,
    adapters: Mutex<Vec<Arc<dyn HostToolAdapter>>>,
    effects: Arc<effects::Effects>,
    platform: Mutex<Option<Arc<platform::Platform>>>,
    pub(crate) plugin_imports: crate::agent_extensions::plugin_import::ReviewCache,
}
#[derive(Default)]
struct WorkState {
    inner: Mutex<Active>,
    changed: Notify,
}
#[derive(Default)]
struct Active {
    closing: bool,
    jobs: HashMap<String, Option<String>>,
    model_cancels: HashMap<String, ModelControl>,
    model_operations: HashMap<String, ModelOperation>,
    model_sequence: u64,
}
struct ModelControl {
    cancel: tokio::sync::watch::Sender<bool>,
    finished: tokio::sync::watch::Receiver<Option<ModelTerminal>>,
}
const MAX_MODEL_OPERATIONS: usize = 128;
#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ModelPhase {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Serialize)]
struct ModelTerminal {
    phase: ModelPhase,
    task_revision: Option<u64>,
    stop_reason: &'static str,
    error: Option<ModelOperationError>,
    worker_drained: bool,
    cleanup_unconfirmed: bool,
    cancellation: Option<crate::task_model::CancellationEvidence>,
}
#[derive(Clone, Serialize)]
struct ModelOperationError {
    code: &'static str,
    message: String,
}
#[derive(Clone, Serialize)]
struct ModelOperation {
    id: String,
    task_id: String,
    submitted_revision: u64,
    max_steps: usize,
    scope_id: String,
    binding_id: String,
    sequence: u64,
    #[serde(flatten)]
    state: ModelTerminal,
}
struct WorkLease {
    state: Arc<WorkState>,
    id: String,
    terminal: Option<ModelTerminal>,
    model_done: Option<tokio::sync::watch::Sender<Option<ModelTerminal>>>,
}
impl WorkLease {
    fn finish_model(mut self, terminal: ModelTerminal) {
        self.terminal = Some(terminal);
    }
}
impl Drop for WorkLease {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.state.inner.lock() {
            inner.jobs.remove(&self.id);
            inner.model_cancels.remove(&self.id);
            // The terminal status and drained lease become visible atomically.
            // Panic/abort cannot leave a status falsely claiming active work.
            if self.model_done.is_some() {
                let mut terminal = self.terminal.take().unwrap_or(ModelTerminal {
                    phase: ModelPhase::Failed, task_revision: None, stop_reason: "worker_stopped",
                    error: Some(ModelOperationError { code: "worker_stopped", message: "Model worker stopped unexpectedly. Inspect durable task state; never replay automatically.".into() }),
                    worker_drained: true, cleanup_unconfirmed: true, cancellation: None,
                });
                terminal.worker_drained = true;
                if let Some(operation) = inner.model_operations.get_mut(&self.id) {
                    operation.state = terminal.clone();
                }
                if let Some(done) = &self.model_done {
                    done.send_replace(Some(terminal));
                }
            }
        }
        self.state.changed.notify_waiters();
    }
}
impl AgentState {
    fn cancel_model(
        &self,
        task_id: &str,
        operation_id: Option<&str>,
        cancel_task: impl FnOnce() -> smith_agent_core::Result<()>,
    ) -> AResult<Vec<tokio::sync::watch::Receiver<Option<ModelTerminal>>>> {
        let inner = self
            .work
            .inner
            .lock()
            .map_err(|_| Error::Persistence("Model cancellation unavailable".into()))?;
        if operation_id.is_some_and(|operation| {
            !inner.model_cancels.contains_key(operation)
                || inner.jobs.get(operation).and_then(Option::as_deref) != Some(task_id)
        }) {
            return Err(Error::Conflict.into());
        }
        // Lock ordering: admission drops all DB guards before acquiring work;
        // model_terminal also releases its DB read before dropping the lease.
        // Keep work locked through this short durable CAS so an exact operation
        // cannot retire/be replaced, or lose its completion watch, in the gap.
        cancel_task()?;
        let mut waiters = Vec::new();
        for (id, task) in &inner.jobs {
            if task.as_deref() == Some(task_id) {
                if let Some(control) = inner.model_cancels.get(id) {
                    control.cancel.send_replace(true);
                    waiters.push(control.finished.clone());
                }
            }
        }
        Ok(waiters)
    }
    /// Trusted host-only installation of reviewed, explicitly bound adapters.
    pub fn register_adapter(
        &self,
        adapter: Arc<dyn HostToolAdapter>,
    ) -> smith_agent_core::Result<()> {
        let mut adapters = self
            .adapters
            .lock()
            .map_err(|_| Error::Persistence("adapter registry unavailable".into()))?;
        let mut registry = ToolRegistry::default();
        for current in adapters.iter() {
            registry = registry.with_adapter(current.clone())?;
        }
        registry.with_adapter(adapter.clone())?;
        adapters.push(adapter);
        Ok(())
    }
    pub(crate) fn adapters(&self) -> smith_agent_core::Result<Vec<Arc<dyn HostToolAdapter>>> {
        let mut adapters = self
            .adapters
            .lock()
            .map_err(|_| Error::Persistence("adapter registry unavailable".into()))?
            .clone();
        adapters.push(self.effects.clone());
        if let Some(platform) = self
            .platform
            .lock()
            .map_err(|_| Error::Persistence("platform state unavailable".into()))?
            .as_ref()
        {
            adapters.push(platform.adapter.clone());
        }
        Ok(adapters)
    }
    fn begin(&self, task: Option<String>) -> AResult<WorkLease> {
        let mut inner = self
            .work
            .inner
            .lock()
            .map_err(|_| AgentError(Error::Persistence("task admission unavailable".into())))?;
        if inner.closing {
            return Err(Error::Denied(
                "The profile is closing. Unlock it before starting new work.".into(),
            )
            .into());
        }
        if inner.jobs.len() >= 16 {
            return Err(Error::LimitExceeded("concurrent task operations".into()).into());
        }
        let id = crate::id();
        inner.jobs.insert(id.clone(), task);
        Ok(WorkLease {
            state: self.work.clone(),
            id,
            terminal: None,
            model_done: None,
        })
    }
    fn begin_model(
        &self,
        task_id: &str,
        mut operation: Option<ModelOperation>,
    ) -> AResult<(WorkLease, tokio::sync::watch::Receiver<bool>)> {
        let mut inner = self
            .work
            .inner
            .lock()
            .map_err(|_| Error::Persistence("Model admission unavailable".into()))?;
        if inner.closing {
            return Err(Error::Denied(
                "The profile is closing; new model work is unavailable.".into(),
            )
            .into());
        }
        if inner
            .model_cancels
            .keys()
            .any(|id| inner.jobs.get(id).and_then(Option::as_deref) == Some(task_id))
        {
            return Err(Error::Conflict.into());
        }
        if inner.jobs.len() >= 16 {
            return Err(Error::LimitExceeded("concurrent task operations".into()).into());
        }
        let id = crate::id();
        if let Some(operation) = &mut operation {
            let sequence = inner
                .model_sequence
                .checked_add(1)
                .ok_or_else(|| Error::LimitExceeded("model operation sequence".into()))?;
            if inner.model_operations.len() >= MAX_MODEL_OPERATIONS {
                let oldest = inner
                    .model_operations
                    .values()
                    .filter(|o| o.state.phase != ModelPhase::Running)
                    .min_by_key(|o| o.sequence)
                    .map(|o| o.id.clone())
                    .ok_or_else(|| Error::LimitExceeded("retained model operations".into()))?;
                inner.model_operations.remove(&oldest);
            }
            operation.id = id.clone();
            operation.sequence = sequence;
            inner.model_sequence = sequence;
            inner.model_operations.insert(id.clone(), operation.clone());
        }
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        inner.jobs.insert(id.clone(), Some(task_id.to_owned()));
        let (done, finished) = tokio::sync::watch::channel(None);
        inner
            .model_cancels
            .insert(id.clone(), ModelControl { cancel, finished });
        Ok((
            WorkLease {
                state: self.work.clone(),
                id,
                terminal: None,
                model_done: Some(done),
            },
            receiver,
        ))
    }
    async fn drain_model(
        &self,
        waiters: Vec<tokio::sync::watch::Receiver<Option<ModelTerminal>>>,
    ) -> AResult<Vec<ModelTerminal>> {
        let mut results = Vec::new();
        for mut waiter in waiters {
            loop {
                if let Some(result) = waiter.borrow_and_update().clone() {
                    results.push(result);
                    break;
                }
                waiter.changed().await.map_err(|_| {
                    Error::Persistence("Model drain result lost; inspect the durable task".into())
                })?;
            }
        }
        Ok(results)
    }
}

pub fn begin_shutdown(app: &App) {
    if let Ok(mut active) = app.agent.work.inner.lock() {
        active.closing = true;
        for cancel in active.model_cancels.values() {
            cancel.cancel.send_replace(true);
        }
    }
    app.agent.plugin_imports.clear();
}

/// Called only by the profile lifecycle after admission is revoked. Pending
/// approvals remain durable; operations currently owned by this process are
/// cancelled and joined before the keyed connection may be closed.
pub async fn shutdown(app: &Arc<App>) -> Result<(), String> {
    begin_shutdown(app);
    let tasks: BTreeSet<String> = app
        .agent
        .work
        .inner
        .lock()
        .map_err(|_| "Task shutdown state unavailable".to_owned())?
        .jobs
        .values()
        .flatten()
        .cloned()
        .collect();
    let store = AgentStore::new(app)
        .map_err(|_| "Encrypted task state unavailable during shutdown".to_owned())?;
    let engine = TaskEngine::new(store.clone(), ToolRegistry::default());
    let mut failed = false;
    for id in tasks {
        let mut resolved = false;
        for _ in 0..4 {
            match engine.get_owned(&id, store.owner()) {
                Ok(task) if !task.state.is_terminal() => match engine.cancel_owned_at(
                    &id,
                    task.revision,
                    store.owner(),
                    "Profile locked while task work was active",
                ) {
                    Ok(_) => {
                        resolved = true;
                        break;
                    }
                    Err(Error::Conflict) => continue,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                },
                Ok(_) | Err(Error::NotFound) => {
                    resolved = true;
                    break;
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        failed |= !resolved;
    }
    loop {
        let notified = app.agent.work.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if app
            .agent
            .work
            .inner
            .lock()
            .map_err(|_| "Task shutdown state unavailable".to_owned())?
            .jobs
            .is_empty()
        {
            break;
        }
        notified.await;
    }
    app.agent.effects.shutdown();
    if let Ok(platform) = app.agent.platform.lock() {
        if let Some(platform) = platform.as_ref() {
            platform.shutdown();
        }
    }
    if failed {
        Err(
            "Task cancellation could not be persisted; work has stopped and will require recovery."
                .into(),
        )
    } else {
        Ok(())
    }
}

/// The lifecycle calls this after holding the exclusive profile lock and
/// verifying the old worker stopped, before exposing the newly unlocked App.
pub fn recover(app: &Arc<App>) -> smith_agent_core::Result<()> {
    let store = AgentStore::new(app)?;
    store.recover_host_operations()?;
    let engine = TaskEngine::new(store.clone(), store.registry()?);
    for task in store.tasks()? {
        if matches!(
            task.state,
            TaskState::Running { in_flight: Some(_) } | TaskState::ModelThinking { .. }
        ) {
            engine.mark_interrupted_owned_at(&task.id, task.revision, store.owner())?;
        }
    }
    Ok(())
}

pub(crate) async fn work<T, F>(app: Arc<App>, task: Option<String>, operation: F) -> AResult<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<App>) -> AResult<T> + Send + 'static,
{
    let lease = app.agent.begin(task)?;
    let (send, receive) = oneshot::channel();
    let worker_app = app.clone();
    // Register the wrapper before spawning. Dropping an HTTP response future
    // never detaches an untracked blocking operation or drops its profile lease.
    app.spawn_task(async move {
        let result = tokio::task::spawn_blocking(move || {
            let _lease = lease;
            operation(worker_app)
        })
        .await
        .unwrap_or_else(|_| {
            Err(Error::Persistence(
                "Task worker stopped unexpectedly; inspect the durable state.".into(),
            )
            .into())
        });
        let _ = send.send(result);
    });
    receive.await.map_err(|_| {
        AgentError(Error::Persistence(
            "Task response interrupted; inspect the durable state.".into(),
        ))
    })?
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Permission {
    Read,
    List,
    Write,
    Search,
    Edit,
    Control,
    Conversations,
    ExportConversations,
    Artifacts,
    RecallMemory,
    SaveMemory,
    Charts,
    Status,
    ReadDocument,
    WriteDocument,
    EmbedText,
    GenerateImage,
    GitRead,
    GitWrite,
    WebFetch,
    SendWebhook,
    Shell,
    Computer,
    SystemMonitor,
    ModelInference,
}
impl Permission {
    pub(crate) fn capability(self) -> Capability {
        match self {
            Self::Read => Capability::ReadWorkspace,
            Self::List => Capability::ListWorkspace,
            Self::Write => Capability::WriteWorkspace,
            Self::Search => Capability::SearchWorkspace,
            Self::Edit => Capability::EditWorkspace,
            Self::Control => Capability::TaskControl,
            Self::Conversations => Capability::ReadConversations,
            Self::ExportConversations => Capability::ExportConversations,
            Self::Artifacts => Capability::SaveArtifact,
            Self::RecallMemory => Capability::ReadMemory,
            Self::SaveMemory => Capability::WriteMemory,
            Self::Charts => Capability::RenderChart,
            Self::Status => Capability::ReadStatus,
            Self::ReadDocument => Capability::ReadDocument,
            Self::WriteDocument => Capability::WriteDocument,
            Self::EmbedText => Capability::EmbedText,
            Self::GenerateImage => Capability::GenerateImage,
            Self::GitRead => Capability::GitRead,
            Self::GitWrite => Capability::GitWrite,
            Self::WebFetch => Capability::WebFetch,
            Self::SendWebhook => Capability::SendWebhook,
            Self::Shell => Capability::Shell,
            Self::Computer => Capability::ComputerUse,
            Self::SystemMonitor => Capability::SystemMonitor,
            Self::ModelInference => Capability::ModelInference,
        }
    }
}
fn permissions(policy: &WorkspacePolicy) -> Vec<Permission> {
    [
        Permission::Read,
        Permission::List,
        Permission::Write,
        Permission::Search,
        Permission::Edit,
        Permission::Control,
        Permission::Conversations,
        Permission::ExportConversations,
        Permission::Artifacts,
        Permission::RecallMemory,
        Permission::SaveMemory,
        Permission::Charts,
        Permission::Status,
        Permission::ReadDocument,
        Permission::WriteDocument,
        Permission::EmbedText,
        Permission::GenerateImage,
        Permission::GitRead,
        Permission::GitWrite,
        Permission::WebFetch,
        Permission::SendWebhook,
        Permission::Shell,
        Permission::Computer,
        Permission::SystemMonitor,
        Permission::ModelInference,
    ]
    .into_iter()
    .filter(|p| policy.capabilities.contains(&p.capability()))
    .collect()
}
pub(crate) fn workspace_view(policy: &WorkspacePolicy) -> Value {
    json!({"id":policy.id,"label":policy.label,"capabilities":permissions(policy),"resources":policy.resources,"policy_revision":policy.revision,"available":policy.status=="active"&&Workspace::reopen_temporary(&policy.identity).is_ok(),"output_boundary":"plaintext_private_temporary_files"})
}
fn task_view(task: &Task, policy: &WorkspacePolicy) -> Value {
    let mut effective = policy.clone();
    effective.capabilities = task.grant.capabilities.clone();
    json!({"id":task.id,"revision":task.revision,"workspace_id":policy.id,"input":task.input,"state":task.state,"plan":task.plan,"clarification_answers":task.clarification_answers,"last_result":task.last_result,"model_steps":task.model_steps,"model_bytes":task.model_bytes,"last_model_reply":task.last_model_reply,"last_model_rejection":task.last_model_rejection,"agent_persona":task.agent_persona,"extensions":task.extensions,"capabilities":permissions(&effective),"resources":task.grant.resources})
}
pub(crate) fn task_response(
    store: &AgentStore,
    id: &str,
    outcome: Option<ToolOutcome>,
) -> AResult<Json<Value>> {
    let task = store.load(id)?;
    let policy = store.policy_for_task(id)?;
    let mut value = json!({"task":task_view(&task,&policy),"workspace":workspace_view(&policy)});
    if let Some(outcome) = outcome {
        value["outcome"] = serde_json::to_value(outcome)
            .map_err(|_| AgentError(Error::Persistence("Response encoding failed".into())))?;
    }
    Ok(Json(value))
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .merge(effects::routes())
        .merge(platform::routes())
        .route("/api/agent/workspaces", get(workspaces).post(provision))
        .route("/api/agent/workspaces/{id}/grants", post(revise_grants))
        .route("/api/agent/resources", get(resources))
        .route("/api/agent/tools", get(tools))
        .route("/api/agent/tasks", get(tasks).post(create))
        .route("/api/agent/tasks/{id}", get(status))
        .route("/api/agent/tasks/{id}/start", post(start))
        .route("/api/agent/tasks/{id}/actions", post(action))
        .route("/api/agent/tasks/{id}/approval", post(approval))
        .route("/api/agent/tasks/{id}/cancel", post(cancel))
        .route("/api/agent/tasks/{id}/resume", post(resume))
        .route("/api/agent/tasks/{id}/trace", get(trace))
        .route("/api/agent/model-selection", post(model_selection))
        .route("/api/agent/tasks/{id}/model-run", post(model_run))
        .route(
            "/api/agent/tasks/{id}/model-runs",
            post(model_run_async).get(model_operations),
        )
        .route(
            "/api/agent/tasks/{id}/model-runs/{operation}",
            get(model_operation),
        )
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
}
async fn resources(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    Ok(Json(AgentStore::new(&app)?.selectable_resources()?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviseGrants {
    expected_policy_revision: u64,
    capabilities: Vec<Permission>,
    resources: BTreeSet<ResourceScope>,
    confirmed: bool,
}
async fn revise_grants(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<ReviseGrants>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    if !input.confirmed {
        return Err(
            Error::Denied("Confirm exact grant changes before creating a task".into()).into(),
        );
    }
    let capabilities: BTreeSet<_> = input.capabilities.iter().map(|p| p.capability()).collect();
    if capabilities.len() != input.capabilities.len() {
        return Err(Error::InvalidInput("Duplicate capability".into()).into());
    }
    let policy = AgentStore::new(&app)?.revise_policy(
        &id,
        input.expected_policy_revision,
        capabilities,
        input.resources,
    )?;
    Ok(Json(json!({"workspace":workspace_view(&policy)})))
}

async fn workspaces(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    let store = AgentStore::new(&app)?;
    Ok(Json(
        json!({"workspaces":store.policies()?.iter().map(workspace_view).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provision {
    label: String,
    capabilities: Vec<Permission>,
    confirmed: bool,
    #[serde(default)]
    resources: BTreeSet<ResourceScope>,
}
async fn provision(State(app): State<Arc<App>>, input: Body<Provision>) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied(
            "Confirm creation and the selected temporary-workspace permissions.".into(),
        )
        .into());
    }
    let capabilities: BTreeSet<_> = input.capabilities.iter().map(|p| p.capability()).collect();
    if capabilities.len() != input.capabilities.len() {
        return Err(Error::InvalidInput("Duplicate workspace permission.".into()).into());
    }
    work(app, None, move |app| {
        let policy =
            AgentStore::new(&app)?.provision_scoped(input.label, capabilities, input.resources)?;
        Ok(Json(json!({"workspace":workspace_view(&policy)})))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsQuery {
    workspace_id: String,
}
async fn tools(
    State(app): State<Arc<App>>,
    Query(query): Query<ToolsQuery>,
) -> AResult<Json<Value>> {
    uuid(&query.workspace_id)?;
    let store = AgentStore::new(&app)?;
    let policy = store.policy(&query.workspace_id)?;
    let descriptors = store
        .registry()?
        .all_tools()
        .into_iter()
        .map(|tool| {
            let granted = tool.availability == Availability::Implemented
                && tool
                    .required_capability
                    .is_none_or(|c| policy.capabilities.contains(&c));
            let mut value = serde_json::to_value(tool).expect("tool descriptor serializes");
            value["granted"] = json!(granted);
            value
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"tools":descriptors})))
}
async fn tasks(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    let store = AgentStore::new(&app)?;
    let rows = store
        .tasks()?
        .iter()
        .map(|task| Ok(task_view(task, &store.policy_for_task(&task.id)?)))
        .collect::<smith_agent_core::Result<Vec<_>>>()?;
    Ok(Json(json!({"tasks":rows})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    workspace_id: String,
    policy_revision: u64,
    input: String,
    #[serde(default)]
    extensions: smith_agent_core::InstructionSelection,
}
async fn create(State(app): State<Arc<App>>, input: Body<Create>) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&input.workspace_id)?;
    work(app, None, move |app| {
        let store = AgentStore::new(&app)?;
        let policy = store.policy(&input.workspace_id)?;
        if policy.revision != input.policy_revision {
            return Err(Error::Conflict.into());
        }
        let engine = TaskEngine::new(store.clone(), store.registry()?);
        let extensions = store.with_db(|db| {
            crate::agent_extensions::capture(db, store.owner().id(), &input.extensions)
        })?;
        let task =
            engine.create_with_context(&store.grant(&policy)?, input.input, None, extensions)?;
        task_response(&store, &task.id, None)
    })
    .await
}
async fn status(State(app): State<Arc<App>>, Path(id): Path<String>) -> AResult<Json<Value>> {
    uuid(&id)?;
    task_response(&AgentStore::new(&app)?, &id, None)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    expected_revision: u64,
}
async fn start(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Revision>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    work(app, Some(id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let grant = store.grant_for_task(&id)?;
        TaskEngine::new(store.clone(), store.registry()?).start_at(
            &id,
            input.expected_revision,
            &grant,
        )?;
        task_response(&store, &id, None)
    })
    .await
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Tool {
        name: String,
        arguments: Value,
    },
    Read {
        path: String,
    },
    List,
    Search {
        query: String,
        max_results: Option<usize>,
        max_bytes: Option<usize>,
    },
    Write {
        path: String,
        contents: String,
    },
    Edit {
        path: String,
        old_string: String,
        new_string: String,
        expected_sha256: String,
    },
    Plan {
        steps: Vec<String>,
    },
    Clarify {
        question: String,
    },
    Complete {
        summary: String,
    },
}
impl Action {
    fn call(self, id: String) -> ToolCall {
        let (name, arguments) = match self {
            Self::Tool { name, arguments } => {
                return ToolCall {
                    id,
                    name,
                    arguments,
                }
            }
            Self::Read { path } => ("read_file", json!({"path":path})),
            Self::List => ("list_directory", json!({})),
            Self::Search {
                query,
                max_results,
                max_bytes,
            } => (
                "search",
                json!({"query":query,"max_results":max_results,"max_bytes":max_bytes}),
            ),
            Self::Write { path, contents } => {
                ("write_file", json!({"path":path,"contents":contents}))
            }
            Self::Edit {
                path,
                old_string,
                new_string,
                expected_sha256,
            } => (
                "edit_file",
                json!({"path":path,"old_string":old_string,"new_string":new_string,"expected_sha256":expected_sha256}),
            ),
            Self::Plan { steps } => ("update_plan", json!({"steps":steps})),
            Self::Clarify { question } => ("ask_clarification", json!({"question":question})),
            Self::Complete { summary } => ("task_complete", json!({"summary":summary})),
        };
        ToolCall {
            id,
            name: name.into(),
            arguments,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    expected_revision: u64,
    request_id: String,
    action: Action,
}
async fn action(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<ActionRequest>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    uuid(&input.request_id)?;
    work(app, Some(id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let grant = store.grant_for_task(&id)?;
        let outcome = TaskEngine::new(store.clone(), store.registry()?).call_at(
            &id,
            input.expected_revision,
            &grant,
            input.action.call(input.request_id),
        )?;
        task_response(&store, &id, Some(outcome))
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    Approve,
    Deny,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    expected_revision: u64,
    call_id: String,
    decision: Decision,
}
async fn approval(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Approval>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    uuid(&input.call_id)?;
    work(app, Some(id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let engine = TaskEngine::new(store.clone(), store.registry()?);
        let task = engine.get_owned(&id, store.owner())?;
        if task.revision != input.expected_revision
            || !matches!(&task.state,TaskState::NeedsApproval{call} if call.id==input.call_id)
        {
            return Err(Error::Conflict.into());
        }
        let decision = match input.decision {
            Decision::Approve => ApprovalDecision::Approve,
            Decision::Deny => ApprovalDecision::Deny,
        };
        let outcome = engine.decide_approval(
            &id,
            input.expected_revision,
            &store.grant_for_task(&id)?,
            decision,
        )?;
        task_response(&store, &id, Some(outcome))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    expected_revision: u64,
    reason: String,
    #[serde(default)]
    model_operation_id: Option<String>,
}
async fn cancel(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Cancel>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    if let Some(operation) = &input.model_operation_id {
        uuid(operation)?;
    }
    // The cancellation CAS has no filesystem effect and does not require its
    // workspace to still exist; model cleanup is awaited after the CAS.
    let store = AgentStore::new(&app)?;
    let engine = TaskEngine::new(store.clone(), store.registry()?);
    let waiters = app
        .agent
        .cancel_model(&id, input.model_operation_id.as_deref(), || {
            engine
                .cancel_owned_at(&id, input.expected_revision, store.owner(), input.reason)
                .map(|_| ())
        })?;
    // Cancellation may outlive the requesting client. The model worker owns
    // its tracked lease independently. Typed evidence distinguishes confirmed
    // owned cleanup from an external server that may continue processing.
    let drained = app.agent.drain_model(waiters).await?;
    if drained
        .iter()
        .any(|result| result.cleanup_unconfirmed || result.phase == ModelPhase::Failed)
    {
        return Err(Error::UncertainSideEffect("Task cancelled but cleanup or storage failed. Inspect the operation and runtime status.".into()).into());
    }
    let Json(mut response) = task_response(&store, &id, None)?;
    response["model_cancellation"] = json!({"worker_drained":true,"evidence":drained.iter().filter_map(|result| result.cancellation).collect::<Vec<_>>()});
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Resume {
    Clarification {
        expected_revision: u64,
        answer: String,
    },
    ContinueWithoutReplay {
        expected_revision: u64,
    },
    RetryUncertainCall {
        expected_revision: u64,
    },
}
async fn resume(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<Resume>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    uuid(&id)?;
    work(app, Some(id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let engine = TaskEngine::new(store.clone(), store.registry()?);
        let outcome = match input {
            Resume::ContinueWithoutReplay { expected_revision } => {
                engine.continue_owned_at(&id, expected_revision, store.owner())?;
                Some(ToolOutcome::Returned(
                    json!({"resumed":true,"replayed":false}),
                ))
            }
            Resume::Clarification {
                expected_revision,
                answer,
            } => {
                engine.resume_clarification(
                    &id,
                    expected_revision,
                    &store.grant_for_task(&id)?,
                    answer,
                )?;
                None
            }
            Resume::RetryUncertainCall { expected_revision } => Some(engine.resume_interrupted(
                &id,
                expected_revision,
                &store.grant_for_task(&id)?,
                RecoveryDecision::RetryUncertainCall,
            )?),
        };
        task_response(&store, &id, outcome)
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceQuery {
    after: Option<u64>,
    limit: Option<usize>,
}
async fn trace(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(query): Query<TraceQuery>,
) -> AResult<Json<Value>> {
    uuid(&id)?;
    let store = AgentStore::new(&app)?;
    let task = store.load(&id)?;
    let limit = query.limit.unwrap_or(50);
    let events = store.events_page(&id, query.after, limit)?;
    let next = if events.len() == limit {
        events.last().map(|e| e.revision)
    } else {
        None
    };
    Ok(Json(
        json!({"task_id":id,"revision":task.revision,"events":events,"next_after":next}),
    ))
}

#[cfg(test)]
#[path = "../tests/support/agent_model_async.rs"]
mod model_async_tests;
#[cfg(test)]
#[path = "../tests/support/agent_endpoints.rs"]
mod tests;

async fn model_selection(
    State(app): State<Arc<App>>,
    input: Body<crate::Expected>,
) -> AResult<Json<Value>> {
    let selection = crate::task_model::selection(&app, &body(input)?).map_err(model_error)?;
    Ok(Json(
        json!({"scope_id":selection.scope_id,"binding_id":selection.binding_id,"provenance":selection.provenance}),
    ))
}
pub(crate) fn model_error(error: crate::ApiError) -> AgentError {
    let message = error.1.chars().take(4096).collect::<String>();
    match error.0 {
        StatusCode::CONFLICT => Error::Conflict.into(),
        StatusCode::FORBIDDEN => Error::Denied(message).into(),
        _ => Error::InvalidInput(message).into(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRun {
    expected_revision: u64,
    expected: crate::Expected,
    max_steps: usize,
    confirmed: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AsyncModelRun {
    expected_revision: u64,
    expected: crate::Expected,
    max_steps: usize,
    confirmed: bool,
    binding_id: String,
}
fn model_admission(
    app: &Arc<App>,
    id: &str,
    input: &ModelRun,
    binding: Option<&str>,
) -> AResult<crate::task_model::ModelSelection> {
    uuid(id)?;
    if !input.confirmed || !(1..=8).contains(&input.max_steps) {
        return Err(Error::InvalidInput("Confirm a model run of 1–8 steps".into()).into());
    }
    let selection = crate::task_model::selection(app, &input.expected).map_err(model_error)?;
    let store = AgentStore::new(app)?;
    let grant = store.grant_for_task(id)?;
    if !grant.permits(Capability::ModelInference)
        || !grant.permits_resource(
            smith_agent_core::ResourceKind::ProviderBinding,
            &selection.scope_id,
        )
    {
        return Err(Error::Denied(
            "Select this exact model target and model_inference when provisioning the task".into(),
        )
        .into());
    }
    let task = store.load(id)?;
    if let Some(target) = task
        .agent_persona
        .as_ref()
        .and_then(|p| p.model_target.as_ref())
    {
        if target.scope_id != selection.scope_id {
            return Err(Error::Denied("This task is bound to its captured saved-agent model target. Select that target before running it.".into()).into());
        }
    }
    if task.revision != input.expected_revision {
        return Err(Error::Conflict.into());
    }
    if !matches!(
        task.state,
        TaskState::Pending | TaskState::Running { in_flight: None }
    ) {
        return Err(Error::InvalidTransition(
            "Resolve pending approval, clarification or interruption before running the model"
                .into(),
        )
        .into());
    }

    if binding.is_some_and(|binding| binding != selection.binding_id) {
        return Err(Error::Conflict.into());
    }
    Ok(selection)
}
async fn model_run(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<ModelRun>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    let selection = model_admission(&app, &id, &input, None)?;
    let (lease, receiver) = app.agent.begin_model(&id, None)?;
    let (send, receive) = oneshot::channel();
    let worker_app = app.clone();
    app.spawn_task(async move {
        let result = guided_model_run(
            worker_app.clone(),
            &id,
            input.expected_revision,
            input.max_steps,
            selection,
            receiver,
        )
        .await;
        let terminal = model_terminal(&worker_app, &id, &result);
        lease.finish_model(terminal);
        let _ = send.send(result.map_err(|failure| failure.error));
    });
    receive.await.map_err(|_| {
        AgentError(Error::Persistence(
            "Model worker stopped; inspect durable task state".into(),
        ))
    })?
}
/// Immediate admission only. This ephemeral receipt is never an execution
/// grant, and loss/eviction of it never permits retrying the submitted revision.
async fn model_run_async(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    input: Body<AsyncModelRun>,
) -> AResult<(StatusCode, Json<Value>)> {
    let input = body(input)?;
    let binding = input.binding_id;
    let input = ModelRun {
        expected_revision: input.expected_revision,
        expected: input.expected,
        max_steps: input.max_steps,
        confirmed: input.confirmed,
    };
    let selection = model_admission(&app, &id, &input, Some(&binding))?;
    let operation = ModelOperation {
        id: String::new(),
        task_id: id.clone(),
        submitted_revision: input.expected_revision,
        max_steps: input.max_steps,
        scope_id: selection.scope_id.clone(),
        binding_id: selection.binding_id.clone(),
        sequence: 0,
        state: ModelTerminal {
            phase: ModelPhase::Running,
            task_revision: None,
            stop_reason: "running",
            error: None,
            worker_drained: false,
            cleanup_unconfirmed: false,
            cancellation: None,
        },
    };
    let (lease, receiver) = app.agent.begin_model(&id, Some(operation))?;
    let admitted = {
        let inner = app
            .agent
            .work
            .inner
            .lock()
            .map_err(|_| Error::Persistence("Model admission unavailable".into()))?;
        inner
            .model_operations
            .get(&lease.id)
            .cloned()
            .ok_or(Error::NotFound)?
    };
    let worker_app = app.clone();
    // No await between reservation and registration. The worker survives a
    // dropped HTTP response and is drained by profile shutdown independently.
    app.spawn_task(async move {
        let result = guided_model_run(
            worker_app.clone(),
            &id,
            input.expected_revision,
            input.max_steps,
            selection,
            receiver,
        )
        .await;
        let terminal = model_terminal(&worker_app, &id, &result);
        lease.finish_model(terminal);
    });
    Ok((StatusCode::ACCEPTED, Json(json!({"operation":admitted}))))
}
struct GuidedFailure {
    error: AgentError,
    cancellation: Option<crate::task_model::CancellationEvidence>,
    cleanup_unconfirmed: bool,
}
impl From<Error> for GuidedFailure {
    fn from(error: Error) -> Self {
        AgentError(error).into()
    }
}
impl From<AgentError> for GuidedFailure {
    fn from(error: AgentError) -> Self {
        Self {
            error,
            cancellation: None,
            cleanup_unconfirmed: false,
        }
    }
}
impl From<crate::task_model::CompletionFailure> for GuidedFailure {
    fn from(failure: crate::task_model::CompletionFailure) -> Self {
        let cancellation = match &failure {
            crate::task_model::CompletionFailure::Cancelled(evidence) => Some(*evidence),
            _ => None,
        };
        let cleanup_unconfirmed =
            matches!(&failure, crate::task_model::CompletionFailure::Cleanup(_));
        Self {
            error: model_error(failure.into_api()),
            cancellation,
            cleanup_unconfirmed,
        }
    }
}
fn model_terminal(
    app: &Arc<App>,
    id: &str,
    result: &Result<Json<Value>, GuidedFailure>,
) -> ModelTerminal {
    let task = AgentStore::new(app).and_then(|store| store.load(id));
    let revision = task.as_ref().ok().map(|task| task.revision);
    let cancelled = task
        .as_ref()
        .is_ok_and(|task| matches!(task.state, TaskState::Cancelled { .. }));
    let mut terminal = ModelTerminal {
        phase: ModelPhase::Failed,
        task_revision: revision,
        stop_reason: "failed",
        error: None,
        worker_drained: true,
        cleanup_unconfirmed: false,
        cancellation: None,
    };
    match result {
        Err(failure)
            if cancelled && failure.cancellation.is_some() && !failure.cleanup_unconfirmed =>
        {
            terminal.phase = ModelPhase::Cancelled;
            terminal.stop_reason = "cancelled";
            terminal.cancellation = failure.cancellation;
            return terminal;
        }
        Err(failure) => {
            let (_, code, message) = public_error(&failure.error.0);
            terminal.cleanup_unconfirmed = failure.cleanup_unconfirmed;
            terminal.error = Some(ModelOperationError {
                code: if failure.cleanup_unconfirmed {
                    "cleanup_unconfirmed"
                } else {
                    code
                },
                message: message.chars().take(2048).collect(),
            });
            return terminal;
        }
        Ok(_) if cancelled => {
            terminal.phase = ModelPhase::Cancelled;
            terminal.stop_reason = "cancelled";
            terminal.cancellation =
                Some(crate::task_model::CancellationEvidence::NoActiveGeneration);
            return terminal;
        }
        Ok(Json(value)) if value["outcome"]["kind"] == "failed" => {
            terminal.error = Some(ModelOperationError {
                code: "tool_failed",
                message:
                    "A task tool failed. Inspect its durable result and trace before continuing."
                        .into(),
            });
            return terminal;
        }
        _ => {}
    }
    let reason = match task.map(|task| task.state) {
        Ok(TaskState::NeedsApproval { .. }) => "needs_approval",
        Ok(TaskState::NeedsClarification { .. }) => "needs_clarification",
        Ok(TaskState::Completed { .. }) => "completed",
        Ok(TaskState::Running { in_flight: None }) => "iteration_limit",
        _ => {
            terminal.stop_reason = "inspect_task";
            terminal.error = Some(ModelOperationError { code: "uncertain_state", message: "The model worker drained but its final task state requires inspection. Do not replay automatically.".into() });
            return terminal;
        }
    };
    terminal.phase = ModelPhase::Succeeded;
    terminal.stop_reason = reason;
    terminal
}

async fn model_operations(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> AResult<Json<Value>> {
    uuid(&id)?;
    AgentStore::new(&app)?.load(&id)?;
    let inner = app
        .agent
        .work
        .inner
        .lock()
        .map_err(|_| Error::Persistence("Model status unavailable".into()))?;
    let mut operations = inner
        .model_operations
        .values()
        .filter(|operation| operation.task_id == id)
        .cloned()
        .collect::<Vec<_>>();
    operations.sort_by_key(|operation| operation.sequence);
    Ok(Json(
        json!({"task_id":id,"operations":operations,"retention":"current_profile_session_latest_128","missing_requires_task_inspection":true}),
    ))
}
async fn model_operation(
    State(app): State<Arc<App>>,
    Path((id, operation)): Path<(String, String)>,
) -> AResult<Json<Value>> {
    uuid(&id)?;
    uuid(&operation)?;
    AgentStore::new(&app)?.load(&id)?;
    let inner = app
        .agent
        .work
        .inner
        .lock()
        .map_err(|_| Error::Persistence("Model status unavailable".into()))?;
    let operation = inner
        .model_operations
        .get(&operation)
        .filter(|operation| operation.task_id == id)
        .ok_or(Error::NotFound)?;
    Ok(Json(json!({"operation":operation})))
}
async fn guided_model_run(
    app: Arc<App>,
    id: &str,
    mut revision: u64,
    max_steps: usize,
    selection: crate::task_model::ModelSelection,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Json<Value>, GuidedFailure> {
    let store = AgentStore::new(&app)?;
    let grant = store.grant_for_task(id)?;
    let engine = TaskEngine::new(store.clone(), store.registry()?);
    if *cancel.borrow() {
        return Err(crate::task_model::CompletionFailure::Cancelled(
            crate::task_model::CancellationEvidence::NoActiveGeneration,
        )
        .into());
    }
    if engine.get_owned(id, store.owner())?.state == TaskState::Pending {
        revision = engine.start_at(id, revision, &grant)?.revision;
    }
    for _ in 0..max_steps {
        if *cancel.borrow() {
            return Err(crate::task_model::CompletionFailure::Cancelled(
                crate::task_model::CancellationEvidence::NoActiveGeneration,
            )
            .into());
        }
        let task = engine.get_owned(id, store.owner())?;
        if task.revision != revision {
            return Err(Error::Conflict.into());
        }
        let context = model_context(&store, &task)?;
        let tools = engine
            .registry()
            .available_tools(&grant)
            .iter()
            .map(|tool| json!({"name":tool.name,"parameters":tool.parameters,"requires_approval":tool.requires_approval}))
            .collect::<Vec<_>>();
        if json!({"context":context,"tools":tools}).to_string().len() > 64 * 1024 {
            return Err(Error::LimitExceeded(
                "Combined model context and tool descriptions".into(),
            )
            .into());
        }
        let reserved = engine.begin_model_at(
            id,
            revision,
            &grant,
            &selection.scope_id,
            &selection.binding_id,
            context.clone(),
            selection.provenance.clone(),
        )?;
        let TaskState::ModelThinking { step_id, .. } = &reserved.state else {
            unreachable!()
        };
        let reply = match crate::task_model::complete(
            app.clone(),
            selection.clone(),
            context,
            tools,
            cancel.clone(),
        )
        .await
        {
            Ok(reply) => reply,
            Err(error) => {
                let rejection = match &error {
                    crate::task_model::CompletionFailure::Rejected(reply) => Some(reply.clone()),
                    _ => None,
                };
                let mut failure = GuidedFailure::from(error);
                let recovery = match store.load(id) {
                    Ok(current) if current.revision == reserved.revision => match rejection {
                        Some(reply) => engine.reject_model_at(id, reserved.revision, store.owner(), step_id, reply),
                        None => engine.abort_model_at(id,reserved.revision,store.owner(),"Selected model step failed or was cancelled; no proposed tool was dispatched"),
                    }.map(|_| ()),
                    Ok(_) => Ok(()), Err(error) => Err(error),
                };
                if let Err(error) = recovery {
                    failure.error = error.into();
                    failure.cancellation = None;
                }
                return Err(failure);
            }
        };
        let call = reply
            .tool_call
            .map(|call| ToolCall::new(call.name, call.arguments));
        let diagnostic_provenance = reply.provenance.clone();
        let finished = match engine.finish_model_at(
            id,
            reserved.revision,
            &grant,
            step_id,
            smith_agent_core::ModelReply {
                content: reply.content,
                tool_call: call.clone(),
                provenance: reply.provenance,
            },
        ) {
            Ok(task) => task,
            Err(error) => {
                // A late response must not overwrite cancellation or another
                // transition. A failed diagnostic commit remains a storage
                // failure; never claim its evidence was durably recorded.
                let current = store.load(id)?;
                if current.revision == reserved.revision {
                    let rejection = smith_agent_core::RejectedModelReply::capture(
                        smith_agent_core::ModelRejectionReason::InvalidTaskReply,
                        &reply.raw_text,
                        diagnostic_provenance,
                    )?;
                    engine.reject_model_at(
                        id,
                        reserved.revision,
                        store.owner(),
                        step_id,
                        rejection,
                    )?;
                }
                return Err(error.into());
            }
        };
        revision = finished.revision;
        let Some(call) = call else {
            return task_response(&store, id, Some(ToolOutcome::Completed)).map_err(Into::into);
        };
        let task_id = id.to_owned();
        let Json(value) = work(app.clone(), Some(task_id.clone()), move |app| {
            let store = AgentStore::new(&app)?;
            let grant = store.grant_for_task(&task_id)?;
            let outcome = TaskEngine::new(store.clone(), store.registry()?)
                .call_at(&task_id, revision, &grant, call)?;
            task_response(&store, &task_id, Some(outcome))
        })
        .await?;
        let current = store.load(id)?;
        revision = current.revision;
        if current.state != (TaskState::Running { in_flight: None })
            || value["outcome"]["kind"] == "failed"
        {
            return Ok(Json(value));
        }
    }
    let Json(mut response) = task_response(&store, id, None)?;
    response["model_run"] =
        json!({"stopped":"iteration_limit","max_steps":max_steps,"requires_explicit_resume":true});
    Ok(Json(response))
}
fn model_context(store: &AgentStore, task: &Task) -> smith_agent_core::Result<Value> {
    fn bounded(value: Value) -> Value {
        let text = value.to_string();
        if text.len() <= 8192 {
            value
        } else {
            json!({"truncated":true,"json_prefix":text.chars().take(2048).collect::<String>()})
        }
    }
    let mut events = Vec::new();
    let after = Some(task.revision.saturating_sub(12));
    for record in store.events_page(&task.id, after, 12)? {
        if matches!(
            record.event,
            smith_agent_core::TraceEvent::ModelStarted { .. }
                | smith_agent_core::TraceEvent::ModelRejected { .. }
        ) {
            continue;
        }
        events.push(bounded(serde_json::to_value(record).map_err(|_| {
            Error::InvalidInput("Model context encoding".into())
        })?));
    }
    let mut value = json!({"task_id":task.id,"input":bounded(json!(task.input)),"agent_persona":task.agent_persona,"extensions":task.extensions,"plan":bounded(json!(task.plan)),"clarification_answers":bounded(json!(task.clarification_answers)),"last_model_reply":bounded(json!(task.last_model_reply)),"last_tool_result":bounded(json!(task.last_result)),"recent_events":events,"data_is_untrusted":true});
    while value.to_string().len() > 40 * 1024 {
        if value["recent_events"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            value["recent_events"].as_array_mut().unwrap().remove(0);
        } else {
            return Err(Error::LimitExceeded("Model context".into()));
        }
    }
    Ok(value)
}

pub(crate) fn ensure_open(app: &App) -> smith_agent_core::Result<()> {
    if app
        .agent
        .work
        .inner
        .lock()
        .map_err(|_| Error::Persistence("Task admission unavailable".into()))?
        .closing
    {
        Err(Error::Denied("Profile is closing".into()))
    } else {
        Ok(())
    }
}
