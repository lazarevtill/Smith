//! Native Agents/Activity API bridge. The UI captures reviews from displayed
//! snapshots before queueing them on its actor; updates still require the UI's
//! NavigationEpoch check. Review types are not deserializable execution grants.
//! Every API mutation uses the captured session, policy/task revision and call
//! identity, with no automatic refresh/retry. Responses are discarded if the
//! profile session changes. Caller-owned task views must also be cleared when
//! Bridge::revoke runs, and must not be rendered after an old navigation epoch.
//!
//! Creating a task and `task_start` do not invoke a model. `task_local_model`
//! validates a displayed local target and returns an exact resource review;
//! model runs use immediate asynchronous admission and one bounded status call
//! per polling command; never block the actor awaiting inference. Likewise process,
//! network, media-provider and OS-interaction effects require their own exact
//! native review screens and are not dispatched/approved through this module.

use crate::bridge::{encode_component, Bridge, Config, Result, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, net::SocketAddr};

const MAX_INPUT: usize = 16 * 1024;
const MAX_ARGUMENTS: usize = 128 * 1024;
const MAX_ROWS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
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
    fn needs_separate_setup(self) -> bool {
        matches!(
            self,
            Self::EmbedText
                | Self::GenerateImage
                | Self::GitRead
                | Self::GitWrite
                | Self::WebFetch
                | Self::SendWebhook
                | Self::Shell
                | Self::Computer
                | Self::SystemMonitor
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Conversation,
    MemoryNamespace,
    Document,
    ArtifactCollection,
    ProviderBinding,
    EffectBinding,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceScope {
    pub kind: ResourceKind,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub label: String,
    pub capabilities: Vec<Permission>,
    pub resources: Vec<ResourceScope>,
    pub policy_revision: u64,
    pub available: bool,
    pub output_boundary: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Running {
        in_flight: Option<ToolCall>,
    },
    ModelThinking {
        step_id: String,
        scope_id: String,
        binding_id: String,
    },
    NeedsApproval {
        call: ToolCall,
    },
    NeedsClarification {
        question: String,
    },
    Interrupted {
        uncertain_call: Option<ToolCall>,
    },
    Completed {
        summary: String,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: String,
    },
}
impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelTarget {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub scope_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPersona {
    pub definition_id: String,
    pub definition_revision: u64,
    pub name: String,
    pub system_prompt: String,
    pub soul: String,
    pub model_target: Option<AgentModelTarget>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub revision: u64,
    pub workspace_id: String,
    pub input: String,
    pub state: TaskState,
    #[serde(default)]
    pub plan: Vec<String>,
    #[serde(default)]
    pub clarification_answers: Vec<String>,
    pub last_result: Option<Value>,
    #[serde(default)]
    pub model_steps: u32,
    #[serde(default)]
    pub model_bytes: u64,
    pub last_model_reply: Option<Value>,
    /// Absent only on older backend DTOs. Explicit empty grants stay empty.
    #[serde(default)]
    pub capabilities: Option<Vec<Permission>>,
    #[serde(default)]
    pub resources: Option<Vec<ResourceScope>>,
    #[serde(default)]
    pub agent_persona: Option<AgentPersona>,
    /// Immutable captured instruction provenance; display only, never current grants.
    #[serde(default)]
    pub extensions: Option<Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task: TaskRecord,
    pub workspace: WorkspaceSnapshot,
    pub outcome: Option<Value>,
}
impl TaskSnapshot {
    pub fn capabilities(&self) -> &[Permission] {
        self.task
            .capabilities
            .as_deref()
            .unwrap_or(&self.workspace.capabilities)
    }
    pub fn resources(&self) -> &[ResourceScope] {
        self.task
            .resources
            .as_deref()
            .unwrap_or(&self.workspace.resources)
    }
    /// An accepted API response can describe a failed effect. Display its
    /// outcome instead of assuming that a returned snapshot means success.
    pub fn operation_failed(&self) -> bool {
        self.outcome.as_ref().is_some_and(|v| v["kind"] == "failed")
    }
}

/// Fresh action identity from the same OS source used by owned Unix launches.
/// This is not a grant or secret; retain it across an uncertain response.
pub fn new_task_request_id() -> Result<String> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|_| "OS randomness is unavailable for a fresh task request.")?;
        bytes[6] = (bytes[6] & 15) | 64;
        bytes[8] = (bytes[8] & 63) | 128;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
    }
    #[cfg(not(unix))]
    {
        Err("Task request identity generation is unqualified on this native platform.".into())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub task_id: String,
    pub revision: u64,
    pub event: Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePage {
    pub task_id: String,
    pub revision: u64,
    pub events: Vec<TraceRecord>,
    pub next_after: Option<u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskOverview {
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub tasks: Vec<TaskRecord>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRunPhase {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCancellationEvidence {
    NoActiveGeneration,
    OwnedGenerationDrained,
    ExternalMayContinue,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRunError {
    pub code: String,
    pub message: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationSnapshot {
    pub id: String,
    pub task_id: String,
    pub submitted_revision: u64,
    pub max_steps: usize,
    pub scope_id: String,
    pub binding_id: String,
    pub sequence: u64,
    pub phase: ModelRunPhase,
    pub task_revision: Option<u64>,
    pub stop_reason: String,
    pub error: Option<ModelRunError>,
    pub worker_drained: bool,
    pub cleanup_unconfirmed: bool,
    pub cancellation: Option<ModelCancellationEvidence>,
}
impl ModelOperationSnapshot {
    pub fn is_terminal(&self) -> bool {
        self.phase != ModelRunPhase::Running
    }
}
#[derive(Clone)]
pub struct ModelRunReview {
    task: TaskReview,
    model: LocalModelReview,
    max_steps: usize,
}
impl ModelRunReview {
    pub fn task(&self) -> &TaskReview {
        &self.task
    }
    pub fn model(&self) -> &LocalModelReview {
        &self.model
    }
    pub fn max_steps(&self) -> usize {
        self.max_steps
    }
}
/// Local recovery evidence for one possibly submitted request. Never a grant;
/// the captured session and sequence boundary cannot be supplied by JSON.
#[derive(Clone)]
pub struct ModelRunRecovery {
    review: ModelRunReview,
    after_sequence: u64,
}
impl ModelRunRecovery {
    pub fn review(&self) -> &ModelRunReview {
        &self.review
    }
    pub fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
    fn matches(&self, operation: &ModelOperationSnapshot) -> bool {
        operation.sequence > self.after_sequence
            && operation.task_id == self.review.task.snapshot.task.id
            && operation.submitted_revision == self.review.task.snapshot.task.revision
            && operation.max_steps == self.review.max_steps
            && operation.scope_id == self.review.model.scope.id
            && operation.binding_id == self.review.model.binding_id
    }
}
#[derive(Clone)]
pub enum ModelSubmitFailure {
    NotSubmitted(String),
    OutcomeUnknown {
        message: String,
        recovery: Box<ModelRunRecovery>,
    },
}
impl std::fmt::Display for ModelSubmitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSubmitted(message) | Self::OutcomeUnknown { message, .. } => {
                f.write_str(message)
            }
        }
    }
}
impl std::fmt::Debug for ModelSubmitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSubmitted(message) => f.debug_tuple("NotSubmitted").field(message).finish(),
            Self::OutcomeUnknown { message, recovery } => f
                .debug_struct("OutcomeUnknown")
                .field("message", message)
                .field("after_sequence", &recovery.after_sequence)
                .finish(),
        }
    }
}
impl std::error::Error for ModelSubmitFailure {}
#[derive(Clone)]
pub struct ModelOperationReview {
    session: String,
    operation: ModelOperationSnapshot,
}
impl ModelOperationReview {
    pub fn operation(&self) -> &ModelOperationSnapshot {
        &self.operation
    }
}

/// Full human-visible proposal. No default permissions, filesystem path or
/// implicit grant. The backend creates its own private temporary workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceProposal {
    pub label: String,
    pub capabilities: Vec<Permission>,
    pub resources: Vec<ResourceScope>,
}

#[derive(Clone)]
pub struct WorkspaceReview {
    session: String,
    workspace: WorkspaceSnapshot,
}
impl WorkspaceReview {
    pub fn workspace(&self) -> &WorkspaceSnapshot {
        &self.workspace
    }
}
#[derive(Clone)]
pub struct ProvisionReview {
    session: String,
    proposal: WorkspaceProposal,
    model: Option<LocalModelReview>,
}
impl ProvisionReview {
    pub fn proposal(&self) -> &WorkspaceProposal {
        &self.proposal
    }
}
#[derive(Clone)]
pub struct TaskReview {
    session: String,
    snapshot: TaskSnapshot,
}
impl TaskReview {
    pub fn snapshot(&self) -> &TaskSnapshot {
        &self.snapshot
    }
}
#[derive(Clone)]
pub struct ActionReview {
    task: TaskReview,
    request_id: String,
    action: TaskAction,
}
impl ActionReview {
    pub fn action(&self) -> &TaskAction {
        &self.action
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn task(&self) -> &TaskReview {
        &self.task
    }
}
#[derive(Clone)]
pub struct ApprovalReview {
    task: TaskReview,
    call: ToolCall,
}
impl ApprovalReview {
    pub fn call(&self) -> &ToolCall {
        &self.call
    }
    pub fn task(&self) -> &TaskReview {
        &self.task
    }
}
#[derive(Clone)]
pub struct LocalModelReview {
    session: String,
    config: Config,
    scope: ResourceScope,
    binding_id: String,
    provenance: Value,
}
impl LocalModelReview {
    pub fn resource(&self) -> &ResourceScope {
        &self.scope
    }
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }
    pub fn provenance(&self) -> &Value {
        &self.provenance
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeDecision {
    Clarification(String),
    ContinueWithoutReplay,
    RetryUncertainCall,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskAction {
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
    /// Narrow local domain tools only. Backend remains responsible for exact
    /// descriptor schemas, granted resource scopes and durable approvals.
    Tool {
        name: String,
        arguments: Value,
    },
}

impl<T: Transport> Bridge<T> {
    /// Captures a displayed task and exact validated local model. This review
    /// neither dispatches inference nor upgrades any workspace permission.
    pub fn task_model_run_review(
        &self,
        task: &TaskReview,
        model: &LocalModelReview,
        max_steps: usize,
    ) -> Result<ModelRunReview> {
        self.task_review_session(task)?;
        if model.session != task.session
            || model.config != self.view.config
            || !(1..=8).contains(&max_steps)
            || !task.snapshot.workspace.available
            || !matches!(
                task.snapshot.task.state,
                TaskState::Pending | TaskState::Running { in_flight: None }
            )
            || !task
                .snapshot
                .capabilities()
                .contains(&Permission::ModelInference)
            || !task.snapshot.resources().contains(&model.scope)
            || task
                .snapshot
                .task
                .agent_persona
                .as_ref()
                .and_then(|persona| persona.model_target.as_ref())
                .is_some_and(|target| {
                    target.provider != model.config.provider
                        || target.endpoint != model.config.endpoint
                        || target.model != model.config.model
                        || target.scope_id != model.scope.id
                })
        {
            return Err(
                "Review an idle task with its exact granted local model and a bound of 1–8 steps."
                    .into(),
            );
        }
        Ok(ModelRunReview {
            task: task.clone(),
            model: model.clone(),
            max_steps,
        })
    }
    /// Returns only an admitted operation. The actor must keep this handle and
    /// poll in separate bounded commands; it must never repeat a lost submit.
    pub fn task_model_submit(
        &mut self,
        review: &ModelRunReview,
        confirmed: bool,
    ) -> Result<ModelOperationReview> {
        self.task_model_submit_checked(review, confirmed)
            .map_err(|error| error.to_string())
    }
    /// Preflight failures prove no run was submitted. Once the POST can have
    /// started, retain an opaque recovery boundary for every ambiguous result.
    pub fn task_model_submit_checked(
        &mut self,
        review: &ModelRunReview,
        confirmed: bool,
    ) -> std::result::Result<ModelOperationReview, ModelSubmitFailure> {
        if !confirmed {
            return Err(ModelSubmitFailure::NotSubmitted(
                "Confirm the displayed task, local model and step limit.".into(),
            ));
        }
        self.task_validate_model(&review.model)
            .map_err(ModelSubmitFailure::NotSubmitted)?;
        let operations = self
            .task_model_operations(&review.task)
            .map_err(ModelSubmitFailure::NotSubmitted)?;
        self.task_check_session(&review.task.session)
            .map_err(ModelSubmitFailure::NotSubmitted)?;
        let recovery = Box::new(ModelRunRecovery {
            review: review.clone(),
            after_sequence: operations.last().map_or(0, |operation| operation.sequence),
        });
        let response = self.transport.request("POST", &task_path(&review.task.snapshot.task.id, "model-runs"), Some(&review.task.session), Some(json!({
            "expected_revision":review.task.snapshot.task.revision,"expected":expected(&review.model.config),
            "max_steps":review.max_steps,"confirmed":true,"binding_id":review.model.binding_id,
        })));
        let result = (|| -> Result<ModelOperationReview> {
            self.task_check_session(&review.task.session)?;
            let operation: ModelOperationSnapshot = decode_field(response?, "operation")?;
            validate_operation(&operation)?;
            if !recovery.matches(&operation) {
                return Err(
                    "Model admission returned a different identity or an old receipt.".into(),
                );
            }
            Ok(ModelOperationReview {
                session: review.task.session.clone(),
                operation,
            })
        })();
        result.map_err(|error| ModelSubmitFailure::OutcomeUnknown {
            message: format!("Model submission has no confirmed result: {error} Inspect saved state before any retry; the action may already have been recorded."),
            recovery,
        })
    }
    /// A matching old receipt cannot resolve a later lost submission. Require
    /// exactly one newer match; absence, expiry or ambiguity remains unknown.
    pub fn task_model_recover(
        &mut self,
        task: &TaskReview,
        recovery: &ModelRunRecovery,
    ) -> Result<ModelOperationReview> {
        self.task_review_session(task)?;
        if task.session != recovery.review.task.session
            || task.snapshot.task.id != recovery.review.task.snapshot.task.id
        {
            return Err("Model recovery belongs to another task or profile session.".into());
        }
        let operations = self.task_model_operations(task)?;
        let mut matches = operations
            .iter()
            .filter(|operation| recovery.matches(operation));
        let operation = matches.next().ok_or("No newer retained receipt confirms this submission. Inspect saved task state without replaying it.")?;
        if matches.next().is_some() {
            return Err("More than one newer receipt matches this submission. Its outcome remains uncertain; do not replay it.".into());
        }
        self.task_model_operation_review(task, operation)
    }
    /// Recover handles after an uncertain response only by inspecting current
    /// task-scoped statuses. Missing/evicted metadata never authorizes replay.
    pub fn task_model_operations(
        &mut self,
        task: &TaskReview,
    ) -> Result<Vec<ModelOperationSnapshot>> {
        let result = self.task_request(
            &task.session,
            "GET",
            &task_path(&task.snapshot.task.id, "model-runs"),
            None,
        )?;
        if result["task_id"] != task.snapshot.task.id
            || result["retention"] != "current_profile_session_latest_128"
            || result["missing_requires_task_inspection"] != true
        {
            return Err("Invalid model-status retention or task identity.".into());
        }
        let operations: Vec<ModelOperationSnapshot> = decode_field(result, "operations")?;
        if operations.len() > 128 {
            return Err("Model operation list exceeds its bound.".into());
        }
        let mut previous = 0;
        let mut ids = BTreeSet::new();
        for operation in &operations {
            validate_operation(operation)?;
            if operation.task_id != task.snapshot.task.id
                || operation.sequence <= previous
                || !ids.insert(&operation.id)
            {
                return Err("Model operation order or ownership is invalid.".into());
            }
            previous = operation.sequence;
        }
        Ok(operations)
    }
    pub fn task_model_operation_review(
        &self,
        task: &TaskReview,
        operation: &ModelOperationSnapshot,
    ) -> Result<ModelOperationReview> {
        self.task_review_session(task)?;
        validate_operation(operation)?;
        if operation.task_id != task.snapshot.task.id {
            return Err("Operation belongs to a different task.".into());
        }
        Ok(ModelOperationReview {
            session: task.session.clone(),
            operation: operation.clone(),
        })
    }
    pub fn task_model_poll(
        &mut self,
        review: &ModelOperationReview,
    ) -> Result<ModelOperationSnapshot> {
        let path = task_path(
            &review.operation.task_id,
            &format!("model-runs/{}", encode_component(&review.operation.id)),
        );
        let operation: ModelOperationSnapshot = decode_field(
            self.task_request(&review.session, "GET", &path, None)?,
            "operation",
        )?;
        validate_operation(&operation)?;
        let old = &review.operation;
        if operation.id != old.id
            || operation.task_id != old.task_id
            || operation.submitted_revision != old.submitted_revision
            || operation.max_steps != old.max_steps
            || operation.scope_id != old.scope_id
            || operation.binding_id != old.binding_id
            || operation.sequence != old.sequence
        {
            return Err("Model status returned a different admitted operation.".into());
        }
        Ok(operation)
    }
    /// The backend persists cancellation and drains its model lease. If this
    /// transport call times out, keep polling; do not claim cleanup completed.
    pub fn task_model_cancel(
        &mut self,
        task: &TaskReview,
        operation: &ModelOperationReview,
        reason: &str,
    ) -> Result<TaskSnapshot> {
        if task.session != operation.session || task.snapshot.task.id != operation.operation.task_id
        {
            return Err("Cancellation task and operation ownership differ.".into());
        }
        if task.snapshot.task.state.is_terminal() {
            return Err("This displayed task is already terminal.".into());
        }
        validate_text(reason, 4096, "Cancellation reason")?;
        self.task_mutate(
            task,
            "cancel",
            json!({"reason":reason,"model_operation_id":operation.operation.id}),
        )
    }
    fn task_session(&self) -> Result<String> {
        if !self.view.available {
            return Err("Local workspace is unavailable; refresh it before a task action.".into());
        }
        self.session
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "Local workspace admission is missing.".into())
    }
    fn task_check_session(&mut self, captured: &str) -> Result<()> {
        if self.task_session()?.as_str() != captured {
            return Err("Task review belongs to a previous local workspace session.".into());
        }
        self.check_session()?;
        if self.session.as_deref() != Some(captured) {
            return Err("Task review session changed.".into());
        }
        Ok(())
    }
    fn task_request(
        &mut self,
        session: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        self.task_check_session(session)?;
        let response = self.transport.request(method, path, Some(session), body);
        // Even failed/ambiguous mutations can coincide with profile revocation.
        // Never publish a late body into the next profile's native task view.
        self.task_check_session(session).map_err(|error| format!("Task response is no longer admitted: {error} Inspect saved state before any retry."))?;
        response.map_err(|error| if method == "GET" { error } else {
            format!("Task request has no confirmed result: {error} Inspect saved state before any retry; the action may already have been recorded.")
        })
    }
    pub fn task_overview(&mut self) -> Result<TaskOverview> {
        let session = self.task_session()?;
        let workspaces = self.task_request(&session, "GET", "/api/agent/workspaces", None)?;
        let tasks = self.task_request(&session, "GET", "/api/agent/tasks", None)?;
        let workspaces: Vec<WorkspaceSnapshot> = decode_field(workspaces, "workspaces")?;
        let tasks: Vec<TaskRecord> = decode_field(tasks, "tasks")?;
        if workspaces.len() > MAX_ROWS || tasks.len() > MAX_ROWS {
            return Err(
                "Native task list exceeds its display limit; no entries were silently omitted."
                    .into(),
            );
        }
        let mut workspace_ids = BTreeSet::new();
        for workspace in &workspaces {
            validate_workspace(workspace)?;
            if !workspace_ids.insert(&workspace.id) {
                return Err("Duplicate workspace identity in response.".into());
            }
        }
        let mut task_ids = BTreeSet::new();
        for task in &tasks {
            validate_task(task)?;
            if !task_ids.insert(&task.id) {
                return Err("Duplicate task identity in response.".into());
            }
        }
        Ok(TaskOverview { workspaces, tasks })
    }
    pub fn task_workspace_review(&self, displayed: &WorkspaceSnapshot) -> Result<WorkspaceReview> {
        validate_workspace(displayed)?;
        Ok(WorkspaceReview {
            session: self.task_session()?,
            workspace: displayed.clone(),
        })
    }
    pub fn task_provision_review(
        &self,
        proposal: WorkspaceProposal,
        model: Option<&LocalModelReview>,
    ) -> Result<ProvisionReview> {
        validate_proposal(&proposal)?;
        let session = self.task_session()?;
        let has_model = proposal.capabilities.contains(&Permission::ModelInference);
        let provider_scopes: Vec<_> = proposal
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::ProviderBinding)
            .collect();
        if has_model {
            let reviewed =
                model.ok_or("Review the exact local model before granting inference.")?;
            if reviewed.session != session
                || reviewed.config != self.view.config
                || provider_scopes != vec![&reviewed.scope]
            {
                return Err("Model grant must match the exact reviewed local target and current connection.".into());
            }
        } else if model.is_some() || !provider_scopes.is_empty() {
            return Err("A model resource requires the explicit model-inference permission and matching review.".into());
        }
        Ok(ProvisionReview {
            session,
            proposal,
            model: model.cloned(),
        })
    }
    pub fn task_provision(
        &mut self,
        reviewed: &ProvisionReview,
        confirmed: bool,
    ) -> Result<WorkspaceSnapshot> {
        if !confirmed {
            return Err(
                "Confirm the displayed workspace permissions and temporary-file boundary.".into(),
            );
        }
        self.task_check_session(&reviewed.session)?;
        if let Some(model) = &reviewed.model {
            self.task_validate_model(model)?;
        }
        let response = self.task_request(
            &reviewed.session,
            "POST",
            "/api/agent/workspaces",
            Some(json!({
                "label":reviewed.proposal.label,"capabilities":reviewed.proposal.capabilities,
                "resources":reviewed.proposal.resources,"confirmed":true
            })),
        )?;
        let workspace: WorkspaceSnapshot = decode_field(response, "workspace")?;
        validate_workspace(&workspace)?;
        if workspace.label != reviewed.proposal.label.trim()
            || set(&workspace.capabilities) != set(&reviewed.proposal.capabilities)
            || set(&workspace.resources) != set(&reviewed.proposal.resources)
        {
            return Err(
                "Created workspace does not match the reviewed permissions; inspect saved state."
                    .into(),
            );
        }
        Ok(workspace)
    }
    pub fn task_create(
        &mut self,
        workspace: &WorkspaceReview,
        input: &str,
    ) -> Result<TaskSnapshot> {
        validate_text(input, MAX_INPUT, "Task input")?;
        if !workspace.workspace.available {
            return Err("The selected temporary workspace is unavailable.".into());
        }
        let response = self.task_request(&workspace.session, "POST", "/api/agent/tasks", Some(json!({
            "workspace_id":workspace.workspace.id,"policy_revision":workspace.workspace.policy_revision,"input":input
        })))?;
        let snapshot = decode_snapshot(response)?;
        if snapshot.workspace.id != workspace.workspace.id
            || snapshot.workspace.policy_revision != workspace.workspace.policy_revision
            || snapshot.task.input != input
        {
            return Err(
                "Created task differs from the selected workspace or input; inspect saved state."
                    .into(),
            );
        }
        Ok(snapshot)
    }
    pub fn task_snapshot(&mut self, id: &str) -> Result<TaskSnapshot> {
        validate_id(id)?;
        let session = self.task_session()?;
        let snapshot =
            decode_snapshot(self.task_request(&session, "GET", &task_path(id, ""), None)?)?;
        if snapshot.task.id != id {
            return Err("Task response belongs to another task.".into());
        }
        Ok(snapshot)
    }
    pub fn task_review(&self, displayed: &TaskSnapshot) -> Result<TaskReview> {
        validate_snapshot(displayed)?;
        Ok(TaskReview {
            session: self.task_session()?,
            snapshot: displayed.clone(),
        })
    }
    fn task_mutate(
        &mut self,
        review: &TaskReview,
        operation: &str,
        mut body: Value,
    ) -> Result<TaskSnapshot> {
        body["expected_revision"] = json!(review.snapshot.task.revision);
        let response = self.task_request(
            &review.session,
            "POST",
            &task_path(&review.snapshot.task.id, operation),
            Some(body),
        )?;
        let snapshot = decode_snapshot(response)?;
        if snapshot.task.id != review.snapshot.task.id
            || snapshot.workspace.id != review.snapshot.workspace.id
            || snapshot.workspace.policy_revision != review.snapshot.workspace.policy_revision
            || snapshot.task.revision <= review.snapshot.task.revision
        {
            return Err("Task mutation response does not match its reviewed owner/revision; inspect saved state without replay.".into());
        }
        Ok(snapshot)
    }
    pub fn task_start(&mut self, review: &TaskReview) -> Result<TaskSnapshot> {
        if review.snapshot.task.state != TaskState::Pending {
            return Err("Only a displayed pending task can be started.".into());
        }
        self.task_mutate(review, "start", json!({}))
    }
    /// request_id is a caller-created UUID for this exact user action. Keep it
    /// with this review across an uncertain response; never silently regenerate
    /// or retry. It is an idempotency identity, not an authorization credential.
    pub fn task_action_review(
        &self,
        task: &TaskReview,
        request_id: &str,
        action: TaskAction,
    ) -> Result<ActionReview> {
        self.task_review_session(task)?;
        validate_id(request_id)?;
        if !matches!(
            task.snapshot.task.state,
            TaskState::Running { in_flight: None }
        ) || !task.snapshot.workspace.available
        {
            return Err("Refresh an available idle running task before submitting a tool.".into());
        }
        validate_action(&action)?;
        Ok(ActionReview {
            task: task.clone(),
            request_id: request_id.to_owned(),
            action,
        })
    }
    pub fn task_action(&mut self, review: &ActionReview) -> Result<TaskSnapshot> {
        self.task_mutate(
            &review.task,
            "actions",
            json!({"request_id":review.request_id,"action":review.action}),
        )
    }
    pub fn task_approval_review(&self, task: &TaskReview) -> Result<ApprovalReview> {
        self.task_review_session(task)?;
        let TaskState::NeedsApproval { call } = &task.snapshot.task.state else {
            return Err("This displayed task has no pending approval.".into());
        };
        Ok(ApprovalReview {
            task: task.clone(),
            call: call.clone(),
        })
    }
    pub fn task_decide(
        &mut self,
        review: &ApprovalReview,
        decision: ApprovalDecision,
        confirmed: bool,
    ) -> Result<TaskSnapshot> {
        if !confirmed {
            return Err("Confirm the exact displayed tool call and decision.".into());
        }
        if decision == ApprovalDecision::Approve && !local_tool(&review.call.name) {
            return Err("This effect requires its dedicated native destination/application review; generic approval is unavailable.".into());
        }
        self.task_mutate(
            &review.task,
            "approval",
            json!({"call_id":review.call.id,
            "decision":if decision == ApprovalDecision::Approve { "approve" } else { "deny" }}),
        )
    }
    pub fn task_cancel(&mut self, review: &TaskReview, reason: &str) -> Result<TaskSnapshot> {
        if review.snapshot.task.state.is_terminal() {
            return Err("This displayed task is already terminal.".into());
        }
        validate_text(reason, 4096, "Cancellation reason")?;
        self.task_mutate(review, "cancel", json!({"reason":reason}))
    }
    pub fn task_resume(
        &mut self,
        review: &TaskReview,
        decision: ResumeDecision,
        confirmed: bool,
    ) -> Result<TaskSnapshot> {
        if !confirmed {
            return Err("Confirm the explicit resume choice; interrupted calls are never replayed automatically.".into());
        }
        let body = match (&review.snapshot.task.state, decision) {
            (TaskState::NeedsClarification {..}, ResumeDecision::Clarification(answer)) => {
                validate_text(&answer, MAX_INPUT, "Clarification answer")?; json!({"kind":"clarification","answer":answer})
            },
            (TaskState::Interrupted {..}, ResumeDecision::ContinueWithoutReplay) => json!({"kind":"continue_without_replay"}),
            (TaskState::Interrupted { uncertain_call:Some(call) }, ResumeDecision::RetryUncertainCall) if local_tool(&call.name) => json!({"kind":"retry_uncertain_call"}),
            _ => return Err("The resume choice does not match the displayed state or needs a dedicated effect review.".into()),
        };
        self.task_mutate(review, "resume", body)
    }
    pub fn task_trace(&mut self, id: &str, after: Option<u64>, limit: usize) -> Result<TracePage> {
        validate_id(id)?;
        if !(1..=100).contains(&limit) || after.is_some_and(|n| n > i64::MAX as u64) {
            return Err("Trace page bounds are invalid.".into());
        }
        let session = self.task_session()?;
        let path = format!(
            "{}?limit={limit}{}",
            task_path(id, "trace"),
            after.map(|n| format!("&after={n}")).unwrap_or_default()
        );
        let page: TracePage = decode(self.task_request(&session, "GET", &path, None)?)?;
        if page.task_id != id || page.events.len() > limit || page.revision > i64::MAX as u64 {
            return Err("Trace response owner or size is invalid.".into());
        }
        let mut previous = after;
        for event in &page.events {
            if event.task_id != id
                || event.revision > page.revision
                || previous.is_some_and(|n| event.revision <= n)
            {
                return Err("Trace page order or owner is invalid.".into());
            }
            previous = Some(event.revision);
        }
        let expected_cursor = if page.events.len() == limit {
            previous
        } else {
            None
        };
        if page.next_after != expected_cursor {
            return Err("Trace cursor does not match the returned page.".into());
        }
        Ok(page)
    }
    /// Backend-only validation; performs no inference, provider discovery,
    /// connection test or file/model download. Target must already be selected.
    pub fn task_local_model(&mut self, displayed: &Config) -> Result<LocalModelReview> {
        if displayed != &self.view.config || !local_target(displayed) {
            return Err("Review the currently selected owned or numeric-loopback local model; Demo, remote URLs and untested providers cannot be granted here.".into());
        }
        let session = self.task_session()?;
        let result = self.task_request(
            &session,
            "POST",
            "/api/agent/model-selection",
            Some(expected(displayed)),
        )?;
        let scope = result["scope_id"]
            .as_str()
            .filter(|s| prefixed_digest(s, "provider:"))
            .ok_or("Local model scope is invalid.")?
            .to_owned();
        let binding = result["binding_id"]
            .as_str()
            .filter(|s| prefixed_digest(s, "model:"))
            .ok_or("Local model binding is invalid.")?
            .to_owned();
        let provenance = &result["provenance"];
        if provenance["provider"] != displayed.provider
            || provenance["endpoint"] != displayed.endpoint
            || provenance["model"] != displayed.model
            || provenance["binding_id"] != binding
            || provenance["scope_id"] != scope
            || (matches!(displayed.provider.as_str(), "managed" | "mlx")
                && !provenance["native"].is_object())
        {
            return Err("Local model validation returned a different target or missing native ownership evidence.".into());
        }
        Ok(LocalModelReview {
            session,
            config: displayed.clone(),
            scope: ResourceScope {
                kind: ResourceKind::ProviderBinding,
                id: scope,
            },
            binding_id: binding,
            provenance: provenance.clone(),
        })
    }
    fn task_validate_model(&mut self, review: &LocalModelReview) -> Result<()> {
        self.task_check_session(&review.session)?;
        if review.config != self.view.config {
            return Err("Reviewed model configuration changed.".into());
        }
        let fresh = self.task_local_model(&review.config)?;
        if fresh.binding_id != review.binding_id
            || fresh.scope != review.scope
            || fresh.provenance != review.provenance
        {
            return Err("Reviewed local model ownership or credential revision changed; review again before granting it.".into());
        }
        Ok(())
    }
    fn task_review_session(&self, review: &TaskReview) -> Result<()> {
        if self.task_session()? != review.session {
            return Err("Task review belongs to another profile session.".into());
        }
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| "Owned task API returned an invalid response.".into())
}
fn validate_operation(operation: &ModelOperationSnapshot) -> Result<()> {
    validate_id(&operation.id)?;
    validate_id(&operation.task_id)?;
    if operation.submitted_revision > i64::MAX as u64
        || !(1..=8).contains(&operation.max_steps)
        || !prefixed_digest(&operation.scope_id, "provider:")
        || !prefixed_digest(&operation.binding_id, "model:")
        || operation.sequence == 0
        || operation.task_revision.is_some_and(|r| r > i64::MAX as u64)
        || operation.stop_reason.is_empty()
        || operation.stop_reason.len() > 64
        || operation.worker_drained != operation.is_terminal()
        || (operation.cleanup_unconfirmed && operation.phase != ModelRunPhase::Failed)
        || (operation.cancellation.is_some() && operation.phase != ModelRunPhase::Cancelled)
        || (operation.phase == ModelRunPhase::Cancelled && operation.cancellation.is_none())
        || (operation.phase == ModelRunPhase::Running
            && (operation.task_revision.is_some()
                || operation.error.is_some()
                || operation.stop_reason != "running"))
        || (operation.phase == ModelRunPhase::Failed && operation.error.is_none())
        || operation.error.as_ref().is_some_and(|e| {
            e.code.is_empty() || e.code.len() > 64 || e.message.chars().count() > 2048
        })
    {
        return Err("Model operation metadata is invalid or exceeds its bounds.".into());
    }
    Ok(())
}
fn decode_field<T: serde::de::DeserializeOwned>(mut value: Value, field: &str) -> Result<T> {
    decode(
        value
            .get_mut(field)
            .ok_or("Owned task API response is missing a field.")?
            .take(),
    )
}
fn set<T: Ord>(items: &[T]) -> BTreeSet<&T> {
    items.iter().collect()
}
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return Err("Task or workspace identity is invalid.".into());
    }
    Ok(())
}
fn validate_text(text: &str, max: usize, label: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > max || text.contains('\0') {
        return Err(format!(
            "{label} is empty, too long or contains a null character."
        ));
    }
    Ok(())
}
fn validate_workspace(workspace: &WorkspaceSnapshot) -> Result<()> {
    validate_id(&workspace.id)?;
    validate_text(&workspace.label, 80, "Workspace label")?;
    if workspace.label.chars().any(char::is_control)
        || workspace.policy_revision > i64::MAX as u64
        || workspace.capabilities.len() > 25
        || workspace.resources.len() > 128
        || set(&workspace.capabilities).len() != workspace.capabilities.len()
        || set(&workspace.resources).len() != workspace.resources.len()
        || workspace.output_boundary != "plaintext_private_temporary_files"
    {
        return Err(
            "Workspace policy response is invalid or has an unknown storage boundary.".into(),
        );
    }
    for scope in &workspace.resources {
        validate_scope(scope)?;
    }
    Ok(())
}
fn validate_scope(scope: &ResourceScope) -> Result<()> {
    if scope.id.is_empty() || scope.id.len() > 256 || scope.id.chars().any(char::is_control) {
        return Err("Resource scope identity is invalid.".into());
    }
    Ok(())
}
fn validate_proposal(proposal: &WorkspaceProposal) -> Result<()> {
    validate_text(&proposal.label, 80, "Workspace label")?;
    if proposal.label.chars().any(char::is_control)
        || proposal.capabilities.len() > 25
        || proposal.resources.len() > 128
        || set(&proposal.capabilities).len() != proposal.capabilities.len()
        || set(&proposal.resources).len() != proposal.resources.len()
    {
        return Err("Workspace permissions contain duplicates or exceed their bounds.".into());
    }
    if proposal
        .capabilities
        .iter()
        .any(|c| c.needs_separate_setup())
        || proposal
            .resources
            .iter()
            .any(|r| r.kind == ResourceKind::EffectBinding)
    {
        return Err("External process, network, media-provider and application permissions need dedicated native setup.".into());
    }
    for scope in &proposal.resources {
        validate_scope(scope)?;
    }
    Ok(())
}
fn validate_call(call: &ToolCall) -> Result<()> {
    validate_id(&call.id)?;
    if call.name.is_empty()
        || call.name.len() > 64
        || !call
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
        || !call.arguments.is_object()
        || serde_json::to_vec(&call.arguments)
            .map_err(|_| "Invalid tool arguments.")?
            .len()
            > MAX_ARGUMENTS
    {
        return Err("Tool call response exceeds its bounds.".into());
    }
    Ok(())
}
fn validate_task(task: &TaskRecord) -> Result<()> {
    validate_id(&task.id)?;
    validate_id(&task.workspace_id)?;
    validate_text(&task.input, MAX_INPUT, "Task input")?;
    if task.revision > i64::MAX as u64 {
        return Err("Task revision exceeds its bounds.".into());
    }
    if task.capabilities.is_some() != task.resources.is_some() {
        return Err("Task grant response is incomplete.".into());
    }
    if let Some(capabilities) = &task.capabilities {
        if capabilities.len() > 25 || set(capabilities).len() != capabilities.len() {
            return Err("Task permissions exceed their bounds or contain duplicates.".into());
        }
    }
    if let Some(resources) = &task.resources {
        if resources.len() > 128 || set(resources).len() != resources.len() {
            return Err("Task resources exceed their bounds or contain duplicates.".into());
        }
        for scope in resources {
            validate_scope(scope)?;
        }
    }
    if let Some(persona) = &task.agent_persona {
        validate_id(&persona.definition_id)?;
        validate_text(&persona.name, 80, "Agent name")?;
        if persona.definition_revision == 0
            || persona.definition_revision > i64::MAX as u64
            || persona.name.chars().any(char::is_control)
            || persona.system_prompt.len() > 8192
            || persona.soul.len() > 4096
        {
            return Err("Captured agent persona exceeds its bounds.".into());
        }
        if let Some(target) = &persona.model_target {
            for (value, limit) in [
                (&target.provider, 32),
                (&target.endpoint, 4096),
                (&target.model, 512),
                (&target.scope_id, 256),
            ] {
                if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
                    return Err("Captured agent model identity is invalid.".into());
                }
            }
        }
    }
    match &task.state {
        TaskState::Running {
            in_flight: Some(call),
        }
        | TaskState::NeedsApproval { call }
        | TaskState::Interrupted {
            uncertain_call: Some(call),
        } => validate_call(call)?,
        _ => {}
    }
    Ok(())
}
fn validate_snapshot(snapshot: &TaskSnapshot) -> Result<()> {
    validate_task(&snapshot.task)?;
    validate_workspace(&snapshot.workspace)?;
    if snapshot.task.workspace_id != snapshot.workspace.id {
        return Err("Task and workspace identities do not match.".into());
    }
    if !set(snapshot.capabilities()).is_subset(&set(&snapshot.workspace.capabilities))
        || !set(snapshot.resources()).is_subset(&set(&snapshot.workspace.resources))
    {
        return Err("Task grants exceed their workspace policy.".into());
    }
    Ok(())
}
fn decode_snapshot(value: Value) -> Result<TaskSnapshot> {
    let snapshot = decode(value)?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}
fn task_path(id: &str, operation: &str) -> String {
    format!(
        "/api/agent/tasks/{}{}",
        encode_component(id),
        if operation.is_empty() {
            String::new()
        } else {
            format!("/{operation}")
        }
    )
}
fn local_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "list_directory"
            | "search"
            | "write_file"
            | "edit_file"
            | "update_plan"
            | "ask_clarification"
            | "task_complete"
            | "smith_status"
            | "search_conversations"
            | "read_document"
            | "write_document"
            | "list_conversations"
            | "read_conversation"
            | "export_conversation"
            | "save_artifact"
            | "recall_memory"
            | "save_memory"
            | "render_chart"
    )
}
fn validate_action(action: &TaskAction) -> Result<()> {
    if serde_json::to_vec(action)
        .map_err(|_| "Invalid task action.")?
        .len()
        > MAX_ARGUMENTS
    {
        return Err("Task action is too large.".into());
    }
    if let TaskAction::Tool { name, arguments } = action {
        if !local_tool(name) || !arguments.is_object() {
            return Err(
                "This tool needs a dedicated native review or has invalid arguments.".into(),
            );
        }
    }
    if let TaskAction::Edit {
        expected_sha256, ..
    } = action
    {
        if !digest(expected_sha256) {
            return Err("Read the exact file version before reviewing an edit.".into());
        }
    }
    Ok(())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn prefixed_digest(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(digest)
}
fn expected(config: &Config) -> Value {
    json!({"revision":config.revision,"provider":config.provider,"endpoint":config.endpoint})
}
fn local_target(config: &Config) -> bool {
    if config.model.trim().is_empty() || config.revision.is_empty() {
        return false;
    }
    match config.provider.as_str() {
        "managed" => config.endpoint.starts_with("managed://"),
        "mlx" => config.endpoint.starts_with("mlx://"),
        "ollama" | "openai" if config.tested => {
            let Some(rest) = config
                .endpoint
                .strip_prefix("http://")
                .or_else(|| config.endpoint.strip_prefix("https://"))
            else {
                return false;
            };
            if rest
                .bytes()
                .any(|b| b.is_ascii_whitespace() || matches!(b, b'@' | b'?' | b'#' | b'\\'))
            {
                return false;
            }
            let authority = rest.split('/').next().unwrap_or("");
            authority
                .parse::<SocketAddr>()
                .is_ok_and(|a| a.ip().is_loopback() && a.port() != 0)
        }
        _ => false,
    }
}
